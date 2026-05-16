//! Reader for Xbox XDVDFS (XISO) disc images, wrapping the `xdvdfs` crate
//! with a synchronous façade.
//!
//! `xdvdfs` is normally async via `maybe-async`; we depend on it with the
//! `sync` feature which compiles every `#[maybe_async]` function into its
//! synchronous form. As a result, no async runtime is needed in our
//! dependency tree.
//!
//! ## Smart offset detection
//!
//! Real Xbox 360 disc dumps have a video partition before the XDVDFS data
//! partition, so the volume descriptor isn't at byte 0x10000 of the file —
//! it's at one of four known offsets depending on the disc generation:
//!
//! | Layout | Pre-partition offset |
//! |---|---|
//! | Raw / trimmed XISO | 0 |
//! | XGD1 | 0x18300000 (≈ 387 MiB) |
//! | XGD2 | 0xFD90000  (≈ 254 MiB) |
//! | XGD3 | 0x2080000  (≈  32 MiB) |
//!
//! [`XisoImage::open`] tries each of these in order and keeps whichever one
//! produces a valid volume descriptor. Reads through this reader are
//! offset-corrected transparently — you never see the pre-partition bytes.
//!
//! ## API shape
//!
//! ```ignore
//! let file = std::fs::File::open("game.iso")?;
//! let mut img = XisoImage::open(file)?;
//! eprintln!("detected pre-partition offset: 0x{:X}", img.partition_offset());
//! for entry in img.walk_files()? {
//!     // entry.path is image-relative
//!     // entry.size is the file length in bytes
//!     // entry.offset is the byte offset within the data partition
//!     //              (NOT including the pre-partition padding)
//! }
//! let mut out = std::io::sink();
//! img.read_into(&entry, &mut out, None, None)?;
//! ```

use std::io::{Read, Seek, Write};

use xdvdfs::blockdev::BlockDeviceRead;
use xdvdfs::layout::{DirectoryEntryNode, DirectoryEntryTable, VolumeDescriptor};
use xdvdfs::read;

use crate::error::{FatxError, Result};

/// XDVDFS sector size — every offset/length in the on-disk format is
/// expressed in sectors of this size.
pub const SECTOR_SIZE: u64 = xdvdfs::layout::SECTOR_SIZE as u64;

/// Default chunk size for [`XisoImage::read_into`] — 1 MiB.
pub const DEFAULT_CHUNK: usize = 1 << 20;

/// One row of the XGD layout table: a human-readable name and the byte
/// offset where the data partition starts on that layout.
#[derive(Debug, Clone, Copy)]
pub struct XgdLayout {
    pub name: &'static str,
    pub offset: u64,
}

/// XGD layouts probed by [`XisoImage::open`], in the order they're tried.
/// This is the **single source of truth** for both the probing logic and
/// any human-facing display — hex literals here, no magic decimals
/// anywhere else.
pub const LAYOUTS: &[XgdLayout] = &[
    XgdLayout {
        name: "raw / trimmed XISO",
        offset: 0x00000000,
    }, // no pre-partition
    XgdLayout {
        name: "XGD1",
        offset: 0x18300000,
    }, // ≈ 387 MiB
    XgdLayout {
        name: "XGD2",
        offset: 0x0FD90000,
    }, // ≈ 254 MiB
    XgdLayout {
        name: "XGD3",
        offset: 0x02080000,
    }, // ≈  32 MiB
];

/// A file in an XDVDFS image. `path` is image-relative with forward slashes;
/// `size` is the file length; `offset` is the byte offset WITHIN the data
/// partition (i.e., it does NOT include the pre-partition padding —
/// [`XisoImage::read_into`] applies the offset automatically).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XisoFile {
    pub path: String,
    pub size: u64,
    pub offset: u64,
}

/// An opened XDVDFS image. Tracks the detected pre-partition offset so reads
/// are transparently shifted into the data partition.
pub struct XisoImage<R: Read + Seek + Send + Sync> {
    source: R,
    volume: VolumeDescriptor,
    partition_offset: u64,
}

impl<R: Read + Seek + Send + Sync> XisoImage<R> {
    /// Open an XDVDFS image. Probes each entry in [`LAYOUTS`] until one
    /// yields a valid volume descriptor; that offset is kept and all
    /// subsequent reads are translated through it.
    pub fn open(mut source: R) -> Result<Self> {
        for layout in LAYOUTS {
            let mut shifted = ShiftedSource {
                inner: &mut source,
                offset: layout.offset,
            };
            if let Ok(volume) = read::read_volume(&mut shifted) {
                return Ok(Self {
                    source,
                    volume,
                    partition_offset: layout.offset,
                });
            }
        }
        Err(FatxError::Other(format!(
            "xdvdfs: no valid volume descriptor at any known partition offset (tried {} layouts)",
            LAYOUTS.len()
        )))
    }

    /// Pre-data-partition byte offset detected at open time. `0` for raw
    /// XISO; one of the XGD values otherwise.
    pub fn partition_offset(&self) -> u64 {
        self.partition_offset
    }

    /// The [`XgdLayout`] row matching this image's detected offset.
    /// Always returns `Some` because [`open`] only succeeds on an offset
    /// drawn from [`LAYOUTS`].
    pub fn layout(&self) -> Option<&'static XgdLayout> {
        LAYOUTS.iter().find(|l| l.offset == self.partition_offset)
    }

    /// Parse the embedded `Default.xex` (Xbox 360) or `default.xbe`
    /// (Original Xbox) and return the title's execution info — TitleID,
    /// MediaID, version, content type, etc. Returns `None` if the image
    /// has neither executable.
    ///
    /// Useful for resolving a human-readable game title via
    /// [`crate::titles::lookup`] before extracting, so on-drive folder
    /// names track the game rather than the local filename.
    pub fn title_info(&mut self) -> Result<Option<crate::executable::TitleInfo>> {
        let mut shifted = ShiftedSource {
            inner: &mut self.source,
            offset: self.partition_offset,
        };
        match crate::executable::TitleInfo::from_image(&mut shifted, self.volume) {
            Ok(info) => Ok(Some(info)),
            Err(crate::error::FatxError::Other(msg)) if msg.contains("no executable found") => {
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }

    /// Walk the entire directory tree, returning every file (not directories)
    /// as a flat list with image-relative paths and data-partition-relative
    /// byte offsets.
    pub fn walk_files(&mut self) -> Result<Vec<XisoFile>> {
        let mut shifted = ShiftedSource {
            inner: &mut self.source,
            offset: self.partition_offset,
        };
        let mut out = Vec::new();
        let root = self.volume.root_table;
        walk_recursive(&mut shifted, &root, String::new(), &mut out)?;
        Ok(out)
    }

    /// Stream a file's bytes into `dest`. Reads in chunks of `chunk_size`
    /// (defaults to [`DEFAULT_CHUNK`]); invokes `progress(read, total)` after
    /// each chunk if provided. Returns total bytes written.
    pub fn read_into<W: Write>(
        &mut self,
        file: &XisoFile,
        dest: &mut W,
        chunk_size: Option<usize>,
        mut progress: Option<&mut dyn FnMut(u64, u64)>,
    ) -> Result<u64> {
        let chunk = chunk_size.unwrap_or(DEFAULT_CHUNK).max(1);
        let mut buf = vec![0u8; chunk];
        let mut written: u64 = 0;
        let mut offset = file.offset;
        let total = file.size;

        let mut shifted = ShiftedSource {
            inner: &mut self.source,
            offset: self.partition_offset,
        };
        while written < total {
            let want = ((total - written) as usize).min(buf.len());
            BlockDeviceRead::<std::io::Error>::read(&mut shifted, offset, &mut buf[..want])
                .map_err(FatxError::Io)?;
            dest.write_all(&buf[..want]).map_err(FatxError::Io)?;
            offset += want as u64;
            written += want as u64;
            if let Some(cb) = progress.as_deref_mut() {
                cb(written, total);
            }
        }
        Ok(written)
    }

    /// Read `buf.len()` bytes from a data-partition-relative `offset`.
    /// Used by [`XisoFileReader`] so its [`Read`] impl can pull bytes one
    /// chunk at a time without holding an internal `&mut R`.
    pub fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let mut shifted = ShiftedSource {
            inner: &mut self.source,
            offset: self.partition_offset,
        };
        BlockDeviceRead::<std::io::Error>::read(&mut shifted, offset, buf).map_err(FatxError::Io)
    }

    /// Borrow this image as a [`Read`] adapter scoped to a single file.
    /// Returned reader is a cursor into `file`'s byte range; reading past EOF
    /// returns `Ok(0)`. Useful for piping into APIs that consume a `Read`
    /// (e.g. [`crate::volume::FatxVolume::create_file_from_reader`]).
    pub fn file_reader<'a>(&'a mut self, file: &XisoFile) -> XisoFileReader<'a, R> {
        XisoFileReader {
            image: self,
            file_offset: file.offset,
            bytes_remaining: file.size,
        }
    }
}

/// `Read`-compatible cursor over a single [`XisoFile`].
///
/// Each call to [`Read::read`] pulls a chunk straight from the underlying
/// image source through [`XisoImage::read_at`]; nothing is buffered above
/// the kernel layer. EOF (`Ok(0)`) is reached after the file's declared
/// `size` bytes have been served.
pub struct XisoFileReader<'a, R: Read + Seek + Send + Sync> {
    image: &'a mut XisoImage<R>,
    file_offset: u64,
    bytes_remaining: u64,
}

impl<R: Read + Seek + Send + Sync> Read for XisoFileReader<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.bytes_remaining == 0 || buf.is_empty() {
            return Ok(0);
        }
        let want = (buf.len() as u64).min(self.bytes_remaining) as usize;
        self.image
            .read_at(self.file_offset, &mut buf[..want])
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        self.file_offset += want as u64;
        self.bytes_remaining -= want as u64;
        Ok(want)
    }
}

/// Thin block-device adapter that adds a constant offset to every read.
/// Lets us reuse `xdvdfs`'s reader against an arbitrary pre-partition
/// padding without re-implementing the format.
struct ShiftedSource<'a, R: Read + Seek + Send + Sync> {
    inner: &'a mut R,
    offset: u64,
}

impl<R: Read + Seek + Send + Sync> BlockDeviceRead<std::io::Error> for ShiftedSource<'_, R> {
    fn read(&mut self, offset: u64, buffer: &mut [u8]) -> std::io::Result<()> {
        BlockDeviceRead::<std::io::Error>::read(self.inner, offset + self.offset, buffer)
    }
}

// `xdvdfs::executable::TitleInfo::from_image` requires `R: BlockDeviceRead + Seek`.
// We pass the inner Seek through, shifting `Start` positions into the data
// partition; `Current` / `End` are forwarded unchanged.
impl<R: Read + Seek + Send + Sync> Seek for ShiftedSource<'_, R> {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        let adjusted = match pos {
            std::io::SeekFrom::Start(s) => std::io::SeekFrom::Start(s + self.offset),
            other => other,
        };
        let abs = self.inner.seek(adjusted)?;
        Ok(abs.saturating_sub(self.offset))
    }
}

/// Recurse through the directory tree. `prefix` is the parent directory path
/// (empty at the root); each entry is prefixed with it to form the full path.
fn walk_recursive<D>(
    dev: &mut D,
    table: &DirectoryEntryTable,
    prefix: String,
    out: &mut Vec<XisoFile>,
) -> Result<()>
where
    D: BlockDeviceRead<std::io::Error>,
{
    let entries: Vec<DirectoryEntryNode> = table
        .walk_dirent_tree(dev)
        .map_err(|e| FatxError::Other(format!("xdvdfs: walk failed ({e:?})")))?;
    for entry in entries {
        let name = entry
            .name_str::<std::io::Error>()
            .map_err(|e| FatxError::Other(format!("xdvdfs: bad filename ({e:?})")))?;
        let path = if prefix.is_empty() {
            name.to_string()
        } else {
            format!("{prefix}/{name}")
        };
        if entry.node.dirent.is_directory() {
            if let Some(sub) = entry.node.dirent.dirent_table() {
                walk_recursive(dev, &sub, path, out)?;
            }
        } else {
            let size = entry.node.dirent.data.size as u64;
            let offset = entry
                .node
                .dirent
                .data
                .offset::<std::io::Error>(0)
                .map_err(|e| FatxError::Other(format!("xdvdfs: bad offset ({e:?})")))?;
            out.push(XisoFile { path, size, offset });
        }
    }
    Ok(())
}
