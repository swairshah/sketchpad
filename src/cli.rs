//! `sketchpad ask|snapshot` — lets an agent (Claude, pi, scripts) pop the
//! pad open with a prompt and an optional figure, then block until the human
//! sends back a drawing. Prints one JSON line; exit 0 = drawing sent.

use std::{
    path::PathBuf,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use serde_json::{Value, json};

use crate::ipc;

const USAGE: &str = "\
sketchpad — a paper-like drawing pad you can share with an agent

USAGE:
  sketchpad skill setup|status|remove|show     teach coding agents to use sketchpad
  sketchpad                         open the pad
  sketchpad ask <prompt> [options]  pop the pad open, ask the human to draw,
                                    wait, print the result as JSON
  sketchpad snapshot                save the current page, print its path
  sketchpad browser …               search / read pages in your Chromium, in background tabs
  sketchpad draw '<json>'           draw / write / erase on the page as the agent
  sketchpad draw -                  (read the ops JSON from stdin)

ASK OPTIONS:
  --image <png|jpg>   figure placed on the page for the human to draw on
  --from <name>       who is asking (shown on the card, default \"Your agent\")
  --keep              keep the existing ink instead of starting a fresh page
  --timeout <secs>    give up waiting after this long (default: wait forever)

DRAW OPS (coordinates = pixels of the page PNG from ask/snapshot):
  {\"draw\":[{\"type\":\"arrow\",\"from\":[100,100],\"to\":[300,200]},
           {\"type\":\"circle\",\"cx\":400,\"cy\":300,\"r\":60,\"color\":\"blue\"},
           {\"type\":\"text\",\"x\":420,\"y\":380,\"text\":\"here\",\"size\":30}],
   \"erase\":[{\"x\":0,\"y\":0,\"w\":200,\"h\":100}],
   \"notes\":[{\"text\":\"margin note\",\"x\":1300,\"y\":200}]}
  types: line arrow circle ellipse rect polyline curve text highlight(x,y,w,h); optional color, tool (pen|pencil|marker)

OUTPUT (stdout, one line):
  {\"status\":\"sent\",\"image\":\"/…/drawing.png\",\"width\":1920,\"height\":1200,\"strokes\":12}
  {\"status\":\"skipped\"}   (human tapped Skip; exit code 3)
";

pub fn run(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("ask") => ask(&args[1..]),
        Some("draw") => draw(&args[1..]),
        Some("snapshot") => print(ensure_and_call(&json!({"cmd":"snapshot"}), None)),
        _ => {
            eprint!("{USAGE}");
            2
        }
    }
}

fn ask(args: &[String]) -> i32 {
    let mut prompt = None;
    let mut image = None;
    let mut from = None;
    let mut keep = false;
    let mut timeout = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--image" => image = it.next().cloned(),
            "--from" => from = it.next().cloned(),
            "--keep" => keep = true,
            "--timeout" => timeout = it.next().and_then(|s| s.parse::<u64>().ok()),
            "-h" | "--help" => {
                eprint!("{USAGE}");
                return 0;
            }
            other if prompt.is_none() => prompt = Some(other.to_owned()),
            other => {
                eprintln!("unexpected argument: {other}");
                return 2;
            }
        }
    }
    let Some(prompt) = prompt else {
        eprint!("{USAGE}");
        return 2;
    };
    let image = match image {
        Some(path) => match std::fs::canonicalize(&path) {
            Ok(p) => Some(p),
            Err(e) => {
                eprintln!("cannot read figure {path}: {e}");
                return 2;
            }
        },
        None => None,
    };
    let command = json!({
        "cmd": "ask",
        "prompt": prompt,
        "image": image.map(|p: PathBuf| p.to_string_lossy().into_owned()),
        "from": from,
        "keep": keep,
    });
    print(ensure_and_call(&command, timeout.map(Duration::from_secs)))
}

fn draw(args: &[String]) -> i32 {
    let raw = match args.first().map(String::as_str) {
        Some("-") | None => {
            let mut buf = String::new();
            if std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf).is_err() {
                eprintln!("cannot read ops from stdin");
                return 2;
            }
            buf
        }
        Some("--file") => match args.get(1).map(std::fs::read_to_string) {
            Some(Ok(s)) => s,
            _ => {
                eprintln!("cannot read ops file");
                return 2;
            }
        },
        Some(json) => json.to_owned(),
    };
    let ops: Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            println!(
                "{}",
                json!({"status": "error", "error": format!("invalid JSON: {e}")})
            );
            return 1;
        }
    };
    print(ensure_and_call(
        &json!({"cmd": "draw", "ops": ops}),
        Some(Duration::from_secs(30)),
    ))
}

fn print(result: Result<Value, String>) -> i32 {
    match result {
        Ok(value) => {
            println!("{value}");
            match value.get("status").and_then(Value::as_str) {
                Some("sent") | Some("ok") => 0,
                Some("error") => 1,
                _ => 3,
            }
        }
        Err(e) => {
            println!("{}", json!({"status": "error", "error": e}));
            1
        }
    }
}

/// Talk to the running pad, launching one (fullscreen) if needed.
fn ensure_and_call(command: &Value, timeout: Option<Duration>) -> Result<Value, String> {
    if !ipc::is_running() {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let log = std::fs::File::create("/tmp/sketchpad.log").map_err(|e| e.to_string())?;
        use std::os::unix::process::CommandExt;
        Command::new(exe)
            .arg("--launched-by-cli")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(log))
            .process_group(0)
            .spawn()
            .map_err(|e| format!("cannot launch sketchpad: {e}"))?;
        let start = Instant::now();
        while !ipc::is_running() {
            if start.elapsed() > Duration::from_secs(20) {
                return Err("sketchpad did not start (see /tmp/sketchpad.log)".into());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    ipc::call(command, timeout).map_err(|e| match e.kind() {
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => {
            "timed out waiting for the human".to_owned()
        }
        _ => e.to_string(),
    })
}
