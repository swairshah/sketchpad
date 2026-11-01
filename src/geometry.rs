//! Where our window sits on the screen. The pen and touchscreen report
//! positions across the whole built-in display; when the window is tiled
//! (not fullscreen) we need its position to put ink under the nib.
//! Polls Hyprland a few times a second; falls back to "fullscreen".

use std::{
    process::Command,
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

use serde_json::Value;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Geometry {
    /// The window's client area, in global logical coordinates.
    pub win: (f32, f32, f32, f32),
    /// The display the touch digitizer covers, in global logical coordinates.
    pub screen: (f32, f32, f32, f32),
}

impl Geometry {
    /// Normalized digitizer position -> window-local logical position.
    pub fn map(&self, nx: f32, ny: f32) -> (f32, f32) {
        let (sx, sy, sw, sh) = self.screen;
        let (wx, wy, _, _) = self.win;
        (sx + nx * sw - wx, sy + ny * sh - wy)
    }
}

pub type Shared = Arc<Mutex<Option<Geometry>>>;

pub fn spawn() -> Shared {
    let shared: Shared = Arc::new(Mutex::new(None));
    let out = shared.clone();
    thread::spawn(move || {
        let pid = std::process::id() as i64;
        loop {
            let geom = query(pid);
            if let Ok(mut g) = out.lock() {
                *g = geom;
            }
            thread::sleep(Duration::from_millis(250));
        }
    });
    shared
}

fn hyprctl(what: &str) -> Option<Value> {
    let out = Command::new("hyprctl").args(["-j", what]).output().ok()?;
    serde_json::from_slice(&out.stdout).ok()
}

fn query(pid: i64) -> Option<Geometry> {
    let clients = hyprctl("clients")?;
    let client = clients
        .as_array()?
        .iter()
        .find(|c| c.get("pid").and_then(Value::as_i64) == Some(pid))?;
    let pair = |v: &Value, k: &str| -> Option<(f32, f32)> {
        let a = v.get(k)?.as_array()?;
        Some((a.first()?.as_f64()? as f32, a.get(1)?.as_f64()? as f32))
    };
    let (x, y) = pair(client, "at")?;
    let (w, h) = pair(client, "size")?;

    let monitors = hyprctl("monitors")?;
    let monitors = monitors.as_array()?;
    // The digitizer belongs to the built-in panel.
    let mon = monitors
        .iter()
        .find(|m| m.get("name").and_then(Value::as_str).is_some_and(|n| n.starts_with("eDP")))
        .or_else(|| monitors.first())?;
    let f = |k: &str| mon.get(k).and_then(Value::as_f64).unwrap_or(0.0) as f32;
    let scale = f("scale").max(0.1);
    Some(Geometry {
        win: (x, y, w, h),
        screen: (f("x"), f("y"), f("width") / scale, f("height") / scale),
    })
}
