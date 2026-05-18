//! Host-filesystem sink for STFS extraction.

use std::io::{BufWriter, Read, Seek, Write};
use std::path::Path;

use crate::error::{FatxError, Result};

use super::core::{StfsSink, run_extract};
use super::{ExtractReport, ProgressFn, StfsPackage};

// ── HostSink ─────────────────────────────────────────────────────────────────

pub(crate) struct HostSink<'a> {
    dest_root: &'a Path,
}

impl<'a> HostSink<'a> {
    pub(crate) fn new(dest_root: &'a Path) -> Result<Self> {
        std::fs::create_dir_all(dest_root).map_err(FatxError::Io)?;
        Ok(Self { dest_root })
    }
}

impl StfsSink for HostSink<'_> {
    fn ensure_dir(&mut self, rel: &Path) -> Result<()> {
        let target = self.dest_root.join(rel);
        std::fs::create_dir_all(&target).map_err(FatxError::Io)
    }

    fn refuse_if_file_exists(&mut self, rel: &Path) -> Result<()> {
        let target = self.dest_root.join(rel);
        if target.exists() {
            return Err(FatxError::Other(format!(
                "refusing to overwrite existing file: {}",
                target.display(),
            )));
        }
        Ok(())
    }

    fn write_file(&mut self, rel: &Path, _size: u64, reader: &mut dyn Read) -> Result<()> {
        let target = self.dest_root.join(rel);
        let file = std::fs::File::create(&target).map_err(FatxError::Io)?;
        let mut writer = BufWriter::new(file);
        std::io::copy(reader, &mut writer).map_err(FatxError::Io)?;
        writer.flush().map_err(FatxError::Io)?;
        Ok(())
    }
}

// ── public wrapper ────────────────────────────────────────────────────────────

/// Walk `package` and extract every file under `dest_root`. Creates
/// directories as needed. Returns counts on success.
///
/// `progress` is invoked once per file just before its write begins.
pub fn extract_to_host<R: Read + Seek>(
    package: &mut StfsPackage<R>,
    dest_root: &Path,
    progress: Option<ProgressFn<'_>>,
) -> Result<ExtractReport> {
    let mut sink = HostSink::new(dest_root)?;
    run_extract(package, &mut sink, progress)
}
