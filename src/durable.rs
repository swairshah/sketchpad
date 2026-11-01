//! Bridge to the notebook's pi-durable agent (agent/notebook-agent.ts), a
//! Node child speaking one JSON object per line on stdio. The conversation
//! lives in ~/.local/share/sketchpad/notebook.sqlite, so it outlives the app.

use std::{
    io::{BufRead, BufReader, Write},
    path::PathBuf,
    process::{Command, Stdio},
    sync::mpsc::{Receiver, Sender},
    thread,
};

use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};

pub enum ToAgent {
    Send {
        request_id: String,
        text: String,
        images: Vec<Vec<u8>>,
        /// Caption the new ink with a small model (shown instead of "N strokes").
        describe: bool,
        typed: String,
    },
    CanvasResult {
        call_id: String,
        result: Value,
    },
    /// Ask Jev (the pause classifier) whether to respond to the handwriting.
    ShouldRespond {
        request_id: String,
        image: Vec<u8>,
        text: String,
    },
    Abort,
    Reset,
}

#[derive(Debug)]
pub enum FromAgent {
    Ready(String),
    Busy(bool),
    /// The assistant message being written, so far.
    Partial(String),
    /// That message, finished.
    Final(String),
    Canvas {
        call_id: String,
        op: String,
        payload: Value,
    },
    Tool {
        name: String,
        summary: String,
    },
    Jev {
        model: Option<String>,
        reason: String,
    },
    Verdict {
        request_id: String,
        probability: Option<f32>,
        error: Option<String>,
    },
    Described {
        request_id: String,
        text: String,
    },
    Error(String),
    Exited(String),
}

fn script() -> PathBuf {
    if let Ok(p) = std::env::var("SKETCHPAD_AGENT") {
        return PathBuf::from(p);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("agent/notebook-agent.ts")
}

pub fn spawn(requests: Receiver<ToAgent>, events: Sender<FromAgent>) {
    thread::spawn(move || {
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open("/tmp/sketchpad-agent.log")
            .map(Stdio::from)
            .unwrap_or_else(|_| Stdio::null());
        let child = Command::new("node")
            .arg(script())
            // The agent's browser tools call `sketchpad browser …` on this very binary.
            .env("SKETCHPAD_BIN", std::env::current_exe().unwrap_or_else(|_| "sketchpad".into()))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(log)
            .spawn();
        let mut child = match child {
            Ok(c) => c,
            Err(e) => {
                let _ = events.send(FromAgent::Exited(format!("cannot start the agent (node): {e}")));
                return;
            }
        };
        let mut stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();

        // Reader thread: agent -> app.
        let reader_events = events.clone();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
                let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
                let event = match v.get("type").and_then(Value::as_str) {
                    Some("ready") => FromAgent::Ready(s("model")),
                    Some("busy") => FromAgent::Busy(v.get("busy").and_then(Value::as_bool).unwrap_or(false)),
                    Some("partial") => FromAgent::Partial(s("text")),
                    Some("final") => FromAgent::Final(s("text")),
                    Some("canvas") => FromAgent::Canvas {
                        call_id: s("callId"),
                        op: s("op"),
                        payload: v.clone(),
                    },
                    Some("tool") => FromAgent::Tool {
                        name: s("name"),
                        summary: s("summary"),
                    },
                    Some("jev") => FromAgent::Jev {
                        model: v.get("model").and_then(Value::as_str).map(str::to_owned),
                        reason: s("reason"),
                    },
                    Some("verdict") => FromAgent::Verdict {
                        request_id: s("requestId"),
                        probability: v.get("probability").and_then(Value::as_f64).map(|p| p as f32),
                        error: v.get("error").and_then(Value::as_str).map(str::to_owned),
                    },
                    Some("described") => FromAgent::Described {
                        request_id: s("requestId"),
                        text: s("text"),
                    },
                    Some("error") => FromAgent::Error(s("message")),
                    _ => continue,
                };
                if reader_events.send(event).is_err() {
                    break;
                }
            }
            let _ = reader_events.send(FromAgent::Exited("the agent stopped (see /tmp/sketchpad-agent.log)".into()));
        });

        // Writer: app -> agent.
        for request in requests {
            let message = match request {
                ToAgent::Send {
                    request_id,
                    text,
                    images,
                    describe,
                    typed,
                } => json!({
                    "type": "send",
                    "requestId": request_id,
                    "text": text,
                    "describe": describe,
                    "typed": typed,
                    "images": images
                        .iter()
                        .map(|png| json!({"data": STANDARD.encode(png), "mimeType": "image/png"}))
                        .collect::<Vec<_>>(),
                }),
                ToAgent::CanvasResult { call_id, result } => {
                    json!({"type": "canvas_result", "callId": call_id, "result": result})
                }
                ToAgent::ShouldRespond { request_id, image, text } => json!({
                    "type": "should_respond",
                    "requestId": request_id,
                    "image": STANDARD.encode(image),
                    "text": text,
                }),
                ToAgent::Abort => json!({"type": "abort"}),
                ToAgent::Reset => json!({"type": "reset"}),
            };
            if writeln!(stdin, "{message}").and_then(|_| stdin.flush()).is_err() {
                break;
            }
        }
        drop(stdin);
        let _ = child.kill();
        let _ = child.wait();
    });
}
