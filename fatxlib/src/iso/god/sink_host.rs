use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::Path;

use crate::error::{FatxError, Result};

use super::core::{GodSink, part_payload_bytes};
use super::prepare::PreparedSource;
use super::{ConvertOptions, FileLayout, HashList, SOURCE_BUFFER_SIZE};

pub(crate) struct HostFsSink<'a> {
    dest_dir: &'a Path,
}

impl<'a> HostFsSink<'a> {
    pub(crate) fn new(dest_dir: &'a Path) -> Self {
        Self { dest_dir }
    }

    fn data_dir_path(&self, source: &PreparedSource) -> std::path::PathBuf {
        FileLayout::new(self.dest_dir, &source.exe_info, source.content_type).data_dir_path()
    }

    fn part_file_path(&self, source: &PreparedSource, part_index: u64) -> std::path::PathBuf {
        FileLayout::new(self.dest_dir, &source.exe_info, source.content_type)
            .part_file_path(part_index)
    }

    fn con_header_file_path(&self, source: &PreparedSource) -> std::path::PathBuf {
        FileLayout::new(self.dest_dir, &source.exe_info, source.content_type).con_header_file_path()
    }
}

impl GodSink for HostFsSink<'_> {
    fn begin(&mut self, source: &PreparedSource) -> Result<()> {
        ensure_empty_dir(&self.data_dir_path(source))
    }

    fn write_part<'a>(
        &mut self,
        source: &PreparedSource,
        part_index: u64,
        _opts: &mut ConvertOptions<'a>,
    ) -> Result<()> {
        let part_path = self.part_file_path(source, part_index);
        let part_file = File::options()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&part_path)
            .map_err(FatxError::Io)?;
        let part_file = BufWriter::with_capacity(SOURCE_BUFFER_SIZE, part_file);
        let remaining_bytes = part_payload_bytes(source.report.data_size, part_index);
        let iso_data_volume = source.open_reader()?;
        super::write_part(iso_data_volume, part_index, remaining_bytes, part_file)
    }

    fn read_master_hash(&mut self, source: &PreparedSource, part_index: u64) -> Result<HashList> {
        let part_path = self.part_file_path(source, part_index);
        read_part_mht(&part_path)
    }

    fn write_master_hash(
        &mut self,
        source: &PreparedSource,
        part_index: u64,
        mht: &HashList,
    ) -> Result<()> {
        let part_path = self.part_file_path(source, part_index);
        write_part_mht(&part_path, mht)
    }

    fn last_part_size(&self, source: &PreparedSource) -> Result<u64> {
        fs::metadata(self.part_file_path(source, source.report.part_count - 1))
            .map_err(FatxError::Io)
            .map(|meta| meta.len())
    }

    fn write_con_header(&mut self, source: &PreparedSource, con_bytes: Vec<u8>) -> Result<()> {
        let mut con_header_file = File::options()
            .write(true)
            .create(true)
            .truncate(true)
            .open(self.con_header_file_path(source))
            .map_err(FatxError::Io)?;
        con_header_file.write_all(&con_bytes).map_err(FatxError::Io)
    }
}

fn ensure_empty_dir(path: &Path) -> Result<()> {
    if fs::exists(path).map_err(FatxError::Io)? {
        fs::remove_dir_all(path).map_err(FatxError::Io)?;
    }
    fs::create_dir_all(path).map_err(FatxError::Io)?;
    Ok(())
}

fn read_part_mht(path: &Path) -> Result<HashList> {
    let mut part_file = File::options()
        .read(true)
        .open(path)
        .map_err(FatxError::Io)?;
    HashList::read(&mut part_file)
}

fn write_part_mht(path: &Path, mht: &HashList) -> Result<()> {
    let mut part_file = File::options()
        .write(true)
        .open(path)
        .map_err(FatxError::Io)?;
    mht.write(&mut part_file)?;
    Ok(())
}
