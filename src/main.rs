//! xtafkit: Command-line + TUI tool for interacting with Xbox FATX/XTAF file systems.
//!
//! Run with no arguments for interactive mode, or use subcommands directly:
//!   xtafkit                     # Interactive guided mode
//!   xtafkit browse /dev/rdisk4  # TUI file browser
//!   xtafkit scan /dev/rdisk4
//!   xtafkit ls /dev/rdisk4 --partition "Data (E)" /

mod extract_stfs;
mod mkimage;
mod tui;

use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, Read as IoRead, Seek, SeekFrom, Write as IoWrite};
use std::path::PathBuf;
use std::process::{self, Command};

use clap::{Parser, Subcommand};
use fatxlib::partition::{DetectedPartition, detect_xbox_partitions, format_size};
use fatxlib::types::FileAttributes;
use fatxlib::volume::FatxVolume;
use serde::Serialize;

/// Get the size of a device, handling macOS raw block devices correctly.
fn get_device_size(file: &mut File) -> u64 {
    // Try seek first (works for regular files / disk images)
    if let Ok(size) = file.seek(SeekFrom::End(0))
        && size > 0
    {
        let _ = file.seek(SeekFrom::Start(0));
        return size;
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
    name = "xtafkit",
    about = "Read and write Xbox FATX/XTAF file systems on macOS",
    version,
    long_about = "Mac-native workbench for FATX/XTAF-formatted Xbox and Xbox 360 drives and disk images.\n\n\
                   Run with no arguments for interactive guided mode, or `xtafkit browse` for the TUI.\n\n\
                   Supports title-ID resolution, profile gamertag decoding, slot-aware folder display,\n\
                   on-demand STFS header parsing, and Finder integration via a local NFS server."
)]
struct Cli {
    /// Enable verbose debug output (shows all I/O, FAT lookups, partition probing, etc.)
    #[arg(short = 'v', long, global = true)]
    verbose: bool,

    /// Force JSON output (overrides the TTY auto-detect).
    #[arg(long, global = true, conflicts_with = "text")]
    json: bool,

    /// Force human-readable text output (overrides the TTY auto-detect).
    #[arg(long, global = true, conflicts_with = "json")]
    text: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

/// Decide whether to emit JSON or human text. When stdout is a terminal we
/// default to text (so casual `xtafkit ls` lands readable); when piped or
/// redirected we default to JSON (so scripts/jq get parseable output). The
/// `--json` and `--text` flags force either mode regardless.
fn want_json(cli: &Cli) -> bool {
    use std::io::IsTerminal;
    if cli.json {
        return true;
    }
    if cli.text {
        return false;
    }
    !std::io::stdout().is_terminal()
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
    /// Extract every file from an Xbox / Xbox 360 XISO disc image to a
    /// local directory. Useful for inspecting an ISO's contents or for
    /// feeding loose game files to alt dashboards.
    Extract {
        /// Source XISO file
        iso: PathBuf,
        /// Destination directory (created if missing)
        dest: PathBuf,
        /// Skip the `$SystemUpdate` folder (dashboard update payload that
        /// alt dashboards never run). On by default; pass
        /// `--keep-systemupdate` to write it out anyway.
        #[arg(long, action = clap::ArgAction::SetTrue)]
        keep_systemupdate: bool,
        /// Print what would be extracted without writing anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Extract every file from an Xbox 360 STFS package (CON / LIVE / PIRS)
    /// to a local directory. Works on Arcade (XBLA), XBLIG, Title Updates,
    /// Marketplace DLC, and other type-1 packages.
    ExtractStfs {
        /// Source STFS package
        package: PathBuf,
        /// Destination directory (created if missing). Defaults to
        /// `./<title-name> [<TitleID>]/` (or `./<file-stem>/` on catalog miss).
        #[arg(long)]
        to: Option<PathBuf>,
        /// Print the file list and totals without writing anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Convert an Xbox 360 XISO into a Games-on-Demand package in a local
    /// directory. Writes `<dest>/<TitleID>/<ContentType>/<MediaID>{,.data/}`.
    God {
        /// Source XISO file
        iso: PathBuf,
        /// Destination directory (the title-id tree lands underneath)
        dest: PathBuf,
        /// How much of the source partition to pack:
        ///   `compact` (default) — rebuild a dense XDVDFS image first, then
        ///   convert that compact image into GoD.
        ///   `preserve-layout` — walk the file tree, pack only
        ///   through the highest used extent while preserving mastered
        ///   holes inside the XDVDFS layout.
        ///   `none` — pack everything from the start of the data partition
        ///   to the end of the source file.
        #[arg(long, value_parser = ["compact", "preserve-layout", "none"], default_value = "compact")]
        trim: String,
        /// Print the parsed metadata (TitleID, MediaID, data_size, part_count)
        /// without writing anything.
        #[arg(long)]
        dry_run: bool,
        /// Override the human-readable title written into the CON header.
        /// Defaults to the catalog name for the parsed TitleID, or blank
        /// if the catalog doesn't know it.
        #[arg(long)]
        game_title: Option<String>,
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
/// Used by both `interactive_mode` and guided `xtafkit mount`.
fn guided_partition_selection() -> Option<SelectedPartition> {
    // Check for sudo
    if !running_as_root() {
        println!("[!] You're not running as root. Raw device access requires sudo.");
        println!("    Re-run with: sudo xtafkit");
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

    let device_path = if disks.len() == 1 {
        // Exactly one external disk — skip the picker and use it directly.
        let path = disks[0].replace("/dev/disk", "/dev/rdisk");
        println!("Found a single external disk; using it automatically:");
        println!("  {}\n", path);
        PathBuf::from(path)
    } else {
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

        loop {
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
        }
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
                        "  You can try a deep scan with: xtafkit scan {} --deep\n",
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
            if let Ok(n) = input.parse::<usize>()
                && n >= 1
                && n <= partitions.len()
            {
                break &partitions[n - 1];
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

/// No-arguments entry point: pick a partition via the guided flow and
/// hand off to the TUI.
fn interactive_mode() {
    println!();
    println!("=================================================");
    println!("  xtafkit — Xbox FATX/XTAF filesystem workbench  ");
    println!("=================================================");
    println!();

    let Some(sel) = guided_partition_selection() else {
        return;
    };

    let file = match OpenOptions::new()
        .read(true)
        .write(true)
        .open(&sel.device_path)
    {
        Ok(f) => f,
        Err(e) => {
            eprintln!("Error opening '{}': {}", sel.device_path.display(), e);
            return;
        }
    };

    #[cfg(target_os = "macos")]
    let raw_fd = {
        use std::os::unix::io::AsRawFd;
        file.as_raw_fd()
    };

    let mut vol = match FatxVolume::open(file, sel.offset, sel.size) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("Error opening FATX volume: {}", e);
            return;
        }
    };

    #[cfg(target_os = "macos")]
    vol.configure_device(raw_fd);

    let device_display = sel.device_path.display().to_string();
    if let Err(e) = tui::run_browser(vol, &sel.partition_name, &device_display) {
        eprintln!("TUI error: {}", e);
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
                if line.starts_with("/dev/disk")
                    && line.contains("external")
                    && let Some(dev) = line.split_whitespace().next()
                {
                    // Remove trailing colon if present
                    let dev = dev.trim_end_matches(':');
                    disks.push(dev.to_string());
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
                "Run: xtafkit scan {} to see available partitions.",
                device.display()
            );

            // Try to auto-detect and list partitions
            if let Ok(mut f) = std::fs::File::open(device) {
                let dev_size = get_device_size(&mut f);
                if let Ok(parts) = fatxlib::partition::detect_xbox_partitions(&mut f, dev_size)
                    && !parts.is_empty()
                {
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
                        "Example: sudo xtafkit browse {} --partition \"{}\"",
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

    // mkimage initialises its own logger with a friendlier format.
    // Only init the CLI logger for other subcommands.
    let is_mkimage = matches!(cli.command, Some(Commands::Mkimage(_)));
    if !is_mkimage {
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

    let json = want_json(&cli);

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
                println!("  xtafkit browse — guided setup");
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
                match fatxlib::partition::scan_for_fatx(&mut file, dev_size, deep_limit) {
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
                } else {
                    eprintln!("Error: {}", e);
                }
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
                } else {
                    eprintln!("Error: {}", e);
                }
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
            use fatxlib::display::{FolderSlot, folder_slot};
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
                match fatxlib::titles::dynamic::scan_folder_files(&mut vol, &path, !no_save) {
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
            let outcome = fatxlib::titles::dynamic::resolve_and_cache(&mut vol, &path, !no_save);

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
                Ok(fatxlib::titles::dynamic::ResolveOutcome::BadTitleIdInPath { last_segment }) => {
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

        Some(Commands::Extract {
            iso,
            dest,
            keep_systemupdate,
            dry_run,
        }) => run_extract(&iso, &dest, keep_systemupdate, dry_run, json),

        Some(Commands::God {
            iso,
            dest,
            trim,
            dry_run,
            game_title,
        }) => run_god(&iso, &dest, &trim, dry_run, game_title.as_deref(), json),

        Some(Commands::ExtractStfs {
            package,
            to,
            dry_run,
        }) => extract_stfs::run_extract_stfs(&package, to.as_deref(), dry_run, json),
    }
}

// ===========================================================================
// `xtafkit extract` — XISO → local directory
// ===========================================================================

fn run_extract(
    iso: &std::path::Path,
    dest: &std::path::Path,
    keep_systemupdate: bool,
    dry_run: bool,
    json: bool,
) {
    use std::io::BufWriter;
    use std::time::Instant;

    let file = match File::open(iso) {
        Ok(f) => f,
        Err(e) => {
            cli_error(json, &format!("open {}: {}", iso.display(), e));
            return;
        }
    };
    let mut img = match fatxlib::iso::image::XisoImage::open(file) {
        Ok(i) => i,
        Err(e) => {
            cli_error(json, &format!("parse {}: {}", iso.display(), e));
            return;
        }
    };
    let plan = match fatxlib::iso::manifest::build_manifest(
        &mut img,
        fatxlib::iso::manifest::IsoFilterPolicy { keep_systemupdate },
    ) {
        Ok(plan) => plan,
        Err(e) => {
            cli_error(json, &format!("walk {}: {}", iso.display(), e));
            return;
        }
    };
    let total_files = plan.kept_files();
    let total_bytes = plan.kept_bytes;
    let skipped_files = plan.skipped_files();
    let skipped_bytes = plan.skipped_bytes;

    if dry_run {
        if json {
            println!(
                "{}",
                serde_json::json!({
                    "iso": iso.display().to_string(),
                    "layout": plan.layout,
                    "files": total_files,
                    "bytes": total_bytes,
                    "skipped_files": skipped_files,
                    "skipped_bytes": skipped_bytes,
                    "entries": plan.entries.iter().map(|e| {
                        serde_json::json!({
                            "path": e.file.path,
                            "offset": e.file.offset,
                            "size": e.file.size,
                            "skipped": e.skipped,
                        })
                    }).collect::<Vec<_>>(),
                })
            );
        } else {
            println!("ISO:      {}", iso.display());
            println!("Layout:   {}", plan.layout);
            println!("Files:    {} ({})", total_files, format_size(total_bytes));
            if skipped_files > 0 {
                println!(
                    "Skipped:  {} files in $SystemUpdate ({})",
                    skipped_files,
                    format_size(skipped_bytes)
                );
            }
            println!();
            for e in &plan.entries {
                let tag = if e.skipped { "skip " } else { "keep " };
                println!(
                    "  {} {:48}  @0x{:010X}  {}",
                    tag,
                    e.file.path,
                    e.file.offset,
                    format_size(e.file.size)
                );
            }
            println!();
            println!("(dry-run; nothing written)");
        }
        return;
    }

    if let Err(e) = std::fs::create_dir_all(dest) {
        cli_error(json, &format!("create_dir_all {}: {}", dest.display(), e));
        return;
    }

    let started = Instant::now();
    let mut files_done = 0usize;
    let mut bytes_done: u64 = 0;
    let last_progress = std::cell::Cell::new(Instant::now());

    for e in plan.kept() {
        let normalized = e.path.replace('\\', "/");
        let local = dest.join(&normalized);
        if let Some(parent) = local.parent()
            && let Err(err) = std::fs::create_dir_all(parent)
        {
            cli_error(
                json,
                &format!("create_dir_all {}: {}", parent.display(), err),
            );
            return;
        }
        let out = match File::create(&local) {
            Ok(f) => BufWriter::new(f),
            Err(err) => {
                cli_error(json, &format!("create {}: {}", local.display(), err));
                return;
            }
        };
        let mut out = out;
        let bytes_done_ref = &mut bytes_done;
        let last_progress_ref = &last_progress;
        let mut cb = |read: u64, _total: u64| {
            // Throttled per-file byte progress for stderr.
            if !json && last_progress_ref.get().elapsed().as_millis() > 250 {
                eprint!(
                    "\r  [{}/{}] {} ({}/{})         ",
                    files_done + 1,
                    total_files,
                    short_name(&normalized),
                    format_size(*bytes_done_ref + read),
                    format_size(total_bytes),
                );
                let _ = io::stderr().flush();
                last_progress_ref.set(Instant::now());
            }
        };
        let written = match img.read_into(e, &mut out, None, Some(&mut cb)) {
            Ok(n) => n,
            Err(err) => {
                if !json {
                    eprintln!();
                }
                cli_error(json, &format!("read {}: {}", e.path, err));
                return;
            }
        };
        if let Err(err) = out.flush() {
            cli_error(json, &format!("flush {}: {}", local.display(), err));
            return;
        }
        bytes_done += written;
        files_done += 1;
    }
    let elapsed = started.elapsed();
    if !json {
        eprint!("\r{:80}\r", "");
    }
    if json {
        println!(
            "{}",
            serde_json::json!({
                "iso": iso.display().to_string(),
                "dest": dest.display().to_string(),
                "files": files_done,
                "bytes": bytes_done,
                "skipped_files": skipped_files,
                "skipped_bytes": skipped_bytes,
                "elapsed_secs": elapsed.as_secs_f64(),
            })
        );
    } else {
        println!(
            "Extracted {} files ({}) → {} in {:?}",
            files_done,
            format_size(bytes_done),
            dest.display(),
            elapsed,
        );
        if skipped_files > 0 {
            println!(
                "Skipped {} files in $SystemUpdate ({})",
                skipped_files,
                format_size(skipped_bytes)
            );
        }
    }
}

fn short_name(p: &str) -> &str {
    p.rsplit('/').next().unwrap_or(p)
}

// ===========================================================================
// `xtafkit god` — XISO → local Games-on-Demand package
// ===========================================================================

fn run_god(
    iso: &std::path::Path,
    dest: &std::path::Path,
    trim: &str,
    dry_run: bool,
    game_title: Option<&str>,
    json: bool,
) {
    use std::time::Instant;

    let trim_mode = match trim {
        "preserve-layout" => fatxlib::iso::god::TrimMode::PreserveLayout,
        "none" => fatxlib::iso::god::TrimMode::None,
        "compact" => fatxlib::iso::god::TrimMode::Compact,
        other => {
            cli_error(json, &format!("invalid --trim {:?}", other));
            return;
        }
    };

    // Catalog-fill the game title from the dry-run report, unless the
    // caller passed --game-title explicitly.
    let mut dry_opts = fatxlib::iso::god::ConvertOptions {
        trim: trim_mode,
        dry_run: true,
        ..Default::default()
    };
    let report = match fatxlib::iso::god::convert_iso(iso, dest, &mut dry_opts) {
        Ok(r) => r,
        Err(e) => {
            cli_error(json, &format!("parse {}: {}", iso.display(), e));
            return;
        }
    };
    let resolved_name = fatxlib::titles::lookup(report.title_id).map(|t| t.name);
    let effective_title = game_title.or(resolved_name);

    if dry_run {
        if json {
            println!(
                "{}",
                serde_json::json!({
                    "iso": iso.display().to_string(),
                    "title_id": format!("{:08X}", report.title_id),
                    "media_id": format!("{:08X}", report.media_id),
                    "name": resolved_name.unwrap_or("(unknown)"),
                    "content_type": format!("{:?}", report.content_type),
                    "data_size": report.data_size,
                    "block_count": report.block_count,
                    "part_count": report.part_count,
                })
            );
        } else {
            println!("ISO:         {}", iso.display());
            println!("Title ID:    {:08X}", report.title_id);
            println!("Media ID:    {:08X}", report.media_id);
            println!(
                "Name:        {}",
                resolved_name.unwrap_or("(unknown — catalog miss)")
            );
            println!("Content:     {:?}", report.content_type);
            println!(
                "Data size:   {} bytes ({})",
                report.data_size,
                format_size(report.data_size)
            );
            println!("Block count: {}", report.block_count);
            println!("Part count:  {}", report.part_count);
            println!();
            println!("(dry-run; nothing written)");
        }
        return;
    }

    if let Err(e) = std::fs::create_dir_all(dest) {
        cli_error(json, &format!("create_dir_all {}: {}", dest.display(), e));
        return;
    }

    let started = Instant::now();
    let last_progress = std::cell::Cell::new(Instant::now());
    let mut last_stage = String::new();
    let mut progress_cb = |stage: &str, current: u64, total: u64| {
        let stage_changed = stage != last_stage;
        if json {
            return;
        }
        if stage_changed || last_progress.get().elapsed().as_millis() > 250 {
            if stage.starts_with("part ") {
                eprint!(
                    "\r  [{}] {} / {}            ",
                    stage,
                    format_size(current),
                    format_size(total)
                );
            } else {
                eprint!("\r  [{}] {}/{}            ", stage, current, total);
            }
            let _ = io::stderr().flush();
            last_progress.set(Instant::now());
            last_stage = stage.to_string();
        }
    };

    let mut opts = fatxlib::iso::god::ConvertOptions {
        trim: trim_mode,
        game_title: effective_title,
        dry_run: false,
        progress: Some(&mut progress_cb),
        should_abort: None,
    };

    let result = fatxlib::iso::god::convert_iso(iso, dest, &mut opts);
    if !json {
        eprint!("\r{:80}\r", "");
    }
    let elapsed = started.elapsed();
    match result {
        Ok(r) => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "iso": iso.display().to_string(),
                        "dest": dest.display().to_string(),
                        "title_id": format!("{:08X}", r.title_id),
                        "media_id": format!("{:08X}", r.media_id),
                        "name": resolved_name.unwrap_or("(unknown)"),
                        "content_type": format!("{:?}", r.content_type),
                        "data_size": r.data_size,
                        "block_count": r.block_count,
                        "part_count": r.part_count,
                        "elapsed_secs": elapsed.as_secs_f64(),
                    })
                );
            } else {
                let resolved_label = resolved_name.unwrap_or("(unknown — catalog miss)");
                println!(
                    "ISO:         {}\nTitle ID:    {:08X}\nMedia ID:    {:08X}\nName:        {}\nContent:     {:?}\nData size:   {} bytes ({})\nBlock count: {}\nPart count:  {}\nDest:        {}\nElapsed:     {:?}",
                    iso.display(),
                    r.title_id,
                    r.media_id,
                    resolved_label,
                    r.content_type,
                    r.data_size,
                    format_size(r.data_size),
                    r.block_count,
                    r.part_count,
                    dest.display(),
                    elapsed
                );
            }
        }
        Err(e) => cli_error(json, &format!("convert_iso: {}", e)),
    }
}

fn cli_error(json: bool, msg: &str) {
    if json {
        println!("{}", serde_json::json!({"error": msg}));
        process::exit(0);
    }
    eprintln!("Error: {}", msg);
    process::exit(1);
}
