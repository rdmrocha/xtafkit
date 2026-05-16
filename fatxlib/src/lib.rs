//! # fatxlib
//!
//! A Rust library for reading and writing Xbox FATX / Xbox 360 XTAF file systems.
//!
//! FATX (original Xbox) and XTAF (Xbox 360) are variants of the same filesystem.
//! This library provides user-space access to both — from raw disk images
//! or physical devices (e.g., Xbox HDDs connected via USB on macOS).
//!
//! ## Quick Start
//!
//! ```no_run
//! use std::fs::OpenOptions;
//! use fatxlib::volume::FatxVolume;
//!
//! let file = OpenOptions::new()
//!     .read(true)
//!     .write(true)
//!     .open("/dev/rdisk4")
//!     .unwrap();
//!
//! let mut vol = FatxVolume::open(file, 0, 0).unwrap();
//! let entries = vol.read_root_directory().unwrap();
//! for entry in &entries {
//!     println!("{}", entry.filename());
//! }
//! ```

pub mod content_types;
pub mod display;
pub mod error;
pub mod partition;
pub mod platform;
pub mod stfs;
pub mod titles;
pub mod types;
pub mod volume;
pub mod xuids;

pub use error::{FatxError, Result};
pub use partition::{detect_xbox_partitions, format_size, DetectedPartition};
pub use types::*;
pub use volume::{FatxVolume, VolumeStats};
