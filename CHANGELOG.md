# Changelog

All notable changes to `xtafkit` will be documented in this file.

## [Unreleased]

### ISO / disc-image support
- Added `xtafkit extract` for streaming Xbox / Xbox 360 XISO contents to a local directory, with `$SystemUpdate` skipped by default. Supports `--keep-systemupdate` to override and `--dry-run` to preview file list + byte totals without writing.
- Added `xtafkit god` for XISO → Games-on-Demand conversion. Default trim is `compact`; `preserve-layout` and `none` stay available for debugging and compatibility. Game-title slot in the CON header auto-fills from the bundled catalog; pass `--game-title TITLE` to override.
- TUI upload (`u`) now sniffs every local file and, on XISO detection, prompts **e(X)tract / (G)oD / (R)aw / Esc**. Default flips by cwd context: inside `/Content/<XUID>/` defaults to GoD (BC playback target); everywhere else defaults to Extract (alt-dashboard target).
- Extract destination folder name is resolved from the catalog when the title is known — `disc1.iso` with TitleID `4D5307E6` extracts as `Halo 3/` rather than `disc1/`. Falls back to the file stem on catalog miss. Names are sanitized for FATX (illegal chars replaced with `-`, runs of whitespace collapsed, truncated to 42 bytes).
- Introduced a shared `fatxlib::iso` namespace for image reading, manifest planning, compact repacking, and GoD conversion.
- Reworked compact GoD conversion to stream a virtual dense XDVDFS layout instead of staging a temporary ISO on disk — peak local disk usage during conversion is zero.
- Centralized ISO filtering and planning so extract, compact trim, and dry-run reporting share the same manifest.
- Removed the old public `fatxlib::xiso` and `fatxlib::iso2god` entry points in favor of `fatxlib::iso::{image,manifest,compact,god}`.
- Refactored GoD conversion to share its engine between host-filesystem and FATX-volume targets via an internal `GodSink` trait — one `run_conversion` loop, two sink implementations.

### Performance
- Hot-path SHA-1 in GoD conversion routes through `openssl::sha::sha1` by default (ARMv8 SHA on Apple Silicon, SHA-NI on x86). Gated by the default-on `openssl-hash` cargo feature; disable to fall back to RustCrypto's `sha1` crate with zero system OpenSSL dependency.
- Fixed a double-I/O bug in `write_part`: the upstream implementation read each subpart, hashed it, then `seek_relative`d back and re-read it via `io::copy` to write the part file. Now writes from the buffer it already has, halving I/O on the hot path (~33 % wall-time reduction on large ISOs).
- 1 MiB `BufReader` on the source ISO during the metadata pre-pass cuts syscall tax on multi-GiB inputs.
- Streaming variant of GoD conversion to FATX (`convert_iso_to_fatx`) builds each part in a reused ~163 MiB buffer and streams straight into the volume — no local staging.

### TUI / quality of life
- Mid-conversion `Esc` cancels GoD conversion cleanly (checked between parts and between MHT-chain steps); no partial silent failures.
- Per-part byte-level progress with MiB/s throughput, rate-limited to ~200 ms intervals.
- Upload prompt no longer prefills with the last-used path — always starts blank.
- TUI extract worker skips `$SystemUpdate` and surfaces the skip count + bytes in the completion message.

### Library API additions
- `fatxlib::iso::image::XisoImage::title_info()` parses the embedded `Default.xex` / `default.xbe` and returns the title's execution info. Used by catalog name resolution and by the GoD conversion pipeline.
- `fatxlib::executable` (top-level module) holds `TitleInfo` / `TitleExecutionInfo` and the XEX/XBE parsers — shared between `iso::image` and `iso::god`.
- `fatxlib::volume::FatxVolume::create_file_from_reader` streams a file into FATX cluster-by-cluster from any `Read` source, capping working-set at one cluster regardless of total file size.

## [1.1.0] - 2026-05-16

First release under the `xtafkit` name. Forked from
[joshuareisbord/fatx-rs](https://github.com/joshuareisbord/fatx-rs); the
FATX/XTAF filesystem core traces back to that work. Everything below was
added or rewritten in the fork.

### Project identity
- Crate + binary renamed to `xtafkit` (was `fatx-cli` + `fatx`). Library crate `fatxlib` retained.
- NOTICE rewritten with dual attribution (xtafkit + upstream fatx-rs), both Apache 2.0.
- Cache file paths migrated from `~/.config/fatx-rs/` to `~/.config/xtafkit/`.
- Devcontainer name + workdir, githook header, integration-test header brought in line with the new name.

### Pre-fork work carried in
- NFS performance: extracted common NFS flushing/cache behavior to reduce redundant disk hits on read paths.
- `fatx copy` directory semantics + `create_file` data integrity: regression tests added first, then the fix; plus a `type_complexity` clippy cleanup.
- macOS metadata cleanup, guided mount/browse, dry-run safety for destructive commands, TUI cleanup, ANSI color compatibility.

### Title resolution & folder display
- Compiled-in title catalog of ~5,500 entries, merged at build time from two community sources: [AdrianCassar's Xbox 360 gist](https://gist.github.com/AdrianCassar/c0d05a14608168259232b3ed8c77f28c) and [jeltaqq's Original Xbox list](https://github.com/jeltaqq/Xbox-Original-GameList).
- Smart conflict normalization (whitespace, subtitle markers, etc.). Conflict report written to `target/<…>/title_conflicts.txt` (gitignored), not the build log.
- Slot-aware folder display in `fatxlib::display`: `Xuid` / `TitleId` / `ContentType` / `StfsFile` / `File`. Format: `<name> [<raw>]`, with raw case preserved.
- Content-type folder labels from the free60 STFS spec.
- All-zeros XUID labeled `Shared`.
- TUI keybinding cleanup: `m` is mkdir, `→` enters folders (mirroring `←` for go-up), top-of-file keymap comment regenerated.

### On-demand STFS + profile gamertag extraction
- STFS header parser (`CON ` / `LIVE` / `PIRS`) in `fatxlib::stfs`.
- On-demand title resolution: when the catalog misses, parse the STFS header of a file inside the unresolved title folder.
- Per-file STFS resolution for Arcade (`0x000D0000`), XNA (`0x000E0000`), Marketplace (`0x00000002`), and Installer (`0x000B0000`) content types.
- Profile gamertag extraction: decrypts the embedded Account file (ARC4 + HMAC-SHA1) using the public PROD + OTHER keys to recover the real gamertag from profile XUID folders.
- TUI: `R` keybinding (slot-aware resolve, dispatches to title-resolve / bulk-scan / single-file), `?` marker on unresolvable entries, sort toggle `s` (by name ⇄ by ID, with bracket-order flip).
- Three persistent caches at `~/.config/xtafkit/`: `user_titles.txt`, `user_files.txt`, `user_profiles.txt`. Plain text, human-editable, self-healing on load.
- Diagnostic helper: `cargo run -p fatxlib --example check_profile -- <file>` to inspect gamertag extraction against a raw STFS file.

### NFS hardening
- Fix NFS write recheck-race panic.
- Remove stale path-based write-session API; align tests with entry-based writes.
- Move NFS dirty-buffer seed reads off the async runtime thread to prevent runtime starvation.
- Reject corrupt FAT next pointers and cyclic cluster chains instead of silently reading garbage.
- Reject FATX rename collisions instead of creating duplicate directory entries.
- NFS exclusive create no longer truncates existing files.
- Flush deferred writes by stable file identity rather than stale paths.
- Keep deferred overwrite sessions unpublished until commit; cancel-rollback regression test added.
- Roll back failed directory creates and parent directory expansions.
- `cargo fmt` + `clippy` pass.

### Scope simplification & TUI-first
- Removed the NFS Finder-mount server entirely, along with the catastrophic stale-mount deadlock that haunted it.
- Dropped 10+ CLI subcommands now subsumed by the TUI: `read`, `write`, `mkdir`, `rm`, `rmr`, `rename`, `copy`, `info`, `hexdump`, `cleanup`, `mount`, `shell`.
- Dropped the interactive numbered-menu shell mode; no-args entry point now lands you in the TUI via guided picker.
- New CLI surface: `browse`, `ls`, `scan`, `mkimage`, `resolve` (5 subcommands, down from 15+).
- TTY-aware `ls` output: text when stdout is a terminal, JSON when piped or redirected. New `--text` / `--json` flags force either mode.
- Auto-pick single disk in the guided no-args flow — if exactly one external disk is detected, skip the picker and use it directly.
- ~3,000 LOC removed, 10 runtime dependencies dropped (including `nfsserve`, `tokio`, `async-trait`, `parking_lot`, `quick_cache`, `bytes`, `ctrlc`, `core-foundation`, `core-foundation-sys`, `io-kit-sys`, `mach2`).

### Toolchain & dependencies
- Rust edition bumped from 2021 to 2024.
- Major dependency bumps: `rand` 0.8 → 0.10, `nix` 0.29 → 0.31, `phf` 0.11 → 0.13, `phf_codegen` 0.11 → 0.13, `hmac` 0.12 → 0.13, `sha1` 0.10 → 0.11. Smaller bumps via `cargo update`.

### Infrastructure
- macOS release pipeline: GitHub Actions builds `x86_64-apple-darwin` and `aarch64-apple-darwin` binaries on tag push, generates a draft release whose notes are sourced from this changelog.
