//! XUID-folder resolution.
//!
//! On an Xbox 360 FATX volume, `Content/<XUID>/` separates per-profile content
//! from the system-wide bucket at `0000000000000000`. Per
//! <https://free60.org/System-Software/Systems/FATX/> and community wikis,
//! the all-zeros XUID is the "public / shared content directory" — anything
//! downloaded from the Marketplace or installed system-wide lives here.
//!
//! Personal XUIDs map to gamertags. Each profile-owning XUID has a profile
//! package at the canonical path
//! `/Content/<XUID>/FFFE07D1/00010000/<XUID>` — a STFS container whose
//! header's display-name field holds the gamertag. [`detect_profile_name`]
//! probes that location and parses out the gamertag; results are cached in
//! [`profile_cache`].

pub mod profile_cache;

use std::io::{Read, Seek, Write};

use crate::error::Result;
use crate::stfs::{self, MIN_HEADER_BYTES};
use crate::types::DirectoryEntry;
use crate::volume::FatxVolume;

const PROFILE_OWNER_TITLE: &str = "FFFE07D1";
const PROFILE_CONTENT_TYPE: &str = "00010000";

/// Resolve a raw 16-hex XUID folder name to a human label. Checks (in order):
///   1. The hardcoded `Shared` mapping for the all-zeros XUID.
///   2. The runtime [`profile_cache`], populated by on-demand or eager
///      [`detect_profile_name`] calls.
///
/// Returns `None` when neither applies; the caller should render raw.
pub fn lookup(raw: &str) -> Option<String> {
    if raw == "0000000000000000" {
        return Some("Shared".to_string());
    }
    profile_cache::lookup(raw)
}

/// Render a raw XUID folder name as `"<name> [<raw>]"` if known, otherwise
/// just `<raw>` unchanged. Raw case is preserved verbatim.
pub fn format_folder(raw: &str) -> String {
    crate::display::format_with_raw(raw, lookup(raw).as_deref())
}

/// Probe the canonical profile path for a XUID and, on success, return the
/// gamertag string extracted from the STFS header.
///
/// Returns `Ok(None)` (not an error) when any of the following fails:
///   - the file at `/Content/<XUID>/FFFE07D1/00010000/<XUID>` doesn't exist
///   - it's a directory or too small to hold an STFS header
///   - the STFS magic doesn't match `CON `/`LIVE`/`PIRS`
///   - the header parses but yields an empty display/title name
///
/// Per the design contract, ANY failure → do nothing (no label).
pub fn detect_profile_name<T: Read + Write + Seek>(
    vol: &mut FatxVolume<T>,
    xuid: &str,
) -> Result<Option<String>> {
    let path = format!("/Content/{xuid}/{PROFILE_OWNER_TITLE}/{PROFILE_CONTENT_TYPE}/{xuid}");
    let entry = match vol.resolve_path(&path) {
        Ok(e) => e,
        Err(_) => return Ok(None),
    };
    if entry.is_directory() {
        return Ok(None);
    }
    if (entry.file_size as usize) < MIN_HEADER_BYTES {
        return Ok(None);
    }
    let bytes = match vol.read_file_range(&entry, 0, MIN_HEADER_BYTES) {
        Ok(b) => b,
        Err(_) => return Ok(None),
    };
    let Some(header) = stfs::parse_header(&bytes) else {
        return Ok(None);
    };
    let name = header.best_name();
    if name.is_empty() {
        return Ok(None);
    }
    Ok(Some(name.to_string()))
}

/// Walk `entries` (the children of `/Content`), probe each unknown
/// personal XUID for a profile package, and populate [`profile_cache`] with
/// any successful detections. Persists the cache to its default path when
/// `persist` is true and at least one new entry was added.
///
/// Returns the number of newly-added profile entries.
pub fn resolve_profile_xuids<T: Read + Write + Seek>(
    vol: &mut FatxVolume<T>,
    entries: &[DirectoryEntry],
    persist: bool,
) -> Result<usize> {
    let mut newly_added = 0;
    for entry in entries {
        if !entry.is_directory() {
            continue;
        }
        let xuid = entry.filename();
        if !is_personal_xuid(&xuid) {
            continue;
        }
        if profile_cache::lookup(&xuid).is_some() {
            continue;
        }
        if let Ok(Some(name)) = detect_profile_name(vol, &xuid) {
            profile_cache::insert(xuid, name);
            newly_added += 1;
        }
    }
    if persist && newly_added > 0 {
        if let Some(p) = profile_cache::default_path() {
            let _ = profile_cache::save_to(&p);
        }
    }
    Ok(newly_added)
}

/// True for 16-hex XUIDs that aren't the all-zeros shared bucket.
fn is_personal_xuid(s: &str) -> bool {
    s.len() == 16
        && s != "0000000000000000"
        && s.chars().all(|c| c.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_personal_xuid_filters_correctly() {
        assert!(is_personal_xuid("E00012A9B73ABE44"));
        assert!(is_personal_xuid("F000FFFFFFFFFFFF"));
        assert!(!is_personal_xuid("0000000000000000")); // shared
        assert!(!is_personal_xuid("E00012A9B73ABE4"));  // too short
        assert!(!is_personal_xuid("E00012A9B73ABE4XX")); // non-hex
        assert!(!is_personal_xuid(""));
        assert!(!is_personal_xuid("Content"));
    }

    #[test]
    fn lookup_shared_unchanged() {
        assert_eq!(lookup("0000000000000000"), Some("Shared".to_string()));
    }

    #[test]
    fn lookup_unknown_xuid_returns_none() {
        // Not in cache, not all-zeros.
        assert_eq!(lookup("FFFFFFFFFFFFFFFF"), None);
    }
}
