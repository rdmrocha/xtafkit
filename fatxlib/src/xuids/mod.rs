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

pub mod account;
pub mod profile_cache;

use std::io::{Read, Seek, Write};

use crate::error::Result;
use crate::stfs;
use crate::types::DirectoryEntry;
use crate::volume::FatxVolume;

const PROFILE_OWNER_TITLE: &str = "FFFE07D1";
const PROFILE_CONTENT_TYPE: &str = "00010000";

/// How many bytes of a profile package we read while hunting for the
/// encrypted Account block. Needs to cover the STFS metadata plus the
/// first few data blocks where the Account file lives — 64 KB comfortably
/// covers both STFS metadata-version layouts on real packages.
const PROFILE_SCAN_BYTES: usize = 0x10000;

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
///   - the header parses but yields no usable name (or only the XUID itself —
///     profile packages frequently store the XUID in `title_name`, which is
///     not a gamertag and shouldn't be shown)
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
    if (entry.file_size as usize) < stfs::MIN_HEADER_BYTES {
        return Ok(None);
    }

    // Read enough to cover both the STFS header (~6 KB) and the embedded
    // Account file (~40–48 KB into the package). Profile packages are
    // small so this is at most one or two clusters' worth of I/O.
    let want = PROFILE_SCAN_BYTES.min(entry.file_size as usize);
    let bytes = match vol.read_file_range(&entry, 0, want) {
        Ok(b) => b,
        Err(_) => return Ok(None),
    };

    let Some(header) = stfs::parse_header(&bytes) else {
        return Ok(None);
    };

    // The STFS-header name fields are usually useless on profile packages:
    // `title_name` is "Xbox 360 Dashboard" (the owner title's name) and
    // `display_name` is the XUID string itself. Try them as a cheap path
    // in case some package writes something useful there, then fall back
    // to decrypting the embedded Account blob.
    if let Some(name) = pick_profile_name(xuid, &header.display_name, &header.title_name) {
        return Ok(Some(name));
    }

    Ok(scan_for_account_gamertag(&bytes))
}

/// Brute-force locate an encrypted Account block in the buffer.
///
/// We don't parse the STFS file table — instead we slide a 404-byte window
/// across 0x1000-aligned offsets starting past the STFS header. For each
/// window, [`crate::xuids::account::extract_gamertag`] tries both PROD and
/// OTHER keys; a successful decryption that yields a plausible gamertag
/// pattern wins. The validation in [`account::looks_like_gamertag`]
/// (printable-ASCII, 1-15 chars, starts with a letter) makes false hits
/// from random ciphertext exceedingly unlikely.
fn scan_for_account_gamertag(bytes: &[u8]) -> Option<String> {
    let mut off = 0x1000;
    while off + account::ACCOUNT_BLOCK_LEN <= bytes.len() {
        if let Some(name) = account::extract_gamertag(&bytes[off..off + account::ACCOUNT_BLOCK_LEN])
        {
            return Some(name);
        }
        off += 0x1000;
    }
    None
}

/// Choose a profile name from the two STFS header strings, rejecting any
/// candidate that's empty or equal (case-insensitive) to the XUID itself.
fn pick_profile_name(xuid: &str, display_name: &str, title_name: &str) -> Option<String> {
    for candidate in [display_name, title_name] {
        let trimmed = candidate.trim().trim_matches('\u{FEFF}');
        if !trimmed.is_empty() && !trimmed.eq_ignore_ascii_case(xuid) {
            return Some(trimmed.to_string());
        }
    }
    None
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

    #[test]
    fn pick_profile_name_prefers_display_name() {
        // Game-like input: title_name has a real name → still ok.
        assert_eq!(
            pick_profile_name("E00012A9B73ABE44", "BobsTag", "Halo 3"),
            Some("BobsTag".to_string())
        );
    }

    #[test]
    fn pick_profile_name_falls_back_to_title_when_display_empty() {
        assert_eq!(
            pick_profile_name("E00012A9B73ABE44", "", "BobsTag"),
            Some("BobsTag".to_string())
        );
    }

    #[test]
    fn pick_profile_name_rejects_xuid_string() {
        // Profile packages commonly store the XUID as title_name; that must
        // not surface as a "resolution".
        assert_eq!(
            pick_profile_name("E00012A9B73ABE44", "", "E00012A9B73ABE44"),
            None
        );
        // Case-insensitive guard.
        assert_eq!(
            pick_profile_name("E00012A9B73ABE44", "e00012a9b73abe44", ""),
            None
        );
    }

    #[test]
    fn pick_profile_name_uses_display_even_if_title_is_xuid() {
        // The real-world case: title_name = XUID, display_name = gamertag.
        assert_eq!(
            pick_profile_name("E00012A9B73ABE44", "Bob", "E00012A9B73ABE44"),
            Some("Bob".to_string())
        );
    }

    #[test]
    fn pick_profile_name_returns_none_when_both_empty() {
        assert_eq!(pick_profile_name("E00012A9B73ABE44", "", ""), None);
    }

    #[test]
    fn scan_for_account_gamertag_finds_synthetic_block() {
        // Build a buffer with an encrypted Account block embedded at a
        // 0x1000-aligned offset — exactly what the brute-force scan walks.
        let block = super::account::tests::synth_block_helper("BobsTag");
        let mut buf = vec![0u8; PROFILE_SCAN_BYTES];
        buf[0x4000..0x4000 + block.len()].copy_from_slice(&block);
        assert_eq!(
            scan_for_account_gamertag(&buf),
            Some("BobsTag".to_string())
        );
    }

    #[test]
    fn scan_for_account_gamertag_returns_none_when_no_block_present() {
        let buf = vec![0u8; PROFILE_SCAN_BYTES];
        assert_eq!(scan_for_account_gamertag(&buf), None);
    }
}

