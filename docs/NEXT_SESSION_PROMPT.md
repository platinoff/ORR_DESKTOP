# NEXT session prompt — ORR Desktop Recorder

Open with the HANDOFF (`docs/HANDOFF_NEW_SESSION.md`) for state and build notes.

Candidate bands, in rough priority:

1. **Hardware encoders (P6)** — vendor APIs in-process (DXGI/D3D11 interop),
   fallback chain mirrors Phase 0 probe order. Now clearly the main lever:
   SW encode is ~14–15 ms/frame even for static 1080p content.
2. **Capture/pump overlap** — the pump serializes WGC readback (~2–3 ms),
   convert (~1.5 ms) and encode; overlapping readback with encode of the
   previous frame would hide most of the non-encode cost.
3. **Auto-update check** — against GitHub releases (needs releases to exist).
4. **ffmpeg bundling story** — installer or first-run wizard that finds/
   downloads ffmpeg for the `ORR_LEGACY=1` path only (native path needs none).
5. **Screenshot/GIF assets** — the last open Phase 1 checkbox (owner-provided
   tray UI capture for the README).
6. **Multi-monitor selector polish** — overlay per monitor, DPI awareness,
   Escape-to-cancel affordance.

SW perf follow-up note: slice MT is on (`max_slice_len` + threads, see
HANDOFF); further SW gains would need converter SIMD (marginal now) or an
openh264-sys-level SM_AUTO_SLICE patch (upstream work).

Drain discipline (GSV kit): S0 disk → HANDOFF → one band → fmt → tests →
one commit.
