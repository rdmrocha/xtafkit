//! STFS file table entry parser.
//!
//! Each file table block (4 KiB) holds up to 64 entries × 64 bytes.
//! An entry with `name_length == 0` marks end-of-table within a block.

use crate::error::{FatxError, Result};

pub const FILE_ENTRY_SIZE: usize = 0x40;
pub const ENTRIES_PER_BLOCK: usize = 0x1000 / FILE_ENTRY_SIZE;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StfsEntry {
    pub name: String,
    pub is_directory: bool,
    pub consecutive: bool,
    pub allocated_blocks: u32,
    pub used_blocks: u32,
    pub start_block: u32,
    /// Signed index into the entry list. `-1` (`0xFFFF`) = root directory parent.
    pub parent_index: i16,
    pub size: u64,
    pub update_timestamp: u32,
    pub access_timestamp: u32,
}

impl StfsEntry {
    /// True when this slot is unused (zero name length, no flags).
    pub fn is_empty_slot(name_length_and_flags: u8) -> bool {
        (name_length_and_flags & 0x3F) == 0
    }
}

/// Parse one 64-byte entry. Returns `Ok(None)` for empty-slot entries.
pub fn parse(bytes: &[u8]) -> Result<Option<StfsEntry>> {
    if bytes.len() < FILE_ENTRY_SIZE {
        return Err(FatxError::Other(format!(
            "STFS file entry truncated: got {} bytes, need {}",
            bytes.len(),
            FILE_ENTRY_SIZE,
        )));
    }
    let flags = bytes[0x28];
    if StfsEntry::is_empty_slot(flags) {
        return Ok(None);
    }
    let name_len = (flags & 0x3F) as usize;
    let consecutive = (flags & 0x40) != 0;
    let is_directory = (flags & 0x80) != 0;

    let name_bytes = &bytes[0..name_len];
    // STFS spec says ASCII; tolerate occasional non-printable bytes by
    // replacing with '_' rather than rejecting the whole entry.
    let name: String = name_bytes
        .iter()
        .map(|&b| {
            if (0x20..=0x7E).contains(&b) {
                b as char
            } else {
                '_'
            }
        })
        .collect();

    let allocated_blocks =
        (bytes[0x29] as u32) | ((bytes[0x2A] as u32) << 8) | ((bytes[0x2B] as u32) << 16);
    let used_blocks =
        (bytes[0x2C] as u32) | ((bytes[0x2D] as u32) << 8) | ((bytes[0x2E] as u32) << 16);
    let start_block =
        (bytes[0x2F] as u32) | ((bytes[0x30] as u32) << 8) | ((bytes[0x31] as u32) << 16);
    let parent_index = i16::from_be_bytes([bytes[0x32], bytes[0x33]]);
    let size = u32::from_be_bytes([bytes[0x34], bytes[0x35], bytes[0x36], bytes[0x37]]) as u64;
    let update_timestamp = u32::from_be_bytes([bytes[0x38], bytes[0x39], bytes[0x3A], bytes[0x3B]]);
    let access_timestamp = u32::from_be_bytes([bytes[0x3C], bytes[0x3D], bytes[0x3E], bytes[0x3F]]);

    Ok(Some(StfsEntry {
        name,
        is_directory,
        consecutive,
        allocated_blocks,
        used_blocks,
        start_block,
        parent_index,
        size,
        update_timestamp,
        access_timestamp,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synthetic_entry(
        name: &str,
        is_dir: bool,
        consecutive: bool,
        used_blocks: u32,
        start_block: u32,
        parent_index: i16,
        size: u32,
    ) -> Vec<u8> {
        let mut buf = vec![0u8; FILE_ENTRY_SIZE];
        let bytes = name.as_bytes();
        buf[..bytes.len()].copy_from_slice(bytes);
        let mut flags = bytes.len() as u8 & 0x3F;
        if consecutive {
            flags |= 0x40;
        }
        if is_dir {
            flags |= 0x80;
        }
        buf[0x28] = flags;
        buf[0x29] = (used_blocks & 0xFF) as u8;
        buf[0x2A] = ((used_blocks >> 8) & 0xFF) as u8;
        buf[0x2B] = ((used_blocks >> 16) & 0xFF) as u8;
        buf[0x2C] = (used_blocks & 0xFF) as u8;
        buf[0x2D] = ((used_blocks >> 8) & 0xFF) as u8;
        buf[0x2E] = ((used_blocks >> 16) & 0xFF) as u8;
        buf[0x2F] = (start_block & 0xFF) as u8;
        buf[0x30] = ((start_block >> 8) & 0xFF) as u8;
        buf[0x31] = ((start_block >> 16) & 0xFF) as u8;
        buf[0x32..0x34].copy_from_slice(&parent_index.to_be_bytes());
        buf[0x34..0x38].copy_from_slice(&size.to_be_bytes());
        buf
    }

    #[test]
    fn parses_consecutive_file_entry() {
        let raw = synthetic_entry("default.xex", false, true, 8, 1, -1, 0x12345);
        let entry = parse(&raw).expect("parse").expect("non-empty");
        assert_eq!(entry.name, "default.xex");
        assert!(!entry.is_directory);
        assert!(entry.consecutive);
        assert_eq!(entry.used_blocks, 8);
        assert_eq!(entry.start_block, 1);
        assert_eq!(entry.parent_index, -1);
        assert_eq!(entry.size, 0x12345);
    }

    #[test]
    fn parses_directory_entry() {
        let raw = synthetic_entry("Media", true, false, 0, 0, -1, 0);
        let entry = parse(&raw).expect("parse").expect("non-empty");
        assert!(entry.is_directory);
        assert!(!entry.consecutive);
        assert_eq!(entry.name, "Media");
    }

    #[test]
    fn empty_slot_returns_none() {
        let raw = vec![0u8; FILE_ENTRY_SIZE];
        let entry = parse(&raw).expect("parse");
        assert!(entry.is_none());
    }

    #[test]
    fn non_printable_bytes_in_name_become_underscore() {
        let mut raw = synthetic_entry("abc", false, true, 1, 0, -1, 0);
        raw[1] = 0x00; // corrupt middle byte
        let entry = parse(&raw).expect("parse").expect("non-empty");
        assert_eq!(entry.name, "a_c");
    }

    #[test]
    fn rejects_truncated_input() {
        let raw = vec![0u8; FILE_ENTRY_SIZE - 1];
        let err = parse(&raw).expect_err("should reject");
        assert!(format!("{}", err).contains("truncated"));
    }

    #[test]
    fn entries_per_block_is_64() {
        assert_eq!(ENTRIES_PER_BLOCK, 64);
    }
}
