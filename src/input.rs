//! Raw evdev readers. GPUI's Wayland backend has no tablet/touch support,
//! so we read the stylus and touchscreen directly (user is in `input`).

use std::{sync::mpsc::Sender, thread};

use evdev::{AbsoluteAxisCode, Device, EventSummary, KeyCode, SynchronizationCode};

#[derive(Debug)]
pub enum InputEvent {
    PenConnected,
    Pen {
        x: f32,
        y: f32,
        pressure: f32,
        touching: bool,
        in_range: bool,
        rubber: bool,
    },
    /// Normalized positions of all fingers currently on the glass.
    Touch(Vec<(f32, f32)>),
    Error(String),
}

struct Axis {
    min: i32,
    range: f32,
}

impl Axis {
    fn of(device: &Device, code: AbsoluteAxisCode) -> Option<Axis> {
        let info = device
            .get_absinfo()
            .ok()?
            .find(|(axis, _)| *axis == code)?
            .1;
        Some(Axis {
            min: info.minimum(),
            range: (info.maximum() - info.minimum()).max(1) as f32,
        })
    }

    fn norm(&self, v: i32) -> f32 {
        ((v - self.min) as f32 / self.range).clamp(0.0, 1.0)
    }
}

pub fn spawn(sender: Sender<InputEvent>) {
    let pen_sender = sender.clone();
    thread::spawn(move || pen_loop(pen_sender));
    thread::spawn(move || touch_loop(sender));
}

fn pen_loop(sender: Sender<InputEvent>) {
    let Some((_, mut device)) = evdev::enumerate().find(|(_, d)| {
        d.supported_keys()
            .is_some_and(|k| k.contains(KeyCode::BTN_TOOL_PEN))
    }) else {
        let _ = sender.send(InputEvent::Error("no stylus found".into()));
        return;
    };
    let (Some(ax), Some(ay)) = (
        Axis::of(&device, AbsoluteAxisCode::ABS_X),
        Axis::of(&device, AbsoluteAxisCode::ABS_Y),
    ) else {
        let _ = sender.send(InputEvent::Error("stylus has no X/Y axes".into()));
        return;
    };
    let ap = Axis::of(&device, AbsoluteAxisCode::ABS_PRESSURE);
    let _ = sender.send(InputEvent::PenConnected);

    let (mut x, mut y, mut p) = (0, 0, 0);
    let (mut touching, mut pen, mut rubber) = (false, false, false);
    loop {
        let Ok(events) = device.fetch_events() else {
            let _ = sender.send(InputEvent::Error("stylus disconnected".into()));
            return;
        };
        for event in events {
            match event.destructure() {
                EventSummary::AbsoluteAxis(_, AbsoluteAxisCode::ABS_X, v) => x = v,
                EventSummary::AbsoluteAxis(_, AbsoluteAxisCode::ABS_Y, v) => y = v,
                EventSummary::AbsoluteAxis(_, AbsoluteAxisCode::ABS_PRESSURE, v) => p = v,
                EventSummary::Key(_, KeyCode::BTN_TOUCH, v) => touching = v != 0,
                EventSummary::Key(_, KeyCode::BTN_TOOL_PEN, v) => pen = v != 0,
                EventSummary::Key(_, KeyCode::BTN_TOOL_RUBBER, v) => rubber = v != 0,
                EventSummary::Synchronization(_, SynchronizationCode::SYN_REPORT, _) => {
                    let pressure = match (&ap, touching) {
                        (Some(a), true) => a.norm(p).max(0.06),
                        (None, true) => 0.6,
                        _ => 0.0,
                    };
                    let frame = InputEvent::Pen {
                        x: ax.norm(x),
                        y: ay.norm(y),
                        pressure,
                        touching,
                        in_range: pen || rubber,
                        rubber,
                    };
                    if sender.send(frame).is_err() {
                        return;
                    }
                }
                _ => {}
            }
        }
    }
}

fn touch_loop(sender: Sender<InputEvent>) {
    let Some((_, mut device)) = evdev::enumerate().find(|(_, d)| {
        let multitouch = d
            .supported_absolute_axes()
            .is_some_and(|a| a.contains(AbsoluteAxisCode::ABS_MT_POSITION_X));
        let stylus = d
            .supported_keys()
            .is_some_and(|k| k.contains(KeyCode::BTN_TOOL_PEN));
        let touchpad = d
            .supported_keys()
            .is_some_and(|k| k.contains(KeyCode::BTN_TOOL_FINGER))
            && d.name()
                .is_some_and(|n| n.to_ascii_lowercase().contains("touchpad"));
        multitouch && !stylus && !touchpad
    }) else {
        return; // no touchscreen: fingers simply do nothing
    };
    let (Some(ax), Some(ay)) = (
        Axis::of(&device, AbsoluteAxisCode::ABS_MT_POSITION_X),
        Axis::of(&device, AbsoluteAxisCode::ABS_MT_POSITION_Y),
    ) else {
        return;
    };

    const SLOTS: usize = 16;
    let mut active = [false; SLOTS];
    let mut pos = [(0i32, 0i32); SLOTS];
    let mut slot = 0usize;
    loop {
        let Ok(events) = device.fetch_events() else {
            return;
        };
        for event in events {
            match event.destructure() {
                EventSummary::AbsoluteAxis(_, AbsoluteAxisCode::ABS_MT_SLOT, v) => {
                    slot = (v.max(0) as usize).min(SLOTS - 1)
                }
                EventSummary::AbsoluteAxis(_, AbsoluteAxisCode::ABS_MT_TRACKING_ID, v) => {
                    active[slot] = v >= 0
                }
                EventSummary::AbsoluteAxis(_, AbsoluteAxisCode::ABS_MT_POSITION_X, v) => {
                    pos[slot].0 = v
                }
                EventSummary::AbsoluteAxis(_, AbsoluteAxisCode::ABS_MT_POSITION_Y, v) => {
                    pos[slot].1 = v
                }
                EventSummary::Synchronization(_, SynchronizationCode::SYN_REPORT, _) => {
                    let contacts = (0..SLOTS)
                        .filter(|&i| active[i])
                        .map(|i| (ax.norm(pos[i].0), ay.norm(pos[i].1)))
                        .collect();
                    if sender.send(InputEvent::Touch(contacts)).is_err() {
                        return;
                    }
                }
                _ => {}
            }
        }
    }
}
