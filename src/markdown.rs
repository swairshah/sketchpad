//! Markdown for the agent's replies: parsed with pulldown-cmark into a small
//! block tree the notebook renders natively, code highlighted with syntect,
//! and ```svg blocks rasterized with resvg.

use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    ops::Range,
    path::PathBuf,
    sync::OnceLock,
};

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use syntect::{
    easy::HighlightLines,
    highlighting::{FontStyle, Theme, ThemeSet},
    parsing::SyntaxSet,
};

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Span {
    pub bold: bool,
    pub italic: bool,
    pub code: bool,
    pub strike: bool,
    pub link: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Rich {
    pub text: String,
    pub spans: Vec<(Range<usize>, Span)>,
}

impl Rich {
    fn push(&mut self, text: &str, span: Span) {
        let start = self.text.len();
        self.text.push_str(text);
        if span != Span::default() {
            self.spans.push((start..self.text.len(), span));
        }
    }

    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty()
    }
}

#[derive(Clone, Debug)]
pub struct Token {
    pub text: String,
    pub color: u32,
    pub bold: bool,
    pub italic: bool,
}

#[derive(Clone, Debug)]
pub enum Block {
    Heading(u8, Rich),
    Paragraph(Rich),
    List {
        start: Option<u64>,
        items: Vec<Vec<Block>>,
    },
    Quote(Vec<Block>),
    Code {
        lang: String,
        lines: Vec<Vec<Token>>,
    },
    Svg {
        path: PathBuf,
        width: f32,
        height: f32,
    },
    /// An svg block still being written (or broken).
    Pending(String),
    Table {
        header: Vec<Rich>,
        rows: Vec<Vec<Rich>>,
    },
    Rule,
}

// ------------------------------------------------------------------ parse

struct Frame {
    kind: FrameKind,
    blocks: Vec<Block>,
}

enum FrameKind {
    Root,
    Quote,
    Item,
    List(Option<u64>, Vec<Vec<Block>>),
}

pub fn parse(source: &str) -> Vec<Block> {
    let mut stack = vec![Frame {
        kind: FrameKind::Root,
        blocks: Vec::new(),
    }];
    let mut rich: Option<Rich> = None;
    let mut heading: Option<u8> = None;
    let mut style = Span::default();
    let mut code: Option<(String, String)> = None;
    let mut table: Option<(Vec<Rich>, Vec<Vec<Rich>>, Vec<Rich>, bool)> = None;

    let flush = |rich: &mut Option<Rich>, heading: &mut Option<u8>, stack: &mut Vec<Frame>| {
        if let Some(r) = rich.take() {
            if !r.is_empty() {
                let block = match heading.take() {
                    Some(level) => Block::Heading(level, r),
                    None => Block::Paragraph(r),
                };
                stack.last_mut().unwrap().blocks.push(block);
            }
        }
    };

    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    for event in Parser::new_ext(source, options) {
        if let Some((_, ref mut text)) = code {
            match event {
                Event::Text(t) => {
                    text.push_str(&t);
                    continue;
                }
                Event::End(TagEnd::CodeBlock) => {
                    let (lang, text) = code.take().unwrap();
                    stack
                        .last_mut()
                        .unwrap()
                        .blocks
                        .push(code_block(&lang, &text, source));
                    continue;
                }
                _ => continue,
            }
        }
        match event {
            Event::Start(Tag::Paragraph) => {
                flush(&mut rich, &mut heading, &mut stack);
                rich = Some(Rich::default());
            }
            Event::End(TagEnd::Paragraph) => flush(&mut rich, &mut heading, &mut stack),
            Event::Start(Tag::Heading { level, .. }) => {
                flush(&mut rich, &mut heading, &mut stack);
                heading = Some(match level {
                    HeadingLevel::H1 => 1,
                    HeadingLevel::H2 => 2,
                    HeadingLevel::H3 => 3,
                    _ => 4,
                });
                rich = Some(Rich::default());
            }
            Event::End(TagEnd::Heading(_)) => flush(&mut rich, &mut heading, &mut stack),
            Event::Start(Tag::BlockQuote(_)) => {
                flush(&mut rich, &mut heading, &mut stack);
                stack.push(Frame {
                    kind: FrameKind::Quote,
                    blocks: Vec::new(),
                });
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                flush(&mut rich, &mut heading, &mut stack);
                let frame = stack.pop().unwrap();
                stack
                    .last_mut()
                    .unwrap()
                    .blocks
                    .push(Block::Quote(frame.blocks));
            }
            Event::Start(Tag::List(start)) => {
                flush(&mut rich, &mut heading, &mut stack);
                stack.push(Frame {
                    kind: FrameKind::List(start, Vec::new()),
                    blocks: Vec::new(),
                });
            }
            Event::End(TagEnd::List(_)) => {
                flush(&mut rich, &mut heading, &mut stack);
                let frame = stack.pop().unwrap();
                if let FrameKind::List(start, items) = frame.kind {
                    stack
                        .last_mut()
                        .unwrap()
                        .blocks
                        .push(Block::List { start, items });
                }
            }
            Event::Start(Tag::Item) => {
                stack.push(Frame {
                    kind: FrameKind::Item,
                    blocks: Vec::new(),
                });
                rich = Some(Rich::default());
            }
            Event::End(TagEnd::Item) => {
                flush(&mut rich, &mut heading, &mut stack);
                let frame = stack.pop().unwrap();
                if let Some(Frame {
                    kind: FrameKind::List(_, items),
                    ..
                }) = stack.last_mut()
                {
                    items.push(frame.blocks);
                }
            }
            Event::TaskListMarker(done) => {
                if let Some(r) = rich.as_mut() {
                    r.push(if done { "☑ " } else { "☐ " }, Span::default());
                }
            }
            Event::Start(Tag::CodeBlock(kind)) => {
                flush(&mut rich, &mut heading, &mut stack);
                let lang = match kind {
                    CodeBlockKind::Fenced(lang) => {
                        lang.split_whitespace().next().unwrap_or("").to_owned()
                    }
                    CodeBlockKind::Indented => String::new(),
                };
                code = Some((lang, String::new()));
            }
            Event::Start(Tag::Table(_)) => {
                flush(&mut rich, &mut heading, &mut stack);
                table = Some((Vec::new(), Vec::new(), Vec::new(), true));
            }
            Event::End(TagEnd::Table) => {
                if let Some((header, rows, _, _)) = table.take() {
                    stack
                        .last_mut()
                        .unwrap()
                        .blocks
                        .push(Block::Table { header, rows });
                }
            }
            Event::Start(Tag::TableHead) => {
                if let Some(t) = table.as_mut() {
                    t.3 = true;
                }
            }
            Event::End(TagEnd::TableHead) => {
                if let Some(t) = table.as_mut() {
                    t.0 = std::mem::take(&mut t.2);
                    t.3 = false;
                }
            }
            Event::Start(Tag::TableRow) => {}
            Event::End(TagEnd::TableRow) => {
                if let Some(t) = table.as_mut() {
                    let row = std::mem::take(&mut t.2);
                    if !t.3 {
                        t.1.push(row);
                    }
                }
            }
            Event::Start(Tag::TableCell) => rich = Some(Rich::default()),
            Event::End(TagEnd::TableCell) => {
                if let (Some(t), Some(r)) = (table.as_mut(), rich.take()) {
                    t.2.push(r);
                }
            }
            Event::Start(Tag::Emphasis) => style.italic = true,
            Event::End(TagEnd::Emphasis) => style.italic = false,
            Event::Start(Tag::Strong) => style.bold = true,
            Event::End(TagEnd::Strong) => style.bold = false,
            Event::Start(Tag::Strikethrough) => style.strike = true,
            Event::End(TagEnd::Strikethrough) => style.strike = false,
            Event::Start(Tag::Link { .. }) => style.link = true,
            Event::End(TagEnd::Link) => style.link = false,
            Event::Text(t) => {
                rich.get_or_insert_with(Rich::default).push(&t, style);
            }
            Event::Code(t) => {
                rich.get_or_insert_with(Rich::default).push(
                    &t,
                    Span {
                        code: true,
                        ..style
                    },
                );
            }
            Event::InlineMath(t) | Event::DisplayMath(t) => {
                rich.get_or_insert_with(Rich::default).push(
                    &t,
                    Span {
                        italic: true,
                        ..style
                    },
                );
            }
            Event::SoftBreak => rich.get_or_insert_with(Rich::default).push(" ", style),
            Event::HardBreak => rich.get_or_insert_with(Rich::default).push("\n", style),
            Event::Rule => {
                flush(&mut rich, &mut heading, &mut stack);
                stack.last_mut().unwrap().blocks.push(Block::Rule);
            }
            _ => {}
        }
    }
    // An unterminated fence while streaming.
    if let Some((lang, text)) = code.take() {
        stack.last_mut().unwrap().blocks.push(if lang == "svg" {
            Block::Pending("drawing a diagram…".into())
        } else {
            code_block(&lang, &text, "")
        });
    }
    flush(&mut rich, &mut heading, &mut stack);
    while stack.len() > 1 {
        let frame = stack.pop().unwrap();
        let blocks = frame.blocks;
        match (frame.kind, stack.last_mut()) {
            (
                FrameKind::Item,
                Some(Frame {
                    kind: FrameKind::List(_, items),
                    ..
                }),
            ) => items.push(blocks),
            (FrameKind::List(start, items), Some(parent)) => {
                parent.blocks.push(Block::List { start, items })
            }
            (_, Some(parent)) => parent.blocks.push(Block::Quote(blocks)),
            _ => {}
        }
    }
    stack.pop().map(|f| f.blocks).unwrap_or_default()
}

// ------------------------------------------------------------------ code

fn syntaxes() -> &'static SyntaxSet {
    static SET: OnceLock<SyntaxSet> = OnceLock::new();
    SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

/// The highlighting for the paper you picked: InspiredGitHub on light, Ocean on dark.
fn theme() -> &'static Theme {
    static THEMES: OnceLock<(Theme, Theme)> = OnceLock::new();
    let (light, dark) = THEMES.get_or_init(|| {
        let mut set = ThemeSet::load_defaults();
        let fallback = set.themes.values().next().unwrap().clone();
        let light = set.themes.remove("InspiredGitHub").unwrap_or_else(|| fallback.clone());
        let dark = set.themes.remove("base16-ocean.dark").unwrap_or(fallback);
        (light, dark)
    });
    if crate::theme::dark() { dark } else { light }
}

/// Load the highlighter in the background so the first code block is instant.
pub fn warm_up() {
    std::thread::spawn(|| {
        let _ = syntaxes();
        let _ = theme();
        let _ = svg_options();
    });
}

fn code_block(lang: &str, text: &str, _source: &str) -> Block {
    if lang == "svg" {
        return match render_svg(text) {
            Ok((path, width, height)) => Block::Svg {
                path,
                width,
                height,
            },
            Err(e) => Block::Pending(format!("diagram could not be drawn: {e}")),
        };
    }
    let set = syntaxes();
    let syntax = set
        .find_syntax_by_token(match lang {
            "ts" | "typescript" | "tsx" => "js",
            "sh" | "shell" | "zsh" => "bash",
            other => other,
        })
        .unwrap_or_else(|| set.find_syntax_plain_text());
    let mut h = HighlightLines::new(syntax, theme());
    let mut lines = Vec::new();
    for line in syntect::util::LinesWithEndings::from(text.trim_end_matches('\n')) {
        let tokens = match h.highlight_line(line, set) {
            Ok(ranges) => ranges
                .into_iter()
                .map(|(style, t)| Token {
                    text: t.trim_end_matches('\n').replace('\t', "    "),
                    color: ((style.foreground.r as u32) << 16)
                        | ((style.foreground.g as u32) << 8)
                        | style.foreground.b as u32,
                    bold: style.font_style.contains(FontStyle::BOLD),
                    italic: style.font_style.contains(FontStyle::ITALIC),
                })
                .filter(|t| !t.text.is_empty())
                .collect(),
            Err(_) => vec![Token {
                text: line.trim_end_matches('\n').to_owned(),
                color: crate::theme::pal().ink,
                bold: false,
                italic: false,
            }],
        };
        lines.push(tokens);
    }
    Block::Code {
        lang: lang.to_owned(),
        lines,
    }
}

// ------------------------------------------------------------------ svg

fn svg_options() -> &'static resvg::usvg::Options<'static> {
    static OPTIONS: OnceLock<resvg::usvg::Options<'static>> = OnceLock::new();
    OPTIONS.get_or_init(|| {
        let mut options = resvg::usvg::Options::default();
        options.fontdb_mut().load_system_fonts();
        for data in crate::fonts::FILES {
            options.fontdb_mut().load_font_data(data.to_vec());
        }
        options.font_family = crate::theme::PLEX_SANS.into();
        options
    })
}

/// Diagrams are drawn for light paper: on dark paper, flip their lightness
/// (dark ink turns cream, pale fills turn charcoal) and keep their hues.
fn for_dark_paper(pixmap: &mut resvg::tiny_skia::Pixmap) {
    for px in pixmap.pixels_mut() {
        let a = px.alpha();
        if a == 0 {
            continue;
        }
        let c = px.demultiply();
        let (h, s, l) = to_hsl(c.red(), c.green(), c.blue());
        // Not all the way to black / white: land between dark paper and cream ink.
        // Pale tinted fills would go muddy brown in the dark: quiet their tint.
        let pale = ((l - 0.6) / 0.3).clamp(0.0, 1.0);
        let (r, g, b) = from_hsl(h, s * (1.0 - 0.65 * pale), 0.11 + (1.0 - l) * 0.80);
        *px = resvg::tiny_skia::ColorU8::from_rgba(r, g, b, a).premultiply();
    }
}

fn to_hsl(r: u8, g: u8, b: u8) -> (f32, f32, f32) {
    let (r, g, b) = (r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0);
    let (max, min) = (r.max(g).max(b), r.min(g).min(b));
    let l = (max + min) / 2.0;
    let d = max - min;
    if d < 1e-6 {
        return (0.0, 0.0, l);
    }
    let s = d / (1.0 - (2.0 * l - 1.0).abs());
    let h = if max == r {
        ((g - b) / d).rem_euclid(6.0)
    } else if max == g {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    };
    (h * 60.0, s, l)
}

fn from_hsl(h: f32, s: f32, l: f32) -> (u8, u8, u8) {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let x = c * (1.0 - ((h / 60.0).rem_euclid(2.0) - 1.0).abs());
    let m = l - c / 2.0;
    let (r, g, b) = match (h / 60.0) as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let to = |v: f32| ((v + m).clamp(0.0, 1.0) * 255.0).round() as u8;
    (to(r), to(g), to(b))
}

fn render_svg(source: &str) -> Result<(PathBuf, f32, f32), String> {
    let mut hasher = DefaultHasher::new();
    source.hash(&mut hasher);
    let dark = crate::theme::dark();
    dark.hash(&mut hasher);
    let dir = std::env::temp_dir().join("sketchpad-svg");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(format!("{:016x}.png", hasher.finish()));
    let tree = resvg::usvg::Tree::from_str(source, svg_options()).map_err(|e| e.to_string())?;
    let size = tree.size();
    // Diagrams are shown at most as wide as the column.
    let max_w = 760.0f32;
    let scale = (max_w / size.width()).min(1.6);
    let (w, h) = (size.width() * scale, size.height() * scale);
    if !path.exists() {
        let px = 2.0; // retina-ish
        let mut pixmap =
            resvg::tiny_skia::Pixmap::new((w * px).ceil() as u32, (h * px).ceil() as u32)
                .ok_or("empty diagram")?;
        resvg::render(
            &tree,
            resvg::tiny_skia::Transform::from_scale(scale * px, scale * px),
            &mut pixmap.as_mut(),
        );
        if dark {
            for_dark_paper(&mut pixmap);
        }
        pixmap.save_png(&path).map_err(|e| e.to_string())?;
    }
    Ok((path, w, h))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hsl_round_trips() {
        for c in [(0xc1, 0x5f, 0x3c), (0x2a, 0x27, 0x22), (0xf5, 0xef, 0xe2), (0x3a, 0x8f, 0xd1)] {
            let (h, s, l) = to_hsl(c.0, c.1, c.2);
            let back = from_hsl(h, s, l);
            assert!((back.0 as i32 - c.0 as i32).abs() <= 1 && (back.1 as i32 - c.1 as i32).abs() <= 1 && (back.2 as i32 - c.2 as i32).abs() <= 1, "{c:?} -> {back:?}");
        }
    }

    #[test]
    fn dark_diagram_preview() {
        crate::theme::set_dark(true);
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="400" height="140"><rect x="10" y="20" width="150" height="50" rx="8" fill="#ddd5c6" stroke="#2a2722" stroke-width="2"/><text x="40" y="52" font-size="20" fill="#2a2722">client A</text><line x1="160" y1="45" x2="220" y2="45" stroke="#2a2722" stroke-width="2"/><rect x="220" y="20" width="170" height="50" rx="8" fill="#e8dccf" stroke="#c15f3c" stroke-width="3"/><text x="232" y="52" font-size="18" fill="#c15f3c">actor ["room"]</text><text x="20" y="110" font-size="16" fill="#8c8273">asleep, wakes on next request</text></svg>"##;
        let (path, _, _) = render_svg(svg).unwrap();
        crate::theme::set_dark(false);
        if let Ok(out) = std::env::var("DIAGRAM_OUT") {
            // On the dark diagram card, as the page shows it.
            let mut card = resvg::tiny_skia::Pixmap::new(800, 280).unwrap();
            card.fill(resvg::tiny_skia::Color::from_rgba8(0x25, 0x22, 0x1e, 255));
            let fig = resvg::tiny_skia::Pixmap::load_png(&path).unwrap();
            card.draw_pixmap(0, 0, fig.as_ref(), &Default::default(), Default::default(), None);
            card.save_png(out).unwrap();
        }
    }

    #[test]
    fn parses_common_markdown() {
        let md = "# Title\n\nSome **bold** and `code`.\n\n- one\n- two\n  1. nested\n\n```rust\nfn main() {}\n```\n\n> quote\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n```svg\n<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 100 50\"><circle cx=\"25\" cy=\"25\" r=\"20\" fill=\"#c15f3c\"/></svg>\n```\n";
        let blocks = parse(md);
        assert!(matches!(blocks[0], Block::Heading(1, _)));
        assert!(matches!(blocks[1], Block::Paragraph(_)));
        assert!(matches!(blocks[2], Block::List { .. }));
        assert!(matches!(blocks[3], Block::Code { .. }));
        assert!(matches!(blocks[4], Block::Quote(_)));
        assert!(matches!(blocks[5], Block::Table { .. }));
        assert!(matches!(blocks[6], Block::Svg { .. }), "{:?}", blocks[6]);
        let streaming = parse("text\n\n```py\nprint(1)");
        assert!(matches!(streaming[1], Block::Code { .. }));
    }
}
