//! Tests for slot-aware folder-name formatting.
//!
//! Format contract:
//!   * Resolvable IDs render as `"<name> [<raw>]"` — raw preserves on-disk
//!     case verbatim (no `0x` prefix, no upper/lower normalization).
//!   * Unresolvable IDs render as `<raw>` unchanged.
//!   * Slot detection follows `Content/<XUID>/<TitleID>/<ContentType>/<file>`.

use fatxlib::display::{folder_slot, format_for_path, FolderSlot};
use fatxlib::{content_types, titles, xuids};

#[test]
fn titles_format_resolved() {
    assert_eq!(titles::format_folder("4D5307E6"), "Halo 3 [4D5307E6]");
}

#[test]
fn titles_format_unresolved_preserves_raw() {
    assert_eq!(titles::format_folder("DEADBEEF"), "DEADBEEF");
}

#[test]
fn titles_format_preserves_lowercase_input() {
    // Important: don't normalize. If on-disk is lowercase, surface that —
    // Xbox 360 stores upper-hex IDs, so lowercase signals a non-360 source.
    assert_eq!(titles::format_folder("4d5307e6"), "Halo 3 [4d5307e6]");
}

#[test]
fn content_types_lookup_known_ids() {
    assert_eq!(content_types::lookup(0x00001000), Some("Xbox 360 Title"));
    assert_eq!(content_types::lookup(0x00005000), Some("Xbox Original Game"));
    assert_eq!(content_types::lookup(0x00080000), Some("Game Demo"));
    assert_eq!(content_types::lookup(0x000D0000), Some("Arcade Title"));
    assert_eq!(content_types::lookup(0x00010000), Some("Profile"));
}

#[test]
fn content_types_format_resolved() {
    assert_eq!(
        content_types::format_folder("00080000"),
        "Game Demo [00080000]"
    );
}

#[test]
fn content_types_format_unresolved() {
    assert_eq!(content_types::format_folder("DEADBEEF"), "DEADBEEF");
}

#[test]
fn xuids_format_shared() {
    assert_eq!(
        xuids::format_folder("0000000000000000"),
        "Shared [0000000000000000]"
    );
}

#[test]
fn xuids_format_personal_xuid_passthrough() {
    assert_eq!(
        xuids::format_folder("E000123456789ABC"),
        "E000123456789ABC"
    );
}

#[test]
fn slot_detection() {
    assert_eq!(folder_slot("/Content"), FolderSlot::Xuid);
    assert_eq!(folder_slot("/Content/0000000000000000"), FolderSlot::TitleId);
    assert_eq!(
        folder_slot("/Content/0000000000000000/4D5307E6"),
        FolderSlot::ContentType
    );
    assert_eq!(
        folder_slot("/Content/0000000000000000/4D5307E6/00001000"),
        FolderSlot::File
    );
    assert_eq!(folder_slot("/"), FolderSlot::File);
    assert_eq!(folder_slot("/Photo"), FolderSlot::File);
}

#[test]
fn slot_detection_is_case_insensitive_on_content_root() {
    // FATX filenames are case-insensitive on lookup; the literal "Content"
    // folder may render with any casing depending on creation order.
    assert_eq!(folder_slot("/content"), FolderSlot::Xuid);
    assert_eq!(folder_slot("/CONTENT"), FolderSlot::Xuid);
}

#[test]
fn format_for_path_dispatches() {
    assert_eq!(
        format_for_path("/Content", "0000000000000000"),
        "Shared [0000000000000000]"
    );
    assert_eq!(
        format_for_path("/Content/0000000000000000", "4D5307E6"),
        "Halo 3 [4D5307E6]"
    );
    assert_eq!(
        format_for_path("/Content/0000000000000000/4D5307E6", "00001000"),
        "Xbox 360 Title [00001000]"
    );
    assert_eq!(
        format_for_path("/Content/0000000000000000/4D5307E6/00001000", "savegame.dat"),
        "savegame.dat"
    );
    assert_eq!(format_for_path("/", "Photo"), "Photo");
}

#[test]
fn format_for_path_unresolvable_id_falls_back_to_raw() {
    assert_eq!(
        format_for_path("/Content/0000000000000000", "DEADBEEF"),
        "DEADBEEF"
    );
    assert_eq!(
        format_for_path("/Content", "E000123456789ABC"),
        "E000123456789ABC"
    );
}
