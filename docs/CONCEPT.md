# ORR Desktop Recorder — Concept

## Problem

Screen recording on Windows means either heavyweight suites (OBS), nagware, or
raw CLI tooling. A person who wants "record this region now, save MP4" has no
light, honest, tray-resident option.

## Vision

A recorder that behaves like a utility, not an application:

- **Tray-first.** No main window. The tray menu is the whole UI.
- **Zero-friction start.** Two clicks from idea to recording.
- **Honest output.** Exactly the region you selected, at the FPS you picked,
  a valid MP4 every single time (graceful stop, encoder flush).
- **Scriptable.** Every GUI action is available as a CLI subcommand.

## Goals

1. Fullscreen + rubber-band region capture.
2. Quality / CPU-usage / FPS controls with sensible presets.
3. Hardware acceleration when available (NVENC / QSV / AMF), auto-probed with
   runtime validation — never a silent fallback to a broken driver.
4. Settings persist across sessions.
5. Single small binary; minimal dependencies; no installer required.

## Non-goals

- Video editing, annotations, effects.
- Streaming/broadcast (RTMP etc.).
- Cross-platform support (Windows-first; abstractions should not preclude
  ports later, but nothing is designed for it up front).
- A plugin system.

## Direction: self-contained by construction

The MVP drives `ffmpeg.exe` as an external process — fast to build, but it
makes the app a *shell* around someone else's binary. The product direction
is a **self-contained Rust pipeline**: capture, encode, and muxing as in-crate
modules behind traits, with **no side applications** launched at runtime.
Rationale, crate evaluation and migration steps:
[ARCHITECTURE_PURE_RUST.md](ARCHITECTURE_PURE_RUST.md).

## Users

- The author first (daily-driver utility).
- Developers/power users who prefer CLI automation of recording.
