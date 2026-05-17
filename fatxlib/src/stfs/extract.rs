//! High-level STFS read + extract API.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::error::{FatxError, Result};
use crate::stfs::block_translator::{BLOCK_SIZE, BLOCKS_PER_L0, block_to_byte_offset};
use crate::stfs::file_entry::{self, ENTRIES_PER_BLOCK, FILE_ENTRY_SIZE, StfsEntry};
use crate::stfs::header::{MIN_HEADER_BYTES, StfsHeader, parse_header};
use crate::stfs::volume_descriptor::{self, VolumeDescriptor};

/// Summary of a completed extraction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractReport {
    pub files: usize,
    pub directories: usize,
    pub bytes: u64,
}

/// Progress callback: `(relative_path, file_size, total_bytes_so_far)`.
pub type ProgressFn<'a> = &'a dyn Fn(&str, u64, u64);

/// Walk `package` and extract every file under `dest_root`. Creates
/// directories as needed. Returns counts on success.
///
/// `progress` is invoked once per file just before its write begins.
pub fn extract_to_host<R: Read + Seek>(
    package: &mut StfsPackage<R>,
    dest_root: &Path,
    progress: Option<ProgressFn<'_>>,
) -> Result<ExtractReport> {
    let entries = package.entries()?;
    let paths = build_relative_paths(&entries)?;

    let mut files = 0usize;
    let mut directories = 0usize;
    let mut bytes = 0u64;

    for (idx, entry) in entries.iter().enumerate() {
        let rel = &paths[idx];
        let target = dest_root.join(rel);
        if entry.is_directory {
            std::fs::create_dir_all(&target).map_err(FatxError::Io)?;
            directories += 1;
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(FatxError::Io)?;
        }
        if target.exists() {
            return Err(FatxError::Other(format!(
                "refusing to overwrite existing file: {}",
                target.display(),
            )));
        }
        if let Some(cb) = progress {
            cb(&rel.to_string_lossy(), entry.size, bytes);
        }
        let file = std::fs::File::create(&target).map_err(FatxError::Io)?;
        let mut writer = std::io::BufWriter::new(file);
        package.read_file(entry, &mut writer)?;
        writer.flush().map_err(FatxError::Io)?;
        files += 1;
        bytes += entry.size;
    }

    Ok(ExtractReport {
        files,
        directories,
        bytes,
    })
}

/// Build a vec of relative paths parallel to `entries`. Walks
/// `parent_index` chains; tolerates orphan entries by attaching them to
/// the root with a `<orphan-N>` prefix.
fn build_relative_paths(entries: &[StfsEntry]) -> Result<Vec<PathBuf>> {
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
        // reset guard for next traversal
        for slot in guard.iter_mut() {
            *slot = false;
        }
    }
    Ok(out.into_iter().map(|p| p.unwrap()).collect())
}

pub struct StfsPackage<R: Read + Seek> {
    reader: R,
    header: StfsHeader,
    volume: VolumeDescriptor,
}

impl<R: Read + Seek> std::fmt::Debug for StfsPackage<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StfsPackage")
            .field("header", &self.header)
            .field("volume", &self.volume)
            .finish_non_exhaustive()
    }
}

impl<R: Read + Seek> StfsPackage<R> {
    /// Parse the STFS header + volume descriptor. Cheap; does not walk
    /// the file table.
    pub fn open(mut reader: R) -> Result<Self> {
        let mut prefix = vec![0u8; MIN_HEADER_BYTES.max(0x379 + 0x24)];
        reader.seek(SeekFrom::Start(0)).map_err(FatxError::Io)?;
        reader.read_exact(&mut prefix).map_err(FatxError::Io)?;

        let header = parse_header(&prefix).ok_or_else(|| {
            FatxError::Other("Not an STFS package (bad magic or truncated header)".to_string())
        })?;

        let volume = volume_descriptor::parse(&prefix[0x379..0x379 + 0x24])?;
        if !volume.read_only_format {
            return Err(FatxError::Other(
                "STFS type 0 (read-write) not supported yet — v1 supports type 1 only".to_string(),
            ));
        }

        Ok(Self {
            reader,
            header,
            volume,
        })
    }

    pub fn header(&self) -> &StfsHeader {
        &self.header
    }

    pub fn volume(&self) -> &VolumeDescriptor {
        &self.volume
    }

    /// Read one data block (4 KiB) into a fresh buffer.
    fn read_data_block(&mut self, block_index: u32) -> Result<Vec<u8>> {
        let total = self.volume.total_alloc_blocks;
        if block_index >= total {
            return Err(FatxError::Other(format!(
                "STFS block index {} out of range (total_alloc_blocks = {})",
                block_index, total,
            )));
        }
        let offset = block_to_byte_offset(block_index);
        let mut buf = vec![0u8; BLOCK_SIZE as usize];
        self.reader
            .seek(SeekFrom::Start(offset))
            .map_err(FatxError::Io)?;
        self.reader.read_exact(&mut buf).map_err(FatxError::Io)?;
        Ok(buf)
    }

    /// L0 hash block byte offset for the group containing `data_block`.
    fn l0_hash_block_offset(&self, data_block: u32) -> u64 {
        let group_start = (data_block / BLOCKS_PER_L0) * BLOCKS_PER_L0;
        // The L0 hash block sits AFTER the group's data blocks. Equivalently
        // it occupies the position one block before block_to_byte_offset
        // would place data block (group_start + BLOCKS_PER_L0).
        block_to_byte_offset(group_start + BLOCKS_PER_L0) - BLOCK_SIZE
    }

    /// Read the next-block pointer for `data_block` from its L0 hash entry.
    /// Returns `0xFFFFFF` (sentinel end-of-chain) if the data block is the
    /// last in its chain.
    fn read_next_block(&mut self, data_block: u32) -> Result<u32> {
        let hash_offset = self.l0_hash_block_offset(data_block);
        let entry_offset = hash_offset + (data_block as u64 % BLOCKS_PER_L0 as u64) * 24 + 0x15;
        let mut buf = [0u8; 3];
        self.reader
            .seek(SeekFrom::Start(entry_offset))
            .map_err(FatxError::Io)?;
        self.reader.read_exact(&mut buf).map_err(FatxError::Io)?;
        Ok((buf[0] as u32) | ((buf[1] as u32) << 8) | ((buf[2] as u32) << 16))
    }

    /// Walk a file's block chain, returning the ordered list of data block
    /// indices. Caps the walk at `entry.used_blocks` to reject malformed
    /// cyclic chains.
    pub fn read_block_chain(&mut self, entry: &StfsEntry) -> Result<Vec<u32>> {
        if entry.used_blocks == 0 {
            return Ok(Vec::new());
        }
        if entry.consecutive {
            return Ok((entry.start_block..entry.start_block + entry.used_blocks).collect());
        }
        let mut chain = Vec::with_capacity(entry.used_blocks as usize);
        let mut current = entry.start_block;
        for _ in 0..entry.used_blocks {
            chain.push(current);
            let next = self.read_next_block(current)?;
            if next == 0xFFFFFF {
                break;
            }
            current = next;
        }
        Ok(chain)
    }

    /// Stream the contents of one file entry through the writer. Truncates
    /// the final block to honor `entry.size`.
    pub fn read_file<W: Write>(&mut self, entry: &StfsEntry, writer: &mut W) -> Result<u64> {
        let chain = self.read_block_chain(entry)?;
        let mut remaining = entry.size;
        for block_idx in chain {
            let block = self.read_data_block(block_idx)?;
            let take = remaining.min(BLOCK_SIZE) as usize;
            writer.write_all(&block[..take]).map_err(FatxError::Io)?;
            remaining -= take as u64;
            if remaining == 0 {
                break;
            }
        }
        Ok(entry.size)
    }

    /// Walk the file table block chain (v1: consecutive only).
    pub fn entries(&mut self) -> Result<Vec<StfsEntry>> {
        let count = self.volume.file_table_block_count as u32;
        let first = self.volume.file_table_block_number;
        let mut out = Vec::new();
        for i in 0..count {
            let block = self.read_data_block(first + i)?;
            for slot in 0..ENTRIES_PER_BLOCK {
                let start = slot * FILE_ENTRY_SIZE;
                let bytes = &block[start..start + FILE_ENTRY_SIZE];
                match file_entry::parse(bytes)? {
                    Some(entry) => out.push(entry),
                    None => return Ok(out), // empty slot terminates the table
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Build the minimum bytes needed for `StfsPackage::open` to succeed.
    /// Magic + title_id at 0x360 + display_name + title_name (already
    /// covered by parse_header's MIN_HEADER_BYTES) + volume descriptor at
    /// 0x379 (which overlaps the header region — we just patch those bytes).
    fn synthetic_package(read_only_format: bool) -> Vec<u8> {
        let mut buf = vec![0u8; MIN_HEADER_BYTES];
        // LIVE magic
        buf[0..4].copy_from_slice(b"LIVE");
        // title_id at 0x360
        buf[0x360..0x364].copy_from_slice(&0x4D5307E6u32.to_be_bytes());
        // Volume descriptor at 0x379
        buf[0x379] = 0x24; // descriptor_size
        buf[0x37B] = if read_only_format { 0x01 } else { 0x00 };
        buf[0x37C..0x37E].copy_from_slice(&1u16.to_be_bytes()); // file_table_block_count = 1
        // file_table_block_number = 0 (already zero)
        buf[0x395..0x399].copy_from_slice(&1u32.to_be_bytes()); // total_alloc = 1
        buf
    }

    #[test]
    fn opens_synthetic_type_1_package() {
        let bytes = synthetic_package(true);
        let pkg = StfsPackage::open(Cursor::new(bytes)).expect("open");
        assert_eq!(&pkg.header().magic, b"LIVE");
        assert_eq!(pkg.header().title_id, 0x4D5307E6);
        assert!(pkg.volume().read_only_format);
    }

    #[test]
    fn rejects_type_0_package() {
        let bytes = synthetic_package(false);
        let err = StfsPackage::open(Cursor::new(bytes)).expect_err("should reject");
        assert!(format!("{}", err).contains("type 0"));
    }

    #[test]
    fn rejects_bad_magic() {
        let mut bytes = synthetic_package(true);
        bytes[0..4].copy_from_slice(b"XXXX");
        let err = StfsPackage::open(Cursor::new(bytes)).expect_err("should reject");
        assert!(format!("{}", err).contains("Not an STFS package"));
    }

    /// Build a more complete synthetic package: header + one file-table
    /// block at block 0 containing two entries (one dir, one file).
    fn synthetic_package_with_one_file_table_block(entries: &[Vec<u8>]) -> Vec<u8> {
        use crate::stfs::block_translator::{BLOCK_SIZE, FIRST_DATA_BLOCK_OFFSET};

        // Header region: MIN_HEADER_BYTES.
        let mut buf = vec![0u8; MIN_HEADER_BYTES];
        buf[0..4].copy_from_slice(b"LIVE");
        buf[0x360..0x364].copy_from_slice(&0x4D5307E6u32.to_be_bytes());
        buf[0x379] = 0x24;
        buf[0x37B] = 0x01;
        buf[0x37C..0x37E].copy_from_slice(&1u16.to_be_bytes());
        // file_table_block_number stays 0
        buf[0x395..0x399].copy_from_slice(&2u32.to_be_bytes()); // total_alloc = 2

        // Grow to the start of block 0.
        let block_zero_offset = FIRST_DATA_BLOCK_OFFSET as usize;
        if buf.len() < block_zero_offset {
            buf.resize(block_zero_offset, 0);
        }
        // File table block 0: pack entries.
        let mut ft_block = vec![0u8; BLOCK_SIZE as usize];
        for (i, e) in entries.iter().enumerate() {
            ft_block[i * 0x40..(i + 1) * 0x40].copy_from_slice(e);
        }
        buf.extend_from_slice(&ft_block);
        buf
    }

    fn fe(name: &str, is_dir: bool, parent: i16, size: u32, start_block: u32) -> Vec<u8> {
        let mut buf = vec![0u8; 0x40];
        let nb = name.as_bytes();
        buf[..nb.len()].copy_from_slice(nb);
        let mut flags = nb.len() as u8 & 0x3F;
        flags |= 0x40; // consecutive
        if is_dir {
            flags |= 0x80;
        }
        buf[0x28] = flags;
        // used_blocks = 1 for files, 0 for dirs
        if !is_dir {
            buf[0x2C] = 1;
        }
        buf[0x2F] = (start_block & 0xFF) as u8;
        buf[0x30] = ((start_block >> 8) & 0xFF) as u8;
        buf[0x31] = ((start_block >> 16) & 0xFF) as u8;
        buf[0x32..0x34].copy_from_slice(&parent.to_be_bytes());
        buf[0x34..0x38].copy_from_slice(&size.to_be_bytes());
        buf
    }

    #[test]
    fn entries_walks_single_file_table_block() {
        let entries = vec![
            fe("Media", true, -1, 0, 0),
            fe("default.xex", false, 0, 0x1000, 1),
        ];
        let bytes = synthetic_package_with_one_file_table_block(&entries);
        let mut pkg = StfsPackage::open(Cursor::new(bytes)).expect("open");
        let listed = pkg.entries().expect("entries");
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].name, "Media");
        assert!(listed[0].is_directory);
        assert_eq!(listed[1].name, "default.xex");
        assert_eq!(listed[1].size, 0x1000);
        assert_eq!(listed[1].parent_index, 0);
    }

    #[test]
    fn entries_stops_at_empty_slot() {
        let entries = vec![fe("only.bin", false, -1, 0x100, 1)];
        let bytes = synthetic_package_with_one_file_table_block(&entries);
        let mut pkg = StfsPackage::open(Cursor::new(bytes)).expect("open");
        let listed = pkg.entries().expect("entries");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "only.bin");
    }

    /// Build a synthetic package with one fragmented file whose blocks are
    /// 5 → 3 → 1 (non-consecutive). Requires planting next-block pointers
    /// inside the L0 hash block.
    fn synthetic_package_with_fragmented_file() -> Vec<u8> {
        use crate::stfs::block_translator::{BLOCK_SIZE, BLOCKS_PER_L0, FIRST_DATA_BLOCK_OFFSET};

        // File table at block 0; data blocks 1..=5.
        // total_alloc_blocks = 6 (covers blocks 0..=5).
        let mut buf = vec![0u8; MIN_HEADER_BYTES];
        buf[0..4].copy_from_slice(b"LIVE");
        buf[0x360..0x364].copy_from_slice(&0x4D5307E6u32.to_be_bytes());
        buf[0x379] = 0x24;
        buf[0x37B] = 0x01;
        buf[0x37C..0x37E].copy_from_slice(&1u16.to_be_bytes());
        buf[0x395..0x399].copy_from_slice(&6u32.to_be_bytes()); // total_alloc = 6

        let block_zero_offset = FIRST_DATA_BLOCK_OFFSET as usize;
        buf.resize(block_zero_offset, 0);

        // Block 0: file table with one fragmented file
        //   "frag.bin", consecutive=false, used_blocks=3, start_block=5
        let mut ft_block = vec![0u8; BLOCK_SIZE as usize];
        let mut entry = vec![0u8; 0x40];
        let name = b"frag.bin";
        entry[..name.len()].copy_from_slice(name);
        // flags: name length 8, consecutive OFF, dir OFF
        entry[0x28] = name.len() as u8 & 0x3F;
        entry[0x2C] = 3; // used_blocks
        entry[0x2F] = 5; // start_block = 5
        entry[0x32..0x34].copy_from_slice(&(-1i16).to_be_bytes());
        // file_size = 3 * BLOCK_SIZE = 0x3000
        entry[0x34..0x38].copy_from_slice(&0x3000u32.to_be_bytes());
        ft_block[..0x40].copy_from_slice(&entry);
        buf.extend_from_slice(&ft_block);

        // Blocks 1..=5: each filled with a single byte equal to the block index.
        for b in 1u8..=5 {
            let block_data = vec![b; BLOCK_SIZE as usize];
            buf.extend_from_slice(&block_data);
        }

        // L0 hash block sits AT block_index = 0xAA per the type-1 layout —
        // but our package has only 6 data blocks, so the hash block lives
        // beyond the file proper. We need to plant next-block pointers
        // somewhere the reader will look for them.
        //
        // For data block N, the next-block pointer lives in the L0 hash
        // block at index (N / BLOCKS_PER_L0) * BLOCKS_PER_L0 + (BLOCKS_PER_L0 - 1) + 1
        // = the position after the group. With BLOCKS_PER_L0 = 0xAA and all
        // our blocks in group 0, that's the hash block at block_to_byte_offset(0xAA) - BLOCK_SIZE.
        //
        // Compute that offset and pad the buffer to it, then plant the
        // hash entries for blocks 5 → 3 → 1.
        let hash_block_offset =
            (FIRST_DATA_BLOCK_OFFSET + (BLOCKS_PER_L0 as u64) * BLOCK_SIZE) as usize;
        if buf.len() < hash_block_offset + BLOCK_SIZE as usize {
            buf.resize(hash_block_offset + BLOCK_SIZE as usize, 0);
        }
        let plant_next = |buf: &mut [u8], block: u32, next: u32| {
            let entry_off = hash_block_offset + (block as usize % 0xAA) * 24 + 0x15;
            buf[entry_off] = (next & 0xFF) as u8;
            buf[entry_off + 1] = ((next >> 8) & 0xFF) as u8;
            buf[entry_off + 2] = ((next >> 16) & 0xFF) as u8;
        };
        plant_next(&mut buf, 5, 3);
        plant_next(&mut buf, 3, 1);
        // block 1 has no successor; plant 0xFFFFFF (end-of-chain)
        plant_next(&mut buf, 1, 0xFFFFFF);

        buf
    }

    #[test]
    fn read_chain_follows_non_consecutive_pointers() {
        let bytes = synthetic_package_with_fragmented_file();
        let mut pkg = StfsPackage::open(Cursor::new(bytes)).expect("open");
        let entries = pkg.entries().expect("entries");
        let entry = &entries[0];
        assert!(!entry.consecutive);
        let chain = pkg.read_block_chain(entry).expect("chain");
        assert_eq!(chain, vec![5, 3, 1]);
    }

    #[test]
    fn read_chain_uses_fast_path_for_consecutive_files() {
        let entries = vec![fe("file.bin", false, -1, 3 * 0x1000, 1)];
        // Mark consecutive (fe sets it true)
        let bytes = synthetic_package_with_one_file_table_block(&entries);
        let mut pkg = StfsPackage::open(Cursor::new(bytes)).expect("open");
        let listed = pkg.entries().expect("entries");
        // fe() sets used_blocks=1, override for this test
        let mut entry = listed[0].clone();
        entry.used_blocks = 3;
        entry.consecutive = true;
        let chain = pkg.read_block_chain(&entry).expect("chain");
        assert_eq!(chain, vec![1, 2, 3]);
    }

    #[test]
    fn read_chain_caps_at_used_blocks_to_reject_cycles() {
        use crate::stfs::block_translator::{BLOCK_SIZE, BLOCKS_PER_L0, FIRST_DATA_BLOCK_OFFSET};

        // Plant a cycle: 5 → 3 → 5 → 3 → ...
        // Start from the unpatched bytes and rewrite the next-pointer for block 3.
        let mut raw = synthetic_package_with_fragmented_file();
        let hash_block_offset =
            (FIRST_DATA_BLOCK_OFFSET + (BLOCKS_PER_L0 as u64) * BLOCK_SIZE) as usize;
        // Rewrite next-pointer for block 3 to point back at block 5
        let entry_off = hash_block_offset + 3 * 24 + 0x15;
        raw[entry_off] = 5;
        raw[entry_off + 1] = 0;
        raw[entry_off + 2] = 0;
        let mut pkg = StfsPackage::open(Cursor::new(raw)).expect("open");
        let entry = pkg.entries().expect("entries")[0].clone();
        // used_blocks=3; walk should return exactly 3 blocks even though
        // the chain loops 5 → 3 → 5 → 3 → ...
        let chain = pkg.read_block_chain(&entry).expect("chain");
        assert_eq!(chain.len(), 3);
    }

    #[test]
    fn read_file_streams_consecutive_file_contents() {
        // Build a package with one consecutive 2-block file containing
        // [0xAA; BLOCK_SIZE] then [0xBB; BLOCK_SIZE - 1] (last block 1 byte short).
        use crate::stfs::block_translator::{BLOCK_SIZE, FIRST_DATA_BLOCK_OFFSET};

        let mut buf = vec![0u8; MIN_HEADER_BYTES];
        buf[0..4].copy_from_slice(b"LIVE");
        buf[0x379] = 0x24;
        buf[0x37B] = 0x01;
        buf[0x37C..0x37E].copy_from_slice(&1u16.to_be_bytes());
        buf[0x395..0x399].copy_from_slice(&3u32.to_be_bytes()); // total_alloc = 3

        let block_zero = FIRST_DATA_BLOCK_OFFSET as usize;
        buf.resize(block_zero, 0);

        let mut ft = vec![0u8; BLOCK_SIZE as usize];
        let name = b"data.bin";
        ft[..name.len()].copy_from_slice(name);
        ft[0x28] = (name.len() as u8 & 0x3F) | 0x40; // consecutive
        ft[0x2C] = 2; // used_blocks
        ft[0x2F] = 1; // start_block
        ft[0x32..0x34].copy_from_slice(&(-1i16).to_be_bytes());
        let file_size: u32 = (BLOCK_SIZE as u32) + (BLOCK_SIZE as u32) - 1;
        ft[0x34..0x38].copy_from_slice(&file_size.to_be_bytes());
        buf.extend_from_slice(&ft);

        buf.extend_from_slice(&[0xAA; 0x1000]); // block 1
        let mut last_block = vec![0xBBu8; 0x1000];
        last_block[BLOCK_SIZE as usize - 1] = 0; // last byte unused
        buf.extend_from_slice(&last_block);

        let mut pkg = StfsPackage::open(Cursor::new(buf)).expect("open");
        let entries = pkg.entries().expect("entries");
        let entry = &entries[0];
        let mut sink = Vec::new();
        pkg.read_file(entry, &mut sink).expect("read_file");

        assert_eq!(sink.len(), file_size as usize);
        assert!(sink[..0x1000].iter().all(|&b| b == 0xAA));
        assert!(sink[0x1000..].iter().all(|&b| b == 0xBB));
    }

    use tempfile::TempDir;

    #[test]
    fn extract_to_host_writes_nested_tree() {
        // Build: /Media/  -> /Media/cover.png (4 bytes "ABCD")
        //        /default.xex (4 bytes "MZRX")
        use crate::stfs::block_translator::{BLOCK_SIZE, FIRST_DATA_BLOCK_OFFSET};

        let mut buf = vec![0u8; MIN_HEADER_BYTES];
        buf[0..4].copy_from_slice(b"LIVE");
        buf[0x379] = 0x24;
        buf[0x37B] = 0x01;
        buf[0x37C..0x37E].copy_from_slice(&1u16.to_be_bytes());
        buf[0x395..0x399].copy_from_slice(&3u32.to_be_bytes()); // total_alloc = 3

        let block_zero = FIRST_DATA_BLOCK_OFFSET as usize;
        buf.resize(block_zero, 0);

        let mut ft = vec![0u8; BLOCK_SIZE as usize];
        // Entry 0: dir "Media", parent -1 (root)
        let mut e0 = vec![0u8; 0x40];
        e0[..5].copy_from_slice(b"Media");
        e0[0x28] = 0x05 | 0x40 | 0x80; // dir + consecutive (irrelevant for dir)
        e0[0x32..0x34].copy_from_slice(&(-1i16).to_be_bytes());
        ft[..0x40].copy_from_slice(&e0);
        // Entry 1: file "cover.png", parent 0 (Media), start_block 1
        let mut e1 = vec![0u8; 0x40];
        e1[..9].copy_from_slice(b"cover.png");
        e1[0x28] = 0x09 | 0x40;
        e1[0x2C] = 1; // used_blocks
        e1[0x2F] = 1; // start_block
        e1[0x32..0x34].copy_from_slice(&0i16.to_be_bytes());
        e1[0x34..0x38].copy_from_slice(&4u32.to_be_bytes()); // size
        ft[0x40..0x80].copy_from_slice(&e1);
        // Entry 2: file "default.xex", parent -1 (root), start_block 2
        let mut e2 = vec![0u8; 0x40];
        e2[..11].copy_from_slice(b"default.xex");
        e2[0x28] = 0x0B | 0x40;
        e2[0x2C] = 1;
        e2[0x2F] = 2;
        e2[0x32..0x34].copy_from_slice(&(-1i16).to_be_bytes());
        e2[0x34..0x38].copy_from_slice(&4u32.to_be_bytes());
        ft[0x80..0xC0].copy_from_slice(&e2);
        buf.extend_from_slice(&ft);

        // Block 1: cover.png contents
        let mut b1 = vec![0u8; BLOCK_SIZE as usize];
        b1[..4].copy_from_slice(b"ABCD");
        buf.extend_from_slice(&b1);
        // Block 2: default.xex contents
        let mut b2 = vec![0u8; BLOCK_SIZE as usize];
        b2[..4].copy_from_slice(b"MZRX");
        buf.extend_from_slice(&b2);

        let mut pkg = StfsPackage::open(Cursor::new(buf)).expect("open");
        let tmp = TempDir::new().expect("tmp");
        let report = extract_to_host(&mut pkg, tmp.path(), None).expect("extract");
        assert_eq!(report.files, 2);
        assert_eq!(report.directories, 1);
        assert_eq!(report.bytes, 8);

        let cover = std::fs::read(tmp.path().join("Media/cover.png")).expect("read cover");
        assert_eq!(cover, b"ABCD");
        let xex = std::fs::read(tmp.path().join("default.xex")).expect("read xex");
        assert_eq!(xex, b"MZRX");
    }
}
