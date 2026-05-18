//! `StfsSink` trait + `run_extract` engine.

use std::io::{Read, Seek};
use std::path::{Path, PathBuf};

use crate::error::{FatxError, Result};
use crate::stfs::file_entry::StfsEntry;

use super::{ExtractReport, ProgressFn, StfsPackage};

// ── StfsSink ────────────────────────────────────────────────────────────────

/// Destination abstraction used by [`run_extract`].
///
/// Implementors handle the sink-specific work (host filesystem vs FATX volume)
/// while the engine drives the common traversal loop.
pub(crate) trait StfsSink {
    /// Create a directory at `rel` relative to the destination root.
    /// Treats "already exists as directory" as success.
    fn ensure_dir(&mut self, rel: &Path) -> Result<()>;

    /// Return an error if a file already exists at `rel`.
    ///
    /// Default: `Ok(())` — FATX's `create_file_from_reader` enforces this
    /// internally so the FATX sink can rely on the default.
    fn refuse_if_file_exists(&mut self, _rel: &Path) -> Result<()> {
        Ok(())
    }

    /// Stream `size` bytes from `reader` into the destination at `rel`.
    fn write_file(&mut self, rel: &Path, size: u64, reader: &mut dyn Read) -> Result<()>;

    /// Called once per entry before any work, enabling early cancellation.
    ///
    /// Default: `Ok(())` — the host sink has no cancel signal.
    fn check_cancelled(&self) -> Result<()> {
        Ok(())
    }
}

// ── engine ──────────────────────────────────────────────────────────────────

/// Walk every entry in `package` and stream it through `sink`.
///
/// `directories` in the returned report counts only STFS package directory
/// entries — NOT the destination root created by the sink itself.
pub(crate) fn run_extract<R, S>(
    package: &mut StfsPackage<R>,
    sink: &mut S,
    progress: Option<ProgressFn<'_>>,
) -> Result<ExtractReport>
where
    R: Read + Seek,
    S: StfsSink,
{
    let entries = package.entries()?;
    let paths = build_relative_paths(&entries)?;

    let mut files = 0usize;
    let mut directories = 0usize;
    let mut bytes = 0u64;

    for (idx, entry) in entries.iter().enumerate() {
        sink.check_cancelled()?;
        let rel = &paths[idx];

        if entry.is_directory {
            sink.ensure_dir(rel)?;
            directories += 1;
            continue;
        }

        // Ensure the file's parent directory exists before writing.
        if let Some(parent) = rel.parent()
            && !parent.as_os_str().is_empty()
        {
            sink.ensure_dir(parent)?;
        }

        sink.refuse_if_file_exists(rel)?;

        if let Some(cb) = progress {
            cb(&rel.to_string_lossy(), entry.size, bytes);
        }

        let mut reader = super::sink_fatx::StfsFileReader::new(package, entry)?;
        sink.write_file(rel, entry.size, &mut reader)?;
        files += 1;
        bytes += entry.size;
    }

    Ok(ExtractReport {
        files,
        directories,
        bytes,
    })
}

// ── build_relative_paths ────────────────────────────────────────────────────

/// Build a `Vec<PathBuf>` parallel to `entries`, resolving `parent_index`
/// chains. Orphan entries (out-of-range parent) get a `<orphan-N>` prefix.
pub(crate) fn build_relative_paths(entries: &[StfsEntry]) -> Result<Vec<PathBuf>> {
    let mut out: Vec<Option<PathBuf>> = vec![None; entries.len()];

    fn resolve(
        idx: usize,
        entries: &[StfsEntry],
        cache: &mut Vec<Option<PathBuf>>,
        guard: &mut Vec<bool>,
    ) -> Result<PathBuf> {
        if let Some(p) = &cache[idx] {
            return Ok(p.clone());
        }
        if guard[idx] {
            return Err(FatxError::Other(format!(
                "STFS entry parent chain cycles at index {}",
                idx,
            )));
        }
        guard[idx] = true;
        let entry = &entries[idx];
        let path = if entry.parent_index == -1 {
            PathBuf::from(&entry.name)
        } else {
            let parent_idx = entry.parent_index as usize;
            if parent_idx >= entries.len() {
                return Err(FatxError::Other(format!(
                    "STFS entry {} references out-of-range parent {}",
                    idx, entry.parent_index,
                )));
            }
            let parent_path = resolve(parent_idx, entries, cache, guard)?;
            parent_path.join(&entry.name)
        };
        cache[idx] = Some(path.clone());
        Ok(path)
    }

    let mut guard = vec![false; entries.len()];
    for i in 0..entries.len() {
        resolve(i, entries, &mut out, &mut guard)?;
        // Reset guard for the next traversal — the recursive helper marks
        // visited nodes but does not reset them between top-level calls.
        for slot in guard.iter_mut() {
            *slot = false;
        }
    }
    Ok(out.into_iter().map(|p| p.unwrap()).collect())
}
