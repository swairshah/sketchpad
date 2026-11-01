//! The look you pick with `:config`: light or dark paper, and the typefaces
//! for headings, text and code. Read from anywhere (the terminal snapshots
//! off the main thread), so it lives in process-wide statics.

use std::sync::{
    RwLock,
    atomic::{AtomicBool, Ordering},
};

use gpui::SharedString;

/// The bundled typefaces (see `fonts.rs`), and the defaults.
pub const NEWSREADER: &str = "Newsreader";
pub const PLEX_SANS: &str = "IBM Plex Sans";
pub const PLEX_MONO: &str = "IBM Plex Mono";

#[derive(Clone, Copy, Debug)]
pub struct Palette {
    pub paper: u32,
    pub tray: u32,
    /// The page's body text, the floating button.
    pub ink_dark: u32,
    pub ink: u32,
    pub ink2: u32,
    pub muted: u32,
    pub faint: u32,
    pub rule: u32,
    pub rule_strong: u32,
    pub surface: u32,
    pub sunken: u32,
    pub code_bg: u32,
    pub accent: u32,
    pub user: u32,
    pub terra: u32,
    pub term_bg: u32,
    pub term_fg: u32,
    pub card: u32,
    pub card_border: u32,
    pub card_text: u32,
    pub divider: u32,
    /// Hand-drawn tray icons (undo, redo, keyboard).
    pub icon: u32,
    pub pi_bg: u32,
    pub pi_bg_listen: u32,
    pub error: u32,
    pub ok: u32,
    /// The terminal's 16 ANSI colors.
    pub ansi: [u32; 16],
}

pub const LIGHT: Palette = Palette {
    paper: 0xf5efe2,
    tray: 0xfcf9f2,
    ink_dark: 0x2a2722,
    ink: 0x26241f,
    ink2: 0x4d4940,
    muted: 0x8c8273,
    faint: 0xb3ada2,
    rule: 0xe3dccd,
    rule_strong: 0xd2c9b8,
    surface: 0xfbf8f1,
    sunken: 0xeee8db,
    code_bg: 0xefe9dc,
    accent: 0xc6432c,
    user: 0x2f6db5,
    terra: 0xc15f3c,
    term_bg: 0xf3eee3,
    term_fg: 0x26241f,
    card: 0xfffdf8,
    card_border: 0xe6dfd0,
    card_text: 0x6f665a,
    divider: 0xe3dbcb,
    icon: 0x5b5348,
    pi_bg: 0xf1e9db,
    pi_bg_listen: 0xf6dccf,
    error: 0xb3412e,
    ok: 0x5c9a6b,
    ansi: [
        0x26241f, 0xb3412e, 0x4f7a3a, 0x9a6b12, 0x2f6db5, 0x8a4f9e, 0x2f7f86, 0x8a8478, //
        0x6f665a, 0xc6432c, 0x5f8f45, 0xb07d1a, 0x3b7fd0, 0xa060b5, 0x3a949c, 0x4d4940,
    ],
};

/// The same paper by lamplight: warm charcoal, cream ink.
pub const DARK: Palette = Palette {
    paper: 0x1e1c19,
    tray: 0x2a2723,
    ink_dark: 0xe6dfd2,
    ink: 0xece6da,
    ink2: 0xc2baab,
    muted: 0x8f8678,
    faint: 0x655e54,
    rule: 0x34302a,
    rule_strong: 0x47423a,
    surface: 0x25221e,
    sunken: 0x2c2925,
    code_bg: 0x282521,
    accent: 0xe2654b,
    user: 0x74a7e3,
    terra: 0xdb7a5a,
    term_bg: 0x23201d,
    term_fg: 0xe6dfd2,
    card: 0x2b2824,
    card_border: 0x3d3831,
    card_text: 0xa89f91,
    divider: 0x3d3831,
    icon: 0xc2baab,
    pi_bg: 0x34302a,
    pi_bg_listen: 0x4a3229,
    error: 0xe0705c,
    ok: 0x7dbb8b,
    ansi: [
        0x3a3631, 0xe0705c, 0x8fbf6e, 0xd9a94a, 0x74a7e3, 0xc08ad3, 0x6cbcc3, 0xc2baab, //
        0x6f675c, 0xf08a72, 0xa5d184, 0xe8bf63, 0x92bdf0, 0xd3a3e3, 0x8ad1d7, 0xece6da,
    ],
};

static DARK_MODE: AtomicBool = AtomicBool::new(false);
static FONTS: RwLock<Option<[SharedString; 3]>> = RwLock::new(None);

pub fn dark() -> bool {
    DARK_MODE.load(Ordering::Relaxed)
}

pub fn set_dark(on: bool) {
    DARK_MODE.store(on, Ordering::Relaxed);
}

pub fn pal() -> &'static Palette {
    if dark() { &DARK } else { &LIGHT }
}

/// Ink as it shows on the page: charcoal strokes turn cream on dark paper.
pub fn ink(color: u32) -> u32 {
    if dark() && color == crate::ui::COLORS[0] { 0xe6dfd2 } else { color }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Headings,
    Text,
    Code,
}

impl Role {
    pub const ALL: [Role; 3] = [Role::Headings, Role::Text, Role::Code];

    pub fn label(self) -> &'static str {
        match self {
            Role::Headings => "Headings",
            Role::Text => "Text",
            Role::Code => "Code",
        }
    }

    /// What the role styles, shown under its name in `:config`.
    pub fn covers(self) -> &'static str {
        match self {
            Role::Headings => "headings in replies, drawing prompts",
            Role::Text => "what you type, replies",
            Role::Code => "code, terminal, status bar",
        }
    }

    pub fn default_family(self) -> &'static str {
        match self {
            Role::Headings => NEWSREADER,
            Role::Text => PLEX_SANS,
            Role::Code => PLEX_MONO,
        }
    }

    /// The key it is saved under in settings.json.
    pub fn key(self) -> &'static str {
        match self {
            Role::Headings => "font_headings",
            Role::Text => "font_text",
            Role::Code => "font_code",
        }
    }

    fn index(self) -> usize {
        self as usize
    }
}

pub fn family(role: Role) -> SharedString {
    FONTS
        .read()
        .ok()
        .and_then(|f| f.as_ref().map(|f| f[role.index()].clone()))
        .unwrap_or_else(|| SharedString::new_static(role.default_family()))
}

pub fn set_family(role: Role, name: impl Into<SharedString>) {
    if let Ok(mut f) = FONTS.write() {
        let fonts = f.get_or_insert_with(|| Role::ALL.map(|r| SharedString::new_static(r.default_family())));
        fonts[role.index()] = name.into();
    }
}

pub fn serif() -> SharedString {
    family(Role::Headings)
}

pub fn sans() -> SharedString {
    family(Role::Text)
}

pub fn mono() -> SharedString {
    family(Role::Code)
}
