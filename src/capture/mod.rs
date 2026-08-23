//! Capture stage. Native sources land here per the rewrite plan:
//! `gdi.rs` (P2), `wgcap.rs` (P3) — both landed. Until P4 the legacy wrapper
//! remains the default record path.

pub mod ffspawn;
pub mod gdi;
pub mod wgcap;
