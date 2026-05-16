//! XUID-folder resolution.
//!
//! On an Xbox 360 FATX volume, `Content/<XUID>/` separates per-profile content
//! from the system-wide bucket at `0000000000000000`. Per
//! <https://free60.org/System-Software/Systems/FATX/> and community wikis,
//! the all-zeros XUID is the "public / shared content directory" — anything
//! downloaded from the Marketplace or installed system-wide lives here.
//!
//! Personal XUIDs map to gamertags, but resolving those requires reading the
//! profile's `Account` blob; out of scope for this static lookup.

/// Resolve a raw 16-hex XUID folder name to a human label. Currently only the
/// all-zeros shared bucket is named.
pub fn lookup(raw: &str) -> Option<&'static str> {
    if raw == "0000000000000000" {
        Some("Shared")
    } else {
        None
    }
}

/// Render a raw XUID folder name as `"<name> [<raw>]"` if known, otherwise
/// just `<raw>` unchanged. Raw case is preserved verbatim.
pub fn format_folder(raw: &str) -> String {
    crate::display::format_with_raw(raw, lookup(raw))
}
