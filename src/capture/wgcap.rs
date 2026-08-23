//! Native Windows.Graphics.Capture frame source (P3).
//!
//! Captures a whole monitor through a free-threaded `Direct3D11CaptureFramePool`,
//! copies each GPU texture into a CPU staging texture and crops it to the
//! requested rectangle, producing the same BGRA `Frame`s as [`gdi::GdiSource`].
//!
//! Coordinate conventions (mirrors `gdi.rs` / gdigrab):
//! - User rects are **process/GDI screen coordinates** (`GetSystemMetrics`
//!   space; primary monitor at 0,0, negative for left/above monitors).
//! - WGC always captures the **physical** monitor texture, so the module maps
//!   the GDI-space crop onto physical pixels using `DEVMODE.dmPosition` /
//!   `dmPelsWidth|Height`, which are DPI-virtualization-independent. This makes
//!   regions exact under per-monitor scaling (e.g. 150% displays).
//!
//! A region that spans more than one monitor cannot be served by a single WGC
//! item; [`native_source`] falls back to the GDI BitBlt backend in that case
//! (and whenever the host predates Windows 10 1903 or has no D3D11 device).
//!
//! Requires Windows 10 1903+ (free-threaded pool + cursor toggle).

// P3 source: wired into the default record path at P4 (native default
// switch). Unit tests exercise it meanwhile.
#![allow(dead_code)]

use crate::capture::gdi::{GdiSource, validate_rect};
use crate::pipeline::{Frame, FrameSource, FrameSpec};
use crate::recorder::Rect;
use anyhow::{Result, anyhow, bail};
use std::time::{Duration, Instant};
use windows::Graphics::Capture::{
    Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::{Direct3D11::IDirect3DDevice, DirectXPixelFormat};
use windows::Win32::Foundation::{HMODULE, LPARAM, RECT, RPC_E_CHANGED_MODE, S_FALSE, S_OK};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Gdi::{
    DEVMODEW, ENUM_CURRENT_SETTINGS, EnumDisplayMonitors, EnumDisplaySettingsW, GetMonitorInfoW,
    HDC, HMONITOR, MONITORINFOEXW,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::core::{BOOL, Interface, PCWSTR, factory};

/// `MONITORINFOF_PRIMARY` was un-typed to a raw `u32` flag in windows-rs 0.62.
const MONITORINFOF_PRIMARY: u32 = 0x1;

/// One attached monitor in both coordinate spaces we care about.
#[derive(Clone, Debug)]
pub struct MonitorInfo {
    pub hmon: HMONITOR,
    /// Process/GDI coordinates (`GetMonitorInfoW.rcMonitor`) — the same space
    /// user-facing rects live in.
    pub gdi_rect: RECT,
    /// Physical desktop origin (`DEVMODE.dmPosition`, never DPI-virtualized).
    pub phys_origin: (i32, i32),
    /// Physical pixel size (`DEVMODE.dmPelsWidth/Height`).
    pub phys_size: (u32, u32),
    pub primary: bool,
}

impl MonitorInfo {
    #[cfg(test)]
    fn synthetic(gdi: (i32, i32, i32, i32), phys_origin: (i32, i32), phys: (u32, u32)) -> Self {
        Self {
            hmon: HMONITOR::default(),
            gdi_rect: RECT {
                left: gdi.0,
                top: gdi.1,
                right: gdi.0 + gdi.2,
                bottom: gdi.1 + gdi.3,
            },
            phys_origin,
            phys_size: phys,
            primary: gdi.0 == 0 && gdi.1 == 0,
        }
    }

    fn gdi_size(&self) -> (i64, i64) {
        (
            (self.gdi_rect.right - self.gdi_rect.left).max(0) as i64,
            (self.gdi_rect.bottom - self.gdi_rect.top).max(0) as i64,
        )
    }

    fn contains_gdi(&self, r: &Rect) -> bool {
        (r.x as i64) >= self.gdi_rect.left as i64
            && (r.y as i64) >= self.gdi_rect.top as i64
            && ((r.x + r.w as i32) as i64) <= self.gdi_rect.right as i64
            && ((r.y + r.h as i32) as i64) <= self.gdi_rect.bottom as i64
    }

    fn intersection_area(&self, r: &Rect) -> i64 {
        let l = (r.x as i64).max(self.gdi_rect.left as i64);
        let t = (r.y as i64).max(self.gdi_rect.top as i64);
        let ri = ((r.x + r.w as i32) as i64).min(self.gdi_rect.right as i64);
        let b = ((r.y + r.h as i32) as i64).min(self.gdi_rect.bottom as i64);
        (ri - l).max(0) * (b - t).max(0)
    }

    /// Map a GDI-space rect onto this monitor's physical pixels.
    /// Returns `(crop_x, crop_y, w, h)` inside the captured texture.
    fn map_crop_physical(&self, r: &Rect) -> Result<(u32, u32, u32, u32)> {
        let (gw, gh) = self.gdi_size();
        if gw == 0 || gh == 0 {
            bail!("monitor {:?} has zero GDI size", self.gdi_rect);
        }
        let (pw, ph) = (self.phys_size.0 as f64, self.phys_size.1 as f64);
        let sx = pw / gw as f64;
        let sy = ph / gh as f64;
        let rx = (r.x as i64 - self.gdi_rect.left as i64) as f64;
        let ry = (r.y as i64 - self.gdi_rect.top as i64) as f64;
        let cx = (rx * sx).round().clamp(0.0, pw - 1.0) as u32;
        let cy = (ry * sy).round().clamp(0.0, ph - 1.0) as u32;
        let cw = ((r.w as f64 * sx).round() as u32).clamp(1, pw as u32 - cx);
        let ch = ((r.h as f64 * sy).round() as u32).clamp(1, ph as u32 - cy);
        Ok((cx, cy, cw, ch))
    }
}

unsafe extern "system" fn collect_hmons(
    hmon: HMONITOR,
    _dc: HDC,
    _rect: *mut RECT,
    lparam: LPARAM,
) -> BOOL {
    // SAFETY: lparam points at our Vec<HMONITOR>; EnumDisplayMonitors calls
    // this synchronously on the same thread.
    let list = unsafe { &mut *(lparam.0 as *mut Vec<HMONITOR>) };
    list.push(hmon);
    true.into()
}

/// Enumerate every attached monitor with GDI and physical geometry.
pub fn enumerate_monitors() -> Result<Vec<MonitorInfo>> {
    let mut hmons: Vec<HMONITOR> = Vec::new();
    unsafe {
        // SAFETY: the callback only appends to our leaked-free Vec handed via
        // LPARAM; EnumDisplayMonitors calls it synchronously.
        if !EnumDisplayMonitors(
            None,
            None,
            Some(collect_hmons),
            LPARAM(&mut hmons as *mut _ as isize),
        )
        .as_bool()
        {
            bail!("EnumDisplayMonitors failed");
        }
    }
    let mut out = Vec::with_capacity(hmons.len());
    for hmon in hmons {
        let mut miex = MONITORINFOEXW::default();
        miex.monitorInfo.cbSize = core::mem::size_of::<MONITORINFOEXW>() as u32;
        unsafe {
            if !GetMonitorInfoW(hmon, &mut miex.monitorInfo).as_bool() {
                bail!("GetMonitorInfoW failed");
            }
            let mut dm = DEVMODEW {
                dmSize: core::mem::size_of::<DEVMODEW>() as u16,
                ..Default::default()
            };
            let got = EnumDisplaySettingsW(
                PCWSTR::from_raw(miex.szDevice.as_ptr()),
                ENUM_CURRENT_SETTINGS,
                &mut dm,
            )
            .as_bool();
            let info = if got {
                let pos = dm.Anonymous1.Anonymous2.dmPosition;
                MonitorInfo {
                    hmon,
                    gdi_rect: miex.monitorInfo.rcMonitor,
                    phys_origin: (pos.x, pos.y),
                    phys_size: (dm.dmPelsWidth.max(1), dm.dmPelsHeight.max(1)),
                    primary: miex.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
                }
            } else {
                // No DEVMODE (rare): assume 1:1 mapping from the GDI rect.
                MonitorInfo {
                    hmon,
                    gdi_rect: miex.monitorInfo.rcMonitor,
                    phys_origin: (
                        miex.monitorInfo.rcMonitor.left,
                        miex.monitorInfo.rcMonitor.top,
                    ),
                    phys_size: (
                        (miex.monitorInfo.rcMonitor.right - miex.monitorInfo.rcMonitor.left).max(1)
                            as u32,
                        (miex.monitorInfo.rcMonitor.bottom - miex.monitorInfo.rcMonitor.top).max(1)
                            as u32,
                    ),
                    primary: miex.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
                }
            };
            out.push(info);
        }
    }
    if out.is_empty() {
        bail!("no monitors found");
    }
    Ok(out)
}

/// Pick the monitor that fully contains `rect`; `None` when the rect spans
/// monitors (single WGC items are per-monitor).
fn choose_containing_monitor<'a>(
    monitors: &'a [MonitorInfo],
    rect: &Rect,
) -> Option<&'a MonitorInfo> {
    monitors
        .iter()
        .filter(|m| m.contains_gdi(rect))
        .max_by_key(|m| m.intersection_area(rect))
}

fn primary_monitor(monitors: &[MonitorInfo]) -> Result<&MonitorInfo> {
    monitors
        .iter()
        .find(|m| m.primary)
        .or_else(|| monitors.first())
        .ok_or_else(|| anyhow!("no monitor"))
}

// ---------------------------------------------------------------------------
// COM / D3D plumbing
// ---------------------------------------------------------------------------

/// Balances a successful `CoInitializeEx` on this thread with `CoUninitialize`.
struct ComGuard {
    owned: bool,
}

impl ComGuard {
    fn init_mta() -> Result<Self> {
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if hr == S_OK || hr == S_FALSE {
            Ok(Self { owned: true })
        } else if hr == RPC_E_CHANGED_MODE {
            // Thread already pinned to another apartment; COM is usable anyway.
            Ok(Self { owned: false })
        } else {
            Err(anyhow!("CoInitializeEx failed: {hr}"))
        }
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.owned {
            unsafe { CoUninitialize() };
        }
    }
}

struct StagingTex {
    tex: ID3D11Texture2D,
    width: u32,
    height: u32,
}

impl StagingTex {
    fn new(ctx: &ID3D11DeviceContext, src: &ID3D11Texture2D) -> Result<Self> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { src.GetDesc(&mut desc) };
        desc.Usage = D3D11_USAGE_STAGING;
        desc.BindFlags = 0;
        desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        desc.MiscFlags = 0;
        desc.MipLevels = 1;
        let mut tex = None;
        unsafe {
            let dev = ctx.GetDevice()?;
            dev.CreateTexture2D(&desc, None, Some(&mut tex))?;
        }
        let tex = tex.ok_or_else(|| anyhow!("CreateTexture2D returned null"))?;
        Ok(Self {
            tex,
            width: desc.Width,
            height: desc.Height,
        })
    }

    /// Copy `src` into the staging texture and read back its bytes.
    fn read(&self, ctx: &ID3D11DeviceContext, src: &ID3D11Texture2D) -> Result<Vec<u8>> {
        unsafe {
            ctx.CopyResource(&self.tex, src);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            ctx.Map(&self.tex, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
            let pitch = mapped.RowPitch as usize;
            let base = mapped.pData as *const u8;
            let row = (self.width * 4) as usize;
            let mut out = vec![0u8; row * self.height as usize];
            for r in 0..self.height as usize {
                core::ptr::copy_nonoverlapping(
                    base.add(r * pitch),
                    out.as_mut_ptr().add(r * row),
                    row,
                );
            }
            ctx.Unmap(&self.tex, 0);
            Ok(out)
        }
    }
}

fn create_d3d_context() -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let mut device = None;
    let mut ctx = None;
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut ctx),
        )?;
    }
    Ok((
        device.ok_or_else(|| anyhow!("D3D11CreateDevice returned null device"))?,
        ctx.ok_or_else(|| anyhow!("D3D11CreateDevice returned null context"))?,
    ))
}

fn wrap_winrt_device(device: &ID3D11Device) -> Result<IDirect3DDevice> {
    let dxgi: IDXGIDevice = device.cast()?;
    let inspectable = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi)? };
    Ok(inspectable.cast()?)
}

fn capture_item_for_monitor(hmon: HMONITOR) -> Result<GraphicsCaptureItem> {
    let interop = factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
    // SAFETY: hmon is a live monitor handle from our enumeration.
    let item = unsafe { interop.CreateForMonitor::<GraphicsCaptureItem>(hmon)? };
    Ok(item)
}

/// Cheap capability probe: COM apartment + D3D11 device + WGC interop factory.
pub fn available() -> bool {
    let Ok(_com) = ComGuard::init_mta() else {
        return false;
    };
    create_d3d_context().is_ok() && capture_probe_ok()
}

fn capture_probe_ok() -> bool {
    factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>().is_ok()
}

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

/// Live capture graph. Held behind [`WgcSession`] so its `Drop` runs *before*
/// the COM guard: every WinRT/D3D reference must be released while the
/// apartment is still initialized, otherwise the pool worker races
/// `CoUninitialize` and faults.
struct WgcSessionInner {
    device: ID3D11Device,
    ctx: ID3D11DeviceContext,
    winrt_device: IDirect3DDevice,
    item: GraphicsCaptureItem,
    pool: Direct3D11CaptureFramePool,
    ctl: GraphicsCaptureSession,
    staging: Option<StagingTex>,
}

impl Drop for WgcSessionInner {
    fn drop(&mut self) {
        // Stop the capture thread and close the pool before the interface
        // references go away.
        let _ = self.ctl.Close();
        let _ = self.pool.Close();
    }
}

struct WgcSession {
    /// `Some` between construction and teardown; taken implicitly by drop.
    inner: Option<WgcSessionInner>,
    /// Physical crop within the captured texture: (x, y, w, h).
    crop: (u32, u32, u32, u32),
    /// Last successfully read crop buffer (reused when WGC throttles).
    last_buf: Vec<u8>,
    pool_size: (i32, i32),
    cursor: bool,
    /// Declared last => dropped last (after `inner`).
    _com: ComGuard,
}

impl WgcSession {
    fn new(rect: &Rect, monitor: &MonitorInfo, cursor: bool) -> Result<Self> {
        let com = ComGuard::init_mta()?;
        let (device, ctx) = create_d3d_context()?;
        let winrt_device = wrap_winrt_device(&device)?;
        let item = capture_item_for_monitor(monitor.hmon)?;

        let crop = monitor.map_crop_physical(rect)?;
        let size = item.Size()?;
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &winrt_device,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            2,
            size,
        )?;
        let ctl = pool.CreateCaptureSession(&item)?;
        // Best effort on pre-1903 hosts where the property is absent: capture
        // still works, the cursor is merely drawn.
        let _ = ctl.SetIsCursorCaptureEnabled(cursor);
        ctl.StartCapture()?;

        Ok(Self {
            inner: Some(WgcSessionInner {
                device,
                ctx,
                winrt_device,
                item,
                pool,
                ctl,
                staging: None,
            }),
            crop,
            last_buf: Vec::new(),
            pool_size: (size.Width, size.Height),
            cursor,
            _com: com,
        })
    }

    /// Drain pending frames into the staging texture; `Ok(true)` means at
    /// least one fresh frame was consumed.
    fn pull_latest(&mut self) -> Result<bool> {
        let inner = self.inner.as_mut().expect("capture session alive");
        let mut fresh = false;
        while let Ok(frame) = inner.pool.TryGetNextFrame() {
            let cs = frame.ContentSize()?;
            if (cs.Width, cs.Height) != self.pool_size {
                inner.pool.Recreate(
                    &inner.winrt_device,
                    DirectXPixelFormat::B8G8R8A8UIntNormalized,
                    2,
                    cs,
                )?;
                self.pool_size = (cs.Width, cs.Height);
            }
            let surface = frame.Surface()?;
            let access = surface.cast::<IDirect3DDxgiInterfaceAccess>()?;
            let tex: ID3D11Texture2D = unsafe { access.GetInterface()? };
            // (Re)build the staging texture whenever the source geometry
            // changes so CopyResource stays legal.
            let restage = match &inner.staging {
                None => true,
                Some(st) => st.width != cs.Width as u32 || st.height != cs.Height as u32,
            };
            if restage {
                inner.staging = Some(StagingTex::new(&inner.ctx, &tex)?);
            }
            self.last_buf = inner
                .staging
                .as_ref()
                .ok_or_else(|| anyhow!("staging missing"))?
                .read(&inner.ctx, &tex)?;
            fresh = true;
        }
        Ok(fresh)
    }

    /// Crop the staging buffer down to `self.crop` as a tightly-packed BGRA
    /// frame payload.
    fn cropped_payload(&self) -> Vec<u8> {
        let (cx, cy, cw, ch) = self.crop;
        let row = (cw * 4) as usize;
        let mut out = vec![0u8; row * ch as usize];
        if self.last_buf.is_empty() {
            return out;
        }
        // Staging width tracks the captured texture == last pool ContentSize.
        let src_row_pitch = self.pool_size.0.max(1) as usize * 4;
        let base = &self.last_buf;
        for r in 0..ch as usize {
            let src_off = ((cy as usize + r) * src_row_pitch) + (cx as usize * 4);
            let dst_off = r * row;
            let end = (src_off + row).min(base.len());
            if src_off < end {
                let n = end - src_off;
                out[dst_off..dst_off + n].copy_from_slice(&base[src_off..end]);
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Source
// ---------------------------------------------------------------------------

/// Windows.Graphics.Capture frame source over a fixed desktop rectangle.
///
/// The rect is resolved to the single monitor that fully contains it; callers
/// wanting cross-monitor rectangles should use [`native_source`], which falls
/// back to the GDI backend automatically.
pub struct WgcSource {
    rect: Rect,
    fps: u32,
    cursor: bool,
    max_frames: Option<u64>,
    session: Option<WgcSession>,
    started_at: Option<Instant>,
    emitted: u64,
}

impl WgcSource {
    pub fn new(rect: Rect, fps: u32) -> Result<Self> {
        Self::with_cursor(rect, fps, true)
    }

    pub fn with_cursor(rect: Rect, fps: u32, cursor: bool) -> Result<Self> {
        if fps == 0 {
            bail!("fps must be > 0");
        }
        let rect = validate_rect(rect)?;
        let monitors = enumerate_monitors()?;
        if choose_containing_monitor(&monitors, &rect).is_none() {
            bail!(
                "rect {:?} spans multiple monitors; a WGC item is per-monitor",
                rect
            );
        }
        Ok(Self {
            rect,
            fps,
            cursor,
            max_frames: None,
            session: None,
            started_at: None,
            emitted: 0,
        })
    }

    /// Whole primary monitor (physical pixels), the WGC analogue of
    /// `GdiSource::full_desktop`.
    pub fn primary_fullscreen(fps: u32) -> Result<Self> {
        let monitors = enumerate_monitors()?;
        let m = primary_monitor(&monitors)?;
        let r = Rect {
            x: m.gdi_rect.left,
            y: m.gdi_rect.top,
            w: (m.gdi_rect.right - m.gdi_rect.left).max(1) as u32,
            h: (m.gdi_rect.bottom - m.gdi_rect.top).max(1) as u32,
        };
        Self::new(r, fps)
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

impl FrameSource for WgcSource {
    fn spec(&self) -> FrameSpec {
        // Spec dimensions follow the physical crop: those are exactly the
        // frame dimensions the encoder will see.
        let dims = self.session.as_ref().map(|s| (s.crop.2, s.crop.3));
        let (w, h) = dims.unwrap_or_else(|| {
            // Pre-start estimate from the monitor table (same math as start).
            enumerate_monitors()
                .ok()
                .and_then(|ms| {
                    choose_containing_monitor(&ms, &self.rect)
                        .and_then(|m| m.map_crop_physical(&self.rect).ok())
                })
                .map(|c| (c.2, c.3))
                .unwrap_or((self.rect.w, self.rect.h))
        });
        FrameSpec {
            width: w,
            height: h,
            fps: self.fps,
        }
    }

    fn start(&mut self) -> Result<()> {
        debug_assert!(self.session.is_none());
        let monitors = enumerate_monitors()?;
        let monitor = choose_containing_monitor(&monitors, &self.rect)
            .ok_or_else(|| anyhow!("rect no longer contained in a single monitor"))?;
        let session = WgcSession::new(&self.rect, monitor, self.cursor)?;
        self.started_at = Some(Instant::now());
        self.session = Some(session);
        Ok(())
    }

    fn next_frame(&mut self) -> Option<Frame> {
        if let Some(max) = self.max_frames
            && self.emitted >= max
        {
            return None;
        }
        let started = self.started_at?;
        // Wall-clock pacing, identical contract to the GDI source. All pacing
        // math runs before the session borrow below.
        let target = started + self.frame_dur() * (self.emitted as u32 + 1);
        let now = Instant::now();
        if target > now {
            std::thread::sleep(target - now);
        }
        let pts_ms = started.elapsed().as_millis() as u64;
        let s = self.session.as_mut()?;
        // First frame: give the pool up to ~500 ms to deliver something.
        let mut have = false;
        for _ in 0..50 {
            have = s.pull_latest().unwrap_or(false);
            if have || !s.last_buf.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        if !have && !s.last_buf.is_empty() {
            // WGC throttles static screens; repeat the last crop with a fresh
            // pts to honor the requested fps cadence.
            have = true;
        }
        if !have {
            return None;
        }
        let data = s.cropped_payload();
        self.emitted += 1;
        Some(Frame {
            width: s.crop.2,
            height: s.crop.3,
            data,
            pts_ms,
        })
    }
}

// ---------------------------------------------------------------------------
// Backend selection
// ---------------------------------------------------------------------------

/// Best native backend for a capture request: WGC when the rect fits inside
/// one monitor and the host supports it, otherwise the GDI BitBlt source.
pub enum NativeSource {
    Wgc(WgcSource),
    Gdi(GdiSource),
}

impl NativeSource {
    pub fn name(&self) -> &'static str {
        match self {
            NativeSource::Wgc(_) => "wgcap",
            NativeSource::Gdi(_) => "gdi",
        }
    }
}

impl FrameSource for NativeSource {
    fn spec(&self) -> FrameSpec {
        match self {
            NativeSource::Wgc(s) => s.spec(),
            NativeSource::Gdi(s) => s.spec(),
        }
    }

    fn start(&mut self) -> Result<()> {
        match self {
            NativeSource::Wgc(s) => s.start(),
            NativeSource::Gdi(s) => s.start(),
        }
    }

    fn next_frame(&mut self) -> Option<Frame> {
        match self {
            NativeSource::Wgc(s) => s.next_frame(),
            NativeSource::Gdi(s) => s.next_frame(),
        }
    }
}

/// Resolve a user rect to the best available native [`FrameSource`].
pub fn native_source(rect: Rect, fps: u32, cursor: bool) -> Result<NativeSource> {
    let validated = validate_rect(rect)?;
    match WgcSource::with_cursor(validated, fps, cursor) {
        Ok(src) => Ok(NativeSource::Wgc(src)),
        Err(wgc_err) => {
            eprintln!("[wgcap] falling back to GDI: {wgc_err:#}");
            Ok(NativeSource::Gdi(GdiSource::new(validated, fps)?))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ----- pure geometry ---------------------------------------------------

    #[test]
    fn crop_mapping_identity_at_100_percent() {
        let m = MonitorInfo::synthetic((0, 0, 1920, 1080), (0, 0), (1920, 1080));
        let crop = m
            .map_crop_physical(&Rect {
                x: 100,
                y: 50,
                w: 320,
                h: 240,
            })
            .expect("maps");
        assert_eq!(crop, (100, 50, 320, 240));
    }

    #[test]
    fn crop_mapping_scales_under_dpi_virtualization() {
        // 150% display: GDI reports 2/3 of the physical pixels.
        let m = MonitorInfo::synthetic((0, 0, 1280, 720), (0, 0), (1920, 1080));
        let crop = m
            .map_crop_physical(&Rect {
                x: 640,
                y: 360,
                w: 640,
                h: 360,
            })
            .expect("maps");
        assert_eq!(crop, (960, 540, 960, 540));
    }

    #[test]
    fn crop_mapping_handles_negative_origins() {
        // Left-of-primary monitor with negative GDI origin.
        let m = MonitorInfo::synthetic((-1920, 0, 1920, 1080), (-1920, 0), (1920, 1080));
        let crop = m
            .map_crop_physical(&Rect {
                x: -1820,
                y: 100,
                w: 320,
                h: 240,
            })
            .expect("maps");
        assert_eq!(crop, (100, 100, 320, 240));
    }

    #[test]
    fn chooser_prefers_fully_contained_monitor() {
        let left = MonitorInfo::synthetic((-1920, 0, 1920, 1080), (-1920, 0), (1920, 1080));
        let right = MonitorInfo::synthetic((0, 0, 2560, 1440), (0, 0), (2560, 1440));
        let monitors = vec![left, right];
        let inside_right = choose_containing_monitor(
            &monitors,
            &Rect {
                x: 100,
                y: 100,
                w: 800,
                h: 600,
            },
        )
        .expect("contained");
        assert_eq!(inside_right.gdi_rect.left, 0);
        let inside_left = choose_containing_monitor(
            &monitors,
            &Rect {
                x: -1900,
                y: 10,
                w: 400,
                h: 300,
            },
        )
        .expect("contained");
        assert_eq!(inside_left.gdi_rect.left, -1920);
        assert!(
            choose_containing_monitor(
                &monitors,
                &Rect {
                    x: -100,
                    y: 100,
                    w: 800,
                    h: 600
                },
            )
            .is_none()
        );
    }

    // ----- backend decision ------------------------------------------------

    #[test]
    fn native_source_falls_back_when_region_spans_monitors() {
        let monitors = enumerate_monitors().expect("enumeration works");
        if monitors.len() < 2 {
            eprintln!("[skip] single-monitor host: span-fallback not exercisable");
            return;
        }
        let first = &monitors[0];
        let last = monitors.last().unwrap();
        let spanned = Rect {
            x: first.gdi_rect.left.min(last.gdi_rect.left) - 10,
            y: first.gdi_rect.top.min(last.gdi_rect.top),
            w: 4096,
            h: 400,
        };
        let src = native_source(spanned, 30, true).expect("fallback succeeds");
        assert_eq!(src.name(), "gdi");
    }

    // ----- live capture (real GPU) ------------------------------------------

    fn require_wgc() -> bool {
        if !available() {
            eprintln!("[skip] Windows.Graphics.Capture unavailable on this host");
            return false;
        }
        true
    }

    #[test]
    fn region_session_produces_requested_dimensions() {
        if !require_wgc() {
            return;
        }
        let rect = Rect {
            x: 200,
            y: 150,
            w: 480,
            h: 360,
        };
        let mut src = WgcSource::new(rect, 30)
            .expect("region inside primary monitor")
            .with_max_frames(2);
        let spec = src.spec();
        src.start().expect("session starts");
        for i in 0..2 {
            let f = src.next_frame().expect("frame under max_frames");
            assert_eq!((f.width, f.height), (spec.width, spec.height));
            assert_eq!(f.data.len(), (spec.width * spec.height * 4) as usize);
            assert!(f.pts_ms >= (i as u64 * 1000) / 30);
        }
        assert!(src.next_frame().is_none());
    }

    #[test]
    fn fullscreen_session_matches_spec_and_monotonic_pts() {
        if !require_wgc() {
            return;
        }
        let mut src = WgcSource::primary_fullscreen(60)
            .expect("fullscreen source")
            .with_max_frames(3);
        let spec = src.spec();
        assert!(spec.width > 0 && spec.height > 0);
        src.start().expect("session starts");
        let mut last_pts = -1i64;
        for _ in 0..3 {
            let f = src.next_frame().expect("frame");
            assert_eq!((f.width, f.height), (spec.width, spec.height));
            assert_eq!(f.data.len(), (spec.width * spec.height * 4) as usize);
            assert!((f.pts_ms as i64) > last_pts);
            last_pts = f.pts_ms as i64;
        }
        assert!(src.next_frame().is_none());
    }

    #[test]
    fn cursor_capture_can_be_disabled() {
        if !require_wgc() {
            return;
        }
        let rect = Rect {
            x: 100,
            y: 100,
            w: 320,
            h: 240,
        };
        let mut src = WgcSource::with_cursor(rect, 30, false)
            .expect("source")
            .with_max_frames(1);
        src.start().expect("session starts");
        let s = src.session.as_ref().expect("live session");
        assert!(
            !s.inner
                .as_ref()
                .expect("inner")
                .ctl
                .IsCursorCaptureEnabled()
                .unwrap()
        );
        let f = src.next_frame().expect("one frame");
        assert_eq!((f.width, f.height), (320, 240));
    }

    #[test]
    fn region_content_parity_with_gdi_source() {
        if !require_wgc() {
            return;
        }
        let rect = Rect {
            x: 200,
            y: 150,
            w: 480,
            h: 360,
        };
        let mut wgc = WgcSource::new(rect, 30).expect("source").with_max_frames(1);
        wgc.start().expect("start");
        let ours = wgc.next_frame().expect("frame");

        let mut gdi = GdiSource::new(rect, 30).expect("gdi source");
        gdi.start().expect("gdi start");
        let theirs = gdi.next_frame().expect("gdi frame");
        drop(gdi);

        assert_eq!(
            (ours.width, ours.height),
            (theirs.width, theirs.height),
            "dimension parity between native backends"
        );
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
        let (mw, mg) = (mean(&ours.data), mean(&theirs.data));
        for c in 0..4 {
            let diff = (mw[c] - mg[c]).abs();
            assert!(
                diff < 12.0,
                "channel {c} mean differs by {diff}: wgc={mw:?} gdi={mg:?}"
            );
        }
    }

    #[test]
    fn start_stop_cycle_is_clean_across_50_iterations() {
        if !require_wgc() {
            return;
        }
        for i in 0..50 {
            let mut src = WgcSource::primary_fullscreen(120)
                .expect("source constructs")
                .with_max_frames(1);
            src.start()
                .unwrap_or_else(|e| panic!("iter {i}: start failed: {e}"));
            let f = src
                .next_frame()
                .unwrap_or_else(|| panic!("iter {i}: no frame"));
            assert!(f.width > 0 && f.height > 0);
            drop(src);
        }
    }
}
