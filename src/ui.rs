//! Geometry shared by rendering and hit-testing (mouse, pencil and finger
//! all go through `Layout::hit`), plus hand-drawn tool illustrations.

use gpui::{
    Bounds, FillOptions, FillRule, PathBuilder, PathStyle, Pixels, Window, point, px, quad, rgb,
    rgba, size,
};

use crate::ink::Tool;

pub const PAPER: u32 = 0xf5efe2;
pub const TRAY: u32 = 0xfcf9f2;
pub const INK_DARK: u32 = 0x2a2722;
pub const MUTED: u32 = 0x8c8273;

/// Paper-ish palette: charcoal, coral, mustard, sky, mint, plum, sepia.
pub const COLORS: [u32; 7] = [
    0x2b2a28, 0xe4573d, 0xf2b134, 0x3a8fd1, 0x4cae7b, 0x8e5f9c, 0x8a5a3c,
];

pub const TRAY_H: f32 = 88.0;
const MARGIN: f32 = 18.0;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }

    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x <= self.x + self.w && y >= self.y && y <= self.y + self.h
    }

    pub fn relative_to(&self, origin: Rect) -> Rect {
        Rect::new(self.x - origin.x, self.y - origin.y, self.w, self.h)
    }
}

/// Which screen edge the tray hugs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Dock {
    Top,
    Bottom,
    Left,
    Right,
}

impl Dock {
    pub fn vertical(self) -> bool {
        matches!(self, Dock::Left | Dock::Right)
    }

    /// The edge closest to a point.
    pub fn nearest(x: f32, y: f32, width: f32, height: f32) -> Dock {
        let candidates = [
            (y, Dock::Top),
            (height - y, Dock::Bottom),
            (x, Dock::Left),
            (width - x, Dock::Right),
        ];
        candidates
            .into_iter()
            .min_by(|a, b| a.0.total_cmp(&b.0))
            .map(|c| c.1)
            .unwrap_or(Dock::Bottom)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Action {
    Tool(Tool),
    Color(usize),
    Undo,
    Redo,
    /// Jump to the composer and type.
    Compose,
    /// Ask the agent now (long press: toggle auto).
    Pi,
    /// The floating button: opens / closes the tray (drag it to move both).
    ToggleTray,
    /// Send / skip an agent's drawing request (figure block).
    Send,
    Skip,
    /// Allow / deny an agent's consequential browser action (the confirm card).
    Allow,
    Deny,
    /// Scroll to a block (checkpoint rail).
    Jump(usize),
}

enum Slot {
    Space(f32),
    Divider(f32),
    Item(Action, f32, f32), // action, length along the tray, inset across it
}

fn slots() -> Vec<Slot> {
    let mut v = vec![Slot::Space(18.0)];
    for tool in Tool::ALL {
        v.push(Slot::Item(Action::Tool(tool), 58.0, 0.0));
    }
    v.push(Slot::Divider(20.0));
    for i in 0..COLORS.len() {
        v.push(Slot::Item(Action::Color(i), 46.0, 18.0));
    }
    v.push(Slot::Divider(20.0));
    for a in [Action::Undo, Action::Redo, Action::Compose] {
        v.push(Slot::Item(a, 48.0, 20.0));
    }
    v.push(Slot::Space(8.0));
    v.push(Slot::Item(Action::Pi, 62.0, 12.0));
    v.push(Slot::Space(18.0));
    v
}

pub const FAB: f32 = 54.0;

pub struct Layout {
    pub dock: Dock,
    pub fab: Rect,
    pub tray: Option<Rect>,
    pub dividers: Vec<Rect>,
    pub buttons: Vec<(Rect, Action)>,
}

impl Layout {
    /// `fab` is the center of the floating button; the tray unfolds from it
    /// toward the middle of the screen.
    pub fn new(width: f32, height: f32, fab: (f32, f32), tray_open: bool) -> Self {
        let (fx, fy) = clamp_fab(fab, width, height);
        let fab_rect = Rect::new(fx - FAB / 2.0, fy - FAB / 2.0, FAB, FAB);
        let mut buttons = vec![(fab_rect, Action::ToggleTray)];
        let mut dividers = Vec::new();

        let vertical = fx < 150.0 || fx > width - 150.0;
        let dock = if vertical {
            if fx < width / 2.0 { Dock::Left } else { Dock::Right }
        } else if fy < height / 2.0 {
            Dock::Top
        } else {
            Dock::Bottom
        };

        let slots = slots();
        let natural: f32 = slots
            .iter()
            .map(|s| match s {
                Slot::Space(l) | Slot::Divider(l) | Slot::Item(_, l, _) => *l,
            })
            .sum();
        let gap = 12.0;
        let room = if vertical {
            if fy < height / 2.0 {
                height - fab_rect.y - FAB - gap - MARGIN
            } else {
                fab_rect.y - gap - MARGIN
            }
        } else {
            width - 2.0 * MARGIN
        };
        let k = (room / natural).clamp(0.3, 1.0);
        let len = natural * k;

        let tray = if vertical {
            let x = (fx - TRAY_H / 2.0).clamp(MARGIN / 2.0, width - TRAY_H - MARGIN / 2.0);
            let y = if fy < height / 2.0 { fab_rect.y + FAB + gap } else { fab_rect.y - gap - len };
            Rect::new(x, y, TRAY_H, len)
        } else {
            let x = (fx - len / 2.0).clamp(MARGIN, (width - len - MARGIN).max(MARGIN));
            let y = if fy < height / 2.0 { fab_rect.y + FAB + gap } else { fab_rect.y - gap - TRAY_H };
            Rect::new(x, y, len, TRAY_H)
        };
        let along_rect = |a: f32, l: f32, inset: f32| {
            if vertical {
                Rect::new(tray.x + inset, tray.y + a, TRAY_H - 2.0 * inset, l)
            } else {
                Rect::new(tray.x + a, tray.y + inset, l, TRAY_H - 2.0 * inset)
            }
        };

        if tray_open {
            let mut a = 0.0;
            for slot in &slots {
                match *slot {
                    Slot::Space(l) => a += l * k,
                    Slot::Divider(l) => {
                        let mid = a + l * k / 2.0;
                        dividers.push(if vertical {
                            Rect::new(tray.x + 22.0, tray.y + mid, TRAY_H - 44.0, 1.0)
                        } else {
                            Rect::new(tray.x + mid, tray.y + 22.0, 1.0, TRAY_H - 44.0)
                        });
                        a += l * k;
                    }
                    Slot::Item(action, l, inset) => {
                        buttons.push((along_rect(a, l * k, inset), action));
                        a += l * k;
                    }
                }
            }
        }

        Self {
            dock,
            fab: fab_rect,
            tray: tray_open.then_some(tray),
            dividers,
            buttons,
        }
    }

    pub fn hit(&self, x: f32, y: f32) -> Option<Action> {
        self.buttons
            .iter()
            .find(|(rect, _)| rect.contains(x, y))
            .map(|(_, action)| *action)
    }

    /// Chrome that should swallow pen-down instead of starting ink.
    pub fn is_chrome(&self, x: f32, y: f32) -> bool {
        self.fab.contains(x, y) || self.tray.is_some_and(|r| r.contains(x, y))
    }
}

pub fn clamp_fab(fab: (f32, f32), width: f32, height: f32) -> (f32, f32) {
    (
        fab.0.clamp(FAB / 2.0 + 8.0, (width - FAB / 2.0 - 8.0).max(FAB)),
        fab.1.clamp(FAB / 2.0 + 8.0, (height - FAB / 2.0 - 8.0).max(FAB)),
    )
}

// ---------------------------------------------------------------------------
// Painting helpers (absolute window coordinates).

fn fill_poly(window: &mut Window, pts: &[(f32, f32)], color: u32, alpha: f32) {
    if pts.len() < 3 {
        return;
    }
    let mut pb = PathBuilder::fill().with_style(PathStyle::Fill(
        FillOptions::default().with_fill_rule(FillRule::NonZero),
    ));
    pb.move_to(point(px(pts[0].0), px(pts[0].1)));
    for &(x, y) in &pts[1..] {
        pb.line_to(point(px(x), px(y)));
    }
    pb.close();
    if let Ok(path) = pb.build() {
        window.paint_path(
            path,
            rgba((color << 8) | (alpha.clamp(0.0, 1.0) * 255.0) as u32),
        );
    }
}

pub fn paint_polygon(window: &mut Window, pts: &[(f32, f32)], color: u32, alpha: f32) {
    fill_poly(window, pts, color, alpha);
}

/// Fill a polygon shifted by (dx, dy) without copying it.
pub fn paint_polygon_at(window: &mut Window, pts: &[(f32, f32)], dx: f32, dy: f32, scale: f32, color: u32, alpha: f32) {
    if pts.len() < 3 {
        return;
    }
    let mut pb = PathBuilder::fill().with_style(PathStyle::Fill(
        FillOptions::default().with_fill_rule(FillRule::NonZero),
    ));
    pb.move_to(point(px(pts[0].0 * scale + dx), px(pts[0].1 * scale + dy)));
    for &(x, y) in &pts[1..] {
        pb.line_to(point(px(x * scale + dx), px(y * scale + dy)));
    }
    pb.close();
    if let Ok(path) = pb.build() {
        window.paint_path(path, rgba((color << 8) | (alpha.clamp(0.0, 1.0) * 255.0) as u32));
    }
}

fn rrect(window: &mut Window, x: f32, y: f32, w: f32, h: f32, r: f32, color: u32) {
    window.paint_quad(quad(
        Bounds {
            origin: point(px(x), px(y)),
            size: size(px(w), px(h)),
        },
        px(r),
        rgb(color),
        px(0.0),
        rgb(color),
        Default::default(),
    ));
}

pub fn dot(window: &mut Window, x: f32, y: f32, r: f32, color: u32, alpha: f32) {
    window.paint_quad(quad(
        Bounds {
            origin: point(px(x - r), px(y - r)),
            size: size(px(r * 2.0), px(r * 2.0)),
        },
        px(r),
        rgba((color << 8) | (alpha * 255.0) as u32),
        px(0.0),
        rgba(0),
        Default::default(),
    ));
}

pub fn ring(window: &mut Window, x: f32, y: f32, r: f32, color: u32, alpha: f32) {
    window.paint_quad(quad(
        Bounds {
            origin: point(px(x - r), px(y - r)),
            size: size(px(r * 2.0), px(r * 2.0)),
        },
        px(r),
        rgba(0),
        px(1.2),
        rgba((color << 8) | (alpha * 255.0) as u32),
        Default::default(),
    ));
}

/// A little illustrated instrument whose tip points at the paper
/// (up from a bottom tray, right from a left tray, ...).
pub fn paint_tool(window: &mut Window, bounds: Bounds<Pixels>, tool: Tool, ink: u32, dock: Dock) {
    let (bx, by) = (f32::from(bounds.origin.x), f32::from(bounds.origin.y));
    let (bw, bh) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
    // (dx across the instrument from its axis, dy along it from the tip)
    let map = move |dx: f32, dy: f32| -> (f32, f32) {
        match dock {
            Dock::Bottom => (bx + bw / 2.0 + dx, by + dy),
            Dock::Top => (bx + bw / 2.0 - dx, by + bh - dy),
            Dock::Left => (bx + bw - dy, by + bh / 2.0 + dx),
            Dock::Right => (bx + dy, by + bh / 2.0 - dx),
        }
    };
    let rect = |window: &mut Window, dx: f32, dy: f32, w: f32, h: f32, r: f32, c: u32| {
        let (x0, y0) = map(dx, dy);
        let (x1, y1) = map(dx + w, dy + h);
        rrect(
            window,
            x0.min(x1),
            y0.min(y1),
            (x1 - x0).abs(),
            (y1 - y0).abs(),
            r,
            c,
        );
    };
    let poly = |window: &mut Window, pts: &[(f32, f32)], c: u32, a: f32| {
        let mapped: Vec<_> = pts.iter().map(|&(x, y)| map(x, y)).collect();
        fill_poly(window, &mapped, c, a);
    };
    let spot = |window: &mut Window, dx: f32, dy: f32, r: f32, c: u32| {
        let (x, y) = map(dx, dy);
        dot(window, x, y, r, c, 1.0);
    };
    match tool {
        Tool::Pencil => {
            rect(window, -8.0, 22.0, 16.0, 80.0, 2.0, 0xf2c14e);
            rect(window, -2.5, 22.0, 5.0, 80.0, 0.0, 0xe2a935);
            poly(
                window,
                &[(-8.0, 22.5), (8.0, 22.5), (0.0, 2.0)],
                0xeed6ad,
                1.0,
            );
            poly(window, &[(-3.2, 10.0), (3.2, 10.0), (0.0, 2.0)], ink, 1.0);
        }
        Tool::Pen => {
            poly(
                window,
                &[
                    (-7.5, 31.0),
                    (7.5, 31.0),
                    (3.5, 15.0),
                    (0.0, 3.0),
                    (-3.5, 15.0),
                ],
                0xd6b36a,
                1.0,
            );
            rect(window, -0.6, 9.0, 1.2, 13.0, 0.0, 0x7d6024);
            spot(window, 0.0, 22.0, 1.7, 0x7d6024);
            spot(window, 0.0, 4.5, 1.8, ink);
            rect(window, -8.5, 30.0, 17.0, 72.0, 4.0, 0x2f2d2b);
            rect(window, -8.5, 30.0, 17.0, 5.0, 1.0, 0xc9a45c);
            rect(window, -5.5, 40.0, 3.0, 50.0, 1.5, 0x46423e);
        }
        Tool::Marker => {
            poly(
                window,
                &[
                    (-7.0, 27.0),
                    (7.0, 27.0),
                    (7.0, 13.0),
                    (-4.0, 5.0),
                    (-7.0, 9.0),
                ],
                ink,
                0.9,
            );
            rect(window, -11.0, 26.0, 22.0, 76.0, 5.0, 0xd9d2c4);
            rect(window, -9.0, 28.0, 18.0, 72.0, 4.0, 0xf6f2ea);
            rect(window, -11.0, 26.0, 22.0, 12.0, 3.0, ink);
        }
        Tool::Eraser => {
            rect(window, -12.0, 8.0, 24.0, 44.0, 7.0, 0xef9d9d);
            rect(window, -9.0, 12.0, 5.0, 30.0, 2.5, 0xf6c0c0);
            rect(window, -12.0, 42.0, 24.0, 60.0, 3.0, 0x4f6d8f);
            rect(window, -12.0, 42.0, 24.0, 4.0, 1.0, 0x3d5674);
        }
        Tool::Highlight => {}
    }
}

/// Clawd, the little terracotta Claude mascot, in pixel art.
const CLAWD: [&str; 8] = [
    "..########..",
    "..#o####o#..",
    "############",
    "############",
    "..########..",
    "..########..",
    "..#.#..#.#..",
    "..#.#..#.#..",
];
pub const CLAWD_ORANGE: u32 = 0xdb7a5a;

pub fn paint_clawd(window: &mut Window, cx: f32, cy: f32, pixel: f32, alpha: f32) {
    let w = CLAWD[0].len() as f32 * pixel;
    let h = CLAWD.len() as f32 * pixel;
    let (x0, y0) = (cx - w / 2.0, cy - h / 2.0);
    for (row, line) in CLAWD.iter().enumerate() {
        for (col, c) in line.chars().enumerate() {
            let color = match c {
                '#' => CLAWD_ORANGE,
                'o' => 0x000000,
                _ => continue,
            };
            // Overlap by a hair so the pixels don't show seams.
            window.paint_quad(quad(
                Bounds {
                    origin: point(px(x0 + col as f32 * pixel), px(y0 + row as f32 * pixel)),
                    size: size(px(pixel + 0.4), px(pixel + 0.4)),
                },
                px(0.0),
                rgba((color << 8) | (alpha * 255.0) as u32),
                px(0.0),
                rgba(0),
                Default::default(),
            ));
        }
    }
}

fn polyline(window: &mut Window, pts: &[(f32, f32)], width: f32, color: u32, alpha: f32) {
    if pts.len() < 2 {
        return;
    }
    let mut pb = PathBuilder::stroke(px(width));
    pb.move_to(point(px(pts[0].0), px(pts[0].1)));
    for &(x, y) in &pts[1..] {
        pb.line_to(point(px(x), px(y)));
    }
    if let Ok(path) = pb.build() {
        window.paint_path(path, rgba((color << 8) | (alpha * 255.0) as u32));
    }
}

/// Curved arrow for undo (`mirror = false`) / redo (`mirror = true`).
pub fn paint_arrow(window: &mut Window, bounds: Bounds<Pixels>, mirror: bool, alpha: f32) {
    let cx = f32::from(bounds.origin.x) + f32::from(bounds.size.width) / 2.0;
    let cy = f32::from(bounds.origin.y) + f32::from(bounds.size.height) / 2.0 + 2.0;
    let sx = if mirror { -1.0 } else { 1.0 };
    let r = 9.0;
    let pts: Vec<(f32, f32)> = (0..=14)
        .map(|i| {
            let a = (200.0 - 230.0 * i as f32 / 14.0f32).to_radians();
            (cx + sx * r * a.cos(), cy - r * a.sin())
        })
        .collect();
    polyline(window, &pts, 2.2, crate::theme::pal().icon, alpha);
    let (hx, hy) = pts[0];
    fill_poly(
        window,
        &[
            (hx - sx * 5.5, hy - 2.0),
            (hx + sx * 3.5, hy - 3.0),
            (hx - sx * 0.5, hy + 5.5),
        ],
        crate::theme::pal().icon,
        alpha,
    );
}

/// A little keyboard: jump to the composer and type.
pub fn paint_keyboard(window: &mut Window, bounds: Bounds<Pixels>) {
    let cx = f32::from(bounds.origin.x) + f32::from(bounds.size.width) / 2.0;
    let cy = f32::from(bounds.origin.y) + f32::from(bounds.size.height) / 2.0;
    let (l, t, r, b) = (cx - 12.0, cy - 8.0, cx + 12.0, cy + 8.0);
    polyline(window, &[(l + 2.0, t), (r - 2.0, t), (r, t + 2.0), (r, b - 2.0), (r - 2.0, b), (l + 2.0, b), (l, b - 2.0), (l, t + 2.0), (l + 2.0, t)], 1.6, crate::theme::pal().icon, 1.0);
    for row in 0..2 {
        for col in 0..4 {
            let x = l + 4.0 + col as f32 * 4.8;
            let y = t + 3.5 + row as f32 * 4.0;
            rrect(window, x, y, 2.6, 2.4, 0.6, crate::theme::pal().icon);
        }
    }
    rrect(window, cx - 6.0, b - 4.5, 12.0, 2.2, 1.0, crate::theme::pal().icon);
}
