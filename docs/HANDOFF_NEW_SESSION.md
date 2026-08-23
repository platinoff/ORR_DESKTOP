# ORR Desktop Recorder — HANDOFF

Windows system-tray screen recorder in Rust. **Default record path is now the
native in-process pipeline** (WGC/GDI → OpenH264 → muxide MP4, zero external
processes). The legacy ffmpeg child-process path remains behind
`ORR_LEGACY=1` (ffmpeg located via `ORR_FFMPEG`, then `%PATH%`).

## State (last session)

- Native pipeline live end-to-end (P4): `native::run_blocking/spawn_session`
  wires `capture::wgcap::native_source` → `encode::sw::SwH264Encoder` →
  `mux::mp4::Mp4Muxer`. Tray + both CLI commands use it by default.
- Acceptance green: 32/32 tests — ftyp-first / moov-before-mdat / duration ==
  frames/fps ±1 box-walk test, duplicate-PTS bump, Annex-B keyframe-first,
  BT.601 fixed-point I420 vectors, and an ffmpeg.exe process-count-stability
  e2e (no child spawned or leaked).
- Known perf gap (documented, P6 follow-up): SW encode sustains ~18–20 fps at
  1080p release on the dev host — single-slice OpenH264 (SM_SINGLE_SLICE
  blocks its internal threading) + scalar BGRA→I420. Candidates: slice-based
  multithreading via `max_slice_len`, converter SIMD. Legacy path unaffected.
- Pure-Rust rewrite state: P1 seam, P2 GDI source, P3 WGC source
  (`capture/wgcap.rs`, free-threaded pool + D3D11 staging readback, DEVMODE
  physical-crop mapping, cursor toggle, WGC→GDI fallback), P4 SW encode +
  MP4 mux + native default (this band). HW encoders = P6.

## Layout

| File | Role |
|------|------|
| `src/main.rs` | tray + winit loop; `RunningSession::{Legacy,Native}`; native default with `ORR_LEGACY=1` escape hatch |
| `src/native.rs` | native session orchestration: rect even-ing, desktop bounds, stop-flag source wrapper, `run_blocking`/`spawn_session` |
| `src/recorder.rs` | legacy ffmpeg detect/probe/build_command/start/graceful stop; `Quality` presets (cq + bitrate_mbps) |
| `src/pipeline.rs` | pure-Rust seam: `FrameSource`/`VideoEncoder`/`Muxer` traits + `run()` pump |
| `src/capture/gdi.rs` | BitBlt frame source (P2) |
| `src/capture/wgcap.rs` | Windows.Graphics.Capture source (P3) + WGC→GDI `native_source()` picker |
| `src/encode/sw.rs` | OpenH264 encoder stage (P4): in-house BGRA→I420, bitrate RC from Quality preset, screen-content usage |
| `src/mux/mp4.rs` | muxide MP4 muxer stage (P4): fast-start, strictly-increasing PTS guard |
| `src/selector.rs` | Win32 `WS_EX_LAYERED` rubber-band overlay |
| `src/settings.rs` | persisted settings |

## CLI

```
orr_desktop.exe                     # GUI (tray)
  probe                             # legacy ffmpeg encoder report
  cli-rec [seconds] [out.mp4]       # fullscreen (native by default)
  cli-area X Y W H [seconds] [out]  # region crop (area rounded to even)
  --version / --help
```

Env: `ORR_LEGACY=1` → ffmpeg child-process path; `ORR_FFMPEG` → legacy binary;
`ORR_PRINT_CMD=1` prints the generated legacy argv instead of running.

## Build

Rust GNU toolchain (`stable-x86_64-pc-windows-gnu`). Broken rustup self-contained
dlltool was replaced by w64devkit v2.9.1 binutils copied to
`C:\Users\plati\.cargo\bin` (`dlltool.exe`, `x86_64-w64-mingw32-dlltool.exe`,
`as.exe`) — keep those if linking breaks.

ffmpeg for testing currently lives under `%TEMP%\opencode\ff\...` (volatile).
If missing: `winget install Gyan.FFmpeg` or set `ORR_FFMPEG` to any ffmpeg.exe.

## Encoders

Runtime probe order after sort: NVENC(0), QSV(1), AMF(2), x264(3). Hardware
encoders are validated with a real test encode (`color` lavfi source → null
muxer) before use. This machine (AMD iGPU only) auto-picks **AMF**.

## Known gaps (NEXT candidates)

See `docs/NEXT_SESSION_PROMPT.md`.
