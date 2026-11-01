//! The notebook: an endless sheet of warm paper. You type in a vim editor at
//! the bottom, draw anywhere with the pen, and a pi-durable agent answers
//! inline with typeset markdown, highlighted code, svg diagrams and
//! hand-drawn sketches. A checkpoint rail on the left maps the conversation;
//! the tools hide behind a floating button you can drag anywhere.

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet, VecDeque},
    hash::{Hash, Hasher},
    path::PathBuf,
    rc::Rc,
    sync::mpsc::{Receiver, Sender},
    time::{Duration, Instant},
};

use gpui::{
    Animation, AnimationExt, AnyElement, Bounds, Context, CursorStyle, FocusHandle, Font,
    FontStyle, FontWeight, Hsla, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent,
    Pixels, Render, ScrollWheelEvent, SharedString, StyledText, TextLayout, TextRun, Timer, UnderlineStyle, Window,
    canvas, div, font, img, point, prelude::*, pulsating_between, px, rgb, rgba,
};
use serde_json::{Value, json};

use crate::{
    diff,
    durable::{FromAgent, ToAgent},
    export::{self, Figure},
    ink::{self, History, Sample, Stroke, Tool},
    input::InputEvent,
    ipc::{Command, Request},
    markdown::{self, Block as Md, Rich},
    notebook::{self, Block, Checkpoint, Notebook, Part},
    terminal::{self, Terminal},
    ops::{self, Note, Ops},
    theme::{self, mono, sans, serif},
    ui::{self, Action, COLORS, Dock, Layout, Rect},
    vim::{self, Editor, Mode},
};

// syncpi's typography, in whatever families `:config` picked.
// The bundled static fonts name each weight as its own family.
const SANS_MEDIUM: &str = "IBM Plex Sans Medium";
const SANS_SEMI: &str = "IBM Plex Sans SemiBold";
const SERIF_MEDIUM: &str = "Newsreader Medium";
const MONO_MEDIUM: &str = "IBM Plex Mono Medium";

/// A font at a weight, by family name (see above); other families take the weight as asked.
fn weighted(family: impl AsRef<str>, weight: FontWeight) -> Font {
    let name = match (family.as_ref(), weight >= FontWeight::SEMIBOLD, weight >= FontWeight::MEDIUM) {
        (theme::PLEX_SANS, true, _) => SANS_SEMI,
        (theme::PLEX_SANS, false, true) => SANS_MEDIUM,
        (theme::NEWSREADER, _, true) => SERIF_MEDIUM,
        (theme::PLEX_MONO, _, true) => MONO_MEDIUM,
        (other, _, _) => return Font { weight, ..font(other.to_owned()) },
    };
    font(name.to_owned())
}

/// The palette of the paper you picked (light or dark).
fn p() -> &'static theme::Palette {
    theme::pal()
}

/// A palette color at an alpha.
fn tint(c: u32, a: u8) -> gpui::Rgba {
    rgba((c << 8) | a as u32)
}
const ERASER_R: f32 = 14.0;
/// Width of the text column.
const COL_W: f32 = 760.0;
const EDITOR_GUTTER: f32 = 52.0;
const EDITOR_FONT: f32 = 15.0;
const EDITOR_LH: f32 = 24.0;
const STATUS_H: f32 = 30.0;
const BLOCK_GAP: f32 = 26.0;
const COLUMN_TOP: f32 = 64.0;
const SKETCH_W: f32 = 760.0;
// The inline terminal: IBM Plex Mono advances 0.6 em per cell.
const TERM_FONT: f32 = 13.0;
const TERM_LH: f32 = 19.0;
const TERM_CELL: f32 = TERM_FONT * 0.6;
const TERM_PAD: f32 = 14.0;
const TERM_HEAD: f32 = 30.0;
/// The `block` of an anchor on a drawing on the page (its `item` is the drawing's key).
const INK_BLOCK: usize = usize::MAX;

/// A place the thread cursor can be: a line of a message of yours, a
/// top-level block of a reply (paragraph, heading, list, code, diagram), a
/// sketch, or a figure.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Anchor {
    block: usize,
    part: usize,
    item: usize,
}

struct CommentDraft {
    at: Anchor,
    excerpt: String,
    editor: Editor,
}

struct Pending {
    block: usize,
    reply: Sender<Value>,
    from: String,
}

/// Agent ink being drawn in, a few samples per frame.
enum Target {
    Page,
    Sketch(usize, usize),
}

struct Anim {
    target: Target,
    stroke: Stroke,
    shown: usize,
}

struct FabPress {
    start: (f32, f32),
    fab: (f32, f32),
    moved: bool,
}

struct Gesture {
    start: Instant,
    start_pos: (f32, f32),
    last: (f32, f32),
    max_contacts: usize,
    moved: f32,
    fab: Option<(f32, f32)>,
    /// A finger drawing (one contact that started on paper).
    inking: bool,
    last_at: Instant,
    velocity: f32,
    /// Started on a button: a tap, never ink.
    on_chrome: bool,
}

/// Measured during paint: where things ended up (document y = screen y + scroll).
#[derive(Default)]
struct Measured {
    /// Document rects of thread-cursor anchors.
    anchors: HashMap<Anchor, Rect>,
    /// (top, height) per block, then the composer.
    blocks: Vec<(f32, f32)>,
    /// Screen rects of buttons that live inside the document (figure Send/Skip).
    hits: Vec<(Rect, Action)>,
    /// Document rects of figure images, by block.
    figures: HashMap<usize, Rect>,
    /// Screen origin of the composer's text area.
    editor_origin: (f32, f32),
    /// Text layouts of the anchors the cursor moves through character by character.
    layouts: HashMap<Anchor, TextLayout>,
    /// Document (top, bottom) of the comment box being written.
    comment: Option<(f32, f32)>,
}

/// Strokes on the page drawn close together in time and space: one drawing,
/// selected, commented on and removed as a whole.
struct InkGroup {
    /// The origin id of its first stroke.
    key: u64,
    ids: Vec<u64>,
    rect: Rect,
    /// When its latest stroke was drawn (unix seconds), if known.
    t: Option<f64>,
    by_agent: bool,
}

pub struct Channels {
    pub input: Receiver<InputEvent>,
    pub agent_events: Receiver<FromAgent>,
    pub agent: Sender<ToAgent>,
    pub ipc: Receiver<Request>,
}

pub struct Pad {
    nb: Notebook,
    history: History,
    editor: Editor,
    focus: FocusHandle,

    tool: Tool,
    color: usize,
    drawing: bool,
    erase_before: Option<Vec<Stroke>>,
    erase_changed: bool,
    last_erase: Option<(f32, f32)>,

    viewport: (f32, f32),
    /// The notebook's scale on screen (it shrinks to fit a narrow window).
    zoom: f32,
    /// The human's text size (ctrl+= / ctrl+- / ctrl+0), a factor on `zoom`.
    text_scale: f32,
    scale: f32,
    /// Where the window is on screen (for mapping the pen/touch digitizer).
    geometry: crate::geometry::Shared,
    window_active: bool,
    scroll: f32,
    scroll_target: Option<f32>,
    /// Fling velocity after a finger scroll (px/s), decaying.
    momentum: f32,
    last_tick: Instant,
    follow: bool,
    measured: Rc<RefCell<Measured>>,
    /// Visual rows of the editor: (line, first char, end char).
    rows: Vec<(usize, usize, usize)>,
    /// x of every char boundary, per editor line.
    line_xs: Vec<Vec<f32>>,

    tray_open: bool,
    fab: (f32, f32),
    fab_press: Option<FabPress>,
    mouse: Option<(f32, f32)>,
    pen_hover: Option<(f32, f32)>,
    pen_in_range: bool,
    rubber: bool,
    pen_down: bool,
    pen_on_chrome: bool,
    pi_press: Option<Instant>,
    gesture: Option<Gesture>,
    last_action: Option<(Action, Instant)>,
    g_pressed: Option<Instant>,

    agent: Sender<ToAgent>,
    model: Option<String>,
    agent_busy: bool,
    agent_down: Option<String>,
    msg_open: bool,
    anims: VecDeque<Anim>,
    /// Let Jev decide when a pause in handwriting deserves an answer.
    listen: bool,
    jev: Option<String>,
    jev_note: Option<String>,
    jev_pending: Option<String>,
    pen_new_ink: bool,
    pen_lift_at: Option<Instant>,
    last_jev: Option<Instant>,
    last_ink_change: Option<Instant>,
    /// The thread cursor (normal mode in the conversation); None = in the composer.
    thread: Option<Anchor>,
    anchors: Vec<(Anchor, String)>,
    comment: Option<CommentDraft>,
    /// Byte offset of the cursor in the thread anchor's text.
    caret: usize,
    /// Where the selection started (v, V or a mouse drag); it ends at the cursor.
    sel_from: Option<(Anchor, usize)>,
    /// V: whole lines rather than characters.
    sel_lines: bool,
    /// A mouse drag is selecting text.
    mouse_select: bool,
    anchor_index: HashMap<Anchor, usize>,
    ink_groups: Vec<InkGroup>,
    /// Running shells, by terminal block id.
    terms: HashMap<u64, Terminal>,
    /// The terminal the keyboard types into.
    term_focus: Option<u64>,
    /// Scroll the comment box into view once it's laid out (it just opened, or grew).
    reveal_comment: bool,
    /// The last object removed with backspace (index, block), for ctrl+z.
    removed: Option<(usize, Block)>,
    comment_rows: (Vec<(usize, usize, usize)>, Vec<Vec<f32>>),
    stroke_by_pen: bool,

    pending: Option<Pending>,
    /// An agent asking before a consequential action (canvas call id, what it wants to do).
    confirm: Option<(String, String)>,
    /// The ink as last sent to the agent (for exact erase ghosts).
    sent_ink: RefCell<Option<Vec<Stroke>>>,
    figures: HashMap<PathBuf, Figure>,
    last_export: Option<((f32, f32), f32)>,
    md_cache: RefCell<HashMap<u64, Rc<Vec<Md>>>>,
    save_at: Option<Instant>,
    toast: Option<(String, Instant)>,
    launched_by_cli: bool,
    /// The `:config` menu, while it is open.
    config: Option<ConfigMenu>,
    /// Every font family on the machine (filled the first time a picker opens).
    font_names: Rc<Vec<SharedString>>,
}

/// The rows of the `:config` menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConfigRow {
    Theme,
    Font(theme::Role),
    Size,
}

const CONFIG_ROWS: [ConfigRow; 5] = [
    ConfigRow::Theme,
    ConfigRow::Font(theme::Role::Headings),
    ConfigRow::Font(theme::Role::Text),
    ConfigRow::Font(theme::Role::Code),
    ConfigRow::Size,
];

struct ConfigMenu {
    row: usize,
    picker: Option<FontPicker>,
}

/// Choosing a family for one role: type to filter, the page previews as you move.
struct FontPicker {
    role: theme::Role,
    query: String,
    sel: usize,
    /// The family before the picker opened, put back on Esc.
    before: SharedString,
}

thread_local! {
    /// How much the notebook is scaled to fit the window (1 = full size).
    static ZOOM: std::cell::Cell<f32> = const { std::cell::Cell::new(1.0) };
}

fn zoom() -> f32 {
    ZOOM.with(|z| z.get())
}

/// Document px -> screen px at the current zoom.
fn zp(v: f32) -> Pixels {
    px(v * zoom())
}

fn hash(text: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
}

fn hsla(c: u32) -> Hsla {
    rgb(c).into()
}

impl Pad {
    pub fn new(
        channels: Channels,
        launched_by_cli: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let Channels {
            input,
            agent_events,
            agent,
            ipc,
        } = channels;
        markdown::warm_up();

        cx.spawn_in(window, async move |this, cx| {
            loop {
                Timer::after(Duration::from_millis(8)).await;
                let inputs: Vec<_> = input.try_iter().collect();
                let events: Vec<_> = agent_events.try_iter().collect();
                let requests: Vec<_> = ipc.try_iter().collect();
                let alive = this.update_in(cx, |this, window, cx| {
                    let mut dirty = false;
                    for e in inputs {
                        dirty |= this.on_input(e, cx);
                    }
                    for e in events {
                        this.on_agent(e);
                        dirty = true;
                    }
                    for r in requests {
                        this.on_request(r, window);
                        dirty = true;
                    }
                    dirty |= this.tick(cx);
                    if dirty {
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        })
        .detach();

        let focus = cx.focus_handle();
        focus.focus(window);
        let nb = Notebook::load(&notebook::path());
        let mut editor = Editor::new();
        editor.set_text(&nb.draft);
        editor.row = editor.lines.len() - 1;
        editor.col = editor.lines[editor.row].len();
        let settings = load_settings();
        theme::set_dark(settings.get("theme").and_then(Value::as_str) == Some("dark"));
        for role in theme::Role::ALL {
            if let Some(name) = settings.get(role.key()).and_then(Value::as_str) {
                theme::set_family(role, name.to_owned());
            }
        }
        let fab = settings
            .get("fab")
            .and_then(|v| Some((v.get(0)?.as_f64()? as f32, v.get(1)?.as_f64()? as f32)))
            .unwrap_or((f32::MAX, f32::MAX));
        let msg_open = false;

        let mut pad = Self {
            nb,
            history: History::default(),
            editor,
            focus,
            tool: Tool::Pen,
            color: 0,
            drawing: false,
            erase_before: None,
            erase_changed: false,
            last_erase: None,
            viewport: (1280.0, 800.0),
            zoom: 1.0,
            text_scale: settings.get("text_scale").and_then(Value::as_f64).map_or(1.0, |v| v as f32),
            scale: 1.5,
            geometry: crate::geometry::spawn(),
            window_active: true,
            scroll: 0.0,
            scroll_target: None,
            momentum: 0.0,
            last_tick: Instant::now(),
            follow: true,
            measured: Rc::new(RefCell::new(Measured::default())),
            rows: Vec::new(),
            line_xs: Vec::new(),
            tray_open: false,
            fab,
            fab_press: None,
            mouse: None,
            pen_hover: None,
            pen_in_range: false,
            rubber: false,
            pen_down: false,
            pen_on_chrome: false,
            pi_press: None,
            gesture: None,
            last_action: None,
            g_pressed: None,
            agent,
            model: None,
            agent_busy: false,
            agent_down: None,
            msg_open,
            anims: VecDeque::new(),
            listen: load_settings().get("listen").and_then(Value::as_bool).unwrap_or(true),
            jev: None,
            jev_note: None,
            jev_pending: None,
            pen_new_ink: false,
            pen_lift_at: None,
            last_jev: None,
            last_ink_change: None,
            thread: None,
            anchors: Vec::new(),
            comment: None,
            caret: 0,
            sel_from: None,
            sel_lines: false,
            mouse_select: false,
            anchor_index: HashMap::new(),
            ink_groups: Vec::new(),
            terms: HashMap::new(),
            term_focus: None,
            removed: None,
            reveal_comment: false,
            comment_rows: (Vec::new(), Vec::new()),
            stroke_by_pen: false,
            pending: None,
            confirm: None,
            sent_ink: RefCell::new(None),
            figures: HashMap::new(),
            last_export: None,
            md_cache: RefCell::new(HashMap::new()),
            save_at: None,
            toast: None,
            launched_by_cli,
            config: None,
            font_names: Rc::new(Vec::new()),
        };
        // A run cut off by a crash is resumed by the agent and streams again.
        if let Some(parts) = pad.nb.open_agent() {
            parts.retain(|p| !matches!(p, Part::Text(t) if t.trim().is_empty()));
        }
        pad.scroll_target = Some(f32::MAX); // land on the composer
        pad
    }

    // ------------------------------------------------------------ helpers

    fn layout(&self) -> Layout {
        let (w, h) = self.viewport;
        let fab = if self.fab.0 == f32::MAX { (52.0, h - STATUS_H - 52.0) } else { self.fab };
        Layout::new(w, h - STATUS_H, fab, self.tray_open)
    }

    /// Left edge of the text column (document x is measured from the centre line).
    fn col_x(&self) -> f32 {
        -COL_W / 2.0
    }

    /// Height of the visible page, in document px.
    fn vh(&self) -> f32 {
        (self.viewport.1 - STATUS_H) / self.zoom
    }

    /// Window px -> document px (x from the centre line, y from the top).
    fn to_doc(&self, x: f32, y: f32) -> (f32, f32) {
        ((x - self.viewport.0 / 2.0) / self.zoom, y / self.zoom + self.scroll)
    }

    /// The document region on screen, in document px.
    fn view_rect(&self) -> Rect {
        let z = self.zoom;
        Rect::new(-self.viewport.0 / 2.0 / z, self.scroll, self.viewport.0 / z, self.viewport.1 / z)
    }

    /// Digitizer position (normalized over the built-in display) -> window px.
    fn map_input(&self, nx: f32, ny: f32) -> (f32, f32) {
        match self.geometry.lock().ok().and_then(|g| *g) {
            Some(g) => g.map(nx, ny),
            None => (nx * self.viewport.0, ny * self.viewport.1),
        }
    }

    fn toast(&mut self, text: impl Into<String>) {
        self.toast = Some((text.into(), Instant::now()));
    }

    fn touch(&mut self) {
        self.save_at = Some(Instant::now() + Duration::from_millis(800));
    }

    fn effective_tool(&self) -> Tool {
        if self.rubber { Tool::Eraser } else { self.tool }
    }

    fn composer_top(&self) -> Option<f32> {
        let m = self.measured.borrow();
        m.blocks.get(self.nb.blocks.len()).map(|b| b.0)
    }

    /// Bottom of the last block that's part of the conversation (not the composer).
    fn last_block_bottom(&self) -> f32 {
        let n = self.nb.blocks.len();
        if n == 0 {
            return COLUMN_TOP - BLOCK_GAP;
        }
        // Blocks added this frame aren't measured yet: use the last one that is.
        let m = self.measured.borrow();
        m.blocks
            .iter()
            .take(n)
            .filter(|(t, h)| *t != 0.0 || *h != 0.0)
            .map(|(t, h)| t + h)
            .fold(COLUMN_TOP - BLOCK_GAP, f32::max)
    }

    /// Handwriting below the conversation pushes the composer (and everything
    /// after it) down, so text never runs over ink. Returns the extra room.
    fn ink_room(&self) -> f32 {
        let floor_from = self.last_block_bottom() + 2.0;
        let mut floor = None::<f32>;
        let strokes = self
            .nb
            .ink
            .iter()
            .chain(self.anims.iter().filter(|a| matches!(a.target, Target::Page)).map(|a| &a.stroke));
        for s in strokes {
            let b = s.shape().1;
            if b[1] > floor_from {
                floor = Some(floor.map_or(b[3], |f: f32| f.max(b[3])));
            }
        }
        for n in &self.nb.notes {
            if n.y > floor_from {
                let bottom = n.y + n.size * 1.4;
                floor = Some(floor.map_or(bottom, |f: f32| f.max(bottom)));
            }
        }
        // Natural composer top = last bottom + gap; with a spacer there are two gaps.
        floor.map_or(0.0, |f| (f + 28.0 - self.last_block_bottom() - 2.0 * BLOCK_GAP).max(0.0))
    }

    fn content_bottom(&self) -> f32 {
        let m = self.measured.borrow();
        let blocks = m.blocks.last().map_or(0.0, |(t, h)| t + h);
        let ink = self.nb.ink.iter().map(|s| s.shape().1[3]).fold(0.0f32, f32::max);
        blocks.max(ink + 40.0)
    }

    fn max_scroll(&self) -> f32 {
        // You can always scroll a little past the last thing: endless paper.
        (self.content_bottom() - self.vh() * 0.25).max(0.0)
    }

    /// Scroll directly (a finger dragging the page: 1:1, no easing).
    fn scroll_by(&mut self, dy: f32) {
        self.scroll = (self.scroll + dy).clamp(0.0, self.max_scroll().max(self.scroll));
        self.scroll_target = None;
        self.momentum = 0.0;
        if dy < 0.0 {
            self.follow = false;
        }
    }

    /// Scroll smoothly (wheel, keys): move the target, let the page glide there.
    fn glide_by(&mut self, dy: f32) {
        let base = self.scroll_target.filter(|t| *t != f32::MAX).unwrap_or(self.scroll);
        let max = self.max_scroll().max(self.scroll);
        self.scroll_target = Some((base + dy).clamp(0.0, max));
        self.momentum = 0.0;
        if dy < 0.0 {
            self.follow = false;
        }
    }

    fn hit(&self, x: f32, y: f32) -> Option<Action> {
        self.layout().hit(x, y).or_else(|| {
            let m = self.measured.borrow();
            m.hits.iter().find(|(r, _)| r.contains(x, y)).map(|(_, a)| *a)
        }).or_else(|| self.rail_hit(x, y))
    }

    fn is_chrome(&self, x: f32, y: f32) -> bool {
        self.layout().is_chrome(x, y) || self.hit(x, y).is_some()
    }

    // ------------------------------------------------------------- rail

    /// Marks on the checkpoint rail: (screen y, block, is agent checkpoint, title).
    fn rail(&self) -> Vec<(f32, usize, bool, String, String)> {
        let mut marks: Vec<(usize, bool, String, String)> = Vec::new();
        for (i, b) in self.nb.blocks.iter().enumerate() {
            if let Block::User { text, attachment, .. } = b {
                let title = if text.trim().is_empty() {
                    attachment.clone().filter(|a| !a.is_empty()).unwrap_or_else(|| "Handwriting".into())
                } else {
                    text.lines().next().unwrap_or_default().chars().take(70).collect()
                };
                marks.push((i, false, title, String::new()));
            }
            if let Block::Figure { prompt, from, .. } = b {
                marks.push((i, false, format!("{from}: {prompt}"), String::new()));
            }
            for c in self.nb.checkpoints.iter().filter(|c| c.block == i) {
                marks.push((i, c.agent, c.title.clone(), c.summary.clone()));
            }
        }
        let n = marks.len();
        if n == 0 {
            return Vec::new();
        }
        let h = self.viewport.1;
        let gap = ((h - 220.0) / n as f32).clamp(3.0, 12.0);
        let y0 = h / 2.0 - gap * n as f32 / 2.0;
        marks
            .into_iter()
            .enumerate()
            .map(|(k, (b, agent, title, summary))| (y0 + k as f32 * gap + gap / 2.0, b, agent, title, summary))
            .collect()
    }

    fn rail_hit(&self, x: f32, y: f32) -> Option<Action> {
        if x < self.viewport.0 - 46.0 {
            return None;
        }
        let rail = self.rail();
        if rail.is_empty() {
            return None;
        }
        let gap = if rail.len() > 1 { rail[1].0 - rail[0].0 } else { 12.0 };
        rail.iter()
            .filter(|m| (m.0 - y).abs() <= (gap / 2.0).max(5.0))
            .min_by(|a, b| (a.0 - y).abs().total_cmp(&(b.0 - y).abs()))
            .map(|m| Action::Jump(m.1))
    }

    /// The block in view (for the rail's "current" mark).
    fn current_block(&self) -> Option<usize> {
        let m = self.measured.borrow();
        let mid = self.scroll + self.vh() * 0.4;
        m.blocks
            .iter()
            .take(self.nb.blocks.len())
            .enumerate()
            .filter(|(_, (top, _))| *top <= mid)
            .map(|(i, _)| i)
            .last()
    }

    // ------------------------------------------------------------- ink

    fn begin(&mut self, x: f32, y: f32, pressure: f32) {
        match self.effective_tool() {
            Tool::Eraser => {
                self.erase_before = Some(self.nb.ink.clone());
                self.erase_changed = false;
                self.last_erase = None;
                self.rub(x, y);
            }
            tool => {
                self.history.checkpoint(&self.nb.ink);
                let stroke = Stroke::new(tool, COLORS[self.color], Sample { x, y, pressure });
                let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64());
                self.nb.ink_times.insert(stroke.origin, now);
                self.nb.ink.push(stroke);
                self.drawing = true;
            }
        }
    }

    fn extend(&mut self, x: f32, y: f32, pressure: f32) {
        if self.drawing {
            if let Some(stroke) = self.nb.ink.last_mut() {
                stroke.push(Sample { x, y, pressure });
            }
        } else if self.erase_before.is_some() {
            self.rub(x, y);
        }
    }

    fn rub(&mut self, x: f32, y: f32) {
        let (lx, ly) = self.last_erase.unwrap_or((x, y));
        let steps = ((x - lx).hypot(y - ly) / 5.0).ceil().max(1.0) as usize;
        for i in 1..=steps {
            let t = i as f32 / steps as f32;
            let (px_, py_) = (lx + (x - lx) * t, ly + (y - ly) * t);
            if ink::erase(&mut self.nb.ink, px_, py_, ERASER_R) {
                self.erase_changed = true;
            }
            let before = self.nb.notes.len();
            self.nb
                .notes
                .retain(|n| (n.x - px_).abs() > 30.0 || (n.y + 10.0 - py_).abs() > 16.0);
            self.erase_changed |= before != self.nb.notes.len();
        }
        self.last_erase = Some((x, y));
    }

    fn end(&mut self) {
        let changed = self.drawing || self.erase_changed;
        if let Some(before) = self.erase_before.take() {
            if self.erase_changed {
                self.history.checkpoint(&before);
            }
        }
        self.drawing = false;
        self.erase_changed = false;
        self.last_erase = None;
        if changed {
            self.last_ink_change = Some(Instant::now());
            if self.stroke_by_pen {
                self.pen_new_ink = true;
                self.pen_lift_at = Some(Instant::now());
            }
            self.touch();
        }
    }

    // ------------------------------------------------------------- actions

    fn press(&mut self, action: Action, at: (f32, f32), cx: &mut Context<Self>) {
        if let Some((last, when)) = self.last_action {
            if last == action && when.elapsed() < Duration::from_millis(220) {
                return;
            }
        }
        self.last_action = Some((action, Instant::now()));
        match action {
            Action::Pi => self.pi_press = Some(Instant::now()),
            Action::ToggleTray => {
                let fab = self.layout().fab;
                self.fab_press = Some(FabPress {
                    start: at,
                    fab: (fab.x + fab.w / 2.0, fab.y + fab.h / 2.0),
                    moved: false,
                });
            }
            other => self.perform(other, cx),
        }
    }

    fn drag(&mut self, at: (f32, f32)) -> bool {
        let Some(p) = self.fab_press.as_mut() else { return false };
        let (dx, dy) = (at.0 - p.start.0, at.1 - p.start.1);
        if dx.hypot(dy) > 6.0 {
            p.moved = true;
        }
        if p.moved {
            let (w, h) = self.viewport;
            self.fab = ui::clamp_fab((p.fab.0 + dx, p.fab.1 + dy), w, h);
        }
        true
    }

    fn release(&mut self) {
        if let Some(p) = self.fab_press.take() {
            if p.moved {
                save_setting("fab", json!([self.fab.0, self.fab.1]));
            } else {
                self.tray_open = !self.tray_open;
            }
        }
        if let Some(at) = self.pi_press.take() {
            self.pi_tap(at.elapsed());
        }
    }

    fn pi_tap(&mut self, held: Duration) {
        if held >= Duration::from_millis(550) {
            self.listen = !self.listen;
            save_setting("listen", json!(self.listen));
            self.toast(if self.listen {
                "Pauses in your handwriting will be read"
            } else {
                "Only replies when you send"
            });
        } else {
            self.send_message();
        }
    }

    /// The human's answer to the confirm card.
    fn decide(&mut self, allowed: bool) {
        if let Some((call_id, _)) = self.confirm.take() {
            let _ = self.agent.send(ToAgent::CanvasResult { call_id, result: json!({"ok": true, "allowed": allowed}) });
            self.toast(if allowed { "Allowed" } else { "Denied" });
        }
    }

    fn perform(&mut self, action: Action, cx: &mut Context<Self>) {
        self.end();
        match action {
            Action::Tool(tool) => self.tool = tool,
            Action::Color(i) => {
                self.color = i;
                if self.tool == Tool::Eraser {
                    self.tool = Tool::Pen;
                }
            }
            Action::Undo => {
                self.history.undo(&mut self.nb.ink);
                self.touch();
            }
            Action::Redo => {
                self.history.redo(&mut self.nb.ink);
                self.touch();
            }
            Action::Compose => {
                self.editor.mode = Mode::Insert;
                self.follow = true;
                self.scroll_target = Some(f32::MAX);
                self.tray_open = false;
            }
            Action::Pi => self.pi_tap(Duration::ZERO),
            Action::ToggleTray => self.tray_open = !self.tray_open,
            Action::Send => self.send_figure(cx),
            Action::Skip => self.finish_figure(json!({"status": "skipped"}), "Skipped", cx),
            Action::Allow => self.decide(true),
            Action::Deny => self.decide(false),
            Action::Jump(block) => {
                if let Some(&(top, _)) = self.measured.borrow().blocks.get(block) {
                    self.scroll_target = Some((top - 90.0).max(0.0));
                    self.follow = false;
                }
            }
        }
    }

    // ------------------------------------------------------------- talking

    fn screen_capture(&self) -> Option<Vec<u8>> {
        if !self.window_active {
            return None;
        }
        let geometry = self.geometry.lock().ok().and_then(|g| *g);
        let mut grim = std::process::Command::new("grim");
        if let Some(g) = geometry {
            let (x, y, w, h) = g.win;
            grim.args(["-g", &format!("{},{} {}x{}", x.round(), y.round(), w.round(), h.round())]);
        }
        let out = grim.arg("-").output().ok()?;
        if !out.status.success() {
            return None;
        }
        let img = image::load_from_memory(&out.stdout).ok()?;
        let (w, h) = (self.viewport.0.round() as u32, self.viewport.1.round() as u32);
        let img = img.resize_exact(w, h, image::imageops::FilterType::Triangle);
        let mut png = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).ok()?;
        Some(png)
    }

    /// Ink the agent hasn't heard about yet: (strokes added, erased ghosts).
    fn ink_changes(&self) -> diff::Diff {
        let seen: HashSet<u64> = self.nb.seen_ink.iter().copied().collect();
        // Reconstruct the seen strokes: current ones it knows + nothing for erased
        // (we keep the last sent copy in memory for exact erase ghosts).
        let seen_strokes: Vec<Stroke> = self
            .sent_ink
            .borrow()
            .clone()
            .unwrap_or_else(|| self.nb.ink.iter().filter(|s| seen.contains(&s.id)).cloned().collect());
        diff::diff(&seen_strokes, &self.nb.ink)
    }

    fn send_message(&mut self) {
        let text = self.editor.text().trim_end().to_owned();
        if self.send(text, None) {
            self.editor.clear();
            self.nb.draft.clear();
        }
    }

    /// Send a message (typed text, a comment on a passage, or just ink).
    fn send(&mut self, text: String, quote: Option<String>) -> bool {
        let changes = self.ink_changes();
        let human_changes = changes.added.len() + changes.erased.len();
        if text.trim().is_empty() && human_changes == 0 {
            self.toast("Write or draw something first");
            return false;
        }
        if self.agent_down.is_some() {
            self.toast("The agent is offline (see /tmp/sketchpad-agent.log)");
            return false;
        }
        let h = self.vh();
        let z = self.zoom;
        let half_w = self.viewport.0 / 2.0;
        let mut images = Vec::new();
        let mut context = format!(
            "\n\n<notebook>\nDocument coordinates: x in px from the notebook's centre line (the text column spans x={:.0}..{:.0}), y in px from the top of the notebook. The human is looking at y={:.0}..{:.0}. Your reply appears below this message at the bottom of the notebook.",
            -COL_W / 2.0,
            COL_W / 2.0,
            self.scroll,
            self.scroll + h
        );
        let visible_ink = self
            .nb
            .ink
            .iter()
            .any(|s| s.samples.iter().any(|p| p.y > self.scroll && p.y < self.scroll + h));
        if human_changes > 0 || visible_ink {
            if let Some(png) = self.screen_capture() {
                images.push(png);
                context.push_str(&format!(
                    "\nImage {}: the window as the human sees it (document x = (image x - {:.0}) / {:.2}, document y = image y / {:.2} + {:.0}).",
                    images.len(),
                    half_w,
                    z,
                    z,
                    self.scroll
                ));
            }
        }
        if human_changes > 0 {
            let added_box = diff::bbox(changes.added.iter().map(|&i| &self.nb.ink[i]));
            let erased_box = diff::bbox(&changes.erased);
            if let Some(changed) = diff::union(added_box, erased_box) {
                let region = Rect::new(changed.x - 50.0, (changed.y - 50.0).max(0.0), changed.w + 100.0, changed.h + 100.0);
                let added: HashSet<usize> = changes.added.iter().copied().collect();
                let mut layers: Vec<export::Layer> = changes
                    .erased
                    .iter()
                    .map(|g| export::Layer { stroke: g, color: 0xd33f3f, alpha: 0.35 })
                    .collect();
                for (i, s) in self.nb.ink.iter().enumerate() {
                    let alpha = if added.contains(&i) { s.alpha() } else { s.alpha() * 0.28 };
                    layers.push(export::Layer { stroke: s, color: s.color, alpha });
                }
                if let Ok(png) = export::render_layers(&layers, None, region, 1.0) {
                    images.push(png);
                    context.push_str(&format!(
                        "\nImage {}: just the ink that changed since your last look, document area [{:.0},{:.0}]-[{:.0},{:.0}] (document px = image px + ({:.0}, {:.0})): older ink faded, new ink dark, erased ink faint red. Text is not drawn in it; use the screen image for what the ink sits on.",
                        images.len(),
                        region.x,
                        region.y,
                        region.x + region.w,
                        region.y + region.h,
                        region.x,
                        region.y
                    ));
                }
            }
            let mut what = Vec::new();
            if !changes.added.is_empty() {
                what.push(format!("added {} stroke(s)", changes.added.len()));
            }
            if !changes.erased.is_empty() {
                what.push("erased some ink".to_owned());
            }
            context.push_str(&format!("\nThe human {} on the page since your last look.", what.join(" and ")));
        }
        context.push_str("\n</notebook>");
        let mut body = if text.trim().is_empty() {
            "(No typed message: look at what I drew or marked on the page.)".to_owned()
        } else {
            text.clone()
        };
        if let Some(q) = &quote {
            body = format!("<comment on=\"an earlier message in this thread\">\n<quote>\n{q}\n</quote>\n{body}\n</comment>");
        }

        // Captioned by a small model in a moment ("wrote \"draw something\"").
        let attachment = (human_changes > 0).then(|| String::new());
        let room = self.ink_room();
        if room > 0.0 {
            self.nb.blocks.push(Block::Space { height: room });
        }
        let request_id = format!(
            "m{}",
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
        );
        self.nb.blocks.push(Block::User {
            text: text.clone(),
            attachment,
            quote,
            request_id: Some(request_id.clone()),
        });
        self.nb.blocks.push(Block::Agent {
            parts: Vec::new(),
            done: false,
            error: None,
        });
        self.msg_open = false;
        self.nb.seen_ink = self.nb.ink.iter().map(|s| s.id).collect();
        *self.sent_ink.borrow_mut() = Some(self.nb.ink.clone());
        self.pen_new_ink = false;
        self.follow = true;
        self.scroll_target = Some(f32::MAX);
        self.last_ink_change = None;
        self.touch();
        let describe = human_changes > 0 && !images.is_empty();
        let _ = self.agent.send(ToAgent::Send {
            request_id,
            text: body + &context,
            images,
            describe,
            typed: text,
        });
        true
    }

    fn ask_jev(&mut self) {
        let image = self.screen_capture().or_else(|| {
            let view = self.view_rect();
            let layers: Vec<export::Layer> = self
                .nb
                .ink
                .iter()
                .map(|s| export::Layer { stroke: s, color: s.color, alpha: s.alpha() })
                .collect();
            export::render_layers(&layers, None, view, self.zoom).ok()
        });
        let Some(image) = image.and_then(|png| fit_for_jev(&png)) else { return };
        eprintln!("jev: asking ({} bytes)", image.len());
        let recent = self
            .nb
            .blocks
            .iter()
            .rev()
            .find_map(|b| match b {
                Block::Agent { parts, .. } => Some(
                    parts
                        .iter()
                        .filter_map(|p| if let Part::Text(t) = p { Some(t.as_str()) } else { None })
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
                _ => None,
            })
            .unwrap_or_default();
        let id = format!("j{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0));
        self.pen_new_ink = false;
        self.last_jev = Some(Instant::now());
        self.jev_pending = Some(id.clone());
        let _ = self.agent.send(ToAgent::ShouldRespond { request_id: id, image, text: recent });
    }

    /// Before a reply continues with a new part, leave room for any
    /// handwriting the human put below it meanwhile, so text never runs over ink.
    fn room_in_reply(&mut self) {
        match self.nb.open_agent() {
            None => return,
            Some(parts) if matches!(parts.last(), Some(Part::Space { .. })) => return,
            _ => {}
        }
        let room = self.ink_room() + BLOCK_GAP - 10.0;
        if room > 4.0 {
            if let Some(parts) = self.nb.open_agent() {
                parts.push(Part::Space { height: room });
            }
        }
    }

    fn agent_parts(&mut self) -> &mut Vec<Part> {
        if self.nb.open_agent().is_none() {
            self.nb.blocks.push(Block::Agent {
                parts: Vec::new(),
                done: false,
                error: None,
            });
            self.msg_open = false;
        }
        self.nb.open_agent().unwrap()
    }

    fn on_agent(&mut self, event: FromAgent) {
        match event {
            FromAgent::Ready(model) => {
                self.model = Some(model);
                self.agent_down = None;
            }
            FromAgent::Busy(busy) => {
                self.agent_busy = busy;
                if !busy {
                    if let Some(Block::Agent { done, parts, .. }) = self.nb.blocks.last_mut() {
                        parts.retain(|p| !matches!(p, Part::Text(t) if t.trim().is_empty()));
                        *done = true;
                    }
                    self.msg_open = false;
                    self.touch();
                }
            }
            FromAgent::Jev { model, reason } => {
                self.jev = model;
                self.jev_note = (!reason.is_empty()).then_some(reason);
            }
            FromAgent::Verdict { request_id, probability, error } => {
                if self.jev_pending.as_deref() == Some(request_id.as_str()) {
                    self.jev_pending = None;
                    eprintln!("jev: verdict {probability:?} {error:?}");
                    match (probability, error) {
                        (Some(p), _) if p >= 0.6 && !self.pen_down && !self.agent_busy => {
                            self.send(String::new(), None);
                        }
                        (_, Some(e)) => self.toast(format!("Pause detection: {e}")),
                        _ => {}
                    }
                }
            }
            FromAgent::Described { request_id, text } => {
                for b in self.nb.blocks.iter_mut().rev() {
                    if let Block::User { request_id: Some(r), attachment, .. } = b {
                        if *r == request_id {
                            *attachment = Some(text);
                            break;
                        }
                    }
                }
                self.touch();
            }
            FromAgent::Partial(text) | FromAgent::Final(text) if text.is_empty() => {}
            FromAgent::Partial(text) => self.set_text(text, false),
            FromAgent::Final(text) => self.set_text(text, true),
            FromAgent::Tool { name, summary } => {
                self.room_in_reply();
                self.agent_parts().push(Part::Tool { name, summary });
                self.msg_open = false;
            }
            // The agent waits for the human's answer: the card replies when they decide.
            FromAgent::Canvas { call_id, op, payload } if op == "confirm" => {
                let what = payload.get("text").and_then(Value::as_str).unwrap_or("do something in your browser").to_owned();
                if let Some((old, _)) = self.confirm.replace((call_id, what)) {
                    let _ = self.agent.send(ToAgent::CanvasResult { call_id: old, result: json!({"ok": true, "allowed": false}) });
                }
            }
            FromAgent::Canvas { call_id, op, payload } => {
                let result = self.canvas_op(&op, &payload);
                let _ = self.agent.send(ToAgent::CanvasResult { call_id, result });
                self.touch();
            }
            FromAgent::Error(e) => {
                if let Some(Block::Agent { error, .. }) = self.nb.blocks.last_mut() {
                    *error = Some(e.clone());
                }
                self.toast(e);
            }
            FromAgent::Exited(e) => {
                self.agent_busy = false;
                self.agent_down = Some(e.clone());
                self.toast(e);
            }
        }
    }

    fn set_text(&mut self, text: String, finished: bool) {
        if !self.msg_open {
            self.room_in_reply();
        }
        let open = self.msg_open;
        let parts = self.agent_parts();
        match parts.last_mut() {
            Some(Part::Text(t)) if open => *t = text,
            _ => parts.push(Part::Text(text)),
        }
        self.msg_open = !finished;
        if finished {
            self.touch();
        }
    }

    fn canvas_op(&mut self, op: &str, payload: &Value) -> Value {
        match op {
            "checkpoint" => {
                let block = self.nb.blocks.len().saturating_sub(1);
                self.nb.checkpoints.push(Checkpoint {
                    block,
                    title: payload.get("title").and_then(Value::as_str).unwrap_or("Checkpoint").to_owned(),
                    summary: payload.get("summary").and_then(Value::as_str).unwrap_or("").to_owned(),
                    agent: true,
                });
                json!({"ok": true})
            }
            "draw" => {
                let ops: Ops = match payload.get("ops").cloned().map(serde_json::from_value) {
                    Some(Ok(ops)) => ops,
                    Some(Err(e)) => return json!({"ok": false, "error": format!("bad shapes: {e}")}),
                    None => Ops::default(),
                };
                let page = payload.get("anchor").and_then(Value::as_str) == Some("page");
                if page {
                    let (drawn, erased) = self.apply_page_ops(&ops, 1.0, (0.0, 0.0));
                    json!({"ok": true, "summary": format!("Drew {drawn} item(s) on the page, erased {erased} region(s).")})
                } else {
                    let built = ops::build(&ops, 1.0, (0.0, 0.0));
                    let bottom = built
                        .strokes
                        .iter()
                        .flat_map(|s| s.samples.iter().map(|p| p.y))
                        .chain(built.notes.iter().map(|n| n.y + n.size * 1.4))
                        .fold(40.0f32, f32::max);
                    let height = (bottom + 24.0).clamp(80.0, 1400.0);
                    let n = built.strokes.len() + built.notes.len();
                    self.room_in_reply();
                    let parts = self.agent_parts();
                    parts.push(Part::Sketch {
                        strokes: Vec::new(),
                        labels: built.notes,
                        height,
                    });
                    let pi = parts.len() - 1;
                    let bi = self.nb.blocks.len() - 1;
                    self.msg_open = false;
                    for stroke in built.strokes {
                        self.anims.push_back(Anim { target: Target::Sketch(bi, pi), stroke, shown: 1 });
                    }
                    json!({"ok": true, "summary": format!("Sketched {n} item(s) in your reply ({SKETCH_W:.0}x{height:.0}).")})
                }
            }
            other => json!({"ok": false, "error": format!("unknown op {other}")}),
        }
    }

    /// Ops on the page itself (document coordinates after origin/scale).
    fn apply_page_ops(&mut self, ops: &Ops, scale: f32, origin: (f32, f32)) -> (usize, usize) {
        let built = ops::build(ops, scale, origin);
        self.flush_anims();
        self.history.checkpoint(&self.nb.ink);
        for r in &built.erase {
            ink::erase_rect(&mut self.nb.ink, r.x, r.y, r.w, r.h);
            self.nb.notes.retain(|n| !r.contains(n.x, n.y));
        }
        let drawn = built.strokes.len() + built.notes.len();
        self.nb.notes.extend(built.notes);
        for stroke in built.strokes {
            self.anims.push_back(Anim { target: Target::Page, stroke, shown: 1 });
        }
        // Agent ink is not news to the agent.
        (drawn, built.erase.len())
    }

    fn flush_anims(&mut self) {
        while let Some(a) = self.anims.pop_front() {
            self.land(a);
        }
    }

    fn land(&mut self, a: Anim) {
        match a.target {
            Target::Page => {
                self.nb.seen_ink.push(a.stroke.id);
                if let Some(s) = self.sent_ink.borrow_mut().as_mut() {
                    s.push(a.stroke.clone());
                }
                self.nb.ink.push(a.stroke);
            }
            Target::Sketch(b, p) => {
                if let Some(Block::Agent { parts, .. }) = self.nb.blocks.get_mut(b) {
                    if let Some(Part::Sketch { strokes, .. }) = parts.get_mut(p) {
                        strokes.push(a.stroke);
                    }
                }
            }
        }
    }

    fn animate(&mut self) -> bool {
        if self.pen_down || self.anims.is_empty() {
            return false;
        }
        let mut budget = 10;
        while budget > 0 {
            let Some(a) = self.anims.front_mut() else { break };
            let step = budget.min(a.stroke.samples.len() - a.shown);
            a.shown += step;
            budget -= step;
            if a.shown >= a.stroke.samples.len() {
                let a = self.anims.pop_front().unwrap();
                self.land(a);
            }
            if step == 0 && budget > 0 {
                budget -= 1;
            }
        }
        true
    }

    // ------------------------------------------------------------- figures (sketchpad ask)

    fn figure(&mut self, path: &PathBuf) -> Option<&Figure> {
        if !self.figures.contains_key(path) {
            let f = Figure::load(path).ok()?;
            self.figures.insert(path.clone(), f);
        }
        self.figures.get(path)
    }

    fn send_figure(&mut self, cx: &mut Context<Self>) {
        let Some(p) = self.pending.as_ref() else { return };
        let block = p.block;
        let Some(&(top, height)) = self.measured.borrow().blocks.get(block) else { return };
        let region = Rect::new(self.col_x() - 40.0, top - 10.0, COL_W + 80.0, height + 20.0);
        let k = self.scale.max(1.0);
        let image_rect = self.measured.borrow().figures.get(&block).copied();
        let image = match self.nb.blocks.get(block) {
            Some(Block::Figure { image: Some(path), .. }) => Some(path.clone()),
            _ => None,
        };
        let figure = image.and_then(|path| self.figure(&path).map(|_| path));
        let figure = figure.as_ref().and_then(|p| self.figures.get(p)).zip(image_rect);
        let layers: Vec<export::Layer> = self
            .nb
            .ink
            .iter()
            .map(|s| export::Layer { stroke: s, color: s.color, alpha: s.alpha() })
            .collect();
        match export::render_layers(&layers, figure, region, k).and_then(|png| export::save(&png, "drawing")) {
            Ok(path) => {
                self.last_export = Some(((region.x, region.y), k));
                let reply = json!({
                    "status": "sent",
                    "image": path,
                    "width": (region.w * k).round(),
                    "height": (region.h * k).round(),
                    "strokes": self.nb.ink.len(),
                });
                let from = self.pending.as_ref().map(|p| p.from.clone()).unwrap_or_default();
                self.finish_figure(reply, &format!("Sent to {from}"), cx);
            }
            Err(e) => self.toast(format!("Could not save drawing: {e}")),
        }
    }

    fn finish_figure(&mut self, reply: Value, message: &str, cx: &mut Context<Self>) {
        let Some(p) = self.pending.take() else { return };
        if let Some(Block::Figure { open, .. }) = self.nb.blocks.get_mut(p.block) {
            *open = false;
        }
        let _ = p.reply.send(reply);
        self.toast(message);
        self.touch();
        if self.launched_by_cli {
            cx.spawn(async |_, cx| {
                Timer::after(Duration::from_millis(700)).await;
                let _ = cx.update(|cx| cx.quit());
            })
            .detach();
        }
    }

    fn on_request(&mut self, request: Request, window: &mut Window) {
        match request.command {
            Command::Focus => {
                bring_to_front(window);
                let _ = request.reply.send(json!({"status": "ok"}));
            }
            Command::Snapshot => {
                let (w, h) = self.viewport;
                let k = self.zoom;
                let view = self.view_rect();
                let png = self.screen_capture().map(|p| (p, k)).or_else(|| {
                    let layers: Vec<export::Layer> = self
                        .nb
                        .ink
                        .iter()
                        .map(|s| export::Layer { stroke: s, color: s.color, alpha: s.alpha() })
                        .collect();
                    export::render_layers(&layers, None, view, k).ok().map(|p| (p, k))
                });
                let reply = match png.map(|(p, k)| (export::save(&p, "snapshot"), k)) {
                    Some((Ok(path), k)) => {
                        self.last_export = Some(((view.x, view.y), k));
                        json!({"status": "ok", "image": path, "width": w.round(), "height": h.round(), "strokes": self.nb.ink.len()})
                    }
                    Some((Err(e), _)) => json!({"status": "error", "error": e}),
                    None => json!({"status": "error", "error": "could not capture the page"}),
                };
                let _ = request.reply.send(reply);
            }
            Command::Draw { ops } => {
                let reply = match serde_json::from_value::<Ops>(ops) {
                    Ok(ops) => {
                        let view = self.view_rect();
                        let (origin, k) = self.last_export.unwrap_or(((view.x, view.y), self.zoom));
                        let (drawn, erased) = self.apply_page_ops(&ops, k, origin);
                        json!({"status": "ok", "drawn": drawn, "erased": erased})
                    }
                    Err(e) => json!({"status": "error", "error": format!("bad ops: {e}")}),
                };
                let _ = request.reply.send(reply);
            }
            Command::Ask {
                prompt,
                image,
                from,
                keep: _,
            } => {
                let (width, height) = match image.as_ref().and_then(|p| self.figure(p)) {
                    Some(f) => (f.width, f.height),
                    None if image.is_some() => {
                        let _ = request.reply.send(json!({"status": "error", "error": "cannot read the figure"}));
                        return;
                    }
                    None => (0, 0),
                };
                if let Some(old) = self.pending.take() {
                    if let Some(Block::Figure { open, .. }) = self.nb.blocks.get_mut(old.block) {
                        *open = false;
                    }
                    let _ = old.reply.send(json!({"status": "superseded"}));
                }
                let from = from.unwrap_or_else(|| "Your agent".into());
                self.nb.blocks.push(Block::Figure {
                    prompt,
                    from: from.clone(),
                    image,
                    width,
                    height,
                    open: true,
                });
                let block = self.nb.blocks.len() - 1;
                self.pending = Some(Pending { block, reply: request.reply, from });
                self.follow = true;
                self.scroll_target = Some(f32::MAX);
                self.touch();
                bring_to_front(window);
            }
        }
    }

    // ------------------------------------------------------------- input

    fn on_input(&mut self, event: InputEvent, cx: &mut Context<Self>) -> bool {
        let (w, h) = self.viewport;
        // The `:config` menu is up: the pen doesn't ink underneath it.
        if self.config.is_some() && matches!(event, InputEvent::Pen { .. }) {
            return false;
        }
        match event {
            InputEvent::PenConnected => false,
            InputEvent::Error(e) => {
                self.toast(e);
                true
            }
            InputEvent::Pen {
                x,
                y,
                pressure,
                touching,
                in_range,
                rubber,
            } => {
                if !self.window_active {
                    self.pen_hover = None;
                    self.pen_in_range = false;
                    if self.pen_down {
                        self.pen_down = false;
                        self.end();
                    }
                    return true;
                }
                let (x, y) = self.map_input(x, y);
                let inside = x >= 0.0 && y >= 0.0 && x <= w && y <= h;
                if !inside && !self.pen_down {
                    // Hovering over another window.
                    self.pen_hover = None;
                    self.pen_in_range = false;
                    return true;
                }
                self.pen_in_range = in_range;
                self.rubber = rubber;
                self.pen_hover = in_range.then_some((x, y));
                let was_down = self.pen_down;
                self.pen_down = touching;
                let (doc_x, doc_y) = self.to_doc(x, y);
                match (was_down, touching) {
                    (false, true) => {
                        if let Some(action) = self.hit(x, y) {
                            self.pen_on_chrome = true;
                            self.press(action, (x, y), cx);
                        } else if self.is_chrome(x, y) {
                            self.pen_on_chrome = true;
                        } else {
                            self.pen_on_chrome = false;
                            self.stroke_by_pen = true;
                            self.begin(doc_x, doc_y, pressure);
                        }
                    }
                    (true, true) if self.pen_on_chrome => {
                        self.drag((x, y));
                    }
                    (true, true) => self.extend(doc_x, doc_y, pressure),
                    (true, false) => {
                        if self.pen_on_chrome {
                            self.pen_on_chrome = false;
                            self.release();
                        } else {
                            self.end();
                        }
                    }
                    _ => {}
                }
                true
            }
            InputEvent::Touch(contacts) => self.on_touch(contacts, cx),
        }
    }

    /// Touch: one finger draws on the paper (or taps a button, or drags the
    /// floating button); two fingers scroll. Ignored while the pen hovers,
    /// so a resting palm never draws.
    fn on_touch(&mut self, contacts: Vec<(f32, f32)>, cx: &mut Context<Self>) -> bool {
        if self.config.is_some() {
            self.gesture = None;
            return false;
        }
        if !self.window_active || self.pen_in_range || self.pen_down {
            if let Some(g) = self.gesture.take() {
                if g.inking {
                    self.end();
                }
            }
            return false;
        }
        let (w, h) = self.viewport;
        let pts: Vec<(f32, f32)> = contacts.iter().map(|&(x, y)| self.map_input(x, y)).collect();
        let n = pts.len();
        let centroid = (n > 0).then(|| {
            (
                pts.iter().map(|p| p.0).sum::<f32>() / n as f32,
                pts.iter().map(|p| p.1).sum::<f32>() / n as f32,
            )
        });

        let Some(g) = self.gesture.as_mut() else {
            if let Some(first) = centroid.filter(|c| c.0 >= 0.0 && c.1 >= 0.0 && c.0 <= w && c.1 <= h) {
                let fab = self.layout().fab;
                let on_fab = fab.contains(first.0, first.1);
                let on_chrome = on_fab || self.is_chrome(first.0, first.1);
                let inking = n == 1 && !on_chrome;
                self.gesture = Some(Gesture {
                    start: Instant::now(),
                    start_pos: first,
                    last: first,
                    max_contacts: n,
                    moved: 0.0,
                    fab: on_fab.then_some((fab.x + fab.w / 2.0, fab.y + fab.h / 2.0)),
                    inking,
                    on_chrome,
                    last_at: Instant::now(),
                    velocity: 0.0,
                });
                if inking {
                    self.stroke_by_pen = false;
                    let (dx, dy) = self.to_doc(first.0, first.1);
                    self.begin(dx, dy, 0.5);
                    return true;
                }
            }
            return false;
        };

        let Some(c) = centroid else {
            let g = self.gesture.take().unwrap();
            if g.inking {
                self.end();
                return true;
            }
            if g.max_contacts >= 2 && g.last_at.elapsed() < Duration::from_millis(80) && g.velocity.abs() > 250.0 {
                self.momentum = g.velocity.clamp(-4000.0, 4000.0);
                return true;
            }
            if g.fab.is_some() {
                if g.moved > 8.0 {
                    save_setting("fab", json!([self.fab.0, self.fab.1]));
                } else {
                    self.tray_open = !self.tray_open;
                }
                return true;
            }
            if g.max_contacts == 1 && g.moved < 14.0 {
                if let Some(action) = self.hit(g.start_pos.0, g.start_pos.1) {
                    if action == Action::Pi {
                        self.pi_tap(g.start.elapsed());
                    } else if g.start.elapsed() < Duration::from_millis(700) {
                        self.press(action, g.start_pos, cx);
                        self.release();
                    }
                    return true;
                }
            }
            return false;
        };

        // A second finger turns a drawing into a scroll: take the stroke back.
        if n >= 2 && g.inking {
            g.inking = false;
            self.drawing = false;
            if self.erase_before.take().is_some() {
                self.erase_changed = false;
            } else {
                self.history.undo(&mut self.nb.ink);
            }
            let g = self.gesture.as_mut().unwrap();
            g.last = c;
            g.max_contacts = n;
            return true;
        }
        let g = self.gesture.as_mut().unwrap();
        g.max_contacts = g.max_contacts.max(n);
        g.moved = g.moved.max((c.0 - g.start_pos.0).hypot(c.1 - g.start_pos.1));
        let dy = c.1 - g.last.1;
        g.last = c;
        if g.inking {
            let (x, y) = pts[0];
            let (dx, dy) = self.to_doc(x, y);
            self.extend(dx, dy, 0.5);
            return true;
        }
        if let Some(fab) = g.fab {
            if g.moved > 8.0 {
                let (sx, sy) = g.start_pos;
                self.fab = ui::clamp_fab((fab.0 + c.0 - sx, fab.1 + c.1 - sy), w, h);
            }
            return true;
        }
        if n >= 2 && dy.abs() > 0.0 {
            let elapsed = g.last_at.elapsed().as_secs_f32().max(0.004);
            let v = -dy / self.zoom / elapsed;
            g.velocity = g.velocity * 0.6 + v * 0.4;
            g.last_at = Instant::now();
            self.scroll_by(-dy / self.zoom);
            return true;
        }
        false
    }

    fn on_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let ks = &event.keystroke;
        let m = &ks.modifiers;
        let key = vim::Key {
            key: ks.key.clone(),
            ch: ks.key_char.clone(),
            ctrl: m.control,
            alt: m.alt,
            shift: m.shift,
        };
        let ch = ks.key_char.clone().filter(|c| !c.is_empty()).unwrap_or_else(|| ks.key.clone());
        if self.config.is_some() {
            self.config_key(&key, window);
            cx.notify();
            return;
        }
        if m.control && ks.key == "`" {
            self.toggle_terminal();
            cx.notify();
            return;
        }
        // The confirm card: y allows, n or Esc denies (not while typing in a terminal or a comment).
        if self.confirm.is_some() && self.term_focus.is_none() && self.comment.is_none() && self.editor.mode != Mode::Insert {
            match ks.key.as_str() {
                "y" => {
                    self.decide(true);
                    cx.notify();
                    return;
                }
                "n" | "escape" => {
                    self.decide(false);
                    cx.notify();
                    return;
                }
                _ => {}
            }
        }
        // Typing in a terminal: every key is the shell's, except Esc, which leaves it.
        if let Some(id) = self.term_focus {
            if ks.key == "escape" {
                self.leave_terminal();
            } else if let Some(t) = self.terms.get(&id) {
                if m.control && m.shift && ks.key == "v" {
                    if let Some(text) = cx.read_from_clipboard().and_then(|c| c.text()) {
                        t.write(terminal::paste_bytes(&text, t.bracketed_paste()));
                    }
                } else if let Some(bytes) = terminal::key_bytes(&ks.key, ks.key_char.as_deref(), m.control, m.alt, t.app_cursor()) {
                    t.write(bytes);
                }
            }
            cx.notify();
            return;
        }
        if m.control && !m.shift && ks.key == "z" {
            if let Some((at, block)) = self.removed.take() {
                let at = at.min(self.nb.blocks.len());
                self.insert_block(at, block);
                // The cursor goes back onto its last line.
                self.anchors = self.compute_anchors();
                self.anchor_index = self.anchors.iter().enumerate().map(|(i, (a, _))| (*a, i)).collect();
                let last = self.anchors.iter().rev().find(|(a, _)| a.block == at).map(|(a, _)| *a);
                self.editor.mode = Mode::Normal;
                self.set_thread(last);
                self.toast("Terminal back");
                self.touch();
                cx.notify();
                return;
            }
        }
        // Ink undo lives outside vim.
        if m.control && ks.key == "z" {
            self.perform(if m.shift { Action::Redo } else { Action::Undo }, cx);
            cx.notify();
            return;
        }
        // ctrl+= / ctrl+- / ctrl+0: text size. The whole page scales, so ink stays on its words.
        if m.control && matches!(ks.key.as_str(), "=" | "+" | "-" | "0") {
            self.set_text_scale(match ks.key.as_str() {
                "-" => self.text_scale / 1.1,
                "0" => 1.0,
                _ => self.text_scale * 1.1,
            });
            self.toast(format!("Text size {:.0}%", self.text_scale * 100.0));
            cx.notify();
            return;
        }
        // ctrl+n: a fresh notebook, same as `:new`.
        if m.control && ks.key == "n" {
            self.comment = None;
            self.thread = None;
            self.ex_command("new", window);
            cx.notify();
            return;
        }

        // A comment being written: its own little vim.
        if let Some(c) = self.comment.as_mut() {
            if ks.key == "escape" && c.editor.mode == Mode::Normal && c.editor.pending_keys().is_empty() {
                self.comment = None;
                self.sel_from = None;
                cx.notify();
                return;
            }
            self.reveal_comment = true;
            match c.editor.handle(&key) {
                vim::Outcome::Send => {
                    let c = self.comment.take().unwrap();
                    let text = c.editor.text().trim().to_owned();
                    if text.is_empty() {
                        self.comment = Some(c);
                    } else {
                        self.thread = None;
                        self.sel_from = None;
                        self.editor.mode = Mode::Insert;
                        self.send(text, Some(c.excerpt));
                    }
                }
                vim::Outcome::Quit => self.comment = None,
                _ => {}
            }
            cx.notify();
            return;
        }

        if m.control && matches!(ks.key.as_str(), "d" | "u" | "e" | "y") && self.editor.mode != Mode::Insert {
            let h = self.vh();
            let dy = match ks.key.as_str() {
                "d" => h * 0.5,
                "u" => -h * 0.5,
                "e" => 40.0,
                _ => -40.0,
            };
            self.glide_by(dy);
            cx.notify();
            return;
        }

        // The thread cursor: j/k walk the conversation, Enter comments there.
        if let Some(at) = self.thread {
            if self.editor.mode == Mode::Command {
                // `:` commands typed while in the thread go to the command line.
            } else {
                let pos = self.anchor_index.get(&at).copied();
                let text = self.anchor_text(at).to_owned();
                if matches!(ks.key.as_str(), "backspace" | "delete") {
                    self.delete_object(at);
                    cx.notify();
                    return;
                }
                match ch.as_str() {
                    "j" | "down" => self.vertical(true),
                    "k" | "up" => self.vertical(false),
                    "h" | "left" => self.caret = prev_char(&text, self.caret),
                    "l" | "right" => self.caret = next_char(&text, self.caret).min(last_char(&text)),
                    "0" | "^" | "home" => self.caret = 0,
                    "$" | "end" => self.caret = last_char(&text),
                    "w" => match word_fwd(&text, self.caret) {
                        Some(o) => self.caret = o,
                        None => {
                            self.step_anchor(true);
                        }
                    },
                    "b" => match word_back(&text, self.caret) {
                        Some(o) => self.caret = o,
                        None => {
                            if self.step_anchor(false) {
                                let t = self.anchor_text(self.thread.unwrap()).to_owned();
                                self.caret = word_back(&t, t.len()).unwrap_or(0);
                            }
                        }
                    },
                    "e" => self.caret = word_end(&text, self.caret),
                    "v" => self.toggle_selection(false),
                    "V" => self.toggle_selection(true),
                    "y" => {
                        let copied = self.selection_text().unwrap_or(text);
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(copied));
                        self.sel_from = None;
                        self.toast("Copied");
                    }
                    "escape" => self.sel_from = None,
                    "}" | "{" => {
                        let forward = ch == "}";
                        let here = pos.unwrap_or(0);
                        let target = if forward {
                            self.anchors.iter().skip(here + 1).find(|(a, _)| a.block != at.block).map(|(a, _)| *a)
                        } else {
                            let prev_block = self.anchors[..here].iter().rev().find(|(a, _)| a.block != at.block).map(|(a, _)| a.block);
                            prev_block.and_then(|b| self.anchors.iter().find(|(a, _)| a.block == b).map(|(a, _)| *a))
                        };
                        match target {
                            Some(t) => self.set_thread(Some(t)),
                            None if forward => self.set_thread(None),
                            None => {}
                        }
                    }
                    "g" => {
                        if self.g_pressed.is_some_and(|t| t.elapsed() < Duration::from_millis(600)) {
                            self.g_pressed = None;
                            if let Some((a, _)) = self.anchors.first() {
                                self.set_thread(Some(*a));
                            }
                        } else {
                            self.g_pressed = Some(Instant::now());
                        }
                    }
                    "G" => self.set_thread(None),
                    "enter" => self.open_comment(at),
                    "i" | "a" | "o" | "A" | "I" | "O" => match self.nb.blocks.get(at.block) {
                        // Back into a terminal that still runs.
                        Some(Block::Terminal { id, .. }) if self.terms.contains_key(id) => {
                            let id = *id;
                            self.enter_terminal(id);
                        }
                        _ => {
                            self.set_thread(None);
                            self.editor.mode = Mode::Insert;
                        }
                    },
                    ":" => {
                        self.editor.handle(&key);
                    }
                    _ => {}
                }
                cx.notify();
                return;
            }
        }

        // In the composer: k on its first line steps up into the conversation.
        if self.editor.mode == Mode::Normal && !m.control && self.editor.pending_keys().is_empty() {
            if matches!(ch.as_str(), "k" | "up") && self.editor.row == 0 {
                if let Some((a, _)) = self.anchors.last() {
                    self.set_thread(Some(*a));
                    cx.notify();
                    return;
                }
            }
            if matches!(ch.as_str(), "{") {
                if let Some(last) = self.anchors.last().map(|(a, _)| a.block) {
                    let first = self.anchors.iter().find(|(a, _)| a.block == last).map(|(a, _)| *a);
                    self.set_thread(first);
                    cx.notify();
                    return;
                }
            }
            if ch == "g" && self.editor.lines.len() == 1 {
                if self.g_pressed.is_some_and(|t| t.elapsed() < Duration::from_millis(600)) {
                    self.g_pressed = None;
                    if let Some((a, _)) = self.anchors.first() {
                        self.set_thread(Some(*a));
                    }
                    cx.notify();
                    return;
                }
                self.g_pressed = Some(Instant::now());
            }
        }
        match self.editor.handle(&key) {
            vim::Outcome::None => {}
            vim::Outcome::Send => self.send_message(),
            vim::Outcome::Quit => {
                self.save();
                cx.quit();
            }
            vim::Outcome::Command(cmd) => self.ex_command(&cmd, window),
        }
        self.nb.draft = self.editor.text();
        self.touch();
        if self.thread.is_none() {
            self.follow = true;
            self.reveal_cursor();
        }
        cx.notify();
    }

    /// Move the thread cursor (None: back to the composer) and keep it in view.
    fn set_thread(&mut self, at: Option<Anchor>) {
        self.thread = at;
        self.caret = 0;
        self.follow = at.is_none();
        match at {
            Some(a) => {
                let rect = {
                    let m = self.measured.borrow();
                    // Terminal lines far off screen aren't laid out: place them from the block's top.
                    m.anchors.get(&a).copied().or_else(|| match self.nb.blocks.get(a.block) {
                        Some(Block::Terminal { .. }) => m
                            .blocks
                            .get(a.block)
                            .map(|b| Rect::new(0.0, b.0 + TERM_HEAD + a.item as f32 * TERM_LH, COL_W, TERM_LH)),
                        _ => None,
                    })
                };
                if let Some(r) = rect {
                    let h = self.vh();
                    let here = self.scroll_target.filter(|t| *t != f32::MAX).unwrap_or(self.scroll);
                    if r.y < here + 90.0 {
                        self.scroll_target = Some((r.y - 120.0).max(0.0));
                    } else if r.y + r.h.min(h * 0.6) > here + h - 90.0 {
                        self.scroll_target = Some(r.y + r.h.min(h * 0.6) - h + 140.0);
                    }
                }
            }
            None => {
                self.sel_from = None;
                self.editor.mode = Mode::Normal;
                self.reveal_cursor();
            }
        }
    }

    /// Enter on a passage: a comment box opens right under it (as in syncpi).
    fn open_comment(&mut self, at: Anchor) {
        let excerpt = self.selection_text().unwrap_or_else(|| self.anchor_text(at).to_owned());
        let mut editor = Editor::new();
        editor.mode = Mode::Insert;
        self.comment = Some(CommentDraft { at, excerpt, editor });
        self.reveal_comment = true;
    }

    // ------------------------------------------------------------ selection

    /// Anchors whose text the cursor moves through character by character (a
    /// line of yours, a paragraph or heading of a reply, a terminal line);
    /// everything else (sketches, code, lists, drawings) is selected whole.
    fn is_text(&self, a: Anchor) -> bool {
        match self.nb.blocks.get(a.block) {
            Some(Block::User { .. }) | Some(Block::Terminal { .. }) => true,
            Some(Block::Agent { parts, .. }) => match parts.get(a.part) {
                Some(Part::Text(t)) => matches!(self.md(t).get(a.item), Some(Md::Heading(..) | Md::Paragraph(_))),
                _ => false,
            },
            _ => false,
        }
    }

    fn anchor_text(&self, a: Anchor) -> &str {
        self.anchor_index.get(&a).map_or("", |i| self.anchors[*i].1.as_str())
    }

    /// The selection in document order: (anchor index, from) .. (anchor index, to), `to` exclusive.
    fn selection(&self) -> Option<((usize, usize), (usize, usize))> {
        let (fa, fo) = self.sel_from?;
        let (i, j) = (*self.anchor_index.get(&fa)?, *self.anchor_index.get(&self.thread?)?);
        let (mut a, mut b) = ((i, fo), (j, self.caret));
        if b < a {
            std::mem::swap(&mut a, &mut b);
        }
        let len = |k: usize| self.anchors[k].1.len();
        if self.sel_lines || !self.is_text(self.anchors[a.0].0) {
            a.1 = 0;
        }
        b.1 = if self.sel_lines || !self.is_text(self.anchors[b.0].0) { len(b.0) } else { next_char(&self.anchors[b.0].1, b.1) };
        Some((a, b))
    }

    /// The part of an anchor's text that is selected.
    fn sel_range(&self, a: Anchor) -> Option<std::ops::Range<usize>> {
        let ((i0, o0), (i1, o1)) = self.selection()?;
        let i = *self.anchor_index.get(&a)?;
        (i0..=i1).contains(&i).then(|| (if i == i0 { o0 } else { 0 })..(if i == i1 { o1 } else { self.anchors[i].1.len() }))
    }

    fn selection_text(&self) -> Option<String> {
        let ((i0, o0), (i1, o1)) = self.selection()?;
        let parts: Vec<String> = (i0..=i1)
            .map(|i| {
                let t = &self.anchors[i].1;
                let (a, b) = (if i == i0 { o0 } else { 0 }, if i == i1 { o1 } else { t.len() });
                t.get(a.min(b)..b).unwrap_or(t).to_owned()
            })
            .collect();
        Some(parts.join("\n"))
    }

    /// The character under the cursor, when it's in this anchor's text.
    fn caret_range(&self, a: Anchor) -> Option<std::ops::Range<usize>> {
        if self.thread != Some(a) || self.term_focus.is_some() || !self.is_text(a) {
            return None;
        }
        let t = self.anchor_text(a);
        let o = self.caret.min(last_char(t));
        (o < t.len()).then(|| o..next_char(t, o))
    }

    /// A text the cursor moves through: selection and cursor painted in, layout kept for the mouse and j/k.
    fn text_el(&self, a: Anchor, text: String, runs: Vec<TextRun>, cursor: Option<std::ops::Range<usize>>) -> StyledText {
        let runs = mark_runs(runs, self.sel_range(a), self.caret_range(a).or(cursor));
        let st = StyledText::new(text).with_runs(runs);
        self.measured.borrow_mut().layouts.insert(a, st.layout().clone());
        st
    }

    /// The text offset under a screen point, in an anchor laid out last frame.
    fn index_at(&self, a: Anchor, at: gpui::Point<Pixels>) -> usize {
        let i = match self.measured.borrow().layouts.get(&a).map(|l| l.index_for_position(at)) {
            Some(Ok(i) | Err(i)) => i,
            None => 0,
        };
        i.min(last_char(self.anchor_text(a)))
    }

    /// j / k: the next visual line, inside a wrapped paragraph first, then the next passage.
    fn vertical(&mut self, down: bool) {
        let Some(at) = self.thread else { return };
        let layout = self.measured.borrow().layouts.get(&at).cloned();
        let here = layout.as_ref().and_then(|l| l.position_for_index(self.caret).map(|p| (l.clone(), p)));
        if let Some((l, p)) = &here {
            let lh = l.line_height();
            let b = l.bounds();
            let target = point(p.x, if down { p.y + lh * 1.5 } else { p.y - lh * 0.5 });
            if target.y > b.top() && target.y < b.bottom() {
                let (Ok(i) | Err(i)) = l.index_for_position(target);
                let i = i.min(last_char(self.anchor_text(at)));
                if i != self.caret {
                    self.caret = i;
                    return;
                }
            }
        }
        let Some(pos) = self.anchor_index.get(&at).copied() else { return };
        let next = if down { self.anchors.get(pos + 1) } else { pos.checked_sub(1).and_then(|k| self.anchors.get(k)) };
        match next.map(|(a, _)| *a) {
            Some(a) => {
                self.set_thread(Some(a));
                // Keep the column: land under (or over) where the cursor was.
                if let (Some((_, p)), Some(l)) = (&here, self.measured.borrow().layouts.get(&a)) {
                    let b = l.bounds();
                    let lh = l.line_height();
                    let y = if down { b.top() + lh * 0.5 } else { b.bottom() - lh * 0.5 };
                    let (Ok(i) | Err(i)) = l.index_for_position(point(p.x, y));
                    self.caret = i;
                }
                self.caret = self.caret.min(last_char(self.anchor_text(a)));
            }
            None if down => self.set_thread(None),
            None => {}
        }
    }

    fn step_anchor(&mut self, forward: bool) -> bool {
        let Some(pos) = self.thread.and_then(|a| self.anchor_index.get(&a).copied()) else { return false };
        let next = if forward { self.anchors.get(pos + 1) } else { pos.checked_sub(1).and_then(|k| self.anchors.get(k)) };
        match next.map(|(a, _)| *a) {
            Some(a) => {
                self.set_thread(Some(a));
                true
            }
            None => false,
        }
    }

    fn toggle_selection(&mut self, lines: bool) {
        let Some(at) = self.thread else { return };
        self.sel_from = if self.sel_from.is_some() && self.sel_lines == lines { None } else { Some((at, self.caret)) };
        self.sel_lines = lines;
    }

    // ------------------------------------------------------------ objects

    fn insert_block(&mut self, at: usize, block: Block) {
        self.nb.blocks.insert(at, block);
        for c in &mut self.nb.checkpoints {
            if c.block >= at {
                c.block += 1;
            }
        }
        if let Some(p) = &mut self.pending {
            if p.block >= at {
                p.block += 1;
            }
        }
    }

    fn remove_block(&mut self, at: usize) -> Block {
        let block = self.nb.blocks.remove(at);
        for c in &mut self.nb.checkpoints {
            if c.block > at {
                c.block -= 1;
            }
        }
        if let Some(p) = &mut self.pending {
            if p.block > at {
                p.block -= 1;
            }
        }
        block
    }

    fn terminal_block(&self, id: u64) -> Option<usize> {
        self.nb.blocks.iter().position(|b| matches!(b, Block::Terminal { id: t, .. } if *t == id))
    }

    /// ctrl+`: leave the terminal you're typing in, go back into the one under
    /// the cursor, or open a new one under the conversation.
    fn toggle_terminal(&mut self) {
        if self.term_focus.is_some() {
            self.leave_terminal();
            return;
        }
        if let Some(Block::Terminal { id, .. }) = self.thread.and_then(|a| self.nb.blocks.get(a.block)) {
            if self.terms.contains_key(id) {
                self.enter_terminal(*id);
                return;
            }
        }
        let id = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
        let cols = ((COL_W - 2.0 * TERM_PAD - 2.0) / TERM_CELL).floor() as usize;
        match Terminal::spawn(cols, id) {
            Ok(t) => {
                self.terms.insert(id, t);
                // Above a reply that's still being written, which keeps streaming into the last block.
                let at = match self.nb.blocks.last() {
                    Some(Block::Agent { done: false, .. }) => self.nb.blocks.len() - 1,
                    _ => self.nb.blocks.len(),
                };
                self.insert_block(at, Block::Terminal { id, lines: Vec::new() });
                self.enter_terminal(id);
                self.scroll_target = Some(f32::MAX);
                self.touch();
            }
            Err(e) => self.toast(format!("Could not start a shell: {e}")),
        }
    }

    fn enter_terminal(&mut self, id: u64) {
        self.comment = None;
        self.sel_from = None;
        self.thread = None;
        self.term_focus = Some(id);
    }

    /// Esc: the terminal becomes text on the page, the cursor on its last line.
    fn leave_terminal(&mut self) {
        let Some(id) = self.term_focus.take() else { return };
        self.editor.mode = Mode::Normal;
        self.anchors = self.compute_anchors();
        self.anchor_index = self.anchors.iter().enumerate().map(|(i, (a, _))| (*a, i)).collect();
        let block = self.terminal_block(id);
        let last = block.and_then(|b| self.anchors.iter().rev().find(|(a, _)| a.block == b).map(|(a, _)| *a));
        match last {
            Some(a) => self.set_thread(Some(a)),
            None => self.set_thread(None),
        }
    }

    /// Backspace on an object: a drawing or a terminal goes (ctrl+z brings it back).
    fn delete_object(&mut self, at: Anchor) {
        let prev = self
            .anchor_index
            .get(&at)
            .and_then(|i| self.anchors[..*i].iter().rev().find(|(a, _)| a.block != at.block))
            .map(|(a, _)| *a);
        if at.block == INK_BLOCK {
            let Some(ids) = self.ink_groups.iter().find(|g| g.key as usize == at.item).map(|g| g.ids.clone()) else { return };
            self.history.checkpoint(&self.nb.ink);
            self.nb.ink.retain(|s| !ids.contains(&s.id));
            self.last_ink_change = Some(Instant::now());
            self.toast("Drawing removed (ctrl+z brings it back)");
        } else if let Some(Block::Terminal { .. }) = self.nb.blocks.get(at.block) {
            let block = self.remove_block(at.block);
            if let Some((_, Block::Terminal { id, .. })) = self.removed.replace((at.block, block)) {
                self.terms.remove(&id);
            }
            self.toast("Terminal removed (ctrl+z brings it back)");
        } else {
            return;
        }
        self.comment = None;
        self.sel_from = None;
        self.set_thread(prev);
        self.touch();
    }

    /// The page's strokes as drawings: each stroke joins a recent drawing it
    /// was drawn near, both in place (60 px) and in time (4 s).
    fn compute_ink_groups(&self) -> Vec<InkGroup> {
        let mut groups: Vec<InkGroup> = Vec::new();
        for s in &self.nb.ink {
            let (_, b) = s.shape();
            let r = Rect::new(b[0], b[1], b[2] - b[0], b[3] - b[1]);
            let t = self.nb.ink_times.get(&s.origin).copied();
            let n = groups.len();
            let near = |g: &InkGroup| {
                let dx = (g.rect.x - (r.x + r.w)).max(r.x - (g.rect.x + g.rect.w)).max(0.0);
                let dy = (g.rect.y - (r.y + r.h)).max(r.y - (g.rect.y + g.rect.h)).max(0.0);
                dx.hypot(dy) < 60.0
            };
            let join = groups.iter().enumerate().rev().take(8).find(|(k, g)| {
                g.by_agent == s.by_agent
                    && near(g)
                    && match (t, g.t) {
                        (Some(a), Some(b)) => (a - b).abs() < 4.0,
                        // Ink from before times were kept: only the drawing just before it.
                        _ => k + 1 == n,
                    }
            });
            match join.map(|(k, _)| k) {
                Some(k) => {
                    let g = &mut groups[k];
                    g.ids.push(s.id);
                    let (x1, y1) = ((g.rect.x + g.rect.w).max(r.x + r.w), (g.rect.y + g.rect.h).max(r.y + r.h));
                    g.rect.x = g.rect.x.min(r.x);
                    g.rect.y = g.rect.y.min(r.y);
                    g.rect.w = x1 - g.rect.x;
                    g.rect.h = y1 - g.rect.y;
                    if let Some(t) = t {
                        g.t = Some(g.t.map_or(t, |x| x.max(t)));
                    }
                }
                None => groups.push(InkGroup { key: s.origin, ids: vec![s.id], rect: r, t, by_agent: s.by_agent }),
            }
        }
        groups
    }

    fn ex_command(&mut self, cmd: &str, _window: &mut Window) {
        match cmd {
            "auto" | "listen" => self.pi_tap(Duration::from_secs(1)),
            "stop" | "abort" => {
                let _ = self.agent.send(ToAgent::Abort);
                self.toast("Stopped");
            }
            "inkclear" | "clearink" => {
                self.history.checkpoint(&self.nb.ink);
                self.nb.ink.clear();
                self.nb.notes.clear();
                self.toast("Ink cleared (ctrl+z brings it back)");
            }
            "new" | "enew" => {
                let archive = notebook::data_dir().join(format!(
                    "notebook-{}.json",
                    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
                ));
                let _ = self.nb.save(&archive);
                self.nb = Notebook { version: 2, ..Notebook::default() };
                self.terms.clear();
                self.term_focus = None;
                self.removed = None;
                self.history = History::default();
                *self.sent_ink.borrow_mut() = None;
                let _ = self.agent.send(ToAgent::Reset);
                self.scroll = 0.0;
                self.toast("A fresh notebook (the old one is archived)");
            }
            "top" => self.scroll_target = Some(0.0),
            "config" | "settings" | "set" | "font" | "fonts" | "theme" => self.config = Some(ConfigMenu { row: 0, picker: None }),
            "dark" => self.set_dark(true),
            "light" => self.set_dark(false),
            other => self.toast(format!("Not a command: :{other}  (try :w :q :new :auto :stop :inkclear :config)")),
        }
    }

    // ------------------------------------------------------------- config

    fn set_text_scale(&mut self, scale: f32) {
        self.text_scale = scale.clamp(0.6, 2.0);
        save_setting("text_scale", json!(self.text_scale));
    }

    fn set_dark(&mut self, on: bool) {
        theme::set_dark(on);
        // Highlighted code keeps its token colors: parse it again for the new paper.
        self.md_cache.borrow_mut().clear();
        save_setting("theme", json!(if on { "dark" } else { "light" }));
    }

    fn set_font(&mut self, role: theme::Role, name: SharedString) {
        save_setting(role.key(), json!(name.as_ref()));
        theme::set_family(role, name);
    }

    fn load_font_names(&mut self, window: &mut Window) {
        if !self.font_names.is_empty() {
            return;
        }
        let mut names: Vec<String> = window
            .text_system()
            .all_font_names()
            .into_iter()
            .filter(|n| !n.starts_with('.') && !n.is_empty())
            .collect();
        names.sort_by_key(|n| n.to_lowercase());
        names.dedup();
        self.font_names = Rc::new(names.into_iter().map(SharedString::from).collect());
    }

    /// The families a picker shows, best matches of its query first.
    fn picker_matches(&self, query: &str) -> Vec<SharedString> {
        let q = query.trim().to_lowercase();
        if q.is_empty() {
            return self.font_names.to_vec();
        }
        let mut hits: Vec<(bool, SharedString)> = self
            .font_names
            .iter()
            .filter_map(|n| {
                let l = n.to_lowercase();
                l.contains(&q).then(|| (!l.starts_with(&q), n.clone()))
            })
            .collect();
        hits.sort_by_key(|(later, _)| *later);
        hits.into_iter().map(|(_, n)| n).collect()
    }

    /// Step a font row through every family on the machine (h / l).
    fn cycle_font(&mut self, role: theme::Role, forward: bool, window: &mut Window) {
        self.load_font_names(window);
        let n = self.font_names.len();
        if n == 0 {
            return;
        }
        let here = self.font_names.iter().position(|f| *f == theme::family(role));
        let next = match (here, forward) {
            (Some(i), true) => (i + 1) % n,
            (Some(i), false) => (i + n - 1) % n,
            (None, true) => 0,
            (None, false) => n - 1,
        };
        let name = self.font_names[next].clone();
        self.set_font(role, name);
    }

    fn open_picker(&mut self, role: theme::Role, window: &mut Window) {
        self.load_font_names(window);
        let before = theme::family(role);
        let sel = self.font_names.iter().position(|f| *f == before).unwrap_or(0);
        if let Some(c) = &mut self.config {
            c.picker = Some(FontPicker { role, query: String::new(), sel, before });
        }
    }

    /// Change the value on a row: `dir` is -1 / +1 (h / l), 0 activates it (Enter).
    fn config_change(&mut self, row: ConfigRow, dir: i32, window: &mut Window) {
        match row {
            ConfigRow::Theme => self.set_dark(!theme::dark()),
            ConfigRow::Font(role) if dir == 0 => self.open_picker(role, window),
            ConfigRow::Font(role) => self.cycle_font(role, dir > 0, window),
            ConfigRow::Size => {
                let to = match dir {
                    0 => 1.0,
                    d if d > 0 => self.text_scale * 1.1,
                    _ => self.text_scale / 1.1,
                };
                self.set_text_scale(to);
            }
        }
    }

    /// Put a row back to how it ships.
    fn config_reset(&mut self, row: ConfigRow) {
        match row {
            ConfigRow::Theme => self.set_dark(false),
            ConfigRow::Font(role) => self.set_font(role, SharedString::new_static(role.default_family())),
            ConfigRow::Size => self.set_text_scale(1.0),
        }
    }

    /// Keep the picked family; or, with `keep` false, put the old one back.
    fn close_picker(&mut self, keep: bool) {
        let Some(picker) = self.config.as_mut().and_then(|c| c.picker.take()) else { return };
        if keep {
            self.set_font(picker.role, theme::family(picker.role));
        } else {
            theme::set_family(picker.role, picker.before);
        }
    }

    /// Preview the family under the picker's cursor.
    fn picker_preview(&mut self) {
        let Some(picker) = self.config.as_ref().and_then(|c| c.picker.as_ref()) else { return };
        let matches = self.picker_matches(&picker.query);
        if let Some(name) = matches.get(picker.sel.min(matches.len().saturating_sub(1))) {
            theme::set_family(picker.role, name.clone());
        }
    }

    fn config_key(&mut self, key: &vim::Key, window: &mut Window) {
        let Some(c) = &self.config else { return };
        if let Some(query) = c.picker.as_ref().map(|p| p.query.clone()) {
            let last = self.picker_matches(&query).len().saturating_sub(1);
            let Some(picker) = self.config.as_mut().and_then(|c| c.picker.as_mut()) else { return };
            match key.key.as_str() {
                "escape" => return self.close_picker(false),
                "enter" => return self.close_picker(true),
                "down" | "tab" => picker.sel = (picker.sel + 1).min(last),
                "n" | "j" if key.ctrl => picker.sel = (picker.sel + 1).min(last),
                "up" => picker.sel = picker.sel.saturating_sub(1),
                "p" | "k" if key.ctrl => picker.sel = picker.sel.saturating_sub(1),
                "pagedown" => picker.sel = (picker.sel + 8).min(last),
                "pageup" => picker.sel = picker.sel.saturating_sub(8),
                "backspace" => {
                    picker.query.pop();
                    picker.sel = 0;
                }
                _ => match &key.ch {
                    Some(ch) if !key.ctrl && !ch.is_empty() => {
                        picker.query.push_str(ch);
                        picker.sel = 0;
                    }
                    _ => return,
                },
            }
            return self.picker_preview();
        }
        let n = CONFIG_ROWS.len();
        let at = c.row;
        let row = CONFIG_ROWS[at];
        let mut go = |to: usize| {
            if let Some(c) = &mut self.config {
                c.row = to;
            }
        };
        match key.key.as_str() {
            "escape" | "q" => self.config = None,
            "j" | "down" | "tab" => go((at + 1) % n),
            "k" | "up" => go((at + n - 1) % n),
            "h" | "left" => self.config_change(row, -1, window),
            "l" | "right" => self.config_change(row, 1, window),
            "enter" | "space" => self.config_change(row, 0, window),
            "r" => self.config_reset(row),
            _ => {}
        }
    }

    /// `}` / `{`: scroll to the next / previous message.
    fn jump_block(&mut self, forward: bool) {
        let tops: Vec<f32> = {
            let m = self.measured.borrow();
            self.nb
                .blocks
                .iter()
                .zip(m.blocks.iter())
                .filter(|(b, _)| !matches!(b, Block::Space { .. }))
                .map(|(_, (t, _))| (t - 70.0).max(0.0))
                .chain(m.blocks.get(self.nb.blocks.len()).map(|(t, _)| (t + 200.0 - self.vh()).max(0.0)))
                .collect()
        };
        let here = self.scroll_target.filter(|t| *t != f32::MAX).unwrap_or(self.scroll);
        let next = if forward {
            tops.iter().copied().filter(|t| *t > here + 4.0).fold(None, |a: Option<f32>, t| Some(a.map_or(t, |a| a.min(t))))
        } else {
            tops.iter().copied().filter(|t| *t < here - 4.0).fold(None, |a: Option<f32>, t| Some(a.map_or(t, |a| a.max(t))))
        };
        if let Some(t) = next {
            self.follow = false;
            self.scroll_target = Some(t.min(self.max_scroll()));
        }
    }

    /// Keep the editor cursor on screen.
    fn reveal_cursor(&mut self) {
        let Some(top) = self.composer_top() else { return };
        let y = top + self.cursor_vrow() as f32 * EDITOR_LH;
        let h = self.vh();
        if y < self.scroll + 60.0 {
            self.scroll_target = Some((y - 120.0).max(0.0));
        } else if y + EDITOR_LH > self.scroll + h - 90.0 {
            self.scroll_target = Some(y + EDITOR_LH - h + 160.0);
        }
    }

    fn cursor_vrow(&self) -> usize {
        cursor_row(&self.rows, &self.editor)
    }

    fn click_editor(&mut self, x: f32, y: f32) -> bool {
        let (sx, sy) = self.measured.borrow().editor_origin;
        let z = self.zoom;
        if y < sy || x < sx - EDITOR_GUTTER * z || x > sx + COL_W * z {
            return false;
        }
        let vrow = ((y - sy) / (EDITOR_LH * z)).floor() as usize;
        let Some(&(line, start, end)) = self.rows.get(vrow) else { return false };
        let Some(xs) = self.line_xs.get(line) else { return false };
        let target = x - sx + xs[start];
        let col = (start..=end)
            .min_by(|&a, &b| (xs[a] - target).abs().total_cmp(&(xs[b] - target).abs()))
            .unwrap_or(start);
        self.editor.row = line;
        self.editor.col = col.min(self.editor.lines[line].len());
        true
    }

    fn tick(&mut self, _cx: &mut Context<Self>) -> bool {
        let now = Instant::now();
        let dt = now.duration_since(self.last_tick).as_secs_f32().min(0.05);
        self.last_tick = now;
        let mut dirty = self.animate();
        // The comment box being written stays on screen: its bottom (where you type), or its top if it's taller than the view.
        if self.reveal_comment && self.comment.is_some() {
            if let Some((top, bottom)) = self.measured.borrow().comment {
                self.reveal_comment = false;
                let h = self.vh();
                let here = self.scroll_target.filter(|t| *t != f32::MAX).unwrap_or(self.scroll);
                if bottom > here + h - 40.0 || top < here + 40.0 {
                    self.scroll_target = Some((bottom - h + 80.0).min(top - 80.0).max(0.0));
                    self.follow = false;
                    dirty = true;
                }
            }
        }
        let mut ended = Vec::new();
        let mut typed_output = false;
        for (id, t) in &mut self.terms {
            if t.refresh() {
                dirty = true;
                typed_output |= self.term_focus == Some(*id);
                self.save_at.get_or_insert(Instant::now() + Duration::from_secs(2));
            }
            if t.exited() {
                ended.push(*id);
            }
        }
        for id in ended {
            let Some(t) = self.terms.remove(&id) else { continue };
            if let Some(Block::Terminal { lines, .. }) = self.terminal_block(id).and_then(|i| self.nb.blocks.get_mut(i)) {
                *lines = t.snapshot().lines.clone();
            }
            if self.term_focus == Some(id) {
                self.leave_terminal();
                self.toast("The shell exited");
            }
            self.touch();
        }
        // Keep the line being typed on in view.
        if typed_output {
            if let Some((i, t)) = self.term_focus.and_then(|id| Some((self.terminal_block(id)?, self.terms.get(&id)?))) {
                let snap = t.snapshot();
                let line = snap.cursor.map_or(snap.lines.len(), |c| c.0);
                let top = self.measured.borrow().blocks.get(i).map(|b| b.0);
                if let Some(top) = top {
                    let y = top + TERM_HEAD + (line + 1) as f32 * TERM_LH;
                    let here = self.scroll_target.filter(|t| *t != f32::MAX).unwrap_or(self.scroll);
                    if y > here + self.vh() - 40.0 {
                        self.scroll_target = Some(y - self.vh() + 120.0);
                    }
                }
            }
        }
        if let Some((_, at)) = &self.toast {
            if at.elapsed() > Duration::from_millis(2800) {
                self.toast = None;
                dirty = true;
            }
        }
        // Follow the conversation as it grows.
        if self.follow && (self.agent_busy || !self.anims.is_empty()) {
            if let Some(top) = self.composer_top() {
                let want = (top + 220.0 - self.vh()).max(0.0);
                let current = self.scroll_target.filter(|t| *t != f32::MAX).unwrap_or(self.scroll);
                if want > current + 24.0 {
                    self.scroll_target = Some(want);
                }
            }
        }
        if let Some(target) = self.scroll_target {
            let target = if target == f32::MAX {
                match self.composer_top() {
                    Some(top) => (top + 260.0 - self.vh()).max(0.0),
                    None => return true,
                }
            } else {
                target
            };
            let k = 1.0 - (-dt * 11.0).exp();
            let next = self.scroll + (target - self.scroll) * k;
            if (target - next).abs() < 0.5 {
                self.scroll = target;
                self.scroll_target = None;
            } else {
                self.scroll = next;
            }
            dirty = true;
        } else if self.momentum.abs() > 15.0 {
            let max = self.max_scroll().max(self.scroll);
            self.scroll = (self.scroll + self.momentum * dt).clamp(0.0, max);
            self.momentum *= (-dt * 3.5).exp();
            if self.scroll <= 0.0 || self.scroll >= max {
                self.momentum = 0.0;
            }
            dirty = true;
        }
        // Writing with the pen: every 3 s, once the pen has rested a moment,
        // ask Jev whether this pause deserves an answer. No pen ink, no call.
        if self.listen
            && self.pen_new_ink
            && !self.pen_down
            && !self.agent_busy
            && self.jev_pending.is_none()
            && self.pen_lift_at.is_some_and(|t| t.elapsed() > Duration::from_millis(1200))
            && self.last_jev.is_none_or(|t| t.elapsed() > Duration::from_secs(3))
        {
            if self.jev.is_some() {
                self.ask_jev();
            } else if let Some(note) = self.jev_note.take() {
                self.toast(format!("Pause detection is off: {note}"));
            }
            dirty = true;
        }
        if self.save_at.is_some_and(|t| Instant::now() >= t) {
            self.save();
        }
        dirty
    }

    fn save(&mut self) {
        self.save_at = None;
        self.nb.draft = self.editor.text();
        for b in &mut self.nb.blocks {
            if let Block::Terminal { id, lines } = b {
                if let Some(t) = self.terms.get(id) {
                    *lines = t.snapshot().lines.clone();
                }
            }
        }
        let origins: HashSet<u64> = self.nb.ink.iter().map(|s| s.origin).collect();
        self.nb.ink_times.retain(|k, _| origins.contains(k));
        if let Err(e) = self.nb.save(&notebook::path()) {
            self.toast(format!("Could not save: {e}"));
        }
    }
}

/// Jev rejects large images: fit the longest side in 1024 px, dimensions a
/// multiple of 32.
fn fit_for_jev(png: &[u8]) -> Option<Vec<u8>> {
    let img = image::load_from_memory(png).ok()?;
    let (w, h) = (img.width() as f32, img.height() as f32);
    let k = (1024.0 / w.max(h)).min(1.0);
    let round = |v: f32| (((v * k) / 32.0).round().max(1.0) * 32.0) as u32;
    let img = img.resize_exact(round(w), round(h), image::imageops::FilterType::Triangle);
    let mut out = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png).ok()?;
    Some(out)
}

fn bring_to_front(window: &mut Window) {
    window.activate_window();
    let pid = format!("pid:{}", std::process::id());
    std::thread::spawn(move || crate::bridge::hypr_focus(&pid));
}

fn settings_path() -> PathBuf {
    notebook::data_dir().join("settings.json")
}

fn load_settings() -> Value {
    std::fs::read_to_string(settings_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({}))
}

fn save_setting(key: &str, value: Value) {
    let mut s = load_settings();
    if !s.is_object() {
        s = json!({});
    }
    s[key] = value;
    let _ = std::fs::create_dir_all(notebook::data_dir());
    let _ = std::fs::write(settings_path(), s.to_string());
}

fn abs(rect: Rect) -> gpui::Div {
    div()
        .absolute()
        .left(px(rect.x))
        .top(px(rect.y))
        .w(px(rect.w))
        .h(px(rect.h))
}

/// A zero-cost element that reports where it was laid out.
fn probe(f: impl FnOnce(Bounds<Pixels>) + 'static) -> impl IntoElement {
    canvas(move |bounds, _, _| f(bounds), |_, _, _, _| {})
        .absolute()
        .top(px(0.0))
        .left(px(0.0))
        .size_full()
}

fn screen_rect(b: Bounds<Pixels>) -> Rect {
    Rect::new(
        f32::from(b.origin.x),
        f32::from(b.origin.y),
        f32::from(b.size.width),
        f32::from(b.size.height),
    )
}

fn clawd(pixel: f32, w: f32, h: f32) -> impl IntoElement {
    div().flex_none().w(zp(w)).h(zp(h)).child(
        canvas(
            |_, _, _| {},
            move |b, _, window, _| {
                let c = b.center();
                ui::paint_clawd(window, f32::from(c.x), f32::from(c.y), pixel * zoom(), 1.0)
            },
        )
        .size_full(),
    )
}

/// Soft-wrapped layout of an editor's lines in proportional text:
/// rows of (line, start char, end char) and the x of every char boundary.
fn editor_layout(window: &mut Window, editor: &Editor, width: f32) -> (Vec<(usize, usize, usize)>, Vec<Vec<f32>>) {
    let base = font(sans());
    let fs = zp(EDITOR_FONT);
    let width = width * zoom();
    let mut rows = Vec::new();
    let mut line_xs = Vec::new();
    for (li, line) in editor.lines.iter().enumerate() {
        let s: String = line.iter().collect();
        let xs: Vec<f32> = if s.is_empty() {
            vec![0.0]
        } else {
            let run = TextRun {
                len: s.len(),
                font: base.clone(),
                color: hsla(p().ink),
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            let shaped = window.text_system().shape_line(s.clone().into(), fs, &[run], None);
            let mut xs = Vec::with_capacity(line.len() + 1);
            let mut byte = 0;
            xs.push(0.0);
            for c in line {
                byte += c.len_utf8();
                xs.push(f32::from(shaped.x_for_index(byte)));
            }
            xs
        };
        let n = line.len();
        if n == 0 {
            rows.push((li, 0, 0));
        }
        let mut start = 0;
        while start < n {
            let mut end = start + 1;
            while end < n && xs[end + 1] - xs[start] <= width {
                end += 1;
            }
            if end < n {
                if let Some(sp) = (start + 1..=end).rev().find(|&k| line[k - 1] == ' ') {
                    end = sp;
                }
            }
            rows.push((li, start, end));
            start = end;
        }
        line_xs.push(xs);
    }
    (rows, line_xs)
}

fn cursor_row(rows: &[(usize, usize, usize)], editor: &Editor) -> usize {
    rows.iter()
        .rposition(|&(line, start, _)| line == editor.row && start <= editor.col)
        .unwrap_or(0)
}

/// The text of an editor with its vim cursor (bar in insert, block in normal)
/// and visual selection.
fn editor_view(
    editor: &Editor,
    rows: &[(usize, usize, usize)],
    line_xs: &[Vec<f32>],
    show_cursor: bool,
    placeholder: &str,
    id: &'static str,
) -> gpui::Div {
    let mode = editor.mode;
    let cv = cursor_row(rows, editor);
    let (_, cstart, _) = rows.get(cv).copied().unwrap_or((editor.row, 0, 0));
    let xs = &line_xs[editor.row.min(line_xs.len() - 1)];
    let cx_ = xs.get(editor.col).copied().unwrap_or(0.0) - xs.get(cstart).copied().unwrap_or(0.0);
    let cw = xs.get(editor.col + 1).copied().map(|x1| x1 - xs[editor.col]).unwrap_or(8.0).max(6.0);
    let cy = cv as f32 * EDITOR_LH;

    let mut text = div().relative().flex_1().min_w_0().flex().flex_col();
    if let Some(((r0, c0), (r1, c1))) = editor.selection() {
        for (vi, &(li, start, end)) in rows.iter().enumerate() {
            if li < r0 || li > r1 {
                continue;
            }
            let a = if li == r0 { c0.max(start) } else { start };
            let b = if li == r1 { (c1 + 1).min(end.max(start + 1)) } else { end.max(start + 1) };
            if b > a {
                let xs = &line_xs[li];
                let x0 = xs[a.min(xs.len() - 1)] - xs[start];
                let x1 = xs.get(b).copied().unwrap_or(xs[xs.len() - 1] + 8.0) - xs[start];
                text = text.child(
                    div()
                        .absolute()
                        .left(px(x0))
                        .top(zp(vi as f32 * EDITOR_LH + 2.0))
                        .w(px((x1 - x0).max(4.0)))
                        .h(zp(EDITOR_LH - 4.0))
                        .rounded(zp(3.0))
                        .bg(tint(p().accent, 0x26)),
                );
            }
        }
    }
    if show_cursor && mode != Mode::Command {
        let cursor = if mode == Mode::Insert {
            div().absolute().left(px(cx_ - 1.0)).top(zp(cy + 3.0)).w(px(2.0)).h(zp(EDITOR_LH - 6.0)).bg(rgb(p().accent))
        } else {
            div().absolute().left(px(cx_)).top(zp(cy + 3.0)).w(px(cw)).h(zp(EDITOR_LH - 6.0)).rounded(zp(2.0)).bg(tint(p().accent, 0x55))
        };
        text = text.child(cursor.with_animation(
            id,
            Animation::new(Duration::from_millis(1060)).repeat().with_easing(pulsating_between(0.0, 1.0)),
            |el, t| el.opacity(if t < 0.55 { 1.0 } else { 0.25 }),
        ));
    }
    for &(li, start, end) in rows {
        let s: String = editor.lines[li][start..end].iter().collect();
        text = text.child(div().h(zp(EDITOR_LH)).whitespace_nowrap().child(if s.is_empty() { " ".to_owned() } else { s }));
    }
    if editor.lines.len() == 1 && editor.is_empty() && mode == Mode::Insert && !placeholder.is_empty() {
        text = text.child(
            div()
                .absolute()
                .left(zp(1.0))
                .top(zp(0.0))
                .h(zp(EDITOR_LH))
                .text_color(rgb(p().faint))
                .child(placeholder.to_owned()),
        );
    }
    text.text_size(zp(EDITOR_FONT)).line_height(zp(EDITOR_LH)).text_color(rgb(p().ink))
}

// ---------------------------------------------------------------- markdown

fn rich_runs(r: &Rich, base: &Font, color: u32) -> Vec<TextRun> {
    let mut cuts = vec![0, r.text.len()];
    for (range, _) in &r.spans {
        cuts.push(range.start);
        cuts.push(range.end);
    }
    cuts.sort_unstable();
    cuts.dedup();
    let mut runs = Vec::new();
    for w in cuts.windows(2) {
        let (a, b) = (w[0], w[1]);
        if a == b {
            continue;
        }
        let span = r
            .spans
            .iter()
            .find(|(range, _)| range.start <= a && b <= range.end)
            .map(|(_, s)| *s)
            .unwrap_or_default();
        let mut f = if span.code { font(mono()) } else { base.clone() };
        if span.bold {
            f = weighted(&base.family, FontWeight::SEMIBOLD);
        }
        if span.italic {
            f.style = FontStyle::Italic;
        }
        runs.push(TextRun {
            len: b - a,
            font: f,
            color: hsla(if span.link {
                p().accent
            } else if span.code {
                p().ink2
            } else {
                color
            }),
            background_color: span.code.then(|| rgb(p().sunken).into()),
            underline: span.link.then(|| UnderlineStyle {
                thickness: px(1.0),
                color: Some(hsla(p().terra)),
                wavy: false,
            }),
            strikethrough: None,
        });
    }
    runs
}

fn rich(r: &Rich, base: &Font, color: u32) -> StyledText {
    StyledText::new(r.text.clone()).with_runs(rich_runs(r, base, color))
}

/// A block's text, for quoting it in a comment.
// ---- moving through text: byte offsets on char boundaries, vim's word classes

fn next_char(t: &str, o: usize) -> usize {
    let o = o.min(t.len());
    t[o..].chars().next().map_or(t.len(), |c| o + c.len_utf8())
}

fn prev_char(t: &str, o: usize) -> usize {
    t[..o.min(t.len())].char_indices().next_back().map_or(0, |(i, _)| i)
}

fn last_char(t: &str) -> usize {
    prev_char(t, t.len())
}

fn char_class(c: char) -> u8 {
    if c.is_whitespace() {
        0
    } else if c.is_alphanumeric() || c == '_' {
        1
    } else {
        2
    }
}

/// `w`: the start of the next word, if there is one on this text.
fn word_fwd(t: &str, o: usize) -> Option<usize> {
    let mut it = t[o.min(t.len())..].char_indices().map(|(i, c)| (o + i, char_class(c))).peekable();
    let start = it.peek()?.1;
    let mut seen_gap = start == 0;
    for (i, class) in it {
        if class == 0 {
            seen_gap = true;
        } else if seen_gap || class != start {
            return Some(i);
        }
    }
    None
}

/// `b`: the start of the word before the cursor, if there is one on this text.
fn word_back(t: &str, o: usize) -> Option<usize> {
    let before: Vec<(usize, u8)> = t[..o.min(t.len())].char_indices().map(|(i, c)| (i, char_class(c))).collect();
    let mut k = before.len();
    while k > 0 && before[k - 1].1 == 0 {
        k -= 1;
    }
    if k == 0 {
        return None;
    }
    let class = before[k - 1].1;
    while k > 0 && before[k - 1].1 == class {
        k -= 1;
    }
    Some(before[k].0)
}

/// `e`: the last char of this or the next word.
fn word_end(t: &str, o: usize) -> usize {
    let chars: Vec<(usize, u8)> = t.char_indices().map(|(i, c)| (i, char_class(c))).collect();
    let mut k = chars.iter().position(|(i, _)| *i >= o).unwrap_or(chars.len()) + 1;
    while k < chars.len() && chars[k].1 == 0 {
        k += 1;
    }
    if k >= chars.len() {
        return last_char(t);
    }
    let class = chars[k].1;
    while k + 1 < chars.len() && chars[k + 1].1 == class {
        k += 1;
    }
    chars[k].0
}

/// Paint a selection and the cursor into a text's runs.
fn mark_runs(runs: Vec<TextRun>, sel: Option<std::ops::Range<usize>>, caret: Option<std::ops::Range<usize>>) -> Vec<TextRun> {
    let cuts: Vec<usize> = sel.iter().chain(caret.iter()).flat_map(|r| [r.start, r.end]).collect();
    if cuts.is_empty() {
        return runs;
    }
    let mut out = Vec::new();
    let mut at = 0;
    for run in runs {
        let end = at + run.len;
        let mut points: Vec<usize> = cuts.iter().copied().filter(|c| *c > at && *c < end).collect();
        points.extend([at, end]);
        points.sort_unstable();
        points.dedup();
        for w in points.windows(2) {
            let (a, b) = (w[0], w[1]);
            let mut r = run.clone();
            r.len = b - a;
            if caret.as_ref().is_some_and(|c| c.start <= a && b <= c.end) {
                r.background_color = Some(hsla(p().ink));
                r.color = hsla(p().paper);
            } else if sel.as_ref().is_some_and(|s| s.start <= a && b <= s.end) {
                r.background_color = Some(tint(p().accent, 0x38).into());
            }
            out.push(r);
        }
        at = end;
    }
    out
}

fn plain_run(len: usize, f: Font, color: u32) -> TextRun {
    TextRun { len, font: f, color: hsla(color), background_color: None, underline: None, strikethrough: None }
}

fn md_plain(b: &Md) -> String {
    match b {
        Md::Heading(_, r) | Md::Paragraph(r) => r.text.clone(),
        Md::List { items, .. } => items
            .iter()
            .map(|item| item.iter().map(md_plain).collect::<Vec<_>>().join(" "))
            .collect::<Vec<_>>()
            .join("\n"),
        Md::Quote(inner) => inner.iter().map(md_plain).collect::<Vec<_>>().join("\n"),
        Md::Code { lines, .. } => lines
            .iter()
            .map(|l| l.iter().map(|t| t.text.as_str()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n"),
        Md::Svg { .. } => "(the diagram)".into(),
        Md::Pending(t) => t.clone(),
        Md::Table { header, rows } => std::iter::once(header)
            .chain(rows.iter())
            .map(|r| r.iter().map(|c| c.text.as_str()).collect::<Vec<_>>().join(" | "))
            .collect::<Vec<_>>()
            .join("\n"),
        Md::Rule => String::new(),
    }
}

fn md_blocks(blocks: &[Md], depth: usize) -> Vec<AnyElement> {
    let body = font(sans());
    blocks
        .iter()
        .map(|b| match b {
            Md::Heading(level, r) => {
                let size = match level {
                    // syncpi's .prose h1/h2/h3
                    1 => 22.0,
                    2 => 19.0,
                    3 => 16.5,
                    _ => 15.0,
                };
                let f = weighted(serif(), FontWeight::MEDIUM);
                div()
                    .mt(zp(if *level <= 2 { 12.0 } else { 6.0 }))
                    .text_size(zp(size))
                    .line_height(zp(size * 1.25))
                    .child(rich(r, &f, p().ink))
                    .into_any_element()
            }
            Md::Paragraph(r) => div()
                .text_size(zp(15.0))
                .line_height(zp(24.75))
                .child(rich(r, &body, p().ink))
                .into_any_element(),
            Md::List { start, items } => div()
                .flex()
                .flex_col()
                .gap(zp(3.0))
                .children(items.iter().enumerate().map(|(i, item)| {
                    let marker = match start {
                        Some(s) => format!("{}.", s + i as u64),
                        None => if depth % 2 == 0 { "•" } else { "◦" }.to_owned(),
                    };
                    div()
                        .flex()
                        .child(
                            div()
                                .flex_none()
                                .w(zp(22.0))
                                .text_size(zp(15.0))
                                .line_height(zp(24.75))
                                .text_color(rgb(p().muted))
                                .child(marker),
                        )
                        .child(div().flex_1().min_w_0().flex().flex_col().gap(zp(3.0)).children(md_blocks(item, depth + 1)))
                }))
                .into_any_element(),
            Md::Quote(inner) => div()
                .pl(zp(12.0))
                .border_l_2()
                .border_color(rgb(p().rule_strong))
                .text_color(rgb(p().ink2))
                .flex()
                .flex_col()
                .gap_2()
                .children(md_blocks(inner, depth))
                .into_any_element(),
            Md::Code { lang, lines } => div()
                .relative()
                .rounded(zp(8.0))
                .bg(rgb(p().code_bg))
                .px(zp(12.0))
                .py(zp(10.0))
                .font_family(mono())
                .text_size(zp(12.5))
                .line_height(zp(19.5))
                .when(!lang.is_empty(), |d| {
                    d.child(
                        div()
                            .absolute()
                            .top(zp(6.0))
                            .right(zp(10.0))
                            .text_size(zp(9.5))
                            .text_color(rgb(p().faint))
                            .child(lang.to_uppercase()),
                    )
                })
                .children(lines.iter().map(|tokens| {
                    if tokens.is_empty() {
                        return div().h(zp(19.5)).into_any_element();
                    }
                    let text: String = tokens.iter().map(|t| t.text.as_str()).collect();
                    let runs = tokens
                        .iter()
                        .map(|t| {
                            let mut f = if t.bold { weighted(mono(), FontWeight::MEDIUM) } else { font(mono()) };
                            if t.italic {
                                f.style = FontStyle::Italic;
                            }
                            TextRun {
                                len: t.text.len(),
                                font: f,
                                color: hsla(t.color),
                                background_color: None,
                                underline: None,
                                strikethrough: None,
                            }
                        })
                        .collect();
                    div().child(StyledText::new(text).with_runs(runs)).into_any_element()
                }))
                .into_any_element(),
            Md::Svg { path, width, height } => div()
                .flex()
                .child(
                    div()
                        .p(zp(12.0))
                        .rounded(zp(10.0))
                        .bg(rgb(p().surface))
                        .border_1()
                        .border_color(rgb(p().rule))
                        .child(img(path.clone()).w(zp(*width)).h(zp(*height))),
                )
                .into_any_element(),
            Md::Pending(text) => div()
                .font_family(serif())
                .italic()
                .text_size(zp(14.0))
                .text_color(rgb(p().muted))
                .child(text.clone())
                .with_animation(
                    ("pending", text.len()),
                    Animation::new(Duration::from_millis(1200)).repeat().with_easing(pulsating_between(0.35, 1.0)),
                    |el, t| el.opacity(t),
                )
                .into_any_element(),
            Md::Table { header, rows } => {
                let cell = |r: &Rich, head: bool| {
                    let f = if head { weighted(sans(), FontWeight::SEMIBOLD) } else { font(sans()) };
                    div()
                        .flex_1()
                        .px(zp(10.0))
                        .py(zp(6.0))
                        .text_size(zp(14.0))
                        .line_height(zp(21.0))
                        .child(rich(r, &f, p().ink))
                };
                div()
                    .flex()
                    .flex_col()
                    .rounded(zp(8.0))
                    .border_1()
                    .border_color(rgb(p().rule))
                    .overflow_hidden()
                    .child(div().flex().bg(rgb(p().sunken)).children(header.iter().map(|r| cell(r, true))))
                    .children(rows.iter().map(|row| {
                        div()
                            .flex()
                            .border_t_1()
                            .border_color(rgb(p().rule))
                            .children(row.iter().map(|r| cell(r, false)))
                    }))
                    .into_any_element()
            }
            Md::Rule => div().my_2().h(zp(1.0)).bg(rgb(p().rule)).into_any_element(),
        })
        .collect()
}

// ---------------------------------------------------------------- render

fn paint_strokes(window: &mut Window, strokes: &[&Stroke], dx: f32, dy: f32, scale: f32, clip: Option<(f32, f32)>) {
    for s in strokes {
        let (outline, b) = s.shape();
        if let Some((top, bottom)) = clip {
            if b[3] < top - 20.0 || b[1] > bottom + 20.0 {
                continue;
            }
        }
        ui::paint_polygon_at(window, &outline, dx, dy, scale, theme::ink(s.color), s.alpha());
    }
}

/// A stroke ready to paint: its cached outline and look.
type Painted = (std::rc::Rc<Vec<(f32, f32)>>, u32, f32);

fn note_el(note: &Note, x: f32, y: f32) -> AnyElement {
    let mut el = div()
        .absolute()
        .left(px(x))
        .top(px(y))
        .max_w(zp(if note.badge { 320.0 } else { 460.0 }))
        .flex()
        .gap_2();
    if note.badge {
        el = el.child(div().mt(zp(5.0)).child(clawd(1.7, 22.0, 16.0)));
    }
    el.child(
        div()
            .font_family(serif())
            .italic()
            .text_size(zp(note.size))
            .line_height(zp(note.size * 1.3))
            .text_color(rgb(note.color))
            .child(note.text.clone()),
    )
    .into_any_element()
}

impl Pad {
    fn md(&self, text: &str) -> Rc<Vec<Md>> {
        let key = hash(text);
        if let Some(b) = self.md_cache.borrow().get(&key) {
            return b.clone();
        }
        let blocks = Rc::new(markdown::parse(text));
        let mut cache = self.md_cache.borrow_mut();
        if cache.len() > 400 {
            cache.clear();
        }
        cache.insert(key, blocks.clone());
        blocks
    }

    /// "You" / "Claude" as in syncpi's thread: a who label, then the body.
    fn who(label: &str, color: u32) -> gpui::Div {
        div()
            .flex_none()
            .text_size(zp(13.5))
            .line_height(zp(24.0))
            .font(weighted(sans(), FontWeight::SEMIBOLD))
            .text_color(rgb(color))
            .child(label.to_owned())
    }

    /// Every place the thread cursor can stop, in reading order.
    fn compute_anchors(&self) -> Vec<(Anchor, String)> {
        let mut out = Vec::new();
        for (i, block) in self.nb.blocks.iter().enumerate() {
            match block {
                Block::User { text, .. } => {
                    for (l, line) in text.split('\n').enumerate() {
                        if !line.trim().is_empty() {
                            out.push((Anchor { block: i, part: 0, item: l }, line.to_owned()));
                        }
                    }
                }
                Block::Agent { parts, .. } => {
                    for (pi, part) in parts.iter().enumerate() {
                        match part {
                            Part::Text(t) => {
                                for (j, b) in self.md(t).iter().enumerate() {
                                    let plain = md_plain(b);
                                    if !plain.trim().is_empty() {
                                        out.push((Anchor { block: i, part: pi, item: j }, plain));
                                    }
                                }
                            }
                            Part::Sketch { .. } => out.push((Anchor { block: i, part: pi, item: 0 }, "(the sketch in your reply)".into())),
                            Part::Tool { .. } | Part::Space { .. } => {}
                        }
                    }
                }
                Block::Figure { prompt, .. } => out.push((Anchor { block: i, part: 0, item: 0 }, prompt.clone())),
                Block::Terminal { id, lines } => {
                    let lines = self.terms.get(id).map_or(lines, |t| &t.snapshot().lines);
                    for (l, line) in lines.iter().enumerate() {
                        let text = terminal::plain(line);
                        if !text.trim().is_empty() {
                            out.push((Anchor { block: i, part: 0, item: l }, text));
                        }
                    }
                }
                Block::Space { .. } => {}
            }
        }
        // Drawings on the page take their place among the passages by height.
        let m = self.measured.borrow();
        let y_of = |a: &Anchor| m.anchors.get(a).map(|r| r.y).or_else(|| m.blocks.get(a.block).map(|b| b.0)).unwrap_or(f32::MAX);
        for g in &self.ink_groups {
            let at = out.iter().position(|(a, _)| a.block != INK_BLOCK && y_of(a) > g.rect.y).unwrap_or(out.len());
            let who = if g.by_agent { "my" } else { "your" };
            let text = format!(
                "({who} drawing on the page at x={:.0}..{:.0}, y={:.0}..{:.0})",
                g.rect.x,
                g.rect.x + g.rect.w,
                g.rect.y,
                g.rect.y + g.rect.h
            );
            out.insert(at, (Anchor { block: INK_BLOCK, part: 0, item: g.key as usize }, text));
        }
        out
    }

    /// A passage the thread cursor can land on: measured, highlighted when
    /// the cursor is there, with the comment box under it when one is open.
    fn anchored(&self, a: Anchor, el: impl IntoElement) -> AnyElement {
        let m = self.measured.clone();
        let (scroll, z, half_w) = (self.scroll, self.zoom, self.viewport.0 / 2.0);
        let object = !self.is_text(a);
        let focused = object && self.thread == Some(a) && self.term_focus.is_none();
        let mut d = div().relative();
        if focused || (object && self.sel_range(a).is_some()) {
            d = d
                .child(
                    div()
                        .absolute()
                        .left(zp(-12.0))
                        .right(zp(-12.0))
                        .top(zp(-2.0))
                        .bottom(zp(-2.0))
                        .rounded(zp(6.0))
                        .bg(tint(p().accent, 0x0f)),
                )
                .when(focused, |d| d.child(div().absolute().left(zp(-12.0)).top(zp(1.0)).bottom(zp(1.0)).w(zp(3.0)).rounded(zp(1.5)).bg(rgb(p().accent))));
        }
        d = d.child(el).child(probe(move |b| {
            let r = screen_rect(b);
            let doc = Rect::new((r.x - half_w) / z, r.y / z + scroll, r.w / z, r.h / z);
            m.borrow_mut().anchors.insert(a, doc);
        }));
        let mut col = div().flex().flex_col().child(d);
        if let Some(c) = self.comment.as_ref().filter(|c| c.at == a) {
            col = col.child(self.comment_box(c));
        }
        col.into_any_element()
    }

    /// The comment being written on a selection, with what it quotes.
    fn comment_box(&self, c: &CommentDraft) -> gpui::Div {
        let (rows, xs) = &self.comment_rows;
        let m = self.measured.clone();
        let (scroll, z) = (self.scroll, self.zoom);
        div()
            .relative()
            // Its own type, also inside a terminal: the editor's cursor is placed for IBM Plex Sans.
            .font_family(sans())
            .text_size(zp(EDITOR_FONT))
            .line_height(zp(EDITOR_LH))
            .text_color(rgb(p().ink_dark))
            .whitespace_normal()
            .child(probe(move |b| {
                let r = screen_rect(b);
                m.borrow_mut().comment = Some((r.y / z + scroll, (r.y + r.h) / z + scroll));
            }))
                    .mt(zp(8.0))
                    .mb(zp(6.0))
                    .p(zp(10.0))
                    .rounded(zp(8.0))
                    .bg(rgb(p().surface))
                    .border_1()
                    .border_color(rgb(p().rule))
                    .flex()
                    .flex_col()
                    .gap(zp(6.0))
                    .child(
                        div()
                            .pl(zp(10.0))
                            .border_l_2()
                            .border_color(rgb(p().accent))
                            .text_size(zp(13.5))
                            .line_height(zp(20.0))
                            .text_color(rgb(p().ink2))
                            .line_clamp(3)
                            .child(c.excerpt.clone()),
                    )
                    .child(editor_view(&c.editor, rows, xs, self.window_active, "Comment on this…", "comment-cursor"))
                    .child(div().text_size(zp(11.5)).text_color(rgb(p().faint)).child("ctrl⏎ comments · esc twice cancels"))
    }

    /// A heading or paragraph of a reply, as md_blocks draws it, with the cursor and selection painted in.
    fn md_text(&self, a: Anchor, b: &Md) -> Option<AnyElement> {
        Some(match b {
            Md::Heading(level, r) => {
                let size = match level {
                    // syncpi's .prose h1/h2/h3
                    1 => 22.0,
                    2 => 19.0,
                    3 => 16.5,
                    _ => 15.0,
                };
                let runs = rich_runs(r, &weighted(serif(), FontWeight::MEDIUM), p().ink);
                div()
                    .mt(zp(if *level <= 2 { 12.0 } else { 6.0 }))
                    .text_size(zp(size))
                    .line_height(zp(size * 1.25))
                    .child(self.text_el(a, r.text.clone(), runs, None))
                    .into_any_element()
            }
            Md::Paragraph(r) => div()
                .text_size(zp(15.0))
                .line_height(zp(24.75))
                .child(self.text_el(a, r.text.clone(), rich_runs(r, &font(sans()), p().ink), None))
                .into_any_element(),
            _ => return None,
        })
    }

    /// An inline terminal: the live screen while its shell runs, the saved transcript after.
    fn render_terminal(&self, i: usize, id: u64, saved: &[terminal::TermLine]) -> AnyElement {
        let live = self.terms.get(&id);
        let (lines, cursor) = match live {
            Some(t) => (t.snapshot().lines.as_slice(), t.snapshot().cursor),
            None => (saved, None),
        };
        let focused = self.term_focus == Some(id);
        let shell = std::env::var("SHELL").ok().and_then(|s| s.rsplit('/').next().map(str::to_owned)).unwrap_or_else(|| "shell".into());
        let status = if focused {
            "esc leaves"
        } else if live.is_some() {
            "i or ctrl+` to type · ⌫ removes"
        } else {
            "ended · ⌫ removes"
        };
        let header = div()
            .h(zp(TERM_HEAD))
            .px(zp(TERM_PAD))
            .flex()
            .items_center()
            .justify_between()
            .child(div().font_family(mono()).italic().text_size(zp(12.0)).text_color(rgb(p().muted)).child(shell))
            .child(div().text_size(zp(11.5)).text_color(rgb(p().faint)).child(status));
        // Only the lines near the view are built; the rest are spacers.
        let top = self.measured.borrow().blocks.get(i).map(|b| b.0 + TERM_HEAD);
        let n = lines.len();
        let (lo, hi) = match top {
            Some(t) if n > 80 => {
                let lo = (((self.scroll - 600.0 - t) / TERM_LH).floor().max(0.0) as usize).min(n);
                let hi = (((self.scroll + self.vh() + 600.0 - t) / TERM_LH).ceil().max(0.0) as usize).clamp(lo, n);
                (lo, hi)
            }
            _ => (0, n),
        };
        let mut body = div()
            .px(zp(TERM_PAD))
            .pb(zp(10.0))
            .flex()
            .flex_col()
            .font_family(mono())
            .text_size(zp(TERM_FONT))
            .line_height(zp(TERM_LH))
            .text_color(rgb(p().ink));
        if lo > 0 {
            body = body.child(div().flex_none().h(zp(lo as f32 * TERM_LH)));
        }
        for (l, line) in lines.iter().enumerate().take(hi).skip(lo) {
            let a = Anchor { block: i, part: 0, item: l };
            let mut row = div().relative().flex_none().h(zp(TERM_LH)).whitespace_nowrap();
            let mut text: String = line.iter().map(|s| s.text.as_str()).collect();
            let has_text = !text.trim().is_empty();
            let mut runs: Vec<TextRun> = line
                .iter()
                .map(|s| TextRun {
                    background_color: s.bg.map(|b| rgb(b).into()),
                    ..plain_run(s.text.len(), if s.bold { weighted(mono(), FontWeight::MEDIUM) } else { font(mono()) }, s.fg)
                })
                .collect();
            // The shell's cursor: the cell at its column, painted into the text (fallback glyphs
            // such as ❯ or file icons are wider than a cell, so a box on the grid would drift).
            let mut cell = None;
            if let Some((_, col)) = cursor.filter(|c| c.0 == l) {
                let (mut cells, mut at) = (0, text.len());
                for (i, ch) in text.char_indices() {
                    if cells >= col {
                        at = i;
                        break;
                    }
                    cells += unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0).max(1);
                }
                if cells < col || at == text.len() {
                    // Past the end of the line: pad out to the cursor and put a blank cell there.
                    let pad = col.saturating_sub(cells) + 1;
                    text.push_str(&" ".repeat(pad));
                    runs.push(plain_run(pad, font(mono()), p().ink));
                    at = text.len() - 1;
                }
                cell = Some(at..next_char(&text, at));
            }
            if !text.is_empty() {
                row = row.child(if has_text {
                    self.text_el(a, text, runs, if focused { cell } else { None })
                } else {
                    StyledText::new(text).with_runs(mark_runs(runs, None, if focused { cell } else { None }))
                });
            }
            body = body.child(if has_text { self.anchored(a, row) } else { row.into_any_element() });
        }
        if hi < n {
            body = body.child(div().flex_none().h(zp((n - hi) as f32 * TERM_LH)));
        }
        div()
            .rounded(zp(8.0))
            .border_1()
            .border_color(rgb(if focused { p().accent } else { p().rule }))
            .bg(rgb(p().term_bg))
            .flex()
            .flex_col()
            .child(header)
            .child(body)
            .into_any_element()
    }

    fn render_block(&self, i: usize, block: &Block) -> AnyElement {
        match block {
            Block::Space { height } => div().h(zp(*height)).into_any_element(),
            Block::Terminal { id, lines } => self.render_terminal(i, *id, lines),
            Block::User { text, attachment, quote, .. } => {
                let mut body = div().flex_1().min_w_0().flex().flex_col();
                if let Some(q) = quote {
                    body = body.child(
                        div()
                            .mb(zp(4.0))
                            .pl(zp(10.0))
                            .border_l_2()
                            .border_color(rgb(p().accent))
                            .text_size(zp(13.5))
                            .line_height(zp(20.0))
                            .text_color(rgb(p().muted))
                            .line_clamp(3)
                            .child(q.clone()),
                    );
                }
                if !text.trim().is_empty() {
                    for (l, line) in text.split('\n').enumerate() {
                        let a = Anchor { block: i, part: 0, item: l };
                        let row = div().min_h(zp(24.0)).text_size(zp(15.0)).line_height(zp(24.0)).text_color(rgb(p().ink2));
                        body = body.child(if line.trim().is_empty() {
                            row.into_any_element()
                        } else {
                            let run = plain_run(line.len(), font(sans()), p().ink2);
                            self.anchored(a, row.child(self.text_el(a, line.to_owned(), vec![run], None)))
                        });
                    }
                }
                if let Some(a) = attachment {
                    body = body.child(
                        div()
                            .flex()
                            .items_center()
                            .gap(zp(6.0))
                            .mt(zp(2.0))
                            .text_size(zp(12.0))
                            .text_color(rgb(p().muted))
                            .child(div().size(zp(6.0)).rounded_full().bg(rgb(p().user)))
                            .child(if a.is_empty() {
                                div().italic().text_color(rgb(p().faint)).child("reading your ink…")
                            } else {
                                div().child(a.clone())
                            }),
                    );
                }
                div()
                    .flex()
                    .gap(zp(12.0))
                    .items_start()
                    .child(Self::who("You", p().user).w(zp(EDITOR_GUTTER - 12.0)))
                    .child(body)
                    .into_any_element()
            }
            Block::Agent { parts, done, error } => {
                let last = i + 1 == self.nb.blocks.len();
                let streaming = last && !done && self.agent_busy;
                let mut content = div().flex_1().min_w_0().flex().flex_col().gap(zp(10.0));
                if streaming && parts.is_empty() {
                    content = content.child(
                        div()
                            .h(zp(24.0))
                            .flex()
                            .items_center()
                            .font_family(serif())
                            .italic()
                            .text_size(zp(14.0))
                            .text_color(rgb(p().muted))
                            .child("thinking…")
                            .with_animation(
                                ("thinking", i),
                                Animation::new(Duration::from_millis(1100)).repeat().with_easing(pulsating_between(0.3, 1.0)),
                                |el, t| el.opacity(t),
                            ),
                    );
                }

                let n_parts = parts.len();
                for (pi, part) in parts.iter().enumerate() {
                    match part {
                        Part::Text(t) => {
                            let blocks = self.md(t);
                            let mut prose = div().flex().flex_col().gap(zp(10.0));
                            for (j, b) in blocks.iter().enumerate() {
                                let a = Anchor { block: i, part: pi, item: j };
                                let el = self.md_text(a, b).unwrap_or_else(|| md_blocks(std::slice::from_ref(b), 0).pop().unwrap());
                                prose = prose.child(self.anchored(a, el));
                            }
                            if streaming && pi + 1 == n_parts && self.msg_open {
                                prose = prose.child(
                                    div()
                                        .text_color(rgb(p().accent))
                                        .child("▍")
                                        .with_animation(
                                            ("caret", i),
                                            Animation::new(Duration::from_millis(1000)).repeat().with_easing(pulsating_between(0.0, 1.0)),
                                            |el, t| el.opacity(if t > 0.5 { 1.0 } else { 0.0 }),
                                        ),
                                );
                            }
                            content = content.child(prose);
                        }
                        Part::Space { height } => {
                            content = content.child(div().h(zp(*height - 10.0).max(zp(0.0))));
                        }
                        Part::Tool { name, summary } => {
                            content = content.child(
                                div()
                                    .flex()
                                    .min_w_0()
                                    .gap(zp(8.0))
                                    .text_size(zp(13.0))
                                    .child(div().flex_none().text_color(rgb(p().ink2)).child(match name.as_str() {
                                        "bash" => "Ran".to_owned(),
                                        "read" | "browser_read" => "Read".to_owned(),
                                        "browser_search" => "Searched".to_owned(),
                                        "browser_open" => "Opened".to_owned(),
                                        "browser_snapshot" => "Looked at tab".to_owned(),
                                        "browser_act" => "Browser".to_owned(),
                                        "browser_show" => "Showed you tab".to_owned(),
                                        "browser_close" => "Closed tab".to_owned(),
                                        "write" => "Wrote".to_owned(),
                                        "edit" => "Edited".to_owned(),
                                        other => other.to_owned(),
                                    }))
                                    // One line, cut at the column with an ellipsis (syncpi's .tool-arg).
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .truncate()
                                            .font_family(mono())
                                            .text_size(zp(12.5))
                                            .text_color(rgb(p().muted))
                                            .child({
                                                // Mono: 0.6 em per char, after a verb up to "Edited" wide.
                                                let fits = ((COL_W - EDITOR_GUTTER - 70.0) / (12.5 * 0.6)) as usize;
                                                let line = summary.split_whitespace().collect::<Vec<_>>().join(" ");
                                                if line.chars().count() > fits {
                                                    format!("{}…", line.chars().take(fits - 1).collect::<String>())
                                                } else {
                                                    line
                                                }
                                            }),
                                    ),
                            );
                        }
                        Part::Sketch { strokes, labels, height } => {
                            let z = self.zoom;
                            let mut drawn: Vec<Stroke> = strokes.clone();
                            if let Some(a) = self.anims.front() {
                                if matches!(a.target, Target::Sketch(b, p) if b == i && p == pi) {
                                    drawn.push(Stroke {
                                        samples: a.stroke.samples[..a.shown.min(a.stroke.samples.len())].to_vec(),
                                        ..a.stroke.clone()
                                    });
                                }
                            }
                            content = content.child(self.anchored(
                                Anchor { block: i, part: pi, item: 0 },
                                div()
                                    .relative()
                                    .w(zp(SKETCH_W))
                                    .h(zp(*height))
                                    .child(
                                        canvas(
                                            |_, _, _| {},
                                            move |b, _, window, _| {
                                                let refs: Vec<&Stroke> = drawn.iter().collect();
                                                paint_strokes(window, &refs, f32::from(b.origin.x), f32::from(b.origin.y), z, None);
                                            },
                                        )
                                        .absolute()
                                        .size_full(),
                                    )
                                    .children(labels.iter().map(|n| note_el(n, n.x * z, n.y * z))),
                            ));
                        }
                    }
                }
                if let Some(e) = error.as_ref().filter(|_| *done || !self.agent_busy) {
                    content = content.child(div().text_size(zp(13.0)).text_color(rgb(p().error)).child(e.clone()));
                }
                div()
                    .flex()
                    .gap(zp(12.0))
                    .items_start()
                    .child(div().w(zp(EDITOR_GUTTER - 12.0)).pt(zp(3.0)).child(clawd(2.0, 30.0, 18.0)))
                    .child(content)
                    .into_any_element()
            }
            Block::Figure { prompt, from, image, width, height, open } => {
                let measured = self.measured.clone();
                let mut card = div()
                    .relative()
                    .flex()
                    .flex_col()
                    .gap(zp(12.0))
                    .p(zp(18.0))
                    .rounded(zp(12.0))
                    .bg(rgb(p().surface))
                    .border_1()
                    .border_color(rgb(p().rule))
                    .child(
                        div()
                            .font_family(mono())
                            .text_size(zp(11.0))
                            .text_color(rgb(p().accent))
                            .child(format!("{} ASKS", from.to_uppercase())),
                    )
                    .child(div().font_family(serif()).text_size(zp(21.0)).line_height(zp(27.0)).child(prompt.clone()));
                if let Some(path) = image {
                    let max_w = COL_W - 38.0;
                    let k = (max_w / *width as f32).min(1.6);
                    let (iw, ih) = (*width as f32 * k, *height as f32 * k);
                    let m = measured.clone();
                    let (scroll, z, half_w) = (self.scroll, self.zoom, self.viewport.0 / 2.0);
                    card = card.child(
                        div()
                            .relative()
                            .w(zp(iw))
                            .h(zp(ih))
                            .rounded(zp(6.0))
                            .overflow_hidden()
                            .child(img(path.clone()).size_full())
                            .child(probe(move |b| {
                                let r = screen_rect(b);
                                m.borrow_mut().figures.insert(i, Rect::new((r.x - half_w) / z, r.y / z + scroll, r.w / z, r.h / z));
                            })),
                    );
                }
                if *open {
                    let button = |label: &'static str, action: Action, dark: bool| {
                        let m = measured.clone();
                        div()
                            .relative()
                            .px(zp(14.0))
                            .py(zp(6.0))
                            .rounded(zp(6.0))
                            .text_size(zp(13.0))
                            .when(dark, |d| d.bg(rgb(p().ink)).text_color(rgb(p().paper)))
                            .when(!dark, |d| d.border_1().border_color(rgb(p().rule_strong)).text_color(rgb(p().ink2)))
                            .child(label)
                            .child(probe(move |b| m.borrow_mut().hits.push((screen_rect(b), action))))
                    };
                    card = card.child(
                        div()
                            .flex()
                            .justify_end()
                            .gap_2()
                            .child(button("Skip", Action::Skip, false))
                            .child(button("Send", Action::Send, true)),
                    );
                } else {
                    card = card.child(div().text_size(zp(12.0)).text_color(rgb(p().muted)).child("Sent"));
                }
                self.anchored(Anchor { block: i, part: 0, item: 0 }, card)
            }
        }
    }

    /// The composer: just the next row of the thread, free text, edited with vim.
    /// The `:config` card: theme, the three typefaces and text size; or, picking
    /// a typeface, a filtered list of every family on the machine.
    fn render_config(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(config) = &self.config else { return div().into_any_element() };
        let hint = |text: &'static str| div().font_family(mono()).text_size(px(11.0)).text_color(rgb(p().faint)).child(text);
        let heading = |text: String| {
            div().font_family(mono()).text_size(px(10.5)).text_color(rgb(p().terra)).child(text)
        };
        let mut card = div()
            .w(px(460.0))
            .p(px(16.0))
            .rounded(px(16.0))
            .bg(rgb(p().card))
            .border_1()
            .border_color(rgb(p().card_border))
            .shadow_lg()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .text_color(rgb(p().ink))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation());

        if let Some(picker) = &config.picker {
            let matches = self.picker_matches(&picker.query);
            let sel = picker.sel.min(matches.len().saturating_sub(1));
            const SHOWN: usize = 9;
            let start = sel.saturating_sub(SHOWN / 2).min(matches.len().saturating_sub(SHOWN));
            let query = if picker.query.is_empty() {
                div().text_color(rgb(p().faint)).child("type to filter…")
            } else {
                div().child(format!("{}▏", picker.query))
            };
            card = card
                .child(heading(format!("{} FONT", picker.role.label().to_uppercase())))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .h(px(36.0))
                        .px(px(12.0))
                        .rounded(px(10.0))
                        .bg(rgb(p().surface))
                        .border_1()
                        .border_color(rgb(p().rule_strong))
                        .font_family(mono())
                        .text_size(px(13.0))
                        .child(query)
                        .child(div().flex_1())
                        .child(
                            div()
                                .text_size(px(11.0))
                                .text_color(rgb(p().faint))
                                .child(format!("{} of {}", if matches.is_empty() { 0 } else { sel + 1 }, matches.len())),
                        ),
                );
            let mut list = div().flex().flex_col().gap(px(2.0)).min_h(px(SHOWN as f32 * 34.0));
            for (i, name) in matches.iter().enumerate().skip(start).take(SHOWN) {
                let picked = i == sel;
                let is_default = name.as_ref() == picker.role.default_family();
                list = list.child(
                    div()
                        .flex()
                        .items_center()
                        .h(px(32.0))
                        .px(px(12.0))
                        .rounded(px(8.0))
                        .when(picked, |d| d.bg(rgb(p().sunken)))
                        .child(div().w(px(3.0)).h(px(16.0)).mr(px(10.0)).rounded(px(1.5)).when(picked, |d| d.bg(rgb(p().accent))))
                        .child(div().font_family(name.clone()).text_size(px(16.0)).child(name.clone()))
                        .child(div().flex_1())
                        .when(is_default, |d| d.child(hint("default")))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                if let Some(p) = this.config.as_mut().and_then(|c| c.picker.as_mut()) {
                                    p.sel = i;
                                }
                                this.picker_preview();
                                this.close_picker(true);
                                cx.notify();
                            }),
                        ),
                );
            }
            if matches.is_empty() {
                list = list.child(div().px(px(12.0)).py(px(8.0)).text_color(rgb(p().muted)).child("No family matches"));
            }
            let sample = matches.get(sel).cloned().unwrap_or_else(|| theme::family(picker.role));
            card = card
                .child(list)
                .child(div().h(px(1.0)).bg(rgb(p().rule)))
                .child(
                    div()
                        .px(px(12.0))
                        .py(px(6.0))
                        .font_family(sample)
                        .text_size(px(18.0))
                        .text_color(rgb(p().ink2))
                        .child("Sphinx of black quartz, judge my vow. 0123"),
                )
                .child(hint("type to filter · ↑↓ move · ⏎ keep · esc back"));
        } else {
            card = card.child(heading("SETTINGS".into()));
            for (i, row) in CONFIG_ROWS.iter().copied().enumerate() {
                let picked = i == config.row;
                let (label, covers) = match row {
                    ConfigRow::Theme => ("Theme", None),
                    ConfigRow::Font(role) => (role.label(), Some(role.covers())),
                    ConfigRow::Size => ("Text size", None),
                };
                let chevron = |text: &'static str, dir: i32| {
                    div()
                        .px(px(6.0))
                        .text_color(rgb(p().faint))
                        .child(text)
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                if let Some(c) = &mut this.config {
                                    c.row = i;
                                }
                                this.config_change(row, dir, window);
                                cx.notify();
                            }),
                        )
                };
                let value = match row {
                    ConfigRow::Theme => {
                        let pill = |text: &'static str, dark: bool| {
                            let on = theme::dark() == dark;
                            div()
                                .px(px(12.0))
                                .py(px(4.0))
                                .rounded_full()
                                .text_size(px(13.0))
                                .when(on, |d| d.bg(rgb(p().ink_dark)).text_color(rgb(p().paper)))
                                .when(!on, |d| d.text_color(rgb(p().muted)))
                                .child(text)
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, _, _, cx| {
                                        cx.stop_propagation();
                                        if let Some(c) = &mut this.config {
                                            c.row = i;
                                        }
                                        if theme::dark() != dark {
                                            this.set_dark(dark);
                                        }
                                        cx.notify();
                                    }),
                                )
                        };
                        div()
                            .flex()
                            .p(px(3.0))
                            .rounded_full()
                            .bg(rgb(p().sunken))
                            .child(pill("Light", false))
                            .child(pill("Dark", true))
                            .into_any_element()
                    }
                    ConfigRow::Font(role) => {
                        let family = theme::family(role);
                        div()
                            .flex()
                            .items_center()
                            .child(chevron("‹", -1))
                            .child(div().font_family(family.clone()).text_size(px(16.0)).child(family))
                            .child(chevron("›", 1))
                            .into_any_element()
                    }
                    ConfigRow::Size => div()
                        .flex()
                        .items_center()
                        .font_family(mono())
                        .text_size(px(13.0))
                        .child(chevron("‹", -1))
                        .child(format!("{:.0}%", self.text_scale * 100.0))
                        .child(chevron("›", 1))
                        .into_any_element(),
                };
                card = card.child(
                    div()
                        .flex()
                        .items_center()
                        .h(px(50.0))
                        .px(px(12.0))
                        .rounded(px(10.0))
                        .when(picked, |d| d.bg(rgb(p().sunken)))
                        .child(div().w(px(3.0)).h(px(18.0)).mr(px(10.0)).rounded(px(1.5)).when(picked, |d| d.bg(rgb(p().accent))))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .child(div().text_size(px(13.0)).text_color(rgb(p().muted)).child(label))
                                .when_some(covers, |d, c| d.child(div().text_size(px(11.0)).text_color(rgb(p().faint)).child(c))),
                        )
                        .child(div().flex_1())
                        .child(value)
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                if let Some(c) = &mut this.config {
                                    c.row = i;
                                }
                                if matches!(row, ConfigRow::Font(_)) {
                                    this.config_change(row, 0, window);
                                }
                                cx.notify();
                            }),
                        ),
                );
            }
            card = card.child(div().pt(px(4.0)).child(hint("j/k move · h/l change · ⏎ pick font · r reset · esc close")));
        }

        div()
            .absolute()
            .left(px(0.0))
            .top(px(0.0))
            .size_full()
            .occlude()
            .bg(tint(0x000000, 0x1c))
            .flex()
            .justify_center()
            .items_start()
            .pt(px(90.0))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    cx.stop_propagation();
                    this.close_picker(false);
                    this.config = None;
                    cx.notify();
                }),
            )
            .on_scroll_wheel(cx.listener(|this, e: &ScrollWheelEvent, _, cx| {
                cx.stop_propagation();
                let dy = f32::from(e.delta.pixel_delta(px(34.0)).y);
                let count = this.config.as_ref().and_then(|c| c.picker.as_ref()).map(|p| this.picker_matches(&p.query).len());
                if let (Some(count), Some(p)) = (count, this.config.as_mut().and_then(|c| c.picker.as_mut())) {
                    let steps = (dy.abs() / 34.0).round().max(1.0) as usize;
                    p.sel = if dy < 0.0 { (p.sel + steps).min(count.saturating_sub(1)) } else { p.sel.saturating_sub(steps) };
                    this.picker_preview();
                    cx.notify();
                }
            }))
            .child(card)
            .into_any_element()
    }

    fn render_composer(&mut self, window: &mut Window) -> AnyElement {
        let (rows, line_xs) = editor_layout(window, &self.editor, COL_W - EDITOR_GUTTER);
        self.rows = rows;
        self.line_xs = line_xs;
        let show = self.window_active && self.thread.is_none() && self.comment.is_none() && self.term_focus.is_none();
        let placeholder = if self.nb.blocks.is_empty() { "Think out loud. Draw anywhere." } else { "Reply…" };
        let text = editor_view(&self.editor, &self.rows, &self.line_xs, show, placeholder, "cursor");
        let m = self.measured.clone();
        let text = text.child(probe(move |b| {
            m.borrow_mut().editor_origin = (f32::from(b.origin.x), f32::from(b.origin.y));
        }));
        div()
            .flex()
            .gap(zp(12.0))
            .items_start()
            .child(Self::who("You", p().user).w(zp(EDITOR_GUTTER - 12.0)))
            .child(text)
            .into_any_element()
    }
}

impl Render for Pad {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let size = window.viewport_size();
        self.viewport = (f32::from(size.width), f32::from(size.height));
        // A narrow window shows the whole notebook smaller instead of cropping it,
        // so handwriting stays where it was written relative to the text.
        self.zoom = ((self.viewport.0 - 70.0) / (COL_W + 140.0)).clamp(0.5, 1.0) * self.text_scale;
        ZOOM.with(|z| z.set(self.zoom));
        self.scale = window.scale_factor();
        self.window_active = window.is_window_active();
        if !self.scroll_target.is_some() {
            self.scroll = self.scroll.clamp(0.0, self.max_scroll().max(self.scroll));
        }
        let (w, h) = self.viewport;
        let scroll = self.scroll;
        let z = self.zoom;
        let col_left = w / 2.0 - COL_W * z / 2.0;
        let layout = self.layout();
        let ink_color = theme::ink(COLORS[self.color]);
        {
            let mut m = self.measured.borrow_mut();
            m.hits.clear();
            m.blocks.resize(self.nb.blocks.len() + 1, (0.0, 0.0));
            m.figures.retain(|k, _| *k < self.nb.blocks.len());
        }

        self.ink_groups = self.compute_ink_groups();
        self.anchors = self.compute_anchors();
        self.anchor_index = self.anchors.iter().enumerate().map(|(i, (a, _))| (*a, i)).collect();
        if self.thread.is_some_and(|t| !self.anchor_index.contains_key(&t)) {
            self.thread = None;
        }
        if self.thread.is_none() || self.sel_from.is_some_and(|(a, _)| !self.anchor_index.contains_key(&a)) {
            self.sel_from = None;
        }
        if let Some(t) = self.thread {
            self.caret = self.caret.min(last_char(self.anchor_text(t)));
        }
        if let Some(c) = &self.comment {
            self.comment_rows = editor_layout(window, &c.editor, COL_W - EDITOR_GUTTER - 24.0);
        }
        {
            let mut m = self.measured.borrow_mut();
            m.anchors.clear();
            m.layouts.clear();
            m.comment = None;
        }
        let mut root = div()
            .id("notebook")
            .relative()
            .size_full()
            .bg(rgb(p().paper))
            .font_family(sans())
            .text_color(rgb(p().ink_dark))
            .overflow_hidden()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, e: &KeyDownEvent, window, cx| this.on_key(e, window, cx)))
            .on_scroll_wheel(cx.listener(|this, e: &ScrollWheelEvent, _, cx| {
                let d = e.delta.pixel_delta(px(40.0));
                this.glide_by(-f32::from(d.y) / this.zoom);
                cx.notify();
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, e: &MouseDownEvent, window, cx| {
                    this.focus.focus(window);
                    let (x, y) = (f32::from(e.position.x), f32::from(e.position.y));
                    // The mouse never draws: only the pen and fingers make marks.
                    if let Some(action) = this.hit(x, y) {
                        this.press(action, (x, y), cx);
                    } else if this.click_editor(x, y) {
                        this.thread = None;
                        this.sel_from = None;
                        this.term_focus = None;
                    } else {
                        let doc = this.to_doc(x, y);
                        let hit = this
                            .measured
                            .borrow()
                            .anchors
                            .iter()
                            .find(|(_, r)| r.contains(doc.0, doc.1))
                            .map(|(a, _)| *a);
                        let in_focused_terminal = |a: Anchor| {
                            this.term_focus.is_some_and(|id| matches!(this.nb.blocks.get(a.block), Some(Block::Terminal { id: t, .. }) if *t == id))
                        };
                        match hit {
                            Some(a) if in_focused_terminal(a) => {}
                            Some(a) => {
                                this.term_focus = None;
                                this.editor.mode = Mode::Normal;
                                this.thread = Some(a);
                                this.caret = this.index_at(a, e.position);
                                this.sel_from = Some((a, this.caret));
                                this.sel_lines = false;
                                this.mouse_select = true;
                            }
                            None => {}
                        }
                    }
                    cx.notify();
                }),
            )
            .on_mouse_move(cx.listener(|this, e: &MouseMoveEvent, _, cx| {
                let (x, y) = (f32::from(e.position.x), f32::from(e.position.y));
                this.mouse = Some((x, y));
                if e.dragging() && this.mouse_select {
                    let doc = this.to_doc(x, y);
                    let hit = this.measured.borrow().anchors.iter().find(|(_, r)| r.contains(doc.0, doc.1)).map(|(a, _)| *a);
                    if let (Some(a), Some((from, from_off))) = (hit, this.sel_from) {
                        let i = this.index_at(a, e.position);
                        let after = (this.anchor_index.get(&a), a == from, i) > (this.anchor_index.get(&from), true, from_off);
                        this.thread = Some(a);
                        // The selection takes in the char under the cursor: dragging forward, stop short of the pointer.
                        this.caret = if after && (a != from || i > from_off) { prev_char(this.anchor_text(a), i) } else { i };
                    }
                } else if e.dragging() {
                    this.drag((x, y));
                }
                cx.notify();
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    if std::mem::take(&mut this.mouse_select) && this.sel_from == this.thread.map(|t| (t, this.caret)) {
                        this.sel_from = None; // a click: just the cursor
                    }
                    this.release();
                    this.end();
                    cx.notify();
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.release();
                    this.end();
                    cx.notify();
                }),
            );
        if self.pen_in_range {
            root = root.cursor_none(CursorStyle::None);
        }

        // ---- the document column (scrolls)
        let mut column = div()
            .absolute()
            .left(px(col_left))
            .top(px(-scroll * z))
            .w(zp(COL_W))
            .flex()
            .flex_col()
            .gap(zp(BLOCK_GAP))
            .pt(zp(COLUMN_TOP));
        for (i, block) in self.nb.blocks.iter().enumerate() {
            let m = self.measured.clone();
            column = column.child(
                div()
                    .relative()
                    .child(self.render_block(i, block))
                    .child(probe(move |b| {
                        let mut mm = m.borrow_mut();
                        if let Some(slot) = mm.blocks.get_mut(i) {
                            *slot = (f32::from(b.origin.y) / z + scroll, f32::from(b.size.height) / z);
                        }
                    })),
            );
        }
        let room = self.ink_room();
        if room > 0.0 {
            column = column.child(div().h(zp(room)));
        }
        let composer = self.render_composer(window);
        let n = self.nb.blocks.len();
        let m = self.measured.clone();
        column = column.child(
            div().relative().child(composer).child(probe(move |b| {
                if let Some(slot) = m.borrow_mut().blocks.get_mut(n) {
                    *slot = (f32::from(b.origin.y) / z + scroll, f32::from(b.size.height) / z);
                }
            })),
        );
        root = root.child(column);

        // ---- free ink and labels on the page
        let mut visible: Vec<Painted> = Vec::new();
        for s in &self.nb.ink {
            let (outline, b) = s.shape();
            if b[3] > scroll - 20.0 && b[1] < scroll + h / z + 20.0 {
                visible.push((outline, theme::ink(s.color), s.alpha()));
            }
        }
        if let Some(a) = self.anims.front().filter(|a| matches!(a.target, Target::Page)) {
            let partial = Stroke {
                samples: a.stroke.samples[..a.shown.min(a.stroke.samples.len())].to_vec(),
                ..a.stroke.clone()
            };
            visible.push((Rc::new(partial.outline()), theme::ink(partial.color), partial.alpha()));
        }
        root = root.child(
            canvas(
                |_, _, _| {},
                move |_, _, window, _| {
                    for (outline, color, alpha) in &visible {
                        ui::paint_polygon_at(window, outline, w / 2.0, -scroll * z, z, *color, *alpha);
                    }
                },
            )
            .absolute()
            .size_full(),
        );
        for g in &self.ink_groups {
            let a = Anchor { block: INK_BLOCK, part: 0, item: g.key as usize };
            self.measured.borrow_mut().anchors.insert(a, g.rect);
            let focused = self.thread == Some(a);
            if focused || self.sel_range(a).is_some() {
                let pad = 10.0;
                root = root.child(
                    div()
                        .absolute()
                        .left(px(w / 2.0 + (g.rect.x - pad) * z))
                        .top(px((g.rect.y - pad - scroll) * z))
                        .w(zp(g.rect.w + 2.0 * pad))
                        .h(zp(g.rect.h + 2.0 * pad))
                        .rounded(zp(8.0))
                        .bg(tint(p().accent, 0x0f))
                        .border_1()
                        .border_color(if focused { rgb(p().accent).into() } else { tint(p().accent, 0x55) }),
                );
            }
        }
        if let Some(c) = self.comment.as_ref().filter(|c| c.at.block == INK_BLOCK) {
            if let Some(g) = self.ink_groups.iter().find(|g| g.key as usize == c.at.item) {
                root = root.child(
                    div().absolute().left(px(col_left)).top(px((g.rect.y + g.rect.h + 18.0 - scroll) * z)).w(zp(COL_W)).child(self.comment_box(c)),
                );
            }
        }
        for note in self.nb.notes.iter().filter(|n| n.y > scroll - 200.0 && n.y < scroll + h / z) {
            root = root.child(note_el(note, w / 2.0 + note.x * z, (note.y - scroll) * z));
        }

        // ---- an agent asking before a consequential browser action
        if let Some((_, what)) = &self.confirm {
            let button = |label: &'static str, action: Action, dark: bool| {
                let m = self.measured.clone();
                div()
                    .relative()
                    .px(px(14.0))
                    .py(px(6.0))
                    .rounded(px(6.0))
                    .text_size(px(13.0))
                    .when(dark, |d| d.bg(rgb(p().ink)).text_color(rgb(p().paper)))
                    .when(!dark, |d| d.border_1().border_color(rgb(p().rule_strong)).text_color(rgb(p().ink2)))
                    .child(label)
                    .child(probe(move |b| m.borrow_mut().hits.push((screen_rect(b), action))))
            };
            root = root.child(
                div()
                    .absolute()
                    .left(px((w / 2.0 - 280.0).max(16.0)))
                    .bottom(px(STATUS_H + 18.0))
                    .w(px(560.0_f32.min(w - 32.0)))
                    .p(px(14.0))
                    .rounded(px(10.0))
                    .bg(rgb(p().surface))
                    .border_1()
                    .border_color(rgb(p().accent))
                    .shadow_lg()
                    .flex()
                    .flex_col()
                    .gap(px(10.0))
                    .child(div().text_size(px(11.5)).text_color(rgb(p().muted)).child("THE AGENT WANTS TO, IN YOUR BROWSER"))
                    .child(div().text_size(px(15.0)).line_height(px(22.0)).text_color(rgb(p().ink)).child(what.clone()))
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap_2()
                            .child(button("Deny  n", Action::Deny, false))
                            .child(button("Allow  y", Action::Allow, true)),
                    ),
            );
        }

        // ---- checkpoint rail
        let rail = self.rail();
        if !rail.is_empty() {
            let current = self.current_block();
            let hover = self
                .mouse
                .or(self.pen_hover)
                .filter(|(x, _)| *x > w - 46.0)
                .and_then(|(x, y)| self.rail_hit(x, y));
            let mut tip = None;
            for (y, block, agent, title, summary) in &rail {
                let is_current = Some(*block) == current;
                let is_hover = hover == Some(Action::Jump(*block));
                let (wdt, color) = match (agent, is_current || is_hover) {
                    (true, true) => (22.0, tint(p().terra, 0xff)),
                    (true, false) => (16.0, tint(p().terra, 0x99)),
                    (false, true) => (18.0, tint(p().ink_dark, 0xcc)),
                    (false, false) => (12.0, tint(p().ink_dark, 0x33)),
                };
                root = root.child(
                    div()
                        .absolute()
                        .right(px(14.0))
                        .top(px(y - 1.0))
                        .w(px(wdt))
                        .h(px(if *agent { 3.0 } else { 2.0 }))
                        .rounded(px(1.5))
                        .bg(color),
                );
                if is_hover && tip.is_none() {
                    tip = Some((*y, *agent, title.clone(), summary.clone()));
                }
            }
            if let Some((y, agent, title, summary)) = tip {
                let mut card = div()
                    .absolute()
                    .right(px(48.0))
                    .top(px((y - 22.0).max(8.0)))
                    .w(px(300.0))
                    .p(px(12.0))
                    .rounded(px(12.0))
                    .bg(rgb(p().card))
                    .border_1()
                    .border_color(rgb(p().card_border))
                    .shadow_lg()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_size(px(10.5))
                            .text_color(rgb(if agent { p().terra } else { p().muted }))
                            .child(if agent { "CHECKPOINT" } else { "YOU ASKED" }),
                    )
                    .child(div().text_size(px(14.0)).line_height(px(20.0)).child(title));
                if !summary.is_empty() {
                    card = card.child(div().text_size(px(13.0)).text_color(rgb(p().card_text)).child(summary));
                }
                root = root.child(card);
            }
        }

        // ---- status bar: vim mode and command line on the left, the agent on the right
        {
            let mode = self.editor.mode;
            let left = if mode == Mode::Command {
                div().text_color(rgb(p().ink)).text_size(px(12.5)).child(format!(":{}▏", self.editor.command))
            } else {
                div().text_color(rgb(p().muted)).child(self.editor.pending_keys())
            };
            let (label, dot) = match (&self.agent_down, &self.model, self.agent_busy) {
                (Some(_), _, _) => ("offline", p().error),
                (None, None, _) => ("starting", p().faint),
                (None, Some(_), true) => ("working", p().accent),
                (None, Some(_), false) if self.jev_pending.is_some() => ("reading your handwriting", p().muted),
                (None, Some(_), false) => ("ready", p().ok),
            };
            let hint = if self.agent_busy { "ctrl⏎ queues" } else { "ctrl⏎ or :w sends" };
            let dot_el = div().size(px(6.0)).rounded_full().bg(rgb(dot));
            let dot_el = if self.agent_busy {
                dot_el
                    .with_animation(
                        "busy-dot",
                        Animation::new(Duration::from_millis(1000)).repeat().with_easing(pulsating_between(0.25, 1.0)),
                        |el, t| el.opacity(t),
                    )
                    .into_any_element()
            } else {
                dot_el.into_any_element()
            };
            root = root.child(
                div()
                    .absolute()
                    .left(px(0.0))
                    .bottom(px(0.0))
                    .w(px(w))
                    .h(px(STATUS_H))
                    .px(px(14.0))
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .bg(rgb(p().paper))
                    .border_t_1()
                    .border_color(rgb(p().rule))
                    .font_family(mono())
                    .text_size(px(11.0))
                    .child(left)
                    .child(div().flex_1())
                    .child(div().text_color(rgb(p().faint)).child(hint))
                    .child(div().w(px(1.0)).h(px(12.0)).bg(rgb(p().rule)))
                    .child(dot_el)
                    .child(div().text_color(rgb(p().muted)).child(label)),
            );
        }

        // ---- the tray (hidden until the floating button opens it)
        if let Some(tray) = layout.tray {
            let mut tray_el = abs(tray)
                .bg(rgb(p().tray))
                .rounded(px(26.0))
                .border_1()
                .border_color(rgb(p().card_border))
                .shadow_lg()
                .overflow_hidden();
            for (rect, action) in &layout.buttons {
                let r = rect.relative_to(tray);
                match *action {
                    Action::Tool(tool) => {
                        let raised = tool == self.effective_tool();
                        let lift = if raised { 4.0 } else { 22.0 };
                        let dock = layout.dock;
                        let len = 110.0;
                        let (left, top, cw, ch) = match dock {
                            Dock::Bottom => (0.0, lift, r.w, len),
                            Dock::Top => (0.0, r.h - lift - len, r.w, len),
                            Dock::Left => (r.w - lift - len, 0.0, len, r.h),
                            Dock::Right => (lift, 0.0, len, r.h),
                        };
                        tray_el = tray_el.child(
                            abs(r).child(
                                canvas(
                                    |_, _, _| {},
                                    move |bounds, _, window, _| ui::paint_tool(window, bounds, tool, ink_color, dock),
                                )
                                .absolute()
                                .left(px(left))
                                .top(px(top))
                                .w(px(cw))
                                .h(px(ch)),
                            ),
                        );
                    }
                    Action::Color(i) => {
                        let selected = i == self.color && self.tool != Tool::Eraser;
                        let d = if selected { 36.0 } else { 28.0 };
                        let mut swatch = div().size(px(d)).rounded_full().bg(rgb(theme::ink(COLORS[i])));
                        if selected {
                            swatch = swatch.border_3().border_color(rgb(0xffffff)).shadow_md();
                        }
                        tray_el = tray_el.child(abs(r).flex().items_center().justify_center().child(swatch));
                    }
                    Action::Undo | Action::Redo => {
                        let enabled = if *action == Action::Undo { self.history.can_undo() } else { self.history.can_redo() };
                        let mirror = *action == Action::Redo;
                        tray_el = tray_el.child(abs(r).child(
                            canvas(
                                |_, _, _| {},
                                move |bounds, _, window, _| ui::paint_arrow(window, bounds, mirror, if enabled { 1.0 } else { 0.3 }),
                            )
                            .size_full(),
                        ));
                    }
                    Action::Compose => {
                        tray_el = tray_el.child(abs(r).child(
                            canvas(|_, _, _| {}, |bounds, _, window, _| ui::paint_keyboard(window, bounds)).size_full(),
                        ));
                    }
                    Action::Pi => {
                        let bg = if self.listen { p().pi_bg_listen } else { p().pi_bg };
                        let ready = self.model.is_some();
                        let mascot = div().size(px(52.0)).child(
                            canvas(
                                |_, _, _| {},
                                move |b, _, window, _| {
                                    let c = b.center();
                                    ui::paint_clawd(window, f32::from(c.x), f32::from(c.y), 3.4, if ready { 1.0 } else { 0.45 })
                                },
                            )
                            .size_full(),
                        );
                        let mascot = if self.agent_busy {
                            mascot
                                .with_animation(
                                    "clawd-busy",
                                    Animation::new(Duration::from_millis(700)).repeat().with_easing(pulsating_between(0.0, 1.0)),
                                    |el, t| el.mt(px(-5.0 * t)),
                                )
                                .into_any_element()
                        } else {
                            mascot.into_any_element()
                        };
                        let mut disc = div()
                            .size(px(58.0))
                            .rounded_full()
                            .bg(rgb(bg))
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(mascot);
                        if self.listen {
                            disc = disc.border_2().border_color(rgb(ui::CLAWD_ORANGE));
                        }
                        tray_el = tray_el.child(abs(r).flex().items_center().justify_center().child(disc));
                    }
                    _ => {}
                }
            }
            for d in &layout.dividers {
                tray_el = tray_el.child(abs(d.relative_to(tray)).bg(rgb(p().divider)));
            }
            root = root.child(tray_el);
        }

        // ---- the floating button
        {
            let fab = layout.fab;
            let open = self.tray_open;
            let dragging = self.fab_press.as_ref().is_some_and(|p| p.moved)
                || self.gesture.as_ref().is_some_and(|g| g.fab.is_some() && g.moved > 8.0);
            root = root.child(
                abs(fab)
                    .rounded_full()
                    .bg(rgb(p().ink_dark))
                    .shadow_lg()
                    .when(dragging, |d| d.opacity(0.85))
                    .child(
                        canvas(
                            |_, _, _| {},
                            move |b, _, window, _| {
                                let c = b.center();
                                let (cx_, cy) = (f32::from(c.x), f32::from(c.y));
                                if open {
                                    for s in [-1.0f32, 1.0] {
                                        let pts = [
                                            (cx_ - 8.0, cy - 8.0 * s - 1.2),
                                            (cx_ - 8.0, cy - 8.0 * s + 1.2),
                                            (cx_ + 8.0, cy + 8.0 * s + 1.2),
                                            (cx_ + 8.0, cy + 8.0 * s - 1.2),
                                        ];
                                        ui::paint_polygon(window, &pts, p().paper, 1.0);
                                    }
                                } else {
                                    // A pencil, tipped with the current ink.
                                    let (dx, dy) = (0.7071f32, -0.7071f32);
                                    let (nx, ny) = (-dy, dx);
                                    let tip = (cx_ - 10.0 * dx, cy - 10.0 * dy);
                                    let at = |t: f32, s: f32| (tip.0 + dx * t + nx * s, tip.1 + dy * t + ny * s);
                                    ui::paint_polygon(window, &[at(7.0, -3.4), at(22.0, -3.4), at(22.0, 3.4), at(7.0, 3.4)], 0xf2c14e, 1.0);
                                    ui::paint_polygon(window, &[at(0.0, 0.0), at(7.0, -3.4), at(7.0, 3.4)], 0xeed6ad, 1.0);
                                    ui::paint_polygon(window, &[at(0.0, 0.0), at(3.0, -1.5), at(3.0, 1.5)], ink_color, 1.0);
                                    ui::paint_polygon(window, &[at(22.0, -3.4), at(25.0, -3.4), at(25.0, 3.4), at(22.0, 3.4)], 0xef9d9d, 1.0);
                                }
                            },
                        )
                        .size_full(),
                    ),
            );
        }

        // ---- toast
        if let Some((text, _)) = &self.toast {
            root = root.child(
                div()
                    .absolute()
                    .top(px(22.0))
                    .left(px(0.0))
                    .w(px(w))
                    .flex()
                    .justify_center()
                    .child(
                        div()
                            .px_4()
                            .py_2()
                            .rounded_full()
                            .bg(tint(p().ink_dark, 0xe6))
                            .text_size(px(14.0))
                            .text_color(rgb(p().paper))
                            .child(text.clone()),
                    ),
            );
        }

        // ---- the `:config` menu
        if self.config.is_some() {
            root = root.child(self.render_config(cx));
        }

        // ---- a nib-sized cursor that follows the hovering pen
        if let Some((x, y)) = self.pen_hover.filter(|_| self.window_active) {
            let tool = self.effective_tool();
            root = root.child(
                canvas(
                    |_, _, _| {},
                    move |_, _, window, _| match tool {
                        Tool::Eraser => ui::ring(window, x, y, ERASER_R, p().muted, 0.9),
                        Tool::Marker => ui::dot(window, x, y, 6.0, ink_color, 0.35),
                        _ => ui::dot(window, x, y, 1.8, ink_color, 0.9),
                    },
                )
                .absolute()
                .size_full(),
            );
        }

        root
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_motions() {
        let t = "ls -la ~/code";
        assert_eq!(word_fwd(t, 0), Some(3)); // ls -> -
        assert_eq!(word_fwd(t, 3), Some(4)); // - -> la
        assert_eq!(word_fwd(t, 4), Some(7)); // la -> ~
        assert_eq!(word_back(t, 7), Some(4));
        assert_eq!(word_back(t, 0), None);
        assert_eq!(word_end(t, 0), 1);
        assert_eq!(word_end(t, 1), 3);
        assert_eq!(word_fwd("end", 0), None);
        assert_eq!(last_char("héllo"), 5);
        assert_eq!(next_char("héllo", 1), 3);
        assert_eq!(prev_char("héllo", 3), 1);
    }

    #[test]
    fn marks_split_runs() {
        let run = plain_run(10, font(mono()), p().ink);
        let out = mark_runs(vec![run], Some(2..6), Some(5..6));
        assert_eq!(out.iter().map(|r| r.len).collect::<Vec<_>>(), vec![2, 3, 1, 4]);
        assert!(out[1].background_color.is_some() && out[0].background_color.is_none());
        assert_eq!(out[2].color, hsla(p().paper));
    }
}
