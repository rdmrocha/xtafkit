//! STFS volume descriptor parser.
//!
//! Lives at offset 0x379 inside the STFS header. 36 bytes (size = 0x24).
//! Tells us where the file table lives and whether the package is type 0
//! (read-write) or type 1 (read-only). v1 only supports type 1.

use crate::error::{FatxError, Result};

/// Volume descriptor offset relative to the start of the STFS file.
pub const VOLUME_DESCRIPTOR_OFFSET: usize = 0x379;

/// Expected `descriptor_size` value. Anything else is rejected.
pub const VOLUME_DESCRIPTOR_SIZE: u8 = 0x24;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeDescriptor {
    /// True for read-only ("male") packs. v1 supports only true.
    pub read_only_format: bool,
    pub file_table_block_count: u16,
    pub file_table_block_number: u32,
    pub total_alloc_blocks: u32,
    pub total_unalloc_blocks: u32,
}

/// Parse a volume descriptor from a slice positioned at offset 0x379 inside
/// the STFS header (i.e. pass `&header_bytes[0x379..0x379 + 0x24]`).
pub fn parse(bytes: &[u8]) -> Result<VolumeDescriptor> {
    if bytes.len() < VOLUME_DESCRIPTOR_SIZE as usize {
        return Err(FatxError::Other(format!(
            "STFS volume descriptor truncated: got {} bytes, need {}",
            bytes.len(),
            VOLUME_DESCRIPTOR_SIZE,
        )));
    }
    let descriptor_size = bytes[0];
    if descriptor_size != VOLUME_DESCRIPTOR_SIZE {
        return Err(FatxError::Other(format!(
            "Unsupported STFS volume descriptor size: 0x{:02X} (expected 0x{:02X})",
            descriptor_size, VOLUME_DESCRIPTOR_SIZE,
        )));
    }
    let block_separation = bytes[2];
    let read_only_format = (block_separation & 0x01) != 0;
    let file_table_block_count = u16::from_be_bytes([bytes[3], bytes[4]]);
    // file_table_block_number is stored LITTLE-endian in a BE-dominated header
    let file_table_block_number =
        (bytes[5] as u32) | ((bytes[6] as u32) << 8) | ((bytes[7] as u32) << 16);
    let total_alloc_blocks =
        u32::from_be_bytes([bytes[0x1C], bytes[0x1D], bytes[0x1E], bytes[0x1F]]);
    let total_unalloc_blocks =
        u32::from_be_bytes([bytes[0x20], bytes[0x21], bytes[0x22], bytes[0x23]]);
    Ok(VolumeDescriptor {
        read_only_format,
        file_table_block_count,
        file_table_block_number,
        total_alloc_blocks,
        total_unalloc_blocks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synthetic_descriptor(
        size: u8,
        block_separation: u8,
        file_table_block_count: u16,
        file_table_block_number: u32,
        total_alloc: u32,
        total_unalloc: u32,
    ) -> Vec<u8> {
        let mut buf = vec![0u8; 0x24];
        buf[0] = size;
        buf[2] = block_separation;
        buf[3..5].copy_from_slice(&file_table_block_count.to_be_bytes());
        buf[5] = (file_table_block_number & 0xFF) as u8;
        buf[6] = ((file_table_block_number >> 8) & 0xFF) as u8;
        buf[7] = ((file_table_block_number >> 16) & 0xFF) as u8;
        buf[0x1C..0x20].copy_from_slice(&total_alloc.to_be_bytes());
        buf[0x20..0x24].copy_from_slice(&total_unalloc.to_be_bytes());
        buf
    }

    #[test]
    fn parses_type_1_descriptor() {
        let bytes = synthetic_descriptor(0x24, 0x01, 256, 0, 12_548, 0);
        let vd = parse(&bytes).expect("parse");
        assert!(vd.read_only_format);
        assert_eq!(vd.file_table_block_count, 256);
        assert_eq!(vd.file_table_block_number, 0);
        assert_eq!(vd.total_alloc_blocks, 12_548);
        assert_eq!(vd.total_unalloc_blocks, 0);
    }

    #[test]
    fn detects_type_0_via_block_separation_bit_0_clear() {
        let bytes = synthetic_descriptor(0x24, 0x00, 1, 0, 1, 0);
        let vd = parse(&bytes).expect("parse");
        assert!(!vd.read_only_format);
    }

    #[test]
    fn file_table_block_number_uses_little_endian_within_be_stream() {
        // Stored bytes 0x010203 (LE) → numeric value 0x00030201
        let bytes = synthetic_descriptor(0x24, 0x01, 1, 0x00030201, 1, 0);
        let vd = parse(&bytes).expect("parse");
        assert_eq!(vd.file_table_block_number, 0x00030201);
    }

    #[test]
    fn rejects_wrong_descriptor_size() {
        let bytes = synthetic_descriptor(0x23, 0x01, 1, 0, 1, 0);
        let err = parse(&bytes).expect_err("should reject");
        assert!(format!("{}", err).contains("Unsupported STFS volume descriptor"));
    }

    #[test]
    fn rejects_truncated_input() {
        let bytes = vec![0u8; 0x20];
        let err = parse(&bytes).expect_err("should reject");
        assert!(format!("{}", err).contains("truncated"));
    }
}
