# ORR Desktop Recorder — HANDOFF

Windows system-tray screen recorder in pure Rust. ffmpeg is an external runtime
dependency (never vendored); the app locates it via `ORR_FFMPEG`, then `%PATH%`.

## State (last session)

- Working end-to-end: tray + menu, fullscreen and rubber-band region capture,
  graceful stop (ffmpeg `q` on stdin), MP4 output.
- Verified: fullscreen 3 s encode (AMF GPU, h264 1920x1080@30), exact region
  crop 960x720, GUI smoke tests alive. Zero rustc/clippy warnings.
- Pure-Rust rewrite started (see docs/ROADMAP.md): P1 pipeline trait seam +
  pump, P2 native GDI BitBlt source with gdigrab crop-parity test. Legacy
  ffmpeg spawn remains the default record path until P4.

## Layout

| File | Role |
|------|------|
| `src/main.rs` | tray + winit loop, `UserEvent{Selector,Tick,Finished,Menu}` |
| `src/recorder.rs` | encoder detect/probe (`detect_encoders`, `probe_encoder`), `build_command`, `start`, graceful stop |
| `src/selector.rs` | Win32 `WS_EX_LAYERED` rubber-band overlay |
| `src/settings.rs` | persisted settings |

## CLI

```
orr-desktop-recorder.exe            # GUI (tray)
  probe                             # list encoders + chosen pick
  cli-rec [seconds] [out.mp4]       # fullscreen
  cli-area X Y W H [seconds] [out]  # region crop
  --version / --help
```

`ORR_PRINT_CMD=1` prints the generated ffmpeg argv instead of running.

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
