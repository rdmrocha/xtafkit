//! Mount Xbox FATX/XTAF file systems via a local NFS server.
//!
//! Starts a localhost NFSv3 server backed by a FATX volume, then mounts it
//! so it appears as a regular volume in Finder.

use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use parking_lot::{Mutex, RwLock};
use quick_cache::sync::Cache;

use async_trait::async_trait;
// clap::Args derived on MountArgs
use log::{debug, error, info, warn};
use nfsserve::nfs::{
    fattr3, fileid3, filename3, ftype3, nfspath3, nfsstat3, nfstime3, sattr3, specdata3,
};
use nfsserve::tcp::{NFSTcp, NFSTcpListener};
use nfsserve::vfs::{DirEntry, NFSFileSystem, ReadDirResult, VFSCapabilities};

use fatxlib::partition::{detect_xbox_partitions, format_size};
use fatxlib::types::{DirectoryEntry, FileAttributes, FIRST_CLUSTER};
use fatxlib::volume::FatxVolume;

mod disk_watcher;

const ROOT_FILEID: fileid3 = 1; // FATX root cluster is FIRST_CLUSTER (1)

/// Convert a FATX cluster number to an NFS file ID.
fn cluster_to_id(cluster: u32) -> fileid3 {
    cluster as fileid3
}

fn id_to_cluster(id: fileid3) -> u32 {
    id as u32
}

// Slice from the buffered in-memory file image used by delayed NFS writes.
// Reads consult this first so they can observe unflushed writes.
fn slice_buffered_range(buffered: &[u8], offset: u64, count: u32) -> (Vec<u8>, bool) {
    let start = offset as usize;
    if start >= buffered.len() {
        return (vec![], true);
    }

    let end = (start + count as usize).min(buffered.len());
    let eof = end >= buffered.len();
    (buffered[start..end].to_vec(), eof)
}

/// Convert FATX packed date+time to nfstime3.
fn fatx_to_nfstime(date: u16, time: u16) -> nfstime3 {
    let year = ((date >> 9) & 0x7F) as i32 + 1980;
    let month = ((date >> 5) & 0x0F) as u32;
    let day = (date & 0x1F) as u32;
    let hour = ((time >> 11) & 0x1F) as u32;
    let min = ((time >> 5) & 0x3F) as u32;
    let sec = ((time & 0x1F) * 2) as u32;

    let days_approx = (year - 1970) as u64 * 365
        + ((year - 1969) / 4) as u64
        + month_day_offset(month, year) as u64
        + (day.saturating_sub(1)) as u64;
    let secs = days_approx * 86400 + hour as u64 * 3600 + min as u64 * 60 + sec as u64;

    nfstime3 {
        seconds: secs as u32,
        nseconds: 0,
    }
}

fn month_day_offset(month: u32, year: i32) -> u32 {
    let days = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
    let base = if (1..=12).contains(&month) {
        days[(month - 1) as usize]
    } else {
        0
    };
    if month > 2 && (year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)) {
        base + 1
    } else {
        base
    }
}

fn now_nfstime() -> nfstime3 {
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    nfstime3 {
        seconds: d.as_secs() as u32,
        nseconds: d.subsec_nanos(),
    }
}

/// Build NFS file attributes from a FATX directory entry.
fn dirent_to_fattr(entry: &DirectoryEntry) -> fattr3 {
    let is_dir = entry.attributes.contains(FileAttributes::DIRECTORY);
    let ftype = if is_dir {
        ftype3::NF3DIR
    } else {
        ftype3::NF3REG
    };
    let mode: u32 = if is_dir { 0o755 } else { 0o644 };
    let size = entry.file_size as u64;

    let ctime = fatx_to_nfstime(entry.creation_date, entry.creation_time);
    let mtime = fatx_to_nfstime(entry.write_date, entry.write_time);
    let atime = fatx_to_nfstime(entry.access_date, entry.access_time);

    fattr3 {
        ftype,
        mode,
        nlink: if is_dir { 2 } else { 1 },
        uid: 501, // default macOS user
        gid: 20,  // staff group
        size,
        used: size,
        rdev: specdata3 {
            specdata1: 0,
            specdata2: 0,
        },
        fsid: 1,
        fileid: cluster_to_id(entry.first_cluster),
        atime,
        mtime,
        ctime,
    }
}

fn root_fattr() -> fattr3 {
    let now = now_nfstime();
    fattr3 {
        ftype: ftype3::NF3DIR,
        mode: 0o755,
        nlink: 2,
        uid: 501,
        gid: 20,
        size: 0,
        used: 0,
        rdev: specdata3 {
            specdata1: 0,
            specdata2: 0,
        },
        fsid: 1,
        fileid: ROOT_FILEID,
        atime: now,
        mtime: now,
        ctime: now,
    }
}

#[derive(Clone, Debug)]
struct DirtyFileState {
    parent_cluster: u32,
    first_cluster: u32,
    data: Vec<u8>,
}

/// Dirty write buffer: first_cluster -> buffered file state.
/// Writes accumulate here in memory and get flushed to disk periodically.
type DirtyFileMap = HashMap<u32, DirtyFileState>;

/// The NFS filesystem backed by a FatxVolume.
///
/// All blocking I/O (USB reads/writes via FatxVolume) is dispatched to
/// `tokio::task::spawn_blocking` so the async NFS event loop never stalls.
///
/// Concurrency model:
/// - vol: RwLock — read lock for cache-only ops (read_fat_entry, read_chain, stats),
///   write lock for device I/O (read_at, write_at, flush)
/// - dir_cache/file_cache: quick_cache — internally sharded, no external lock needed
/// - inode_parents: RwLock — read-heavy, small map
/// - dirty_files: Mutex — write-only, short critical sections
///
/// Lock ordering: (1) vol, (2) inode_parents, (3) caches (lockless), (4) dirty_files
struct FatxNfs {
    vol: Arc<RwLock<FatxVolume<File>>>,
    /// Cache: parent_cluster -> Vec<DirectoryEntry> (bounded, concurrent, no lock needed)
    dir_cache: Arc<Cache<u32, Vec<DirectoryEntry>>>,
    /// Reverse lookup: cluster -> (parent_cluster, name)
    inode_parents: Arc<RwLock<HashMap<u32, (u32, String)>>>,
    /// File data cache: cluster -> file bytes (bounded by weight, zero-copy via Bytes)
    file_cache: Arc<Cache<u32, Bytes>>,
    /// Dirty write buffer — see [DirtyFileMap].
    dirty_files: Arc<Mutex<DirtyFileMap>>,
    /// Whether the volume was opened read-only
    readonly: bool,
    /// Flag: set to true when a write dirtied the FAT; cleared after periodic flush
    flush_needed: Arc<AtomicBool>,
    /// Epoch millis of the last successful NFS I/O operation (watchdog heartbeat)
    last_io_epoch_ms: Arc<AtomicU64>,
}

impl FatxNfs {
    fn new(vol: FatxVolume<File>, readonly: bool) -> Self {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        FatxNfs {
            vol: Arc::new(RwLock::new(vol)),
            dir_cache: Arc::new(Cache::new(1000)),
            inode_parents: Arc::new(RwLock::new(HashMap::new())),
            file_cache: Arc::new(Cache::new(4096)),
            readonly,
            flush_needed: Arc::new(AtomicBool::new(false)),
            dirty_files: Arc::new(Mutex::new(HashMap::new())),
            last_io_epoch_ms: Arc::new(AtomicU64::new(now_ms)),
        }
    }

    /// Update the watchdog heartbeat — call after every successful NFS operation.
    fn touch_io(&self) {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        self.last_io_epoch_ms.store(now_ms, Ordering::Relaxed);
    }

    /// Read directory entries for a cluster, populating caches.
    /// Returns cached data on hit; only goes to USB on cache miss.
    async fn get_dir_entries(&self, cluster: u32) -> Result<Vec<DirectoryEntry>, nfsstat3> {
        // Fast path: check cache (quick_cache is concurrent, no lock needed)
        if let Some(entries) = self.dir_cache.get(&cluster) {
            return Ok(entries);
        }

        // Cache miss — read from USB via spawn_blocking
        let vol = Arc::clone(&self.vol);
        let dir_cache = Arc::clone(&self.dir_cache);
        let inode_parents = Arc::clone(&self.inode_parents);

        tokio::task::spawn_blocking(move || {
            // Double-check cache inside the blocking task
            if let Some(entries) = dir_cache.get(&cluster) {
                return Ok(entries);
            }

            let t0 = Instant::now();
            // Shared lock is safe here because `fatxlib` uses positional reads
            // for the shared-directory path instead of mutating the file cursor.
            let vol = vol.read();
            let entries = if cluster == FIRST_CLUSTER {
                vol.read_root_directory_shared()
            } else {
                vol.read_directory_shared(cluster)
            };
            let elapsed = t0.elapsed();

            match entries {
                Ok(entries) => {
                    info!(
                        "dir read cluster {} -> {} entries ({:.1}ms USB)",
                        cluster,
                        entries.len(),
                        elapsed.as_secs_f64() * 1000.0
                    );
                    {
                        let mut parents = inode_parents.write();
                        for e in &entries {
                            parents.insert(e.first_cluster, (cluster, e.filename()));
                        }
                    }
                    dir_cache.insert(cluster, entries.clone());
                    Ok(entries)
                }
                Err(e) => {
                    warn!(
                        "readdir cluster {} ({:.1}ms): {}",
                        cluster,
                        elapsed.as_secs_f64() * 1000.0,
                        e
                    );
                    Err(nfsstat3::NFS3ERR_IO)
                }
            }
        })
        .await
        .unwrap_or(Err(nfsstat3::NFS3ERR_IO))
    }

    /// Resolve a full FATX path from parent fileid + name.
    fn resolve_fatx_path(&self, parent_id: fileid3, name: &str) -> String {
        let parent_cluster = id_to_cluster(parent_id);
        let mut parts = vec![name.to_string()];
        let mut current = parent_cluster;
        let parents = self.inode_parents.read();
        while current != FIRST_CLUSTER {
            if let Some((grandparent, dir_name)) = parents.get(&current) {
                parts.push(dir_name.clone());
                current = *grandparent;
            } else {
                break;
            }
        }
        parts.reverse();
        format!("/{}", parts.join("/"))
    }

    fn dirty_state_is_in_subtree(
        parents: &HashMap<u32, (u32, String)>,
        state: &DirtyFileState,
        root_cluster: u32,
    ) -> bool {
        if state.first_cluster == root_cluster {
            return true;
        }

        let mut current = state.parent_cluster;
        loop {
            if current == root_cluster {
                return true;
            }
            if current == FIRST_CLUSTER {
                return false;
            }
            let Some((parent, _name)) = parents.get(&current) else {
                return false;
            };
            current = *parent;
        }
    }

    fn purge_dirty_subtree(&self, root_cluster: u32) {
        let to_remove = {
            let parents = self.inode_parents.read();
            let dirty = self.dirty_files.lock();
            dirty
                .iter()
                .filter_map(|(&cluster, state)| {
                    // inode_parents is opportunistic and may be incomplete for
                    // untouched ancestors. Purge the subtree entries we can
                    // prove belong here; any missed stale buffers are still
                    // dropped later because identity-based flush refuses to
                    // recreate missing dirents.
                    if Self::dirty_state_is_in_subtree(&parents, state, root_cluster) {
                        Some(cluster)
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
        };

        if to_remove.is_empty() {
            return;
        }

        {
            let mut dirty = self.dirty_files.lock();
            for cluster in &to_remove {
                dirty.remove(cluster);
            }
        }
        for cluster in to_remove {
            self.file_cache.remove(&cluster);
        }
    }

    /// Check if a filename is macOS metadata that should be silently rejected.
    #[allow(dead_code)] // used by tests; may be re-enabled for NFS filtering later
    fn is_macos_metadata(name: &str) -> bool {
        fatxlib::types::is_macos_metadata(name)
    }

    /// Invalidate the directory cache for a parent cluster, plus any cached
    /// file data for children of that directory (they may have new clusters
    /// after a delete+recreate write cycle).
    fn invalidate_dir(&self, parent_cluster: u32) {
        // Remove child file caches (quick_cache is concurrent, no external lock)
        if let Some(entries) = self.dir_cache.get(&parent_cluster) {
            for e in &entries {
                self.file_cache.remove(&e.first_cluster);
            }
        }
        self.dir_cache.remove(&parent_cluster);
    }

    /// Invalidate a single file's data cache (e.g. after write).
    #[allow(dead_code)] // kept for future use
    fn invalidate_file(&self, cluster: u32) {
        self.file_cache.remove(&cluster);
    }

    /// Check if readonly and return appropriate error.
    fn check_writable(&self) -> Result<(), nfsstat3> {
        if self.readonly {
            Err(nfsstat3::NFS3ERR_ROFS)
        } else {
            Ok(())
        }
    }
}

#[async_trait]
impl NFSFileSystem for FatxNfs {
    fn capabilities(&self) -> VFSCapabilities {
        if self.readonly {
            VFSCapabilities::ReadOnly
        } else {
            VFSCapabilities::ReadWrite
        }
    }

    fn root_dir(&self) -> fileid3 {
        ROOT_FILEID
    }

    async fn lookup(&self, dirid: fileid3, filename: &filename3) -> Result<fileid3, nfsstat3> {
        self.touch_io();
        let t0 = Instant::now();
        let cluster = id_to_cluster(dirid);
        let name_str =
            std::str::from_utf8(filename.as_ref()).map_err(|_| nfsstat3::NFS3ERR_NOENT)?;

        debug!("NFS lookup: dir={} name=\"{}\"", dirid, name_str);

        // Handle . and ..
        if name_str == "." {
            return Ok(dirid);
        }
        if name_str == ".." {
            let parents = self.inode_parents.read();
            if let Some((parent, _)) = parents.get(&cluster) {
                return Ok(cluster_to_id(*parent));
            }
            return Ok(ROOT_FILEID);
        }

        let entries = self.get_dir_entries(cluster).await?;
        for entry in &entries {
            if entry.filename().eq_ignore_ascii_case(name_str) {
                let id = cluster_to_id(entry.first_cluster);
                debug!(
                    "NFS lookup: \"{}\" -> id={} ({:.1}ms)",
                    name_str,
                    id,
                    t0.elapsed().as_secs_f64() * 1000.0
                );
                return Ok(id);
            }
        }
        debug!(
            "NFS lookup: \"{}\" -> NOENT ({:.1}ms)",
            name_str,
            t0.elapsed().as_secs_f64() * 1000.0
        );
        Err(nfsstat3::NFS3ERR_NOENT)
    }

    async fn getattr(&self, id: fileid3) -> Result<fattr3, nfsstat3> {
        self.touch_io();
        let t0 = Instant::now();
        if id == ROOT_FILEID {
            debug!(
                "NFS getattr: id={} (root) ({:.1}ms)",
                id,
                t0.elapsed().as_secs_f64() * 1000.0
            );
            return Ok(root_fattr());
        }

        let cluster = id_to_cluster(id);
        let parent_cluster = {
            let parents = self.inode_parents.read();
            parents.get(&cluster).map(|(p, _)| *p)
        };

        if let Some(pc) = parent_cluster {
            let entries = self.get_dir_entries(pc).await?;
            for entry in &entries {
                if entry.first_cluster == cluster {
                    debug!(
                        "NFS getattr: id={} \"{}\" size={} ({:.1}ms)",
                        id,
                        entry.filename(),
                        entry.file_size,
                        t0.elapsed().as_secs_f64() * 1000.0
                    );
                    return Ok(dirent_to_fattr(entry));
                }
            }
        }
        debug!(
            "NFS getattr: id={} -> NOENT ({:.1}ms)",
            id,
            t0.elapsed().as_secs_f64() * 1000.0
        );
        Err(nfsstat3::NFS3ERR_NOENT)
    }

    async fn setattr(&self, _id: fileid3, _setattr: sattr3) -> Result<fattr3, nfsstat3> {
        debug!("NFS setattr: id={}", _id);
        // FATX has limited attribute support; return current attrs
        self.getattr(_id).await
    }

    async fn read(
        &self,
        id: fileid3,
        offset: u64,
        count: u32,
    ) -> Result<(Vec<u8>, bool), nfsstat3> {
        self.touch_io();
        let t0 = Instant::now();

        let cluster = id_to_cluster(id);

        // Reads must reflect buffered-but-not-yet-flushed writes without
        // cloning the whole file into the read cache on every chunk.
        {
            let dirty = self.dirty_files.lock();
            if let Some(state) = dirty.get(&cluster) {
                return Ok(slice_buffered_range(&state.data, offset, count));
            }
        }

        // Fast path: serve from file cache without any USB I/O
        {
            // Fast path: serve from file cache (quick_cache — no lock needed)
            if let Some(data) = self.file_cache.get(&cluster) {
                let start = offset as usize;
                let end = (start + count as usize).min(data.len());
                if start >= data.len() {
                    debug!(
                        "NFS read: id={} offset={} count={} -> EOF (cached, {:.1}ms)",
                        id,
                        offset,
                        count,
                        t0.elapsed().as_secs_f64() * 1000.0
                    );
                    return Ok((vec![], true));
                }
                let eof = end >= data.len();
                debug!(
                    "NFS read: id={} offset={} count={} -> {} bytes, eof={} (cached, {:.1}ms)",
                    id,
                    offset,
                    count,
                    end - start,
                    eof,
                    t0.elapsed().as_secs_f64() * 1000.0
                );
                // Zero-copy slice from Bytes (refcount bump, no memcpy)
                return Ok((data.slice(start..end).to_vec(), eof));
            }
        }

        // Cache miss — find the directory entry, then read only the requested range
        let parent_cluster = {
            let parents = self.inode_parents.read();
            parents.get(&cluster).map(|(p, _)| *p)
        };

        let entry = if let Some(pc) = parent_cluster {
            let entries = self.get_dir_entries(pc).await?;
            entries.into_iter().find(|e| e.first_cluster == cluster)
        } else {
            None
        };

        let entry = entry.ok_or(nfsstat3::NFS3ERR_NOENT)?;
        let entry_for_read = entry.clone();

        let vol = Arc::clone(&self.vol);
        let data = tokio::task::spawn_blocking(move || {
            let t0 = Instant::now();
            // Shared lock is enough here because the range-read path is
            // implemented with positional reads in fatxlib.
            let vol = vol.read();
            match vol.read_file_range_shared(&entry_for_read, offset, count as usize) {
                Ok(data) => {
                    let elapsed = t0.elapsed();
                    info!(
                        "file range read cluster {} offset={} count={} -> {} bytes ({:.1}ms USB)",
                        cluster,
                        offset,
                        count,
                        data.len(),
                        elapsed.as_secs_f64() * 1000.0
                    );
                    Ok(data)
                }
                Err(e) => {
                    warn!("read cluster {}: {}", cluster, e);
                    Err(nfsstat3::NFS3ERR_IO)
                }
            }
        })
        .await
        .unwrap_or(Err(nfsstat3::NFS3ERR_IO))?;

        let eof = offset as usize + data.len() >= entry.file_size as usize;
        debug!(
            "NFS read: id={} offset={} count={} -> {} bytes, eof={} ({:.1}ms)",
            id,
            offset,
            count,
            data.len(),
            eof,
            t0.elapsed().as_secs_f64() * 1000.0
        );
        Ok((data, eof))
    }

    async fn write(&self, id: fileid3, offset: u64, data: &[u8]) -> Result<fattr3, nfsstat3> {
        self.touch_io();
        let t0 = Instant::now();
        self.check_writable()?;

        let cluster = id_to_cluster(id);
        debug!("NFS write: id={} offset={} len={}", id, offset, data.len());

        // Look up path info for this file (needed when we flush to disk later)
        let parent_cluster = {
            let parents = self.inode_parents.read();
            parents.get(&cluster).map(|(p, _)| *p)
        };
        let parent_cluster = parent_cluster.ok_or(nfsstat3::NFS3ERR_NOENT)?;

        let entry = {
            let entries = self.get_dir_entries(parent_cluster).await?;
            entries.into_iter().find(|e| e.first_cluster == cluster)
        };
        let entry = entry.ok_or(nfsstat3::NFS3ERR_NOENT)?;

        let needs_seed = {
            let dirty = self.dirty_files.lock();
            dirty.get(&cluster).is_none()
        };

        let prefetched_seed = if needs_seed {
            if let Some(cached) = self.file_cache.get(&cluster) {
                Some(cached.to_vec())
            } else {
                let vol = Arc::clone(&self.vol);
                let entry = entry.clone();
                Some(
                    tokio::task::spawn_blocking(move || {
                        let mut vol = vol.write();
                        vol.read_file(&entry).map_err(|e| {
                            warn!(
                                "seed read for cluster {} failed: {}",
                                entry.first_cluster, e
                            );
                            nfsstat3::NFS3ERR_IO
                        })
                    })
                    .await
                    .unwrap_or(Err(nfsstat3::NFS3ERR_IO))?,
                )
            }
        } else {
            None
        };

        // Buffer the write in memory — NO disk I/O here.
        // The periodic flush task will write dirty files to disk.
        let mut prefetched_seed = prefetched_seed;
        loop {
            let mut dirty = self.dirty_files.lock();
            match dirty.entry(cluster) {
                Entry::Occupied(mut entry) => {
                    let buf = entry.get_mut();
                    let write_end = offset as usize + data.len();
                    if write_end > buf.data.len() {
                        buf.data.resize(write_end, 0);
                    }
                    buf.data[offset as usize..write_end].copy_from_slice(data);
                    break;
                }
                Entry::Vacant(entry) => {
                    // We intentionally re-check here after prefetching outside the lock.
                    // Another writer may have inserted the dirty buffer while we were
                    // reading from cache/disk, in which case the Occupied arm wins and
                    // this prefetched seed is discarded instead of clobbering newer data.
                    let Some(seed) = prefetched_seed.take() else {
                        drop(dirty);
                        prefetched_seed =
                            self.file_cache.get(&cluster).map(|cached| cached.to_vec());
                        if prefetched_seed.is_some() {
                            continue;
                        }
                        warn!(
                            "seed buffer for cluster {} disappeared before insert; returning I/O error",
                            cluster
                        );
                        return Err(nfsstat3::NFS3ERR_IO);
                    };

                    let buf = entry.insert(DirtyFileState {
                        parent_cluster,
                        first_cluster: cluster,
                        data: seed,
                    });
                    let write_end = offset as usize + data.len();
                    if write_end > buf.data.len() {
                        buf.data.resize(write_end, 0);
                    }
                    buf.data[offset as usize..write_end].copy_from_slice(data);
                    break;
                }
            }
        }

        self.flush_needed.store(true, Ordering::Relaxed);

        debug!(
            "NFS write: id={} buffered ({:.1}ms)",
            id,
            t0.elapsed().as_secs_f64() * 1000.0
        );
        self.getattr(id).await
    }

    async fn create(
        &self,
        dirid: fileid3,
        filename: &filename3,
        _attr: sattr3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        let t0 = Instant::now();
        self.check_writable()?;

        let name_str =
            std::str::from_utf8(filename.as_ref()).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;

        info!("NFS create: dir={} name=\"{}\"", dirid, name_str);

        let path = self.resolve_fatx_path(dirid, name_str);
        let parent_cluster = id_to_cluster(dirid);

        let vol = Arc::clone(&self.vol);
        let path_clone = path.clone();
        tokio::task::spawn_blocking(move || {
            let mut vol = vol.write();
            // Truncate semantics: if file exists, replace it.
            vol.create_or_replace_file(&path_clone, &[]).map_err(|e| {
                warn!("create '{}': {}", path_clone, e);
                nfsstat3::NFS3ERR_IO
            })?;
            Ok::<(), nfsstat3>(())
        })
        .await
        .unwrap_or(Err(nfsstat3::NFS3ERR_IO))?;

        self.flush_needed.store(true, Ordering::Relaxed);
        self.invalidate_dir(parent_cluster);

        // Look up the new entry
        let entries = self.get_dir_entries(parent_cluster).await?;
        for entry in &entries {
            if entry.filename().eq_ignore_ascii_case(name_str) {
                info!(
                    "NFS create: \"{}\" -> id={} ({:.1}ms)",
                    name_str,
                    cluster_to_id(entry.first_cluster),
                    t0.elapsed().as_secs_f64() * 1000.0
                );
                return Ok((cluster_to_id(entry.first_cluster), dirent_to_fattr(entry)));
            }
        }
        error!(
            "NFS create: \"{}\" created but not found in dir listing!",
            name_str
        );
        Err(nfsstat3::NFS3ERR_IO)
    }

    async fn create_exclusive(
        &self,
        dirid: fileid3,
        filename: &filename3,
    ) -> Result<fileid3, nfsstat3> {
        let t0 = Instant::now();
        self.check_writable()?;

        let name_str =
            std::str::from_utf8(filename.as_ref()).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;
        info!("NFS create exclusive: dir={} name=\"{}\"", dirid, name_str);

        let path = self.resolve_fatx_path(dirid, name_str);
        let parent_cluster = id_to_cluster(dirid);

        let vol = Arc::clone(&self.vol);
        let path_clone = path.clone();
        tokio::task::spawn_blocking(move || {
            let mut vol = vol.write();
            match vol.create_file(&path_clone, &[]) {
                Ok(()) => Ok::<(), nfsstat3>(()),
                Err(fatxlib::error::FatxError::FileExists(_)) => Err(nfsstat3::NFS3ERR_EXIST),
                Err(e) => {
                    warn!("create exclusive '{}': {}", path_clone, e);
                    Err(nfsstat3::NFS3ERR_IO)
                }
            }
        })
        .await
        .unwrap_or(Err(nfsstat3::NFS3ERR_IO))?;

        self.flush_needed.store(true, Ordering::Relaxed);
        self.invalidate_dir(parent_cluster);

        let entries = self.get_dir_entries(parent_cluster).await?;
        for entry in &entries {
            if entry.filename().eq_ignore_ascii_case(name_str) {
                info!(
                    "NFS create exclusive: \"{}\" -> id={} ({:.1}ms)",
                    name_str,
                    cluster_to_id(entry.first_cluster),
                    t0.elapsed().as_secs_f64() * 1000.0
                );
                return Ok(cluster_to_id(entry.first_cluster));
            }
        }

        error!(
            "NFS create exclusive: \"{}\" created but not found in dir listing!",
            name_str
        );
        Err(nfsstat3::NFS3ERR_IO)
    }

    async fn mkdir(
        &self,
        dirid: fileid3,
        dirname: &filename3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        let t0 = Instant::now();
        self.check_writable()?;

        let name_str =
            std::str::from_utf8(dirname.as_ref()).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;

        info!("NFS mkdir: dir={} name=\"{}\"", dirid, name_str);
        let path = self.resolve_fatx_path(dirid, name_str);
        let parent_cluster = id_to_cluster(dirid);

        let vol = Arc::clone(&self.vol);
        let path_clone = path.clone();
        let mkdir_result = tokio::task::spawn_blocking(move || {
            let mut vol = vol.write();
            match vol.create_directory(&path_clone) {
                Ok(()) => {
                    Ok(false) // created new
                }
                Err(fatxlib::error::FatxError::FileExists(_)) => {
                    Ok(true) // already exists — not an error
                }
                Err(e) => {
                    warn!("mkdir '{}': {}", path_clone, e);
                    Err(nfsstat3::NFS3ERR_IO)
                }
            }
        })
        .await
        .unwrap_or(Err(nfsstat3::NFS3ERR_IO))?;

        if !mkdir_result {
            self.flush_needed.store(true, Ordering::Relaxed);
        }
        self.invalidate_dir(parent_cluster);

        let entries = self.get_dir_entries(parent_cluster).await?;
        for entry in &entries {
            if entry.filename().eq_ignore_ascii_case(name_str) {
                let action = if mkdir_result { "exists" } else { "created" };
                info!(
                    "NFS mkdir: \"{}\" {} -> id={} ({:.1}ms)",
                    name_str,
                    action,
                    cluster_to_id(entry.first_cluster),
                    t0.elapsed().as_secs_f64() * 1000.0
                );
                return Ok((cluster_to_id(entry.first_cluster), dirent_to_fattr(entry)));
            }
        }
        error!(
            "NFS mkdir: \"{}\" created but not found in dir listing!",
            name_str
        );
        Err(nfsstat3::NFS3ERR_IO)
    }

    async fn remove(&self, dirid: fileid3, filename: &filename3) -> Result<(), nfsstat3> {
        let t0 = Instant::now();
        self.check_writable()?;

        let name_str =
            std::str::from_utf8(filename.as_ref()).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;

        info!("NFS remove: dir={} name=\"{}\"", dirid, name_str);
        let path = self.resolve_fatx_path(dirid, name_str);
        let removed_cluster = {
            let entries = self.get_dir_entries(id_to_cluster(dirid)).await?;
            entries
                .into_iter()
                .find(|e| e.filename().eq_ignore_ascii_case(name_str))
                .map(|e| e.first_cluster)
        };
        let vol = Arc::clone(&self.vol);
        let path_clone = path.clone();
        tokio::task::spawn_blocking(move || {
            let mut vol = vol.write();
            match vol.delete(&path_clone) {
                Ok(()) => Ok(()),
                Err(fatxlib::error::FatxError::DirectoryNotEmpty(_)) => {
                    // Finder sends a single remove for non-empty directories.
                    // NFS has no recursive delete, so we handle it here.
                    info!("remove '{}': not empty, using recursive delete", path_clone);
                    vol.delete_recursive(&path_clone).map_err(|e| {
                        warn!("recursive remove '{}': {}", path_clone, e);
                        nfsstat3::NFS3ERR_IO
                    })
                }
                Err(e) => {
                    warn!("remove '{}': {}", path_clone, e);
                    Err(nfsstat3::NFS3ERR_IO)
                }
            }
        })
        .await
        .unwrap_or(Err(nfsstat3::NFS3ERR_IO))?;

        if let Some(cluster) = removed_cluster {
            // Drop any delayed-write state for the deleted subtree so the
            // periodic flush cannot recreate files after the delete succeeds.
            self.purge_dirty_subtree(cluster);
        }

        self.flush_needed.store(true, Ordering::Relaxed);

        // Invalidate caches — a recursive delete can affect many directories.
        // quick_cache doesn't have clear(), so we invalidate the parent and
        // rely on cache misses to re-read from disk for other entries.
        self.invalidate_dir(id_to_cluster(dirid));

        info!(
            "NFS remove: \"{}\" done ({:.1}ms)",
            name_str,
            t0.elapsed().as_secs_f64() * 1000.0
        );
        Ok(())
    }

    async fn rename(
        &self,
        from_dirid: fileid3,
        from_filename: &filename3,
        to_dirid: fileid3,
        to_filename: &filename3,
    ) -> Result<(), nfsstat3> {
        let t0 = Instant::now();
        self.check_writable()?;

        // FATX only supports same-directory rename
        if from_dirid != to_dirid {
            warn!("NFS rename: cross-directory rename not supported");
            return Err(nfsstat3::NFS3ERR_NOTSUPP);
        }

        let from_name =
            std::str::from_utf8(from_filename.as_ref()).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;
        let to_name =
            std::str::from_utf8(to_filename.as_ref()).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;
        info!("NFS rename: \"{}\" -> \"{}\"", from_name, to_name);
        let path = self.resolve_fatx_path(from_dirid, from_name);
        let parent_cluster = id_to_cluster(from_dirid);

        let vol = Arc::clone(&self.vol);
        let path_clone = path.clone();
        let to_name_owned = to_name.to_string();
        tokio::task::spawn_blocking(move || {
            let mut vol = vol.write();
            vol.rename(&path_clone, &to_name_owned).map_err(|e| {
                warn!("rename '{}' -> '{}': {}", path_clone, to_name_owned, e);
                nfsstat3::NFS3ERR_IO
            })?;
            Ok::<(), nfsstat3>(())
        })
        .await
        .unwrap_or(Err(nfsstat3::NFS3ERR_IO))?;

        self.flush_needed.store(true, Ordering::Relaxed);
        self.invalidate_dir(parent_cluster);
        info!(
            "NFS rename: \"{}\" -> \"{}\" done ({:.1}ms)",
            from_name,
            to_name,
            t0.elapsed().as_secs_f64() * 1000.0
        );
        Ok(())
    }

    async fn readdir(
        &self,
        dirid: fileid3,
        start_after: fileid3,
        max_entries: usize,
    ) -> Result<ReadDirResult, nfsstat3> {
        let t0 = Instant::now();
        let cluster = id_to_cluster(dirid);
        debug!(
            "NFS readdir: dir={} start_after={} max={}",
            dirid, start_after, max_entries
        );
        let entries = self.get_dir_entries(cluster).await?;

        let mut result = Vec::new();

        // Build full listing: . and .. first, then entries
        let mut full_list: Vec<(fileid3, fattr3, String)> = Vec::new();

        // Add . entry
        let self_attr = self.getattr(dirid).await?;
        full_list.push((dirid, self_attr, ".".to_string()));

        // Add .. entry
        let parent_id = {
            let parents = self.inode_parents.read();
            parents
                .get(&cluster)
                .map(|(p, _)| cluster_to_id(*p))
                .unwrap_or(ROOT_FILEID)
        };
        let parent_attr = self.getattr(parent_id).await?;
        full_list.push((parent_id, parent_attr, "..".to_string()));

        // Add real entries
        for entry in &entries {
            full_list.push((
                cluster_to_id(entry.first_cluster),
                dirent_to_fattr(entry),
                entry.filename(),
            ));
        }

        // Pagination: skip entries until we pass start_after
        let mut found_start = start_after == 0;
        for (id, attr, name) in &full_list {
            if !found_start {
                if *id == start_after {
                    found_start = true;
                }
                continue;
            }
            if result.len() >= max_entries {
                return Ok(ReadDirResult {
                    entries: result,
                    end: false,
                });
            }
            result.push(DirEntry {
                fileid: *id,
                name: name.as_bytes().into(),
                attr: *attr,
            });
        }

        debug!(
            "NFS readdir: dir={} returning {} entries, end=true ({:.1}ms)",
            dirid,
            result.len(),
            t0.elapsed().as_secs_f64() * 1000.0
        );
        Ok(ReadDirResult {
            entries: result,
            end: true,
        })
    }

    async fn symlink(
        &self,
        _dirid: fileid3,
        _linkname: &filename3,
        _symlink: &nfspath3,
        _attr: &sattr3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        // FATX does not support symlinks
        Err(nfsstat3::NFS3ERR_NOTSUPP)
    }

    async fn readlink(&self, _id: fileid3) -> Result<nfspath3, nfsstat3> {
        Err(nfsstat3::NFS3ERR_NOTSUPP)
    }
}

// ===========================================================================
// CLI
// ===========================================================================

fn parse_hex_or_dec(s: &str) -> Result<u64, String> {
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16).map_err(|e| e.to_string())
    } else {
        s.parse::<u64>().map_err(|e| e.to_string())
    }
}

/// Get the size of a device, handling macOS raw block devices correctly.
fn get_device_size(file: &mut File) -> u64 {
    if let Ok(size) = file.seek(SeekFrom::End(0)) {
        if size > 0 {
            let _ = file.seek(SeekFrom::Start(0));
            return size;
        }
    }

    #[cfg(target_os = "macos")]
    {
        use std::os::unix::io::AsRawFd;
        if let Some(size) = fatxlib::platform::get_block_device_size(file.as_raw_fd()) {
            let _ = file.seek(SeekFrom::Start(0));
            return size;
        }
    }

    let _ = file.seek(SeekFrom::Start(0));
    0
}

#[derive(clap::Args)]
#[command(about = "Mount Xbox FATX/XTAF file systems (shows in Finder)")]
pub struct MountArgs {
    /// Device or disk image to mount (omit for guided selection)
    pub device: Option<PathBuf>,

    /// Partition name (e.g. "360 Data", "Data (E)")
    #[arg(long)]
    pub partition: Option<String>,

    /// Manual partition offset (hex or decimal)
    #[arg(long, value_parser = parse_hex_or_dec, default_value = "0")]
    offset: u64,

    /// Manual partition size (hex or decimal, 0 = auto)
    #[arg(long, value_parser = parse_hex_or_dec, default_value = "0")]
    size: u64,

    /// NFS server port
    #[arg(long, default_value = "11111")]
    port: u16,

    /// Mount point (default: /Volumes/Xbox Drive)
    #[arg(long)]
    mountpoint: Option<PathBuf>,

    /// Enable verbose logging (info + NFS operations)
    #[arg(short = 'v', long)]
    verbose: bool,

    /// Enable trace logging (debug-level: every NFS lookup/getattr/read)
    #[arg(long)]
    trace: bool,

    /// Mount read-only
    #[arg(long)]
    readonly: bool,

    /// Actually mount in Finder (off by default for safety).
    /// Without this flag, only the NFS server starts and you can
    /// test with: showmount -e localhost
    #[arg(long)]
    pub mount: bool,

    /// Emergency cleanup: kill stale NFS mounts and exit.
    /// Use this if a previous fatx-mount session left a zombie mount.
    #[arg(long)]
    pub cleanup: bool,
}

/// Entry point for the mount subcommand. Creates a tokio runtime and runs the async main.
pub fn run(cli: MountArgs) {
    let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
    rt.block_on(async_main(cli));
}

async fn async_main(cli: MountArgs) {
    let log_level = if cli.trace {
        "debug"
    } else if cli.verbose {
        "info"
    } else {
        "warn"
    };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(log_level))
        .format_timestamp_millis()
        .init();

    info!("fatx-mount starting (log_level={})", log_level);

    // ── PANIC HANDLER: ensure we try to unmount even on unexpected crashes ──
    // A panic that kills the NFS server without unmounting first leaves a zombie
    // mount that deadlocks Finder and requires a reboot to fix.
    {
        let panic_mp = cli
            .mountpoint
            .as_deref()
            .unwrap_or_else(|| std::path::Path::new("/Volumes/Xbox Drive"))
            .display()
            .to_string();
        let should_mount = cli.mount;
        let default_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if should_mount {
                eprintln!("\n[PANIC] fatx-mount crashed! Emergency unmount...");
                let _ = std::process::Command::new("bash")
                    .args([
                        "-c",
                        &format!(
                            "timeout 5 umount -f '{}' 2>/dev/null; \
                         timeout 3 rm -rf '{}' 2>/dev/null; true",
                            panic_mp, panic_mp
                        ),
                    ])
                    .output();
            }
            default_hook(info);
        }));
    }

    // --cleanup: emergency recovery from stale NFS mounts
    // ALL operations use `timeout` to prevent hanging on dead mounts.
    if cli.cleanup {
        eprintln!("[cleanup] Emergency cleanup of stale fatx-mount NFS mounts...");
        eprintln!("[cleanup] (All operations use timeouts to avoid hanging)");

        // Kill any fatx-mount/mount_nfs processes (not us)
        let our_pid = std::process::id();
        eprintln!("[cleanup] Killing stale processes (our PID={})...", our_pid);
        let _ = std::process::Command::new("bash")
            .args([
                "-c",
                &format!(
                    "pgrep -f fatx-mount | grep -v {} | xargs -r kill -9 2>/dev/null; \
                 killall -9 mount_nfs 2>/dev/null; true",
                    our_pid
                ),
            ])
            .output();

        // Force-unmount any localhost NFS mounts (with timeout!)
        let mount_output = std::process::Command::new("mount")
            .output()
            .expect("failed to run mount");
        let mount_list = String::from_utf8_lossy(&mount_output.stdout);
        for line in mount_list.lines() {
            if line.contains("localhost") || line.contains("127.0.0.1") {
                let parts: Vec<&str> = line.split(" on ").collect();
                if parts.len() >= 2 {
                    let mp = parts[1].split(' ').next().unwrap_or("");
                    if !mp.is_empty() {
                        eprintln!("  Force-unmounting (3s timeout): {}", mp);
                        let _ = std::process::Command::new("bash")
                            .args([
                                "-c",
                                &format!(
                                    "timeout 3 umount -f '{}' 2>&1 || echo 'umount timed out'",
                                    mp
                                ),
                            ])
                            .output();
                    }
                }
            }
        }

        // Clean up mount point directories (with timeout — rm can hang too!)
        let default_mps = ["/Volumes/Xbox Drive", "/Volumes/TestFATX"];
        for mp in &default_mps {
            eprintln!("  Removing mount point (3s timeout): {}", mp);
            let _ = std::process::Command::new("bash")
                .args([
                    "-c",
                    &format!(
                        "timeout 3 rm -rf '{}' 2>/dev/null || echo 'rm timed out for {}'",
                        mp, mp
                    ),
                ])
                .output();
        }
        if let Some(ref mp) = cli.mountpoint {
            eprintln!("  Removing mount point (3s timeout): {}", mp.display());
            let _ = std::process::Command::new("bash")
                .args([
                    "-c",
                    &format!("timeout 3 rm -rf '{}' 2>/dev/null || true", mp.display()),
                ])
                .output();
        }

        eprintln!("[cleanup] Done. If Finder is still broken, reboot is required.");
        eprintln!("[cleanup] (A stale mount in kernel D-state can only be cleared by reboot)");
        std::process::exit(0);
    }

    let device_path = cli.device.as_ref().unwrap_or_else(|| {
        eprintln!("Device path is required (unless using --cleanup)");
        std::process::exit(1);
    });

    // Open the device
    let mut file = if cli.readonly {
        File::open(device_path)
    } else {
        OpenOptions::new().read(true).write(true).open(device_path)
    }
    .unwrap_or_else(|e| {
        eprintln!("Error opening '{}': {}", device_path.display(), e);
        if e.kind() == std::io::ErrorKind::NotFound {
            eprintln!("Device not found. Run 'diskutil list' to find your Xbox drive.");
            eprintln!("Look for an unrecognized disk and use /dev/rdiskN (raw device).");
        } else if e.kind() == std::io::ErrorKind::PermissionDenied {
            eprintln!(
                "Permission denied. Try: sudo fatx mount {}",
                device_path.display()
            );
        } else {
            eprintln!(
                "Try: sudo fatx mount {} --partition \"360 Data\"",
                device_path.display()
            );
        }
        std::process::exit(1);
    });

    let device_size = get_device_size(&mut file);

    // Resolve partition
    let (offset, size) = if let Some(ref name) = cli.partition {
        let partitions = detect_xbox_partitions(&mut file, device_size).unwrap_or_else(|e| {
            eprintln!("Error scanning partitions: {}", e);
            std::process::exit(1);
        });

        let found = partitions
            .iter()
            .find(|p| p.name.eq_ignore_ascii_case(name));

        match found {
            Some(p) => {
                info!("Using partition '{}' at offset 0x{:X}", p.name, p.offset);
                (p.offset, p.size)
            }
            None => {
                eprintln!("Partition '{}' not found. Available:", name);
                for p in &partitions {
                    eprintln!(
                        "  {} (offset 0x{:X}, size {})",
                        p.name,
                        p.offset,
                        format_size(p.size)
                    );
                }
                std::process::exit(1);
            }
        }
    } else {
        (cli.offset, cli.size)
    };

    // Capture raw fd before file is moved into the volume
    #[cfg(target_os = "macos")]
    let raw_fd = {
        use std::os::unix::io::AsRawFd;
        file.as_raw_fd()
    };

    // Open the FATX volume
    let mut vol = FatxVolume::open(file, offset, size).unwrap_or_else(|e| {
        eprintln!("Error opening FATX volume: {}", e);
        std::process::exit(1);
    });

    // Configure macOS-specific I/O (F_NOCACHE, F_RDAHEAD, device params)
    #[cfg(target_os = "macos")]
    vol.configure_device(raw_fd);

    let cluster_size = vol.superblock.cluster_size();
    let total_clusters = vol.total_clusters;
    info!(
        "FATX volume: {} clusters x {} = {}",
        total_clusters,
        format_size(cluster_size),
        format_size(total_clusters as u64 * cluster_size)
    );

    let mode = if cli.readonly {
        "read-only"
    } else {
        "read-write"
    };
    info!("Creating NFS filesystem adapter (mode={})", mode);
    let fs = FatxNfs::new(vol, cli.readonly);

    // Grab references for the periodic flush task, watchdog, and shutdown flush
    let flush_vol = Arc::clone(&fs.vol);
    let flush_flag = Arc::clone(&fs.flush_needed);
    let flush_dirty = Arc::clone(&fs.dirty_files);
    let flush_dir_cache = Arc::clone(&fs.dir_cache);
    let flush_file_cache = Arc::clone(&fs.file_cache);
    let watchdog_io = Arc::clone(&fs.last_io_epoch_ms);
    let watchdog_dirty = Arc::clone(&fs.dirty_files);
    let shutdown_vol = Arc::clone(&fs.vol);
    let shutdown_flush_flag = Arc::clone(&fs.flush_needed);
    let shutdown_dirty = Arc::clone(&fs.dirty_files);
    let shutdown_dir_cache = Arc::clone(&fs.dir_cache);

    let bind_addr = format!("127.0.0.1:{}", cli.port);
    info!("Binding NFS server to {}...", bind_addr);
    let listener = match NFSTcpListener::bind(&bind_addr, fs).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("Failed to bind NFS server on {}: {}", bind_addr, e);
            eprintln!();
            eprintln!(
                "Port {} is already in use. A previous fatx-mount may still be running.",
                cli.port
            );
            eprintln!();
            eprintln!("To fix:");
            eprintln!(
                "  sudo lsof -i :{} | grep LISTEN   # find the process",
                cli.port
            );
            eprintln!("  sudo kill <PID>                    # kill it");
            eprintln!();
            eprintln!("Or use a different port:");
            eprintln!("  fatx mount {} --port 11112", device_path.display());
            std::process::exit(1);
        }
    };

    let port = listener.get_listen_port();
    println!("NFS server listening on 127.0.0.1:{}", port);
    info!("NFS server ready on port {}", port);

    // Resolve mountpoint string once — used by mount logic and Ctrl+C handler
    let mp_str = if cli.mount {
        cli.mountpoint
            .as_deref()
            .unwrap_or_else(|| std::path::Path::new("/Volumes/Xbox Drive"))
            .display()
            .to_string()
    } else {
        String::new()
    };

    // Auto-mount unless --no-mount
    if cli.mount {
        let mountpoint = PathBuf::from(&mp_str);

        // ── AGGRESSIVE STALE MOUNT CLEANUP ──
        // A previous fatx-mount crash can leave a zombie NFS mount that hangs
        // Finder and makes umount -f hang. We MUST clean this up before mounting.
        // IMPORTANT: Never use `ls`, `stat`, or anything that touches the mount
        // path — those will hang too. Only use umount/rm with timeouts.
        if mountpoint.exists() {
            eprintln!("[startup] Cleaning up stale mount at {}...", mp_str);

            // Kill any lingering mount_nfs or fatx-mount processes (not us)
            let our_pid = std::process::id();
            let _ = std::process::Command::new("bash")
                .args([
                    "-c",
                    &format!(
                        "pgrep -f fatx-mount | grep -v {} | xargs -r kill -9 2>/dev/null; \
                     killall -9 mount_nfs 2>/dev/null; true",
                        our_pid
                    ),
                ])
                .output();

            // Try force-unmount with a 3-second timeout (umount -f can hang on dead NFS)
            let umount_result = std::process::Command::new("bash")
                .args(["-c", &format!(
                    "timeout 3 umount -f '{}' 2>&1 || timeout 3 diskutil unmount force '{}' 2>&1 || true",
                    mp_str, mp_str
                )])
                .output();

            if let Ok(o) = &umount_result {
                let out = String::from_utf8_lossy(&o.stdout);
                if !out.trim().is_empty() {
                    eprintln!("[startup] umount: {}", out.trim());
                }
            }

            // Remove the stale mountpoint directory (this works even when umount hangs,
            // as long as the mount is no longer in the kernel mount table)
            let _ = std::process::Command::new("bash")
                .args([
                    "-c",
                    &format!("timeout 3 rm -rf '{}' 2>/dev/null || true", mp_str),
                ])
                .output();

            eprintln!("[startup] Stale mount cleanup done.");
        }

        // Create mount point
        if !mountpoint.exists() {
            if let Err(e) = std::fs::create_dir_all(&mountpoint) {
                eprintln!("Failed to create mount point '{}': {}", mp_str, e);
                eprintln!("Try: sudo mkdir -p \"{}\"", mp_str);
                std::process::exit(1);
            }
        }

        // Mount in background with a timeout so it can't hang forever
        let mp_clone = mp_str.clone();
        tokio::spawn(async move {
            // Small delay to let the NFS server start accepting connections
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;

            let opts = format!(
                "nolocks,noresvport,vers=3,tcp,rsize=131072,wsize=131072,actimeo=2,intr,soft,retrans=2,timeo=10,port={port},mountport={port}"
            );

            info!(
                "Running: mount_nfs -o {} localhost:/ \"{}\"",
                opts, mp_clone
            );

            // Use tokio timeout so a hanging mount_nfs doesn't block forever
            let mount_future = tokio::process::Command::new("mount_nfs")
                .args(["-o", &opts, "localhost:/", &mp_clone])
                .output();

            match tokio::time::timeout(std::time::Duration::from_secs(10), mount_future).await {
                Ok(Ok(o)) if o.status.success() => {
                    println!("Mounted at {}", mp_clone);
                    println!("The drive should appear in Finder.");
                    println!("Unmount with: umount \"{}\"", mp_clone);
                }
                Ok(Ok(o)) => {
                    let stderr = String::from_utf8_lossy(&o.stderr);
                    error!("mount_nfs failed: {}", stderr);
                    eprintln!("Mount failed: {}", stderr);
                    eprintln!(
                        "Try manually: mount_nfs -o nolocks,noresvport,vers=3,tcp,port={port},mountport={port} localhost:/ \"{}\"",
                        mp_clone
                    );
                }
                Ok(Err(e)) => {
                    error!("Failed to run mount_nfs: {}", e);
                }
                Err(_) => {
                    eprintln!("Mount timed out after 10s. Killing mount_nfs...");
                    let _ = std::process::Command::new("killall")
                        .args(["-9", "mount_nfs"])
                        .output();
                    eprintln!(
                        "Try manually: mount_nfs -o nolocks,noresvport,vers=3,tcp,port={port},mountport={port} localhost:/ \"{}\"",
                        mp_clone
                    );
                }
            }
        });
    } else {
        println!("NFS server running (no auto-mount). To mount in Finder:");
        println!(
            "  sudo mount_nfs -o nolocks,noresvport,vers=3,tcp,soft,intr,retrans=2,timeo=10,port={port},mountport={port} localhost:/ /Volumes/Xbox\\ Drive"
        );
        println!("To unmount:");
        println!("  sudo umount -f /Volumes/Xbox\\ Drive");
        println!("Pass --mount to auto-mount in Finder.");
    }

    // Periodic flush task — writes dirty files to disk and flushes the FAT every
    // 5 seconds. This batches many small NFS writes into one disk operation per file.
    //
    // CRITICAL: We flush ONE file at a time, releasing the vol lock between files.
    // A single large file (130MB+) can take seconds to write over USB. If we held
    // the vol lock for the entire batch, all NFS operations would block and Finder
    // would time out with ESTALE. By releasing between files, write() can still
    // seed new buffers from disk between flushes.
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
        loop {
            interval.tick().await;
            if flush_flag.swap(false, Ordering::Relaxed) {
                let vol = Arc::clone(&flush_vol);
                let dirty = Arc::clone(&flush_dirty);
                let _dir_cache = Arc::clone(&flush_dir_cache);
                let file_cache = Arc::clone(&flush_file_cache);
                let _ = tokio::task::spawn_blocking(move || {
                    let t0 = Instant::now();

                    // Drain all dirty files
                    let pending: Vec<DirtyFileState> = {
                        let mut dirty = dirty.lock();
                        let taken = std::mem::take(&mut *dirty);
                        for state in taken.values() {
                            // Publish the buffered view once per flush cycle so
                            // reads stay coherent while cluster writes are in
                            // flight after `dirty_files` has been drained.
                            file_cache
                                .insert(state.first_cluster, Bytes::copy_from_slice(&state.data));
                        }
                        taken.into_values().collect()
                    };

                    if !pending.is_empty() {
                        let count = pending.len();

                        // Flush each file with chunked cluster writes.
                        // The vol lock is held briefly for FAT chain management,
                        // then released between each cluster write so NFS reads
                        // (Finder browsing) can interleave.
                        for state in pending {
                            let ft = Instant::now();
                            let data_len = state.data.len();

                            // Phase 1: Prepare chain using stable file identity.
                            let session = {
                                let mut vol = vol.write();
                                match vol.begin_write_in_place_for_entry(
                                    state.parent_cluster,
                                    state.first_cluster,
                                    state.data.len(),
                                ) {
                                    Ok(session) => session,
                                    Err(fatxlib::error::FatxError::FileNotFound(_)) => {
                                        info!(
                                            "Dropping dirty buffer for deleted/missing cluster {}",
                                            state.first_cluster
                                        );
                                        continue;
                                    }
                                    Err(e) => {
                                        error!(
                                            "Flush prepare cluster {} failed: {}",
                                            state.first_cluster, e
                                        );
                                        continue;
                                    }
                                }
                            };
                            // Vol lock released — NFS ops can proceed between clusters

                            // Phase 2: Write data cluster-by-cluster, releasing lock between each
                            let cluster_size = {
                                let vol = vol.read();
                                vol.superblock.cluster_size() as usize
                            };
                            let mut offset = 0usize;
                            let mut write_failed = false;
                            for &c in session.clusters() {
                                let end = (offset + cluster_size).min(state.data.len());
                                let mut cluster_buf = vec![0u8; cluster_size];
                                if offset < state.data.len() {
                                    let len = end - offset;
                                    cluster_buf[..len].copy_from_slice(&state.data[offset..end]);
                                }
                                {
                                    let mut vol = vol.write();
                                    if let Err(e) = vol.write_cluster(c, &cluster_buf) {
                                        error!(
                                            "Flush cluster {} for file {} failed: {}",
                                            c, state.first_cluster, e
                                        );
                                        write_failed = true;
                                        break;
                                    }
                                }
                                // Lock released — NFS reads can proceed
                                offset += cluster_size;
                                if offset >= state.data.len() {
                                    break;
                                }
                            }

                            {
                                let mut vol = vol.write();
                                if write_failed {
                                    if let Err(e) = vol.cancel_write_session(session) {
                                        error!(
                                            "Flush cancel cluster {} failed: {}",
                                            state.first_cluster, e
                                        );
                                    }
                                    continue;
                                }
                                if let Err(e) = vol.commit_write_session(session) {
                                    error!(
                                        "Flush commit cluster {} failed: {}",
                                        state.first_cluster, e
                                    );
                                    continue;
                                }
                            }

                            info!(
                                "Flushed cluster {} ({} bytes, {:.1}ms)",
                                state.first_cluster,
                                data_len,
                                ft.elapsed().as_secs_f64() * 1000.0
                            );
                        }

                        // Dir cache entries will naturally expire from quick_cache.
                        // Targeted invalidation already happens in write handlers.

                        // Now flush the FAT (one final lock)
                        {
                            let mut vol = vol.write();
                            if let Err(e) = vol.flush() {
                                error!("Periodic FAT flush failed: {}", e);
                            }
                        }
                        info!(
                            "Periodic flush: {} file(s) + FAT ({:.1}ms)",
                            count,
                            t0.elapsed().as_secs_f64() * 1000.0
                        );
                    } else {
                        // No dirty files, just flush the FAT
                        let mut vol = vol.write();
                        if let Err(e) = vol.flush() {
                            error!("Periodic FAT flush failed: {}", e);
                        } else {
                            info!(
                                "Periodic FAT flush ({:.1}ms)",
                                t0.elapsed().as_secs_f64() * 1000.0
                            );
                        }
                    }
                })
                .await;
            }
        }
    });

    // ── WATCHDOG: auto-shutdown if NFS server stalls ──
    // If no NFS I/O happens for 120 seconds AND there are dirty writes pending,
    // the server has probably hung. Force-unmount and exit to prevent the
    // catastrophic stale-mount deadlock that kills Finder.
    // If there are no dirty writes, the drive is just idle — that's fine.
    {
        let watchdog_mp = if cli.mount {
            Some(mp_str.clone())
        } else {
            None
        };
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(15));
            loop {
                interval.tick().await;
                let last_ms = watchdog_io.load(Ordering::Relaxed);
                let now_ms = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64;
                let idle_secs = (now_ms.saturating_sub(last_ms)) / 1000;

                // Only panic if we have dirty writes AND the server is unresponsive
                let has_dirty = {
                    let dirty = watchdog_dirty.lock();
                    !dirty.is_empty()
                };

                if idle_secs > 120 && has_dirty {
                    eprintln!(
                        "\n[WATCHDOG] NFS server unresponsive for {}s with dirty writes pending!",
                        idle_secs
                    );
                    eprintln!("[WATCHDOG] Triggering emergency shutdown to prevent stale mount...");

                    // Emergency unmount
                    if let Some(ref mp) = watchdog_mp {
                        eprintln!("[WATCHDOG] Force-unmounting {}...", mp);
                        let _ = std::process::Command::new("bash")
                            .args([
                                "-c",
                                &format!(
                                    "timeout 5 umount -f '{}' 2>/dev/null; \
                                 timeout 3 rm -rf '{}' 2>/dev/null; true",
                                    mp, mp
                                ),
                            ])
                            .output();
                    }
                    eprintln!("[WATCHDOG] Exiting. Run 'fatx mount --cleanup' if Finder is stuck.");
                    std::process::exit(1);
                }
            }
        });
    }

    // ── DISK REMOVAL WATCHER ──
    // Monitor the underlying block device for removal (USB unplug, eject, etc.).
    // Uses Apple's DiskArbitration framework for near-instant detection (~100ms)
    // with a polling fallback (checks /dev/rdiskN every 2 seconds).
    // When the disk disappears, we trigger a graceful shutdown to avoid the
    // catastrophic stale NFS mount deadlock.
    let _disk_disappeared = {
        let device_str = device_path.display().to_string();
        let bsd_name = disk_watcher::bsd_name_from_device_path(&device_str);

        if let Some(ref name) = bsd_name {
            let watcher = disk_watcher::DiskWatcher::start(name, &device_str);
            let disappeared = Arc::clone(&watcher.disappeared);

            // Spawn a tokio task that polls the disappeared flag and triggers shutdown
            let disk_mp = if cli.mount {
                Some(mp_str.clone())
            } else {
                None
            };
            let disk_vol = Arc::clone(&shutdown_vol);
            let disk_flush_flag = Arc::clone(&shutdown_flush_flag);
            let disk_dirty = Arc::clone(&shutdown_dirty);
            let _disk_dir_cache = Arc::clone(&shutdown_dir_cache);
            let disappeared_for_task = Arc::clone(&disappeared);

            tokio::spawn(async move {
                // Hold onto the watcher so its threads stay alive.
                // When this task exits (via process::exit), Drop sets the stop flag.
                let _watcher = watcher;

                let mut interval = tokio::time::interval(std::time::Duration::from_millis(500));
                loop {
                    interval.tick().await;
                    if disappeared_for_task.load(Ordering::Relaxed) {
                        eprintln!(
                            "\n[DISK REMOVED] Device disconnected! Starting emergency shutdown..."
                        );
                        eprintln!("[DISK REMOVED] Dirty writes will be LOST (device is gone).");

                        // Clear dirty files — can't write to a missing device
                        {
                            let mut dirty = disk_dirty.lock();
                            let lost = dirty.len();
                            dirty.clear();
                            if lost > 0 {
                                eprintln!(
                                    "[DISK REMOVED] Dropped {} dirty file(s) (device unavailable)",
                                    lost
                                );
                            }
                        }
                        disk_flush_flag.store(false, Ordering::Relaxed);

                        // Clear caches
                        // Cache entries invalidated implicitly — quick_cache has no clear()
                        // but the caches are Arc'd so they'll be dropped when the process exits.

                        // Force-unmount to prevent stale mount
                        if let Some(ref mp) = disk_mp {
                            eprintln!("[DISK REMOVED] Force-unmounting {}...", mp);
                            let _ = std::process::Command::new("bash")
                                .args([
                                    "-c",
                                    &format!(
                                        "timeout 5 umount -f '{}' 2>/dev/null; \
                                     timeout 3 rm -rf '{}' 2>/dev/null; true",
                                        mp, mp
                                    ),
                                ])
                                .output();
                            eprintln!("[DISK REMOVED] Unmount complete.");
                        }

                        // Drop the volume lock (release device fd)
                        drop(disk_vol);

                        eprintln!("[DISK REMOVED] Shutdown complete. Exiting.");
                        std::process::exit(0);
                    }
                }
            });

            Some(disappeared)
        } else {
            warn!(
                "Could not determine BSD name from device path: {}",
                device_str
            );
            warn!("Disk removal detection is DISABLED for this session.");
            None
        }
    };

    println!("Press Ctrl+C to stop.");

    // CRITICAL: The shutdown sequence must be:
    //   1. Unmount WHILE the NFS server is still running (so umount can talk to it)
    //   2. Then kill the NFS server
    //   3. Then exit
    //
    // If we kill the server first, umount hangs trying to talk to a dead server,
    // which freezes Finder and can require a reboot.
    //
    // We use a dedicated thread with raw signal handling because tokio's
    // ctrl_c() can't fire when the event loop is blocked by NFS I/O.
    {
        let mp_for_signal = if cli.mount {
            Some(mp_str.clone())
        } else {
            None
        };

        std::thread::spawn(move || {
            let (tx, rx) = std::sync::mpsc::channel();
            ctrlc_channel(tx);
            let _ = rx.recv(); // blocks until SIGINT

            eprintln!("\n[shutdown] Signal received, beginning clean shutdown...");

            // Step 1: Unmount FIRST while the NFS server is still alive.
            // This lets umount cleanly disconnect from the server.
            if let Some(ref mp) = mp_for_signal {
                eprintln!(
                    "[shutdown] Step 1/3: Unmounting {} (server still running)...",
                    mp
                );
                let umount = std::process::Command::new("umount").arg(mp).output();
                match umount {
                    Ok(o) if o.status.success() => {
                        eprintln!("[shutdown] Clean unmount succeeded.");
                    }
                    Ok(o) => {
                        let stderr = String::from_utf8_lossy(&o.stderr);
                        eprintln!(
                            "[shutdown] Clean unmount failed ({}), trying force...",
                            stderr.trim()
                        );
                        let force = std::process::Command::new("umount")
                            .args(["-f", mp])
                            .output();
                        match force {
                            Ok(o) if o.status.success() => {
                                eprintln!("[shutdown] Force unmount succeeded.")
                            }
                            Ok(o) => eprintln!(
                                "[shutdown] Force unmount failed: {}",
                                String::from_utf8_lossy(&o.stderr).trim()
                            ),
                            Err(e) => eprintln!("[shutdown] Force unmount error: {}", e),
                        }
                    }
                    Err(e) => {
                        eprintln!("[shutdown] umount error: {}", e);
                    }
                }

                // Give macOS a moment to finish the unmount
                eprintln!("[shutdown] Step 2/3: Waiting for macOS to release mount...");
                std::thread::sleep(std::time::Duration::from_millis(300));

                // Clean up the mount point directory
                eprintln!("[shutdown] Step 3/3: Cleaning up mount point directory...");
                match std::fs::remove_dir(mp) {
                    Ok(_) => eprintln!("[shutdown] Removed mount point {}.", mp),
                    Err(e) => {
                        eprintln!("[shutdown] Could not remove mount point: {} (non-fatal)", e)
                    }
                }
            } else {
                eprintln!("[shutdown] No mount to clean up (server-only mode).");
            }

            // Step 2: Flush dirty files and FAT if needed
            if shutdown_flush_flag.load(Ordering::Relaxed) {
                // Drain dirty write buffers to disk
                let pending: HashMap<u32, DirtyFileState> = {
                    let mut dirty = shutdown_dirty.lock();
                    std::mem::take(&mut *dirty)
                };
                if !pending.is_empty() {
                    eprintln!(
                        "[shutdown] Flushing {} dirty file(s) to disk...",
                        pending.len()
                    );
                    {
                        let mut vol = shutdown_vol.write();
                        for state in pending.values() {
                            match vol.begin_write_in_place_for_entry(
                                state.parent_cluster,
                                state.first_cluster,
                                state.data.len(),
                            ) {
                                Ok(session) => {
                                    let cluster_size = vol.superblock.cluster_size() as usize;
                                    let mut offset = 0usize;
                                    let mut write_failed = false;
                                    for &cluster in session.clusters() {
                                        let end = (offset + cluster_size).min(state.data.len());
                                        let mut cluster_buf = vec![0u8; cluster_size];
                                        if offset < state.data.len() {
                                            let len = end - offset;
                                            cluster_buf[..len]
                                                .copy_from_slice(&state.data[offset..end]);
                                        }
                                        if let Err(e) = vol.write_cluster(cluster, &cluster_buf) {
                                            eprintln!(
                                                "[shutdown] Failed to write cluster {} for file {}: {}",
                                                cluster, state.first_cluster, e
                                            );
                                            write_failed = true;
                                            break;
                                        }
                                        offset += cluster_size;
                                        if offset >= state.data.len() {
                                            break;
                                        }
                                    }

                                    if write_failed {
                                        if let Err(e) = vol.cancel_write_session(session) {
                                            eprintln!(
                                                "[shutdown] Failed to cancel pending write for cluster {}: {}",
                                                state.first_cluster, e
                                            );
                                        }
                                    } else if let Err(e) = vol.commit_write_session(session) {
                                        eprintln!(
                                            "[shutdown] Failed to commit pending write for cluster {}: {}",
                                            state.first_cluster, e
                                        );
                                    }
                                }
                                Err(fatxlib::error::FatxError::FileNotFound(_)) => eprintln!(
                                    "[shutdown] Dropping dirty buffer for deleted/missing cluster {}",
                                    state.first_cluster
                                ),
                                Err(e) => eprintln!(
                                    "[shutdown] Failed to prepare dirty file {}: {}",
                                    state.first_cluster, e
                                ),
                            }
                        }
                        // Dir cache entries don't need clearing at shutdown
                    }
                }

                eprintln!("[shutdown] Flushing FAT to disk...");
                {
                    let mut vol = shutdown_vol.write();
                    match vol.flush() {
                        Ok(()) => eprintln!("[shutdown] FAT flush complete."),
                        Err(e) => {
                            eprintln!("[shutdown] FAT flush failed: {} (data may be lost!)", e)
                        }
                    }
                }
            }

            // Step 3: Now it's safe to exit — no stale mount left behind
            eprintln!("[shutdown] Shutdown complete. Exiting.");
            std::process::exit(0);
        });
    }

    // Run the NFS server forever (Ctrl+C handled by the thread above)
    if let Err(e) = listener.handle_forever().await {
        error!("NFS server error: {}", e);
    }
}

/// Set up a channel-based Ctrl+C (SIGINT) listener using raw signals.
fn ctrlc_channel(tx: std::sync::mpsc::Sender<()>) {
    unsafe {
        libc::signal(
            libc::SIGINT,
            sigint_handler as *const () as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGTERM,
            sigint_handler as *const () as libc::sighandler_t,
        );
    }
    *SIGINT_TX.lock().unwrap() = Some(tx);
}

static SIGINT_TX: std::sync::Mutex<Option<std::sync::mpsc::Sender<()>>> =
    std::sync::Mutex::new(None);

extern "C" fn sigint_handler(_sig: libc::c_int) {
    if let Ok(guard) = SIGINT_TX.lock() {
        if let Some(tx) = guard.as_ref() {
            let _ = tx.send(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mkimage::{run as run_mkimage, MkimageArgs};
    use fatxlib::FatEntry;
    use std::fs::OpenOptions;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn create_test_nfs() -> (TempDir, PathBuf, FatxNfs) {
        let tmp = TempDir::new().expect("tempdir");
        let image_path = tmp.path().join("mount-test.img");
        run_mkimage(MkimageArgs {
            output: image_path.clone(),
            size: "16M".to_string(),
            format: "fatx".to_string(),
            spc: 32,
            populate: false,
            force: true,
        });

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&image_path)
            .expect("open image");
        let vol = FatxVolume::open(file, 0, 0).expect("open FATX volume");
        (tmp, image_path, FatxNfs::new(vol, false))
    }

    // ── is_macos_metadata tests ──

    #[test]
    fn test_blocks_ds_store() {
        assert!(FatxNfs::is_macos_metadata(".DS_Store"));
    }

    #[test]
    fn test_blocks_spotlight() {
        assert!(FatxNfs::is_macos_metadata(".Spotlight-V100"));
    }

    #[test]
    fn test_blocks_trashes() {
        assert!(FatxNfs::is_macos_metadata(".Trashes"));
    }

    #[test]
    fn test_blocks_fseventsd() {
        assert!(FatxNfs::is_macos_metadata(".fseventsd"));
    }

    #[test]
    fn test_blocks_resource_fork_prefix() {
        assert!(FatxNfs::is_macos_metadata("._anything"));
        assert!(FatxNfs::is_macos_metadata("._Icon\r"));
        assert!(FatxNfs::is_macos_metadata("._"));
    }

    #[test]
    fn test_allows_normal_files() {
        assert!(!FatxNfs::is_macos_metadata("game.bin"));
        assert!(!FatxNfs::is_macos_metadata("Content"));
        assert!(!FatxNfs::is_macos_metadata(".hidden"));
        assert!(!FatxNfs::is_macos_metadata("DS_Store")); // no leading dot
    }

    // ── inode/cluster mapping tests ──

    #[test]
    fn test_root_inode_is_one() {
        // The NFS root inode should be 1 (standard NFS convention)
        assert_eq!(FIRST_CLUSTER, 1);
    }

    // ── check_writable tests ──
    // These require constructing a FatxNfs instance, which needs a real volume.
    // We test the logic indirectly through the NFS integration tests.

    // ── cache invalidation logic ──

    #[test]
    fn test_cache_structures() {
        // Verify that HashMap<u32, Vec<DirectoryEntry>> and HashMap<u32, Vec<u8>>
        // are the correct types by constructing them
        let dir_cache: HashMap<u32, Vec<DirectoryEntry>> = HashMap::new();
        let file_cache: HashMap<u32, Vec<u8>> = HashMap::new();
        assert!(dir_cache.is_empty());
        assert!(file_cache.is_empty());
    }

    #[test]
    fn test_slice_buffered_range_middle() {
        let data = b"abcdefghij";
        let (slice, eof) = slice_buffered_range(data, 3, 4);
        assert_eq!(slice, b"defg");
        assert!(!eof);
    }

    #[test]
    fn test_slice_buffered_range_past_end() {
        let data = b"abcdefghij";
        let (slice, eof) = slice_buffered_range(data, 20, 4);
        assert!(slice.is_empty());
        assert!(eof);
    }

    #[test]
    fn test_slice_buffered_range_from_start_to_end() {
        let data = b"abcdefghij";
        let (slice, eof) = slice_buffered_range(data, 0, 32);
        assert_eq!(slice, data);
        assert!(eof);
    }

    #[test]
    fn test_slice_buffered_range_exact_last_byte() {
        let data = b"abcdefghij";
        let (slice, eof) = slice_buffered_range(data, 9, 1);
        assert_eq!(slice, b"j");
        assert!(eof);
    }

    #[test]
    fn test_identity_flush_survives_ancestor_directory_rename() {
        let (_tmp, _image_path, fs) = create_test_nfs();

        let (dir_cluster, file_cluster) = {
            let mut vol = fs.vol.write();
            vol.create_directory("/Dir").expect("mkdir");
            vol.create_file("/Dir/file.bin", b"old")
                .expect("create file");

            let dir_cluster = vol.resolve_path("/Dir").expect("resolve dir").first_cluster;
            let file_cluster = vol
                .resolve_path("/Dir/file.bin")
                .expect("resolve file")
                .first_cluster;

            vol.rename("/Dir", "RenamedDir").expect("rename dir");
            (dir_cluster, file_cluster)
        };

        {
            let mut parents = fs.inode_parents.write();
            parents.insert(dir_cluster, (FIRST_CLUSTER, "Dir".to_string()));
            parents.insert(file_cluster, (dir_cluster, "file.bin".to_string()));
        }

        let state = DirtyFileState {
            parent_cluster: dir_cluster,
            first_cluster: file_cluster,
            data: b"updated".to_vec(),
        };

        {
            let mut vol = fs.vol.write();
            let session = vol
                .begin_write_in_place_for_entry(
                    state.parent_cluster,
                    state.first_cluster,
                    state.data.len(),
                )
                .expect("begin session");
            let cluster_size = vol.superblock.cluster_size() as usize;
            let mut offset = 0usize;
            for &cluster in session.clusters() {
                let end = (offset + cluster_size).min(state.data.len());
                let mut cluster_buf = vec![0u8; cluster_size];
                if offset < state.data.len() {
                    let len = end - offset;
                    cluster_buf[..len].copy_from_slice(&state.data[offset..end]);
                }
                vol.write_cluster(cluster, &cluster_buf)
                    .expect("write cluster");
                offset += cluster_size;
                if offset >= state.data.len() {
                    break;
                }
            }
            vol.commit_write_session(session).expect("commit session");
            vol.flush().expect("flush");
        }

        let mut vol = fs.vol.write();
        assert!(vol.resolve_path("/Dir/file.bin").is_err());
        assert_eq!(
            vol.read_file_by_path("/RenamedDir/file.bin")
                .expect("read renamed file"),
            b"updated"
        );
    }

    #[test]
    fn test_purge_dirty_subtree_removes_descendants() {
        let (_tmp, _image_path, fs) = create_test_nfs();

        {
            let mut parents = fs.inode_parents.write();
            parents.insert(10, (FIRST_CLUSTER, "Dir".to_string()));
            parents.insert(11, (10, "Nested".to_string()));
            parents.insert(20, (10, "child.bin".to_string()));
            parents.insert(21, (11, "grandchild.bin".to_string()));
            parents.insert(30, (FIRST_CLUSTER, "keep.bin".to_string()));
        }

        {
            let mut dirty = fs.dirty_files.lock();
            dirty.insert(
                20,
                DirtyFileState {
                    parent_cluster: 10,
                    first_cluster: 20,
                    data: vec![1, 2, 3],
                },
            );
            dirty.insert(
                21,
                DirtyFileState {
                    parent_cluster: 11,
                    first_cluster: 21,
                    data: vec![4, 5, 6],
                },
            );
            dirty.insert(
                30,
                DirtyFileState {
                    parent_cluster: FIRST_CLUSTER,
                    first_cluster: 30,
                    data: vec![7, 8, 9],
                },
            );
        }
        fs.file_cache.insert(20, Bytes::from_static(b"child"));
        fs.file_cache.insert(21, Bytes::from_static(b"grandchild"));
        fs.file_cache.insert(30, Bytes::from_static(b"keep"));

        fs.purge_dirty_subtree(10);

        let dirty = fs.dirty_files.lock();
        assert!(!dirty.contains_key(&20));
        assert!(!dirty.contains_key(&21));
        assert!(dirty.contains_key(&30));
        drop(dirty);

        assert!(fs.file_cache.get(&20).is_none());
        assert!(fs.file_cache.get(&21).is_none());
        assert!(fs.file_cache.get(&30).is_some());
    }

    #[test]
    fn test_create_exclusive_rejects_existing_file_without_truncating() {
        let (_tmp, _image_path, fs) = create_test_nfs();

        {
            let mut vol = fs.vol.write();
            vol.create_file("/exists.bin", b"original")
                .expect("create existing file");
        }

        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        let result = rt.block_on(async {
            fs.create_exclusive(ROOT_FILEID, &b"exists.bin".to_vec().into())
                .await
        });

        assert!(matches!(result, Err(nfsstat3::NFS3ERR_EXIST)));

        let mut vol = fs.vol.write();
        assert_eq!(
            vol.read_file_by_path("/exists.bin")
                .expect("read existing file"),
            b"original"
        );
    }

    #[test]
    fn test_write_seeds_from_disk_on_cache_miss() {
        let (_tmp, _image_path, fs) = create_test_nfs();

        let file_id = {
            let mut vol = fs.vol.write();
            vol.create_file("/seed.bin", b"abcdef")
                .expect("create seed file");
            let entry = vol.resolve_path("/seed.bin").expect("resolve seed file");
            fs.inode_parents
                .write()
                .insert(entry.first_cluster, (FIRST_CLUSTER, "seed.bin".to_string()));
            cluster_to_id(entry.first_cluster)
        };

        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        let attr = rt.block_on(async { fs.write(file_id, 0, b"ZZ").await });
        assert!(attr.is_ok(), "write should succeed on cache miss");

        let dirty = fs.dirty_files.lock();
        let state = dirty.get(&id_to_cluster(file_id)).expect("dirty buffer");
        assert_eq!(state.data, b"ZZcdef");
    }

    #[test]
    fn test_write_returns_io_if_seed_read_fails() {
        let (_tmp, _image_path, fs) = create_test_nfs();

        let file_id = {
            let mut vol = fs.vol.write();
            vol.create_file("/broken.bin", b"abcdef")
                .expect("create broken file");
            let entry = vol
                .resolve_path("/broken.bin")
                .expect("resolve broken file");
            vol.write_fat_entry(entry.first_cluster, FatEntry::Free)
                .expect("corrupt chain");
            fs.inode_parents.write().insert(
                entry.first_cluster,
                (FIRST_CLUSTER, "broken.bin".to_string()),
            );
            cluster_to_id(entry.first_cluster)
        };

        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        let result = rt.block_on(async { fs.write(file_id, 0, b"ZZ").await });
        assert!(matches!(result, Err(nfsstat3::NFS3ERR_IO)));
        assert!(
            !fs.dirty_files.lock().contains_key(&id_to_cluster(file_id)),
            "failed seed read must not create a dirty buffer"
        );
    }
}
