# xtafkit — Project Rules

## Overview
Rust toolkit for reading and writing FATX/XTAF file systems on Xbox/Xbox 360 formatted drives connected via USB to macOS. Single TUI-first binary with title/profile resolution and a small CLI surface for scripting. Started as a fork of `joshuareisbord/fatx-rs`; now maintained independently.

## Architecture
- **Cargo workspace** with two crates:
  - `fatxlib` — Library crate. FATX/XTAF volume implementation, types, partition detection, platform I/O. Also: bundled title catalog (Xbox 360 + Original Xbox), STFS header parser, profile (Account) blob decryption, slot-aware display formatting.
  - `xtafkit` (root) — Single binary (`xtafkit`). Five subcommands via clap (`browse`, `ls`, `scan`, `mkimage`, `resolve`); no-args entry point launches the TUI via guided picker. Ratatui-based TUI is the primary UX. Test image generator (`mkimage`) is the only non-TUI write path that ships.

## Key Technical Details

### Endianness
- **FATX** (Original Xbox): Little-endian on-disk format. Magic: `46 41 54 58` ("FATX")
- **XTAF** (Xbox 360): **Big-endian** on-disk format. Magic: `58 54 41 46` ("XTAF")
- The `big_endian` field on `FatxVolume` controls byte order for ALL on-disk fields: superblock, FAT entries, and directory entries
- Always use the endian-aware helpers (`read_u16`, `read_u32`, `write_u16_bytes`, `write_u32_bytes`) — never raw `from_le_bytes`/`from_be_bytes` outside of those helpers

### Disk Format
- 4KB superblock, single FAT copy, 64-byte directory entries, 42-char filename max
- FAT16 (< 65,520 clusters) vs FAT32 (larger partitions)
- FAT size rounded UP to 4KB boundary
- Xbox 360 partition offsets: Game Content @ 0x80080000, Data @ 0x130EB0000
- **XTAF cluster count**: Xbox 360 uses `(partition_size - superblock) / cluster_size` — it does NOT subtract FAT space. Using the wrong formula shifts data_offset on large partitions.
- **XTAF timestamp layout**: Directory entry offsets 52-55 store `date(2) + time(2)` (date first), whereas FATX stores `time(2) + date(2)` (time first). Same packed FAT format, just swapped field order. Timestamps are stored in UTC.

### macOS Raw Device I/O
- Raw devices (`/dev/rdiskN`) require ALL I/O to be 512-byte sector-aligned
- `seek(SeekFrom::End(0))` returns 0 for raw block devices; use platform ioctls instead
- The `read_at`/`write_at` methods in volume.rs handle sector alignment transparently

### Title resolution
- Compiled-in `phf::Map<u32, TitleInfo>` of ~5,500 entries, merged at build time from `fatxlib/data/xbox360_titles.json` (AdrianCassar gist) and `fatxlib/data/xbox_originals.tsv` (jeltaqq list). OG name wins on overlap. Conflict report at `target/<…>/title_conflicts.txt`.
- On-demand resolver in `fatxlib::titles::dynamic` parses STFS headers when the catalog misses.
- File-level resolution (`scan_folder_files`) covers Arcade / XNA / Marketplace / Installer content folders.
- Profile gamertag extraction in `fatxlib::xuids::account` decrypts the embedded Account file (ARC4 + HMAC-SHA1) using the public PROD + OTHER keys from py360.
- Caches under `~/.config/xtafkit/` — `user_titles.txt`, `user_files.txt`, `user_profiles.txt`. Plain text, human-editable, self-healing on load.

## Development Workflow

### Building
```bash
cargo build --release
```
Produces `target/release/xtafkit`. All subcommands (browse, ls, scan, mkimage, resolve) live inside it.

### Testing
```bash
cargo test --workspace
```
Library tests in `fatxlib/tests/` exercise the filesystem, title catalog, slot-aware display, STFS parser, and Account decryption. CLI integration tests in `tests/cli_integration.rs` exercise `ls`/`scan`/`mkimage` only.

### Bug-Driven Testing Rule
**Every bug fix MUST include a regression test.** When a bug is found — whether from user reports, logs, or code review — write a test that reproduces the failure BEFORE fixing it, then verify the fix makes it pass. This applies to all crates. No exceptions. Claude should do this automatically without being asked.

Test locations:
- `fatxlib` bugs → an appropriate file under `fatxlib/tests/` (or `#[cfg(test)] mod tests` next to the code)
- CLI / TUI / mkimage bugs → `tests/cli_integration.rs` or `#[cfg(test)] mod tests` near the offending code in `src/`

### Diagnostic example
`fatxlib/examples/check_profile.rs` reads a raw STFS profile package and prints any gamertag the Account decryption manages to extract. Useful when investigating new drives:
```bash
cargo run -p fatxlib --example check_profile -- /path/to/profile-file
```

### Test drive
- 1TB Xbox 360 formatted drive at `/dev/rdisk4` (may change between sessions — verify with `diskutil list`)
- Two XTAF partitions: "360 Game Content" and "360 Data"

## Git Conventions
- **Default branch**: `main`
- Commit and push at each milestone (working feature, major fix, etc.)

## Future Work (Deferred)
- Eager / deferred-sync auto-resolve for files inside STFS content-type folders (Marketplace/Arcade/etc.) — currently on-demand only
- `extract-xiso` integration for on-the-fly ISO extraction during copy
- `iso2god`-style ISO → Games-on-Demand conversion (cherry-picked from iso2god-rs, refactored for streaming)
