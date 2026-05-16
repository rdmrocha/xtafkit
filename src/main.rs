//! fatx: Command-line tool for interacting with Xbox FATX file systems.
//!
//! Run with no arguments for interactive mode, or use subcommands directly:
//!   fatx                     # Interactive guided mode
//!   fatx browse /dev/rdisk4  # TUI file browser
//!   fatx scan /dev/rdisk4
//!   fatx ls /dev/rdisk4 --partition "Data (E)" /

mod mkimage;
mod mount;
mod tui;

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, Read as IoRead, Seek, SeekFrom, Write as IoWrite};
use std::path::PathBuf;
use std::process::{self, Command};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use base64::Engine;
use clap::{Parser, Subcommand};
use fatxlib::error::FatxError;
use fatxlib::partition::{detect_xbox_partitions, format_size, DetectedPartition};
use fatxlib::types::FileAttributes;
use fatxlib::volume::FatxVolume;
use serde::Serialize;

/// Get the size of a device, handling macOS raw block devices correctly.
fn get_device_size(file: &mut File) -> u64 {
    // Try seek first (works for regular files / disk images)
    if let Ok(size) = file.seek(SeekFrom::End(0)) {
        if size > 0 {
            let _ = file.seek(SeekFrom::Start(0));
            return size;
        }
    }

    // On macOS, raw devices need ioctl
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::io::AsRawFd;
        if let Some(size) = fatxlib::platform::get_block_device_size(file.as_raw_fd()) {
            let _ = file.seek(SeekFrom::Start(0));
            return size;
        }
    }

    // Fallback
    let _ = file.seek(SeekFrom::Start(0));
    0
}

// ===========================================================================
// CLI definition
// ===========================================================================

#[derive(Parser)]
#[command(
    name = "fatx",
    about = "Read and write Xbox FATX file systems on macOS",
    version,
    long_about = "A command-line tool for interacting with FATX-formatted drives and disk images.\n\n\
                   Run with no arguments for interactive guided mode.\n\n\
                   FATX is the filesystem used by the original Xbox console. This tool lets you\n\
                   browse, extract, and modify files on FATX volumes from macOS."
)]
struct Cli {
    /// Enable verbose debug output (shows all I/O, FAT lookups, partition probing, etc.)
    #[arg(short = 'v', long, global = true)]
    verbose: bool,

    /// Output results as JSON (for programmatic use / MCP integration)
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

// ---------------------------------------------------------------------------
// JSON output types
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct JsonPartition {
    name: String,
    offset: u64,
    offset_hex: String,
    size: u64,
    size_human: String,
    has_valid_magic: bool,
    magic: String,
    generation: String,
}

#[derive(Serialize)]
struct JsonDirEntry {
    name: String,
    is_directory: bool,
    size: u64,
    attributes: String,
    first_cluster: u32,
    created: String,
    modified: String,
    accessed: String,
}

#[derive(Serialize)]
struct JsonVolumeInfo {
    volume_id: String,
    fat_type: String,
    cluster_size: u64,
    cluster_size_human: String,
    total_clusters: u32,
    used_clusters: u32,
    free_clusters: u32,
    bad_clusters: u32,
    total_size: u64,
    used_size: u64,
    free_size: u64,
    total_size_human: String,
    used_size_human: String,
    free_size_human: String,
}

#[derive(Serialize)]
struct JsonHexdump {
    offset: u64,
    offset_hex: String,
    count: usize,
    data_base64: String,
    data_hex: String,
}

#[derive(Serialize)]
struct JsonFileContent {
    path: String,
    size: usize,
    data_base64: String,
}

#[derive(Serialize)]
struct JsonSuccess {
    success: bool,
    message: String,
}

#[derive(Subcommand)]
enum Commands {
    /// Interactive TUI file browser — navigate, download, and upload files
    Browse {
        /// Device or disk image (omit for guided selection)
        device: Option<PathBuf>,
        #[arg(long, value_parser = parse_hex_or_dec, default_value = "0")]
        offset: u64,
        #[arg(long, value_parser = parse_hex_or_dec, default_value = "0")]
        size: u64,
        #[arg(long)]
        partition: Option<String>,
    },
    /// Scan a device for FATX partitions at standard Xbox offsets
    Scan {
        device: PathBuf,
        #[arg(long)]
        deep: bool,
        #[arg(long, default_value = "0x20000000", value_parser = parse_hex_or_dec)]
        deep_limit: u64,
    },
    /// List files and directories
    Ls {
        device: PathBuf,
        #[arg(default_value = "/")]
        path: String,
        #[arg(long, value_parser = parse_hex_or_dec, default_value = "0")]
        offset: u64,
        #[arg(long, value_parser = parse_hex_or_dec, default_value = "0")]
        size: u64,
        #[arg(long)]
        partition: Option<String>,
        #[arg(short, long)]
        long: bool,
    },
    /// Read / extract a file from the FATX volume
    Read {
        device: PathBuf,
        path: String,
        #[arg(short, long)]
        output: Option<PathBuf>,
        #[arg(long, value_parser = parse_hex_or_dec, default_value = "0")]
        offset: u64,
        #[arg(long, value_parser = parse_hex_or_dec, default_value = "0")]
        size: u64,
        #[arg(long)]
        partition: Option<String>,
    },
    /// Write a local file into the FATX volume
    Write {
        device: PathBuf,
        path: String,
        #[arg(short, long)]
        input: PathBuf,
        #[arg(long, value_parser = parse_hex_or_dec, default_value = "0")]
        offset: u64,
        #[arg(long, value_parser = parse_hex_or_dec, default_value = "0")]
        size: u64,
        #[arg(long)]
        partition: Option<String>,
    },
    /// Create a directory on the FATX volume
    Mkdir {
        device: PathBuf,
        path: String,
        #[arg(long, value_parser = parse_hex_or_dec, default_value = "0")]
        offset: u64,
        #[arg(long, value_parser = parse_hex_or_dec, default_value = "0")]
        size: u64,
        #[arg(long)]
        partition: Option<String>,
    },
    /// Delete a file or directory (-r for recursive)
    Rm {
        device: PathBuf,
        path: String,
        /// Recursive — delete directory and all its contents
        #[arg(short, long)]
        recursive: bool,
        #[arg(long, value_parser = parse_hex_or_dec, default_value = "0")]
        offset: u64,
        #[arg(long, value_parser = parse_hex_or_dec, default_value = "0")]
        size: u64,
        #[arg(long)]
        partition: Option<String>,
    },
    /// Rename a file or directory
    Rename {
        device: PathBuf,
        path: String,
        new_name: String,
        #[arg(long, value_parser = parse_hex_or_dec, default_value = "0")]
        offset: u64,
        #[arg(long, value_parser = parse_hex_or_dec, default_value = "0")]
        size: u64,
        #[arg(long)]
        partition: Option<String>,
    },
    /// Show volume information and usage statistics
    Info {
        device: PathBuf,
        #[arg(long, value_parser = parse_hex_or_dec, default_value = "0")]
        offset: u64,
        #[arg(long, value_parser = parse_hex_or_dec, default_value = "0")]
        size: u64,
        #[arg(long)]
        partition: Option<String>,
    },
    /// Recursively copy a local directory into the FATX volume
    Copy {
        device: PathBuf,
        /// Local source directory to copy from
        #[arg(long, short = 'i')]
        from: PathBuf,
        /// Destination path on the FATX volume
        #[arg(long, short = 'o')]
        to: String,
        #[arg(long, value_parser = parse_hex_or_dec, default_value = "0")]
        offset: u64,
        #[arg(long, value_parser = parse_hex_or_dec, default_value = "0")]
        size: u64,
        #[arg(long)]
        partition: Option<String>,
    },
    /// Recursively delete (alias for rm -r, kept for backwards compatibility)
    #[command(hide = true)]
    Rmr {
        device: PathBuf,
        path: String,
        #[arg(long, value_parser = parse_hex_or_dec, default_value = "0")]
        offset: u64,
        #[arg(long, value_parser = parse_hex_or_dec, default_value = "0")]
        size: u64,
        #[arg(long)]
        partition: Option<String>,
    },
    /// Print a hex dump at a given offset (debugging)
    Hexdump {
        device: PathBuf,
        #[arg(long, value_parser = parse_hex_or_dec, default_value = "0")]
        offset: u64,
        #[arg(short, long, default_value = "512")]
        count: usize,
    },
    /// Remove macOS metadata files (.DS_Store, ._ files, etc.) from the volume
    Cleanup {
        device: PathBuf,
        /// Show what would be deleted without actually deleting
        #[arg(long)]
        dry_run: bool,
        #[arg(long, value_parser = parse_hex_or_dec, default_value = "0")]
        offset: u64,
        #[arg(long, value_parser = parse_hex_or_dec, default_value = "0")]
        size: u64,
        #[arg(long)]
        partition: Option<String>,
    },
    /// Mount a FATX volume via NFS server (shows in Finder)
    Mount(mount::MountArgs),
    /// Create a blank FATX/XTAF disk image for testing
    Mkimage(mkimage::MkimageArgs),
    /// Resolve a title-ID folder's name by parsing the STFS header inside,
    /// caching the result for future runs
    Resolve {
        device: PathBuf,
        /// Path to a `Content/<XUID>/<TitleID>` folder to resolve.
        path: String,
        #[arg(long, value_parser = parse_hex_or_dec, default_value = "0")]
        offset: u64,
        #[arg(long, value_parser = parse_hex_or_dec, default_value = "0")]
        size: u64,
        #[arg(long)]
        partition: Option<String>,
        /// Skip writing the resolved name to the user cache file.
        #[arg(long)]
        no_save: bool,
    },
}

// ===========================================================================
// Interactive mode
// ===========================================================================

/// Result of the guided partition selection flow.
struct SelectedPartition {
    device_path: PathBuf,
    partition_name: String,
    offset: u64,
    size: u64,
}

/// Interactive device detection + partition scanning + selection.
/// Used by both `interactive_mode` and guided `fatx mount`.
fn guided_partition_selection() -> Option<SelectedPartition> {
    // Check for sudo
    if !running_as_root() {
        println!("[!] You're not running as root. Raw device access requires sudo.");
        println!("    Re-run with: sudo fatx");
        println!();
        print!("Continue anyway? (y/n): ");
        io::stdout().flush().unwrap();
        let ans = read_line();
        if !ans.starts_with('y') && !ans.starts_with('Y') {
            println!("Exiting.");
            return None;
        }
        println!();
    }

    // Detect disks
    println!("[1/3] Detecting available disks...\n");
    let disks = detect_macos_disks();
    if disks.is_empty() {
        println!("No external disks detected.");
        println!("You can also enter a path to a disk image file.\n");
    } else {
        println!("Available disks:");
        for (i, disk) in disks.iter().enumerate() {
            println!("  {}) {}", i + 1, disk);
        }
        println!(
            "  {}) Enter a custom path (device or image file)",
            disks.len() + 1
        );
        println!();
    }

    let device_path = loop {
        print!("Select a disk [1-{}]: ", disks.len() + 1);
        io::stdout().flush().unwrap();
        let input = read_line();
        if let Ok(n) = input.parse::<usize>() {
            if n >= 1 && n <= disks.len() {
                let path = &disks[n - 1];
                let raw = path.replace("/dev/disk", "/dev/rdisk");
                break PathBuf::from(raw);
            } else if n == disks.len() + 1 {
                print!("Enter device or image path: ");
                io::stdout().flush().unwrap();
                let p = read_line();
                break PathBuf::from(p);
            }
        }
        println!("Invalid selection, try again.");
    };

    println!("\nUsing device: {}\n", device_path.display());

    // Unmount if it's a real device
    if device_path.to_string_lossy().contains("/dev/") {
        let disk_path = device_path
            .to_string_lossy()
            .replace("/dev/rdisk", "/dev/disk");
        println!("[2/3] Unmounting {}...", disk_path);
        let status = Command::new("diskutil")
            .args(["unmountDisk", &disk_path])
            .status();
        match status {
            Ok(s) if s.success() => println!("  Unmounted successfully.\n"),
            Ok(_) => println!("  Warning: unmount may have failed. Continuing anyway.\n"),
            Err(e) => println!("  Could not run diskutil: {}. Continuing anyway.\n", e),
        }
    } else {
        println!("[2/3] Skipping unmount (not a block device).\n");
    }

    // Scan for FATX partitions
    println!("[3/3] Scanning for FATX partitions...\n");

    let mut file = match OpenOptions::new().read(true).write(true).open(&device_path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("Error opening '{}': {}", device_path.display(), e);
            if e.kind() == io::ErrorKind::NotFound {
                eprintln!("Device not found. Run 'diskutil list' to find your Xbox drive.");
                eprintln!("Look for an unrecognized disk and use /dev/rdiskN (raw device).");
            } else if e.kind() == io::ErrorKind::PermissionDenied {
                eprintln!("Hint: Run with sudo for raw device access.");
            }
            return None;
        }
    };

    let direct_fatx = {
        let mut magic = [0u8; 4];
        file.seek(SeekFrom::Start(0)).ok();
        file.read_exact(&mut magic).ok();
        fatxlib::types::is_valid_magic(&magic)
    };

    let dev_size_for_direct = get_device_size(&mut file);
    let partitions: Vec<DetectedPartition> = if direct_fatx {
        println!("  Found FATX/XTAF volume directly at start of device.\n");
        vec![DetectedPartition {
            name: "Whole Device".to_string(),
            offset: 0,
            size: dev_size_for_direct,
            has_valid_magic: true,
            magic: "auto".to_string(),
            generation: fatxlib::types::XboxGeneration::Xbox360,
        }]
    } else {
        let dev_size = get_device_size(&mut file);
        match detect_xbox_partitions(&mut file, dev_size) {
            Ok(parts) => {
                let valid: Vec<_> = parts.into_iter().filter(|p| p.has_valid_magic).collect();
                if valid.is_empty() {
                    println!("  No FATX/XTAF partitions found at known Xbox or Xbox 360 offsets.");
                    println!(
                        "  You can try a deep scan with: fatx scan {} --deep\n",
                        device_path.display()
                    );
                    return None;
                }
                valid
            }
            Err(e) => {
                eprintln!("  Error scanning: {}", e);
                return None;
            }
        }
    };

    println!("Found {} partition(s):", partitions.len());
    for (i, p) in partitions.iter().enumerate() {
        println!(
            "  {}) {} [{}] — offset 0x{:010X}, size {}, {}",
            i + 1,
            p.name,
            p.magic,
            p.offset,
            format_size(p.size),
            p.generation,
        );
    }
    println!();

    let selected = if partitions.len() == 1 {
        println!("Auto-selecting the only partition.\n");
        &partitions[0]
    } else {
        loop {
            print!("Select partition [1-{}]: ", partitions.len());
            io::stdout().flush().unwrap();
            let input = read_line();
            if let Ok(n) = input.parse::<usize>() {
                if n >= 1 && n <= partitions.len() {
                    break &partitions[n - 1];
                }
            }
            println!("Invalid selection.");
        }
    };

    Some(SelectedPartition {
        device_path,
        partition_name: selected.name.clone(),
        offset: selected.offset,
        size: selected.size,
    })
}

fn interactive_mode() {
    println!();
    println!("========================================");
    println!("  fatx — Xbox FATX filesystem tool  ");
    println!("========================================");
    println!();

    let sel = match guided_partition_selection() {
        Some(s) => s,
        None => return,
    };

    let part_offset = sel.offset;
    let part_size = sel.size;
    let device_path = &sel.device_path;

    // Open the volume
    let file = match OpenOptions::new().read(true).write(true).open(device_path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("Error reopening device: {}", e);
            return;
        }
    };

    // Capture raw fd before file is moved into the volume
    #[cfg(target_os = "macos")]
    let raw_fd = {
        use std::os::unix::io::AsRawFd;
        file.as_raw_fd()
    };

    let mut vol = match FatxVolume::open(file, part_offset, part_size) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("Error opening FATX volume: {}", e);
            return;
        }
    };

    // Configure macOS-specific I/O (F_NOCACHE, F_RDAHEAD, device params)
    #[cfg(target_os = "macos")]
    vol.configure_device(raw_fd);

    println!(
        "Volume opened: {} ({}, {})",
        sel.partition_name,
        vol.superblock.magic_str(),
        vol.fat_type,
    );
    println!(
        "  Cluster size: {}, Total clusters: {}\n",
        format_size(vol.superblock.cluster_size()),
        vol.total_clusters
    );

    // Interactive shell loop
    let mut cwd = "/".to_string();
    loop {
        println!("------------------------------------------");
        println!("  Current directory: {}", cwd);
        println!("------------------------------------------");
        println!("  1) ls        — List files here");
        println!("  2) cd        — Change directory");
        println!("  3) read      — Extract a file to local disk");
        println!("  4) write     — Upload a local file or directory to the FATX volume");
        println!("  5) mkdir     — Create a directory");
        println!("  6) rm        — Delete a file or directory");
        println!("  7) rename    — Rename a file or directory");
        println!("  8) info      — Volume statistics");
        println!("  9) hexdump   — Raw hex dump");
        println!(" 10) cleanup   — Remove macOS metadata (.DS_Store, ._ files, etc.)");
        println!("  0) quit");
        println!();
        print!("fatx> ");
        io::stdout().flush().unwrap();

        let input = read_line();
        let cmd = input.trim();
        println!();

        match cmd {
            "1" | "ls" | "dir" => {
                interactive_ls(&mut vol, &cwd);
            }
            "2" | "cd" => {
                cwd = interactive_cd(&mut vol, &cwd);
            }
            "3" | "read" | "extract" | "get" => {
                interactive_read(&mut vol, &cwd);
            }
            "4" | "write" | "put" | "upload" => {
                interactive_write(&mut vol, &cwd);
            }
            "5" | "mkdir" => {
                interactive_mkdir(&mut vol, &cwd);
            }
            "6" | "rm" | "del" | "delete" => {
                interactive_rm(&mut vol, &cwd);
            }
            "7" | "rename" | "mv" | "ren" => {
                interactive_rename(&mut vol, &cwd);
            }
            "8" | "info" | "stats" => {
                interactive_info(&mut vol);
            }
            "9" | "hexdump" | "hex" => {
                interactive_hexdump(device_path);
            }
            "10" | "cleanup" => {
                println!("  Scanning for macOS metadata files...\n");
                match vol.scan_macos_metadata() {
                    Ok(found) => {
                        if found.is_empty() {
                            println!("  No macOS metadata found.");
                        } else {
                            let total_bytes: u64 = found.iter().map(|e| e.size).sum();
                            let file_count = found.iter().filter(|e| !e.is_dir).count();
                            let dir_count = found.iter().filter(|e| e.is_dir).count();
                            println!("  Found:");
                            for entry in &found {
                                let kind = if entry.is_dir { "dir " } else { "file" };
                                println!(
                                    "    [{}] {} ({})",
                                    kind,
                                    entry.path,
                                    format_size(entry.size)
                                );
                            }
                            println!(
                                "\n  {} file(s), {} dir(s), {} to free",
                                file_count,
                                dir_count,
                                format_size(total_bytes)
                            );
                            print!("\n  Delete all? (y/n): ");
                            io::stdout().flush().unwrap();
                            let ans = read_line();
                            if ans.trim().starts_with('y') || ans.trim().starts_with('Y') {
                                let progress = |path: &str| {
                                    println!("  Deleting {}", path);
                                };
                                match vol.delete_macos_metadata(&found, Some(&progress)) {
                                    Ok((files, dirs, bytes)) => {
                                        let _ = vol.flush();
                                        println!(
                                            "\n  Removed {} file(s), {} dir(s), freed {}",
                                            files,
                                            dirs,
                                            format_size(bytes)
                                        );
                                    }
                                    Err(e) => eprintln!("  Error during cleanup: {}", e),
                                }
                            } else {
                                println!("  Cancelled.");
                            }
                        }
                    }
                    Err(e) => eprintln!("  Error scanning: {}", e),
                }
            }
            "0" | "quit" | "exit" | "q" => {
                println!("Flushing and exiting...");
                let _ = vol.flush();
                println!("Done. Goodbye!");
                return;
            }
            "" => continue,
            _ => {
                println!(
                    "Unknown command '{}'. Enter a number 0-9 or a command name.",
                    cmd
                );
            }
        }
        println!();
    }
}

fn interactive_ls(vol: &mut FatxVolume<std::fs::File>, cwd: &str) {
    let entry = match vol.resolve_path(cwd) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("Error: {}", e);
            return;
        }
    };

    let entries = match vol.read_directory(entry.first_cluster) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("Error reading directory: {}", e);
            return;
        }
    };

    if entries.is_empty() {
        println!("  (empty directory)");
        return;
    }

    println!("  {:<6} {:>12} {:<20} Name", "Attr", "Size", "Modified");
    println!("  {}", "-".repeat(60));

    for entry in &entries {
        let name = if entry.is_directory() {
            fatxlib::display::format_for_path(cwd, &entry.filename())
        } else {
            entry.filename()
        };
        let attr = format!(
            "{}{}{}{}",
            if entry.is_directory() { "d" } else { "-" },
            if entry.attributes.contains(FileAttributes::READ_ONLY) {
                "r"
            } else {
                "-"
            },
            if entry.attributes.contains(FileAttributes::HIDDEN) {
                "h"
            } else {
                "-"
            },
            if entry.attributes.contains(FileAttributes::SYSTEM) {
                "s"
            } else {
                "-"
            },
        );
        let size_str = if entry.is_directory() {
            "<DIR>".to_string()
        } else {
            format_size(entry.file_size as u64)
        };
        println!(
            "  {:<6} {:>12} {:<20} {}",
            attr,
            size_str,
            entry.write_datetime_str(),
            name
        );
    }
    println!("\n  {} item(s)", entries.len());
}

fn interactive_cd(vol: &mut FatxVolume<std::fs::File>, cwd: &str) -> String {
    let mut current = cwd.to_string();

    loop {
        let entry = match vol.resolve_path(&current) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("Error: {}", e);
                return current;
            }
        };

        let entries = vol.read_directory(entry.first_cluster).unwrap_or_default();
        let dirs: Vec<_> = entries.iter().filter(|e| e.is_directory()).collect();

        println!("\n  Current: {}", current);
        if dirs.is_empty() && current != "/" {
            println!("  No subdirectories here.");
        } else {
            println!("  Subdirectories:");
            if current != "/" {
                println!("    ..) Go up (parent directory)");
            }
            for (i, d) in dirs.iter().enumerate() {
                let display =
                    fatxlib::display::format_for_path(&current, &d.filename());
                println!("    {}) {}/", i + 1, display);
            }
        }

        print!("\n  Navigate (number, '..', '/') or Enter to stay here: ");
        io::stdout().flush().unwrap();
        let input = read_line();
        let input = input.trim();

        // Empty input = done navigating, stay at current
        if input.is_empty() {
            return current;
        }

        if input == "/" {
            current = "/".to_string();
            continue;
        }
        if input == ".." {
            if current == "/" {
                continue;
            }
            let parent = match current.rfind('/') {
                Some(0) => "/",
                Some(pos) => &current[..pos],
                None => "/",
            };
            current = parent.to_string();
            continue;
        }

        let target = if let Ok(n) = input.parse::<usize>() {
            if n >= 1 && n <= dirs.len() {
                dirs[n - 1].filename()
            } else {
                println!("  Invalid selection.");
                continue;
            }
        } else {
            input.to_string()
        };

        let new_path = if current == "/" {
            format!("/{}", target)
        } else {
            format!("{}/{}", current, target)
        };

        match vol.resolve_path(&new_path) {
            Ok(e) if e.is_directory() => {
                current = new_path;
            }
            Ok(_) => {
                println!("  '{}' is not a directory.", target);
            }
            Err(e) => {
                println!("  Error: {}", e);
            }
        }
    }
}

fn interactive_read(vol: &mut FatxVolume<std::fs::File>, cwd: &str) {
    // Show files in current directory
    let entry = match vol.resolve_path(cwd) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("Error: {}", e);
            return;
        }
    };

    let entries = vol.read_directory(entry.first_cluster).unwrap_or_default();
    let files: Vec<_> = entries.iter().filter(|e| !e.is_directory()).collect();

    if files.is_empty() {
        println!("  No files in current directory.");
        return;
    }

    println!("  Files:");
    for (i, f) in files.iter().enumerate() {
        println!(
            "    {}) {} ({})",
            i + 1,
            f.filename(),
            format_size(f.file_size as u64)
        );
    }

    print!("\n  Select file [1-{}] or enter name: ", files.len());
    io::stdout().flush().unwrap();
    let input = read_line();
    let input = input.trim();

    let filename = if let Ok(n) = input.parse::<usize>() {
        if n >= 1 && n <= files.len() {
            files[n - 1].filename()
        } else {
            println!("  Invalid selection.");
            return;
        }
    } else {
        input.to_string()
    };

    let fatx_path = if cwd == "/" {
        format!("/{}", filename)
    } else {
        format!("{}/{}", cwd, filename)
    };

    print!("  Save to local path [default: ./{}]: ", filename);
    io::stdout().flush().unwrap();
    let out_input = read_line();
    let out_path = if out_input.trim().is_empty() {
        PathBuf::from(&filename)
    } else {
        PathBuf::from(out_input.trim())
    };

    match vol.read_file_by_path(&fatx_path) {
        Ok(data) => match fs::write(&out_path, &data) {
            Ok(()) => println!(
                "  Extracted '{}' -> '{}' ({} bytes)",
                fatx_path,
                out_path.display(),
                data.len()
            ),
            Err(e) => eprintln!("  Error writing: {}", e),
        },
        Err(e) => eprintln!("  Error reading: {}", e),
    }
}

fn interactive_write(vol: &mut FatxVolume<std::fs::File>, cwd: &str) {
    print!("  Local file or directory to upload: ");
    io::stdout().flush().unwrap();
    let local_path = read_line();
    let local_path = local_path.trim();

    if local_path.is_empty() {
        println!("  Cancelled.");
        return;
    }

    // Unescape shell backslashes (e.g. Call\ of\ Duty → Call of Duty)
    let unescaped = local_path.replace("\\ ", " ");
    let path = PathBuf::from(&unescaped);
    if !path.exists() {
        eprintln!("  Not found: {}", unescaped);
        return;
    }

    let default_name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "file.dat".to_string());

    if path.is_dir() {
        // Directory upload — use copy_from_host
        print!("  Folder name on FATX volume [default: {}]: ", default_name);
        io::stdout().flush().unwrap();
        let name_input = read_line();
        let fatx_name = if name_input.trim().is_empty() {
            default_name
        } else {
            name_input.trim().to_string()
        };

        let fatx_dest = if cwd == "/" {
            format!("/{}", fatx_name)
        } else {
            format!("{}/{}", cwd, fatx_name)
        };

        println!("  Copying directory '{}' to '{}'...", local_path, fatx_dest);

        let local = std::path::Path::new(local_path);
        let progress_fn = |msg: &str, _current: u64, _total: u64| {
            println!("    {}", msg);
        };

        match vol.copy_from_host(local, &fatx_dest, Some(&progress_fn)) {
            Ok((dirs, files, bytes)) => {
                let _ = vol.flush();
                println!(
                    "  Done! {} dirs, {} files, {} copied",
                    dirs,
                    files,
                    format_size(bytes)
                );
            }
            Err(e) => eprintln!("  Error: {}", e),
        }
    } else {
        // Single file upload
        let data = match fs::read(local_path) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("  Error reading '{}': {}", local_path, e);
                return;
            }
        };

        print!("  Name on FATX volume [default: {}]: ", default_name);
        io::stdout().flush().unwrap();
        let name_input = read_line();
        let fatx_name = if name_input.trim().is_empty() {
            default_name
        } else {
            name_input.trim().to_string()
        };

        let fatx_path = if cwd == "/" {
            format!("/{}", fatx_name)
        } else {
            format!("{}/{}", cwd, fatx_name)
        };

        println!(
            "  Writing {} ({}) to '{}'...",
            local_path,
            format_size(data.len() as u64),
            fatx_path
        );

        match vol.create_or_replace_file(&fatx_path, &data) {
            Ok(()) => {
                let _ = vol.flush();
                println!("  Done!");
            }
            Err(e) => eprintln!("  Error: {}", e),
        }
    }
}

fn interactive_mkdir(vol: &mut FatxVolume<std::fs::File>, cwd: &str) {
    print!("  New directory name: ");
    io::stdout().flush().unwrap();
    let name = read_line();
    let name = name.trim();

    if name.is_empty() {
        println!("  Cancelled.");
        return;
    }

    let path = if cwd == "/" {
        format!("/{}", name)
    } else {
        format!("{}/{}", cwd, name)
    };

    match vol.create_directory(&path) {
        Ok(()) => {
            let _ = vol.flush();
            println!("  Created directory '{}'", path);
        }
        Err(e) => eprintln!("  Error: {}", e),
    }
}

fn interactive_rm(vol: &mut FatxVolume<std::fs::File>, cwd: &str) {
    // List items
    let entry = match vol.resolve_path(cwd) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("Error: {}", e);
            return;
        }
    };

    let entries = vol.read_directory(entry.first_cluster).unwrap_or_default();
    if entries.is_empty() {
        println!("  Nothing to delete.");
        return;
    }

    println!("  Items:");
    for (i, e) in entries.iter().enumerate() {
        let kind = if e.is_directory() { "<DIR>" } else { "" };
        let display = if e.is_directory() {
            fatxlib::display::format_for_path(cwd, &e.filename())
        } else {
            e.filename()
        };
        println!("    {}) {} {}", i + 1, display, kind);
    }

    print!(
        "\n  Select item to delete [1-{}] or enter name: ",
        entries.len()
    );
    io::stdout().flush().unwrap();
    let input = read_line();
    let input = input.trim();

    let name = if let Ok(n) = input.parse::<usize>() {
        if n >= 1 && n <= entries.len() {
            entries[n - 1].filename()
        } else {
            println!("  Invalid selection.");
            return;
        }
    } else {
        input.to_string()
    };

    let path = if cwd == "/" {
        format!("/{}", name)
    } else {
        format!("{}/{}", cwd, name)
    };

    // Check if it's a directory to offer recursive delete
    let is_dir = vol
        .resolve_path(&path)
        .map(|e| e.is_directory())
        .unwrap_or(false);

    if is_dir {
        print!(
            "  '{}' is a directory. Delete it and all its contents? (y/n): ",
            path
        );
    } else {
        print!("  Really delete '{}'? (y/n): ", path);
    }
    io::stdout().flush().unwrap();
    let confirm = read_line();
    if !confirm.starts_with('y') && !confirm.starts_with('Y') {
        println!("  Cancelled.");
        return;
    }

    let result = if is_dir {
        vol.delete_recursive(&path)
    } else {
        vol.delete(&path)
    };

    match result {
        Ok(()) => {
            let _ = vol.flush();
            println!("  Deleted '{}'", path);
        }
        Err(e) => eprintln!("  Error: {}", e),
    }
}

fn interactive_rename(vol: &mut FatxVolume<std::fs::File>, cwd: &str) {
    let entry = match vol.resolve_path(cwd) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("Error: {}", e);
            return;
        }
    };

    let entries = vol.read_directory(entry.first_cluster).unwrap_or_default();
    if entries.is_empty() {
        println!("  Nothing to rename.");
        return;
    }

    println!("  Items:");
    for (i, e) in entries.iter().enumerate() {
        let display = if e.is_directory() {
            fatxlib::display::format_for_path(cwd, &e.filename())
        } else {
            e.filename()
        };
        println!("    {}) {}", i + 1, display);
    }

    print!("\n  Select item [1-{}] or enter name: ", entries.len());
    io::stdout().flush().unwrap();
    let input = read_line();
    let input = input.trim();

    let old_name = if let Ok(n) = input.parse::<usize>() {
        if n >= 1 && n <= entries.len() {
            entries[n - 1].filename()
        } else {
            println!("  Invalid selection.");
            return;
        }
    } else {
        input.to_string()
    };

    print!("  New name for '{}': ", old_name);
    io::stdout().flush().unwrap();
    let new_name = read_line();
    let new_name = new_name.trim();

    if new_name.is_empty() {
        println!("  Cancelled.");
        return;
    }

    let path = if cwd == "/" {
        format!("/{}", old_name)
    } else {
        format!("{}/{}", cwd, old_name)
    };

    match vol.rename(&path, new_name) {
        Ok(()) => {
            let _ = vol.flush();
            println!("  Renamed '{}' -> '{}'", old_name, new_name);
        }
        Err(e) => eprintln!("  Error: {}", e),
    }
}

fn interactive_info(vol: &mut FatxVolume<std::fs::File>) {
    println!("  FATX Volume Information");
    println!("  =======================");
    println!("  Volume ID:        0x{:08X}", vol.superblock.volume_id);
    println!("  FAT type:         {}", vol.fat_type);
    println!("  Sectors/cluster:  {}", vol.superblock.sectors_per_cluster);
    println!(
        "  Cluster size:     {}",
        format_size(vol.superblock.cluster_size())
    );
    println!("  Total clusters:   {}", vol.total_clusters);

    match vol.stats() {
        Ok(stats) => {
            println!();
            println!("  Space Usage");
            println!("  -----------");
            println!("  Total:            {}", format_size(stats.total_size));
            println!(
                "  Used:             {} ({} clusters)",
                format_size(stats.used_size),
                stats.used_clusters
            );
            println!(
                "  Free:             {} ({} clusters)",
                format_size(stats.free_size),
                stats.free_clusters
            );
            if stats.bad_clusters > 0 {
                println!("  Bad:              {} clusters", stats.bad_clusters);
            }
            let pct = if stats.total_clusters > 0 {
                stats.used_clusters as f64 / stats.total_clusters as f64 * 100.0
            } else {
                0.0
            };
            println!("  Utilization:      {:.1}%", pct);
        }
        Err(e) => eprintln!("  Error: {}", e),
    }
}

fn interactive_hexdump(device: &PathBuf) {
    print!("  Offset (hex e.g. 0x1000, or decimal): ");
    io::stdout().flush().unwrap();
    let off_str = read_line();
    let offset = match parse_hex_or_dec(off_str.trim()) {
        Ok(v) => v,
        Err(e) => {
            println!("  {}", e);
            return;
        }
    };

    print!("  Bytes to dump [default: 256]: ");
    io::stdout().flush().unwrap();
    let count_str = read_line();
    let count: usize = if count_str.trim().is_empty() {
        256
    } else {
        count_str.trim().parse().unwrap_or(256)
    };

    let mut file = match OpenOptions::new().read(true).open(device) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("  Error: {}", e);
            return;
        }
    };

    // Sector-align the read for macOS raw devices (/dev/rdiskN)
    let sector_start = offset & !0x1FF; // round down to 512-byte boundary
    let pre_skip = (offset - sector_start) as usize;
    let aligned_len = (pre_skip + count + 511) & !511; // round up to sector

    file.seek(SeekFrom::Start(sector_start)).unwrap();
    let mut aligned_buf = vec![0u8; aligned_len];
    if let Err(e) = file.read_exact(&mut aligned_buf) {
        eprintln!("  Error reading: {}", e);
        return;
    }
    let buf = &aligned_buf[pre_skip..pre_skip + count];

    println!();
    for (i, chunk) in buf.chunks(16).enumerate() {
        let addr = offset + (i * 16) as u64;
        print!("  {:08X}  ", addr);
        for (j, byte) in chunk.iter().enumerate() {
            print!("{:02X} ", byte);
            if j == 7 {
                print!(" ");
            }
        }
        if chunk.len() < 16 {
            for j in chunk.len()..16 {
                print!("   ");
                if j == 7 {
                    print!(" ");
                }
            }
        }
        print!(" |");
        for byte in chunk {
            if byte.is_ascii_graphic() || *byte == b' ' {
                print!("{}", *byte as char);
            } else {
                print!(".");
            }
        }
        println!("|");
    }
}

// ===========================================================================
// Helpers
// ===========================================================================

fn read_line() -> String {
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line).unwrap_or(0);
    line.trim_end_matches('\n')
        .trim_end_matches('\r')
        .to_string()
}

fn running_as_root() -> bool {
    unsafe { libc::geteuid() == 0 }
}

/// Use `diskutil list` to find external/physical disks on macOS.
fn detect_macos_disks() -> Vec<String> {
    let output = Command::new("diskutil").args(["list", "external"]).output();
    match output {
        Ok(out) => {
            let text = String::from_utf8_lossy(&out.stdout);
            let mut disks = Vec::new();
            for line in text.lines() {
                // Lines like "/dev/disk4 (external, physical):" indicate a disk
                if line.starts_with("/dev/disk") && line.contains("external") {
                    if let Some(dev) = line.split_whitespace().next() {
                        // Remove trailing colon if present
                        let dev = dev.trim_end_matches(':');
                        disks.push(dev.to_string());
                    }
                }
            }
            disks
        }
        Err(_) => {
            // Not on macOS or diskutil not available, try listing disk images
            Vec::new()
        }
    }
}

fn parse_hex_or_dec(s: &str) -> std::result::Result<u64, String> {
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16).map_err(|e| format!("Invalid hex: {}", e))
    } else {
        s.parse::<u64>()
            .map_err(|e| format!("Invalid number: {}", e))
    }
}

// ===========================================================================
// Direct subcommand implementations (non-interactive)
// ===========================================================================

fn resolve_partition(
    device: &PathBuf,
    partition_name: &Option<String>,
    offset: u64,
    size: u64,
) -> (u64, u64) {
    if let Some(name) = partition_name {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(device)
            .unwrap_or_else(|e| {
                eprintln!("Error opening device: {}", e);
                process::exit(1);
            });

        let dev_size = get_device_size(&mut file);
        match detect_xbox_partitions(&mut file, dev_size) {
            Ok(parts) => {
                for p in &parts {
                    if p.name.eq_ignore_ascii_case(name) && p.has_valid_magic {
                        return (p.offset, p.size);
                    }
                }
                eprintln!("Partition '{}' not found or has no valid FATX magic.", name);
                process::exit(1);
            }
            Err(e) => {
                eprintln!("Error scanning partitions: {}", e);
                process::exit(1);
            }
        }
    }
    (offset, size)
}

fn open_volume(
    device: &PathBuf,
    partition: &Option<String>,
    offset: u64,
    size: u64,
) -> FatxVolume<std::fs::File> {
    let (offset, size) = resolve_partition(device, partition, offset, size);
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(device)
        .unwrap_or_else(|e| {
            eprintln!("Error opening '{}': {}", device.display(), e);
            if e.kind() == io::ErrorKind::PermissionDenied {
                eprintln!("Hint: Try running with sudo.");
            }
            process::exit(1);
        });

    // If size is still 0 (no --partition and no --size), compute it from the device
    let size = if size == 0 {
        let dev_size = get_device_size(&mut file);
        dev_size.saturating_sub(offset)
    } else {
        size
    };

    // Capture raw fd before file is moved into the volume
    #[cfg(target_os = "macos")]
    let raw_fd = {
        use std::os::unix::io::AsRawFd;
        file.as_raw_fd()
    };

    let mut vol = FatxVolume::open(file, offset, size).unwrap_or_else(|e| {
        eprintln!("Error opening FATX volume: {}", e);

        // If no partition was specified, scan and show available partitions
        if partition.is_none() && offset == 0 {
            eprintln!();
            eprintln!("No --partition specified. This device likely has multiple Xbox partitions.");
            eprintln!(
                "Run: fatx scan {} to see available partitions.",
                device.display()
            );

            // Try to auto-detect and list partitions
            if let Ok(mut f) = std::fs::File::open(device) {
                let dev_size = get_device_size(&mut f);
                if let Ok(parts) = fatxlib::partition::detect_xbox_partitions(&mut f, dev_size) {
                    if !parts.is_empty() {
                        eprintln!();
                        eprintln!("Available partitions:");
                        for p in &parts {
                            if p.has_valid_magic {
                                eprintln!(
                                    "  --partition \"{}\"  ({})",
                                    p.name,
                                    fatxlib::partition::format_size(p.size)
                                );
                            }
                        }
                        eprintln!();
                        eprintln!(
                            "Example: sudo fatx browse {} --partition \"{}\"",
                            device.display(),
                            parts
                                .iter()
                                .find(|p| p.has_valid_magic)
                                .map(|p| p.name.as_str())
                                .unwrap_or("360 Data")
                        );
                    }
                }
            }
        }
        process::exit(1);
    });

    // Configure macOS-specific I/O (F_NOCACHE, F_RDAHEAD, device params)
    #[cfg(target_os = "macos")]
    vol.configure_device(raw_fd);

    vol
}

fn dirent_to_json(entry: &fatxlib::types::DirectoryEntry) -> JsonDirEntry {
    let attr = format!(
        "{}{}{}{}",
        if entry.is_directory() { "d" } else { "-" },
        if entry.attributes.contains(FileAttributes::READ_ONLY) {
            "r"
        } else {
            "-"
        },
        if entry.attributes.contains(FileAttributes::HIDDEN) {
            "h"
        } else {
            "-"
        },
        if entry.attributes.contains(FileAttributes::SYSTEM) {
            "s"
        } else {
            "-"
        },
    );
    JsonDirEntry {
        name: entry.filename(),
        is_directory: entry.is_directory(),
        size: entry.file_size as u64,
        attributes: attr,
        first_cluster: entry.first_cluster,
        created: entry.creation_datetime_str(),
        modified: entry.write_datetime_str(),
        accessed: entry.access_datetime_str(),
    }
}

fn print_entry(entry: &fatxlib::types::DirectoryEntry, parent_path: &str, long: bool) {
    let name = if entry.is_directory() {
        fatxlib::display::format_for_path(parent_path, &entry.filename())
    } else {
        entry.filename()
    };
    if long {
        let attr = format!(
            "{}{}{}{}",
            if entry.is_directory() { "d" } else { "-" },
            if entry.attributes.contains(FileAttributes::READ_ONLY) {
                "r"
            } else {
                "-"
            },
            if entry.attributes.contains(FileAttributes::HIDDEN) {
                "h"
            } else {
                "-"
            },
            if entry.attributes.contains(FileAttributes::SYSTEM) {
                "s"
            } else {
                "-"
            },
        );
        let size = if entry.is_directory() {
            "<DIR>".to_string()
        } else {
            format_size(entry.file_size as u64)
        };
        println!(
            "{:<6} {:>12} {:<20} {}",
            attr,
            size,
            entry.write_datetime_str(),
            name
        );
    } else if entry.is_directory() {
        println!("{}/", name);
    } else {
        println!("{}", name);
    }
}

// ===========================================================================
// main
// ===========================================================================

fn main() {
    let cli = Cli::parse();

    // Best-effort load of user-resolved titles + files. Silent on failure —
    // a missing file is normal on first run, and any error here shouldn't
    // block CLI use.
    if let Some(cache_path) = fatxlib::titles::user_cache::default_path() {
        let _ = fatxlib::titles::user_cache::load_from(&cache_path);
    }
    if let Some(cache_path) = fatxlib::titles::file_cache::default_path() {
        let _ = fatxlib::titles::file_cache::load_from(&cache_path);
    }
    if let Some(cache_path) = fatxlib::xuids::profile_cache::default_path() {
        let _ = fatxlib::xuids::profile_cache::load_from(&cache_path);
    }

    // Mount and mkimage init their own loggers with their preferred format.
    // Only init the CLI logger for other subcommands.
    let is_mount = matches!(cli.command, Some(Commands::Mount(_)));
    let is_mkimage = matches!(cli.command, Some(Commands::Mkimage(_)));
    if !is_mount && !is_mkimage {
        let default_level = if cli.verbose { "debug" } else { "warn" };
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(default_level))
            .format(|buf, record| {
                writeln!(
                    buf,
                    "[{} {}:{}] {}",
                    record.level(),
                    record.file().unwrap_or("?"),
                    record.line().unwrap_or(0),
                    record.args()
                )
            })
            .init();

        if cli.verbose {
            log::debug!("Verbose mode enabled");
        }
    }

    let json = cli.json;

    match cli.command {
        None => interactive_mode(),

        Some(Commands::Browse {
            device,
            offset,
            size,
            partition,
        }) => {
            let (device, partition) = if let Some(dev) = device {
                (dev, partition)
            } else {
                // Guided mode
                println!();
                println!("========================================");
                println!("  fatx browse — guided setup");
                println!("========================================");
                println!();
                match guided_partition_selection() {
                    Some(sel) => (sel.device_path, Some(sel.partition_name)),
                    None => process::exit(0),
                }
            };
            let part_name = partition
                .clone()
                .unwrap_or_else(|| "FATX Volume".to_string());
            let vol = open_volume(&device, &partition, offset, size);
            let dev_display = device.display().to_string();
            if let Err(e) = tui::run_browser(vol, &part_name, &dev_display) {
                eprintln!("TUI error: {}", e);
                process::exit(1);
            }
        }

        Some(Commands::Scan {
            device,
            deep,
            deep_limit,
        }) => {
            let mut file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&device)
                .unwrap_or_else(|e| {
                    eprintln!("Error: {}", e);
                    process::exit(1);
                });
            if !json {
                println!(
                    "Scanning {} for FATX/XTAF partitions...\n",
                    device.display()
                );
            }
            let dev_size = get_device_size(&mut file);
            match detect_xbox_partitions(&mut file, dev_size) {
                Ok(parts) => {
                    if json {
                        let jp: Vec<JsonPartition> = parts
                            .iter()
                            .map(|p| JsonPartition {
                                name: p.name.clone(),
                                offset: p.offset,
                                offset_hex: format!("0x{:X}", p.offset),
                                size: p.size,
                                size_human: format_size(p.size),
                                has_valid_magic: p.has_valid_magic,
                                magic: if p.has_valid_magic {
                                    p.magic.clone()
                                } else {
                                    String::new()
                                },
                                generation: if p.has_valid_magic {
                                    format!("{}", p.generation)
                                } else {
                                    String::new()
                                },
                            })
                            .collect();
                        println!("{}", serde_json::to_string_pretty(&jp).unwrap());
                    } else {
                        println!(
                            "{:<25} {:>14} {:>10} {:>6} Console",
                            "Partition", "Offset", "Size", "Magic"
                        );
                        println!("{}", "-".repeat(75));
                        for p in &parts {
                            let magic_str = if p.has_valid_magic {
                                p.magic.as_str()
                            } else {
                                "--"
                            };
                            let gen_str = if p.has_valid_magic {
                                format!("{}", p.generation)
                            } else {
                                String::new()
                            };
                            println!(
                                "{:<25} 0x{:010X}   {:>10} {:>6} {}",
                                p.name,
                                p.offset,
                                format_size(p.size),
                                magic_str,
                                gen_str
                            );
                        }
                    }
                }
                Err(e) => {
                    if json {
                        println!("{}", serde_json::json!({"error": format!("{}", e)}));
                    } else {
                        eprintln!("Error: {}", e);
                    }
                }
            }
            if deep && !json {
                println!("\nDeep scanning up to 0x{:X}...", deep_limit);
                match fatxlib::partition::scan_for_fatx(&mut file, deep_limit) {
                    Ok(offsets) => {
                        if offsets.is_empty() {
                            println!("No additional signatures found.");
                        } else {
                            for off in &offsets {
                                println!("  FATX at 0x{:08X}", off);
                            }
                        }
                    }
                    Err(e) => eprintln!("Error: {}", e),
                }
            }
        }

        Some(Commands::Ls {
            device,
            path,
            offset,
            size,
            partition,
            long,
        }) => {
            let mut vol = open_volume(&device, &partition, offset, size);
            let entry = vol.resolve_path(&path).unwrap_or_else(|e| {
                if json {
                    println!("{}", serde_json::json!({"error": format!("{}", e)}));
                    process::exit(0);
                }
                eprintln!("Error: {}", e);
                process::exit(1);
            });
            if !entry.is_directory() {
                if json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&[dirent_to_json(&entry)]).unwrap()
                    );
                } else {
                    print_entry(&entry, &path, long);
                }
                return;
            }
            let entries = vol.read_directory(entry.first_cluster).unwrap_or_else(|e| {
                if json {
                    println!("{}", serde_json::json!({"error": format!("{}", e)}));
                    process::exit(0);
                }
                eprintln!("Error: {}", e);
                process::exit(1);
            });
            // Eager: when listing /Content, probe each personal XUID for a
            // profile package so the gamertag shows up in the listing below.
            // JSON output stays untouched per the locked rule.
            if fatxlib::display::folder_slot(&path) == fatxlib::display::FolderSlot::Xuid {
                let _ = fatxlib::xuids::resolve_profile_xuids(&mut vol, &entries, true);
            }
            if json {
                let je: Vec<JsonDirEntry> = entries.iter().map(dirent_to_json).collect();
                println!("{}", serde_json::to_string_pretty(&je).unwrap());
            } else {
                if entries.is_empty() {
                    println!("(empty)");
                    return;
                }
                if long {
                    println!("{:<6} {:>12} {:<20} Name", "Attr", "Size", "Modified");
                    println!("{}", "-".repeat(65));
                }
                for e in &entries {
                    print_entry(e, &path, long);
                }
            }
        }

        Some(Commands::Read {
            device,
            path,
            output,
            offset,
            size,
            partition,
        }) => {
            let mut vol = open_volume(&device, &partition, offset, size);
            let data = vol.read_file_by_path(&path).unwrap_or_else(|e| {
                if json {
                    println!("{}", serde_json::json!({"error": format!("{}", e)}));
                    process::exit(0);
                }
                eprintln!("Error: {}", e);
                process::exit(1);
            });
            if json {
                let jf = JsonFileContent {
                    path: path.clone(),
                    size: data.len(),
                    data_base64: base64::engine::general_purpose::STANDARD.encode(&data),
                };
                println!("{}", serde_json::to_string_pretty(&jf).unwrap());
            } else {
                match output {
                    Some(out) => {
                        fs::write(&out, &data).unwrap_or_else(|e| {
                            eprintln!("Error: {}", e);
                            process::exit(1);
                        });
                        println!(
                            "Extracted '{}' -> '{}' ({} bytes)",
                            path,
                            out.display(),
                            data.len()
                        );
                    }
                    None => {
                        io::stdout().write_all(&data).unwrap();
                    }
                }
            }
        }

        Some(Commands::Write {
            device,
            path,
            input,
            offset,
            size,
            partition,
        }) => {
            let mut vol = open_volume(&device, &partition, offset, size);
            let data = fs::read(&input).unwrap_or_else(|e| {
                if json {
                    println!("{}", serde_json::json!({"error": format!("{}", e)}));
                    process::exit(0);
                }
                eprintln!("Error: {}", e);
                process::exit(1);
            });
            vol.create_file(&path, &data).unwrap_or_else(|e| {
                if json {
                    println!("{}", serde_json::json!({"error": format!("{}", e)}));
                    process::exit(0);
                }
                eprintln!("Error: {}", e);
                process::exit(1);
            });
            vol.flush().unwrap();
            let msg = format!(
                "Wrote '{}' -> '{}' ({} bytes)",
                input.display(),
                path,
                data.len()
            );
            if json {
                println!(
                    "{}",
                    serde_json::to_string(&JsonSuccess {
                        success: true,
                        message: msg
                    })
                    .unwrap()
                );
            } else {
                println!("{}", msg);
            }
        }

        Some(Commands::Mkdir {
            device,
            path,
            offset,
            size,
            partition,
        }) => {
            let mut vol = open_volume(&device, &partition, offset, size);
            vol.create_directory(&path).unwrap_or_else(|e| {
                if json {
                    println!("{}", serde_json::json!({"error": format!("{}", e)}));
                    process::exit(0);
                }
                eprintln!("Error: {}", e);
                process::exit(1);
            });
            vol.flush().unwrap();
            let msg = format!("Created '{}'", path);
            if json {
                println!(
                    "{}",
                    serde_json::to_string(&JsonSuccess {
                        success: true,
                        message: msg
                    })
                    .unwrap()
                );
            } else {
                println!("{}", msg);
            }
        }

        Some(Commands::Rm {
            device,
            path,
            recursive,
            offset,
            size,
            partition,
        }) => {
            let mut vol = open_volume(&device, &partition, offset, size);
            let result = if recursive {
                vol.delete_recursive(&path)
            } else {
                vol.delete(&path)
            };
            result.unwrap_or_else(|e| {
                if json {
                    println!("{}", serde_json::json!({"error": format!("{}", e)}));
                    process::exit(0);
                }
                eprintln!("Error: {}", e);
                process::exit(1);
            });
            vol.flush().unwrap();
            let msg = if recursive {
                format!("Recursively deleted '{}'", path)
            } else {
                format!("Deleted '{}'", path)
            };
            if json {
                println!(
                    "{}",
                    serde_json::to_string(&JsonSuccess {
                        success: true,
                        message: msg
                    })
                    .unwrap()
                );
            } else {
                println!("{}", msg);
            }
        }

        Some(Commands::Copy {
            device,
            from,
            to,
            offset,
            size,
            partition,
        }) => {
            let mut vol = open_volume(&device, &partition, offset, size);

            if !from.is_dir() {
                if json {
                    println!(
                        "{}",
                        serde_json::json!({"error": format!("'{}' is not a directory", from.display())})
                    );
                    process::exit(0);
                }
                eprintln!("Error: '{}' is not a directory", from.display());
                process::exit(1);
            }

            let start = std::time::Instant::now();
            let progress_fn = |path: &str, file_size: u64, total: u64| {
                if !json {
                    eprintln!(
                        "  [{:.1} MB] {} ({:.1} MB)",
                        total as f64 / 1_048_576.0,
                        path,
                        file_size as f64 / 1_048_576.0
                    );
                }
            };

            let interrupted = Arc::new(AtomicBool::new(false));
            let interrupted_handler = Arc::clone(&interrupted);
            ctrlc::set_handler(move || {
                interrupted_handler.store(true, Ordering::SeqCst);
            })
            .unwrap_or_else(|e| {
                if json {
                    println!(
                        "{}",
                        serde_json::json!({"error": format!("failed to install Ctrl-C handler: {}", e)})
                    );
                    process::exit(0);
                }
                eprintln!("Error: failed to install Ctrl-C handler: {}", e);
                process::exit(1);
            });

            let should_abort = || interrupted.load(Ordering::SeqCst);
            let copy_result = vol.copy_from_host_with_control(
                &from,
                &to,
                Some(&progress_fn),
                Some(&should_abort),
                100,
                256 * 1024 * 1024,
            );
            let (files, dirs, bytes) = match copy_result {
                Ok(result) => result,
                Err(FatxError::Io(ref io_err))
                    if io_err.kind() == std::io::ErrorKind::Interrupted =>
                {
                    if json {
                        println!("{}", serde_json::json!({"error": "copy interrupted"}));
                    } else {
                        eprintln!("Interrupted");
                    }
                    process::exit(130);
                }
                Err(e) => {
                    if json {
                        println!("{}", serde_json::json!({"error": format!("{}", e)}));
                        process::exit(0);
                    }
                    eprintln!("Error: {}", e);
                    process::exit(1);
                }
            };
            vol.flush().unwrap();

            let elapsed = start.elapsed().as_secs_f64();
            let rate = if elapsed > 0.0 {
                bytes as f64 / elapsed / 1_048_576.0
            } else {
                0.0
            };
            let msg = format!(
                "Copied {} files, {} dirs ({:.1} MB) in {:.1}s ({:.1} MB/s)",
                files,
                dirs,
                bytes as f64 / 1_048_576.0,
                elapsed,
                rate
            );
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "success": true,
                        "message": msg,
                        "files": files,
                        "directories": dirs,
                        "bytes": bytes,
                        "elapsed_seconds": elapsed,
                        "rate_mbps": rate
                    })
                );
            } else {
                println!("{}", msg);
            }
        }

        Some(Commands::Rmr {
            device,
            path,
            offset,
            size,
            partition,
        }) => {
            let mut vol = open_volume(&device, &partition, offset, size);
            vol.delete_recursive(&path).unwrap_or_else(|e| {
                if json {
                    println!("{}", serde_json::json!({"error": format!("{}", e)}));
                    process::exit(0);
                }
                eprintln!("Error: {}", e);
                process::exit(1);
            });
            vol.flush().unwrap();
            let msg = format!("Recursively deleted '{}'", path);
            if json {
                println!(
                    "{}",
                    serde_json::to_string(&JsonSuccess {
                        success: true,
                        message: msg
                    })
                    .unwrap()
                );
            } else {
                println!("{}", msg);
            }
        }

        Some(Commands::Rename {
            device,
            path,
            new_name,
            offset,
            size,
            partition,
        }) => {
            let mut vol = open_volume(&device, &partition, offset, size);
            vol.rename(&path, &new_name).unwrap_or_else(|e| {
                if json {
                    println!("{}", serde_json::json!({"error": format!("{}", e)}));
                    process::exit(0);
                }
                eprintln!("Error: {}", e);
                process::exit(1);
            });
            vol.flush().unwrap();
            let msg = format!("Renamed '{}' -> '{}'", path, new_name);
            if json {
                println!(
                    "{}",
                    serde_json::to_string(&JsonSuccess {
                        success: true,
                        message: msg
                    })
                    .unwrap()
                );
            } else {
                println!("{}", msg);
            }
        }

        Some(Commands::Info {
            device,
            offset,
            size,
            partition,
        }) => {
            let vol = open_volume(&device, &partition, offset, size);
            if json {
                let stats = vol.stats().unwrap_or_else(|e| {
                    println!("{}", serde_json::json!({"error": format!("{}", e)}));
                    process::exit(0);
                });
                let ji = JsonVolumeInfo {
                    volume_id: format!("0x{:08X}", vol.superblock.volume_id),
                    fat_type: format!("{}", vol.fat_type),
                    cluster_size: vol.superblock.cluster_size(),
                    cluster_size_human: format_size(vol.superblock.cluster_size()),
                    total_clusters: vol.total_clusters,
                    used_clusters: stats.used_clusters,
                    free_clusters: stats.free_clusters,
                    bad_clusters: stats.bad_clusters,
                    total_size: stats.total_size,
                    used_size: stats.used_size,
                    free_size: stats.free_size,
                    total_size_human: format_size(stats.total_size),
                    used_size_human: format_size(stats.used_size),
                    free_size_human: format_size(stats.free_size),
                };
                println!("{}", serde_json::to_string_pretty(&ji).unwrap());
            } else {
                println!("FATX Volume Information");
                println!("=======================");
                println!("Volume ID:          0x{:08X}", vol.superblock.volume_id);
                println!("FAT type:           {}", vol.fat_type);
                println!(
                    "Cluster size:       {}",
                    format_size(vol.superblock.cluster_size())
                );
                println!("Total clusters:     {}", vol.total_clusters);
                if let Ok(stats) = vol.stats() {
                    println!(
                        "\nUsed:  {} ({} clusters)",
                        format_size(stats.used_size),
                        stats.used_clusters
                    );
                    println!(
                        "Free:  {} ({} clusters)",
                        format_size(stats.free_size),
                        stats.free_clusters
                    );
                }
            }
        }

        Some(Commands::Hexdump {
            device,
            offset,
            count,
        }) => {
            let mut file = OpenOptions::new()
                .read(true)
                .open(&device)
                .unwrap_or_else(|e| {
                    eprintln!("Error: {}", e);
                    process::exit(1);
                });

            // Sector-align for macOS raw devices
            let sector_start = offset & !0x1FF;
            let pre_skip = (offset - sector_start) as usize;
            let aligned_len = (pre_skip + count + 511) & !511;

            file.seek(SeekFrom::Start(sector_start)).unwrap();
            let mut aligned_buf = vec![0u8; aligned_len];
            file.read_exact(&mut aligned_buf).unwrap_or_else(|e| {
                if json {
                    println!("{}", serde_json::json!({"error": format!("{}", e)}));
                    process::exit(0);
                }
                eprintln!("Error: {}", e);
                process::exit(1);
            });
            let buf = &aligned_buf[pre_skip..pre_skip + count];

            if json {
                let jh = JsonHexdump {
                    offset,
                    offset_hex: format!("0x{:08X}", offset),
                    count,
                    data_base64: base64::engine::general_purpose::STANDARD.encode(buf),
                    data_hex: hex::encode(buf),
                };
                println!("{}", serde_json::to_string_pretty(&jh).unwrap());
            } else {
                println!("Offset 0x{:08X}, {} bytes:", offset, count);
                for (i, chunk) in buf.chunks(16).enumerate() {
                    let addr = offset + (i * 16) as u64;
                    print!("{:08X}  ", addr);
                    for (j, b) in chunk.iter().enumerate() {
                        print!("{:02X} ", b);
                        if j == 7 {
                            print!(" ");
                        }
                    }
                    if chunk.len() < 16 {
                        for j in chunk.len()..16 {
                            print!("   ");
                            if j == 7 {
                                print!(" ");
                            }
                        }
                    }
                    print!(" |");
                    for b in chunk {
                        print!(
                            "{}",
                            if b.is_ascii_graphic() || *b == b' ' {
                                *b as char
                            } else {
                                '.'
                            }
                        );
                    }
                    println!("|");
                }
            }
        }

        Some(Commands::Cleanup {
            device,
            dry_run,
            offset,
            size,
            partition,
        }) => {
            let mut vol = open_volume(&device, &partition, offset, size);
            let found = vol.scan_macos_metadata().unwrap_or_else(|e| {
                if json {
                    println!("{}", serde_json::json!({"error": format!("{}", e)}));
                    process::exit(0);
                }
                eprintln!("Error scanning: {}", e);
                process::exit(1);
            });

            if found.is_empty() {
                let msg = "No macOS metadata found";
                if json {
                    println!(
                        "{}",
                        serde_json::json!({"success": true, "message": msg, "files": 0, "dirs": 0, "bytes": 0})
                    );
                } else {
                    println!("{}", msg);
                }
            } else if dry_run {
                let total_bytes: u64 = found.iter().map(|e| e.size).sum();
                let file_count = found.iter().filter(|e| !e.is_dir).count();
                let dir_count = found.iter().filter(|e| e.is_dir).count();
                if !json {
                    println!("Would delete:");
                    for entry in &found {
                        let kind = if entry.is_dir { "dir " } else { "file" };
                        println!("  [{}] {} ({})", kind, entry.path, format_size(entry.size));
                    }
                    println!(
                        "\n{} file(s), {} dir(s), {} would be freed",
                        file_count,
                        dir_count,
                        format_size(total_bytes)
                    );
                } else {
                    let paths: Vec<&str> = found.iter().map(|e| e.path.as_str()).collect();
                    println!(
                        "{}",
                        serde_json::json!({
                            "dry_run": true,
                            "files": file_count,
                            "dirs": dir_count,
                            "bytes": total_bytes,
                            "entries": paths
                        })
                    );
                }
            } else {
                let progress = |path: &str| {
                    if !json {
                        eprintln!("  Deleting {}", path);
                    }
                };
                match vol.delete_macos_metadata(&found, Some(&progress)) {
                    Ok((files, dirs, bytes)) => {
                        vol.flush().unwrap();
                        let msg = format!(
                            "Removed {} file(s), {} dir(s), freed {}",
                            files,
                            dirs,
                            format_size(bytes)
                        );
                        if json {
                            println!(
                                "{}",
                                serde_json::json!({
                                    "success": true,
                                    "message": msg,
                                    "files_deleted": files,
                                    "dirs_deleted": dirs,
                                    "bytes_freed": bytes
                                })
                            );
                        } else {
                            println!("{}", msg);
                        }
                    }
                    Err(e) => {
                        if json {
                            println!("{}", serde_json::json!({"error": format!("{}", e)}));
                            process::exit(0);
                        }
                        eprintln!("Error: {}", e);
                        process::exit(1);
                    }
                }
            }
        }

        Some(Commands::Mount(mut args)) => {
            // Guided mode: no device specified and not a cleanup run
            if args.device.is_none() && !args.cleanup {
                println!();
                println!("========================================");
                println!("  fatx mount — guided setup");
                println!("========================================");
                println!();

                match guided_partition_selection() {
                    Some(sel) => {
                        args.device = Some(sel.device_path);
                        args.partition = Some(sel.partition_name);
                        // Auto-enable mount — the user chose guided mode, they want Finder access
                        args.mount = true;
                    }
                    None => {
                        process::exit(0);
                    }
                }
            }
            mount::run(args);
        }

        Some(Commands::Mkimage(args)) => {
            mkimage::run(args);
        }

        Some(Commands::Resolve {
            device,
            path,
            offset,
            size,
            partition,
            no_save,
        }) => {
            let mut vol = open_volume(&device, &partition, offset, size);

            // Dispatch by what kind of path was given. We use folder_slot of
            // the path itself (which describes what slot its *children* are
            // in) to identify whether the path is a title-ID folder, a
            // content-type folder holding STFS files, or something else.
            use fatxlib::display::{folder_slot, FolderSlot};
            let slot_of_children = folder_slot(&path);
            let entry = match vol.resolve_path(&path) {
                Ok(e) => e,
                Err(e) => {
                    if json {
                        println!("{}", serde_json::json!({"error": format!("{}", e)}));
                        process::exit(0);
                    }
                    eprintln!("Error: {}", e);
                    process::exit(1);
                }
            };

            // Bulk scan: path is a content-type folder of STFS files
            if entry.is_directory() && slot_of_children == FolderSlot::StfsFile {
                match fatxlib::titles::dynamic::scan_folder_files(
                    &mut vol, &path, !no_save,
                ) {
                    Ok(summary) => {
                        if json {
                            println!(
                                "{}",
                                serde_json::json!({
                                    "scan": {
                                        "resolved": summary.resolved,
                                        "skipped": summary.skipped,
                                        "saved_to": summary.saved_to
                                            .as_ref()
                                            .map(|p| p.to_string_lossy().to_string()),
                                    }
                                })
                            );
                        } else {
                            println!(
                                "Scanned: {} resolved, {} skipped",
                                summary.resolved, summary.skipped
                            );
                            if let Some(p) = summary.saved_to {
                                println!("Saved to {}", p.display());
                            }
                        }
                        return;
                    }
                    Err(e) => {
                        if json {
                            println!("{}", serde_json::json!({"error": format!("{}", e)}));
                            process::exit(0);
                        }
                        eprintln!("Error: {}", e);
                        process::exit(1);
                    }
                }
            }

            // Single file: path is an STFS file in a known content-type folder
            if !entry.is_directory() {
                match fatxlib::titles::dynamic::from_file(&mut vol, &path) {
                    Ok(Some(name)) => {
                        fatxlib::titles::file_cache::insert(path.clone(), name.clone());
                        let saved_to = if !no_save {
                            fatxlib::titles::file_cache::default_path().and_then(|p| {
                                fatxlib::titles::file_cache::save_to(&p).ok().map(|_| p)
                            })
                        } else {
                            None
                        };
                        if json {
                            println!(
                                "{}",
                                serde_json::json!({
                                    "file": path,
                                    "name": name,
                                    "saved_to": saved_to
                                        .as_ref()
                                        .map(|p| p.to_string_lossy().to_string()),
                                })
                            );
                        } else {
                            println!("Resolved {} → {}", path, name);
                            if let Some(p) = saved_to {
                                println!("Saved to {}", p.display());
                            }
                        }
                        return;
                    }
                    Ok(None) => {
                        let msg = "file is not a parseable STFS package";
                        if json {
                            println!("{}", serde_json::json!({"error": msg}));
                            process::exit(0);
                        }
                        eprintln!("Could not resolve: {}", msg);
                        process::exit(2);
                    }
                    Err(e) => {
                        if json {
                            println!("{}", serde_json::json!({"error": format!("{}", e)}));
                            process::exit(0);
                        }
                        eprintln!("Error: {}", e);
                        process::exit(1);
                    }
                }
            }

            // Default: title-ID folder resolve (the original behavior).
            let outcome = fatxlib::titles::dynamic::resolve_and_cache(
                &mut vol, &path, !no_save,
            );

            match outcome {
                Ok(fatxlib::titles::dynamic::ResolveOutcome::Resolved {
                    title_id,
                    name,
                    saved_to,
                }) => {
                    if json {
                        println!(
                            "{}",
                            serde_json::json!({
                                "title_id": format!("{:08X}", title_id),
                                "name": name,
                                "saved_to": saved_to
                                    .as_ref()
                                    .map(|p| p.to_string_lossy().to_string()),
                            })
                        );
                    } else {
                        println!("Resolved {:08X} → {}", title_id, name);
                        if let Some(p) = saved_to {
                            println!("Saved to {}", p.display());
                        }
                    }
                }
                Ok(fatxlib::titles::dynamic::ResolveOutcome::BadTitleIdInPath {
                    last_segment,
                }) => {
                    let msg = format!(
                        "path's last segment is not an 8-hex title ID: {:?}",
                        last_segment
                    );
                    if json {
                        println!("{}", serde_json::json!({"error": msg}));
                        process::exit(0);
                    }
                    eprintln!("Error: {}", msg);
                    process::exit(1);
                }
                Ok(fatxlib::titles::dynamic::ResolveOutcome::NoStfs) => {
                    let msg = "no parseable STFS package found in folder";
                    if json {
                        println!("{}", serde_json::json!({"error": msg}));
                        process::exit(0);
                    }
                    eprintln!("Could not resolve: {}", msg);
                    process::exit(2);
                }
                Err(e) => {
                    if json {
                        println!("{}", serde_json::json!({"error": format!("{}", e)}));
                        process::exit(0);
                    }
                    eprintln!("Error: {}", e);
                    process::exit(1);
                }
            }
        }
    }
}
