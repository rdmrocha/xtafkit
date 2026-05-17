# xtafkit

[![License](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](LICENSE)

Mac-native TUI workbench for Xbox 360 (XTAF) and Original Xbox (FATX) drives. Plug a console drive into your Mac over USB, then browse, transfer files, resolve game titles, decode profile gamertags, and work with XISO disc images from a polished terminal UI plus a small CLI surface.

## Highlights

- **TUI file browser** with slot-aware folder labels — game titles, content-type categories, and gamertags shown next to raw IDs
- **Title resolution** — ~5,500 Xbox 360 and Original Xbox games compiled in, with on-demand STFS header parsing for anything the catalog misses
- **Profile gamertag extraction** — decrypts the embedded Account file (ARC4 + HMAC-SHA1) to label profile XUIDs
- **Per-file resolution** for Arcade / XNA / Marketplace / Installer folders, with one-keystroke bulk scan
- **XISO extraction and GoD conversion** — stream XISO contents to a local directory or build Games-on-Demand packages for Xbox 360 backward-compatible titles
- **Sort toggle** — by resolved name or by raw ID (display format flips to match)
- **Persistent caches** under `~/.config/xtafkit/` — plain text, human-editable
- macOS-native I/O (`F_NOCACHE`, `F_RDAHEAD`, device-optimal alignment)
- File-backed test image generator for development without hardware

## Supported formats

| Format | Console | Endianness | Auto-detected |
|---|---|---|---|
| **FATX** | Original Xbox | Little-endian | Yes |
| **XTAF** | Xbox 360 | Big-endian | Yes |

Both FAT16 and FAT32 entry sizes are handled automatically based on partition size.

## Install

```bash
git clone https://github.com/rdmrocha/xtafkit.git
cd xtafkit
cargo build --release
```

Produces a single binary: `target/release/xtafkit`.

The default build links against the system OpenSSL for hardware-accelerated SHA-1 during GoD conversion. On macOS install via Homebrew (`brew install openssl@3`); on Debian/Ubuntu install `libssl-dev`. To skip the OpenSSL dependency entirely and fall back to portable Rust SHA-1, build with `cargo build --release --no-default-features`.

## Quick start

```bash
# Find your Xbox drive
diskutil list | grep external

# Unmount macOS's hold on it
diskutil unmountDisk /dev/diskN

# Scan for Xbox partitions
sudo xtafkit scan /dev/rdiskN

# Launch the TUI (guided picker — detects disks automatically)
sudo xtafkit
```

## Commands

```
xtafkit                                       launch TUI (guided picker)
xtafkit browse [DEVICE] [--partition NAME]    launch TUI on a known device
xtafkit ls <DEVICE> [PATH] [-l]               list files (text in TTY, JSON when piped)
xtafkit scan <DEVICE> [--deep]                detect FATX/XTAF partitions
xtafkit mkimage <PATH> [--size 1G] [--populate] [--format fatx|xtaf]
xtafkit resolve <DEVICE> <PATH>               STFS-based title / file resolution
xtafkit extract <ISO> <DEST> [--keep-systemupdate] [--dry-run]
xtafkit god <ISO> <DEST> [--trim compact|preserve-layout|none] [--dry-run] [--game-title TITLE]
```

Seven subcommands total — file operations (download/upload/mkdir/rm/rename/copy/info/cleanup) live inside the TUI.

## XISO Tools

`xtafkit extract` streams every file from an XISO to a local directory and skips `$SystemUpdate` by default. `xtafkit god` converts an XISO into a Games-on-Demand package; the default trim mode is `compact`, which repacks XDVDFS densely before GoD packaging. Pass `--trim preserve-layout` to keep mastered holes, or `--trim none` to use the full data partition.

## TUI

```bash
# Guided
sudo xtafkit

# Direct
sudo xtafkit browse /dev/rdisk4 --partition "360 Data"
```

### Keybindings

| Key | Action |
|---|---|
| `↑` / `k` / `↓` / `j` | Navigate |
| `Enter` / `→` | Open directory / show file info |
| `Backspace` / `←` | Go up |
| `R` | Resolve title or bulk-scan files (slot-aware) |
| `s` | Toggle sort: by name ⇄ by ID (flips bracket order) |
| `m` | Create directory |
| `d` / `u` | Download / upload (XISO uploads prompt for e**(X)**tract / **(G)**oD / **(R)**aw — see below) |
| `D` / `r` | Delete / rename |
| `i` | Volume info |
| `c` | Clean up macOS metadata |
| `Esc` / `q` | Cancel running operation / quit |

Entries that can be resolved show a `?` marker. Resolution results are cached under `~/.config/xtafkit/` and persist across runs.

### Uploading an XISO

When the file you point at is an Xbox / Xbox 360 disc image (XDVDFS volume detected automatically), the upload prompt becomes:

```
Detected XISO 'Halo.iso'. e(X)tract / (G)oD / (R)aw / Esc:
```

| Choice | Result |
|---|---|
| **(X)tract** | Walks the XISO and writes each file into `<cwd>/<name>/` on the drive. `$SystemUpdate` is skipped automatically. `<name>` is the catalog-known game title when available, otherwise the local filename stem. Best for alt dashboards (Aurora / FreeStyle / XBMC4XBOX) that launch loose `default.xex` / `default.xbe` directly. |
| **(G)oD** | Streams a Games-on-Demand package into `<cwd>/<TitleID>/00007000/<MediaID>{,.data/}`. Uses the compact trim by default so the output is sized to actual content, not the original mastered layout. Required for stock Xbox 360 backward-compatibility playback. |
| **(R)aw** | Plain byte-for-byte copy of the source ISO file. |

The default action (the capitalized letter) flips by context: inside `/Content/<XUID>/` the default is **G** (where the dashboard looks for BC packages); everywhere else the default is **X**.

Press `Esc` to cancel mid-conversion at any time — the worker checks between parts and between hash-tree steps.

## `xtafkit resolve`

Auto-dispatches by what you point at:

- A title-ID folder (`/Content/<XUID>/<TitleID>`) → resolves the title name from the first STFS file inside.
- A content-type folder holding standalone STFS files (Arcade / XNA / Marketplace / Installer) → bulk-scans every file.
- A single STFS file → resolves that one file.

## Output format

`xtafkit ls` auto-detects: human-readable text when stdout is a terminal, JSON when stdout is piped or redirected. Force either with `--text` or `--json`:

```bash
# Text in your terminal:
sudo xtafkit ls /dev/rdisk4 --partition "360 Data" /Content

# JSON for scripts (auto-detected — no flag needed):
sudo xtafkit ls /dev/rdisk4 --partition "360 Data" /Content | jq '.[] | .name'

# Force one or the other:
sudo xtafkit --json ls /dev/rdisk4 --partition "360 Data" /Content
sudo xtafkit --text ls /dev/rdisk4 --partition "360 Data" /Content > listing.txt
```

JSON output preserves raw on-disk names (slot-aware formatting only happens on text output).

## Project structure

Cargo workspace with two crates:

| Crate | Binary | Purpose |
|---|---|---|
| `fatxlib` | — | FATX/XTAF filesystem library: volume I/O, FAT operations, partition detection, title catalog, STFS parser, profile decryption |
| `xtafkit` (root) | `xtafkit` | CLI + TUI browser + image generator |

## macOS notes

- Use `/dev/rdiskN` (raw device), **not** `/dev/diskN` — raw is significantly faster.
- `sudo` is required for raw device access.
- Find your drive: `diskutil list`. Unmount macOS first: `diskutil unmountDisk /dev/diskN`.

## Testing

```bash
cargo test --workspace
```

Tests use file-backed FATX/XTAF images generated by `xtafkit mkimage`. No hardware required.

## Origin

`xtafkit` started as a fork of [joshuareisbord/fatx-rs](https://github.com/joshuareisbord/fatx-rs), which provided the FATX/XTAF filesystem core. The project has since diverged — title catalog with merged Xbox 360 + Original Xbox sources, on-demand STFS resolution, profile gamertag decryption, slot-aware folder display, TUI-first workflow, XISO extraction, and Games-on-Demand conversion. Credit to the original author for the filesystem foundation.

## License

Licensed under the Apache License, Version 2.0. See [LICENSE](LICENSE) for the full text and [NOTICE](NOTICE) for attribution requirements.
