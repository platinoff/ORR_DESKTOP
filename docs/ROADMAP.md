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

- [ ] `windows-rs` based frame source: Windows.Graphics.Capture primary,
      GDI `BitBlt` fallback for older hosts.
- [ ] DPI-aware coordinates; multi-monitor origin handling parity with the
      ffmpeg `gdigrab` offsets.
- [ ] `trait FrameSource` (BGRA frames + timestamp), ffmpeg path kept as a
      reference implementation behind the same trait until Phase 3 lands.
- [ ] Acceptance: pixel-exact crop vs `cli-area` output on a test pattern.

## Phase 3 — Native encode + mux (no side applications)

- [ ] `trait VideoEncoder` (feed BGRA frame → bitstream) with hardware
      discovery preserved (probe stays).
- [ ] Software encoder decision: pure-Rust AV1 (`rav1e`) vs in-process C
      (`openh264` bundled via -sys crate). Hard rule in both cases: **no
      external process**; see ARCHITECTURE_PURE_RUST.md tradeoff table.
- [ ] Hardware encoders via vendor APIs in-process where feasible
      (DXGI/D3D11 interop); graceful fallback chain mirrors Phase 0.
- [ ] MP4 muxing in-crate (`mp4-muxer` crate candidate); remove the
      `std::process::Command` ffmpeg spawn entirely.
- [ ] Acceptance: binary runs with **zero** external executables on a clean
      machine; byte-level sanity of output MP4 (moov present, duration exact).

## Phase 4 — Recording UX

- [ ] Pause / resume with tray state change.
- [ ] Audio: microphone and/or WASAPI loopback mixed into the MP4.
- [ ] Bitrate/quality presets surfaced in the menu; output-folder picker.

## Phase 5 — Distribution

- [ ] Single-file release builds (release profile already LTO+stripped).
- [ ] Installer or portable zip; first-run wizard only if ffmpeg-era
      compat shim is still needed (target: not needed after Phase 3).
- [ ] Auto-update check against GitHub releases.
