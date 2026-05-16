# xtafkit

[![License](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](LICENSE)

Mac-native TUI workbench for Xbox 360 (XTAF) and original Xbox (FATX) drives. Plug a console drive into your Mac over USB; browse, transfer files, resolve game titles, decode profile gamertags, and (optionally) mount the volume in Finder via a local NFS server.

## Highlights

- **TUI file browser** with slot-aware folder labels — game titles, content type categories, and gamertags shown next to raw IDs
- **Title resolution** — ~5,500 Xbox 360 and Original Xbox games compiled in, with on-demand STFS header parsing for the long tail
- **Profile gamertag extraction** — decrypts the embedded Account file (ARC4 + HMAC-SHA1) to label profile XUIDs
- **Per-file resolution** in Arcade / XNA / Marketplace / Installer folders — bulk-scan with one keystroke
- **Sort toggle** — by resolved name or by raw ID (display format flips to match)
- **Persistent caches** under `~/.config/xtafkit/` — plain text, human-editable
- **Finder mount** via a local NFSv3 server, with clean shutdown on Ctrl+C
- **JSON output** for scripting and agentic workflows
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

Produces a single binary: `target/release/xtafkit`. All functionality is accessed through subcommands.

## Quick start

```bash
# Find your Xbox drive
diskutil list | grep external

# Unmount macOS's hold on it
diskutil unmountDisk /dev/diskN

# Scan for Xbox partitions
sudo xtafkit scan /dev/rdiskN

# List files
sudo xtafkit ls /dev/rdiskN --partition "360 Data" /Content

# Interactive guided mode (prompts for everything)
sudo xtafkit
```

## TUI

```bash
# Guided mode — detects disks and partitions automatically
sudo xtafkit browse

# Browse a specific partition
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
| `d` / `u` | Download / upload |
| `D` / `r` | Delete / rename |
| `i` | Volume info |
| `c` | Clean up macOS metadata |
| `Esc` / `q` | Cancel running operation / quit |

Entries that can be resolved show a `?` marker. Pressing `R` triggers an STFS header read; results are cached under `~/.config/xtafkit/` and persist across runs.

## Finder mount

```bash
# Guided mode
sudo xtafkit mount

# Mount a specific partition
sudo xtafkit mount /dev/rdisk4 --partition "360 Data" -v --mount

# Start NFS server only (no Finder mount)
sudo xtafkit mount /dev/rdisk4 --partition "360 Data" -v

# Emergency cleanup if a previous mount went stale
sudo xtafkit mount --cleanup
```

The mount uses a local NFSv3 server with `soft,intr` options. Ctrl+C cleanly unmounts before exiting.

## Common subcommands

```bash
sudo xtafkit ls       /dev/rdisk4 --partition "360 Data" /Content -l
sudo xtafkit read     /dev/rdisk4 --partition "360 Data" /name.txt -o name.txt
sudo xtafkit write    /dev/rdisk4 --partition "360 Data" /hello.txt -i hello.txt
sudo xtafkit mkdir    /dev/rdisk4 --partition "360 Data" /MyFolder
sudo xtafkit rm       /dev/rdisk4 --partition "360 Data" /hello.txt
sudo xtafkit rmr      /dev/rdisk4 --partition "360 Data" /Content
sudo xtafkit copy     /dev/rdisk4 --partition "360 Data" --from ./local --to /DestFolder
sudo xtafkit rename   /dev/rdisk4 --partition "360 Data" /old.txt new.txt
sudo xtafkit info     /dev/rdisk4 --partition "360 Data"
sudo xtafkit cleanup  /dev/rdisk4 --partition "360 Data" --dry-run
sudo xtafkit hexdump  /dev/rdisk4 --offset 0x80080000 --count 512
sudo xtafkit resolve  /dev/rdisk4 --partition "360 Data" /Content/0000000000000000/4D5307E6
```

`xtafkit resolve` auto-dispatches based on the target:
- A title-ID folder → resolves the title name from the first STFS file inside.
- A content-type folder holding standalone STFS files (Arcade / XNA / Marketplace / Installer) → bulk-scans every file.
- A single STFS file → resolves that one file.

## JSON output

Add `--json` to most commands for machine-readable output:

```bash
sudo xtafkit ls   /dev/rdisk4 --partition "360 Data" /Content --json
sudo xtafkit info /dev/rdisk4 --partition "360 Data" --json
```

JSON keeps raw on-disk names — the human-display formatting only happens on text output.

## Create test images

Generate FATX/XTAF disk images for testing without hardware:

```bash
xtafkit mkimage test.img      --size 1G  --populate            # FATX with sample content
xtafkit mkimage xbox360.img   --size 512M --format xtaf        # Xbox 360 format
xtafkit mkimage test.img      --size 1G  --force               # overwrite existing
```

## Project structure

Cargo workspace with two crates:

| Crate | Binary | Purpose |
|---|---|---|
| `fatxlib` | — | FATX/XTAF filesystem library: volume I/O, FAT operations, partition detection, title catalog, STFS parser, profile decryption |
| `xtafkit` (root) | `xtafkit` | CLI, TUI browser, NFS mount server, image generator |

## macOS notes

- Use `/dev/rdiskN` (raw device), **not** `/dev/diskN` — raw is significantly faster.
- `sudo` is required for raw device access and mounting.
- Find your drive: `diskutil list`. Unmount macOS first: `diskutil unmountDisk /dev/diskN`.
- The NFS mount has one known catastrophic failure mode (stale-mount deadlock that requires a reboot) if the server dies while a Finder window is open — auto-mount is off by default for this reason. See CLAUDE.md for details.

## Testing

```bash
cargo test --workspace
```

Tests use file-backed FATX/XTAF images generated by `xtafkit mkimage`. No hardware required.

## Origin

`xtafkit` started as a fork of [joshuareisbord/fatx-rs](https://github.com/joshuareisbord/fatx-rs), which provided the FATX/XTAF filesystem core. The project has since diverged significantly — title catalog with merged Xbox 360 + Original Xbox sources, on-demand STFS resolution, slot-aware folder display, profile gamertag decryption, content-type-aware browsing, NFS hardening, and TUI workflow polish — and is maintained independently as a personal variant. Credit to the original author for the filesystem foundation.

## License

Licensed under the Apache License, Version 2.0. See [LICENSE](LICENSE) for the full text and [NOTICE](NOTICE) for attribution requirements.
