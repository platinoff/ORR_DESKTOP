//! Hardware discovery / diagnostics for the LEGACY reporting path only
//! (`orr_desktop probe`). Since P5 the record path never spawns a process:
//! recording runs through the native in-process pipeline (`src/native.rs`).
//! Everything here shells out to ffmpeg purely to *report* which hardware
//! encoders a machine offers until P6 wires vendor APIs in-process.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::process::{Command, Stdio};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Encoder {
    Auto,
    Nvenc,
    Qsv,
    Amf,
    X264,
}

impl Encoder {
    pub const ALL: [Encoder; 5] = [
        Encoder::Auto,
        Encoder::Nvenc,
        Encoder::Qsv,
        Encoder::Amf,
        Encoder::X264,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Encoder::Auto => "Auto (best available)",
            Encoder::Nvenc => "NVIDIA NVENC (GPU)",
            Encoder::Qsv => "Intel QuickSync (GPU)",
            Encoder::Amf => "AMD AMF (GPU)",
            Encoder::X264 => "CPU x264",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Quality {
    Ultra,
    High,
    Medium,
    Low,
}

impl Quality {
    pub const ALL: [Quality; 4] = [Quality::Ultra, Quality::High, Quality::Medium, Quality::Low];
    pub fn label(self) -> &'static str {
        match self {
            Quality::Ultra => "Ultra",
            Quality::High => "High",
            Quality::Medium => "Medium",
            Quality::Low => "Low",
        }
    }
    pub fn cq(self) -> i32 {
        match self {
            Quality::Ultra => 18,
            Quality::High => 21,
            Quality::Medium => 24,
            Quality::Low => 28,
        }
    }
    pub fn bitrate_mbps(self) -> u32 {
        match self {
            Quality::Ultra => 80,
            Quality::High => 50,
            Quality::Medium => 30,
            Quality::Low => 15,
        }
    }
}

#[derive(Debug, Default)]
pub struct Capabilities {
    pub encoders: Vec<Encoder>,
    pub ddagrab: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    FullScreen,
    Area(Rect),
}

fn run_capture(cmd: &str, args: &[&str], needle: &str) -> bool {
    Command::new(cmd)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains(needle))
        .unwrap_or(false)
}

fn encoder_priority(e: Encoder) -> usize {
    match e {
        Encoder::Nvenc => 0,
        Encoder::Qsv => 1,
        Encoder::Amf => 2,
        _ => 3,
    }
}

fn encoder_name(e: Encoder) -> Option<&'static str> {
    match e {
        Encoder::Nvenc => Some("h264_nvenc"),
        Encoder::Qsv => Some("h264_qsv"),
        Encoder::Amf => Some("h264_amf"),
        _ => None,
    }
}

fn encoder_usable(ffmpeg: &str, e: Encoder) -> bool {
    let Some(name) = encoder_name(e) else {
        return true;
    };
    Command::new(ffmpeg)
        .args([
            "-hide_banner",
            "-loglevel",
            "quiet",
            "-f",
            "lavfi",
            "-i",
            "color=c=black:s=320x240:r=10:d=0.2",
            "-frames:v",
            "3",
            "-c:v",
            name,
            "-f",
            "null",
            "-",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Probe ffmpeg for available H.264 encoders (diagnostics only). Degrades to
/// empty capabilities when ffmpeg is absent — the record path does not care.
pub fn detect(ffmpeg: &str) -> Capabilities {
    let mut caps = Capabilities::default();
    let enc_out = Command::new(ffmpeg)
        .args(["-hide_banner", "-encoders"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output();
    if let Ok(o) = enc_out {
        let text = String::from_utf8_lossy(&o.stdout);
        for line in text.lines() {
            let l = line.trim();
            if l.starts_with("A") || l.starts_with("V") {
                if l.contains("h264_nvenc") && !caps.encoders.contains(&Encoder::Nvenc) {
                    caps.encoders.push(Encoder::Nvenc);
                } else if l.contains("h264_qsv") && !caps.encoders.contains(&Encoder::Qsv) {
                    caps.encoders.push(Encoder::Qsv);
                } else if l.contains("h264_amf") && !caps.encoders.contains(&Encoder::Amf) {
                    caps.encoders.push(Encoder::Amf);
                } else if l.contains("libx264") && !caps.encoders.contains(&Encoder::X264) {
                    caps.encoders.push(Encoder::X264);
                }
            }
        }
    }
    caps.encoders.sort_by_key(|e| encoder_priority(*e));
    caps.encoders.retain(|e| encoder_usable(ffmpeg, *e));
    caps.ddagrab = run_capture(ffmpeg, &["-hide_banner", "-filters"], "ddagrab");
    caps
}

pub fn output_file(dir: &std::path::Path) -> PathBuf {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let days = t / 86400;
    let rem = t % 86400;
    let (y, m, d) = civil_from_days(days);
    let name = format!(
        "ORR_{:04}{:02}{:02}_{:02}{:02}{:02}.mp4",
        y,
        m,
        d,
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    );
    dir.join(name)
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

pub fn validate_output_dir(dir: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("cannot create {:?}", dir))?;
    if !dir.is_dir() {
        bail!("{:?} is not a directory", dir);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_degrades_gracefully_without_ffmpeg() {
        // P5 acceptance: recording must work on machines where this returns
        // empty capabilities — the native pipeline never consults ffmpeg.
        let caps = detect("Z:/definitely-not-installed/ffmpeg.exe");
        assert!(caps.encoders.is_empty());
        assert!(!caps.ddagrab);
    }

    #[test]
    fn output_file_name_has_timestamp_shape() {
        let p = output_file(std::path::Path::new("."));
        let name = p.file_name().unwrap().to_string_lossy();
        assert!(name.starts_with("ORR_"));
        assert!(name.ends_with(".mp4"));
        assert_eq!(name.len(), "ORR_YYYYMMDD_HHMMSS.mp4".len());
    }
}
