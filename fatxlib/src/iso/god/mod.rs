//! ISO → Games-on-Demand conversion pipeline.
//!
//! Vendored from [QAston/iso2god-rs `xdvdfx` branch](https://github.com/QAston/iso2god-rs/tree/xdvdfx)
//! (parent: [iliazeus/iso2god-rs](https://github.com/iliazeus/iso2god-rs);
//! both MIT-licensed). Local deviations from upstream:
//!
//! - `anyhow::Error` → [`crate::error::FatxError`] so errors flow through
//!   the same channel as the rest of fatxlib.
//! - Upstream's `src/executable/` lives at [`crate::executable`] now and is
//!   shared with the XDVDFS image reader.
//! - The original `src/game_list/` (4.9 KLOC of compiled-in title catalog) is
//!   dropped; fatxlib already has a richer catalog via [`crate::titles`].
//! - The upstream binary (`src/bin/iso2god.rs`) lives elsewhere — fatxlib only
//!   provides the library surface; the CLI/TUI wraps it in `xtafkit`.
//!
//! See `NOTICE` at the repo root for the full attribution.

use std::io::{Read, Seek, SeekFrom, Write};

use crate::error::{FatxError, Result};

pub const SOURCE_BUFFER_SIZE: usize = 1 << 20;

mod convert;
pub use convert::{ConvertOptions, ConvertReport, TrimMode, convert_iso, convert_iso_to_fatx};

mod con_header;
mod core;
pub use con_header::*;

mod file_layout;
pub use file_layout::*;

mod gdf_sector;
pub use gdf_sector::*;

mod hash_list;
pub use hash_list::*;

mod prepare;
mod sink_fatx;
mod sink_host;

/// Single hot-path SHA-1 entry point used by [`HashList`] and
/// [`ConHeaderBuilder`]. With the `openssl-hash` feature (default on)
/// this routes to `openssl::sha::sha1`, which uses ARMv8 SHA on Apple
/// Silicon and SHA-NI on x86. Without the feature it falls back to the
/// portable-Rust `sha1` crate.
#[inline]
pub(crate) fn sha1_digest(data: &[u8]) -> [u8; 20] {
    #[cfg(feature = "openssl-hash")]
    {
        openssl::sha::sha1(data)
    }
    #[cfg(not(feature = "openssl-hash"))]
    {
        use sha1::{Digest, Sha1};
        Sha1::digest(data).into()
    }
}

pub const BLOCKS_PER_PART: u64 = 0xa1c4;
pub const BLOCKS_PER_SUBPART: u64 = 0xcc;
pub const BLOCK_SIZE: u64 = 0x1000;
pub const SUBPARTS_PER_PART: u32 = 0xcb;
pub const SUBPART_SIZE: u64 = BLOCK_SIZE * BLOCKS_PER_SUBPART;

pub fn write_part<R: Read + Seek, W: Write + Seek>(
    mut data_volume: R,
    part_index: u64,
    remaining_bytes: u64,
    mut part_file: W,
) -> Result<()> {
    data_volume
        .seek_relative((part_index * BLOCKS_PER_PART * BLOCK_SIZE) as i64)
        .map_err(FatxError::Io)?;

    let mut master_hash_list = HashList::new();

    let master_hash_list_position = part_file.stream_position().map_err(FatxError::Io)?;
    master_hash_list.write(&mut part_file)?;

    // Pre-allocated subpart buffer — avoids `take + read_to_end`'s repeated
    // grow/check ceremony and the Vec-append work that came with it. We read
    // straight into a fixed-size buffer and slice off the actual length.
    let mut subpart_buf = vec![0u8; SUBPART_SIZE as usize];
    let mut bytes_left = remaining_bytes;

    for _subpart_index in 0..SUBPARTS_PER_PART {
        if bytes_left == 0 {
            break;
        }
        // Fill subpart_buf one read at a time. The last subpart may be
        // short — that's fine, we slice with `got` below.
        let want = (subpart_buf.len() as u64).min(bytes_left) as usize;
        let mut got = 0usize;
        while got < want {
            let n = data_volume
                .read(&mut subpart_buf[got..want])
                .map_err(FatxError::Io)?;
            if n == 0 {
                break;
            }
            got += n;
        }
        if got == 0 {
            break;
        }
        let subpart = &subpart_buf[..got];

        let mut sub_hash_list = HashList::new();

        for block in subpart.chunks(BLOCK_SIZE as usize) {
            sub_hash_list.add_block_hash(block);
        }

        sub_hash_list.write(&mut part_file)?;
        master_hash_list.add_block_hash(sub_hash_list.bytes());

        // Write the subpart we already buffered. An earlier shape
        // seeked back and re-read via `io::copy` (a `reflink` hint for
        // CoW filesystems), but APFS doesn't honor reflink on partial-
        // file writes — the re-read just doubled I/O without benefit.
        part_file.write_all(subpart).map_err(FatxError::Io)?;
        bytes_left -= got as u64;

        if got < want {
            break;
        }
    }

    part_file
        .seek(SeekFrom::Start(master_hash_list_position))
        .map_err(FatxError::Io)?;
    master_hash_list.write(&mut part_file)?;

    Ok(())
}
