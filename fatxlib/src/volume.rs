//! FATX volume — the main interface for reading and writing a FATX filesystem.
//!
//! A `FatxVolume` wraps a seekable reader/writer (file, block device, or disk image)
//! and provides methods to navigate directories, read files, and perform write operations.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};

use log::{info, warn};

use crate::error::{FatxError, Result};
use crate::platform::DeviceInfo;
use crate::types::*;

/// A mounted FATX volume backed by a seekable stream.
pub struct FatxVolume<T: Read + Write + Seek> {
    /// The underlying device / file handle.
    inner: T,
    /// Byte offset where this FATX partition starts within the device.
    /// (0 if the device *is* the partition.)
    partition_offset: u64,
    /// Parsed superblock.
    pub superblock: Superblock,
    /// Whether this volume uses FAT16 or FAT32.
    pub fat_type: FatType,
    /// Total number of clusters in the data area.
    pub total_clusters: u32,
    /// Byte offset of the FAT table (relative to partition start).
    fat_offset: u64,
    /// Size of the FAT in bytes (rounded up to 4KB boundary per original Xbox driver).
    #[allow(dead_code)]
    fat_size: u64,
    /// Byte offset of the data/cluster area (relative to partition start).
    data_offset: u64,
    /// Total size of this partition in bytes.
    partition_size: u64,
    /// Whether this volume uses big-endian on-disk format (Xbox 360 XTAF).
    big_endian: bool,
    /// In-memory copy of the entire FAT for fast lookup and allocation.
    fat_cache: Vec<u8>,
    /// Whether the FAT cache has been modified and needs to be flushed.
    fat_dirty: bool,
    /// Device I/O parameters (None for non-device backends like Cursor).
    device_info: Option<DeviceInfo>,
    /// I/O alignment in bytes. 512 for standard devices/files, 4096 when F_NOCACHE is active.
    alignment: u64,
    /// Last allocated cluster (next-fit hint). Allocation starts scanning from here.
    prev_free: u32,
    /// Cached count of free clusters (maintained incrementally).
    free_cluster_count: u32,
    /// Cached count of bad clusters (computed once at open, doesn't change).
    bad_cluster_count: u32,
    /// Byte ranges within fat_cache that have been modified since last flush.
    /// Each entry is (start_byte_offset, end_byte_offset) within fat_cache.
    dirty_ranges: Vec<(usize, usize)>,
    /// Free-cluster bitmap: 1 bit per cluster, set = free. Enables O(1) allocation
    /// via word-level scanning with trailing_zeros(). For a 500GB partition with
    /// 16KB clusters (~30M clusters), this is ~3.6MB vs scanning 120MB of FAT entries.
    free_bitmap: Vec<u64>,
}

/// Progress callback: `(fatx_path, file_size, total_bytes_so_far)`.
type ProgressFn<'a> = &'a dyn Fn(&str, u64, u64);

struct CopyFromHostState<'a> {
    progress: Option<ProgressFn<'a>>,
    should_abort: Option<&'a dyn Fn() -> bool>,
    flush_every_files: usize,
    flush_every_bytes: u64,
    files_since_flush: usize,
    bytes_since_flush: u64,
}

#[doc(hidden)]
#[must_use = "WriteSession must be commit()'d or cancel()'d"]
pub struct WriteSession {
    parent_cluster: u32,
    first_cluster: u32,
    old_count: usize,
    chain: Vec<u32>,
    new_size: usize,
    finalized: bool,
}

impl WriteSession {
    pub fn clusters(&self) -> &[u32] {
        &self.chain
    }
}

impl Drop for WriteSession {
    fn drop(&mut self) {
        if !self.finalized {
            warn!(
                "WriteSession for cluster {} dropped without commit/cancel; uncommitted FAT reservations may remain",
                self.first_cluster
            );
        }
    }
}

impl<T: Read + Write + Seek> FatxVolume<T> {
    /// Open a FATX volume.
    ///
    /// - `inner`: A seekable read/write handle to the device or image file.
    /// - `partition_offset`: Byte offset where the FATX partition begins.
    /// - `partition_size`: Size of the partition in bytes (0 = auto-detect from stream length).
    pub fn open(mut inner: T, partition_offset: u64, partition_size: u64) -> Result<Self> {
        // Determine actual partition size if not provided.
        let partition_size = if partition_size == 0 {
            let end = inner.seek(SeekFrom::End(0))?;
            end.saturating_sub(partition_offset)
        } else {
            partition_size
        };

        if partition_size < SUPERBLOCK_SIZE + SECTOR_SIZE {
            return Err(FatxError::VolumeTooSmall);
        }

        // Read the entire 4 KB superblock at once.
        // macOS raw devices require sector-aligned reads (512 bytes minimum),
        // so we read the full superblock rather than individual fields.
        inner.seek(SeekFrom::Start(partition_offset))?;
        let mut sb_buf = [0u8; SUPERBLOCK_SIZE as usize];
        inner.read_exact(&mut sb_buf)?;

        let magic: [u8; 4] = [sb_buf[0], sb_buf[1], sb_buf[2], sb_buf[3]];
        info!(
            "Read magic at offset 0x{:X}: {:02X} {:02X} {:02X} {:02X} (\"{}\")",
            partition_offset,
            magic[0],
            magic[1],
            magic[2],
            magic[3],
            String::from_utf8_lossy(&magic)
        );
        if !is_valid_magic(&magic) {
            return Err(FatxError::BadMagic(magic));
        }

        // Xbox 360 XTAF uses big-endian for superblock fields;
        // original Xbox FATX uses little-endian.
        let is_xtaf = &magic == b"XTAF";
        let (volume_id, sectors_per_cluster, fat_copies) = if is_xtaf {
            (
                u32::from_be_bytes([sb_buf[4], sb_buf[5], sb_buf[6], sb_buf[7]]),
                u32::from_be_bytes([sb_buf[8], sb_buf[9], sb_buf[10], sb_buf[11]]),
                u16::from_be_bytes([sb_buf[12], sb_buf[13]]),
            )
        } else {
            (
                u32::from_le_bytes([sb_buf[4], sb_buf[5], sb_buf[6], sb_buf[7]]),
                u32::from_le_bytes([sb_buf[8], sb_buf[9], sb_buf[10], sb_buf[11]]),
                u16::from_le_bytes([sb_buf[12], sb_buf[13]]),
            )
        };

        // Validate sectors_per_cluster (must be a power of 2, typically 1..128)
        if sectors_per_cluster == 0
            || sectors_per_cluster > 128
            || !sectors_per_cluster.is_power_of_two()
        {
            return Err(FatxError::BadSectorsPerCluster(sectors_per_cluster));
        }

        let superblock = Superblock {
            magic,
            volume_id,
            sectors_per_cluster,
            fat_copies,
        };

        let cluster_size = superblock.cluster_size();
        info!(
            "FATX volume: id=0x{:08X}, cluster_size={}, fat_copies={}",
            volume_id, cluster_size, fat_copies
        );

        // Calculate layout — based on the original Xbox FATX driver:
        //   1. FAT starts immediately after the 4KB superblock
        //   2. FAT size is rounded UP to 4KB boundary
        //   3. Data clusters begin right after the (rounded) FAT
        //   4. The root directory occupies the first cluster in the data area
        let fat_offset = SUPERBLOCK_SIZE;

        // Total sectors available after superblock (superblock = 8 sectors)
        let total_sectors = (partition_size / SECTOR_SIZE) - (SUPERBLOCK_SIZE / SECTOR_SIZE);
        let spc = sectors_per_cluster as u64;

        // Determine FAT type using the original driver's formula:
        //   if (total_sectors - 260) / sectors_per_cluster >= 65525 => FAT32
        // The "260" accounts for the root directory overhead estimate.
        let cluster_estimate = total_sectors.saturating_sub(260) / spc;
        let fat_type = if cluster_estimate >= 65_525 {
            FatType::Fat32
        } else {
            FatType::Fat16
        };

        let entry_size = fat_type.entry_size();

        // Calculate cluster count and FAT size.
        //
        // The Xbox 360 XTAF driver uses a naive formula that does NOT subtract
        // FAT space from the cluster count:
        //     total_clusters = (partition_size - superblock) / cluster_size
        //
        // The original Xbox FATX driver subtracts FAT overhead:
        //     total_clusters ≈ total_data_bytes / (cluster_size + entry_size)
        //
        // Using the wrong formula shifts the data_offset and causes the root
        // directory (and all data) to be read from the wrong location.
        let total_clusters = if is_xtaf {
            ((partition_size - SUPERBLOCK_SIZE) / cluster_size) as u32
        } else {
            (total_sectors * SECTOR_SIZE / (cluster_size + entry_size)) as u32
        };
        let raw_fat_size = total_clusters as u64 * entry_size;

        // Round FAT size UP to 4KB boundary (as the original driver does)
        let fat_size = (raw_fat_size + 0xFFF) & !0xFFF;

        // Data area begins right after the rounded FAT
        let data_offset = fat_offset + fat_size;

        info!(
            "FAT type: {}, clusters: {}, FAT size: {} bytes, data offset: 0x{:X}",
            fat_type, total_clusters, fat_size, data_offset
        );
        info!(
            "Layout: partition=0x{:X}+{}, superblock=0x{:X}..0x{:X}, FAT=0x{:X}..0x{:X} (raw {}), data=0x{:X}..end",
            partition_offset, crate::partition::format_size(partition_size),
            partition_offset, partition_offset + SUPERBLOCK_SIZE,
            partition_offset + fat_offset, partition_offset + fat_offset + fat_size,
            crate::partition::format_size(raw_fat_size),
            partition_offset + data_offset,
        );

        // Read the entire FAT into memory for fast lookup and allocation.
        // On a 927 GB partition with 32 KB clusters this is ~120 MB — fits
        // comfortably in RAM and eliminates millions of individual seeks.
        let fat_abs = partition_offset + fat_offset;
        let fat_aligned_start = fat_abs & !0x1FF;
        let fat_pre_skip = (fat_abs - fat_aligned_start) as usize;
        let fat_total = fat_pre_skip + fat_size as usize;
        let fat_aligned_len = (fat_total + 511) & !511;

        inner.seek(SeekFrom::Start(fat_aligned_start))?;
        let mut fat_aligned_buf = vec![0u8; fat_aligned_len];
        inner.read_exact(&mut fat_aligned_buf)?;
        let fat_cache = fat_aligned_buf[fat_pre_skip..fat_pre_skip + fat_size as usize].to_vec();
        info!(
            "Loaded FAT into memory: {} bytes ({:.1} MB)",
            fat_cache.len(),
            fat_cache.len() as f64 / 1_048_576.0
        );

        // Compute initial free/bad cluster counts and build the free-cluster bitmap.
        // This is done once at open time; counts and bitmap are maintained incrementally.
        let entry_size = fat_type.entry_size() as usize;
        let mut free_cluster_count = 0u32;
        let mut bad_cluster_count = 0u32;
        // Bitmap: 1 bit per cluster, set = free. Need words for clusters 0..total_clusters+FIRST_CLUSTER.
        let bitmap_words = ((FIRST_CLUSTER + total_clusters) as usize).div_ceil(64);
        let mut free_bitmap = vec![0u64; bitmap_words];

        for cluster in FIRST_CLUSTER..(FIRST_CLUSTER + total_clusters) {
            let cache_offset = cluster as usize * entry_size;
            let is_free = match fat_type {
                FatType::Fat16 => {
                    let val = if is_xtaf {
                        u16::from_be_bytes([fat_cache[cache_offset], fat_cache[cache_offset + 1]])
                    } else {
                        u16::from_le_bytes([fat_cache[cache_offset], fat_cache[cache_offset + 1]])
                    };
                    if val == FAT16_FREE {
                        Some(true)
                    } else if val == FAT16_BAD {
                        Some(false)
                    } else {
                        None
                    }
                }
                FatType::Fat32 => {
                    let val = if is_xtaf {
                        u32::from_be_bytes([
                            fat_cache[cache_offset],
                            fat_cache[cache_offset + 1],
                            fat_cache[cache_offset + 2],
                            fat_cache[cache_offset + 3],
                        ])
                    } else {
                        u32::from_le_bytes([
                            fat_cache[cache_offset],
                            fat_cache[cache_offset + 1],
                            fat_cache[cache_offset + 2],
                            fat_cache[cache_offset + 3],
                        ])
                    };
                    if val == FAT32_FREE {
                        Some(true)
                    } else if val == FAT32_BAD {
                        Some(false)
                    } else {
                        None
                    }
                }
            };
            match is_free {
                Some(true) => {
                    free_cluster_count += 1;
                    // Set bit in bitmap: cluster N -> word N/64, bit N%64
                    let word = cluster as usize / 64;
                    let bit = cluster as usize % 64;
                    free_bitmap[word] |= 1u64 << bit;
                }
                Some(false) => bad_cluster_count += 1,
                None => {} // used cluster
            }
        }
        info!(
            "Cluster counts: {} free, {} bad, {} used (of {} total), bitmap {} words ({:.1} KB)",
            free_cluster_count,
            bad_cluster_count,
            total_clusters - free_cluster_count - bad_cluster_count,
            total_clusters,
            free_bitmap.len(),
            free_bitmap.len() as f64 * 8.0 / 1024.0,
        );

        Ok(FatxVolume {
            inner,
            partition_offset,
            superblock,
            fat_type,
            total_clusters,
            fat_offset,
            fat_size,
            data_offset,
            partition_size,
            big_endian: is_xtaf,
            fat_cache,
            fat_dirty: false,
            device_info: None,
            alignment: SECTOR_SIZE,
            prev_free: FIRST_CLUSTER,
            free_cluster_count,
            bad_cluster_count,
            dirty_ranges: Vec::new(),
            free_bitmap,
        })
    }

    /// Configure device I/O parameters for a raw block device.
    ///
    /// Call this after `open()` when the underlying stream is a raw device
    /// (e.g., `/dev/rdiskN`). Sets F_NOCACHE, F_RDAHEAD(0), and queries
    /// device-specific I/O parameters.
    ///
    /// Does nothing if `configure_device_io` returns None (not a block device).
    #[cfg(target_os = "macos")]
    pub fn configure_device(&mut self, fd: std::os::unix::io::RawFd) {
        if let Some(info) = crate::platform::configure_device_io(fd) {
            // With F_NOCACHE active, I/O must be page-aligned (4096 bytes).
            // Use the maximum of sector size, physical block size, and page size.
            let page_size = 4096u64;
            self.alignment = SECTOR_SIZE
                .max(info.physical_block_size as u64)
                .max(page_size);
            info!(
                "I/O alignment set to {} bytes (physical_block_size={}, F_NOCACHE requires {})",
                self.alignment, info.physical_block_size, page_size
            );
            self.device_info = Some(info);
        }
    }

    /// Stub for non-macOS platforms.
    #[cfg(not(target_os = "macos"))]
    pub fn configure_device(&mut self, _fd: i32) {
        // No-op on non-macOS
    }

    /// Get the device info, if available.
    pub fn device_info(&self) -> Option<&DeviceInfo> {
        self.device_info.as_ref()
    }

    // -----------------------------------------------------------------------
    // Endian-aware integer helpers
    // -----------------------------------------------------------------------

    fn read_u16(&self, buf: &[u8]) -> u16 {
        if self.big_endian {
            u16::from_be_bytes([buf[0], buf[1]])
        } else {
            u16::from_le_bytes([buf[0], buf[1]])
        }
    }

    fn read_u32(&self, buf: &[u8]) -> u32 {
        if self.big_endian {
            u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]])
        } else {
            u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]])
        }
    }

    fn write_u16_bytes(&self, val: u16) -> [u8; 2] {
        if self.big_endian {
            val.to_be_bytes()
        } else {
            val.to_le_bytes()
        }
    }

    fn write_u32_bytes(&self, val: u32) -> [u8; 4] {
        if self.big_endian {
            val.to_be_bytes()
        } else {
            val.to_le_bytes()
        }
    }

    // -----------------------------------------------------------------------
    // Low-level I/O helpers (sector-aligned for macOS raw devices)
    // -----------------------------------------------------------------------

    /// Absolute byte offset within the device for a partition-relative offset.
    fn abs_offset(&self, partition_rel: u64) -> u64 {
        self.partition_offset + partition_rel
    }

    /// Merge overlapping/adjacent dirty ranges into consolidated spans.
    fn merge_dirty_ranges(&self) -> Vec<(usize, usize)> {
        if self.dirty_ranges.is_empty() {
            return Vec::new();
        }
        let mut sorted = self.dirty_ranges.clone();
        sorted.sort_by_key(|r| r.0);

        let mut merged: Vec<(usize, usize)> = Vec::new();
        let (mut cur_start, mut cur_end) = sorted[0];

        for &(start, end) in &sorted[1..] {
            if start <= cur_end {
                // Overlapping or adjacent — extend
                cur_end = cur_end.max(end);
            } else {
                merged.push((cur_start, cur_end));
                cur_start = start;
                cur_end = end;
            }
        }
        merged.push((cur_start, cur_end));
        merged
    }

    /// Align `offset` down to `self.alignment` boundary.
    fn align_down(&self, offset: u64) -> u64 {
        let mask = self.alignment - 1;
        offset & !mask
    }

    /// Round `size` up to next `self.alignment` boundary.
    fn align_up(&self, size: usize) -> usize {
        let mask = self.alignment as usize - 1;
        (size + mask) & !mask
    }

    /// Read `buf.len()` bytes from a partition-relative offset.
    /// Handles alignment automatically for raw devices (512B default, 4KB with F_NOCACHE).
    fn read_at(&mut self, partition_rel: u64, buf: &mut [u8]) -> Result<()> {
        let abs = self.abs_offset(partition_rel);
        let aligned_start = self.align_down(abs);
        let pre_skip = (abs - aligned_start) as usize;
        let total_needed = pre_skip + buf.len();
        let aligned_len = self.align_up(total_needed);

        self.inner.seek(SeekFrom::Start(aligned_start))?;
        let mut aligned_buf = vec![0u8; aligned_len];
        self.inner.read_exact(&mut aligned_buf)?;
        buf.copy_from_slice(&aligned_buf[pre_skip..pre_skip + buf.len()]);
        Ok(())
    }

    /// Write `buf` at a partition-relative offset.
    /// For raw devices, does a read-modify-write if the write isn't aligned.
    fn write_at(&mut self, partition_rel: u64, buf: &[u8]) -> Result<()> {
        let abs = self.abs_offset(partition_rel);
        let aligned_start = self.align_down(abs);
        let pre_skip = (abs - aligned_start) as usize;
        let total_needed = pre_skip + buf.len();
        let aligned_len = self.align_up(total_needed);
        let align = self.alignment as usize;

        if pre_skip == 0 && buf.len().is_multiple_of(align) {
            // Already aligned — write directly
            self.inner.seek(SeekFrom::Start(abs))?;
            self.inner.write_all(buf)?;
        } else {
            // Read-modify-write
            self.inner.seek(SeekFrom::Start(aligned_start))?;
            let mut aligned_buf = vec![0u8; aligned_len];
            self.inner.read_exact(&mut aligned_buf)?;
            aligned_buf[pre_skip..pre_skip + buf.len()].copy_from_slice(buf);
            self.inner.seek(SeekFrom::Start(aligned_start))?;
            self.inner.write_all(&aligned_buf)?;
        }
        Ok(())
    }

    /// Returns the byte offset of the given cluster's data (partition-relative).
    fn cluster_offset(&self, cluster: u32) -> Result<u64> {
        if cluster < FIRST_CLUSTER || cluster >= FIRST_CLUSTER + self.total_clusters {
            return Err(FatxError::ClusterOutOfRange(
                cluster,
                FIRST_CLUSTER + self.total_clusters - 1,
            ));
        }
        Ok(self.data_offset + (cluster - FIRST_CLUSTER) as u64 * self.superblock.cluster_size())
    }

    // -----------------------------------------------------------------------
    // FAT operations
    // -----------------------------------------------------------------------

    /// Read a single FAT entry for the given cluster.
    /// Takes `&self` — only reads from the in-memory fat_cache, no device I/O.
    pub fn read_fat_entry(&self, cluster: u32) -> Result<FatEntry> {
        if cluster < FIRST_CLUSTER || cluster >= FIRST_CLUSTER + self.total_clusters {
            return Err(FatxError::ClusterOutOfRange(
                cluster,
                FIRST_CLUSTER + self.total_clusters - 1,
            ));
        }

        let cache_offset = (cluster as u64 * self.fat_type.entry_size()) as usize;
        let entry_size = self.fat_type.entry_size() as usize;
        if cache_offset + entry_size > self.fat_cache.len() {
            return Err(FatxError::ClusterOutOfRange(
                cluster,
                FIRST_CLUSTER + self.total_clusters - 1,
            ));
        }

        match self.fat_type {
            FatType::Fat16 => {
                let buf = [
                    self.fat_cache[cache_offset],
                    self.fat_cache[cache_offset + 1],
                ];
                let val = self.read_u16(&buf);
                Ok(match val {
                    FAT16_FREE => FatEntry::Free,
                    FAT16_BAD => FatEntry::Bad,
                    v if v >= FAT16_EOC => FatEntry::EndOfChain,
                    v => {
                        let next = v as u32;
                        if next < FIRST_CLUSTER || next >= FIRST_CLUSTER + self.total_clusters {
                            return Err(FatxError::CorruptChain(cluster));
                        }
                        FatEntry::Next(next)
                    }
                })
            }
            FatType::Fat32 => {
                let buf = [
                    self.fat_cache[cache_offset],
                    self.fat_cache[cache_offset + 1],
                    self.fat_cache[cache_offset + 2],
                    self.fat_cache[cache_offset + 3],
                ];
                let val = self.read_u32(&buf);
                Ok(match val {
                    FAT32_FREE => FatEntry::Free,
                    FAT32_BAD => FatEntry::Bad,
                    v if v >= FAT32_EOC => FatEntry::EndOfChain,
                    v => {
                        if v < FIRST_CLUSTER || v >= FIRST_CLUSTER + self.total_clusters {
                            return Err(FatxError::CorruptChain(cluster));
                        }
                        FatEntry::Next(v)
                    }
                })
            }
        }
    }

    /// Write a FAT entry for the given cluster (updates in-memory cache).
    /// Changes are flushed to disk when `flush()` is called.
    /// Maintains `free_cluster_count` incrementally on state transitions.
    pub fn write_fat_entry(&mut self, cluster: u32, entry: FatEntry) -> Result<()> {
        // Read old entry to detect state transitions for free count maintenance
        let old_entry = self.read_fat_entry(cluster)?;
        let was_free = matches!(old_entry, FatEntry::Free);
        let will_be_free = matches!(entry, FatEntry::Free);

        let cache_offset = (cluster as u64 * self.fat_type.entry_size()) as usize;

        match self.fat_type {
            FatType::Fat16 => {
                let val: u16 = match entry {
                    FatEntry::Free => FAT16_FREE,
                    FatEntry::EndOfChain => FAT16_EOC,
                    FatEntry::Bad => FAT16_BAD,
                    FatEntry::Next(c) => c as u16,
                };
                let bytes = self.write_u16_bytes(val);
                self.fat_cache[cache_offset] = bytes[0];
                self.fat_cache[cache_offset + 1] = bytes[1];
            }
            FatType::Fat32 => {
                let val: u32 = match entry {
                    FatEntry::Free => FAT32_FREE,
                    FatEntry::EndOfChain => FAT32_EOC,
                    FatEntry::Bad => FAT32_BAD,
                    FatEntry::Next(c) => c,
                };
                let bytes = self.write_u32_bytes(val);
                self.fat_cache[cache_offset] = bytes[0];
                self.fat_cache[cache_offset + 1] = bytes[1];
                self.fat_cache[cache_offset + 2] = bytes[2];
                self.fat_cache[cache_offset + 3] = bytes[3];
            }
        }

        // Maintain free cluster count and bitmap
        let word = cluster as usize / 64;
        let bit = cluster as usize % 64;
        if was_free && !will_be_free {
            self.free_cluster_count = self.free_cluster_count.saturating_sub(1);
            self.free_bitmap[word] &= !(1u64 << bit); // clear bit
        } else if !was_free && will_be_free {
            self.free_cluster_count += 1;
            self.free_bitmap[word] |= 1u64 << bit; // set bit
        }

        // Record dirty range for partial flush
        let entry_size = self.fat_type.entry_size() as usize;
        self.dirty_ranges
            .push((cache_offset, cache_offset + entry_size));

        self.fat_dirty = true;
        Ok(())
    }

    /// Follow the cluster chain starting from `start_cluster` and return
    /// the list of clusters in order.
    /// Takes `&self` — only reads from the in-memory fat_cache, no device I/O.
    pub fn read_chain(&self, start_cluster: u32) -> Result<Vec<u32>> {
        use std::collections::HashSet;

        let mut chain = Vec::new();
        let mut seen = HashSet::new();
        let mut current = start_cluster;
        let max_iters = self.total_clusters as usize + 1; // safety bound

        for _ in 0..max_iters {
            if !seen.insert(current) {
                warn!("Cluster chain cycle detected at {}", current);
                return Err(FatxError::CorruptChain(current));
            }
            chain.push(current);
            match self.read_fat_entry(current)? {
                FatEntry::EndOfChain => break,
                FatEntry::Next(next) => current = next,
                FatEntry::Free => {
                    warn!("Cluster chain hit free cluster at {}", current);
                    return Err(FatxError::CorruptChain(current));
                }
                FatEntry::Bad => {
                    warn!("Cluster chain hit bad cluster at {}", current);
                    return Err(FatxError::CorruptChain(current));
                }
            }
        }

        if chain.len() > self.total_clusters as usize {
            warn!(
                "Cluster chain exceeded total cluster count from {}",
                start_cluster
            );
            return Err(FatxError::CorruptChain(current));
        }

        Ok(chain)
    }

    /// Find a free cluster via bitmap scan and mark it as end-of-chain.
    /// Uses next-fit: starts scanning from `prev_free + 1`, wraps around if needed.
    /// Scans the free_bitmap (1 bit/cluster) which is 32x faster than FAT entries.
    pub fn allocate_cluster(&mut self) -> Result<u32> {
        if self.free_cluster_count == 0 {
            return Err(FatxError::DiskFull);
        }

        let end = FIRST_CLUSTER + self.total_clusters;
        let start_from = if self.prev_free + 1 >= end {
            FIRST_CLUSTER
        } else {
            self.prev_free + 1
        };

        if let Some(cluster) = self.bitmap_find_free(start_from, end) {
            self.write_fat_entry(cluster, FatEntry::EndOfChain)?;
            self.prev_free = cluster;
            return Ok(cluster);
        }

        // Wraparound
        if start_from > FIRST_CLUSTER {
            if let Some(cluster) = self.bitmap_find_free(FIRST_CLUSTER, start_from) {
                self.write_fat_entry(cluster, FatEntry::EndOfChain)?;
                self.prev_free = cluster;
                return Ok(cluster);
            }
        }

        Err(FatxError::DiskFull)
    }

    /// Find the first free cluster in the bitmap between `from` (inclusive) and `to` (exclusive).
    fn bitmap_find_free(&self, from: u32, to: u32) -> Option<u32> {
        let from = from as usize;
        let to = to as usize;
        let start_word = from / 64;
        let end_word = to.div_ceil(64);

        for word_idx in start_word..end_word.min(self.free_bitmap.len()) {
            let mut word = self.free_bitmap[word_idx];
            if word == 0 {
                continue;
            }

            // Mask out bits before `from` in the starting word
            if word_idx == start_word {
                let start_bit = from % 64;
                word &= !((1u64 << start_bit) - 1);
                if word == 0 {
                    continue;
                }
            }

            // Find first set bit
            let bit = word.trailing_zeros() as usize;
            let cluster = word_idx * 64 + bit;

            if cluster >= to {
                return None;
            }
            return Some(cluster as u32);
        }
        None
    }

    /// Allocate `count` clusters and chain them together. Returns the first cluster.
    /// Uses bitmap scanning from `prev_free + 1`, wraps around if needed.
    pub fn allocate_chain(&mut self, count: usize) -> Result<u32> {
        if count == 0 {
            return Err(FatxError::DiskFull);
        }

        if (self.free_cluster_count as usize) < count {
            return Err(FatxError::DiskFull);
        }

        let end = FIRST_CLUSTER + self.total_clusters;
        let start_from = if self.prev_free + 1 >= end {
            FIRST_CLUSTER
        } else {
            self.prev_free + 1
        };

        let mut allocated = Vec::with_capacity(count);
        let mut cursor = start_from;

        // Pass 1: from prev_free+1 to end
        while allocated.len() < count {
            match self.bitmap_find_free(cursor, end) {
                Some(cluster) => {
                    allocated.push(cluster);
                    cursor = cluster + 1;
                }
                None => break,
            }
        }

        // Pass 2: wraparound from beginning
        if allocated.len() < count && start_from > FIRST_CLUSTER {
            cursor = FIRST_CLUSTER;
            while allocated.len() < count {
                match self.bitmap_find_free(cursor, start_from) {
                    Some(cluster) => {
                        allocated.push(cluster);
                        cursor = cluster + 1;
                    }
                    None => break,
                }
            }
        }

        if allocated.len() < count {
            return Err(FatxError::DiskFull);
        }

        // Chain them together
        for i in 0..allocated.len() - 1 {
            self.write_fat_entry(allocated[i], FatEntry::Next(allocated[i + 1]))?;
        }
        self.write_fat_entry(*allocated.last().unwrap(), FatEntry::EndOfChain)?;

        // Update prev_free to the last allocated cluster
        self.prev_free = *allocated.last().unwrap();

        Ok(allocated[0])
    }

    /// Free all clusters in a chain starting at `start_cluster`.
    pub fn free_chain(&mut self, start_cluster: u32) -> Result<()> {
        // Tolerant chain walk: free clusters until we hit end-of-chain,
        // a free cluster, or a bad cluster. This handles corrupt chains
        // from interrupted writes gracefully.
        let mut current = start_cluster;
        let max_iters = self.total_clusters as usize + 1;

        for _ in 0..max_iters {
            let entry = self.read_fat_entry(current)?;
            self.write_fat_entry(current, FatEntry::Free)?;
            match entry {
                FatEntry::Next(next) => current = next,
                FatEntry::EndOfChain => break,
                FatEntry::Free => {
                    warn!(
                        "free_chain: hit already-free cluster at {}, stopping",
                        current
                    );
                    break;
                }
                FatEntry::Bad => {
                    warn!("free_chain: hit bad cluster at {}, stopping", current);
                    break;
                }
            }
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Cluster I/O
    // -----------------------------------------------------------------------

    /// Read a full cluster into `buf`. The buffer must be `cluster_size` bytes.
    pub fn read_cluster(&mut self, cluster: u32, buf: &mut [u8]) -> Result<()> {
        let offset = self.cluster_offset(cluster)?;
        self.read_at(offset, buf)?;
        Ok(())
    }

    /// Write a full cluster from `buf`.
    pub fn write_cluster(&mut self, cluster: u32, buf: &[u8]) -> Result<()> {
        let offset = self.cluster_offset(cluster)?;
        self.write_at(offset, buf)?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Directory entry I/O
    // -----------------------------------------------------------------------

    /// Parse a single 64-byte directory entry at the given partition-relative offset.
    /// Uses sector-aligned I/O so it works on macOS raw devices.
    fn read_dirent_at(&mut self, partition_rel: u64) -> Result<DirectoryEntry> {
        let mut buf = [0u8; DIRENT_SIZE];
        self.read_at(partition_rel, &mut buf)?;
        Ok(self.parse_dirent_buf(&buf))
    }

    fn parse_dirent_buf(&self, buf: &[u8; DIRENT_SIZE]) -> DirectoryEntry {
        let filename_len = buf[0];
        let attributes = FileAttributes::from_bits_truncate(buf[1]);
        let mut filename_raw = [0u8; MAX_FILENAME_LEN];
        filename_raw.copy_from_slice(&buf[2..2 + MAX_FILENAME_LEN]);
        let first_cluster = self.read_u32(&[buf[44], buf[45], buf[46], buf[47]]);
        let file_size = self.read_u32(&[buf[48], buf[49], buf[50], buf[51]]);
        // XTAF (Xbox 360) stores timestamps as date-then-time at each pair of offsets,
        // while original FATX stores time-then-date. Both are 2-byte fields.
        let (creation_time, creation_date) = if self.big_endian {
            (
                self.read_u16(&[buf[54], buf[55]]),
                self.read_u16(&[buf[52], buf[53]]),
            )
        } else {
            (
                self.read_u16(&[buf[52], buf[53]]),
                self.read_u16(&[buf[54], buf[55]]),
            )
        };
        let (write_time, write_date) = if self.big_endian {
            (
                self.read_u16(&[buf[58], buf[59]]),
                self.read_u16(&[buf[56], buf[57]]),
            )
        } else {
            (
                self.read_u16(&[buf[56], buf[57]]),
                self.read_u16(&[buf[58], buf[59]]),
            )
        };
        let (access_time, access_date) = if self.big_endian {
            (
                self.read_u16(&[buf[62], buf[63]]),
                self.read_u16(&[buf[60], buf[61]]),
            )
        } else {
            (
                self.read_u16(&[buf[60], buf[61]]),
                self.read_u16(&[buf[62], buf[63]]),
            )
        };

        DirectoryEntry {
            filename_len,
            attributes,
            filename_raw,
            first_cluster,
            file_size,
            creation_time,
            creation_date,
            write_time,
            write_date,
            access_time,
            access_date,
        }
    }

    /// Read all valid directory entries from a directory cluster chain.
    pub fn read_directory(&mut self, first_cluster: u32) -> Result<Vec<DirectoryEntry>> {
        let chain = self.read_chain(first_cluster)?;
        let cluster_size = self.superblock.cluster_size() as usize;
        let entries_per_cluster = cluster_size / DIRENT_SIZE;
        let mut entries = Vec::new();

        for &cluster in &chain {
            let base_offset = self.cluster_offset(cluster)?;

            for slot in 0..entries_per_cluster {
                let slot_offset = base_offset + (slot * DIRENT_SIZE) as u64;
                let entry = self.read_dirent_at(slot_offset)?;
                if entry.is_end() {
                    return Ok(entries);
                }
                if !entry.is_deleted() {
                    entries.push(entry);
                }
            }
        }

        Ok(entries)
    }

    /// Read the root directory entries (root directory starts at cluster 1).
    pub fn read_root_directory(&mut self) -> Result<Vec<DirectoryEntry>> {
        self.read_directory(FIRST_CLUSTER)
    }

    /// Resolve a path like "/saves/game1.sav" into directory entries along the way,
    /// returning the final entry.
    pub fn resolve_path(&mut self, path: &str) -> Result<DirectoryEntry> {
        let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

        if parts.is_empty() {
            // Root directory pseudo-entry
            return Ok(DirectoryEntry {
                filename_len: 1,
                attributes: FileAttributes::DIRECTORY,
                filename_raw: [0xFF; MAX_FILENAME_LEN],
                first_cluster: FIRST_CLUSTER,
                file_size: 0,
                creation_time: 0,
                creation_date: 0,
                write_time: 0,
                write_date: 0,
                access_time: 0,
                access_date: 0,
            });
        }

        let mut current_cluster = FIRST_CLUSTER;

        for (i, part) in parts.iter().enumerate() {
            let entries = self.read_directory(current_cluster)?;
            let found = entries
                .into_iter()
                .find(|e| e.filename().eq_ignore_ascii_case(part));

            match found {
                Some(entry) => {
                    if i < parts.len() - 1 {
                        // Intermediate path component must be a directory
                        if !entry.is_directory() {
                            return Err(FatxError::NotADirectory(part.to_string()));
                        }
                        current_cluster = entry.first_cluster;
                    } else {
                        return Ok(entry);
                    }
                }
                None => return Err(FatxError::FileNotFound(part.to_string())),
            }
        }

        unreachable!()
    }

    // -----------------------------------------------------------------------
    // File reading
    // -----------------------------------------------------------------------

    /// Read the full contents of a file given its directory entry.
    pub fn read_file(&mut self, entry: &DirectoryEntry) -> Result<Vec<u8>> {
        self.read_file_range(entry, 0, entry.file_size as usize)
    }

    /// Read a byte range from a file given its directory entry.
    pub fn read_file_range(
        &mut self,
        entry: &DirectoryEntry,
        offset: u64,
        count: usize,
    ) -> Result<Vec<u8>> {
        if entry.is_directory() {
            return Err(FatxError::IsADirectory(entry.filename()));
        }

        let file_size = entry.file_size as usize;
        let start = offset as usize;
        if start >= file_size || count == 0 {
            return Ok(Vec::new());
        }

        let chain = self.read_chain(entry.first_cluster)?;
        let cluster_size = self.superblock.cluster_size() as usize;
        let mut data = Vec::with_capacity(count.min(file_size - start));
        let mut remaining = count.min(file_size - start);
        let start_cluster_idx = start / cluster_size;
        let mut offset_in_cluster = start % cluster_size;

        for &cluster in chain.iter().skip(start_cluster_idx) {
            let to_read = remaining.min(cluster_size - offset_in_cluster);
            let mut buf = vec![0u8; cluster_size];
            self.read_cluster(cluster, &mut buf)?;
            data.extend_from_slice(&buf[offset_in_cluster..offset_in_cluster + to_read]);
            remaining -= to_read;
            offset_in_cluster = 0;
            if remaining == 0 {
                break;
            }
        }

        Ok(data)
    }

    /// Read a file by path.
    pub fn read_file_by_path(&mut self, path: &str) -> Result<Vec<u8>> {
        let entry = self.resolve_path(path)?;
        self.read_file(&entry)
    }

    // -----------------------------------------------------------------------
    // Write operations
    // -----------------------------------------------------------------------

    /// Validate a filename for FATX.
    fn validate_filename(name: &str) -> Result<()> {
        if name.len() > MAX_FILENAME_LEN {
            return Err(FatxError::FilenameTooLong(name.len(), MAX_FILENAME_LEN));
        }
        if name.is_empty() {
            return Err(FatxError::FilenameTooLong(0, MAX_FILENAME_LEN));
        }
        for ch in name.chars() {
            if ch == '\0' || ch as u32 > 127 {
                return Err(FatxError::InvalidFilenameChar(ch));
            }
        }
        Ok(())
    }

    /// Serialize a DirectoryEntry back to its 64-byte on-disk form.
    fn serialize_dirent(&self, entry: &DirectoryEntry) -> [u8; DIRENT_SIZE] {
        let mut buf = [0u8; DIRENT_SIZE];
        buf[0] = entry.filename_len;
        buf[1] = entry.attributes.bits();
        buf[2..2 + MAX_FILENAME_LEN].copy_from_slice(&entry.filename_raw);
        buf[44..48].copy_from_slice(&self.write_u32_bytes(entry.first_cluster));
        buf[48..52].copy_from_slice(&self.write_u32_bytes(entry.file_size));
        // XTAF stores date-then-time; FATX stores time-then-date
        if self.big_endian {
            buf[52..54].copy_from_slice(&self.write_u16_bytes(entry.creation_date));
            buf[54..56].copy_from_slice(&self.write_u16_bytes(entry.creation_time));
            buf[56..58].copy_from_slice(&self.write_u16_bytes(entry.write_date));
            buf[58..60].copy_from_slice(&self.write_u16_bytes(entry.write_time));
            buf[60..62].copy_from_slice(&self.write_u16_bytes(entry.access_date));
            buf[62..64].copy_from_slice(&self.write_u16_bytes(entry.access_time));
        } else {
            buf[52..54].copy_from_slice(&self.write_u16_bytes(entry.creation_time));
            buf[54..56].copy_from_slice(&self.write_u16_bytes(entry.creation_date));
            buf[56..58].copy_from_slice(&self.write_u16_bytes(entry.write_time));
            buf[58..60].copy_from_slice(&self.write_u16_bytes(entry.write_date));
            buf[60..62].copy_from_slice(&self.write_u16_bytes(entry.access_time));
            buf[62..64].copy_from_slice(&self.write_u16_bytes(entry.access_date));
        }
        buf
    }

    /// Create a new directory entry in the given parent directory cluster chain.
    fn add_dirent_to_directory(
        &mut self,
        parent_cluster: u32,
        entry: &DirectoryEntry,
    ) -> Result<()> {
        let chain = self.read_chain(parent_cluster)?;
        let cluster_size = self.superblock.cluster_size() as usize;
        let entries_per_cluster = cluster_size / DIRENT_SIZE;

        // Search for a free slot (deleted or end-of-directory)
        for &cluster in &chain {
            let base_offset = self.cluster_offset(cluster)?;
            for slot in 0..entries_per_cluster {
                let slot_offset = base_offset + (slot * DIRENT_SIZE) as u64;
                // Read just the first byte (marker) via a full dirent read
                let mut marker_buf = [0u8; 1];
                self.read_at(slot_offset, &mut marker_buf)?;
                let marker = marker_buf[0];

                if marker == DIRENT_END || marker == DIRENT_DELETED || marker == 0x00 {
                    // Found a free slot — write the entry
                    let raw = self.serialize_dirent(entry);
                    self.write_at(slot_offset, &raw)?;

                    // If we overwrote an end marker and there's space after, write a new end marker
                    if marker == DIRENT_END || marker == 0x00 {
                        let next_slot = slot + 1;
                        if next_slot < entries_per_cluster {
                            let next_offset = base_offset + (next_slot * DIRENT_SIZE) as u64;
                            self.write_at(next_offset, &[DIRENT_END])?;
                        }
                    }

                    return Ok(());
                }
            }
        }

        // No free slot in existing clusters — allocate a new cluster for the directory
        let new_cluster = self.allocate_cluster()?;
        let last_cluster = *chain.last().unwrap();
        let result = (|| -> Result<()> {
            // Extend the chain: update the last cluster to point to the new one
            self.write_fat_entry(last_cluster, FatEntry::Next(new_cluster))?;

            // Initialize the new cluster with 0xFF (end markers)
            let blank = vec![0xFF; cluster_size];
            self.write_cluster(new_cluster, &blank)?;

            // Write the entry at the first slot of the new cluster
            let base_offset = self.cluster_offset(new_cluster)?;
            let raw = self.serialize_dirent(entry);
            self.write_at(base_offset, &raw)?;

            // Write end marker at slot 1
            if entries_per_cluster > 1 {
                self.write_at(base_offset + DIRENT_SIZE as u64, &[DIRENT_END])?;
            }

            Ok(())
        })();

        if let Err(err) = result {
            if let Err(cleanup_err) = self.write_fat_entry(last_cluster, FatEntry::EndOfChain) {
                warn!(
                    "add_dirent cleanup restore for parent {} failed after error {}: {}",
                    parent_cluster, err, cleanup_err
                );
            }
            if let Err(cleanup_err) = self.write_fat_entry(new_cluster, FatEntry::Free) {
                warn!(
                    "add_dirent cleanup free for new cluster {} failed after error {}: {}",
                    new_cluster, err, cleanup_err
                );
            }
            return Err(err);
        }

        Ok(())
    }

    /// Create a new file with the given data at the specified path.
    pub fn create_file(&mut self, path: &str, data: &[u8]) -> Result<()> {
        let (parent_path, filename) = split_path(path);
        Self::validate_filename(filename)?;

        // `create_file` is strict: callers that want overwrite semantics must
        // opt into them explicitly via a higher-level helper or policy.
        if self.resolve_path(path).is_ok() {
            return Err(FatxError::FileExists(path.to_string()));
        }

        // Resolve parent directory
        let parent = self.resolve_path(parent_path)?;
        if !parent.attributes.contains(FileAttributes::DIRECTORY) {
            return Err(FatxError::NotADirectory(parent_path.to_string()));
        }

        // Allocate clusters for the file data
        let cluster_size = self.superblock.cluster_size() as usize;
        let clusters_needed = if data.is_empty() {
            1
        } else {
            data.len().div_ceil(cluster_size)
        };

        let first_cluster = self.allocate_chain(clusters_needed)?;

        // If anything after allocation fails, release the new chain so we don't
        // strand clusters behind an unreachable directory entry.
        let result = (|| -> Result<()> {
            // Write the data
            let chain = self.read_chain(first_cluster)?;
            let mut offset = 0;
            for &cluster in &chain {
                let end = (offset + cluster_size).min(data.len());
                if offset < data.len() {
                    let mut cluster_buf = vec![0u8; cluster_size];
                    let len = end - offset;
                    cluster_buf[..len].copy_from_slice(&data[offset..end]);
                    self.write_cluster(cluster, &cluster_buf)?;
                }
                offset += cluster_size;
            }

            // Create directory entry — use UTC so Xbox displays correct local time
            let now = time::OffsetDateTime::now_utc();
            let date = DirectoryEntry::encode_date(now.year() as u16, now.month() as u8, now.day());
            let time = DirectoryEntry::encode_time(now.hour(), now.minute(), now.second());

            let mut filename_raw = [0xFFu8; MAX_FILENAME_LEN];
            let name_bytes = filename.as_bytes();
            filename_raw[..name_bytes.len()].copy_from_slice(name_bytes);

            let entry = DirectoryEntry {
                filename_len: name_bytes.len() as u8,
                attributes: FileAttributes::ARCHIVE,
                filename_raw,
                first_cluster,
                file_size: data.len() as u32,
                creation_time: time,
                creation_date: date,
                write_time: time,
                write_date: date,
                access_time: time,
                access_date: date,
            };

            self.add_dirent_to_directory(parent.first_cluster, &entry)?;
            Ok(())
        })();

        if let Err(err) = result {
            if let Err(cleanup_err) = self.free_chain(first_cluster) {
                warn!(
                    "create_file cleanup for '{}' failed after error {}: {}",
                    path, err, cleanup_err
                );
            }
            return Err(err);
        }

        info!(
            "Created file '{}' ({} bytes, {} clusters)",
            filename,
            data.len(),
            clusters_needed
        );
        Ok(())
    }

    /// Create a file if it doesn't exist, otherwise overwrite the existing file
    /// in place. Callers that want strict create-only semantics should keep
    /// using `create_file`.
    pub fn create_or_replace_file(&mut self, path: &str, data: &[u8]) -> Result<()> {
        match self.create_file(path, data) {
            Ok(()) => Ok(()),
            Err(FatxError::FileExists(_)) => {
                let existing = self.resolve_path(path)?;
                if existing.is_directory() {
                    return Err(FatxError::IsADirectory(path.to_string()));
                }
                self.write_file_in_place(path, data)
            }
            Err(err) => Err(err),
        }
    }

    /// Create a new directory at the specified path.
    pub fn create_directory(&mut self, path: &str) -> Result<()> {
        let (parent_path, dirname) = split_path(path);
        Self::validate_filename(dirname)?;

        // Check if it already exists
        if self.resolve_path(path).is_ok() {
            return Err(FatxError::FileExists(path.to_string()));
        }

        let parent = self.resolve_path(parent_path)?;
        if !parent.attributes.contains(FileAttributes::DIRECTORY) {
            return Err(FatxError::NotADirectory(parent_path.to_string()));
        }

        // Allocate one cluster for the new directory
        let cluster = self.allocate_cluster()?;
        let result = (|| -> Result<()> {
            // Initialize with end markers
            let cluster_size = self.superblock.cluster_size() as usize;
            let blank = vec![0xFFu8; cluster_size];
            self.write_cluster(cluster, &blank)?;

            // Use UTC so Xbox displays correct local time
            let now = time::OffsetDateTime::now_utc();
            let date = DirectoryEntry::encode_date(now.year() as u16, now.month() as u8, now.day());
            let time = DirectoryEntry::encode_time(now.hour(), now.minute(), now.second());

            let mut filename_raw = [0xFFu8; MAX_FILENAME_LEN];
            let name_bytes = dirname.as_bytes();
            filename_raw[..name_bytes.len()].copy_from_slice(name_bytes);

            let entry = DirectoryEntry {
                filename_len: name_bytes.len() as u8,
                attributes: FileAttributes::DIRECTORY,
                filename_raw,
                first_cluster: cluster,
                file_size: 0,
                creation_time: time,
                creation_date: date,
                write_time: time,
                write_date: date,
                access_time: time,
                access_date: date,
            };

            self.add_dirent_to_directory(parent.first_cluster, &entry)?;
            Ok(())
        })();

        if let Err(err) = result {
            if let Err(cleanup_err) = self.write_fat_entry(cluster, FatEntry::Free) {
                warn!(
                    "create_directory cleanup for '{}' failed after error {}: {}",
                    path, err, cleanup_err
                );
            }
            return Err(err);
        }

        info!("Created directory '{}'", dirname);
        Ok(())
    }

    /// Write data to an existing file IN-PLACE, reusing its cluster chain.
    ///
    /// This is dramatically faster than delete+recreate for large files because:
    /// - Existing clusters are overwritten directly (no FAT changes needed)
    /// - Only NEWLY needed clusters trigger FAT allocations
    /// - If the file shrank, excess clusters are freed
    /// - The directory entry's file_size is updated on disk
    ///
    /// This method follows the same pattern as Linux's FAT32 driver: write data
    /// directly to existing cluster offsets, extend the chain if the file grew,
    /// and free tail clusters if it shrank. The FAT is only modified when the
    /// cluster count actually changes.
    pub fn write_file_in_place(&mut self, path: &str, data: &[u8]) -> Result<()> {
        let (parent_path, filename) = split_path(path);
        let parent = self.resolve_path(parent_path)?;
        let target = self.resolve_path(path)?;

        if target.is_directory() {
            return Err(FatxError::IsADirectory(path.to_string()));
        }

        let cluster_size = self.superblock.cluster_size() as usize;
        let clusters_needed = if data.is_empty() {
            1
        } else {
            data.len().div_ceil(cluster_size)
        };

        // Read the existing cluster chain
        let old_chain = self.read_chain(target.first_cluster)?;
        let old_count = old_chain.len();
        let mut chain = old_chain.clone();

        // ── Phase 1: Extend chain if file grew ──
        if clusters_needed > old_count {
            let extra = clusters_needed - old_count;
            // Find the last cluster in the existing chain
            let last_old = *old_chain.last().unwrap();

            // Allocate additional clusters using bitmap scan from prev_free
            let mut new_clusters = Vec::with_capacity(extra);
            let end = FIRST_CLUSTER + self.total_clusters;
            let start_from = if self.prev_free + 1 >= end {
                FIRST_CLUSTER
            } else {
                self.prev_free + 1
            };
            let mut cursor = start_from;

            // Pass 1: from prev_free+1 to end
            while new_clusters.len() < extra {
                match self.bitmap_find_free(cursor, end) {
                    Some(cluster) => {
                        new_clusters.push(cluster);
                        cursor = cluster + 1;
                    }
                    None => break,
                }
            }
            // Pass 2: wraparound
            if new_clusters.len() < extra && start_from > FIRST_CLUSTER {
                cursor = FIRST_CLUSTER;
                while new_clusters.len() < extra {
                    match self.bitmap_find_free(cursor, start_from) {
                        Some(cluster) => {
                            new_clusters.push(cluster);
                            cursor = cluster + 1;
                        }
                        None => break,
                    }
                }
            }
            if new_clusters.len() < extra {
                return Err(FatxError::DiskFull);
            }
            // Update prev_free
            if let Some(&last) = new_clusters.last() {
                self.prev_free = last;
            }

            // Link: old_last -> new_clusters[0] -> ... -> EOC
            self.write_fat_entry(last_old, FatEntry::Next(new_clusters[0]))?;
            for i in 0..new_clusters.len() - 1 {
                self.write_fat_entry(new_clusters[i], FatEntry::Next(new_clusters[i + 1]))?;
            }
            self.write_fat_entry(*new_clusters.last().unwrap(), FatEntry::EndOfChain)?;

            // Re-read chain after the extension is linked into the FAT cache.
            chain = self.read_chain(target.first_cluster)?;
        }

        // ── Phase 2: Write data to clusters ──
        let mut offset = 0;
        for &cluster in chain.iter().take(clusters_needed) {
            let end = (offset + cluster_size).min(data.len());
            let mut cluster_buf = vec![0u8; cluster_size];
            if offset < data.len() {
                let len = end - offset;
                cluster_buf[..len].copy_from_slice(&data[offset..end]);
            }
            self.write_cluster(cluster, &cluster_buf)?;
            offset += cluster_size;
        }

        // ── Phase 3: Publish the new logical file size and timestamps ──
        //
        // This happens only after all payload writes completed, so callers never
        // observe a dirent advertising bytes that haven't been written yet.
        let now = time::OffsetDateTime::now_utc();
        self.update_dirent_metadata(parent.first_cluster, filename, data.len() as u32, Some(now))?;

        // ── Phase 4: Free excess clusters if file shrank ──
        if clusters_needed < old_count {
            // Mark the new last cluster as EOC
            self.write_fat_entry(chain[clusters_needed - 1], FatEntry::EndOfChain)?;
            // Free the tail
            for &cluster in chain.iter().take(old_count).skip(clusters_needed) {
                self.write_fat_entry(cluster, FatEntry::Free)?;
            }
        }

        info!(
            "Wrote '{}' in-place ({} bytes, {} clusters, was {})",
            filename,
            data.len(),
            clusters_needed,
            old_count
        );
        Ok(())
    }

    fn plan_write_in_place_for_entry(
        &mut self,
        target: &DirectoryEntry,
        new_size: usize,
    ) -> Result<(usize, Vec<u32>)> {
        if target.is_directory() {
            return Err(FatxError::IsADirectory(target.filename()));
        }

        let cluster_size = self.superblock.cluster_size() as usize;
        let clusters_needed = if new_size == 0 {
            1
        } else {
            new_size.div_ceil(cluster_size)
        };

        let old_chain = self.read_chain(target.first_cluster)?;
        let old_count = old_chain.len();

        if clusters_needed > old_count {
            let extra = clusters_needed - old_count;
            let last_old = *old_chain.last().unwrap();

            let end = FIRST_CLUSTER + self.total_clusters;
            let start_from = if self.prev_free + 1 >= end {
                FIRST_CLUSTER
            } else {
                self.prev_free + 1
            };
            let mut new_clusters = Vec::with_capacity(extra);
            let mut cursor = start_from;

            while new_clusters.len() < extra {
                match self.bitmap_find_free(cursor, end) {
                    Some(c) => {
                        new_clusters.push(c);
                        cursor = c + 1;
                    }
                    None => break,
                }
            }
            if new_clusters.len() < extra && start_from > FIRST_CLUSTER {
                cursor = FIRST_CLUSTER;
                while new_clusters.len() < extra {
                    match self.bitmap_find_free(cursor, start_from) {
                        Some(c) => {
                            new_clusters.push(c);
                            cursor = c + 1;
                        }
                        None => break,
                    }
                }
            }
            if new_clusters.len() < extra {
                return Err(FatxError::DiskFull);
            }
            if let Some(&last) = new_clusters.last() {
                self.prev_free = last;
            }

            self.write_fat_entry(last_old, FatEntry::Next(new_clusters[0]))?;
            for i in 0..new_clusters.len() - 1 {
                self.write_fat_entry(new_clusters[i], FatEntry::Next(new_clusters[i + 1]))?;
            }
            self.write_fat_entry(*new_clusters.last().unwrap(), FatEntry::EndOfChain)?;
        }

        let planned_chain = if clusters_needed > old_count {
            self.read_chain(target.first_cluster)?
        } else {
            old_chain
        };

        Ok((
            old_count,
            planned_chain.into_iter().take(clusters_needed).collect(),
        ))
    }

    fn find_entry_in_parent_by_cluster(
        &mut self,
        parent_cluster: u32,
        first_cluster: u32,
    ) -> Result<DirectoryEntry> {
        let entries = if parent_cluster == FIRST_CLUSTER {
            self.read_root_directory()?
        } else {
            self.read_directory(parent_cluster)?
        };
        entries
            .into_iter()
            .find(|entry| entry.first_cluster == first_cluster)
            .ok_or_else(|| FatxError::FileNotFound(format!("cluster {}", first_cluster)))
    }

    #[doc(hidden)]
    pub fn begin_write_in_place_for_entry(
        &mut self,
        parent_cluster: u32,
        first_cluster: u32,
        new_size: usize,
    ) -> Result<WriteSession> {
        let target = self.find_entry_in_parent_by_cluster(parent_cluster, first_cluster)?;
        let (old_count, chain) = self.plan_write_in_place_for_entry(&target, new_size)?;
        Ok(WriteSession {
            parent_cluster,
            first_cluster,
            old_count,
            chain,
            new_size,
            finalized: false,
        })
    }

    #[doc(hidden)]
    pub fn commit_write_session(&mut self, mut session: WriteSession) -> Result<()> {
        let target =
            self.find_entry_in_parent_by_cluster(session.parent_cluster, session.first_cluster)?;
        let filename = target.filename();

        let now = time::OffsetDateTime::now_utc();
        self.update_dirent_metadata(
            session.parent_cluster,
            &filename,
            session.new_size as u32,
            Some(now),
        )?;

        if session.chain.len() < session.old_count {
            let full_chain = self.read_chain(session.first_cluster)?;
            self.write_fat_entry(full_chain[session.chain.len() - 1], FatEntry::EndOfChain)?;
            for &cluster in full_chain.iter().skip(session.chain.len()) {
                self.write_fat_entry(cluster, FatEntry::Free)?;
            }
        }

        session.finalized = true;
        Ok(())
    }

    #[doc(hidden)]
    pub fn cancel_write_session(&mut self, mut session: WriteSession) -> Result<()> {
        if session.chain.len() > session.old_count {
            let old_last = session.chain[session.old_count - 1];
            self.write_fat_entry(old_last, FatEntry::EndOfChain)?;
            for &cluster in session.chain.iter().skip(session.old_count) {
                self.write_fat_entry(cluster, FatEntry::Free)?;
            }
        }

        session.finalized = true;
        Ok(())
    }

    /// Update selected mutable metadata for a directory entry on disk.
    /// `creation_*` is preserved for overwrites; write/access timestamps are
    /// refreshed when `touch` is provided.
    fn update_dirent_metadata(
        &mut self,
        parent_cluster: u32,
        name: &str,
        new_size: u32,
        touch: Option<time::OffsetDateTime>,
    ) -> Result<()> {
        let chain = self.read_chain(parent_cluster)?;
        let cluster_size = self.superblock.cluster_size() as usize;
        let entries_per_cluster = cluster_size / DIRENT_SIZE;

        for &cluster in &chain {
            let base_offset = self.cluster_offset(cluster)?;
            for slot in 0..entries_per_cluster {
                let slot_offset = base_offset + (slot * DIRENT_SIZE) as u64;
                let mut entry = self.read_dirent_at(slot_offset)?;

                if entry.is_end() {
                    return Err(FatxError::FileNotFound(name.to_string()));
                }
                if !entry.is_deleted() && entry.filename().eq_ignore_ascii_case(name) {
                    entry.file_size = new_size;
                    if let Some(now) = touch {
                        let date = DirectoryEntry::encode_date(
                            now.year() as u16,
                            now.month() as u8,
                            now.day(),
                        );
                        let time =
                            DirectoryEntry::encode_time(now.hour(), now.minute(), now.second());
                        entry.write_date = date;
                        entry.write_time = time;
                        entry.access_date = date;
                        entry.access_time = time;
                    }
                    let raw = self.serialize_dirent(&entry);
                    self.write_at(slot_offset, &raw)?;
                    return Ok(());
                }
            }
        }

        Err(FatxError::FileNotFound(name.to_string()))
    }

    /// Delete a file or empty directory at the specified path.
    pub fn delete(&mut self, path: &str) -> Result<()> {
        let (parent_path, target_name) = split_path(path);

        let parent = self.resolve_path(parent_path)?;
        let target = self.resolve_path(path)?;

        // If target is a directory, ensure it's empty
        if target.is_directory() {
            let contents = self.read_directory(target.first_cluster)?;
            if !contents.is_empty() {
                return Err(FatxError::DirectoryNotEmpty(path.to_string()));
            }
        }

        // Free the cluster chain (tolerant — continues even if chain is corrupt)
        if let Err(e) = self.free_chain(target.first_cluster) {
            warn!("delete '{}': failed to free chain: {}", path, e);
        }

        // Mark the directory entry as deleted
        self.mark_dirent_deleted(parent.first_cluster, target_name)?;

        info!("Deleted '{}'", path);
        Ok(())
    }

    /// Recursively delete a file or directory and all its contents.
    /// Tolerates corrupt chains from interrupted writes.
    pub fn delete_recursive(&mut self, path: &str) -> Result<()> {
        let (parent_path, target_name) = split_path(path);
        let parent = self.resolve_path(parent_path)?;
        let target = self.resolve_path(path)?;

        if target.is_directory() {
            // Tolerate corrupt directory reads — delete as many children as possible
            match self.read_directory(target.first_cluster) {
                Ok(contents) => {
                    for entry in &contents {
                        let child_path = if path == "/" {
                            format!("/{}", entry.filename())
                        } else {
                            format!("{}/{}", path, entry.filename())
                        };
                        if let Err(e) = self.delete_recursive(&child_path) {
                            warn!("delete_recursive: skipping '{}': {}", child_path, e);
                        }
                    }
                }
                Err(e) => {
                    warn!(
                        "delete_recursive: cannot read directory '{}': {}, will still delete entry",
                        path, e
                    );
                }
            }
        }

        // Free the cluster chain (tolerant — continues even if chain is corrupt)
        if let Err(e) = self.free_chain(target.first_cluster) {
            warn!("delete_recursive '{}': failed to free chain: {}", path, e);
        }

        // Force-delete the directory entry even if directory wasn't fully emptied
        // (we've done our best to clean up children above)
        self.mark_dirent_deleted(parent.first_cluster, target_name)?;

        info!("Deleted '{}' (recursive)", path);
        Ok(())
    }

    /// Scan the entire volume for macOS metadata files without deleting anything.
    /// Returns a list of matching entries.
    pub fn scan_macos_metadata(&mut self) -> Result<Vec<MacosMetadataEntry>> {
        self.scan_macos_metadata_from("/")
    }

    /// Scan from a specific directory path for macOS metadata files.
    pub fn scan_macos_metadata_from(&mut self, path: &str) -> Result<Vec<MacosMetadataEntry>> {
        let entry = self.resolve_path(path)?;
        if !entry.is_directory() {
            return Err(FatxError::NotADirectory(path.to_string()));
        }
        let mut found = Vec::new();
        self.scan_macos_metadata_inner(path, entry.first_cluster, &mut found)?;
        Ok(found)
    }

    fn scan_macos_metadata_inner(
        &mut self,
        dir_path: &str,
        dir_cluster: u32,
        found: &mut Vec<MacosMetadataEntry>,
    ) -> Result<()> {
        let entries = self.read_directory(dir_cluster)?;

        // Collect to avoid borrow conflict with recursive &mut self calls
        let children: Vec<(String, bool, u32, u32)> = entries
            .iter()
            .map(|e| (e.filename(), e.is_directory(), e.first_cluster, e.file_size))
            .collect();

        for (name, is_dir, first_cluster, file_size) in children {
            let child_path = if dir_path == "/" {
                format!("/{}", name)
            } else {
                format!("{}/{}", dir_path, name)
            };

            if crate::types::is_macos_metadata(&name) {
                found.push(MacosMetadataEntry {
                    path: child_path,
                    is_dir,
                    size: file_size as u64,
                });
                // Don't recurse into metadata dirs — they'll be deleted whole
            } else if is_dir {
                self.scan_macos_metadata_inner(&child_path, first_cluster, found)?;
            }
        }

        Ok(())
    }

    /// Delete all entries from a previous `scan_macos_metadata` result.
    /// Returns (files_deleted, dirs_deleted, bytes_freed).
    pub fn delete_macos_metadata(
        &mut self,
        entries: &[MacosMetadataEntry],
        progress: Option<&dyn Fn(&str)>,
    ) -> Result<(usize, usize, u64)> {
        let mut files_deleted = 0usize;
        let mut dirs_deleted = 0usize;
        let mut bytes_freed = 0u64;

        for entry in entries {
            if let Some(cb) = &progress {
                cb(&entry.path);
            }
            if entry.is_dir {
                if let Err(e) = self.delete_recursive(&entry.path) {
                    warn!("cleanup: failed to delete dir '{}': {}", entry.path, e);
                    continue;
                }
                dirs_deleted += 1;
            } else {
                if let Err(e) = self.delete(&entry.path) {
                    warn!("cleanup: failed to delete '{}': {}", entry.path, e);
                    continue;
                }
                files_deleted += 1;
                bytes_freed += entry.size;
            }
        }

        Ok((files_deleted, dirs_deleted, bytes_freed))
    }

    /// Recursively copy a local directory tree into the FATX volume.
    /// Opens volume once and writes all files/dirs in a single session.
    pub fn copy_from_host(
        &mut self,
        local_path: &std::path::Path,
        dest_path: &str,
        progress: Option<ProgressFn<'_>>,
    ) -> Result<(usize, usize, u64)> {
        self.copy_from_host_with_control(local_path, dest_path, progress, None, 0, 0)
    }

    pub fn copy_from_host_with_control(
        &mut self,
        local_path: &std::path::Path,
        dest_path: &str,
        progress: Option<ProgressFn<'_>>,
        should_abort: Option<&dyn Fn() -> bool>,
        flush_every_files: usize,
        flush_every_bytes: u64,
    ) -> Result<(usize, usize, u64)> {
        // A trailing slash means "--to is the parent"; without it, the caller
        // is naming the target directory itself and we preserve the old behavior.
        let effective_dest = if dest_path.ends_with('/') {
            let dir_name = local_path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| {
                    FatxError::Io(std::io::Error::other(format!(
                        "Cannot derive source directory name from '{}'",
                        local_path.display()
                    )))
                })?;
            let trimmed = dest_path.trim_end_matches('/');
            if trimmed.is_empty() {
                format!("/{}", dir_name)
            } else {
                format!("{}/{}", trimmed, dir_name)
            }
        } else {
            dest_path.to_string()
        };
        let mut state = CopyFromHostState {
            progress,
            should_abort,
            flush_every_files,
            flush_every_bytes,
            files_since_flush: 0,
            bytes_since_flush: 0,
        };
        self.copy_from_host_inner(local_path, &effective_dest, &mut state, 0)
    }

    #[allow(clippy::type_complexity)]
    fn copy_from_host_inner(
        &mut self,
        local_path: &std::path::Path,
        dest_path: &str,
        state: &mut CopyFromHostState<'_>,
        base_bytes: u64,
    ) -> Result<(usize, usize, u64)> {
        use std::fs;

        let dest_path = if dest_path == "/" {
            dest_path
        } else {
            dest_path.trim_end_matches('/')
        };

        self.abort_copy_if_requested(state)?;

        let mut file_count = 0usize;
        let mut dir_count = 0usize;
        let mut total_bytes = 0u64;

        // Create destination directory
        match self.create_directory(dest_path) {
            Ok(_) => {}
            Err(FatxError::FileExists(_)) => {
                // `create_directory` reports FileExists for both files and
                // directories, so verify the existing target is usable.
                let existing = self.resolve_path(dest_path)?;
                if !existing.is_directory() {
                    return Err(FatxError::NotADirectory(dest_path.to_string()));
                }
            }
            Err(e) => return Err(e),
        }
        dir_count += 1;

        // Read local directory entries
        let entries = fs::read_dir(local_path).map_err(|e| {
            FatxError::Io(std::io::Error::other(format!(
                "Cannot read local dir '{}': {}",
                local_path.display(),
                e
            )))
        })?;

        for entry in entries {
            self.abort_copy_if_requested(state)?;

            let entry = entry.map_err(|e| FatxError::Io(std::io::Error::other(e.to_string())))?;
            let local_child = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();

            // Skip macOS metadata files — meaningless on Xbox, wastes clusters
            if crate::types::is_macos_metadata(&name) {
                info!("Skipping macOS metadata: {}", name);
                continue;
            }

            let fatx_child = if dest_path == "/" {
                format!("/{}", name)
            } else {
                format!("{}/{}", dest_path, name)
            };

            if local_child.is_dir() {
                let (fc, dc, tb) = self.copy_from_host_inner(
                    &local_child,
                    &fatx_child,
                    state,
                    base_bytes + total_bytes,
                )?;
                file_count += fc;
                dir_count += dc;
                total_bytes += tb;
            } else if local_child.is_file() {
                let data = fs::read(&local_child).map_err(|e| {
                    FatxError::Io(std::io::Error::other(format!(
                        "Cannot read '{}': {}",
                        local_child.display(),
                        e
                    )))
                })?;
                let file_size = data.len() as u64;

                if let Some(cb) = &state.progress {
                    cb(&fatx_child, file_size, base_bytes + total_bytes);
                }

                self.create_file(&fatx_child, &data)?;
                file_count += 1;
                total_bytes += file_size;
                state.files_since_flush += 1;
                state.bytes_since_flush += file_size;
                self.flush_copy_if_needed(state)?;
            }
        }

        Ok((file_count, dir_count, total_bytes))
    }

    fn abort_copy_if_requested(&mut self, state: &CopyFromHostState<'_>) -> Result<()> {
        if state
            .should_abort
            .is_some_and(|should_abort| should_abort())
        {
            self.flush()?;
            return Err(FatxError::Io(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "Copy interrupted",
            )));
        }
        Ok(())
    }

    fn flush_copy_if_needed(&mut self, state: &mut CopyFromHostState<'_>) -> Result<()> {
        let flush_due_to_files =
            state.flush_every_files > 0 && state.files_since_flush >= state.flush_every_files;
        let flush_due_to_bytes =
            state.flush_every_bytes > 0 && state.bytes_since_flush >= state.flush_every_bytes;

        if flush_due_to_files || flush_due_to_bytes {
            self.flush()?;
            state.files_since_flush = 0;
            state.bytes_since_flush = 0;
        }
        Ok(())
    }

    /// Find and mark a directory entry as deleted (set filename_len to 0xE5).
    fn mark_dirent_deleted(&mut self, parent_cluster: u32, name: &str) -> Result<()> {
        let chain = self.read_chain(parent_cluster)?;
        let cluster_size = self.superblock.cluster_size() as usize;
        let entries_per_cluster = cluster_size / DIRENT_SIZE;

        for &cluster in &chain {
            let base_offset = self.cluster_offset(cluster)?;
            for slot in 0..entries_per_cluster {
                let slot_offset = base_offset + (slot * DIRENT_SIZE) as u64;
                let entry = self.read_dirent_at(slot_offset)?;

                if entry.is_end() {
                    return Err(FatxError::FileNotFound(name.to_string()));
                }
                if !entry.is_deleted() && entry.filename().eq_ignore_ascii_case(name) {
                    // Mark as deleted by writing 0xE5 to the first byte
                    self.write_at(slot_offset, &[DIRENT_DELETED])?;
                    return Ok(());
                }
            }
        }

        Err(FatxError::FileNotFound(name.to_string()))
    }

    /// Rename a file or directory.
    pub fn rename(&mut self, old_path: &str, new_name: &str) -> Result<()> {
        Self::validate_filename(new_name)?;

        let (parent_path, old_name) = split_path(old_path);
        let parent = self.resolve_path(parent_path)?;
        let source = self.resolve_path(old_path)?;

        let chain = self.read_chain(parent.first_cluster)?;
        let cluster_size = self.superblock.cluster_size() as usize;
        let entries_per_cluster = cluster_size / DIRENT_SIZE;

        for &cluster in &chain {
            let base_offset = self.cluster_offset(cluster)?;
            for slot in 0..entries_per_cluster {
                let slot_offset = base_offset + (slot * DIRENT_SIZE) as u64;
                let entry = self.read_dirent_at(slot_offset)?;

                if entry.is_end() {
                    break;
                }
                if !entry.is_deleted()
                    && entry.first_cluster != source.first_cluster
                    && entry.filename().eq_ignore_ascii_case(new_name)
                {
                    let dest_path = if parent_path == "/" {
                        format!("/{}", new_name)
                    } else {
                        format!("{}/{}", parent_path, new_name)
                    };
                    return Err(FatxError::FileExists(dest_path));
                }
            }
        }

        for &cluster in &chain {
            let base_offset = self.cluster_offset(cluster)?;
            for slot in 0..entries_per_cluster {
                let slot_offset = base_offset + (slot * DIRENT_SIZE) as u64;
                let entry = self.read_dirent_at(slot_offset)?;

                if entry.is_end() {
                    return Err(FatxError::FileNotFound(old_name.to_string()));
                }
                if !entry.is_deleted() && entry.filename().eq_ignore_ascii_case(old_name) {
                    // Read the full 64-byte entry, update filename fields, write back
                    let mut raw = [0u8; DIRENT_SIZE];
                    self.read_at(slot_offset, &mut raw)?;

                    let name_bytes = new_name.as_bytes();
                    raw[0] = name_bytes.len() as u8;
                    // Clear filename area and write new name
                    raw[2..2 + MAX_FILENAME_LEN].fill(0xFF);
                    raw[2..2 + name_bytes.len()].copy_from_slice(name_bytes);

                    self.write_at(slot_offset, &raw)?;

                    info!("Renamed '{}' -> '{}'", old_name, new_name);
                    return Ok(());
                }
            }
        }

        Err(FatxError::FileNotFound(old_name.to_string()))
    }

    /// Flush any buffered writes to the underlying device.
    pub fn flush(&mut self) -> Result<()> {
        if self.fat_dirty && !self.dirty_ranges.is_empty() {
            // Merge overlapping/adjacent dirty ranges and align to I/O boundaries.
            let merged = self.merge_dirty_ranges();
            let fat_abs = self.partition_offset + self.fat_offset;

            let mut flushed_bytes = 0usize;
            let mut failed = false;

            for (start, end) in &merged {
                let range_abs = fat_abs + *start as u64;
                let aligned_start = self.align_down(range_abs);
                let pre_skip = (range_abs - aligned_start) as usize;
                let range_len = end - start;
                let total = pre_skip + range_len;
                let aligned_len = self.align_up(total);

                // Read-modify-write for this aligned region
                self.inner.seek(SeekFrom::Start(aligned_start))?;
                let mut buf = vec![0u8; aligned_len];
                if let Err(e) = self.inner.read_exact(&mut buf) {
                    warn!(
                        "Dirty-range flush read failed at offset 0x{:X}: {}",
                        aligned_start, e
                    );
                    failed = true;
                    break;
                }

                buf[pre_skip..pre_skip + range_len].copy_from_slice(&self.fat_cache[*start..*end]);

                self.inner.seek(SeekFrom::Start(aligned_start))?;
                if let Err(e) = self.inner.write_all(&buf) {
                    warn!(
                        "Dirty-range flush write failed at offset 0x{:X}: {}",
                        aligned_start, e
                    );
                    failed = true;
                    break;
                }

                flushed_bytes += aligned_len;
            }

            if failed {
                // Remove successfully flushed ranges, keep remaining for retry.
                // (On failure, we break out of the loop — remaining ranges stay.)
                warn!("Partial FAT flush — some ranges may need retry");
            } else {
                self.dirty_ranges.clear();
                self.fat_dirty = false;
            }

            info!(
                "Flushed FAT: {} dirty ranges, {} bytes written (of {} total FAT)",
                merged.len(),
                flushed_bytes,
                self.fat_cache.len()
            );
        }
        self.inner.flush()?;
        Ok(())
    }

    /// Get volume statistics. O(1) — uses cached counts maintained incrementally.
    pub fn stats(&self) -> Result<VolumeStats> {
        let free_clusters = self.free_cluster_count;
        let bad_clusters = self.bad_cluster_count;
        let used_clusters = self.total_clusters - free_clusters - bad_clusters;
        let cluster_size = self.superblock.cluster_size();

        Ok(VolumeStats {
            total_clusters: self.total_clusters,
            free_clusters,
            used_clusters,
            bad_clusters,
            cluster_size,
            total_size: self.partition_size,
            free_size: free_clusters as u64 * cluster_size,
            used_size: used_clusters as u64 * cluster_size,
        })
    }
}

impl FatxVolume<File> {
    // Use positional reads so shared readers don't need to mutate the file
    // cursor. This is the basis for serving NFS reads under `vol.read()`.
    fn read_exact_abs(&self, abs_offset: u64, buf: &mut [u8]) -> Result<()> {
        use std::os::unix::fs::FileExt;

        let mut read = 0usize;
        while read < buf.len() {
            let n = self
                .inner
                .read_at(&mut buf[read..], abs_offset + read as u64)?;
            if n == 0 {
                return Err(FatxError::Io(std::io::Error::from(
                    std::io::ErrorKind::UnexpectedEof,
                )));
            }
            read += n;
        }
        Ok(())
    }

    fn read_at_shared(&self, partition_rel: u64, buf: &mut [u8]) -> Result<()> {
        let abs = self.abs_offset(partition_rel);
        let aligned_start = self.align_down(abs);
        let pre_skip = (abs - aligned_start) as usize;
        let total_needed = pre_skip + buf.len();
        let aligned_len = self.align_up(total_needed);

        let mut aligned_buf = vec![0u8; aligned_len];
        self.read_exact_abs(aligned_start, &mut aligned_buf)?;
        buf.copy_from_slice(&aligned_buf[pre_skip..pre_skip + buf.len()]);
        Ok(())
    }

    fn read_cluster_shared(&self, cluster: u32, buf: &mut [u8]) -> Result<()> {
        let offset = self.cluster_offset(cluster)?;
        self.read_at_shared(offset, buf)?;
        Ok(())
    }

    fn read_dirent_at_shared(&self, partition_rel: u64) -> Result<DirectoryEntry> {
        let mut buf = [0u8; DIRENT_SIZE];
        self.read_at_shared(partition_rel, &mut buf)?;
        Ok(self.parse_dirent_buf(&buf))
    }

    pub fn read_directory_shared(&self, first_cluster: u32) -> Result<Vec<DirectoryEntry>> {
        let chain = self.read_chain(first_cluster)?;
        let cluster_size = self.superblock.cluster_size() as usize;
        let entries_per_cluster = cluster_size / DIRENT_SIZE;
        let mut entries = Vec::new();

        for &cluster in &chain {
            let base_offset = self.cluster_offset(cluster)?;

            for slot in 0..entries_per_cluster {
                let slot_offset = base_offset + (slot * DIRENT_SIZE) as u64;
                let entry = self.read_dirent_at_shared(slot_offset)?;
                if entry.is_end() {
                    return Ok(entries);
                }
                if !entry.is_deleted() {
                    entries.push(entry);
                }
            }
        }

        Ok(entries)
    }

    pub fn read_root_directory_shared(&self) -> Result<Vec<DirectoryEntry>> {
        self.read_directory_shared(FIRST_CLUSTER)
    }

    pub fn read_file_range_shared(
        &self,
        entry: &DirectoryEntry,
        offset: u64,
        count: usize,
    ) -> Result<Vec<u8>> {
        if entry.is_directory() {
            return Err(FatxError::IsADirectory(entry.filename()));
        }

        let file_size = entry.file_size as usize;
        let start = offset as usize;
        if start >= file_size || count == 0 {
            return Ok(Vec::new());
        }

        let chain = self.read_chain(entry.first_cluster)?;
        let cluster_size = self.superblock.cluster_size() as usize;
        // Walk only the clusters covering the requested byte range. This avoids
        // materializing the whole file just to satisfy one NFS read chunk.
        let mut data = Vec::with_capacity(count.min(file_size - start));
        let mut remaining = count.min(file_size - start);
        let start_cluster_idx = start / cluster_size;
        let mut offset_in_cluster = start % cluster_size;

        for &cluster in chain.iter().skip(start_cluster_idx) {
            let to_read = remaining.min(cluster_size - offset_in_cluster);
            let mut buf = vec![0u8; cluster_size];
            self.read_cluster_shared(cluster, &mut buf)?;
            data.extend_from_slice(&buf[offset_in_cluster..offset_in_cluster + to_read]);
            remaining -= to_read;
            offset_in_cluster = 0;
            if remaining == 0 {
                break;
            }
        }

        Ok(data)
    }
}

/// A macOS metadata entry found by `scan_macos_metadata`.
#[derive(Debug)]
pub struct MacosMetadataEntry {
    /// FATX path (e.g., "/Content/.DS_Store")
    pub path: String,
    /// Whether this is a directory (.Spotlight-V100, .Trashes, .fseventsd)
    pub is_dir: bool,
    /// File size in bytes (0 for directories)
    pub size: u64,
}

/// Volume usage statistics.
#[derive(Debug)]
pub struct VolumeStats {
    pub total_clusters: u32,
    pub free_clusters: u32,
    pub used_clusters: u32,
    pub bad_clusters: u32,
    pub cluster_size: u64,
    pub total_size: u64,
    pub free_size: u64,
    pub used_size: u64,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Split a path into (parent, basename).
/// "/saves/game1.sav" -> ("/saves", "game1.sav")
/// "/readme.txt" -> ("/", "readme.txt")
fn split_path(path: &str) -> (&str, &str) {
    let path = path.trim_end_matches('/');
    match path.rfind('/') {
        Some(0) => ("/", &path[1..]),
        Some(pos) => (&path[..pos], &path[pos + 1..]),
        None => ("/", path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_path() {
        assert_eq!(split_path("/foo/bar.txt"), ("/foo", "bar.txt"));
        assert_eq!(split_path("/bar.txt"), ("/", "bar.txt"));
        assert_eq!(split_path("bar.txt"), ("/", "bar.txt"));
        assert_eq!(split_path("/a/b/c"), ("/a/b", "c"));
    }
}
