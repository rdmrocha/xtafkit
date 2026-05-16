//! Persistent XUID → gamertag mapping, populated on demand by parsing the
//! profile package found at the canonical path
//! `/Content/<XUID>/FFFE07D1/00010000/<XUID>`.
//!
//! Sibling to [`crate::titles::user_cache`] and [`crate::titles::file_cache`],
//! same plain-text format (one entry per line, `<xuid>\t<gamertag>`,
//! tab-delimited so spaces in gamertags survive).
//!
//! Default file: `~/.config/fatx-rs/user_profiles.txt`.

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

/// Look up the gamertag for a XUID. Returns `None` if not yet detected.
pub fn lookup(xuid: &str) -> Option<String> {
    cache().read().ok()?.get(xuid).cloned()
}

/// Insert or overwrite a XUID → gamertag entry. Call [`save_to`] for durability.
pub fn insert(xuid: String, gamertag: String) {
    if let Ok(mut map) = cache().write() {
        map.insert(xuid, gamertag);
    }
}

/// Number of entries currently in the runtime cache.
pub fn len() -> usize {
    cache().read().map(|m| m.len()).unwrap_or(0)
}

/// Default location of the persistent cache file.
pub fn default_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".config/fatx-rs/user_profiles.txt"))
}

/// Load `<xuid>\t<gamertag>` lines into the runtime cache. Missing file is OK.
///
/// Entries whose gamertag is empty or equals the XUID (case-insensitive)
/// are *skipped* — they are remnants of an earlier bug where the STFS
/// `title_name` field (often the XUID itself) was cached as a gamertag.
/// Dropping them on load lets the next `/Content` listing re-probe the
/// drive and write back a correct entry.
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
        let Some((xuid, name)) = line.split_once('\t') else {
            continue;
        };
        let xuid = xuid.trim();
        let name = name.trim();
        if xuid.is_empty() || name.is_empty() {
            continue;
        }
        if name.eq_ignore_ascii_case(xuid) {
            continue;
        }
        map.insert(xuid.to_string(), name.to_string());
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
        writeln!(file, "# fatx-rs user profile cache — <xuid>\\t<gamertag>")?;
        let mut entries: Vec<(&String, &String)> = map.iter().collect();
        entries.sort_by(|a, b| a.0.cmp(b.0));
        for (xuid, name) in entries {
            writeln!(file, "{xuid}\t{name}")?;
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
        insert("E00012A9B73ABE44".into(), "Bob".into());
        assert_eq!(lookup("E00012A9B73ABE44"), Some("Bob".into()));
        assert_eq!(lookup("MISSING"), None);
    }

    #[test]
    fn load_skips_stale_xuid_equals_name_entries() {
        let _g = TEST_LOCK.lock().unwrap();
        clear();
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("profiles.txt");
        fs::write(
            &p,
            "# header\n\
             E00012A9B73ABE44\tE00012A9B73ABE44\n\
             E00012A9B73ABE45\te00012a9b73abe45\n\
             E00012A9B73ABE46\tBob\n",
        )
        .unwrap();
        // Two stale entries (value == key, case-insensitive) get dropped;
        // only the real gamertag is loaded.
        let n = load_from(&p).unwrap();
        assert_eq!(n, 1);
        assert_eq!(lookup("E00012A9B73ABE44"), None);
        assert_eq!(lookup("E00012A9B73ABE45"), None);
        assert_eq!(lookup("E00012A9B73ABE46"), Some("Bob".to_string()));
    }

    #[test]
    fn save_and_reload_roundtrip() {
        let _g = TEST_LOCK.lock().unwrap();
        clear();
        insert("E00000000000A001".into(), "Player One".into());
        insert("E00000000000A002".into(), "Player Two".into());

        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("profiles.txt");
        save_to(&p).unwrap();

        clear();
        assert_eq!(lookup("E00000000000A001"), None);

        let n = load_from(&p).unwrap();
        assert_eq!(n, 2);
        assert_eq!(lookup("E00000000000A001"), Some("Player One".into()));
        assert_eq!(lookup("E00000000000A002"), Some("Player Two".into()));
    }
}
