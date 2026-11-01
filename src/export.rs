//! Rasterize the page (paper + figure + ink) to PNG with tiny-skia, so the
//! agent sees exactly the vector page — no screenshots, no UI chrome.

use std::path::{Path, PathBuf};

use tiny_skia::{Color, FillRule, IntSize, Paint, PathBuilder, Pixmap, PixmapPaint, Transform};

use crate::ink::Stroke;
use crate::ui::{PAPER, Rect};

pub struct Figure {
    pub path: PathBuf,
    pub width: u32,
    pub height: u32,
    pixmap: Pixmap,
}

impl Figure {
    pub fn load(path: &Path) -> Result<Self, String> {
        let image = image::open(path)
            .map_err(|e| format!("cannot open {}: {e}", path.display()))?
            .to_rgba8();
        let (width, height) = image.dimensions();
        let mut data = image.into_raw();
        for px in data.chunks_exact_mut(4) {
            let a = px[3] as u16;
            for c in &mut px[..3] {
                *c = ((*c as u16 * a) / 255) as u8;
            }
        }
        let size = IntSize::from_wh(width, height).ok_or("empty image")?;
        let pixmap = Pixmap::from_vec(data, size).ok_or("bad image buffer")?;
        Ok(Self {
            path: path.to_path_buf(),
            width,
            height,
            pixmap,
        })
    }
}

fn color(rgb: u32, alpha: f32) -> Color {
    Color::from_rgba8(
        (rgb >> 16) as u8,
        (rgb >> 8) as u8,
        rgb as u8,
        (alpha.clamp(0.0, 1.0) * 255.0) as u8,
    )
}

/// Render the page at `scale` device pixels per logical pixel.
pub fn render_png(
    strokes: &[Stroke],
    figure: Option<(&Figure, Rect)>,
    width: f32,
    height: f32,
    scale: f32,
) -> Result<Vec<u8>, String> {
    let layers: Vec<Layer> = strokes
        .iter()
        .map(|s| Layer {
            stroke: s,
            color: s.color,
            alpha: s.alpha(),
        })
        .collect();
    render_layers(&layers, figure, Rect::new(0.0, 0.0, width, height), scale)
}

pub struct Layer<'a> {
    pub stroke: &'a Stroke,
    pub color: u32,
    pub alpha: f32,
}

/// Render a `region` of the page (logical px) with per-stroke styling.
pub fn render_layers(
    layers: &[Layer],
    figure: Option<(&Figure, Rect)>,
    region: Rect,
    scale: f32,
) -> Result<Vec<u8>, String> {
    let w = (region.w * scale).round().max(1.0) as u32;
    let h = (region.h * scale).round().max(1.0) as u32;
    let mut pixmap = Pixmap::new(w, h).ok_or("cannot allocate page")?;
    pixmap.fill(color(PAPER, 1.0));
    let page = Transform::from_translate(-region.x, -region.y).post_scale(scale, scale);

    if let Some((figure, rect)) = figure {
        let sx = rect.w / figure.width as f32;
        let sy = rect.h / figure.height as f32;
        let transform = Transform::from_row(sx, 0.0, 0.0, sy, rect.x, rect.y).post_concat(page);
        pixmap.draw_pixmap(
            0,
            0,
            figure.pixmap.as_ref(),
            &PixmapPaint {
                quality: tiny_skia::FilterQuality::Bicubic,
                ..PixmapPaint::default()
            },
            transform,
            None,
        );
    }

    for layer in layers {
        let outline = layer.stroke.outline();
        if outline.len() < 3 {
            continue;
        }
        let mut pb = PathBuilder::new();
        pb.move_to(outline[0].0, outline[0].1);
        for &(x, y) in &outline[1..] {
            pb.line_to(x, y);
        }
        pb.close();
        let Some(path) = pb.finish() else { continue };
        let mut paint = Paint::default();
        paint.set_color(color(layer.color, layer.alpha));
        paint.anti_alias = true;
        pixmap.fill_path(&path, &paint, FillRule::Winding, page, None);
    }

    pixmap.encode_png().map_err(|e| e.to_string())
}

pub fn drawings_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join(".local/share/sketchpad/drawings")
}

/// Save a PNG under ~/.local/share/sketchpad/drawings and return its path.
pub fn save(png: &[u8], stem: &str) -> Result<PathBuf, String> {
    let dir = drawings_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default();
    let path = dir.join(format!("{stem}-{stamp}.png"));
    std::fs::write(&path, png).map_err(|e| e.to_string())?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ink::{Sample, Stroke, Tool};

    #[test]
    fn renders_all_tools() {
        let mut strokes = Vec::new();
        for (row, tool) in [Tool::Pencil, Tool::Pen, Tool::Marker]
            .into_iter()
            .enumerate()
        {
            let y0 = 80.0 + row as f32 * 110.0;
            let mut s = Stroke::new(
                tool,
                crate::ui::COLORS[row * 2],
                Sample {
                    x: 40.0,
                    y: y0,
                    pressure: 0.2,
                },
            );
            for i in 1..300 {
                let t = i as f32;
                s.push(Sample {
                    x: 40.0 + t * 1.8,
                    y: y0 + (t / 18.0).sin() * 30.0,
                    pressure: 0.2 + 0.8 * (t / 300.0),
                });
            }
            strokes.push(s);
        }
        let mut erase_me = strokes.clone();
        crate::ink::erase(&mut erase_me, 300.0, 300.0, 14.0);
        assert!(erase_me.len() >= strokes.len());
        let png = render_png(&erase_me, None, 640.0, 400.0, 1.5).unwrap();
        std::fs::write("/tmp/ink-test.png", png).unwrap();
    }
}
