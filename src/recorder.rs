use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};

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
    pub fn x264_preset(self) -> &'static str {
        match self {
            Quality::Ultra => "slow",
            Quality::High => "medium",
            Quality::Medium => "veryfast",
            Quality::Low => "ultrafast",
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

pub fn resolve_encoder(pref: Encoder, caps: &Capabilities) -> Option<Encoder> {
    match pref {
        Encoder::Auto => caps.encoders.first().copied(),
        e => caps.encoders.iter().find(|c| **c == e).copied(),
    }
}

struct CommonArgs {
    fps: u32,
    mouse: bool,
    quality: Quality,
    perf: bool,
    threads: u32,
}

fn push_all(args: &mut Vec<String>, items: &[&str]) {
    args.extend(items.iter().map(|s| s.to_string()));
}

fn nvenc_args(a: &CommonArgs, out: &mut Vec<String>) {
    out.push("-c:v".into());
    out.push("h264_nvenc".into());
    if a.perf {
        push_all(
            out,
            &["-preset", "p1", "-tune", "ll", "-rc", "cbr", "-delay", "0"],
        );
        let mb = a.quality.bitrate_mbps().max(60);
        out.push("-b:v".into());
        out.push(format!("{mb}M"));
        out.push("-bufsize".into());
        out.push(format!("{}M", mb * 2));
    } else {
        push_all(out, &["-preset", "p5", "-tune", "hq", "-rc", "vbr"]);
        out.push("-cq".into());
        out.push(a.quality.cq().to_string());
        push_all(out, &["-b:v", "0"]);
        out.push("-maxrate".into());
        out.push(format!("{}M", a.quality.bitrate_mbps()));
        out.push("-bufsize".into());
        out.push(format!("{}M", a.quality.bitrate_mbps() * 2));
    }
    push_all(out, &["-profile:v", "high"]);
}

fn qsv_args(a: &CommonArgs, out: &mut Vec<String>) {
    out.push("-c:v".into());
    out.push("h264_qsv".into());
    if a.perf {
        push_all(out, &["-preset", "veryfast"]);
        out.push("-b:v".into());
        out.push(format!("{}M", a.quality.bitrate_mbps()));
    } else {
        push_all(out, &["-preset", "medium", "-global_quality"]);
        out.push(a.quality.cq().to_string());
    }
}

fn amf_args(a: &CommonArgs, out: &mut Vec<String>) {
    out.push("-c:v".into());
    out.push("h264_amf".into());
    if a.perf {
        push_all(out, &["-quality", "speed", "-rc", "cbr"]);
    } else {
        push_all(out, &["-quality", "balanced", "-rc", "vbr_peak"]);
    }
    out.push("-b:v".into());
    out.push(format!("{}M", a.quality.bitrate_mbps()));
}

fn x264_args(a: &CommonArgs, out: &mut Vec<String>) {
    out.push("-c:v".into());
    out.push("libx264".into());
    if a.perf {
        out.push("-preset".into());
        out.push("ultrafast".into());
    } else {
        out.push("-preset".into());
        out.push(a.quality.x264_preset().into());
    }
    out.push("-crf".into());
    out.push(a.quality.cq().to_string());
    if a.threads > 0 {
        out.push("-threads".into());
        out.push(a.threads.to_string());
    }
    out.push("-pix_fmt".into());
    out.push("yuv420p".into());
}

/// Pure argv builder for the legacy ffmpeg path (no process is spawned).
pub(crate) fn build_args(
    settings: &crate::settings::Settings,
    caps: &Capabilities,
    mode: Mode,
    encoder: Encoder,
    out_path: &std::path::Path,
) -> Vec<String> {
    let common = CommonArgs {
        fps: settings.fps,
        mouse: settings.capture_mouse,
        quality: settings.quality,
        perf: settings.performance_mode,
        threads: settings.cpu_threads,
    };
    let mut args: Vec<String> = vec![
        "-y".into(),
        "-hide_banner".into(),
        "-loglevel".into(),
        "warning".into(),
    ];

    let full_gpu = mode == Mode::FullScreen && encoder == Encoder::Nvenc && caps.ddagrab;

    if full_gpu {
        args.push("-filter_complex".into());
        args.push(format!(
            "ddagrab=output_idx=0:framerate={}:draw_mouse={}",
            common.fps,
            if common.mouse { 1 } else { 0 }
        ));
        nvenc_args(&common, &mut args);
    } else {
        push_all(&mut args, &["-f", "gdigrab"]);
        args.push("-framerate".into());
        args.push(common.fps.to_string());
        args.push("-draw_mouse".into());
        args.push(if common.mouse { "1" } else { "0" }.into());
        if let Mode::Area(r) = mode {
            args.push("-offset_x".into());
            args.push(r.x.to_string());
            args.push("-offset_y".into());
            args.push(r.y.to_string());
            args.push("-video_size".into());
            args.push(format!("{}x{}", r.w, r.h));
        }
        args.push("-i".into());
        args.push("desktop".into());
        match encoder {
            Encoder::Nvenc => nvenc_args(&common, &mut args),
            Encoder::Qsv => qsv_args(&common, &mut args),
            Encoder::Amf => amf_args(&common, &mut args),
            _ => x264_args(&common, &mut args),
        }
        if encoder != Encoder::X264 {
            args.push("-pix_fmt".into());
            args.push("yuv420p".into());
        }
    }

    args.push("-movflags".into());
    args.push("+faststart".into());
    args.push(out_path.to_string_lossy().to_string());
    args
}

pub fn build_command(
    ffmpeg: &str,
    settings: &crate::settings::Settings,
    caps: &Capabilities,
    mode: Mode,
    encoder: Encoder,
    out_path: &std::path::Path,
) -> Result<Command> {
    let args = build_args(settings, caps, mode, encoder, out_path);
    let mut cmd = Command::new(ffmpeg);
    cmd.args(&args)
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .stdout(Stdio::null());
    if std::env::var_os("ORR_PRINT_CMD").is_some() {
        eprintln!("[orr] ffmpeg {}", shell_words_join(&args));
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000);
    }
    Ok(cmd)
}

fn shell_words_join(args: &[String]) -> String {
    args.iter()
        .map(|a| {
            if a.contains(' ') {
                format!("\"{a}\"")
            } else {
                a.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub struct Spawned {
    pub child: Child,
    pub stdin: ChildStdin,
}

pub fn start(mut cmd: Command) -> Result<Spawned> {
    let mut child = cmd.spawn().context("failed to launch ffmpeg")?;
    let stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
    Ok(Spawned { child, stdin })
}

pub fn graceful_stop(stdin: &mut ChildStdin) {
    use std::io::Write;
    let _ = stdin.write_all(b"q");
    let _ = stdin.flush();
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
    use crate::settings::Settings;

    fn test_settings() -> Settings {
        Settings::default()
    }

    fn pos(path: &str) -> std::path::PathBuf {
        std::path::PathBuf::from(path)
    }

    #[test]
    fn area_args_use_gdigrab_offsets() {
        let s = test_settings();
        let caps = Capabilities::default();
        let args = build_args(
            &s,
            &caps,
            Mode::Area(Rect {
                x: 100,
                y: 50,
                w: 960,
                h: 720,
            }),
            Encoder::X264,
            &pos("out.mp4"),
        );
        let has = |v: &str| args.iter().any(|a| a == v);
        assert!(has("-f"));
        assert!(args.contains(&"gdigrab".to_string()));
        let idx = args.iter().position(|a| a == "-offset_x").unwrap();
        assert_eq!(args[idx + 1], "100");
        let idx = args.iter().position(|a| a == "-offset_y").unwrap();
        assert_eq!(args[idx + 1], "50");
        let idx = args.iter().position(|a| a == "-video_size").unwrap();
        assert_eq!(args[idx + 1], "960x720");
        assert_eq!(args.last().unwrap(), "out.mp4");
        // x264 keeps its own pix_fmt; no extra global pix_fmt push
        assert_eq!(args.iter().filter(|a| a.as_str() == "-pix_fmt").count(), 1);
    }

    #[test]
    fn fullscreen_nvenc_with_ddagrab_uses_filter_complex() {
        let s = test_settings();
        let caps = Capabilities {
            ddagrab: true,
            ..Default::default()
        };
        let args = build_args(&s, &caps, Mode::FullScreen, Encoder::Nvenc, &pos("gpu.mp4"));
        assert!(args.contains(&"-filter_complex".to_string()));
        assert!(
            args.iter()
                .any(|a| a.starts_with("ddagrab=output_idx=0:framerate="))
        );
        assert!(args.contains(&"h264_nvenc".to_string()));
        assert!(!args.contains(&"desktop".to_string()));
    }

    #[test]
    fn fullscreen_without_ddagrab_falls_back_to_gdigrab() {
        let s = test_settings();
        let caps = Capabilities::default(); // ddagrab: false
        let args = build_args(&s, &caps, Mode::FullScreen, Encoder::Amf, &pos("amf.mp4"));
        assert!(!args.contains(&"-filter_complex".to_string()));
        assert!(args.contains(&"desktop".to_string()));
        assert!(args.contains(&"h264_amf".to_string()));
        assert!(args.contains(&"yuv420p".to_string())); // hw encoder pix_fmt
        assert_eq!(args.last().unwrap(), "amf.mp4");
    }
}
