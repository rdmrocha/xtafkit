//! Title-resolution lookup tests.
//!
//! Data sources merged at build time:
//! - Xbox 360: https://gist.github.com/AdrianCassar/c0d05a14608168259232b3ed8c77f28c
//! - Original Xbox: https://github.com/jeltaqq/Xbox-Original-GameList

use fatxlib::titles::{lookup, Source, ENTRY_COUNT};

#[test]
fn resolves_360_only_title() {
    let info = lookup(0x4D5307E6).expect("Halo 3 should resolve");
    assert_eq!(info.name, "Halo 3");
    assert_eq!(info.source, Source::Xbox360);
}

#[test]
fn resolves_og_only_title() {
    // OG-only ID — not in AdrianCassar gist.
    let info = lookup(0x3FA2CC33).expect("Freestyle Street Soccer should resolve");
    assert_eq!(info.name, "Freestyle Street Soccer");
    assert_eq!(info.source, Source::XboxOriginal);
}

#[test]
fn resolves_overlap_prefers_og_and_marks_both() {
    // ID present in both sources with normalize-equal names; OG-wins policy
    // means the name comes from the OG source and `source` is Both.
    let info = lookup(0x41430007).expect("Aggressive Inline should resolve");
    assert_eq!(info.name, "Aggressive Inline");
    assert_eq!(info.source, Source::Both);
}

#[test]
fn returns_none_for_unknown_id() {
    assert!(lookup(0xDEAD_BEEF).is_none());
}

#[test]
fn merged_map_coverage_floor() {
    // 5133 (360) + 990 (OG) - 613 (overlap) = 5510. Floor at 5400 for slack.
    assert!(
        ENTRY_COUNT >= 5400,
        "merged map looks too small: {ENTRY_COUNT}"
    );
}
