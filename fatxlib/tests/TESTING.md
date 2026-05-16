# Testing xtafkit

xtafkit has two levels of tests: unit/integration tests that run anywhere, and hardware tests that require a real Xbox 360 formatted drive.

## Unit and Integration Tests

These use in-memory FATX images and don't need any hardware. Run them with:

```bash
cargo test --workspace
```

## Hardware Tests

Hardware tests verify xtafkit against a real XTAF (Xbox 360) formatted drive. They require two things: reference data downloaded from the Xbox, and the drive connected to your Mac.

### Why Reference Data?

The Xbox 360 is the only authoritative source for what's on the drive. xtafkit reads raw disk sectors, so to verify it's reading correctly, you need to independently confirm what files exist, their sizes, timestamps, and content. The Xbox's FTP server provides that ground truth.

Reference data is not checked into the repo because it contains files from a specific drive and will differ for every user.

### Step 1: Collect Reference Data

You need the drive installed in a running Xbox 360 with a custom dashboard (Aurora, FreeStyle, etc.) that exposes an FTP server.

1. Put the drive in your Xbox 360 and boot it
2. Find the console's IP address (check your router's DHCP table, or look in Aurora's network settings)
3. Connect with any FTP client from your Mac:
   ```bash
   ftp <xbox-ip>
   # Username: xboxftp
   # Password: 123456
   ```
   Or use a GUI client like Cyberduck or FileZilla with the same credentials on port 21.

4. Browse `/Hdd1/` — this is the root of the "360 Data" XTAF partition, the main data partition on the drive. Record:
   - The full directory tree (all files and folders, recursively)
   - File sizes
   - Timestamps (the FTP server reports local time; XTAF stores UTC)
   - Download a few small files to use as byte-for-byte comparisons

5. Save your reference data to `.tmp/reference-files/` in the repo root (this directory is gitignored). For example:
   ```
   .tmp/
   ├── REFERENCE_DATA.md       # Your notes: tree, sizes, timestamps
   └── reference-files/
       ├── name.txt             # /Hdd1/name.txt (drive display name)
       ├── config.txt           # Any small text file from the drive
       └── ...                  # Other files you want to compare
   ```

### Step 2: Connect the Drive

1. Move the drive from the Xbox to your Mac via USB
2. Identify the device:
   ```bash
   diskutil list
   ```
   Look for the 1TB (or whatever size) disk that is not your system drive. It will show as unformatted since macOS doesn't understand XTAF. Note the disk number (e.g., `disk4`).

3. Verify xtafkit can see the partitions:
   ```bash
   sudo ./target/release/xtafkit scan /dev/rdisk4
   ```
   You should see entries for "360 Game Content" and "360 Data" with `XTAF` magic. Use `/dev/rdiskN` (raw device), not `/dev/diskN`.

### Step 3: Run Comparisons

Compare xtafkit's listing against your reference data. When piped, `ls` auto-emits JSON for easy parsing — no flag needed:

```bash
# List root directory — compare names, sizes, timestamps against FTP listing
sudo ./target/release/xtafkit ls --partition "360 Data" /dev/rdisk4 / | jq

# Deep traversal — verify subdirectories match
sudo ./target/release/xtafkit ls --partition "360 Data" /dev/rdisk4 /Apps/XeXMenu | jq
```

For file content comparisons (read, write, info, hexdump and similar one-off ops are now TUI-only — those used to be CLI subcommands but were dropped in the simplification pass). To compare bytes, launch the TUI, download via the `d` key, then `diff` against your reference copy:

```bash
sudo ./target/release/xtafkit browse /dev/rdisk4 --partition "360 Data"
# Navigate to /name.txt, press d, save to /tmp/xtaf_name.txt
diff /tmp/xtaf_name.txt .tmp/reference-files/name.txt
```

### Timestamp Notes

The Xbox 360 FTP server displays timestamps in your console's local timezone. XTAF stores timestamps in UTC using standard FAT date/time encoding. When comparing, account for the timezone offset. For example, if your Xbox is set to PDT (UTC-7):

- FTP shows: `Apr 06 20:32`
- xtafkit shows: `2026-04-07 03:32:14` (UTC)
- These are the same moment in time

### Partition Notes

A typical Xbox 360 formatted drive has two XTAF partitions:

- **360 Game Content** (offset 0x80080000, ~2.5 GB) — Installed games and DLC. May be empty if no games are installed.
- **360 Data** (offset 0x130EB0000, remainder of drive) — User data, apps, profiles, content. This is where `/Hdd1/` maps to via FTP.

Several other partition slots exist (Config, Cache, System, etc.) but may not have valid XTAF headers depending on how the drive was formatted.

### Agent (Optional)

If you're working with Claude in a Cowork session, the file-based agent (`.agent/agent.sh`) can automate drive access from the sandbox. Run it on your Mac with `sudo bash .agent/agent.sh` and it will watch for commands. This is not required for manual testing — it's a convenience for AI-assisted development sessions.
