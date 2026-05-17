//! STFS block-index → byte-offset translator (type 1, read-only format).
//!
//! Hash blocks are interleaved between data block groups:
//!   - Every 0xAA   (170)   data blocks → one L0 hash block after the group
//!   - Every 0x70E4 (28900) data blocks → one L1 hash block after the group of L0 groups
//!   - Every 0x4AF768       data blocks → one L2 hash block (rarely reached)
//!
//! References:
//!   - Free60: https://free60.org/System-Software/Formats/STFS/
//!   - py360 STFSPackage._getRealBlockNum
//!   - Velocity StfsPackage::GetRealAddress

/// First data block byte offset inside an STFS package (header is 0xB000 +
/// the first 0x1000 reserved).
pub const FIRST_DATA_BLOCK_OFFSET: u64 = 0xC000;

/// Block size in bytes.
pub const BLOCK_SIZE: u64 = 0x1000;

/// Data blocks per L0 hash group.
pub const BLOCKS_PER_L0: u32 = 0xAA;

/// Data blocks per L1 hash group (`BLOCKS_PER_L0 * BLOCKS_PER_L0`).
pub const BLOCKS_PER_L1: u32 = 0x70E4;

/// Data blocks per L2 hash group.
pub const BLOCKS_PER_L2: u32 = 0x4AF768;

/// Translate a logical block index into a byte offset.
///
/// Accounts for L0, L1, and L2 hash blocks interleaved between data block
/// groups in the type-1 (read-only / "male pack") layout.
pub fn block_to_byte_offset(block_index: u32) -> u64 {
    let mut adjusted = block_index as u64;
    if block_index >= BLOCKS_PER_L0 {
        adjusted += (block_index / BLOCKS_PER_L0) as u64;
    }
    if block_index >= BLOCKS_PER_L1 {
        adjusted += (block_index / BLOCKS_PER_L1) as u64;
    }
    if block_index >= BLOCKS_PER_L2 {
        adjusted += (block_index / BLOCKS_PER_L2) as u64;
    }
    FIRST_DATA_BLOCK_OFFSET + adjusted * BLOCK_SIZE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_zero_lands_at_0xc000() {
        assert_eq!(block_to_byte_offset(0), 0xC000);
    }

    #[test]
    fn last_block_of_first_l0_group() {
        // Block 0xA9 is the 170th block — no hash blocks before it.
        // Offset = 0xC000 + 0xA9 * 0x1000 = 0xB5000
        assert_eq!(block_to_byte_offset(0xA9), 0xB5000);
    }

    #[test]
    fn first_block_of_second_l0_group_skips_one_l0_hash() {
        // Block 0xAA: one L0 hash block sits between it and block 0xA9.
        // Offset = 0xC000 + (0xAA + 1) * 0x1000 = 0xB7000
        assert_eq!(block_to_byte_offset(0xAA), 0xB7000);
    }

    #[test]
    fn second_block_of_second_l0_group() {
        // Block 0xAB: same L0 hash skipped as block 0xAA.
        // Offset = 0xC000 + (0xAB + 1) * 0x1000 = 0xB8000
        assert_eq!(block_to_byte_offset(0xAB), 0xB8000);
    }

    #[test]
    fn last_block_before_first_l1_hash() {
        // Block 0x70E3: 169 complete L0 groups passed (0x70E3 / 0xAA = 0xA9).
        // Offset = 0xC000 + (0x70E3 + 0xA9) * 0x1000 = 0xC000 + 0x718C000 = 0x7198000
        assert_eq!(block_to_byte_offset(0x70E3), 0x7198000);
    }

    #[test]
    fn first_block_after_l1_hash_skips_l0_and_l1() {
        // Block 0x70E4: one L1 hash + one L0 hash inserted since 0x70E3.
        // Offset = 0xC000 + (0x70E4 + 0xAA + 1) * 0x1000
        //        = 0xC000 + 0x718F * 0x1000
        //        = 0x719B000
        assert_eq!(block_to_byte_offset(0x70E4), 0x719B000);
    }

    #[test]
    fn block_translator_is_strictly_monotonic_across_boundaries() {
        // Sanity: offsets must strictly increase as block index increases.
        let probes: [u32; 9] = [0, 1, 0xA9, 0xAA, 0xAB, 0x70E3, 0x70E4, 0x70E5, 100_000];
        let mut last = 0u64;
        for n in probes {
            let off = block_to_byte_offset(n);
            assert!(
                off > last,
                "non-monotonic at block 0x{:X}: 0x{:X} <= 0x{:X}",
                n,
                off,
                last
            );
            last = off;
        }
    }
}
