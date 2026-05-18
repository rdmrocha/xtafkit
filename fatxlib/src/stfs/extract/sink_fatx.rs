//! FATX-volume sink for STFS extraction + `StfsFileReader`.

use std::io::{Read, Seek, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::error::{FatxError, Result};
use crate::stfs::block_translator::BLOCK_SIZE;
use crate::stfs::file_entry::StfsEntry;
use crate::volume::FatxVolume;

use super::core::{StfsSink, run_extract};
use super::{ExtractReport, ProgressFn, StfsPackage};

// ── FatxSink ────────────────────────────────────────────────────────────────

pub(crate) struct FatxSink<'a, S: Read + Write + Seek> {
    vol: &'a mut FatxVolume<S>,
    root: String,
    cancel: Option<&'a AtomicBool>,
}

impl<'a, S: Read + Write + Seek> FatxSink<'a, S> {
    pub(crate) fn new(
        vol: &'a mut FatxVolume<S>,
        dest_root: &str,
        cancel: Option<&'a AtomicBool>,
    ) -> Result<Self> {
        let root = dest_root.trim_end_matches('/').to_string();
        ensure_fatx_dir_chain(vol, &root)?;
        Ok(Self { vol, root, cancel })
    }

    /// Convert a relative `Path` (from the engine) to an absolute FATX path.
    fn fatx_path(&self, rel: &Path) -> String {
        let rel_str = rel.to_string_lossy();
        let rel_fwd = rel_str.replace('\\', "/");
        format!("{}/{}", self.root, rel_fwd)
    }
}

impl<S: Read + Write + Seek> StfsSink for FatxSink<'_, S> {
    fn ensure_dir(&mut self, rel: &Path) -> Result<()> {
        ensure_fatx_dir_chain(self.vol, &self.fatx_path(rel))
    }

    fn write_file(&mut self, rel: &Path, size: u64, reader: &mut dyn Read) -> Result<()> {
        let path = self.fatx_path(rel);
        self.vol.create_file_from_reader(&path, size, reader, None)
    }

    fn check_cancelled(&self) -> Result<()> {
        if let Some(flag) = self.cancel
            && flag.load(Ordering::Relaxed)
        {
            return Err(FatxError::Other("cancelled".to_string()));
        }
        Ok(())
    }
}

// ── public wrapper ───────────────────────────────────────────────────────────

/// Walk `package` and stream every file into `dest_root` on the given FATX
/// volume. Creates directories as needed; refuses to overwrite existing
/// files. Returns counts on success.
///
/// `progress` is invoked once per file just before its write begins:
/// `(relative_path, file_size, total_bytes_so_far)`.
pub fn extract_to_fatx<R, S>(
    package: &mut StfsPackage<R>,
    vol: &mut FatxVolume<S>,
    dest_root: &str,
    progress: Option<ProgressFn<'_>>,
    cancel: Option<&AtomicBool>,
) -> Result<ExtractReport>
where
    R: Read + Seek,
    S: Read + Write + Seek,
{
    let mut sink = FatxSink::new(vol, dest_root, cancel)?;
    run_extract(package, &mut sink, progress)
}

// ── StfsFileReader ───────────────────────────────────────────────────────────

/// Adapter that exposes a single STFS file entry as a streaming [`Read`].
///
/// `create_file_from_reader` (and the engine's `sink.write_file`) require
/// `Read`. We pre-walk the block chain once and read blocks on demand to avoid
/// buffering entire files in memory.
pub(crate) struct StfsFileReader<'a, R: Read + Seek> {
    package: &'a mut StfsPackage<R>,
    chain: Vec<u32>,
    chain_idx: usize,
    block_buf: Vec<u8>,
    block_pos: usize,
    block_len: usize,
    remaining: u64,
}

impl<'a, R: Read + Seek> StfsFileReader<'a, R> {
    pub(crate) fn new(package: &'a mut StfsPackage<R>, entry: &StfsEntry) -> Result<Self> {
        let chain = package.read_block_chain(entry)?;
        Ok(Self {
            package,
            chain,
            chain_idx: 0,
            block_buf: Vec::new(),
            block_pos: 0,
            block_len: 0,
            remaining: entry.size,
        })
    }
}

impl<R: Read + Seek> Read for StfsFileReader<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.remaining == 0 {
            return Ok(0);
        }
        // Refill block buffer when exhausted.
        if self.block_pos == self.block_len {
            if self.chain_idx == self.chain.len() {
                return Ok(0);
            }
            let block_idx = self.chain[self.chain_idx];
            self.chain_idx += 1;
            self.block_buf = self
                .package
                .read_data_block(block_idx)
                .map_err(|e| std::io::Error::other(format!("STFS block read: {e}")))?;
            let take = self.remaining.min(BLOCK_SIZE) as usize;
            self.block_len = take;
            self.block_pos = 0;
        }
        let want = buf.len().min(self.block_len - self.block_pos);
        buf[..want].copy_from_slice(&self.block_buf[self.block_pos..self.block_pos + want]);
        self.block_pos += want;
        self.remaining -= want as u64;
        Ok(want)
    }
}

// ── ensure_fatx_dir_chain ────────────────────────────────────────────────────

/// Create the FATX directory `path` and every missing ancestor.
pub(crate) fn ensure_fatx_dir_chain<S: Read + Write + Seek>(
    vol: &mut FatxVolume<S>,
    path: &str,
) -> Result<()> {
    let trimmed = path.trim_start_matches('/').trim_end_matches('/');
    if trimmed.is_empty() {
        return Ok(());
    }
    let mut current = String::new();
    for part in trimmed.split('/') {
        if part.is_empty() {
            continue;
        }
        current.push('/');
        current.push_str(part);
        match vol.create_directory(&current) {
            Ok(()) => {}
            Err(FatxError::FileExists(_)) => {
                // Tolerate FileExists only if the existing entry is actually a
                // directory. A stale file would cause a confusing NotADirectory
                // error later inside create_file_from_reader.
                match vol.resolve_path(&current) {
                    Ok(entry) if entry.is_directory() => {}
                    Ok(_) => {
                        return Err(FatxError::Other(format!(
                            "STFS extract: '{}' exists but is not a directory",
                            current
                        )));
                    }
                    Err(e) => return Err(e),
                }
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}
