//! xtafkit: Command-line + TUI tool for interacting with Xbox FATX/XTAF file systems.
//!
//! Run with no arguments for interactive mode, or use subcommands directly:
//!   xtafkit                     # Interactive guided mode
//!   xtafkit browse /dev/rdisk4  # TUI file browser
//!   xtafkit scan /dev/rdisk4
//!   xtafkit ls /dev/rdisk4 --partition "Data (E)" /

mod mkimage;
mod tui;

use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, Read as IoRead, Seek, SeekFrom, Write as IoWrite};
use std::path::PathBuf;
use std::process::{self, Command};

use clap::{Parser, Subcommand};
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

/// No-arguments entry point: pick a partition via the guided flow and
/// hand off to the TUI.
fn interactive_mode() {
    println!();
    println!("========================================");
    println!("  xtafkit — Xbox FATX/XTAF filesystem workbench  ");
    println!("========================================");
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
                "Run: xtafkit scan {} to see available partitions.",
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
