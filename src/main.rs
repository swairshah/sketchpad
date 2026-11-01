mod app;
mod bridge;
mod cli;
mod diff;
mod durable;
mod export;
mod fonts;
mod geometry;
mod ink;
mod input;
mod ipc;
mod markdown;
mod notebook;
mod terminal;
mod theme;
mod ops;
mod skill;
mod ui;
mod vim;

use std::sync::mpsc;

use gpui::{App, Application, TitlebarOptions, WindowOptions, prelude::*};
use serde_json::json;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let launched_by_cli = args.iter().any(|a| a == "--launched-by-cli");
    match args.first().map(String::as_str) {
        // Chromium starting us as the extension's native-messaging host.
        Some(origin) if origin.starts_with("chrome-extension://") => std::process::exit(bridge::host()),
        Some("browser") => std::process::exit(bridge::cli(&args[1..])),
        Some("skill") => std::process::exit(skill::run(&SKILL, &args[1..])),
        Some("__internal") => std::process::exit(skill::internal(&SKILL, &args[1..])),
        _ => {}
    }
    if let Some(first) = args.first().filter(|a| !a.starts_with("--launched")) {
        if first != "--app" {
            std::process::exit(cli::run(&args));
        }
    }

    // One pad at a time: a second launch just brings the first forward.
    if ipc::is_running() {
        let _ = ipc::call(&json!({"cmd": "focus"}), None);
        return;
    }

    let (ipc_tx, ipc_rx) = mpsc::channel();
    if let Err(e) = ipc::serve(ipc_tx) {
        eprintln!("sketchpad: cannot open control socket: {e}");
    }

    let (input_tx, input_rx) = mpsc::channel();
    input::spawn(input_tx);

    let (agent_req_tx, agent_req_rx) = mpsc::channel();
    let (agent_evt_tx, agent_evt_rx) = mpsc::channel();
    durable::spawn(agent_req_rx, agent_evt_tx);

    Application::new().run(move |cx: &mut App| {
        if let Err(e) = cx.text_system().add_fonts(fonts::all()) {
            eprintln!("sketchpad: could not load bundled fonts: {e}");
        }
        let channels = app::Channels {
            input: input_rx,
            agent_events: agent_evt_rx,
            agent: agent_req_tx,
            ipc: ipc_rx,
        };
        let options = WindowOptions {
            app_id: Some("sketchpad".into()),
            titlebar: Some(TitlebarOptions {
                title: Some("Sketchpad".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        cx.open_window(options, move |window, cx| {
            window.toggle_fullscreen();
            cx.new(|cx| app::Pad::new(channels, launched_by_cli, window, cx))
        })
        .expect("failed to open window");
        cx.on_window_closed(|cx| cx.quit()).detach();
        cx.activate(true);
    });

    let _ = std::fs::remove_file(ipc::socket_path());
}

pub const SKILL: skill::Spec = skill::Spec {
    name: "sketchpad",
    description: "Ask the human to draw something by hand on their pen-enabled touchscreen, optionally on top of a figure you provide, and get the drawing back as a PNG. Use when a sketch, diagram, annotation, handwriting, layout mock-up, or \"show me\" answer from the user would be faster or clearer than text, or when you want them to mark up an image (circle, label, correct, fill in).",
    guide: include_str!("guide.md"),
    session_note: session_note,
};

/// One line for a new agent session: what is here and whether it's live.
fn session_note() -> Option<String> {
    // Only useful on a machine that can actually show it.
    std::env::var_os("WAYLAND_DISPLAY")?;
    let stylus = std::fs::read_to_string("/proc/bus/input/devices")
        .map(|d| d.to_ascii_lowercase().contains("stylus"))
        .unwrap_or(false);
    let running = ipc::is_running();
    Some(format!(
        "The `sketchpad` CLI is installed: a Paper-style drawing pad on this machine ({}); the pad is {}. When a sketch from the human (or marking up an image you made) beats text, use it; run `sketchpad __internal skill` first for the commands.",
        if stylus {
            "with a pen/stylus"
        } else {
            "mouse only, no stylus found"
        },
        if running {
            "open"
        } else {
            "closed (ask/draw opens it)"
        }
    ))
}
