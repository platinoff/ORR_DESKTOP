# ORR Desktop Recorder — Roadmap

Phased bands. Each phase is one drain: S0 → implement/docs → fmt →
`cargo test` → one commit.

## Phase 0 — ffmpeg-driven MVP ✅ (shipped)

- [x] Tray + menu (record fullscreen / region, quality, CPU, FPS).
- [x] Rubber-band region selector (layered overlay).
- [x] Encoder detect + runtime probe, priority NVENC → QSV → AMF → x264.
- [x] Graceful stop producing valid MP4.
- [x] CLI: `probe`, `cli-rec`, `cli-area`, `--version`, `--help`.
- [x] Settings persistence; verified AMF GPU encode + exact crops on AMD iGPU.

## Phase 1 — Publication readiness

- [ ] Docs set final review (README/CONCEPT/ROADMAP/ARCHITECTURE).
- [ ] GitHub repo `platinoff/ORR_DESKTOP`: create, push initial commit.
- [ ] CI sketch: fmt --check + clippy --all-targets + test on windows-gnu.
- [ ] Screenshot/GIF assets for README.
- [ ] License decision (MIT OR Apache-2.0) + LICENSE files.

## Phase 2 — Native capture

- [x] **P1** pipeline seam: `FrameSource`/`VideoEncoder`/`Muxer` traits +
      `run()` pump (`src/pipeline.rs`); legacy ffmpeg behind
      `capture::ffspawn` wrapper, still the default until P4.
- [x] **P2** GDI `BitBlt` source (`src/capture/gdi.rs`): BGRA frames + pts,
      wall-clock pacing (blt duration cannot drift fps), virtual-screen
      clamp for multi-monitor bounds; crop **parity test vs real gdigrab**
      (same rect → same geometry; channel means within tolerance).
- [x] **P3** Windows.Graphics.Capture primary source via `windows-rs`
      (`capture/wgcap.rs`): free-threaded frame pool, D3D11 staging
      readback, physical-pixel crop mapping (DEVMODE-based, DPI-safe),
      negative-origin monitors, cursor toggle, `native_source()` WGC→GDI
      fallback; x50 start/stop leak-cycle test.

## Phase 3 — Native encode + mux (no side applications)

- [x] `trait VideoEncoder` (feed BGRA frame → bitstream) with hardware
      discovery preserved (`probe` stays for the legacy path).
- [x] Software encoder decision: **in-process C via `openh264` crate**
      (bundled source build, linked into the binary; no external process).
      In-house BGRA→I420 conversion (BT.601 limited range, fixed point).
- [ ] Hardware encoders via vendor APIs in-process where feasible
      (DXGI/D3D11 interop); graceful fallback chain mirrors Phase 0.
- [x] MP4 muxing in-crate (`muxide` crate): fast-start moov-before-mdat,
      Annex-B samples accepted directly, exact duration on finalize.
      Default record path is now native (`ORR_LEGACY=1` restores ffmpeg);
      the ffmpeg spawn still exists behind the legacy flag only.
- [x] Acceptance: native pipeline runs with **zero** external executables
      (Toolhelp process-count test), MP4 byte-level sanity (ftyp first,
      moov before mdat, duration == frames/fps ±1).

## Phase 4 — Recording UX ✅ (shipped)

- [x] Pause / resume with tray state change.
- [x] Audio: microphone and/or WASAPI loopback mixed into the MP4.
- [x] Bitrate/quality presets surfaced in the menu; output-folder picker.

## Phase 5 — Distribution ✅ (shipped)

- [x] Single-file release builds (release profile already LTO+stripped).
- [x] Installer or portable zip (`dist/orr_desktop_portable.zip` with bundled MinGW runtime DLLs).
- [ ] Auto-update check against GitHub releases.
