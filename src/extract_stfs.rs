//! `xtafkit extract-stfs` — extract an STFS package to a local directory.
//!
//! Mirrors the shape of `run_extract` (XISO) in main.rs.

use std::fs::File;
use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

use fatxlib::stfs::{StfsHeader, StfsPackage, extract::extract_to_host};

use crate::{cli_error, format_size, short_name};

pub fn run_extract_stfs(package: &Path, to: Option<&Path>, dry_run: bool, json: bool) {
    let file = match File::open(package) {
        Ok(f) => f,
        Err(e) => {
            cli_error(json, &format!("open {}: {}", package.display(), e));
            return;
        }
    };

    let mut pkg = match StfsPackage::open(file) {
        Ok(p) => p,
        Err(e) => {
            cli_error(json, &format!("parse {}: {}", package.display(), e));
            return;
        }
    };

    let dest = match to {
        Some(p) => p.to_path_buf(),
        None => default_destination(package, pkg.header()),
    };

    let entries = match pkg.entries() {
        Ok(e) => e,
        Err(e) => {
            cli_error(json, &format!("walk {}: {}", package.display(), e));
            return;
        }
    };
    let total_files = entries.iter().filter(|e| !e.is_directory).count();
    let total_bytes: u64 = entries
        .iter()
        .filter(|e| !e.is_directory)
        .map(|e| e.size)
        .sum();

    if dry_run {
        if json {
            let entries_json: Vec<_> = entries
                .iter()
                .map(|e| {
                    serde_json::json!({
                        "name": e.name,
                        "parent_index": e.parent_index,
                        "is_directory": e.is_directory,
                        "size": e.size,
                        "blocks_used": e.used_blocks,
                        "consecutive": e.consecutive,
                    })
                })
                .collect();
            println!(
                "{}",
                serde_json::json!({
                    "package": package.display().to_string(),
                    "magic": String::from_utf8_lossy(&pkg.header().magic),
                    "title_id": format!("{:08X}", pkg.header().title_id),
                    "title_name": pkg.header().title_name,
                    "display_name": pkg.header().display_name,
                    "files": total_files,
                    "directories": entries.iter().filter(|e| e.is_directory).count(),
                    "bytes": total_bytes,
                    "entries": entries_json,
                })
            );
        } else {
            println!("Package: {}", package.display());
            println!("Magic:   {}", String::from_utf8_lossy(&pkg.header().magic),);
            println!(
                "Title:   {} (0x{:08X})",
                pkg.header().best_name(),
                pkg.header().title_id,
            );
            println!("Files:   {} ({})", total_files, format_size(total_bytes),);
            println!();
            for entry in &entries {
                let kind = if entry.is_directory { "dir " } else { "file" };
                println!(
                    "  {} {:48}  {:>10}",
                    kind,
                    entry.name,
                    if entry.is_directory {
                        String::from("-")
                    } else {
                        format_size(entry.size)
                    },
                );
            }
            println!();
            println!("(dry-run; nothing written)");
        }
        return;
    }

    if let Err(e) = std::fs::create_dir_all(&dest) {
        cli_error(json, &format!("create_dir_all {}: {}", dest.display(), e));
        return;
    }

    let started = Instant::now();
    let last_progress = std::cell::Cell::new(Instant::now());
    let total_bytes_ref = total_bytes;
    let cb = |path: &str, _size: u64, bytes_done: u64| {
        if !json && last_progress.get().elapsed().as_millis() > 250 {
            eprint!(
                "\r  {} ({}/{})         ",
                short_name(path),
                format_size(bytes_done),
                format_size(total_bytes_ref),
            );
            let _ = io::stderr().flush();
            last_progress.set(Instant::now());
        }
    };

    let report = match extract_to_host(&mut pkg, &dest, Some(&cb)) {
        Ok(r) => r,
        Err(e) => {
            if !json {
                eprintln!();
            }
            cli_error(json, &format!("extract: {}", e));
            return;
        }
    };

    let elapsed = started.elapsed();
    if !json {
        eprint!("\r{:80}\r", "");
    }
    if json {
        println!(
            "{}",
            serde_json::json!({
                "package": package.display().to_string(),
                "dest": dest.display().to_string(),
                "title_id": format!("{:08X}", pkg.header().title_id),
                "files": report.files,
                "directories": report.directories,
                "bytes": report.bytes,
                "elapsed_secs": elapsed.as_secs_f64(),
            })
        );
    } else {
        println!(
            "Extracted {} files, {} dirs ({}) → {} in {:?}",
            report.files,
            report.directories,
            format_size(report.bytes),
            dest.display(),
            elapsed,
        );
    }
}

/// Default destination when `--to` is omitted. Prefers the STFS header's
/// own naming over the bundled catalog because the catalog can only resolve
/// `title_id` to a *system* name (e.g. XBLIG → "Community Package"), while
/// the per-package `display_name` carries the specific title (e.g. "DLC
/// Quest"). Resolution order:
///   1. `header.display_name` — specific package label.
///   2. `header.title_name`   — often the parent/system label.
///   3. source file stem      — final fallback.
fn default_destination(package: &Path, header: &StfsHeader) -> PathBuf {
    let candidate = if !header.display_name.trim().is_empty() {
        header.display_name.trim().to_string()
    } else if !header.title_name.trim().is_empty() {
        header.title_name.trim().to_string()
    } else {
        return PathBuf::from(
            package
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "stfs-extract".to_string()),
        );
    };
    PathBuf::from(sanitize_fatx_name(&candidate))
}

/// Replace characters illegal as macOS / Windows path components.
fn sanitize_fatx_name(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '-',
            c if (c as u32) < 0x20 => '-',
            c => c,
        })
        .collect::<String>()
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(display_name: &str, title_name: &str) -> StfsHeader {
        StfsHeader {
            magic: *b"LIVE",
            title_id: 0,
            title_name: title_name.to_string(),
            display_name: display_name.to_string(),
        }
    }

    #[test]
    fn default_destination_uses_display_name_when_present() {
        // display_name is the specific package label; title_name is often
        // a system bucket. We want the specific name.
        let dest = default_destination(
            Path::new("/tmp/pkg"),
            &header("Specific Title", "System Bucket"),
        );
        assert_eq!(dest, PathBuf::from("Specific Title"));
    }

    #[test]
    fn default_destination_omits_title_id_suffix() {
        // Regression guard: no "[XXXXXXXX]" appended to the folder name.
        let dest =
            default_destination(Path::new("/tmp/pkg"), &header("Sample Game", "Sample Game"));
        let s = dest.to_string_lossy();
        assert!(!s.contains('['), "unexpected title-id suffix: {s}");
        assert_eq!(dest, PathBuf::from("Sample Game"));
    }

    #[test]
    fn default_destination_falls_back_to_title_name_when_display_empty() {
        let dest = default_destination(Path::new("/tmp/pkg"), &header("", "Bucket: Subtitle"));
        // The colon is illegal on Windows/macOS and gets sanitized to '-'.
        assert_eq!(dest, PathBuf::from("Bucket- Subtitle"));
    }

    #[test]
    fn default_destination_falls_back_to_file_stem_when_no_names() {
        let dest = default_destination(Path::new("/tmp/some_pkg.stfs"), &header("", ""));
        assert_eq!(dest, PathBuf::from("some_pkg"));
    }
}
