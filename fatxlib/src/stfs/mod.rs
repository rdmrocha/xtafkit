//! STFS (Secure Transacted File System) container format.
//!
//! Xbox 360 packages — CON (signed by console), LIVE (signed by Microsoft Live),
//! and PIRS (signed Microsoft installer) — share the same on-disk layout.
//! This module groups all STFS parsing and extraction logic.

pub mod block_translator;
pub mod extract;
pub mod file_entry;
pub mod header;
pub mod volume_descriptor;

pub use extract::StfsPackage;
pub use header::{MIN_HEADER_BYTES, StfsHeader, parse_header};
