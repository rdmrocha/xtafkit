//! Shared ISO manifest planning.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek};

use crate::error::Result;
use crate::executable::TitleInfo;

use super::image::{XisoFile, XisoImage};
use super::policy::is_systemupdate_path;

#[derive(Debug, Clone, Copy, Default)]
pub struct IsoFilterPolicy {
    pub keep_systemupdate: bool,
}

impl IsoFilterPolicy {
    pub fn keeps(&self, path: &str) -> bool {
        self.keep_systemupdate || !is_systemupdate_path(path)
    }
}

#[derive(Debug, Clone)]
pub struct ManifestEntry {
    pub file: XisoFile,
    pub skipped: bool,
}

impl ManifestEntry {
    pub fn path(&self) -> &str {
        &self.file.path
    }
}

#[derive(Debug, Clone)]
pub struct IsoManifest {
    pub layout: String,
    pub partition_offset: u64,
    pub title_info: Option<TitleInfo>,
    pub entries: Vec<ManifestEntry>,
    pub kept_bytes: u64,
    pub skipped_bytes: u64,
}

impl IsoManifest {
    pub fn kept_files(&self) -> usize {
        self.entries.iter().filter(|entry| !entry.skipped).count()
    }

    pub fn skipped_files(&self) -> usize {
        self.entries.iter().filter(|entry| entry.skipped).count()
    }

    pub fn kept(&self) -> impl Iterator<Item = &XisoFile> {
        self.entries
            .iter()
            .filter(|entry| !entry.skipped)
            .map(|entry| &entry.file)
    }

    pub fn skipped(&self) -> impl Iterator<Item = &XisoFile> {
        self.entries
            .iter()
            .filter(|entry| entry.skipped)
            .map(|entry| &entry.file)
    }

    pub fn kept_path_set(&self) -> HashSet<String> {
        self.kept()
            .map(|entry| normalize_path(&entry.path).to_string())
            .collect()
    }

    pub fn kept_dir_set(&self) -> HashSet<String> {
        let mut dirs = HashSet::from([String::new()]);
        for entry in self.kept() {
            let mut prefix = String::new();
            let path = normalize_path(&entry.path);
            for component in path.split('/').take(path.matches('/').count()) {
                if !prefix.is_empty() {
                    prefix.push('/');
                }
                prefix.push_str(component);
                dirs.insert(prefix.clone());
            }
        }
        dirs
    }

    pub fn kept_offset_map(&self) -> HashMap<String, u64> {
        self.kept()
            .map(|entry| (normalize_path(&entry.path).to_string(), entry.offset))
            .collect()
    }
}

pub fn build_manifest<R: Read + Seek + Send + Sync>(
    img: &mut XisoImage<R>,
    policy: IsoFilterPolicy,
) -> Result<IsoManifest> {
    let files = img.walk_files()?;
    let entries: Vec<ManifestEntry> = files
        .into_iter()
        .map(|file| ManifestEntry {
            skipped: !policy.keeps(&file.path),
            file,
        })
        .collect();
    let kept_bytes = entries
        .iter()
        .filter(|entry| !entry.skipped)
        .map(|entry| entry.file.size)
        .sum();
    let skipped_bytes = entries
        .iter()
        .filter(|entry| entry.skipped)
        .map(|entry| entry.file.size)
        .sum();
    let layout = img
        .layout()
        .map(|layout| format!("{} (0x{:08X})", layout.name, layout.offset))
        .unwrap_or_else(|| format!("unknown @ 0x{:08X}", img.partition_offset()));

    Ok(IsoManifest {
        layout,
        partition_offset: img.partition_offset(),
        title_info: img.title_info()?,
        entries,
        kept_bytes,
        skipped_bytes,
    })
}

fn normalize_path(path: &str) -> &str {
    path.trim_start_matches('/')
}

#[cfg(test)]
mod tests {
    use super::{IsoManifest, ManifestEntry};
    use crate::iso::image::XisoFile;

    #[test]
    fn kept_dir_set_contains_all_parent_directories() {
        let manifest = IsoManifest {
            layout: String::new(),
            partition_offset: 0,
            title_info: None,
            kept_bytes: 12,
            skipped_bytes: 0,
            entries: vec![
                ManifestEntry {
                    file: XisoFile {
                        path: "default.xex".to_string(),
                        size: 4,
                        offset: 0,
                    },
                    skipped: false,
                },
                ManifestEntry {
                    file: XisoFile {
                        path: "Media/Videos/intro.bik".to_string(),
                        size: 8,
                        offset: 4,
                    },
                    skipped: false,
                },
            ],
        };

        let dirs = manifest.kept_dir_set();
        assert!(dirs.contains(""));
        assert!(dirs.contains("Media"));
        assert!(dirs.contains("Media/Videos"));
    }
}
