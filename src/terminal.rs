//! An inline terminal: a shell on a PTY, with alacritty_terminal doing the
//! VT parsing, drawn as a block in the notebook (ctrl+` opens one). Its text
//! is snapshotted into the notebook, so after a restart it stays on the page
//! as a frozen transcript you can still select and comment on.

use std::{
    borrow::Cow,
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use alacritty_terminal::{
    event::{Event, EventListener, WindowSize},
    event_loop::{EventLoop, EventLoopSender, Msg},
    grid::Dimensions,
    index::{Column, Line},
    sync::FairMutex,
    term::{Config, Term, TermMode, cell::Flags, test::TermSize},
    tty,
    vte::ansi::{Color, NamedColor},
};
use serde::{Deserialize, Serialize};

/// Rows of the shell's screen; the block shows scrollback above them as it grows.
pub const ROWS: usize = 24;
/// Lines kept in a saved transcript and drawn on the page.
pub const KEEP: usize = 1000;

/// A run of same-styled text on one line.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Span {
    pub text: String,
    pub fg: u32,
    #[serde(default)]
    pub bg: Option<u32>,
    #[serde(default)]
    pub bold: bool,
}

pub type TermLine = Vec<Span>;

pub fn plain(line: &TermLine) -> String {
    line.iter().map(|s| s.text.as_str()).collect()
}

/// What the block draws: the lines, and where the cursor is among them.
#[derive(Clone, Default)]
pub struct Snapshot {
    pub lines: Vec<TermLine>,
    /// (line index into `lines`, column), when the cursor is shown.
    pub cursor: Option<(usize, usize)>,
}

#[derive(Clone)]
struct Listener {
    dirty: Arc<AtomicBool>,
    exited: Arc<AtomicBool>,
    tx: Arc<Mutex<Option<EventLoopSender>>>,
}

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        match event {
            Event::Wakeup => self.dirty.store(true, Ordering::Relaxed),
            // Answers to the shell's queries (cursor position etc.).
            Event::PtyWrite(text) => {
                if let Some(tx) = self.tx.lock().unwrap().as_ref() {
                    let _ = tx.send(Msg::Input(Cow::Owned(text.into_bytes())));
                }
            }
            Event::Exit | Event::ChildExit(_) => {
                self.exited.store(true, Ordering::Relaxed);
                self.dirty.store(true, Ordering::Relaxed);
            }
            _ => {}
        }
    }
}

pub struct Terminal {
    term: Arc<FairMutex<Term<Listener>>>,
    tx: EventLoopSender,
    dirty: Arc<AtomicBool>,
    exited: Arc<AtomicBool>,
    snapshot: Snapshot,
}

impl Terminal {
    pub fn spawn(cols: usize, id: u64) -> std::io::Result<Self> {
        let dirty = Arc::new(AtomicBool::new(true));
        let exited = Arc::new(AtomicBool::new(false));
        let tx_slot = Arc::new(Mutex::new(None));
        let listener = Listener { dirty: dirty.clone(), exited: exited.clone(), tx: tx_slot.clone() };
        let size = TermSize::new(cols, ROWS);
        let term = Arc::new(FairMutex::new(Term::new(Config::default(), &size, listener.clone())));
        let home = std::env::var("HOME").ok().map(Into::into);
        let options = tty::Options {
            shell: None,
            working_directory: home,
            drain_on_exit: true,
            env: HashMap::from([("TERM".into(), "xterm-256color".into()), ("COLORTERM".into(), "truecolor".into())]),
        };
        let window = WindowSize { num_lines: ROWS as u16, num_cols: cols as u16, cell_width: 8, cell_height: 18 };
        let pty = tty::new(&options, window, id)?;
        let event_loop = EventLoop::new(term.clone(), listener, pty, true, false)?;
        let tx = event_loop.channel();
        *tx_slot.lock().unwrap() = Some(tx.clone());
        event_loop.spawn();
        Ok(Self { term, tx, dirty, exited, snapshot: Snapshot::default() })
    }

    pub fn exited(&self) -> bool {
        self.exited.load(Ordering::Relaxed)
    }

    /// Re-read the grid if the shell wrote something. True when it changed.
    pub fn refresh(&mut self) -> bool {
        if !self.dirty.swap(false, Ordering::Relaxed) {
            return false;
        }
        self.snapshot = snapshot(&self.term.lock());
        true
    }

    pub fn snapshot(&self) -> &Snapshot {
        &self.snapshot
    }

    pub fn write(&self, bytes: Vec<u8>) {
        let _ = self.tx.send(Msg::Input(Cow::Owned(bytes)));
    }

    /// Arrow keys send different sequences when the program asked for "application cursor" mode.
    pub fn app_cursor(&self) -> bool {
        self.term.lock().mode().contains(TermMode::APP_CURSOR)
    }

    pub fn bracketed_paste(&self) -> bool {
        self.term.lock().mode().contains(TermMode::BRACKETED_PASTE)
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Shutdown);
    }
}

fn snapshot(term: &Term<Listener>) -> Snapshot {
    let grid = term.grid();
    let alt = term.mode().contains(TermMode::ALT_SCREEN);
    let top = if alt { 0 } else { -(grid.history_size() as i32) };
    let bottom = grid.screen_lines() as i32 - 1;
    let cursor = grid.cursor.point;
    let mut lines = Vec::new();
    for l in top..=bottom {
        let row = &grid[Line(l)];
        let mut spans: TermLine = Vec::new();
        for c in 0..grid.columns() {
            let cell = &row[Column(c)];
            if cell.flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
                continue;
            }
            let (mut fg, mut bg) = (color(term, cell.fg, true), color_bg(term, cell.bg));
            if cell.flags.contains(Flags::INVERSE) {
                let back = bg.unwrap_or(paper());
                bg = Some(fg);
                fg = back;
            }
            if cell.flags.contains(Flags::DIM) {
                fg = mix(fg, paper(), 0.4);
            }
            let bold = cell.flags.contains(Flags::BOLD);
            let ch = if cell.flags.contains(Flags::HIDDEN) { ' ' } else { cell.c };
            match spans.last_mut() {
                Some(s) if s.fg == fg && s.bg == bg && s.bold == bold => s.text.push(ch),
                _ => spans.push(Span { text: ch.to_string(), fg, bg, bold }),
            }
        }
        // Trailing blanks are not content.
        while let Some(s) = spans.last_mut() {
            if s.bg.is_some() {
                break;
            }
            let trimmed = s.text.trim_end().len();
            if trimmed == 0 {
                spans.pop();
            } else {
                s.text.truncate(trimmed);
                break;
            }
        }
        lines.push(spans);
    }
    let cursor_at = (cursor.line.0 - top) as usize;
    if !alt {
        // The screen's empty rows below the cursor aren't part of the transcript yet.
        let last = lines.iter().rposition(|l| !l.is_empty()).unwrap_or(0).max(cursor_at);
        lines.truncate(last + 1);
    }
    let skip = lines.len().saturating_sub(KEEP);
    lines.drain(..skip);
    let shown = term.mode().contains(TermMode::SHOW_CURSOR);
    Snapshot {
        cursor: (shown && cursor_at >= skip).then(|| (cursor_at - skip, cursor.column.0)),
        lines,
    }
}

// Colors for a terminal on warm paper (or dark paper): syncpi's ink and accents.
fn paper() -> u32 {
    crate::theme::pal().term_bg
}

fn ink() -> u32 {
    crate::theme::pal().term_fg
}

fn indexed(i: u8) -> u32 {
    match i {
        0..=15 => crate::theme::pal().ansi[i as usize],
        16..=231 => {
            let i = i - 16;
            let level = |v: u8| if v == 0 { 0 } else { 55 + v as u32 * 40 };
            (level(i / 36) << 16) | (level((i / 6) % 6) << 8) | level(i % 6)
        }
        _ => {
            let v = 8 + (i as u32 - 232) * 10;
            (v << 16) | (v << 8) | v
        }
    }
}

fn color(term: &Term<Listener>, c: Color, fg: bool) -> u32 {
    match c {
        Color::Spec(rgb) => ((rgb.r as u32) << 16) | ((rgb.g as u32) << 8) | rgb.b as u32,
        Color::Indexed(i) => match term.colors()[i as usize] {
            Some(rgb) => ((rgb.r as u32) << 16) | ((rgb.g as u32) << 8) | rgb.b as u32,
            None => indexed(i),
        },
        Color::Named(n) => match n {
            NamedColor::Foreground | NamedColor::BrightForeground | NamedColor::Cursor => ink(),
            NamedColor::DimForeground => mix(ink(), paper(), 0.4),
            NamedColor::Background => {
                if fg {
                    paper()
                } else {
                    ink()
                }
            }
            n if (n as usize) < 16 => crate::theme::pal().ansi[n as usize],
            n if (n as usize) >= NamedColor::DimBlack as usize => mix(crate::theme::pal().ansi[(n as usize - NamedColor::DimBlack as usize) % 8], paper(), 0.3),
            _ => ink(),
        },
    }
}

fn color_bg(term: &Term<Listener>, c: Color) -> Option<u32> {
    match c {
        Color::Named(NamedColor::Background) => None,
        c => Some(color(term, c, false)),
    }
}

fn mix(a: u32, b: u32, t: f32) -> u32 {
    let ch = |v: u32, s: u32| (((v >> s) & 0xff) as f32 * (1.0 - t) + ((b >> s) & 0xff) as f32 * t) as u32;
    (ch(a, 16) << 16) | (ch(a, 8) << 8) | ch(a, 0)
}

/// Bytes a key sends to the shell, as xterm would.
pub fn key_bytes(key: &str, ch: Option<&str>, ctrl: bool, alt: bool, app_cursor: bool) -> Option<Vec<u8>> {
    let arrow = |c: char| Some(if app_cursor { format!("\x1bO{c}") } else { format!("\x1b[{c}") }.into_bytes());
    let mut out = match key {
        "enter" => Some(b"\r".to_vec()),
        "backspace" => Some(if ctrl { vec![0x17] } else { vec![0x7f] }),
        "tab" => Some(b"\t".to_vec()),
        "up" => arrow('A'),
        "down" => arrow('B'),
        "right" => arrow('C'),
        "left" => arrow('D'),
        "home" => Some(b"\x1b[H".to_vec()),
        "end" => Some(b"\x1b[F".to_vec()),
        "delete" => Some(b"\x1b[3~".to_vec()),
        "pageup" => Some(b"\x1b[5~".to_vec()),
        "pagedown" => Some(b"\x1b[6~".to_vec()),
        "space" if ctrl => Some(vec![0]),
        k if ctrl && k.len() == 1 => {
            let c = k.as_bytes()[0].to_ascii_lowercase();
            match c {
                b'a'..=b'z' => Some(vec![c - b'a' + 1]),
                b'[' => Some(vec![0x1b]),
                b'\\' => Some(vec![0x1c]),
                b']' => Some(vec![0x1d]),
                _ => None,
            }
        }
        _ => ch.filter(|c| !c.is_empty()).map(|c| c.as_bytes().to_vec()),
    }?;
    if alt {
        out.insert(0, 0x1b);
    }
    Some(out)
}

/// Pasted text, wrapped when the program asked for bracketed paste.
pub fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    let text = text.replace("\r\n", "\r").replace('\n', "\r");
    if bracketed {
        format!("\x1b[200~{text}\x1b[201~").into_bytes()
    } else {
        text.into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys() {
        assert_eq!(key_bytes("c", Some("c"), true, false, false), Some(vec![3]));
        assert_eq!(key_bytes("up", None, false, false, true), Some(b"\x1bOA".to_vec()));
        assert_eq!(key_bytes("x", Some("x"), false, true, false), Some(b"\x1bx".to_vec()));
        assert_eq!(key_bytes("enter", None, false, false, false), Some(b"\r".to_vec()));
    }

    #[test]
    fn shell_runs() {
        let mut t = Terminal::spawn(80, 1).unwrap();
        t.write(b"echo sketch$((40+2))\r".to_vec());
        let start = std::time::Instant::now();
        loop {
            t.refresh();
            if t.snapshot().lines.iter().any(|l| plain(l).trim() == "sketch42") {
                break;
            }
            assert!(start.elapsed().as_secs() < 5, "no output: {:?}", t.snapshot().lines.iter().map(plain).collect::<Vec<_>>());
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
}
