//! Compile-time codegen for the title-ID → name lookup table.
//!
//! Sources merged into a single `phf::Map<u32, TitleInfo>`:
//!
//!   * Xbox 360:
//!     https://gist.github.com/AdrianCassar/c0d05a14608168259232b3ed8c77f28c
//!     Flat JSON array of `{ "titleid": "<8-hex>", "title": "<name>" }`.
//!
//!   * Original Xbox:
//!     https://github.com/jeltaqq/Xbox-Original-GameList
//!     TSV with columns `Xbox Game Title \t Title ID (Hex) \t Title \t XDK`.
//!     Column 0 (the editorial name) is the display name. Rows with an empty
//!     title ID are skipped.
//!
//! Merge policy:
//!   * ID only in 360 → `Source::Xbox360`.
//!   * ID only in OG  → `Source::XboxOriginal`.
//!   * ID in both     → OG name wins (better editorial), `Source::Both`. If
//!                      the names normalize-differ we emit a `cargo:warning`
//!                      so genuine drift surfaces at build time.

use std::collections::BTreeMap;
use std::env;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::PathBuf;

use serde::Deserialize;

#[derive(Deserialize)]
struct Xbox360Entry {
    titleid: String,
    title: String,
}

fn main() {
    let out_dir: PathBuf = env::var_os("OUT_DIR").expect("OUT_DIR not set").into();

    let xbox360 = read_xbox360("data/xbox360_titles.json");
    let xbox_og = read_xbox_originals("data/xbox_originals.tsv");
    let (merged, conflicts) = merge(&xbox360, &xbox_og);

    write_merged(&out_dir.join("titles.rs"), &merged);
    write_conflicts(&out_dir.join("title_conflicts.txt"), &conflicts);
}

fn read_xbox360(src: &str) -> BTreeMap<u32, String> {
    println!("cargo:rerun-if-changed={src}");
    let bytes = fs::read(src).unwrap_or_else(|e| panic!("read {src}: {e}"));
    let entries: Vec<Xbox360Entry> =
        serde_json::from_slice(&bytes).unwrap_or_else(|e| panic!("parse {src}: {e}"));

    // Last write wins (the gist has a handful of duplicate IDs).
    let mut map = BTreeMap::new();
    for e in entries {
        let id = u32::from_str_radix(e.titleid.trim_start_matches("0x"), 16)
            .unwrap_or_else(|_| panic!("bad title id {:?}", e.titleid));
        map.insert(id, e.title);
    }
    map
}

fn read_xbox_originals(src: &str) -> BTreeMap<u32, String> {
    println!("cargo:rerun-if-changed={src}");
    let text = fs::read_to_string(src).unwrap_or_else(|e| panic!("read {src}: {e}"));

    let mut map = BTreeMap::new();
    for (i, line) in text.lines().enumerate() {
        if i == 0 || line.trim().is_empty() {
            continue; // header / blank
        }
        let cols: Vec<&str> = line.split('\t').collect();
        if cols.len() < 2 {
            continue;
        }
        let name = cols[0].trim();
        let id_hex = cols[1].trim();
        if name.is_empty() || id_hex.is_empty() {
            continue;
        }
        let id = u32::from_str_radix(id_hex, 16)
            .unwrap_or_else(|_| panic!("line {}: bad title id {:?}", i + 1, id_hex));
        map.insert(id, name.to_string());
    }
    map
}

/// Merge the two sources. Returns the merged map and the list of conflicting
/// IDs (where the two sources disagree on the name post-normalize).
fn merge(
    xbox360: &BTreeMap<u32, String>,
    xbox_og: &BTreeMap<u32, String>,
) -> (BTreeMap<u32, (String, &'static str)>, Vec<Conflict>) {
    let mut out = BTreeMap::new();
    let mut conflicts = Vec::new();

    for (id, name360) in xbox360 {
        if let Some(name_og) = xbox_og.get(id) {
            if !names_compatible(name360, name_og) {
                conflicts.push(Conflict {
                    id: *id,
                    name_360: name360.clone(),
                    name_og: name_og.clone(),
                });
            }
            out.insert(*id, (name_og.clone(), "Source::Both"));
        } else {
            out.insert(*id, (name360.clone(), "Source::Xbox360"));
        }
    }
    for (id, name_og) in xbox_og {
        if !xbox360.contains_key(id) {
            out.insert(*id, (name_og.clone(), "Source::XboxOriginal"));
        }
    }

    (out, conflicts)
}

struct Conflict {
    id: u32,
    name_360: String,
    name_og: String,
}

fn write_conflicts(dst: &PathBuf, conflicts: &[Conflict]) {
    let mut out = BufWriter::new(File::create(dst).expect("create title_conflicts.txt"));
    writeln!(
        out,
        "# Title-name conflicts between Xbox 360 (AdrianCassar gist) and\n\
         # Original Xbox (jeltaqq) sources, after normalize+whitespace+subtitle\n\
         # equivalence. The OG name is what ends up in the compiled map.\n\
         # {} conflict(s) total.\n",
        conflicts.len(),
    )
    .unwrap();
    for c in conflicts {
        writeln!(
            out,
            "0x{:08X}  360={:?}  og={:?}",
            c.id, c.name_360, c.name_og
        )
        .unwrap();
    }

    if !conflicts.is_empty() {
        println!(
            "cargo:warning=title catalog: {} name conflict(s) — see {}",
            conflicts.len(),
            dst.display()
        );
    }
}

fn normalize(s: &str) -> String {
    let cleaned: String = s
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect();
    cleaned.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Whether two title-name variants are "compatible enough" to dedupe silently.
///
/// Three layers, increasing tolerance:
///   1. Exact normalize match  — punctuation/case only.
///   2. Whitespace-insensitive — catches `MarvelVsCapcom2` vs `Marvel vs. Capcom 2`.
///   3. Subtitle-marker prefix — catches `MechAssault 2` vs `MechAssault 2: Lone Wolf`.
///
/// We deliberately do **not** suppress plain word-boundary prefixes
/// (e.g. `Halo` vs `Halo Wars`) — those are genuinely different games.
fn names_compatible(a: &str, b: &str) -> bool {
    let na = normalize(a);
    let nb = normalize(b);
    if na == nb {
        return true;
    }
    if strip_ws(&na) == strip_ws(&nb) {
        return true;
    }
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    let short_n = normalize(short);
    for marker in [":", ";", " - ", " — "] {
        if let Some((before, _)) = long.split_once(marker) {
            if normalize(before) == short_n {
                return true;
            }
        }
    }
    false
}

fn strip_ws(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

fn write_merged(dst: &PathBuf, map: &BTreeMap<u32, (String, &str)>) {
    let mut builder = phf_codegen::Map::<u32>::new();
    let literals: Vec<String> = map
        .values()
        .map(|(name, src)| format!("TitleInfo {{ name: {name:?}, source: {src} }}"))
        .collect();
    for ((id, _), lit) in map.iter().zip(literals.iter()) {
        builder.entry(*id, lit);
    }

    let mut out = BufWriter::new(File::create(dst).expect("create titles.rs"));
    writeln!(
        out,
        "// @generated by build.rs from data/xbox360_titles.json + data/xbox_originals.tsv\n\
         pub static TITLES: phf::Map<u32, TitleInfo> = {};\n\
         pub const ENTRY_COUNT: usize = {};",
        builder.build(),
        map.len(),
    )
    .unwrap();
}
