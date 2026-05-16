//! On-demand title resolution: walk a `Content/<XUID>/<TitleID>/` folder,
//! find the first parseable STFS package, and extract its title name.
//!
//! Used when [`crate::titles::lookup`] misses (e.g. homebrew or dev-kit
//! titles not present in the bundled catalog). Pairs with [`super::user_cache`]
//! to persist successful resolutions across runs.

use std::io::{Read, Seek, Write};
use std::path::PathBuf;

use crate::error::Result;
use crate::stfs::{self, MIN_HEADER_BYTES};
use crate::titles::user_cache;
use crate::volume::FatxVolume;

/// Outcome of [`resolve_and_cache`]. Callers present this however they like
/// (text, JSON, TUI status message); the library doesn't bake in a format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveOutcome {
    /// STFS parse succeeded; `name` was inserted into the runtime cache and,
    /// when `saved_to` is `Some(_)`, persisted to that path.
    Resolved {
        title_id: u32,
        name: String,
        saved_to: Option<PathBuf>,
    },
    /// The path's trailing segment wasn't a parseable 8-hex title ID, so
    /// there was no key under which to cache anything.
    BadTitleIdInPath { last_segment: String },
    /// Walked the folder but no parseable STFS package was found.
    NoStfs,
}

/// Extract the title ID from the trailing path segment, if it's an 8-hex value.
pub fn title_id_from_path(path: &str) -> Option<u32> {
    let last = path.trim_end_matches('/').rsplit('/').next()?;
    if last.len() != 8 {
        return None;
    }
    u32::from_str_radix(last, 16).ok()
}

/// End-to-end on-demand resolution: parse the title ID out of the path,
/// run the STFS resolver, insert into the runtime cache on success, and
/// (when `persist` is true) save the cache to its default location on disk.
///
/// I/O errors from the filesystem propagate. Cache-save errors are logged
/// to the caller via `saved_to == None` rather than failing the call,
/// since a successful resolve is independently useful for this process.
pub fn resolve_and_cache<T: Read + Write + Seek>(
    vol: &mut FatxVolume<T>,
    title_folder_path: &str,
    persist: bool,
) -> Result<ResolveOutcome> {
    let title_id = match title_id_from_path(title_folder_path) {
        Some(id) => id,
        None => {
            let last = title_folder_path
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .unwrap_or("")
                .to_string();
            return Ok(ResolveOutcome::BadTitleIdInPath { last_segment: last });
        }
    };

    let Some(name) = from_folder(vol, title_folder_path)? else {
        return Ok(ResolveOutcome::NoStfs);
    };

    user_cache::insert(title_id, name.clone());

    let saved_to = if persist {
        user_cache::default_path().and_then(|p| user_cache::save_to(&p).ok().map(|_| p))
    } else {
        None
    };

    Ok(ResolveOutcome::Resolved {
        title_id,
        name,
        saved_to,
    })
}

#[cfg(test)]
mod tests {
    use super::title_id_from_path;

    #[test]
    fn extracts_id_from_clean_path() {
        assert_eq!(
            title_id_from_path("/Content/0000000000000000/4D5307E6"),
            Some(0x4D5307E6)
        );
    }

    #[test]
    fn extracts_id_with_trailing_slash() {
        assert_eq!(
            title_id_from_path("/Content/0000000000000000/4D5307E6/"),
            Some(0x4D5307E6)
        );
    }

    #[test]
    fn rejects_wrong_length() {
        // 16-hex (XUID) and short hex shouldn't match the title-ID slot.
        assert_eq!(title_id_from_path("/Content/0000000000000000"), None);
        assert_eq!(title_id_from_path("/Content/0000/AB"), None);
    }

    #[test]
    fn rejects_non_hex() {
        assert_eq!(title_id_from_path("/Content/0000000000000000/HelloAll"), None);
    }
}

/// Resolve a title display name by reading the STFS header of the first
/// parseable file found under `title_folder_path`.
///
/// Walks one directory level deep — directly contained files first, then
/// files inside any immediate subdirectory (the content-type tier). Files
/// are tried smallest first to minimize I/O on a successful first hit.
///
/// Returns `Ok(Some(name))` on a successful parse, `Ok(None)` if no usable
/// STFS file was found, or `Err(_)` for filesystem I/O errors.
pub fn from_folder<T: Read + Write + Seek>(
    vol: &mut FatxVolume<T>,
    title_folder_path: &str,
) -> Result<Option<String>> {
    let entry = vol.resolve_path(title_folder_path)?;
    if !entry.is_directory() {
        return Ok(None);
    }

    let mut candidates: Vec<(u64, String)> = Vec::new();
    let trimmed = title_folder_path.trim_end_matches('/');
    let children = vol.read_directory(entry.first_cluster)?;
    for child in &children {
        let name = child.filename();
        let child_path = format!("{trimmed}/{name}");
        if child.is_directory() {
            if let Ok(sub_entries) = vol.read_directory(child.first_cluster) {
                for sub in &sub_entries {
                    if !sub.is_directory() {
                        candidates.push((
                            sub.file_size as u64,
                            format!("{child_path}/{}", sub.filename()),
                        ));
                    }
                }
            }
        } else {
            candidates.push((child.file_size as u64, child_path));
        }
    }

    candidates.sort_by_key(|(size, _)| *size);

    for (size, path) in candidates {
        if (size as usize) < MIN_HEADER_BYTES {
            continue;
        }
        let entry = match vol.resolve_path(&path) {
            Ok(e) => e,
            Err(_) => continue,
        };
        let bytes = match vol.read_file_range(&entry, 0, MIN_HEADER_BYTES) {
            Ok(b) => b,
            Err(_) => continue,
        };
        if let Some(header) = stfs::parse_header(&bytes) {
            let name = header.best_name();
            if !name.is_empty() {
                return Ok(Some(name.to_string()));
            }
        }
    }

    Ok(None)
}
