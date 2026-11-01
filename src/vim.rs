//! A small, honest vim: normal / insert / visual / visual-line / command-line
//! modes, counts, the common motions and operators, registers, undo/redo.
//! Pure logic — the notebook renders it.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Insert,
    Visual,
    VisualLine,
    Command,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Mode::Normal => "NORMAL",
            Mode::Insert => "INSERT",
            Mode::Visual => "VISUAL",
            Mode::VisualLine => "V-LINE",
            Mode::Command => "COMMAND",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Key {
    /// GPUI key name: "a", "enter", "escape", "backspace", "left", …
    pub key: String,
    /// The text the key types, if any (shift applied).
    pub ch: Option<String>,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    None,
    Send,
    Quit,
    /// A `:command` the editor itself doesn't know.
    Command(String),
}

#[derive(Clone)]
struct Snapshot {
    lines: Vec<Vec<char>>,
    row: usize,
    col: usize,
}

pub struct Editor {
    pub lines: Vec<Vec<char>>,
    pub row: usize,
    pub col: usize,
    pub mode: Mode,
    /// Visual-mode anchor.
    pub anchor: (usize, usize),
    /// The `:` command line being typed.
    pub command: String,
    count: String,
    pending: Option<char>,
    pending_count: usize,
    pending_g: bool,
    count_given: bool,
    register: (String, bool),
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    want_col: usize,
}

impl Default for Editor {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    Space,
    Word,
    Punct,
}

fn class(c: char) -> Class {
    if c.is_whitespace() {
        Class::Space
    } else if c.is_alphanumeric() || c == '_' {
        Class::Word
    } else {
        Class::Punct
    }
}

impl Editor {
    pub fn new() -> Self {
        Self {
            lines: vec![Vec::new()],
            row: 0,
            col: 0,
            mode: Mode::Insert,
            anchor: (0, 0),
            command: String::new(),
            count: String::new(),
            pending: None,
            pending_count: 1,
            pending_g: false,
            count_given: false,
            register: (String::new(), false),
            undo: Vec::new(),
            redo: Vec::new(),
            want_col: 0,
        }
    }

    pub fn text(&self) -> String {
        self.lines
            .iter()
            .map(|l| l.iter().collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn set_text(&mut self, text: &str) {
        self.lines = text.split('\n').map(|l| l.chars().collect()).collect();
        if self.lines.is_empty() {
            self.lines.push(Vec::new());
        }
        self.row = 0;
        self.col = 0;
    }

    pub fn is_empty(&self) -> bool {
        self.lines
            .iter()
            .all(|l| l.iter().all(|c| c.is_whitespace()))
    }

    pub fn clear(&mut self) {
        self.snapshot();
        self.lines = vec![Vec::new()];
        self.row = 0;
        self.col = 0;
        self.mode = Mode::Insert;
    }

    /// Selection as ((row, col), (row, col)) inclusive, ordered.
    pub fn selection(&self) -> Option<((usize, usize), (usize, usize))> {
        match self.mode {
            Mode::Visual => {
                let a = self.anchor;
                let b = (self.row, self.col);
                Some(if a <= b { (a, b) } else { (b, a) })
            }
            Mode::VisualLine => {
                let (r0, r1) = (self.anchor.0.min(self.row), self.anchor.0.max(self.row));
                Some(((r0, 0), (r1, self.lines[r1].len().max(1) - 1)))
            }
            _ => None,
        }
    }

    pub fn pending_keys(&self) -> String {
        let mut s = self.count.clone();
        if let Some(p) = self.pending {
            s.push(p);
        }
        s
    }

    fn snapshot(&mut self) {
        self.undo.push(Snapshot {
            lines: self.lines.clone(),
            row: self.row,
            col: self.col,
        });
        if self.undo.len() > 500 {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    fn line_len(&self, row: usize) -> usize {
        self.lines[row].len()
    }

    /// Clamp the cursor for the current mode (normal mode sits on a char).
    fn clamp(&mut self) {
        self.row = self.row.min(self.lines.len() - 1);
        let len = self.line_len(self.row);
        let max = if self.mode == Mode::Insert {
            len
        } else {
            len.saturating_sub(1)
        };
        self.col = self.col.min(max);
    }

    fn take_count(&mut self) -> usize {
        let n = self.count.parse::<usize>().unwrap_or(1).max(1);
        self.count.clear();
        n
    }

    // ------------------------------------------------------------- motions

    fn char_at(&self, row: usize, col: usize) -> Option<char> {
        self.lines.get(row).and_then(|l| l.get(col)).copied()
    }

    /// Next position in reading order (crossing lines as a virtual newline).
    fn step_fwd(&self, (r, c): (usize, usize)) -> Option<(usize, usize)> {
        if c < self.line_len(r) {
            Some((r, c + 1))
        } else if r + 1 < self.lines.len() {
            Some((r + 1, 0))
        } else {
            None
        }
    }

    fn step_back(&self, (r, c): (usize, usize)) -> Option<(usize, usize)> {
        if c > 0 {
            Some((r, c - 1))
        } else if r > 0 {
            Some((r - 1, self.line_len(r - 1)))
        } else {
            None
        }
    }

    fn class_at(&self, p: (usize, usize)) -> Class {
        self.char_at(p.0, p.1).map_or(Class::Space, class)
    }

    fn word_fwd(&self, mut p: (usize, usize)) -> (usize, usize) {
        let start = self.class_at(p);
        // Leave the current word.
        if start != Class::Space {
            while let Some(n) = self.step_fwd(p) {
                p = n;
                if self.class_at(p) != start || p.1 == 0 {
                    break;
                }
            }
        }
        // Skip spaces (an empty line counts as a word).
        while self.class_at(p) == Class::Space {
            if self.line_len(p.0) == 0 && p != (self.row, self.col) {
                break;
            }
            match self.step_fwd(p) {
                Some(n) => p = n,
                None => break,
            }
        }
        p
    }

    fn word_back(&self, mut p: (usize, usize)) -> (usize, usize) {
        while let Some(n) = self.step_back(p) {
            p = n;
            if self.class_at(p) != Class::Space {
                break;
            }
        }
        let cls = self.class_at(p);
        while let Some(n) = self.step_back(p) {
            if self.class_at(n) != cls || n.0 != p.0 {
                break;
            }
            p = n;
        }
        p
    }

    fn word_end(&self, mut p: (usize, usize)) -> (usize, usize) {
        if let Some(n) = self.step_fwd(p) {
            p = n;
        }
        while self.class_at(p) == Class::Space {
            match self.step_fwd(p) {
                Some(n) => p = n,
                None => return p,
            }
        }
        let cls = self.class_at(p);
        while let Some(n) = self.step_fwd(p) {
            if self.class_at(n) != cls || n.0 != p.0 {
                break;
            }
            p = n;
        }
        p
    }

    fn first_non_blank(&self, row: usize) -> usize {
        self.lines[row]
            .iter()
            .position(|c| !c.is_whitespace())
            .unwrap_or(0)
    }

    /// Resolve a motion key. Returns target, linewise, inclusive.
    fn motion(&mut self, k: &str, n: usize) -> Option<((usize, usize), bool, bool)> {
        let (r, c) = (self.row, self.col);
        let last = self.lines.len() - 1;
        Some(match k {
            "h" | "left" => ((r, c.saturating_sub(n)), false, false),
            "l" | "right" | " " => ((r, (c + n).min(self.line_len(r))), false, false),
            "j" | "down" => (((r + n).min(last), c), true, true),
            "k" | "up" => ((r.saturating_sub(n), c), true, true),
            "0" | "home" => ((r, 0), false, false),
            "^" => ((r, self.first_non_blank(r)), false, false),
            "$" | "end" => (
                (
                    (r + n - 1).min(last),
                    self.line_len((r + n - 1).min(last)).saturating_sub(1),
                ),
                false,
                true,
            ),
            "w" => {
                let mut p = (r, c);
                for _ in 0..n {
                    p = self.word_fwd(p);
                }
                (p, false, false)
            }
            "b" => {
                let mut p = (r, c);
                for _ in 0..n {
                    p = self.word_back(p);
                }
                (p, false, false)
            }
            "e" => {
                let mut p = (r, c);
                for _ in 0..n {
                    p = self.word_end(p);
                }
                (p, false, true)
            }
            "G" => {
                let row = if self.count_given {
                    n.saturating_sub(1).min(last)
                } else {
                    last
                };
                ((row, 0), true, true)
            }
            "gg" => ((n.saturating_sub(1).min(last), 0), true, true),
            _ => return None,
        })
        .map(|(target, linewise, inclusive)| {
            let target: (usize, usize) = target;
            (target, linewise, inclusive)
        })
    }

    // ------------------------------------------------------------- edits

    fn range_text(&self, a: (usize, usize), b: (usize, usize)) -> String {
        // [a, b) in reading order.
        let mut out = String::new();
        let mut p = a;
        while p < b {
            match self.char_at(p.0, p.1) {
                Some(ch) => out.push(ch),
                None => out.push('\n'),
            }
            match self.step_fwd(p) {
                Some(n) => p = n,
                None => break,
            }
        }
        out
    }

    fn delete_range(&mut self, a: (usize, usize), b: (usize, usize)) {
        // [a, b) in reading order.
        if a >= b {
            return;
        }
        let tail: Vec<char> = self.lines[b.0][b.1.min(self.line_len(b.0))..].to_vec();
        self.lines[a.0].truncate(a.1);
        self.lines[a.0].extend(tail);
        if b.0 > a.0 {
            self.lines.drain(a.0 + 1..=b.0);
        }
        self.row = a.0;
        self.col = a.1;
    }

    fn delete_lines(&mut self, r0: usize, r1: usize) -> String {
        let text: Vec<String> = self.lines[r0..=r1]
            .iter()
            .map(|l| l.iter().collect())
            .collect();
        self.lines.drain(r0..=r1);
        if self.lines.is_empty() {
            self.lines.push(Vec::new());
        }
        self.row = r0.min(self.lines.len() - 1);
        self.col = self.first_non_blank(self.row);
        text.join("\n")
    }

    fn insert_text(&mut self, text: &str) {
        for ch in text.chars() {
            if ch == '\n' {
                let rest = self.lines[self.row].split_off(self.col);
                self.lines.insert(self.row + 1, rest);
                self.row += 1;
                self.col = 0;
            } else {
                self.lines[self.row].insert(self.col, ch);
                self.col += 1;
            }
        }
    }

    fn put(&mut self, before: bool) {
        let (text, linewise) = self.register.clone();
        if text.is_empty() {
            return;
        }
        self.snapshot();
        if linewise {
            let at = if before { self.row } else { self.row + 1 };
            for (i, line) in text.split('\n').enumerate() {
                self.lines.insert(at + i, line.chars().collect());
            }
            self.row = at;
            self.col = self.first_non_blank(at);
        } else {
            if !before && self.line_len(self.row) > 0 {
                self.col += 1;
            }
            self.insert_text(&text);
            self.col = self.col.saturating_sub(1);
        }
    }

    fn indent(&mut self, r0: usize, r1: usize, out: bool) {
        self.snapshot();
        for r in r0..=r1.min(self.lines.len() - 1) {
            if out {
                let n = self.lines[r]
                    .iter()
                    .take(4)
                    .take_while(|c| **c == ' ')
                    .count();
                self.lines[r].drain(..n);
            } else if !self.lines[r].is_empty() {
                for _ in 0..4 {
                    self.lines[r].insert(0, ' ');
                }
            }
        }
        self.row = r0;
        self.col = self.first_non_blank(r0);
    }

    /// Apply operator `op` over a motion result.
    fn operate(&mut self, op: char, target: (usize, usize), linewise: bool, inclusive: bool) {
        let here = (self.row, self.col);
        if linewise {
            let (r0, r1) = (here.0.min(target.0), here.0.max(target.0));
            match op {
                'y' => {
                    let text: Vec<String> = self.lines[r0..=r1]
                        .iter()
                        .map(|l| l.iter().collect())
                        .collect();
                    self.register = (text.join("\n"), true);
                }
                'd' => {
                    self.snapshot();
                    let text = self.delete_lines(r0, r1);
                    self.register = (text, true);
                }
                'c' => {
                    self.snapshot();
                    let text: Vec<String> = self.lines[r0..=r1]
                        .iter()
                        .map(|l| l.iter().collect())
                        .collect();
                    self.register = (text.join("\n"), true);
                    let indent: Vec<char> = self.lines[r0]
                        .iter()
                        .take_while(|c| c.is_whitespace())
                        .copied()
                        .collect();
                    self.lines.drain(r0..=r1);
                    self.lines.insert(r0, indent.clone());
                    self.row = r0;
                    self.col = indent.len();
                    self.mode = Mode::Insert;
                }
                '>' | '<' => self.indent(r0, r1, op == '<'),
                _ => {}
            }
            return;
        }
        let (mut a, mut b) = if here <= target {
            (here, target)
        } else {
            (target, here)
        };
        if inclusive {
            b = self.step_fwd(b).unwrap_or((b.0, self.line_len(b.0)));
        }
        // `cw` on a word behaves like `ce`; `dw` at line end stops at the line.
        if b.0 > a.0 && b.1 == 0 && op != 'y' && !inclusive {
            b = (b.0 - 1, self.line_len(b.0 - 1));
            if a > b {
                a = b;
            }
        }
        let text = self.range_text(a, b);
        match op {
            'y' => {
                self.register = (text, false);
                self.row = a.0;
                self.col = a.1;
            }
            'd' | 'c' => {
                self.snapshot();
                self.register = (text, false);
                self.delete_range(a, b);
                if op == 'c' {
                    self.mode = Mode::Insert;
                }
            }
            _ => {}
        }
    }

    // ------------------------------------------------------------- input

    pub fn handle(&mut self, key: &Key) -> Outcome {
        if key.ctrl && key.key == "enter" {
            return Outcome::Send;
        }
        let outcome = match self.mode {
            Mode::Insert => self.insert_key(key),
            Mode::Command => self.command_key(key),
            Mode::Normal | Mode::Visual | Mode::VisualLine => self.normal_key(key),
        };
        self.clamp();
        outcome
    }

    fn insert_key(&mut self, key: &Key) -> Outcome {
        let k = key.key.as_str();
        if k == "escape" || (key.ctrl && k == "[") || (key.ctrl && k == "c") {
            self.mode = Mode::Normal;
            self.col = self.col.saturating_sub(1);
            return Outcome::None;
        }
        if key.ctrl {
            match k {
                "w" => {
                    let end = (self.row, self.col);
                    let start = if self.col == 0 {
                        end
                    } else {
                        self.word_back(end)
                    };
                    self.delete_range((self.row, start.1.min(self.col)), end);
                }
                "u" => {
                    let end = (self.row, self.col);
                    self.delete_range((self.row, 0), end);
                }
                "h" => self.backspace(),
                _ => {}
            }
            return Outcome::None;
        }
        match k {
            "enter" => {
                let indent: String = self.lines[self.row]
                    .iter()
                    .take_while(|c| **c == ' ')
                    .collect();
                self.insert_text("\n");
                self.insert_text(&indent);
            }
            "backspace" => self.backspace(),
            "delete" => {
                if self.col < self.line_len(self.row) {
                    self.lines[self.row].remove(self.col);
                } else if self.row + 1 < self.lines.len() {
                    let next = self.lines.remove(self.row + 1);
                    self.lines[self.row].extend(next);
                }
            }
            "tab" => self.insert_text("    "),
            "left" => self.col = self.col.saturating_sub(1),
            "right" => self.col += 1,
            "up" => self.row = self.row.saturating_sub(1),
            "down" => self.row = (self.row + 1).min(self.lines.len() - 1),
            "home" => self.col = 0,
            "end" => self.col = self.line_len(self.row),
            _ => {
                if let Some(ch) = &key.ch {
                    if !ch.chars().any(char::is_control) {
                        self.insert_text(ch);
                    }
                }
            }
        }
        Outcome::None
    }

    fn backspace(&mut self) {
        if self.col > 0 {
            self.col -= 1;
            self.lines[self.row].remove(self.col);
        } else if self.row > 0 {
            let line = self.lines.remove(self.row);
            self.row -= 1;
            self.col = self.line_len(self.row);
            self.lines[self.row].extend(line);
        }
    }

    fn command_key(&mut self, key: &Key) -> Outcome {
        match key.key.as_str() {
            "escape" => {
                self.command.clear();
                self.mode = Mode::Normal;
            }
            "backspace" => {
                if self.command.pop().is_none() {
                    self.mode = Mode::Normal;
                }
            }
            "enter" => {
                let cmd = std::mem::take(&mut self.command);
                self.mode = Mode::Normal;
                return match cmd.trim() {
                    "w" | "send" | "wq" | "x" => Outcome::Send,
                    "q" | "q!" | "qa" => Outcome::Quit,
                    "clear" | "%d" => {
                        self.clear();
                        self.mode = Mode::Normal;
                        Outcome::None
                    }
                    other => Outcome::Command(other.to_owned()),
                };
            }
            _ => {
                if let Some(ch) = &key.ch {
                    self.command.push_str(ch);
                }
            }
        }
        Outcome::None
    }

    fn normal_key(&mut self, key: &Key) -> Outcome {
        let name = key.key.as_str();
        let visual = matches!(self.mode, Mode::Visual | Mode::VisualLine);

        if name == "escape" || (key.ctrl && name == "[") {
            self.pending = None;
            self.count.clear();
            self.mode = Mode::Normal;
            return Outcome::None;
        }
        if key.ctrl {
            match name {
                "r" => {
                    if let Some(s) = self.redo.pop() {
                        self.undo.push(Snapshot {
                            lines: self.lines.clone(),
                            row: self.row,
                            col: self.col,
                        });
                        self.lines = s.lines;
                        self.row = s.row;
                        self.col = s.col;
                    }
                }
                _ => {}
            }
            return Outcome::None;
        }

        // The typed character, or the named key for arrows etc.
        let k: String = match &key.ch {
            Some(ch) if !ch.is_empty() && !ch.chars().any(char::is_control) => ch.clone(),
            _ => name.to_owned(),
        };

        // `r<char>`: replace.
        if self.pending == Some('r') {
            self.pending = None;
            if let Some(c) = k.chars().next().filter(|_| k.chars().count() == 1) {
                let n = self.take_count();
                if self.col + n <= self.line_len(self.row) {
                    self.snapshot();
                    for i in 0..n {
                        self.lines[self.row][self.col + i] = c;
                    }
                    self.col += n - 1;
                }
            }
            return Outcome::None;
        }

        // Counts.
        if k.len() == 1
            && k.chars().all(|c| c.is_ascii_digit())
            && !(k == "0" && self.count.is_empty())
        {
            self.count.push_str(&k);
            return Outcome::None;
        }

        // `g` prefix.
        let motion_key = if self.pending == Some('g') || (self.pending_g) {
            self.pending_g = false;
            if self.pending == Some('g') {
                self.pending = None;
            }
            if k == "g" {
                "gg".to_owned()
            } else {
                return Outcome::None;
            }
        } else if k == "g" {
            if matches!(self.pending, Some('d' | 'c' | 'y' | '>' | '<')) {
                self.pending_g = true;
            } else {
                self.pending = Some('g');
            }
            return Outcome::None;
        } else {
            k.clone()
        };

        self.count_given = !self.count.is_empty();
        let n = self.take_count();

        // Operator pending: wait for a motion or the doubled operator.
        if let Some(op) = self
            .pending
            .filter(|p| matches!(p, 'd' | 'c' | 'y' | '>' | '<'))
        {
            self.pending = None;
            let n = n * self.pending_count;
            if motion_key.len() == 1 && motion_key.starts_with(op) {
                let target = ((self.row + n - 1).min(self.lines.len() - 1), 0);
                self.operate(op, target, true, true);
            } else if let Some((target, linewise, inclusive)) = self.motion(&motion_key, n) {
                // `cw` changes to the end of the word.
                let (target, inclusive) = if op == 'c'
                    && motion_key == "w"
                    && self.class_at((self.row, self.col)) != Class::Space
                {
                    let mut p = (self.row, self.col);
                    for i in 0..n {
                        if i > 0
                            || (self.step_fwd(p).is_some_and(|q| q.0 == p.0)
                                && self.class_at(self.step_fwd(p).unwrap()) == self.class_at(p))
                        {
                            p = self.word_end(p);
                        }
                    }
                    (p, true)
                } else {
                    (target, inclusive)
                };
                self.operate(op, target, linewise, inclusive);
            }
            return Outcome::None;
        }

        // Plain motion.
        if let Some((target, linewise, _)) = self.motion(&motion_key, n) {
            if linewise && !matches!(motion_key.as_str(), "G" | "gg") {
                self.row = target.0;
                self.col = self.want_col;
                return Outcome::None;
            }
            self.row = target.0;
            self.col = target.1;
            if motion_key == "G" || motion_key == "gg" {
                self.col = self.first_non_blank(self.row);
            }
            self.want_col = self.col;
            return Outcome::None;
        }

        if visual {
            return self.visual_command(&motion_key);
        }

        match motion_key.as_str() {
            "i" => {
                self.snapshot();
                self.mode = Mode::Insert;
            }
            "a" => {
                self.snapshot();
                self.mode = Mode::Insert;
                self.col = (self.col + 1).min(self.line_len(self.row));
            }
            "I" => {
                self.snapshot();
                self.mode = Mode::Insert;
                self.col = self.first_non_blank(self.row);
            }
            "A" => {
                self.snapshot();
                self.mode = Mode::Insert;
                self.col = self.line_len(self.row);
            }
            "o" | "O" => {
                self.snapshot();
                let indent: Vec<char> = self.lines[self.row]
                    .iter()
                    .take_while(|c| **c == ' ')
                    .copied()
                    .collect();
                let at = if motion_key == "o" {
                    self.row + 1
                } else {
                    self.row
                };
                self.lines.insert(at, indent.clone());
                self.row = at;
                self.col = indent.len();
                self.mode = Mode::Insert;
            }
            "x" | "delete" => {
                if self.line_len(self.row) > 0 {
                    let end = (self.col + n).min(self.line_len(self.row));
                    self.operate('d', (self.row, end), false, false);
                }
            }
            "X" => {
                if self.col > 0 {
                    let start = self.col.saturating_sub(n);
                    self.operate('d', (self.row, start), false, false);
                }
            }
            "D" => {
                let end = self.line_len(self.row);
                self.operate('d', (self.row, end), false, false);
            }
            "C" => {
                let end = self.line_len(self.row);
                self.operate('c', (self.row, end), false, false);
                self.mode = Mode::Insert;
            }
            "s" => {
                let end = (self.col + n).min(self.line_len(self.row));
                self.operate('c', (self.row, end), false, false);
                self.mode = Mode::Insert;
            }
            "S" => self.operate('c', (self.row, 0), true, true),
            "Y" => self.operate(
                'y',
                ((self.row + n - 1).min(self.lines.len() - 1), 0),
                true,
                true,
            ),
            "d" | "c" | "y" | ">" | "<" => {
                self.pending = motion_key.chars().next();
                self.pending_count = n;
            }
            "r" => self.pending = Some('r'),
            "p" => self.put(false),
            "P" => self.put(true),
            "u" => {
                if let Some(s) = self.undo.pop() {
                    self.redo.push(Snapshot {
                        lines: self.lines.clone(),
                        row: self.row,
                        col: self.col,
                    });
                    self.lines = s.lines;
                    self.row = s.row;
                    self.col = s.col;
                }
            }
            "J" => {
                if self.row + 1 < self.lines.len() {
                    self.snapshot();
                    let next: Vec<char> = self.lines.remove(self.row + 1);
                    let next: Vec<char> =
                        next.into_iter().skip_while(|c| c.is_whitespace()).collect();
                    let at = self.line_len(self.row);
                    if !next.is_empty() && at > 0 {
                        self.lines[self.row].push(' ');
                    }
                    self.col = self.line_len(self.row);
                    self.lines[self.row].extend(next);
                    let _ = at;
                }
            }
            "~" => {
                if let Some(c) = self.char_at(self.row, self.col) {
                    self.snapshot();
                    let swapped: Vec<char> = if c.is_uppercase() {
                        c.to_lowercase().collect()
                    } else {
                        c.to_uppercase().collect()
                    };
                    self.lines[self.row][self.col] = swapped[0];
                    self.col += 1;
                }
            }
            "v" => {
                self.mode = Mode::Visual;
                self.anchor = (self.row, self.col);
            }
            "V" => {
                self.mode = Mode::VisualLine;
                self.anchor = (self.row, self.col);
            }
            ":" => {
                self.mode = Mode::Command;
                self.command.clear();
            }
            _ => {}
        }
        Outcome::None
    }

    fn visual_command(&mut self, k: &str) -> Outcome {
        let Some((a, b)) = self.selection() else {
            return Outcome::None;
        };
        let linewise = self.mode == Mode::VisualLine;
        match k {
            "v" => {
                self.mode = if self.mode == Mode::Visual {
                    Mode::Normal
                } else {
                    Mode::Visual
                };
            }
            "V" => {
                self.mode = if self.mode == Mode::VisualLine {
                    Mode::Normal
                } else {
                    Mode::VisualLine
                };
            }
            "d" | "x" | "y" | "c" | ">" | "<" => {
                let op = match k {
                    "x" => 'd',
                    other => other.chars().next().unwrap(),
                };
                self.mode = Mode::Normal;
                self.row = a.0;
                self.col = a.1;
                if linewise || op == '>' || op == '<' {
                    self.operate(op, (b.0, 0), true, true);
                } else {
                    self.operate(op, b, false, true);
                }
            }
            "o" => {
                let here = (self.row, self.col);
                self.row = self.anchor.0;
                self.col = self.anchor.1;
                self.anchor = here;
            }
            ":" => {
                self.mode = Mode::Command;
                self.command.clear();
            }
            _ => {}
        }
        Outcome::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(e: &mut Editor, s: &str) {
        for c in s.chars() {
            let key = match c {
                '\x1b' => Key {
                    key: "escape".into(),
                    ..Default::default()
                },
                '\n' => Key {
                    key: "enter".into(),
                    ..Default::default()
                },
                c => Key {
                    key: c.to_lowercase().to_string(),
                    ch: Some(c.to_string()),
                    shift: c.is_uppercase(),
                    ..Default::default()
                },
            };
            e.handle(&key);
        }
    }

    #[test]
    fn insert_and_edit() {
        let mut e = Editor::new();
        keys(&mut e, "hello world\x1b");
        assert_eq!(e.text(), "hello world");
        keys(&mut e, "0dw");
        assert_eq!(e.text(), "world");
        keys(&mut e, "ohi there\x1bk");
        assert_eq!(e.text(), "world\nhi there");
        keys(&mut e, "ddp");
        assert_eq!(e.text(), "hi there\nworld");
        keys(&mut e, "u");
        assert_eq!(e.text(), "hi there");
        keys(&mut e, "0cwyo\x1b");
        assert_eq!(e.text(), "yo there");
        keys(&mut e, "$x");
        assert_eq!(e.text(), "yo ther");
        keys(&mut e, "0vey");
        keys(&mut e, "$p");
        assert_eq!(e.text(), "yo theryo");
        keys(&mut e, "yyP");
        assert_eq!(e.text(), "yo theryo\nyo theryo");
        keys(&mut e, "ggVjd");
        assert_eq!(e.text(), "");
    }

    #[test]
    fn counts_and_command_line() {
        let mut e = Editor::new();
        keys(&mut e, "a\nb\nc\nd\x1bgg2dd");
        assert_eq!(e.text(), "c\nd");
        keys(&mut e, ":clear\n");
        assert_eq!(e.text(), "");
        let mut e = Editor::new();
        keys(&mut e, "x\x1b");
        let k = |c: char| Key {
            key: c.to_string(),
            ch: Some(c.to_string()),
            ..Default::default()
        };
        e.handle(&k(':'));
        e.handle(&k('w'));
        assert_eq!(
            e.handle(&Key {
                key: "enter".into(),
                ..Default::default()
            }),
            Outcome::Send
        );
    }
}
