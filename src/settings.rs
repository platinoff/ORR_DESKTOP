use crate::recorder::{Encoder, Quality};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub fps: u32,
    pub quality: Quality,
    pub encoder: Encoder,
    pub performance_mode: bool,
    pub capture_mouse: bool,
    pub cpu_threads: u32,
    pub output_dir: PathBuf,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            fps: 30,
            quality: Quality::High,
            encoder: Encoder::Auto,
            performance_mode: false,
            capture_mouse: true,
            cpu_threads: 0,
            output_dir: default_video_dir(),
        }
    }
}

fn default_video_dir() -> PathBuf {
    dirs::video_dir()
        .or_else(dirs::document_dir)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("ORR_DESKTOP")
}

impl Settings {
    pub fn load(path: &Path) -> Settings {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn sanitized(mut self) -> Self {
        if ![24, 30, 60].contains(&self.fps) {
            self.fps = 30;
        }
        if self.output_dir.as_os_str().is_empty() {
            self.output_dir = default_video_dir();
        }
        self
    }

    pub fn config_path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("ORR_DESKTOP")
            .join("config.json")
    }
}
