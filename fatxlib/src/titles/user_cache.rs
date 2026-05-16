//! Persistent user-supplied title resolutions, layered on top of the
//! compiled-in catalog.
//!
//! Plain-text format: one line per entry, `<8-hex-id> <name>`. Lines
//! starting with `#` and blank lines are ignored. This keeps the file
//! human-readable and editable without a JSON parser.
//!
//! The runtime in-memory map is checked by [`super::lookup`] *after* the
//! compiled catalog; the catalog wins on conflicts, so a stale or wrong
//! user entry can't shadow a known title.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Write as IoWrite};
use std::path::Path;
use std::path::PathBuf;
use std::sync::{OnceLock, RwLock};

static CACHE: OnceLock<RwLock<HashMap<u32, String>>> = OnceLock::new();

fn cache() -> &'static RwLock<HashMap<u32, String>> {
    CACHE.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Look up an entry from the runtime user cache. Returns `None` when the
/// title hasn't been resolved by the user yet.
pub fn lookup(title_id: u32) -> Option<String> {
    cache().read().ok()?.get(&title_id).cloned()
}

/// Insert or overwrite a user resolution in the runtime cache. Call
/// [`save_to`] afterwards to make it durable.
pub fn insert(title_id: u32, name: String) {
    if let Ok(mut map) = cache().write() {
        map.insert(title_id, name);
    }
}

/// Number of entries currently in the runtime cache.
pub fn len() -> usize {
    cache().read().map(|m| m.len()).unwrap_or(0)
}

/// Default location of the user cache file (`~/.config/fatx-rs/user_titles.txt`).
/// Returns `None` if `HOME` is not set.
pub fn default_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".config/fatx-rs/user_titles.txt"))
}

/// Load entries from a plain-text file into the runtime cache. Existing
/// entries with the same ID are overwritten. Missing file is not an error.
pub fn load_from(path: &Path) -> io::Result<usize> {
    if !path.exists() {
        return Ok(0);
    }
    let text = fs::read_to_string(path)?;
    let mut loaded = 0;
    let mut map = cache().write().map_err(|_| io::Error::other("cache lock"))?;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let (id_str, name) = match trimmed.split_once(' ') {
            Some(pair) => pair,
            None => continue,
        };
        let id = match u32::from_str_radix(id_str.trim_start_matches("0x"), 16) {
            Ok(id) => id,
            Err(_) => continue,
        };
        map.insert(id, name.trim().to_string());
        loaded += 1;
    }
    Ok(loaded)
}

/// Persist the runtime cache to a plain-text file, atomically (write
/// to a temp file then rename).
pub fn save_to(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    {
        let map = cache().read().map_err(|_| io::Error::other("cache lock"))?;
        let mut file = fs::File::create(&tmp)?;
        writeln!(file, "# fatx-rs user title cache — one entry per line: <hex-id> <name>")?;
        let mut entries: Vec<(&u32, &String)> = map.iter().collect();
        entries.sort_by_key(|(id, _)| **id);
        for (id, name) in entries {
            writeln!(file, "{id:08X} {name}")?;
        }
        file.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Serialize tests since they touch a process-global static.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn clear() {
        if let Ok(mut m) = cache().write() {
            m.clear();
        }
    }

    #[test]
    fn insert_and_lookup() {
        let _guard = TEST_LOCK.lock().unwrap();
        clear();
        insert(0xDEAD_BEEF, "Test Title".into());
        assert_eq!(lookup(0xDEAD_BEEF), Some("Test Title".into()));
        assert_eq!(lookup(0x1234_5678), None);
    }

    #[test]
    fn save_and_reload_roundtrip() {
        let _guard = TEST_LOCK.lock().unwrap();
        clear();
        insert(0x4D53_0064, "Halo 2".into());
        insert(0x4D53_07E6, "Halo 3".into());

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("cache.txt");
        save_to(&path).unwrap();

        clear();
        assert_eq!(lookup(0x4D53_0064), None);

        let n = load_from(&path).unwrap();
        assert_eq!(n, 2);
        assert_eq!(lookup(0x4D53_0064), Some("Halo 2".into()));
        assert_eq!(lookup(0x4D53_07E6), Some("Halo 3".into()));
    }

    #[test]
    fn load_skips_comments_and_blank_lines() {
        let _guard = TEST_LOCK.lock().unwrap();
        clear();
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("cache.txt");
        fs::write(
            &path,
            "# header comment\n\n4D5307E6 Halo 3\n  # indented comment\n1234 short id ok\n",
        )
        .unwrap();
        let n = load_from(&path).unwrap();
        assert_eq!(n, 2);
        assert_eq!(lookup(0x4D53_07E6), Some("Halo 3".into()));
        assert_eq!(lookup(0x1234), Some("short id ok".into()));
    }

    #[test]
    fn missing_file_is_not_an_error() {
        let _guard = TEST_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("does-not-exist.txt");
        assert_eq!(load_from(&path).unwrap(), 0);
    }
}
