//! Native GDI capture — BitBlt frame source (P2).
//!
//! Crop semantics mirror ffmpeg's `gdigrab` desktop mode: user coordinates are
//! standard screen coordinates (primary monitor origin at 0,0, negative for
//! monitors left/above), validated against the virtual-screen bounds. This is
//! the exact convention `cli-area` already uses, so parity is structural.
//!
//! Pacing is wall-clock driven (`Instant` targets per tick), never
//! "sleep after blt", so a slow BitBlt cannot accumulate drift.

// P2 source: wired into the default record path at P4 (native default
// switch). Unit tests exercise it meanwhile.
#![allow(dead_code)]

use crate::pipeline::{Frame, FrameSource, FrameSpec};
use crate::recorder::Rect;
use anyhow::{Result, anyhow, bail};
use std::ptr::null_mut;
use std::time::{Duration, Instant};
use windows_sys::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CreateCompatibleDC, CreateDIBSection,
    DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, HBITMAP, HDC, HGDIOBJ, ReleaseDC, SRCCOPY,
    SelectObject,
};

const CAPTUREBLT: u32 = 0x4000_0000;

fn virtual_bounds() -> (i32, i32, i32, i32) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
        SM_YVIRTUALSCREEN,
    };
    unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    }
}

/// Validate a rect against the virtual desktop; returns the clamped rect.
pub fn validate_rect(rect: Rect) -> Result<Rect> {
    let (vx, vy, vw, vh) = virtual_bounds();
    if vw <= 0 || vh <= 0 {
        bail!("virtual screen has no size");
    }
    let left = rect.x.max(vx);
    let top = rect.y.max(vy);
    let right = (rect.x + rect.w as i32).min(vx + vw);
    let bottom = (rect.y + rect.h as i32).min(vy + vh);
    if right - left < 1 || bottom - top < 1 {
        bail!(
            "rect {:?} does not intersect virtual screen ({vx},{vy} {vw}x{vh})",
            rect
        );
    }
    Ok(Rect {
        x: left,
        y: top,
        w: (right - left) as u32,
        h: (bottom - top) as u32,
    })
}

struct Session {
    screen_dc: HDC,
    mem_dc: HDC,
    bmp: HBITMAP,
    old_bmp: HGDIOBJ,
    bits: *mut core::ffi::c_void,
    buf_size: usize,
}

impl Session {
    fn new(w: u32, h: u32) -> Result<Self> {
        unsafe {
            let screen_dc = GetDC(null_mut());
            if screen_dc.is_null() {
                return Err(anyhow!("GetDC(NULL) failed"));
            }
            let mem_dc = CreateCompatibleDC(screen_dc);
            if mem_dc.is_null() {
                ReleaseDC(null_mut(), screen_dc);
                return Err(anyhow!("CreateCompatibleDC failed"));
            }
            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: core::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w as i32,
                    // negative height => top-down rows, no manual flip
                    biHeight: -(h as i32),
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits: *mut core::ffi::c_void = null_mut();
            let bmp = CreateDIBSection(mem_dc, &bmi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
            if bmp.is_null() || bits.is_null() {
                DeleteDC(mem_dc);
                ReleaseDC(null_mut(), screen_dc);
                return Err(anyhow!("CreateDIBSection failed"));
            }
            let old_bmp = SelectObject(mem_dc, bmp);
            Ok(Self {
                screen_dc,
                mem_dc,
                bmp,
                old_bmp,
                bits,
                buf_size: (w * h * 4) as usize,
            })
        }
    }

    /// Blt the desktop region into the section and copy it out.
    fn grab(&mut self, x: i32, y: i32, w: u32, h: u32, pts_ms: u64) -> Frame {
        unsafe {
            BitBlt(
                self.mem_dc,
                0,
                0,
                w as i32,
                h as i32,
                self.screen_dc,
                x,
                y,
                SRCCOPY | CAPTUREBLT,
            );
        }
        let mut data = vec![0u8; self.buf_size];
        unsafe {
            core::ptr::copy_nonoverlapping(
                self.bits as *const u8,
                data.as_mut_ptr(),
                self.buf_size,
            );
        }
        Frame {
            width: w,
            height: h,
            data,
            pts_ms,
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.mem_dc, self.old_bmp);
            DeleteObject(self.bmp as HGDIOBJ);
            DeleteDC(self.mem_dc);
            ReleaseDC(null_mut(), self.screen_dc);
        }
    }
}
/// GDI BitBlt frame source over a fixed desktop rectangle.
pub struct GdiSource {
    rect: Rect,
    fps: u32,
    /// Test/determinism hook: `None` streams until externally stopped.
    max_frames: Option<u64>,
    session: Option<Session>,
    started_at: Option<Instant>,
    emitted: u64,
}

impl GdiSource {
    pub fn new(rect: Rect, fps: u32) -> Result<Self> {
        if fps == 0 {
            bail!("fps must be > 0");
        }
        Ok(Self {
            rect: validate_rect(rect)?,
            fps,
            max_frames: None,
            session: None,
            started_at: None,
            emitted: 0,
        })
    }

    pub fn full_desktop(fps: u32) -> Result<Self> {
        let (vx, vy, vw, vh) = virtual_bounds();
        Self::new(
            Rect {
                x: vx,
                y: vy,
                w: vw.max(1) as u32,
                h: vh.max(1) as u32,
            },
            fps,
        )
    }

    #[cfg(test)]
    fn with_max_frames(mut self, n: u64) -> Self {
        self.max_frames = Some(n);
        self
    }

    fn frame_dur(&self) -> Duration {
        Duration::from_nanos(1_000_000_000 / self.fps as u64)
    }
}

impl FrameSource for GdiSource {
    fn spec(&self) -> FrameSpec {
        FrameSpec {
            width: self.rect.w,
            height: self.rect.h,
            fps: self.fps,
        }
    }

    fn start(&mut self) -> Result<()> {
        debug_assert!(self.session.is_none());
        self.session = Some(Session::new(self.rect.w, self.rect.h)?);
        self.started_at = Some(Instant::now());
        Ok(())
    }

    fn next_frame(&mut self) -> Option<Frame> {
        if let Some(max) = self.max_frames
            && self.emitted >= max
        {
            return None;
        }
        let started = self.started_at?;
        // Wall-clock pacing: target time of this tick is independent of how
        // long previous grabs took.
        let target = started + self.frame_dur() * (self.emitted as u32 + 1);
        let now = Instant::now();
        if target > now {
            std::thread::sleep(target - now);
        }
        let pts_ms = started.elapsed().as_millis() as u64;
        let frame =
            self.session
                .as_mut()?
                .grab(self.rect.x, self.rect.y, self.rect.w, self.rect.h, pts_ms);
        self.emitted += 1;
        Some(frame)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ffmpeg_exe() -> Option<String> {
        let exe = std::env::var("ORR_FFMPEG").unwrap_or_else(|_| "ffmpeg".to_string());
        let ok = std::process::Command::new(&exe)
            .arg("-version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        ok.then_some(exe)
    }

    #[test]
    fn spec_reports_requested_geometry() {
        let src = GdiSource::new(
            Rect {
                x: 100,
                y: 50,
                w: 320,
                h: 240,
            },
            30,
        )
        .expect("rect valid on any display");
        assert_eq!(
            src.spec(),
            FrameSpec {
                width: 320,
                height: 240,
                fps: 30
            }
        );
    }

    #[test]
    fn offscreen_rect_is_rejected() {
        let (vx, vy, vw, vh) = virtual_bounds();
        let far = Rect {
            x: vx + vw + 10_000,
            y: vy + vh + 10_000,
            w: 100,
            h: 100,
        };
        assert!(GdiSource::new(far, 30).is_err());
    }

    #[test]
    fn captures_frames_with_monotonic_pts() {
        let mut src = GdiSource::full_desktop(60)
            .expect("desktop source")
            .with_max_frames(3);
        let spec = src.spec();
        assert!(spec.width > 0 && spec.height > 0);
        src.start().expect("session starts");
        let mut last_pts = -1i64;
        for _ in 0..3 {
            let f = src.next_frame().expect("frame under max");
            assert_eq!((f.width, f.height), (spec.width, spec.height));
            assert_eq!(f.data.len(), (spec.width * spec.height * 4) as usize);
            assert!((f.pts_ms as i64) > last_pts);
            last_pts = f.pts_ms as i64;
        }
        assert!(src.next_frame().is_none(), "stream ends at max_frames");
    }

    #[test]
    fn crop_parity_with_gdigrab() {
        let Some(ffmpeg) = ffmpeg_exe() else {
            eprintln!("[skip] ffmpeg not available (set ORR_FFMPEG)");
            return;
        };
        let rect = Rect {
            x: 200,
            y: 150,
            w: 480,
            h: 360,
        };
        let mut src = GdiSource::new(rect, 30)
            .expect("rect valid")
            .with_max_frames(1);
        src.start().expect("session");
        let ours = src.next_frame().expect("one frame");

        // Same rect through gdigrab, rawvideo BGRA to stdout, one frame.
        let out = std::process::Command::new(&ffmpeg)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "gdigrab",
                "-offset_x",
                &rect.x.to_string(),
                "-offset_y",
                &rect.y.to_string(),
                "-video_size",
                &format!("{}x{}", rect.w, rect.h),
                "-frames:v",
                "1",
                "-pix_fmt",
                "bgra",
                "-f",
                "rawvideo",
                "-",
                "-y",
            ])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output()
            .expect("ffmpeg runs");
        assert!(out.status.success(), "gdigrab run failed");

        // Structural parity first: identical byte count = identical geometry.
        assert_eq!(
            out.stdout.len(),
            ours.data.len(),
            "gdigrab crop dimensions differ from GDI source"
        );

        // Content tolerance: live desktop moves between the two grabs
        // (clocks, cursor blink), so compare channel means with slack.
        let mean = |b: &[u8]| -> [f64; 4] {
            let n = b.len() / 4;
            let mut acc = [0f64; 4];
            for px in b.chunks_exact(4) {
                for c in 0..4 {
                    acc[c] += px[c] as f64;
                }
            }
            acc.map(|a| a / n as f64)
        };
        let m_ours = mean(&ours.data);
        let m_gdi = mean(&out.stdout);
        for c in 0..4 {
            let diff = (m_ours[c] - m_gdi[c]).abs();
            assert!(
                diff < 12.0,
                "channel {c} mean differs by {diff}: ours={:?} gdigrab={:?}",
                m_ours,
                m_gdi
            );
        }
    }
}
