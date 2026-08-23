//! Capture stage. Native sources land here per the rewrite plan:
//! `gdi.rs` (P2), `wgcap.rs` (P3) — both landed. No legacy wrapper remains;
//! recording runs through the native in-process pipeline (`src/native.rs`).
//! The `probe` subcommand still reports legacy ffmpeg encoders when
//! ORR_FFMPEG is set (diagnostics only).

pub mod gdi;
pub mod wgcap;
