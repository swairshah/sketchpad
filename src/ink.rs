//! Vector ink: strokes, Paper-style tools, pressure/speed-shaped outlines,
//! a splitting eraser, and snapshot undo history.

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Tool {
    Pencil,
    Pen,
    Marker,
    Eraser,
    /// Agent highlighter: translucent band whose width (logical px) rides in
    /// each sample's `pressure`, so a highlight can match a line of text.
    Highlight,
}

impl Tool {
    pub const ALL: [Tool; 4] = [Tool::Pencil, Tool::Pen, Tool::Marker, Tool::Eraser];
}

#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub struct Sample {
    pub x: f32,
    pub y: f32,
    pub pressure: f32,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Stroke {
    /// Unique per stroke (pieces left by an eraser get fresh ids).
    pub id: u64,
    /// Lineage: the id of the stroke this piece was originally drawn as.
    pub origin: u64,
    /// Drawn by the agent rather than the human.
    pub by_agent: bool,
    pub tool: Tool,
    pub color: u32,
    pub samples: Vec<Sample>,
}

static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// After loading saved strokes: never hand out an id that is already taken.
pub fn reserve_ids(max: u64) {
    NEXT_ID.fetch_max(max + 1, std::sync::atomic::Ordering::Relaxed);
}

pub fn next_id() -> u64 {
    NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

thread_local! {
    /// Outlines are pure functions of a stroke's samples: compute each once.
    static OUTLINES: std::cell::RefCell<std::collections::HashMap<(u64, usize, u32, u32), (std::rc::Rc<Vec<(f32, f32)>>, [f32; 4])>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

impl Stroke {
    fn cache_key(&self) -> (u64, usize, u32, u32) {
        let last = self.samples.last().map_or((0, 0), |p| (p.x.to_bits(), p.y.to_bits()));
        (self.id, self.samples.len(), last.0, last.1)
    }

    /// The outline polygon and its bounds (x0, y0, x1, y1), cached.
    pub fn shape(&self) -> (std::rc::Rc<Vec<(f32, f32)>>, [f32; 4]) {
        let key = self.cache_key();
        if let Some(hit) = OUTLINES.with(|c| c.borrow().get(&key).cloned()) {
            return hit;
        }
        let outline = self.outline();
        let mut b = [f32::MAX, f32::MAX, f32::MIN, f32::MIN];
        for &(x, y) in &outline {
            b[0] = b[0].min(x);
            b[1] = b[1].min(y);
            b[2] = b[2].max(x);
            b[3] = b[3].max(y);
        }
        let value = (std::rc::Rc::new(outline), b);
        OUTLINES.with(|c| {
            let mut c = c.borrow_mut();
            if c.len() > 20_000 {
                c.clear();
            }
            c.insert(key, value.clone());
        });
        value
    }

    pub fn new(tool: Tool, color: u32, first: Sample) -> Self {
        let id = next_id();
        Self {
            id,
            origin: id,
            by_agent: false,
            tool,
            color,
            samples: vec![first],
        }
    }

    /// Opacity of the ink (marker is translucent, pencil slightly soft).
    pub fn alpha(&self) -> f32 {
        match self.tool {
            Tool::Pencil => 0.82,
            Tool::Marker => 0.36,
            Tool::Highlight => 0.30,
            _ => 1.0,
        }
    }

    /// Append a raw sample, lightly low-pass filtered to remove digitizer jitter.
    pub fn push(&mut self, raw: Sample) {
        let Some(last) = self.samples.last().copied() else {
            self.samples.push(raw);
            return;
        };
        if (raw.x - last.x).hypot(raw.y - last.y) < 0.6 {
            return;
        }
        let k = 0.45;
        self.samples.push(Sample {
            x: last.x + (raw.x - last.x) * k,
            y: last.y + (raw.y - last.y) * k,
            pressure: last.pressure + (raw.pressure - last.pressure) * 0.35,
        });
    }

    /// Per-sample nib width in logical pixels.
    fn widths(&self) -> Vec<f32> {
        let n = self.samples.len();
        let mut widths = Vec::with_capacity(n);
        let mut smoothed = None::<f32>;
        for (i, s) in self.samples.iter().enumerate() {
            let speed = if i == 0 {
                0.0
            } else {
                let p = self.samples[i - 1];
                (s.x - p.x).hypot(s.y - p.y)
            };
            let p = s.pressure.clamp(0.05, 1.0);
            let raw = match self.tool {
                Tool::Pencil => 1.7 * (0.45 + 0.9 * p),
                Tool::Pen => {
                    let speed_thin = (1.15 - speed / 40.0).clamp(0.55, 1.15);
                    3.3 * (0.3 + 1.1 * p) * speed_thin
                }
                Tool::Marker => 15.0 * (0.85 + 0.25 * p),
                Tool::Highlight => s.pressure.max(1.0),
                Tool::Eraser => 0.0,
            };
            let w = match smoothed {
                Some(prev) => prev + (raw - prev) * 0.3,
                None => raw,
            };
            smoothed = Some(w);
            widths.push(w);
        }
        // Gentle taper at both ends for the pen-like tools.
        if matches!(self.tool, Tool::Pen | Tool::Pencil) && n > 6 {
            let taper = 5.min(n / 2);
            for i in 0..taper {
                let f = 0.45 + 0.55 * (i as f32 / taper as f32);
                widths[i] *= f;
                widths[n - 1 - i] *= f;
            }
        }
        widths
    }

    /// Closed outline polygon of the stroke (logical px), ready to fill
    /// with the non-zero rule. Catmull-Rom resampled for smooth curves.
    pub fn outline(&self) -> Vec<(f32, f32)> {
        let n = self.samples.len();
        if n == 0 {
            return Vec::new();
        }
        let widths = self.widths();
        if n == 1 {
            let s = self.samples[0];
            return circle(s.x, s.y, (widths[0] * 0.5).max(1.0), 14);
        }

        // Catmull-Rom subdivision.
        let mut pts: Vec<(f32, f32, f32)> = Vec::with_capacity(n * 4);
        let get = |i: isize| {
            let i = i.clamp(0, n as isize - 1) as usize;
            (self.samples[i].x, self.samples[i].y)
        };
        for i in 0..n - 1 {
            let p0 = get(i as isize - 1);
            let p1 = get(i as isize);
            let p2 = get(i as isize + 1);
            let p3 = get(i as isize + 2);
            let steps = 4;
            for s in 0..steps {
                let t = s as f32 / steps as f32;
                let (x, y) = catmull(p0, p1, p2, p3, t);
                let w = widths[i] + (widths[i + 1] - widths[i]) * t;
                pts.push((x, y, w));
            }
        }
        let last = self.samples[n - 1];
        pts.push((last.x, last.y, widths[n - 1]));

        let m = pts.len();
        let mut left = Vec::with_capacity(m);
        let mut right = Vec::with_capacity(m);
        let mut normal = (0.0f32, -1.0f32);
        let mut normals = Vec::with_capacity(m);
        for j in 0..m {
            let a = pts[j.saturating_sub(1)];
            let b = pts[(j + 1).min(m - 1)];
            let (tx, ty) = (b.0 - a.0, b.1 - a.1);
            let len = tx.hypot(ty);
            if len > 1e-4 {
                normal = (-ty / len, tx / len);
            }
            normals.push(normal);
            let r = (pts[j].2 * 0.5).max(0.45);
            left.push((pts[j].0 + normal.0 * r, pts[j].1 + normal.1 * r));
            right.push((pts[j].0 - normal.0 * r, pts[j].1 - normal.1 * r));
        }

        let mut poly = Vec::with_capacity(m * 2 + 24);
        poly.extend_from_slice(&left);
        // End cap: from the left side, sweeping forward, to the right side.
        let (ex, ey, ew) = pts[m - 1];
        let a_end = normals[m - 1].1.atan2(normals[m - 1].0);
        cap(&mut poly, ex, ey, (ew * 0.5).max(0.45), a_end);
        poly.extend(right.iter().rev());
        // Start cap: from the right side, sweeping backward, to the left side.
        let (sx, sy, sw) = pts[0];
        let a_start = normals[0].1.atan2(normals[0].0) + std::f32::consts::PI;
        cap(&mut poly, sx, sy, (sw * 0.5).max(0.45), a_start);
        poly
    }

    fn near(&self, x: f32, y: f32, r: f32) -> bool {
        self.samples.iter().any(|s| (s.x - x).hypot(s.y - y) <= r)
    }
}

fn catmull(p0: (f32, f32), p1: (f32, f32), p2: (f32, f32), p3: (f32, f32), t: f32) -> (f32, f32) {
    let t2 = t * t;
    let t3 = t2 * t;
    let f = |a: f32, b: f32, c: f32, d: f32| {
        0.5 * ((2.0 * b)
            + (-a + c) * t
            + (2.0 * a - 5.0 * b + 4.0 * c - d) * t2
            + (-a + 3.0 * b - 3.0 * c + d) * t3)
    };
    (f(p0.0, p1.0, p2.0, p3.0), f(p0.1, p1.1, p2.1, p3.1))
}

fn cap(poly: &mut Vec<(f32, f32)>, cx: f32, cy: f32, r: f32, from_angle: f32) {
    let steps = 8;
    for i in 1..steps {
        let a = from_angle - std::f32::consts::PI * (i as f32 / steps as f32);
        poly.push((cx + r * a.cos(), cy + r * a.sin()));
    }
}

pub fn circle(cx: f32, cy: f32, r: f32, steps: usize) -> Vec<(f32, f32)> {
    (0..steps)
        .map(|i| {
            let a = std::f32::consts::TAU * i as f32 / steps as f32;
            (cx + r * a.cos(), cy + r * a.sin())
        })
        .collect()
}

/// Rub out ink within `r` of (x, y), splitting strokes where they are cut.
/// Returns true if anything changed.
pub fn erase(strokes: &mut Vec<Stroke>, x: f32, y: f32, r: f32) -> bool {
    let mut changed = false;
    let mut out = Vec::with_capacity(strokes.len());
    for stroke in strokes.drain(..) {
        if !stroke.near(x, y, r) {
            out.push(stroke);
            continue;
        }
        changed = true;
        let mut run: Vec<Sample> = Vec::new();
        for s in &stroke.samples {
            if (s.x - x).hypot(s.y - y) <= r {
                if run.len() >= 2 {
                    out.push(Stroke {
                        id: next_id(),
                        origin: stroke.origin,
                        by_agent: stroke.by_agent,
                        tool: stroke.tool,
                        color: stroke.color,
                        samples: std::mem::take(&mut run),
                    });
                }
                run.clear();
            } else {
                run.push(*s);
            }
        }
        if run.len() >= 2 {
            out.push(Stroke {
                id: next_id(),
                origin: stroke.origin,
                by_agent: stroke.by_agent,
                tool: stroke.tool,
                color: stroke.color,
                samples: run,
            });
        }
    }
    *strokes = out;
    changed
}

/// Rub out every bit of ink inside an axis-aligned rectangle.
pub fn erase_rect(strokes: &mut Vec<Stroke>, x: f32, y: f32, w: f32, h: f32) -> bool {
    let inside = |s: &Sample| s.x >= x && s.x <= x + w && s.y >= y && s.y <= y + h;
    let mut changed = false;
    let mut out = Vec::with_capacity(strokes.len());
    for stroke in strokes.drain(..) {
        if !stroke.samples.iter().any(inside) {
            out.push(stroke);
            continue;
        }
        changed = true;
        let mut run: Vec<Sample> = Vec::new();
        for s in &stroke.samples {
            if inside(s) {
                if run.len() >= 2 {
                    out.push(Stroke {
                        id: next_id(),
                        origin: stroke.origin,
                        by_agent: stroke.by_agent,
                        tool: stroke.tool,
                        color: stroke.color,
                        samples: std::mem::take(&mut run),
                    });
                }
                run.clear();
            } else {
                run.push(*s);
            }
        }
        if run.len() >= 2 {
            out.push(Stroke {
                id: next_id(),
                origin: stroke.origin,
                by_agent: stroke.by_agent,
                tool: stroke.tool,
                color: stroke.color,
                samples: run,
            });
        }
    }
    *strokes = out;
    changed
}

#[derive(Default)]
pub struct History {
    undo: Vec<Vec<Stroke>>,
    redo: Vec<Vec<Stroke>>,
}

impl History {
    pub fn checkpoint(&mut self, strokes: &[Stroke]) {
        self.undo.push(strokes.to_vec());
        self.redo.clear();
        if self.undo.len() > 200 {
            self.undo.remove(0);
        }
    }

    pub fn undo(&mut self, strokes: &mut Vec<Stroke>) -> bool {
        match self.undo.pop() {
            Some(prev) => {
                self.redo.push(std::mem::replace(strokes, prev));
                true
            }
            None => false,
        }
    }

    pub fn redo(&mut self, strokes: &mut Vec<Stroke>) -> bool {
        match self.redo.pop() {
            Some(next) => {
                self.undo.push(std::mem::replace(strokes, next));
                true
            }
            None => false,
        }
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }
}
