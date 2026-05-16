//! Xbox 360 STFS content-type folder resolution.
//!
//! Sourced from <https://free60.org/System-Software/Formats/STFS/>. These are
//! the values used in `Content/<XUID>/<TitleID>/<ContentType>/` folder names.

/// Free60 STFS content type table.
const CONTENT_TYPES: &[(u32, &str)] = &[
    (0x00000001, "Saved Game"),
    (0x00000002, "Marketplace Content"),
    (0x00000003, "Publisher"),
    (0x00001000, "Xbox 360 Title"),
    (0x00002000, "IPTV Pause Buffer"),
    (0x00004000, "Installed Game"),
    (0x00005000, "Xbox Original Game"),
    (0x00007000, "Game on Demand"),
    (0x00009000, "Avatar Item"),
    (0x00010000, "Profile"),
    (0x00020000, "Gamer Picture"),
    (0x00030000, "Theme"),
    (0x00040000, "Cache File"),
    (0x00050000, "Storage Download"),
    (0x00060000, "Xbox Saved Game"),
    (0x00070000, "Xbox Download"),
    (0x00080000, "Game Demo"),
    (0x00090000, "Video"),
    (0x000A0000, "Game Title"),
    (0x000B0000, "Installer"),
    (0x000C0000, "Game Trailer"),
    (0x000D0000, "Arcade Title"),
    (0x000E0000, "XNA"),
    (0x000F0000, "License Store"),
    (0x00100000, "Movie"),
    (0x00200000, "TV"),
    (0x00300000, "Music Video"),
    (0x00400000, "Game Video"),
    (0x00500000, "Podcast Video"),
    (0x00600000, "Viral Video"),
    (0x02000000, "Community Game"),
];

/// Resolve a content-type ID to its human label. Returns `None` for IDs not
/// in the free60 table.
pub fn lookup(id: u32) -> Option<&'static str> {
    CONTENT_TYPES
        .iter()
        .find(|(k, _)| *k == id)
        .map(|(_, name)| *name)
}

/// Render a raw on-disk content-type folder name (e.g. `"00080000"`) as
/// `"<name> [<raw>]"` if known, otherwise just `<raw>` unchanged. Raw case is
/// preserved verbatim.
pub fn format_folder(raw: &str) -> String {
    let resolved = u32::from_str_radix(raw, 16).ok().and_then(lookup);
    crate::display::format_with_raw(raw, resolved)
}
