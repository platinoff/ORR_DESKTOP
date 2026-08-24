# NEXT session prompt — ORR Desktop Recorder

Open with the HANDOFF (`docs/HANDOFF_NEW_SESSION.md`) for state and build notes.

Candidate bands, in rough priority:

1. **First CI verification** — after the GitHub push, confirm the first
   windows-gnu CI run is green (fmt/clippy/test); fix runner-specific issues
   (MSYS2 PATH, dlltool) if any.
2. **SW encode perf (P6)** — slice-based multithreading (`max_slice_len`) and
   SIMD BGRA→I420; closes the ~18–20 fps @ 1080p gap.
3. **Hardware encoders (P6)** — vendor APIs in-process (DXGI/D3D11 interop),
   fallback chain mirrors Phase 0 probe order.
4. **Auto-update check** — against GitHub releases (needs releases to exist).
5. **ffmpeg bundling story** — installer or first-run wizard that finds/
   downloads ffmpeg for the `ORR_LEGACY=1` path only (native path needs none).
6. **Screenshot/GIF assets** — the last open Phase 1 checkbox (owner-provided
   tray UI capture for the README).
7. **Multi-monitor selector polish** — overlay per monitor, DPI awareness,
   Escape-to-cancel affordance.

Drain discipline (GSV kit): S0 disk → HANDOFF → one band → fmt → tests →
one commit.
