//! Public entry point for ISO → Games-on-Demand conversion.
//!
//! The actual work is split across:
//! - `prepare` for source analysis and layout sizing
//! - `core` for the shared conversion loop
//! - `sink_host` / `sink_fatx` for transport-specific outputs
//!
//! See `NOTICE` for the upstream sources this code descends from.

use std::path::Path;

use crate::error::Result;
use crate::volume::FatxVolume;

use super::ContentType;
use super::core::run_conversion;
use super::prepare::prepare_source;
use super::sink_fatx::FatxSink;
use super::sink_host::HostFsSink;

/// Progress callback shape: `(stage, current, total)` where `stage` is one
/// of `"parts"`, `"mht"`, `"header"`.
pub type ProgressFn<'a> = &'a mut dyn FnMut(&str, u64, u64);

/// How to size the output GoD relative to the source ISO.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TrimMode {
    /// Walk the existing directory tree, find the max `(offset + size)`,
    /// and pack only that many bytes. Preserves any mastered holes inside
    /// the XDVDFS layout while trimming trailing slack after the highest
    /// file extent.
    PreserveLayout,
    /// Pack every byte from the start of the data partition to the end of
    /// the source file. Larger output, but useful when the directory tree
    /// is suspect.
    None,
    /// Rebuild the XDVDFS image densely as a virtual layout and stream
    /// those bytes directly through the GoD pipeline.
    #[default]
    Compact,
}

/// Knobs the caller can adjust per conversion.
#[derive(Default)]
pub struct ConvertOptions<'a> {
    pub trim: TrimMode,
    pub game_title: Option<&'a str>,
    pub dry_run: bool,
    pub progress: Option<ProgressFn<'a>>,
    pub should_abort: Option<&'a dyn Fn() -> bool>,
}

/// Metadata extracted from the source ISO and the resulting layout sizing.
#[derive(Debug, Clone, Copy)]
pub struct ConvertReport {
    pub title_id: u32,
    pub media_id: u32,
    pub content_type: ContentType,
    pub part_count: u64,
    pub block_count: u64,
    pub data_size: u64,
}

/// Convert an Xbox 360 / original-Xbox ISO into a Games-on-Demand package.
pub fn convert_iso<'a>(
    source_iso: &Path,
    dest_dir: &Path,
    opts: &'a mut ConvertOptions<'a>,
) -> Result<ConvertReport> {
    let source = prepare_source(source_iso, opts)?;
    if opts.dry_run {
        return Ok(source.report);
    }

    let mut sink = HostFsSink::new(dest_dir);
    run_conversion(&source, &mut sink, opts, "convert_iso")
}

/// Convert an ISO directly into a Games-on-Demand package rooted at a FATX volume.
pub fn convert_iso_to_fatx<'a, T>(
    source_iso: &Path,
    vol: &mut FatxVolume<T>,
    dest_dir: &str,
    opts: &'a mut ConvertOptions<'a>,
) -> Result<ConvertReport>
where
    T: std::io::Read + std::io::Seek + std::io::Write,
{
    let source = prepare_source(source_iso, opts)?;
    if opts.dry_run {
        return Ok(source.report);
    }

    let mut sink = FatxSink::new(vol, dest_dir);
    run_conversion(&source, &mut sink, opts, "convert_iso_to_fatx")
}
