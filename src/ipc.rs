//! Unix-socket control plane: one JSON object per line in each direction.
//!
//!   {"cmd":"ask","prompt":"...","image":"/abs/fig.png","from":"Claude","keep":false}
//!       -> (after the human taps Send/Skip)
//!          {"status":"sent","image":"/…/drawing.png","width":..,"height":..,"strokes":n}
//!          {"status":"skipped"} | {"status":"superseded"} | {"status":"closed"}
//!   {"cmd":"draw","ops":{"draw":[...],"erase":[...],"notes":[...]}} -> {"status":"ok",...}
//!   {"cmd":"snapshot"} -> {"status":"ok","image":"/…/snapshot.png",...}
//!   {"cmd":"focus"}    -> {"status":"ok"}

use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::{UnixListener, UnixStream},
    path::PathBuf,
    sync::mpsc::{self, Sender},
    thread,
};

use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Debug, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Command {
    Ask {
        prompt: String,
        #[serde(default)]
        image: Option<PathBuf>,
        #[serde(default)]
        from: Option<String>,
        #[serde(default)]
        keep: bool,
    },
    Draw {
        ops: serde_json::Value,
    },
    Snapshot,
    Focus,
}

pub struct Request {
    pub command: Command,
    pub reply: Sender<Value>,
}

pub fn socket_path() -> PathBuf {
    match std::env::var("XDG_RUNTIME_DIR") {
        Ok(dir) => PathBuf::from(dir).join("sketchpad.sock"),
        Err(_) => PathBuf::from(format!("/tmp/sketchpad-{}.sock", user_name())),
    }
}

fn user_name() -> String {
    std::env::var("USER").unwrap_or_else(|_| "user".into())
}

/// Bind the socket and forward requests to the UI thread.
pub fn serve(requests: Sender<Request>) -> std::io::Result<()> {
    let path = socket_path();
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let requests = requests.clone();
            thread::spawn(move || handle(stream, requests));
        }
    });
    Ok(())
}

fn handle(stream: UnixStream, requests: Sender<Request>) {
    let mut line = String::new();
    let Ok(read) = stream.try_clone() else { return };
    if BufReader::new(read).read_line(&mut line).is_err() {
        return;
    }
    let reply = match serde_json::from_str::<Command>(line.trim()) {
        Ok(command) => {
            let (tx, rx) = mpsc::channel();
            if requests.send(Request { command, reply: tx }).is_err() {
                json!({"status": "closed"})
            } else {
                rx.recv().unwrap_or_else(|_| json!({"status": "closed"}))
            }
        }
        Err(e) => json!({"status": "error", "error": format!("bad request: {e}")}),
    };
    let mut stream = stream;
    let _ = writeln!(stream, "{reply}");
    let _ = stream.flush();
}

/// Send one command to a running instance and wait for its reply line.
pub fn call(command: &Value, timeout: Option<std::time::Duration>) -> std::io::Result<Value> {
    let mut stream = UnixStream::connect(socket_path())?;
    stream.set_read_timeout(timeout)?;
    writeln!(stream, "{command}")?;
    stream.flush()?;
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line)?;
    serde_json::from_str(line.trim()).map_err(std::io::Error::other)
}

pub fn is_running() -> bool {
    UnixStream::connect(socket_path()).is_ok()
}
