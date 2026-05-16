//! Compact virtual XDVDFS layout planning.
//!
//! Builds an in-memory plan for a dense XDVDFS image without materializing a
//! temporary `.iso` on disk. Metadata regions are synthesized in memory; file
//! regions are read lazily from the source image when the reader is consumed.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use crate::error::{FatxError, Result};
use crate::executable::TitleExecutionInfo;

use super::god::ContentType;
use super::image::XisoImage;
use super::manifest::{IsoFilterPolicy, build_manifest};

use xdvdfs::layout::{DirectoryEntryTable, SECTOR_SIZE, VolumeDescriptor};
use xdvdfs::write::dirtab::DirectoryEntryTableWriter;
use xdvdfs::write::fs::{FileEntry, FileType, Filesystem, PathVec, XDVDFSFilesystem};
use xdvdfs::write::sector::SectorAllocator;

type SourceOffsetDevice = xdvdfs::blockdev::OffsetWrapper<File, std::io::Error>;
type SourceFilesystem = XDVDFSFilesystem<std::io::Error, SourceOffsetDevice>;

#[derive(Clone)]
struct CompactTreeEntry {
    dir: PathVec,
    listing: Vec<FileEntry>,
}

enum CompactRegionData {
    Bytes(Box<[u8]>),
    Source { source_offset: u64 },
}

struct CompactRegion {
    start: u64,
    len: u64,
    data: CompactRegionData,
}

struct CompactImagePlan {
    data_size: u64,
    regions: Vec<CompactRegion>,
}

pub(crate) struct CompactSource {
    exe_info: TitleExecutionInfo,
    content_type: ContentType,
    partition_offset: u64,
    plan: CompactImagePlan,
}

pub(crate) struct CompactImageReader<'a> {
    source: File,
    partition_offset: u64,
    plan: &'a CompactImagePlan,
    cursor: u64,
}

impl CompactSource {
    pub(crate) fn open_reader(&self, source_iso: &Path) -> Result<CompactImageReader<'_>> {
        Ok(CompactImageReader {
            source: File::open(source_iso).map_err(FatxError::Io)?,
            partition_offset: self.partition_offset,
            plan: &self.plan,
            cursor: 0,
        })
    }

    pub(crate) fn exe_info(&self) -> &TitleExecutionInfo {
        &self.exe_info
    }

    pub(crate) fn content_type(&self) -> ContentType {
        self.content_type
    }

    pub(crate) fn data_size(&self) -> u64 {
        self.plan.data_size
    }
}

impl CompactImageReader<'_> {
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
        buf.fill(0);
        if buf.is_empty() {
            return Ok(());
        }

        let end = offset.saturating_add(buf.len() as u64);
        let mut idx = self
            .plan
            .regions
            .partition_point(|region| region.start.saturating_add(region.len) <= offset);

        while idx < self.plan.regions.len() {
            let region = &self.plan.regions[idx];
            let region_end = region.start.saturating_add(region.len);
            if region.start >= end {
                break;
            }

            let overlap_start = offset.max(region.start);
            let overlap_end = end.min(region_end);
            if overlap_start < overlap_end {
                let dst_start = (overlap_start - offset) as usize;
                let dst_end = (overlap_end - offset) as usize;
                let dst = &mut buf[dst_start..dst_end];
                let src_offset = overlap_start - region.start;
                match &region.data {
                    CompactRegionData::Bytes(bytes) => {
                        let src_start = src_offset as usize;
                        let src_end = src_start + dst.len();
                        dst.copy_from_slice(&bytes[src_start..src_end]);
                    }
                    CompactRegionData::Source { source_offset } => {
                        self.source.seek(SeekFrom::Start(
                            self.partition_offset + source_offset + src_offset,
                        ))?;
                        self.source.read_exact(dst)?;
                    }
                }
            }
            idx += 1;
        }

        Ok(())
    }
}

impl Read for CompactImageReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.cursor >= self.plan.data_size || buf.is_empty() {
            return Ok(0);
        }
        let want = ((self.plan.data_size - self.cursor) as usize).min(buf.len());
        self.read_at(self.cursor, &mut buf[..want])?;
        self.cursor += want as u64;
        Ok(want)
    }
}

impl Seek for CompactImageReader<'_> {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        let len = self.plan.data_size as i128;
        let next = match pos {
            SeekFrom::Start(pos) => pos as i128,
            SeekFrom::Current(delta) => self.cursor as i128 + delta as i128,
            SeekFrom::End(delta) => len + delta as i128,
        };
        if next < 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "negative seek in CompactImageReader",
            ));
        }
        self.cursor = next as u64;
        Ok(self.cursor)
    }
}

fn cancelled(op: &str) -> FatxError {
    FatxError::Other(format!("{op}: cancelled"))
}

fn xdvdfs_other<E: std::fmt::Debug>(ctx: &str, err: E) -> FatxError {
    FatxError::Other(format!("{ctx}: {err:?}"))
}

fn collect_compact_tree(
    fs: &mut SourceFilesystem,
    should_abort: Option<&dyn Fn() -> bool>,
    kept_paths: &HashSet<String>,
    kept_dirs: &HashSet<String>,
) -> Result<Vec<CompactTreeEntry>> {
    let mut dirs = vec![PathVec::default()];
    let mut out = Vec::new();

    while let Some(dir) = dirs.pop() {
        if let Some(abort) = should_abort
            && abort()
        {
            return Err(cancelled("compact_tree"));
        }

        let mut listing =
            <SourceFilesystem as Filesystem<File, std::io::Error>>::read_dir(fs, &dir)
                .map_err(|e| FatxError::Other(format!("xdvdfs compact read_dir: {e}")))?;
        listing.retain(|entry| {
            let path = PathVec::from_base(&dir, &entry.name).as_string();
            let path = normalize_path(&path);
            match entry.file_type {
                FileType::Directory => kept_dirs.contains(path),
                FileType::File => kept_paths.contains(path),
            }
        });

        for entry in &listing {
            if matches!(entry.file_type, FileType::Directory) {
                dirs.push(PathVec::from_base(&dir, &entry.name));
            }
        }

        out.push(CompactTreeEntry { dir, listing });
    }

    Ok(out)
}

fn build_compact_dirent_tables(
    tree: &[CompactTreeEntry],
) -> Result<BTreeMap<PathVec, DirectoryEntryTableWriter>> {
    let mut dirent_tables: BTreeMap<PathVec, DirectoryEntryTableWriter> = BTreeMap::new();

    for entry in tree.iter().rev() {
        let mut dirtab = DirectoryEntryTableWriter::default();
        for child in &entry.listing {
            match child.file_type {
                FileType::Directory => {
                    let child_path = PathVec::from_base(&entry.dir, &child.name);
                    let dir_size = dirent_tables
                        .get(&child_path)
                        .ok_or_else(|| {
                            FatxError::Other(format!(
                                "xdvdfs compact: missing dirtab for {}",
                                child_path.as_string()
                            ))
                        })?
                        .dirtab_size();
                    dirtab
                        .add_dir::<std::io::Error>(&child.name, dir_size)
                        .map_err(|e| xdvdfs_other("xdvdfs add_dir", e))?;
                }
                FileType::File => {
                    let size = child
                        .len
                        .try_into()
                        .map_err(|_| FatxError::Other(format!("file too large: {}", child.len)))?;
                    dirtab
                        .add_file::<std::io::Error>(&child.name, size)
                        .map_err(|e| xdvdfs_other("xdvdfs add_file", e))?;
                }
            }
        }
        dirtab
            .compute_size::<std::io::Error>()
            .map_err(|e| xdvdfs_other("xdvdfs compute_size", e))?;
        dirent_tables.insert(entry.dir.clone(), dirtab);
    }

    Ok(dirent_tables)
}

pub(crate) fn build_compact_source(
    source_iso: &Path,
    should_abort: Option<&dyn Fn() -> bool>,
) -> Result<CompactSource> {
    if let Some(abort) = should_abort
        && abort()
    {
        return Err(cancelled("compact_source"));
    }

    let manifest = {
        let file = File::open(source_iso).map_err(FatxError::Io)?;
        let mut img = XisoImage::open(file)?;
        build_manifest(
            &mut img,
            IsoFilterPolicy {
                keep_systemupdate: false,
            },
        )?
    };
    let title_info = manifest
        .title_info
        .clone()
        .ok_or_else(|| FatxError::Other("xdvdfs compact: no executable found".into()))?;
    let exe_info = title_info.execution_info;
    let content_type = title_info.content_type;
    let partition_offset = manifest.partition_offset;
    let file_offsets: HashMap<String, u64> = manifest.kept_offset_map();
    let kept_paths = manifest.kept_path_set();
    let kept_dirs = manifest.kept_dir_set();

    let file = File::open(source_iso).map_err(FatxError::Io)?;
    let xiso = xdvdfs::blockdev::OffsetWrapper::new(file)
        .map_err(|e| xdvdfs_other("xdvdfs offset detect", e))?;
    let mut fs = XDVDFSFilesystem::new(xiso)
        .ok_or_else(|| FatxError::Other("xdvdfs compact: could not open source image".into()))?;
    let tree = collect_compact_tree(&mut fs, should_abort, &kept_paths, &kept_dirs)?;
    let dirent_tables = build_compact_dirent_tables(&tree)?;

    let mut dir_sectors = BTreeMap::new();
    let mut allocator = SectorAllocator::default();
    let (root_path, root_dirtab) = dirent_tables
        .first_key_value()
        .ok_or_else(|| FatxError::Other("xdvdfs compact: empty directory tree".into()))?;
    let root_sector = allocator.allocate_contiguous(root_dirtab.dirtab_size() as u64);
    let root_table = DirectoryEntryTable::new(root_dirtab.dirtab_size(), root_sector);
    dir_sectors.insert(root_path.clone(), root_sector as u64);

    let volume_bytes = VolumeDescriptor::new(root_table)
        .serialize::<std::io::Error>()
        .map_err(|e| xdvdfs_other("xdvdfs serialize volume", e))?;
    let mut regions = vec![CompactRegion {
        start: 32 * SECTOR_SIZE as u64,
        len: volume_bytes.len() as u64,
        data: CompactRegionData::Bytes(Box::from(volume_bytes)),
    }];

    for (path, dirtab) in dirent_tables {
        if let Some(abort) = should_abort
            && abort()
        {
            return Err(cancelled("compact_source"));
        }

        let sector = *dir_sectors
            .get(&path)
            .ok_or_else(|| FatxError::Other(format!("missing sector for {}", path.as_string())))?;
        let repr = dirtab
            .disk_repr::<std::io::Error>(&mut allocator)
            .map_err(|e| xdvdfs_other("xdvdfs disk_repr", e))?;
        regions.push(CompactRegion {
            start: sector * SECTOR_SIZE as u64,
            len: repr.entry_table.len() as u64,
            data: CompactRegionData::Bytes(repr.entry_table),
        });

        for entry in repr.file_listing {
            let child_path = PathVec::from_base(&path, &entry.name);
            if entry.is_dir {
                dir_sectors.insert(child_path, entry.sector);
                continue;
            }

            let logical_path = child_path.as_string();
            let logical_path = logical_path.trim_start_matches('/').to_string();
            let source_offset = *file_offsets.get(&logical_path).ok_or_else(|| {
                FatxError::Other(format!(
                    "xdvdfs compact: missing source offset for {}",
                    logical_path
                ))
            })?;
            regions.push(CompactRegion {
                start: entry.sector * SECTOR_SIZE as u64,
                len: entry.size,
                data: CompactRegionData::Source { source_offset },
            });
        }
    }

    regions.sort_by_key(|region| region.start);
    let data_size = regions
        .iter()
        .map(|region| region.start + region.len)
        .max()
        .unwrap_or(0);

    Ok(CompactSource {
        exe_info,
        content_type,
        partition_offset,
        plan: CompactImagePlan { data_size, regions },
    })
}

fn normalize_path(path: &str) -> &str {
    path.trim_start_matches('/')
}
