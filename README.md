# ORR Desktop Recorder

Windows screen recorder that lives in the system tray. Pick fullscreen or
rubber-band a region, choose quality/CPU/FPS, hit record — output is MP4.

Built in **Rust** (windows-gnu toolchain). Recording is currently driven by an
external `ffmpeg.exe` process; the **100% pure-Rust pipeline** (native capture,
encode, mux — no side applications) is the target architecture, see
[ROADMAP](docs/ROADMAP.md) and [ARCHITECTURE_PURE_RUST](docs/ARCHITECTURE_PURE_RUST.md).

## Features

- Tray icon menu: record fullscreen / select region, quality, CPU usage, FPS.
- Rubber-band region selector overlay (Win32 layered window).
- Hardware encoders auto-detected and **runtime-probed**: NVENC → QSV → AMF,
  falling back to x264. A broken driver entry is filtered out by a real test
  encode before use.
- Graceful stop (flushes the encoder, valid MP4 every time).
- Persisted settings.
- Scriptable CLI for automation (`probe`, `cli-rec`, `cli-area`).

## Requirements

- Windows 10/11 x64.
- `ffmpeg` in `%PATH%`, or set `ORR_FFMPEG` to the full path of `ffmpeg.exe`
  (any recent build works; e.g. `winget install Gyan.FFmpeg`).
- Rust `stable-x86_64-pc-windows-gnu` to build from source.

## Build

```powershell
cargo build --release
# binary: target\release\orr-desktop-recorder.exe
```

If linking fails with dlltool errors, replace rustup's self-contained binutils
with w64devkit's (`dlltool.exe`, `x86_64-w64-mingw32-dlltool.exe`, `as.exe`)
copied into `%USERPROFILE%\.cargo\bin`.

## Usage

Run without arguments for the tray app. CLI subcommands:

| Command | Effect |
|---------|--------|
| `probe` | List detected encoders + the auto pick |
| `cli-rec [seconds] [out.mp4]` | Record fullscreen |
| `cli-area X Y W H [seconds] [out.mp4]` | Record a region crop |
| `--version` / `--help` | Meta |

Set `ORR_PRINT_CMD=1` to print the generated ffmpeg argv instead of recording.

## Docs

| Doc | Contents |
|-----|----------|
| [docs/CONCEPT.md](docs/CONCEPT.md) | Vision, goals, non-goals |
| [docs/ROADMAP.md](docs/ROADMAP.md) | Phased plan to 1.0 |
| [docs/ARCHITECTURE_PURE_RUST.md](docs/ARCHITECTURE_PURE_RUST.md) | Target architecture: no external apps, all-Rust pipeline |
| [docs/HANDOFF_NEW_SESSION.md](docs/HANDOFF_NEW_SESSION.md) | Contributor handoff |

## Status

Pre-1.0. The ffmpeg-driven MVP is functional and verified (GPU AMF encode,
region crops); the pure-Rust rewrite is designed but not started.

License: TBD before first public release.
