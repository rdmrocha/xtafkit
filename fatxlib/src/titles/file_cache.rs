//! Persistent per-file resolutions for STFS packages inside content-type
//! folders (Arcade, XNA, Marketplace, Installer).
//!
//! Sibling to [`super::user_cache`]. Key is the full volume-relative file
//! path (e.g. `/Content/.../000D0000/abc123def456`), value is the title
//! name extracted from the STFS header.
//!
//! Plain-text format with a tab delimiter (file paths can contain spaces
//! per FATX's allowed character set, so space is not safe as a separator):
//!
//! ```text
//! # xtafkit user file cache
//! /Content/.../000D0000/abc123\tHalo Wars
//! ```
//!
//! Staleness on rename is accepted by design — the file path is the key,
//! so renaming a file invalidates its cache entry. Re-running R on the
//! folder produces a fresh entry.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Write as IoWrite};
use std::path::Path;
use std::path::PathBuf;
use std::sync::{OnceLock, RwLock};

static CACHE: OnceLock<RwLock<HashMap<String, String>>> = OnceLock::new();

fn cache() -> &'static RwLock<HashMap<String, String>> {
    CACHE.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Look up a resolved name by full file path. Returns `None` if not cached.
pub fn lookup(path: &str) -> Option<String> {
    cache().read().ok()?.get(path).cloned()
}

/// Insert/overwrite a per-file resolution. Call [`save_to`] for durability.
pub fn insert(path: String, name: String) {
    if let Ok(mut map) = cache().write() {
        map.insert(path, name);
    }
}

/// Number of entries currently in the runtime cache.
pub fn len() -> usize {
    cache().read().map(|m| m.len()).unwrap_or(0)
}

/// Default location of the user file cache (`~/.config/xtafkit/user_files.txt`).
pub fn default_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".config/xtafkit/user_files.txt"))
}

/// Load entries from a plain-text file into the runtime cache. Lines must
/// be `<path>\t<name>`; lines starting with `#` and blank lines are skipped.
/// Missing file is not an error.
pub fn load_from(path: &Path) -> io::Result<usize> {
    if !path.exists() {
        return Ok(0);
    }
    let text = fs::read_to_string(path)?;
    let mut loaded = 0;
    let mut map = cache().write().map_err(|_| io::Error::other("cache lock"))?;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((file_path, name)) = line.split_once('\t') else {
            continue;
        };
        if file_path.is_empty() {
            continue;
        }
        map.insert(file_path.to_string(), name.trim_end().to_string());
        loaded += 1;
    }
    Ok(loaded)
}

/// Persist the runtime cache atomically (temp-file + rename).
pub fn save_to(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    {
        let map = cache().read().map_err(|_| io::Error::other("cache lock"))?;
        let mut file = fs::File::create(&tmp)?;
        writeln!(file, "# xtafkit user file cache — one entry per line: <path>\\t<name>")?;
        let mut entries: Vec<(&String, &String)> = map.iter().collect();
        entries.sort_by(|a, b| a.0.cmp(b.0));
        for (p, name) in entries {
            writeln!(file, "{p}\t{name}")?;
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

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn clear() {
        if let Ok(mut m) = cache().write() {
            m.clear();
        }
    }

    #[test]
    fn insert_and_lookup() {
        let _g = TEST_LOCK.lock().unwrap();
        clear();
        let path = "/Content/0/4D5307E6/000D0000/some_pkg".to_string();
        insert(path.clone(), "Halo Wars".into());
        assert_eq!(lookup(&path), Some("Halo Wars".into()));
        assert_eq!(lookup("/missing"), None);
    }

    #[test]
    fn save_and_reload_roundtrip() {
        let _g = TEST_LOCK.lock().unwrap();
        clear();
        insert("/a/b c/file 1".into(), "Title With Spaces".into());
        insert("/x/y/z".into(), "Other".into());

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("files.txt");
        save_to(&path).unwrap();

        clear();
        assert_eq!(lookup("/a/b c/file 1"), None);

        let n = load_from(&path).unwrap();
        assert_eq!(n, 2);
        assert_eq!(lookup("/a/b c/file 1"), Some("Title With Spaces".into()));
        assert_eq!(lookup("/x/y/z"), Some("Other".into()));
    }

    #[test]
    fn missing_file_is_not_an_error() {
        let _g = TEST_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("nope.txt");
        assert_eq!(load_from(&p).unwrap(), 0);
    }
}
