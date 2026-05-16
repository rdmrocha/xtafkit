//! STFS header parser tests.
//!
//! Layout (from <https://free60.org/System-Software/Formats/STFS/>):
//!   * 0x000-0x003  Magic: "CON ", "LIVE", or "PIRS"
//!   * 0x360-0x363  Title ID (u32 big-endian)
//!   * 0x411-0x690  Display name, 18 locales × 0x80 bytes UTF-16BE
//!   * 0x1691-0x1710 Title name, 0x80 bytes UTF-8

use fatxlib::stfs::{parse_header, StfsHeader};

const HEADER_SIZE: usize = 0x1800;

/// Build a synthetic STFS header for testing.
fn make_header(
    magic: &[u8; 4],
    title_id: u32,
    title_name_utf8: &str,
    display_name_utf16be_loc0: &str,
) -> Vec<u8> {
    let mut buf = vec![0u8; HEADER_SIZE];
    buf[0..4].copy_from_slice(magic);
    buf[0x360..0x364].copy_from_slice(&title_id.to_be_bytes());

    // Display Name at 0x411, locale 0, UTF-16BE, up to 0x80 bytes (0x40 chars).
    for (i, ch) in display_name_utf16be_loc0.encode_utf16().take(0x40).enumerate() {
        let be = ch.to_be_bytes();
        buf[0x411 + i * 2] = be[0];
        buf[0x411 + i * 2 + 1] = be[1];
    }

    // Title Name at 0x1691, UTF-8, up to 0x80 bytes.
    let title_bytes = title_name_utf8.as_bytes();
    let take = title_bytes.len().min(0x80);
    buf[0x1691..0x1691 + take].copy_from_slice(&title_bytes[..take]);

    buf
}

#[test]
fn parses_con_header() {
    let bytes = make_header(b"CON ", 0x4D5307E6, "Halo 3", "Halo 3 Multiplayer Map Pack");
    let h: StfsHeader = parse_header(&bytes).expect("CON header should parse");
    assert_eq!(h.magic, *b"CON ");
    assert_eq!(h.title_id, 0x4D5307E6);
    assert_eq!(h.title_name, "Halo 3");
    assert_eq!(h.display_name, "Halo 3 Multiplayer Map Pack");
}

#[test]
fn parses_live_header() {
    let bytes = make_header(b"LIVE", 0x4D5307D5, "Gears of War", "");
    let h = parse_header(&bytes).expect("LIVE header should parse");
    assert_eq!(h.magic, *b"LIVE");
    assert_eq!(h.title_id, 0x4D5307D5);
}

#[test]
fn parses_pirs_header() {
    let bytes = make_header(b"PIRS", 0xFFFE07D1, "Xbox 360 Dashboard", "");
    let h = parse_header(&bytes).expect("PIRS header should parse");
    assert_eq!(h.magic, *b"PIRS");
    assert_eq!(h.title_id, 0xFFFE07D1);
}

#[test]
fn rejects_bad_magic() {
    let mut bytes = make_header(b"CON ", 0x12345678, "x", "x");
    bytes[0..4].copy_from_slice(b"XXXX");
    assert!(parse_header(&bytes).is_none());
}

#[test]
fn rejects_short_input() {
    assert!(parse_header(&[0u8; 100]).is_none());
    assert!(parse_header(&[]).is_none());
}

#[test]
fn best_name_prefers_title_name() {
    let bytes = make_header(b"CON ", 0x4D530004, "Halo", "Halo: Combat Evolved (some package)");
    let h = parse_header(&bytes).unwrap();
    assert_eq!(h.best_name(), "Halo");
}

#[test]
fn best_name_falls_back_to_display_name_when_title_empty() {
    let bytes = make_header(b"CON ", 0x12345678, "", "Fallback Display");
    let h = parse_header(&bytes).unwrap();
    assert_eq!(h.title_name, "");
    assert_eq!(h.display_name, "Fallback Display");
    assert_eq!(h.best_name(), "Fallback Display");
}

#[test]
fn handles_utf8_in_title_name() {
    let bytes = make_header(b"CON ", 0x12345678, "OTOGI -百鬼討伐絵巻-", "");
    let h = parse_header(&bytes).unwrap();
    assert_eq!(h.title_name, "OTOGI -百鬼討伐絵巻-");
}

#[test]
fn strips_trailing_nulls_from_strings() {
    // The on-disk fields are null-padded; the parser should trim trailing NULs.
    let bytes = make_header(b"CON ", 0x12345678, "Halo 3", "Halo 3");
    let h = parse_header(&bytes).unwrap();
    assert!(!h.title_name.contains('\0'));
    assert!(!h.display_name.contains('\0'));
}
