//! Drawing operations an agent can perform on the page — shared by the
//! in-app buddy and the `sketchpad draw` CLI. Coordinates are pixels of the
//! page image the agent saw (the PNG from ask/snapshot, or the buddy's view).

use serde::Deserialize;

use crate::ink::{Sample, Stroke, Tool};
use crate::ui::Rect;

pub const AGENT_INK: u32 = 0xc15f3c; // Claude terracotta
pub const HIGHLIGHT: u32 = 0xf5c518; // highlighter yellow

#[derive(Debug, Default, Deserialize)]
pub struct Ops {
    #[serde(default)]
    pub draw: Vec<Shape>,
    #[serde(default)]
    pub erase: Vec<Area>,
    #[serde(default)]
    pub notes: Vec<NoteSpec>,
}

impl Ops {
    pub fn is_empty(&self) -> bool {
        self.draw.is_empty() && self.erase.is_empty() && self.notes.is_empty()
    }
}

#[derive(Debug, Deserialize)]
pub struct Area {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

#[derive(Debug, Deserialize)]
pub struct NoteSpec {
    pub text: String,
    pub x: f32,
    pub y: f32,
}

/// Deliberately lenient: one struct, every field optional, `type` decides.
#[derive(Debug, Default, Deserialize)]
pub struct Shape {
    #[serde(rename = "type", default)]
    pub kind: String,
    from: Option<[f32; 2]>,
    to: Option<[f32; 2]>,
    cx: Option<f32>,
    cy: Option<f32>,
    r: Option<f32>,
    rx: Option<f32>,
    ry: Option<f32>,
    x: Option<f32>,
    y: Option<f32>,
    w: Option<f32>,
    h: Option<f32>,
    points: Option<Vec<Vec<f32>>>,
    closed: Option<bool>,
    text: Option<String>,
    size: Option<f32>,
    color: Option<String>,
    tool: Option<String>,
}

/// Text placed on the page (document px, top-left), optionally signed by Clawd.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Note {
    pub text: String,
    pub x: f32,
    pub y: f32,
    pub size: f32,
    pub color: u32,
    pub badge: bool,
}

pub struct Built {
    pub strokes: Vec<Stroke>,
    pub notes: Vec<Note>,
    pub erase: Vec<Rect>,
}

pub fn parse_color(name: Option<&str>) -> u32 {
    let Some(name) = name.map(|s| s.trim().to_ascii_lowercase()) else {
        return AGENT_INK;
    };
    if let Some(hex) = name.strip_prefix('#') {
        if let Ok(v) = u32::from_str_radix(hex, 16) {
            return if hex.len() == 3 {
                let (r, g, b) = ((v >> 8) & 0xf, (v >> 4) & 0xf, v & 0xf);
                (r * 17) << 16 | (g * 17) << 8 | (b * 17)
            } else {
                v & 0xffffff
            };
        }
    }
    match name.as_str() {
        "black" | "charcoal" => 0x2b2a28,
        "red" | "coral" => 0xe4573d,
        "orange" | "claude" | "terracotta" => AGENT_INK,
        "yellow" | "mustard" => 0xf2b134,
        "blue" => 0x3a8fd1,
        "green" => 0x4cae7b,
        "purple" | "plum" => 0x8e5f9c,
        "brown" | "sepia" => 0x8a5a3c,
        "gray" | "grey" => 0x8c8273,
        _ => AGENT_INK,
    }
}

/// Remove emoji and pictographs: the page is pen and paper.
pub fn strip_emoji(text: &str) -> String {
    text.chars()
        .filter(|&c| {
            let u = c as u32;
            !(matches!(u, 0x1F000..=0x1FAFF | 0x2600..=0x27BF | 0x2B00..=0x2BFF | 0xFE00..=0xFE0F
                | 0x200D | 0x20E3 | 0xE0020..=0xE007F))
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// A tiny deterministic wobble so agent ink looks hand drawn.
struct Wobble(u32);

impl Wobble {
    fn next(&mut self) -> f32 {
        self.0 = self.0.wrapping_mul(1664525).wrapping_add(1013904223);
        (self.0 >> 8) as f32 / (1u32 << 24) as f32 - 0.5
    }
}

fn densify(points: &[(f32, f32)], step: f32) -> Vec<(f32, f32)> {
    let mut out = Vec::new();
    for pair in points.windows(2) {
        let ((x0, y0), (x1, y1)) = (pair[0], pair[1]);
        let n = ((x1 - x0).hypot(y1 - y0) / step).ceil().max(1.0) as usize;
        for i in 0..n {
            let t = i as f32 / n as f32;
            out.push((x0 + (x1 - x0) * t, y0 + (y1 - y0) * t));
        }
    }
    if let Some(&last) = points.last() {
        out.push(last);
    }
    out
}

/// Dense Catmull-Rom spline through the given points.
fn smooth(points: &[(f32, f32)]) -> Vec<(f32, f32)> {
    let n = points.len();
    if n < 3 {
        return densify(points, 4.0);
    }
    let get = |i: isize| points[i.clamp(0, n as isize - 1) as usize];
    let mut out = Vec::new();
    for i in 0..n - 1 {
        let (p0, p1, p2, p3) = (
            get(i as isize - 1),
            get(i as isize),
            get(i as isize + 1),
            get(i as isize + 2),
        );
        let steps = ((p2.0 - p1.0).hypot(p2.1 - p1.1) / 3.0).ceil().max(2.0) as usize;
        for s in 0..steps {
            let t = s as f32 / steps as f32;
            let (t2, t3) = (t * t, t * t * t);
            let f = |a: f32, b: f32, c: f32, d: f32| {
                0.5 * (2.0 * b
                    + (-a + c) * t
                    + (2.0 * a - 5.0 * b + 4.0 * c - d) * t2
                    + (-a + 3.0 * b - 3.0 * c + d) * t3)
            };
            out.push((f(p0.0, p1.0, p2.0, p3.0), f(p0.1, p1.1, p2.1, p3.1)));
        }
    }
    out.push(points[n - 1]);
    out
}

/// Convert ops into document geometry: a position `v` along an axis becomes
/// `origin + v / scale` (so pixels of an exported image map back onto the
/// notebook); sizes just scale.
pub fn build(ops: &Ops, scale: f32, origin: (f32, f32)) -> Built {
    let k = 1.0 / scale.max(0.1);
    let p = |v: f32| v * k;
    let (ox, oy) = origin;
    let px = move |x: f32| ox + x * k;
    let py = move |y: f32| oy + y * k;
    let mut wobble = Wobble(0x5eed);
    let mut strokes = Vec::new();
    let mut notes = Vec::new();

    let mut stroke =
        |pts: Vec<(f32, f32)>, color: u32, tool: Tool, width: f32, wobble: &mut Wobble| {
            if pts.is_empty() {
                return;
            }
            let hl = tool == Tool::Highlight;
            let jitter = if hl { 0.15 } else { 0.5 };
            let drift = if hl {
                (0.0, 0.0)
            } else {
                (wobble.next() * 1.2, wobble.next() * 1.2)
            };
            let samples = pts
                .into_iter()
                .map(|(x, y)| Sample {
                    x: x + drift.0 + wobble.next() * jitter,
                    y: y + drift.1 + wobble.next() * jitter,
                    pressure: if hl {
                        width
                    } else {
                        0.55 + wobble.next() * 0.15
                    },
                })
                .collect();
            let id = crate::ink::next_id();
            strokes.push(Stroke {
                id,
                origin: id,
                by_agent: true,
                tool,
                color,
                samples,
            });
        };

    for shape in &ops.draw {
        // The agent always writes with the same terracotta pen, so its marks
        // read as one hand (highlights keep their yellow band).
        let color = AGENT_INK;
        let tool = Tool::Pen;
        // An agent's "marker" is a translucent highlighter sized to the shape,
        // not the human's 15px felt tip (that turns small circles into blobs).
        let sw = 6.0;
        match shape.kind.as_str() {
            "line" | "arrow" => {
                let (Some(a), Some(b)) = (shape.from, shape.to) else {
                    continue;
                };
                let (a, b) = ((px(a[0]), py(a[1])), (px(b[0]), py(b[1])));
                stroke(densify(&[a, b], 4.0), color, tool, sw, &mut wobble);
                if shape.kind == "arrow" {
                    let ang = (b.1 - a.1).atan2(b.0 - a.0);
                    let len = 16.0f32.min((b.0 - a.0).hypot(b.1 - a.1) * 0.4);
                    // One stroke for the head (wing, tip, wing): no doubled alpha.
                    let wing = |side: f32| {
                        let t = ang + std::f32::consts::PI - side * 0.45;
                        (b.0 + len * t.cos(), b.1 + len * t.sin())
                    };
                    let head = densify(&[wing(-1.0), b, wing(1.0)], 3.0);
                    stroke(head, color, tool, sw, &mut wobble);
                }
            }
            "circle" | "ellipse" => {
                let (Some(cx), Some(cy)) = (shape.cx.or(shape.x), shape.cy.or(shape.y)) else {
                    continue;
                };
                let rx = shape.rx.or(shape.r).unwrap_or(20.0);
                let ry = shape.ry.or(shape.r).unwrap_or(rx);
                let (cx, cy, rx, ry) = (px(cx), py(cy), p(rx), p(ry));
                let n = ((rx.max(ry) * std::f32::consts::TAU / 5.0) as usize).clamp(24, 160);
                // Start at a random angle and overshoot a little, like a real hand.
                let start = wobble.next() * std::f32::consts::TAU;
                let pts = (0..=n + n / 14)
                    .map(|i| {
                        let a = start + std::f32::consts::TAU * i as f32 / n as f32;
                        (cx + rx * a.cos(), cy + ry * a.sin())
                    })
                    .collect();
                let sw = (0.35 * rx.min(ry)).clamp(2.5, 8.0);
                stroke(pts, color, tool, sw, &mut wobble);
            }
            "highlight" => {
                // A highlighter swipe across a box, e.g. a line of text.
                let (Some(x), Some(y), Some(bw), Some(bh)) = (shape.x, shape.y, shape.w, shape.h)
                else {
                    continue;
                };
                let (x, y, bw, bh) = (px(x), py(y), p(bw), p(bh).max(4.0));
                let mid = y + bh / 2.0;
                let (x0, x1) = (x + bh / 2.0, (x + bw - bh / 2.0).max(x + bh / 2.0));
                let color = if shape.color.is_some() {
                    color
                } else {
                    HIGHLIGHT
                };
                let swipe = densify(&[(x0, mid), (x1, mid)], 4.0);
                stroke(swipe, color, Tool::Highlight, bh, &mut wobble);
            }
            "rect" | "box" => {
                let (Some(x), Some(y), Some(w), Some(h)) = (shape.x, shape.y, shape.w, shape.h)
                else {
                    continue;
                };
                let (x, y, w, h) = (px(x), py(y), p(w), p(h));
                let corners = [(x, y), (x + w, y), (x + w, y + h), (x, y + h), (x, y + 0.5)];
                stroke(densify(&corners, 4.0), color, tool, sw, &mut wobble);
            }
            "polyline" | "polygon" | "path" | "curve" => {
                let Some(points) = &shape.points else {
                    continue;
                };
                let mut pts: Vec<(f32, f32)> = points
                    .iter()
                    .filter(|q| q.len() >= 2)
                    .map(|q| (px(q[0]), py(q[1])))
                    .collect();
                if shape.closed.unwrap_or(shape.kind == "polygon") {
                    if let Some(&first) = pts.first() {
                        pts.push(first);
                    }
                }
                let pts = if shape.kind == "curve" {
                    smooth(&pts)
                } else {
                    densify(&pts, 4.0)
                };
                stroke(pts, color, tool, sw, &mut wobble);
            }
            "text" | "label" => {
                let (Some(x), Some(y), Some(text)) = (shape.x, shape.y, shape.text.as_deref())
                else {
                    continue;
                };
                let text = strip_emoji(text);
                if text.is_empty() {
                    continue;
                }
                notes.push(Note {
                    text,
                    x: px(x),
                    y: py(y),
                    size: p(shape.size.unwrap_or(24.0)).clamp(11.0, 60.0),
                    color,
                    badge: false,
                });
            }
            _ => {}
        }
    }

    for n in &ops.notes {
        let text = strip_emoji(&n.text);
        if text.is_empty() {
            continue;
        }
        notes.push(Note {
            text,
            x: px(n.x),
            y: py(n.y),
            size: 20.0,
            color: AGENT_INK,
            badge: true,
        });
    }

    let erase = ops
        .erase
        .iter()
        .map(|a| Rect::new(px(a.x), py(a.y), p(a.w), p(a.h)))
        .collect();

    Built {
        strokes,
        notes,
        erase,
    }
}

/// Pull the first JSON object out of a model reply (tolerates prose/fences).
pub fn parse_reply(reply: &str) -> Result<Ops, serde_json::Error> {
    let start = reply.find('{').unwrap_or(0);
    let end = reply.rfind('}').map_or(reply.len(), |i| i + 1);
    serde_json::from_str(&reply[start..end.max(start)])
}
