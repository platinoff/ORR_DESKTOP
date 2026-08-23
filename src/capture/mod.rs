//! Capture stage. Native sources land here per the rewrite plan:
//! `gdi.rs` (P2, landed), `wgcap.rs` (P3). Until P4 the legacy wrapper
//! remains the default record path.

pub mod ffspawn;
pub mod gdi;
