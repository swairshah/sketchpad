//! What changed on the page since the agent last looked.
//!
//! Strokes carry ids and an `origin` lineage, and eraser pieces copy their
//! samples verbatim, so we can tell exactly which ink is new and which bits
//! were rubbed out — and send the model a small "change view" instead of
//! re-sending the whole page every time.

use std::collections::HashSet;

use crate::ink::Stroke;
use crate::ui::Rect;

pub struct Diff {
    /// New human ink (indices into `current`).
    pub added: Vec<usize>,
    /// Ink that disappeared, as ghost strokes holding only the erased parts.
    pub erased: Vec<Stroke>,
    /// Number of strokes the agent itself added since (it already knows).
    pub agent_added: usize,
}

impl Diff {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.erased.is_empty()
    }
}

fn key(x: f32, y: f32) -> (u32, u32) {
    (x.to_bits(), y.to_bits())
}

pub fn diff(seen: &[Stroke], current: &[Stroke]) -> Diff {
    let seen_ids: HashSet<u64> = seen.iter().map(|s| s.id).collect();
    let seen_origins: HashSet<u64> = seen.iter().map(|s| s.origin).collect();
    let current_ids: HashSet<u64> = current.iter().map(|s| s.id).collect();

    let mut added = Vec::new();
    let mut agent_added = 0;
    for (i, s) in current.iter().enumerate() {
        if seen_ids.contains(&s.id) {
            continue;
        }
        if seen_origins.contains(&s.origin) {
            continue; // a leftover piece of something already seen
        }
        if s.by_agent {
            agent_added += 1;
        } else {
            added.push(i);
        }
    }

    let mut erased = Vec::new();
    for gone in seen.iter().filter(|s| !current_ids.contains(&s.id)) {
        let kept: HashSet<(u32, u32)> = current
            .iter()
            .filter(|c| c.origin == gone.origin)
            .flat_map(|c| c.samples.iter().map(|p| key(p.x, p.y)))
            .collect();
        let mut run = Vec::new();
        for p in &gone.samples {
            if kept.contains(&key(p.x, p.y)) {
                if !run.is_empty() {
                    erased.push(Stroke {
                        samples: std::mem::take(&mut run),
                        ..gone.clone()
                    });
                }
            } else {
                run.push(*p);
            }
        }
        if !run.is_empty() {
            erased.push(Stroke {
                samples: run,
                ..gone.clone()
            });
        }
    }

    Diff {
        added,
        erased,
        agent_added,
    }
}

pub fn bbox<'a>(strokes: impl IntoIterator<Item = &'a Stroke>) -> Option<Rect> {
    let mut b: Option<(f32, f32, f32, f32)> = None;
    for s in strokes {
        for p in &s.samples {
            b = Some(match b {
                None => (p.x, p.y, p.x, p.y),
                Some((x0, y0, x1, y1)) => (x0.min(p.x), y0.min(p.y), x1.max(p.x), y1.max(p.y)),
            });
        }
    }
    b.map(|(x0, y0, x1, y1)| Rect::new(x0, y0, x1 - x0, y1 - y0))
}

pub fn union(a: Option<Rect>, b: Option<Rect>) -> Option<Rect> {
    match (a, b) {
        (Some(a), Some(b)) => {
            let x0 = a.x.min(b.x);
            let y0 = a.y.min(b.y);
            let x1 = (a.x + a.w).max(b.x + b.w);
            let y1 = (a.y + a.h).max(b.y + b.h);
            Some(Rect::new(x0, y0, x1 - x0, y1 - y0))
        }
        (a, None) => a,
        (None, b) => b,
    }
}

/// Grow a rect by `pad` and clamp it to the page.
pub fn pad(r: Rect, pad: f32, w: f32, h: f32) -> Rect {
    let x0 = (r.x - pad).max(0.0);
    let y0 = (r.y - pad).max(0.0);
    let x1 = (r.x + r.w + pad).min(w);
    let y1 = (r.y + r.h + pad).min(h);
    Rect::new(x0, y0, (x1 - x0).max(1.0), (y1 - y0).max(1.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ink::{self, Sample, Tool};

    fn line(x0: f32, y: f32, x1: f32) -> Stroke {
        let mut s = Stroke::new(
            Tool::Pen,
            0,
            Sample {
                x: x0,
                y,
                pressure: 0.5,
            },
        );
        let mut x = x0;
        while x < x1 {
            x += 2.0;
            s.samples.push(Sample {
                x,
                y,
                pressure: 0.5,
            });
        }
        s
    }

    #[test]
    fn detects_added_and_partially_erased() {
        let seen = vec![line(0.0, 10.0, 200.0)];
        let mut current = seen.clone();
        ink::erase(&mut current, 100.0, 10.0, 10.0);
        current.push(line(0.0, 50.0, 40.0));
        let d = diff(&seen, &current);
        assert_eq!(d.added.len(), 1, "only the new line counts as added");
        let gone = bbox(&d.erased).unwrap();
        assert!(
            gone.x >= 88.0 && gone.x + gone.w <= 112.0,
            "erased bbox {gone:?}"
        );
    }
}
