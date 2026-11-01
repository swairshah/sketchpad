//! The typefaces of the syncpi app, bundled: Newsreader for headings,
//! IBM Plex Sans for text, IBM Plex Mono for code and the status bar.

use std::borrow::Cow;

macro_rules! font_files {
    ($($name:literal),* $(,)?) => {
        &[$(include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/fonts/", $name)) as &[u8]),*]
    };
}

pub const FILES: &[&[u8]] = font_files!(
    "IBMPlexMono-400.ttf",
    "IBMPlexMono-400-italic.ttf",
    "IBMPlexMono-500.ttf",
    "IBMPlexSans-400.ttf",
    "IBMPlexSans-400-italic.ttf",
    "IBMPlexSans-500.ttf",
    "IBMPlexSans-600.ttf",
    "Newsreader-400.ttf",
    "Newsreader-400-italic.ttf",
    "Newsreader-500.ttf",
    "Newsreader-500-italic.ttf",
    "Newsreader-600.ttf",
);

pub fn all() -> Vec<Cow<'static, [u8]>> {
    FILES.iter().map(|b| Cow::Borrowed(*b)).collect()
}
