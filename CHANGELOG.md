# Changelog

All notable changes to `xtafkit` will be documented in this file.

## [1.3.0] - 2026-05-17

Adds read-extraction for Xbox 360 STFS container packages — Arcade (XBLA), XBLIG, Title Updates, Marketplace DLC. New library surface under `fatxlib::stfs::extract` and a new `xtafkit extract-stfs` CLI subcommand. TUI integration for STFS is intentionally deferred to a later release.

### STFS extraction
- Added `xtafkit extract-stfs <PACKAGE>` for streaming Xbox 360 STFS containers (CON / LIVE / PIRS) to a local directory. `--to <DIR>` overrides the destination; `--dry-run` lists the entries with sizes and totals; `--json` emits machine-readable output for both dry-run and post-extract modes.
- Default destination (no `--to`) is `./<DisplayName>/` taken straight from the STFS header — no catalog lookup, no `[TitleID]` suffix. The per-package `display_name` is consistently more specific than the title-id catalog mapping (notably for XBLIG, where every indie game shares the same system title-id and the catalog can only return a generic bucket name).
- Read-only / type-1 STFS only. Type-0 (read-write, used by save games / on-drive system files / CON packages) surfaces an explicit `"STFS type 0 (read-write) not supported yet"` error rather than producing wrong output.
- Block-index → byte-offset translator handles all interleaved hash levels: L0 every `0xAA` blocks, L1 every `0x70E4`, L2 every `0x4AF768`. Inline boundary tests pin offsets at `0xA9`, `0xAA`, `0x70E3`, `0x70E4` against literal hex values and assert strict monotonicity across boundaries.
- File chain follower covers both the consecutive fast path (no hash-block reads) and the fragmented case (next-block pointer threaded through the L0 hash block at offset `(N % 0xAA) * 24 + 0x15`). Walk is capped at `used_blocks` iterations to reject malformed cyclic chains, mirroring the existing FAT cycle rejection in `volume.rs`.
- Defensive parent-chain resolution in `extract_to_host`: rejects cyclic `parent_index` references and out-of-range parent pointers; refuses to overwrite existing output files.

### Library API additions
- New `fatxlib::stfs` submodules: `volume_descriptor`, `block_translator`, `file_entry`, `extract`. Existing header parsing (`StfsHeader`, `parse_header`, `MIN_HEADER_BYTES`) moved into `fatxlib::stfs::header` and re-exported at the namespace root — no breaking changes for existing callers.
- `fatxlib::stfs::StfsPackage::{open, header, volume, entries, read_block_chain, read_file}` — read API for STFS packages. `read_file<W: Write>` streams through a writer; no full-file buffering even for multi-hundred-MiB packages.
- `fatxlib::stfs::extract::extract_to_host(&mut StfsPackage, &Path, Option<ProgressFn>) -> Result<ExtractReport>` — top-level walk + write, with progress callback shape matching the existing XISO extract (`(rel_path, file_size, total_bytes_so_far)`).
- `fatxlib::stfs::extract::ExtractReport` — `{ files, directories, bytes }` returned on success.

### Testing / maintenance
- 27 inline synthetic tests across the four new STFS submodules: volume-descriptor parsing (type-0/1 detection, wrong-size rejection, truncation), type-1 block translator (boundary indices at every hash level plus monotonicity), file entry parsing (consecutive flag, directory flag, parent-index, non-ASCII tolerance), and end-to-end synthetic package extraction with a nested directory tree.
- v2 TUI extract-gating rule documented in the design spec: the future TUI sniff prompt will offer `(X)tract` only when the package contains a `default.xex` file (the only reliable signal that loose extraction produces something useful for alt-dashboards). Fallback: gate on `content_type == 0x000D0000`. The CLI `extract-stfs` is unrestricted.

## [1.2.1] - 2026-05-17

Maintenance and hardening release on top of 1.2.0. No new features; all changes are bug fixes, internal refactors that reduce drift, and test coverage for areas that previously had none.

### Correctness / hardening
- Fixed `XBE` certificate parsing reading 4 bytes past `dwVersion` — the version field landed in the LAN-key region, so every Original Xbox executable routed through `TitleInfo::from_xbe` was returning a wrong version. Seek width corrected from 164 to 160 bytes.
- Fixed `ConHeaderBuilder::with_game_title` panicking on titles ≥ 64 UTF-16 code units; the UTF-16 writer now caps at the field length (0x80 bytes / 64 units, null included) for both title slots in the CON header.
- Fixed `looks_like_gamertag` rejecting all non-Latin gamertags — Cyrillic, CJK, Greek, etc. now resolve correctly. ASCII-only checks replaced with `is_alphabetic` / `is_alphanumeric`.
- Fixed `pick_profile_name` caching STFS system-package titles ("Xbox 360 Dashboard") as if they were gamertags; candidates now run through `looks_like_gamertag` before being accepted.
- Fixed `seek(SeekFrom::End(0))` silently returning 0 on macOS raw block devices: `partition::detect_xbox_partitions`, `partition::scan_for_fatx`, and `FatxVolume::open` now error explicitly when given a zero `device_size`. `scan_for_fatx` also gained an explicit `device_size: u64` parameter, with the CLI deep-scan caller updated accordingly.
- Fixed FAT16/FAT32 selection drifting from the FATX-specific 65520 (0xFFF0) threshold. `volume.rs` and `mkimage.rs` now use the `FAT16_CLUSTER_THRESHOLD` constant from `fatxlib::types`. Volumes with 65520–65524 clusters were previously mislabeled as FAT16.
- Fixed `FatxVolume::write_file_in_place` not flushing the FAT cache between chain extension (Phase 1) and payload writes (Phase 2); a mid-operation failure could previously leave the directory entry advertising clusters the on-disk FAT had not yet linked.
- Fixed `validate_filename` measuring byte length instead of character count when comparing against the 42-char FATX cap.
- Fixed `FatxVolume::stats` panicking in debug builds (or wrapping in release) when corrupt cached counts produced `free + bad > total`; counts are now `saturating_sub`'d.
- Fixed host-to-FATX directory copies reading entire files into memory; large host-FS uploads now stream cluster-by-cluster through `create_file_from_reader`.
- Fixed the guided picker's stale `sudo fatx` hint — now `sudo xtafkit`.
- Tightened `build.rs` catalog floor checks (`xbox360 > 4000`, `xbox_og > 800`) so a truncated source no longer ships an empty or partial title map. The literal-pairing loop in `write_merged` was simplified to a single pass over the merged map, eliminating a parallel `values()` / `iter()` zip that was correct only by coincidence of `BTreeMap` iteration order.
- Centralized FATX cluster allocation behind two private helpers (`reserve_free_clusters`, `link_allocated_clusters`); previously three near-identical bitmap-walk + chain-linking copies in `allocate_chain`, `write_file_in_place`, and `plan_write_in_place_for_entry` could drift on independent fixes.

### TUI / quality of life
- Installed a panic hook and `Drop`-based terminal guard so a crash inside Ratatui or the background IO worker no longer leaves the shell in raw mode with the alternate screen stuck.
- The UI now detects `TryRecvError::Disconnected` on the IO response channel: if the worker thread dies unexpectedly the UI surfaces the failure and quits cleanly instead of freezing on `is_busy` with no way out.
- Flush errors after destructive operations are no longer silently swallowed. A shared `flush_or_error` helper is wired into `WriteFile`, `CopyDir`, `ExtractXiso`, `ConvertXisoToGod`, `Mkdir`, `Delete`, `Rename`, `Cleanup`, and `Flush` — a failed FAT commit can no longer be reported as "uploaded successfully."
- Single-delete confirmation captures the selection at prompt time via a new `pending_delete` field rather than re-reading the current selection at Enter — closes a race where a background `ListDir` refresh could re-sort entries mid-confirmation and re-target the delete.
- Replaced prompt-string `starts_with` dispatch in `handle_input_key` with an exhaustive `match` on `InputMode`, so every input mode has a single, type-checked completion handler.

### Testing / maintenance
- Pinned `HashList` semantics with direct fixed-byte tests: `read` preserves offsets and zero gaps, `write` emits exactly 4 KiB, and `HashList::new().digest()` matches `sha1(b"\0" * 4096)` byte-for-byte.
- Rewrote the GoD MHT back-chain test to seed via `HashList::read` (not `add_hash`) and assert against literal SHA-1 byte arrays for a three-part conversion, so byte-order or padding regressions in `digest()` and propagation bugs in the back-chain surface immediately.
- Added a `write_file_in_place` FAT-flush regression test that injects post-flush write failures through a new file-backed `FailingWriteFile` wrapper, reopens the image, and asserts the chain extension survived the failure.
- Added FAT16/FAT32 boundary tests at exactly `FAT16_CLUSTER_THRESHOLD` and `THRESHOLD - 1` — `fat_type_for_cluster_estimate` extracted to a private module function so the predicate is unit-testable in isolation.
- Added direct `partition` tests covering both the zero-size error path and a planted-magic happy path on `detect_xbox_partitions` and `scan_for_fatx`.
- Added an XBE certificate regression test that builds a synthetic header and verifies `title_id` and `version` are read from the correct offsets.
- Added a CON header truncation test for `with_game_title` at oversized inputs.
- Added a Cyrillic gamertag case ("Игрок 7") to the `looks_like_gamertag` rules and a `pick_profile_name` test that rejects the overlong "Xbox 360 Dashboard" candidate.
- Moved the volume stats saturation test inline into `volume.rs` so the `force_stats_counts_for_test` public helper could be deleted; `fatxlib/tests/stats.rs` removed.
- Removed redundant scaffolding tests: `fatxlib/tests/fixture_test.rs` (three "did `open()` succeed" checks already covered by every other integration test) and five `optimization.rs` tests that either re-tested already-covered invariants (`test_bitmap_consistent_after_allocations`, `test_bitmap_consistent_after_free_chain`, `test_flush_after_no_changes_is_noop`) or labeled themselves alignment tests while only exercising file-backed (non-aligned) I/O (`test_default_alignment_works`, `test_read_write_at_various_offsets`).
- `tests/cli_integration::test_scan_image` now asserts the command actually succeeded instead of discarding the exit code.


## [1.2.0] - 2026-05-17

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
