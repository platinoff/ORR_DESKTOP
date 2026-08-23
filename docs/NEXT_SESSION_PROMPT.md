# NEXT session prompt — ORR Desktop Recorder

Open with the HANDOFF (`docs/HANDOFF_NEW_SESSION.md`) for state and build notes.

Candidate bands, in rough priority:

1. **ffmpeg bundling story** — installer or first-run wizard that finds/downloads
   ffmpeg (winget `Gyan.FFmpeg`), so `ORR_FFMPEG` is not required.
2. **Pause / resume** — menu item + ffmpeg segment/pause handling; tray icon
   state change while paused.
3. **Audio capture** — microphone and/or system loopback (dshow) mixed into the
   MP4; settings toggles.
4. **Multi-monitor selector polish** — overlay per monitor, DPI awareness,
   Escape-to-cancel affordance.
5. **Settings UX** — bitrate/quality presets surfaced in the tray menu
   (currently CPU% + FPS only), output-folder picker.
6. **GitHub remote** — create `platinoff/ORR_DESKTOP`, push, add CI (fmt +
   clippy + test on windows-gnu).

Drain discipline (GSV kit): S0 disk → HANDOFF → one band → fmt → tests →
one commit.
