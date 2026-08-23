//! Legacy ffmpeg-driven record path.
//!
//! The current implementation grabs the desktop and encodes by spawning an
//! external `ffmpeg.exe` (`recorder::build_command` + `start`). It stays the
//! default until P4 switches the default to the in-process pipeline; this
//! module is deleted at P5 (zero-external-executables acceptance).
//!
//! Ticket map: P1 wraps it, P4 replaces it, P5 removes it.

pub use crate::recorder::{build_command, graceful_stop, start};
