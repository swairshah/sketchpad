//! The notebook document: an endless sheet of paper holding the human's
//! typed messages, the agent's replies (markdown + sketches), agent-asked
//! figures, free ink anywhere, and checkpoints. Saved as JSON on every change
//! (debounced by the app), so it survives restarts like the agent does.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{
    ink::{self, Stroke},
    ops::Note,
    terminal::TermLine,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Part {
    /// Markdown, as streamed.
    Text(String),
    /// A hand-drawn figure inside the reply; coordinates are local to it.
    Sketch {
        strokes: Vec<Stroke>,
        labels: Vec<Note>,
        height: f32,
    },
    /// A tool the agent ran (shown as a quiet one-liner).
    Tool { name: String, summary: String },
    /// Room for handwriting the human put where the reply was about to continue.
    Space { height: f32 },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Block {
    User {
        text: String,
        /// "with 3 annotations" etc. when ink went along with the message.
        attachment: Option<String>,
        /// What this message comments on (a passage of an earlier message).
        #[serde(default)]
        quote: Option<String>,
        /// The agent request it went out as (to attach the ink caption later).
        #[serde(default)]
        request_id: Option<String>,
    },
    Agent {
        parts: Vec<Part>,
        done: bool,
        #[serde(default)]
        error: Option<String>,
    },
    /// Room kept for handwriting, so the text that follows flows below it
    /// instead of over it.
    Space { height: f32 },
    /// Something an outside agent asked the human to draw on (`sketchpad ask`).
    Figure {
        prompt: String,
        from: String,
        image: Option<PathBuf>,
        width: u32,
        height: u32,
        open: bool,
    },
    /// An inline terminal (ctrl+`). `lines` is its transcript as last saved:
    /// while the shell runs the app draws the live screen; after a restart
    /// this is what stays on the page.
    Terminal {
        id: u64,
        lines: Vec<TermLine>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Checkpoint {
    pub block: usize,
    pub title: String,
    #[serde(default)]
    pub summary: String,
    /// Marked by the agent (a milestone) rather than a plain turn.
    pub agent: bool,
}

#[derive(Default, Serialize, Deserialize)]
pub struct Notebook {
    pub blocks: Vec<Block>,
    /// Free ink on the page, in document coordinates (x: window px, y: from the top of the notebook).
    pub ink: Vec<Stroke>,
    /// Free text on the page (agent labels), document coordinates.
    pub notes: Vec<Note>,
    pub checkpoints: Vec<Checkpoint>,
    /// What's in the composer.
    pub draft: String,
    /// Ink ids the agent has already been told about.
    #[serde(default)]
    pub seen_ink: Vec<u64>,
    /// When each page stroke was drawn (unix seconds), by its origin id; ink
    /// drawn close together in time and space forms one drawing.
    #[serde(default)]
    pub ink_times: std::collections::HashMap<u64, f64>,
    /// 2: page ink/notes x is measured from the centre line (was window px).
    #[serde(default)]
    pub version: u32,
}

pub fn data_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join(".local/share/sketchpad")
}

pub fn path() -> PathBuf {
    data_dir().join("notebook.json")
}

impl Notebook {
    pub fn load(path: &Path) -> Self {
        let nb: Notebook = std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        let max_id = nb
            .ink
            .iter()
            .chain(nb.blocks.iter().flat_map(|b| match b {
                Block::Agent { parts, .. } => parts
                    .iter()
                    .flat_map(|p| match p {
                        Part::Sketch { strokes, .. } => strokes.iter().collect::<Vec<_>>(),
                        _ => Vec::new(),
                    })
                    .collect::<Vec<_>>(),
                _ => Vec::new(),
            }))
            .map(|s| s.id.max(s.origin))
            .max()
            .unwrap_or(0);
        ink::reserve_ids(max_id);
        let mut nb = nb;
        if nb.version < 2 {
            // Older notebooks stored x as window px of a full-width (1280 px) window.
            for s in &mut nb.ink {
                for p in &mut s.samples {
                    p.x -= 640.0;
                }
            }
            for n in &mut nb.notes {
                n.x -= 640.0;
            }
            nb.version = 2;
        }
        nb
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(self).map_err(std::io::Error::other)?)?;
        std::fs::rename(tmp, path)
    }

    /// The agent block being written, if any.
    pub fn open_agent(&mut self) -> Option<&mut Vec<Part>> {
        match self.blocks.last_mut() {
            Some(Block::Agent { parts, done: false, .. }) => Some(parts),
            _ => None,
        }
    }
}
