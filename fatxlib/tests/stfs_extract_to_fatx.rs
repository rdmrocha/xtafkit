//! Integration test for `extract_to_fatx` — streams STFS package contents
//! into a FATX volume in memory and verifies the resulting tree byte-for-byte.

mod common;

use std::io::Cursor;

use fatxlib::stfs::StfsPackage;
use fatxlib::stfs::block_translator::{BLOCK_SIZE, FIRST_DATA_BLOCK_OFFSET};
use fatxlib::stfs::extract::{ExtractReport, extract_to_fatx};
use fatxlib::stfs::header::MIN_HEADER_BYTES;

/// Build a synthetic STFS package containing:
///   /Media/cover.png  — 4 bytes "ABCD"  (block 1)
///   /default.xex      — 4 bytes "MZRX"  (block 2)
///
/// File-table block is at block 0; total_alloc_blocks = 3.
fn make_two_file_package() -> Vec<u8> {
    let mut buf = vec![0u8; MIN_HEADER_BYTES];
    buf[0..4].copy_from_slice(b"LIVE");
    // Volume descriptor at 0x379
    buf[0x379] = 0x24; // descriptor_size
    buf[0x37B] = 0x01; // read_only_format
    buf[0x37C..0x37E].copy_from_slice(&1u16.to_be_bytes()); // file_table_block_count = 1
    // file_table_block_number = 0 (already zero)
    buf[0x395..0x399].copy_from_slice(&3u32.to_be_bytes()); // total_alloc_blocks = 3

    // Grow to the start of block 0.
    let block_zero = FIRST_DATA_BLOCK_OFFSET as usize;
    buf.resize(block_zero, 0);

    // Block 0: file table
    let mut ft = vec![0u8; BLOCK_SIZE as usize];

    // Entry 0: dir "Media", parent -1 (root), consecutive + dir flags
    let mut e0 = vec![0u8; 0x40];
    e0[..5].copy_from_slice(b"Media");
    e0[0x28] = 0x05 | 0x40 | 0x80; // name_len=5 | consecutive | dir
    e0[0x32..0x34].copy_from_slice(&(-1i16).to_be_bytes()); // parent = root
    ft[..0x40].copy_from_slice(&e0);

    // Entry 1: file "cover.png", parent 0 (Media), block 1, size 4
    let mut e1 = vec![0u8; 0x40];
    e1[..9].copy_from_slice(b"cover.png");
    e1[0x28] = 0x09 | 0x40; // name_len=9 | consecutive
    e1[0x2C] = 1; // used_blocks
    e1[0x2F] = 1; // start_block
    e1[0x32..0x34].copy_from_slice(&0i16.to_be_bytes()); // parent = entry 0
    e1[0x34..0x38].copy_from_slice(&4u32.to_be_bytes()); // size
    ft[0x40..0x80].copy_from_slice(&e1);

    // Entry 2: file "default.xex", parent -1 (root), block 2, size 4
    let mut e2 = vec![0u8; 0x40];
    e2[..11].copy_from_slice(b"default.xex");
    e2[0x28] = 0x0B | 0x40; // name_len=11 | consecutive
    e2[0x2C] = 1;
    e2[0x2F] = 2; // start_block
    e2[0x32..0x34].copy_from_slice(&(-1i16).to_be_bytes()); // parent = root
    e2[0x34..0x38].copy_from_slice(&4u32.to_be_bytes());
    ft[0x80..0xC0].copy_from_slice(&e2);

    buf.extend_from_slice(&ft);

    // Block 1: cover.png payload
    let mut b1 = vec![0u8; BLOCK_SIZE as usize];
    b1[..4].copy_from_slice(b"ABCD");
    buf.extend_from_slice(&b1);

    // Block 2: default.xex payload
    let mut b2 = vec![0u8; BLOCK_SIZE as usize];
    b2[..4].copy_from_slice(b"MZRX");
    buf.extend_from_slice(&b2);

    buf
}

#[test]
fn extract_to_fatx_writes_nested_tree() {
    // Spin up a fresh FATX volume backed by a temp file.
    let (_tmp, mut vol) = common::create_fatx_image(4);

    let raw = make_two_file_package();
    let mut pkg = StfsPackage::open(Cursor::new(raw)).expect("open stfs package");

    let report: ExtractReport =
        extract_to_fatx(&mut pkg, &mut vol, "/Halo Wars", None, None).expect("extract to fatx");

    // Two files extracted. directories counts only STFS package dir entries
    // (just /Media here); the dest_root (/Halo Wars) is NOT counted,
    // matching the unified host-semantics behaviour.
    assert_eq!(report.files, 2, "files");
    assert_eq!(report.directories, 1, "directories");
    assert_eq!(report.bytes, 8, "bytes");

    let cover = vol
        .read_file_by_path("/Halo Wars/Media/cover.png")
        .expect("read cover.png");
    assert_eq!(cover, b"ABCD", "cover.png content");

    let xex = vol
        .read_file_by_path("/Halo Wars/default.xex")
        .expect("read default.xex");
    assert_eq!(xex, b"MZRX", "default.xex content");
}

#[test]
fn extract_to_fatx_honors_cancel_flag() {
    use std::sync::atomic::AtomicBool;

    let (_tmp, mut vol) = common::create_fatx_image(4);

    let raw = make_two_file_package();
    let mut pkg = StfsPackage::open(Cursor::new(raw)).expect("open stfs package");

    // Pre-cancelled flag — should abort before writing any file.
    let cancel = AtomicBool::new(true);
    let err = extract_to_fatx(&mut pkg, &mut vol, "/cancelled", None, Some(&cancel))
        .expect_err("should cancel");
    assert!(
        format!("{}", err).contains("cancelled"),
        "error message should contain 'cancelled', got: {}",
        err
    );
}
