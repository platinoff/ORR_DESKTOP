//! Native session orchestration (P4): wires a capture [`FrameSource`] to the
//! software encoder and MP4 muxer with no external process involved. Used by
//! both the tray app (spawned on a worker thread) and the CLI (blocking).

use crate::capture::wgcap::{enumerate_monitors, native_source};
use crate::encode::sw::SwH264Encoder;
use crate::mux::mp4::Mp4Muxer;
use crate::pipeline::{self, Frame, FrameSource, FrameSpec, PipelineStats};
use crate::recorder::{Quality, Rect};
use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Round a rect down to even width/height — I420 chroma subsampling needs
/// whole 2x2 blocks.
pub fn even_rect(r: Rect) -> Rect {
    Rect {
        x: r.x,
        y: r.y,
        w: r.w - r.w % 2,
        h: r.h - r.h % 2,
    }
}

/// Bounding rect of the entire virtual desktop (all monitors, GDI coords).
pub fn full_desktop_rect() -> Result<Rect> {
    let monitors = enumerate_monitors().context("cannot enumerate monitors")?;
    anyhow::ensure!(!monitors.is_empty(), "no monitors attached");
    let l = monitors.iter().map(|m| m.gdi_rect.left).min().unwrap();
    let t = monitors.iter().map(|m| m.gdi_rect.top).min().unwrap();
    let r = monitors.iter().map(|m| m.gdi_rect.right).max().unwrap();
    let b = monitors.iter().map(|m| m.gdi_rect.bottom).max().unwrap();
    Ok(Rect {
        x: l,
        y: t,
        w: (r - l).max(0) as u32,
        h: (b - t).max(0) as u32,
    })
}

/// Everything one recording session needs besides the output path.
#[derive(Clone, Debug)]
pub struct SessionParams {
    pub rect: Rect,
    pub fps: u32,
    pub cursor: bool,
    pub quality: Quality,
}

/// Source wrapper that ends the stream when the stop flag is raised or the
/// optional frame budget is spent.
struct Stoppable<S: FrameSource> {
    inner: S,
    stop: Arc<AtomicBool>,
    max_frames: Option<u32>,
    emitted: u32,
}

impl<S: FrameSource> FrameSource for Stoppable<S> {
    fn spec(&self) -> FrameSpec {
        self.inner.spec()
    }

    fn start(&mut self) -> Result<()> {
        self.inner.start()
    }

    fn next_frame(&mut self) -> Option<Frame> {
        if self.stop.load(Ordering::Relaxed) {
            return None;
        }
        if let Some(max) = self.max_frames
            && self.emitted >= max
        {
            return None;
        }
        match self.inner.next_frame() {
            Some(f) => {
                self.emitted += 1;
                Some(f)
            }
            None => None,
        }
    }
}

/// Run one recording session to completion on the calling thread. `stop` ends
/// the stream gracefully (muxer still finalizes); `max_frames` caps it for
/// tests/deterministic runs.
pub fn run_blocking(
    params: &SessionParams,
    out_path: PathBuf,
    stop: Arc<AtomicBool>,
    max_frames: Option<u32>,
) -> Result<(PipelineStats, PathBuf)> {
    let rect = even_rect(params.rect);
    anyhow::ensure!(rect.w > 0 && rect.h > 0, "empty capture region");
    let source = native_source(rect, params.fps, params.cursor)?;
    println!(
        "[orr] native pipeline: {} {}x{} @{}fps -> {}",
        source.name(),
        rect.w,
        rect.h,
        params.fps,
        out_path.display()
    );
    let mut source = Stoppable {
        inner: source,
        stop,
        max_frames,
        emitted: 0,
    };
    let mut encoder = SwH264Encoder::new(params.quality);
    let mut muxer = Mp4Muxer::create(out_path);
    pipeline::run(&mut source, &mut encoder, &mut muxer)
}

/// Handle for a recording running on a background thread.
pub struct NativeSessionHandle {
    stop: Arc<AtomicBool>,
}

impl NativeSessionHandle {
    /// Request a graceful stop; the worker finishes encoding and muxing, then
    /// invokes the completion callback.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Spawn a recording session on its own thread. `on_done` receives the output
/// path on success or a formatted error.
pub fn spawn_session(
    params: SessionParams,
    out_path: PathBuf,
    on_done: impl FnOnce(Result<PathBuf, String>) + Send + 'static,
) -> Result<NativeSessionHandle> {
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = Arc::clone(&stop);
    std::thread::Builder::new()
        .name("orr-native-rec".into())
        .spawn(move || {
            let result = run_blocking(&params, out_path, stop2, None);
            on_done(result.map(|(_, path)| path).map_err(|e| format!("{e:#}")));
        })
        .context("cannot spawn recorder thread")?;
    Ok(NativeSessionHandle { stop })
}

/// Count processes whose image name matches `name` case-insensitively via a
/// Toolhelp snapshot. Used by tests to prove the native path never spawns an
/// external encoder executable.
#[cfg(test)]
pub(crate) fn count_processes(name: &str) -> usize {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };

    fn lower(c: u16) -> u16 {
        if (b'A' as u16..=b'Z' as u16).contains(&c) {
            c + 32
        } else {
            c
        }
    }
    let needle: Vec<u16> = std::ffi::OsStr::new(name)
        .encode_wide()
        .map(lower)
        .collect();
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return 0;
        }
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut count = 0usize;
        if Process32FirstW(snap, &mut entry) != 0 {
            loop {
                let len = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(0);
                let exe: Vec<u16> = entry.szExeFile[..len].iter().map(|&c| lower(c)).collect();
                if exe == needle {
                    count += 1;
                }
                if Process32NextW(snap, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snap);
        count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn even_rect_rounds_down() {
        let r = Rect {
            x: -10,
            y: 5,
            w: 321,
            h: 241,
        };
        let e = even_rect(r);
        assert_eq!((e.x, e.y), (-10, 5));
        assert_eq!((e.w, e.h), (320, 240));
    }

    #[test]
    fn full_desktop_covers_primary() {
        let r = full_desktop_rect().expect("monitors");
        assert!(r.w >= 640 && r.h >= 480, "virtual desktop too small: {r:?}");
    }

    #[test]
    fn ffmpeg_process_count_stable_across_native_run() {
        // The acceptance gate for P4: a full native recording must not touch
        // any external encoder binary. We compare ffmpeg.exe process counts
        // before/after; any delta means something spawned (or leaked) a child.
        let before = count_processes("ffmpeg.exe");
        let spec_rect = full_desktop_rect().unwrap();
        // Cap the region so WGC/GDI work on a small area even on 4K desktops.
        let params = SessionParams {
            rect: Rect {
                x: spec_rect.x,
                y: spec_rect.y,
                w: spec_rect.w.min(160),
                h: spec_rect.h.min(120),
            },
            fps: 30,
            cursor: false,
            quality: Quality::Low,
        };
        let out = std::env::temp_dir().join(format!("orr_native_e2e_{}.mp4", std::process::id()));
        let stop = Arc::new(AtomicBool::new(false));
        let (stats, written) =
            run_blocking(&params, out.clone(), Arc::clone(&stop), Some(9)).expect("e2e run");
        assert_eq!(stats.frames_encoded, 9);
        assert_eq!(written, out);
        assert!(out.metadata().expect("meta").len() > 512);

        let after = count_processes("ffmpeg.exe");
        assert_eq!(
            before, after,
            "ffmpeg.exe process appeared/disappeared during a native run"
        );
        std::fs::remove_file(&out).ok();
    }

    #[test]
    fn spawned_session_reports_completion() {
        let spec_rect = full_desktop_rect().unwrap();
        let params = SessionParams {
            rect: Rect {
                x: spec_rect.x,
                y: spec_rect.y,
                w: spec_rect.w.min(128),
                h: spec_rect.h.min(96),
            },
            fps: 30,
            cursor: false,
            quality: Quality::Low,
        };
        let out = std::env::temp_dir().join(format!("orr_spawn_e2e_{}.mp4", std::process::id()));
        let (tx, rx) = std::sync::mpsc::channel();
        let handle = spawn_session(params, out.clone(), move |res| {
            let _ = tx.send(res);
        })
        .expect("spawn");
        std::thread::sleep(std::time::Duration::from_millis(150));
        handle.stop();
        let res = rx
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("worker finished");
        let path = res.expect("session ok");
        assert_eq!(path, out);
        assert!(out.exists());
        std::fs::remove_file(&out).ok();
    }
}
