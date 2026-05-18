//! TUI file browser for FATX volumes.
//!
//! Architecture: two-thread model with channel communication.
//! - UI thread: draws the terminal, reads keypresses, sends commands
//! - I/O thread: owns FatxVolume, executes commands, sends responses
//!
//! Keyboard:
//!   ↑ / k             Move selection up
//!   ↓ / j             Move selection down
//!   Home              Jump to first entry
//!   End               Jump to last entry
//!   PageUp            Move selection up by one page
//!   PageDown          Move selection down by one page
//!   Enter / →         Open directory / show file info
//!   Backspace / ←     Go up one directory
//!   d                 Download selected file to local disk
//!   u                 Upload a local file or directory into current directory.
//!                     If the file parses as an XDVDFS/XISO disc image, the
//!                     TUI asks how to bring it onto the drive:
//!                       (x)tract — stream the file tree into <cwd>/<stem>/
//!                       (g)oD    — convert to a Games-on-Demand package
//!                                  rooted at <cwd>/<TitleID>/00007000/...
//!                       (r)aw    — copy the source ISO byte-for-byte
//!                     Default is GoD when cwd is inside `/Content/<XUID>/`,
//!                     extract otherwise.
//!   m                 Create new directory (mkdir)
//!   D                 Delete selected file/directory
//!   r                 Rename selected file/directory
//!   R                 Resolve names from STFS headers (entries needing
//!                     resolution are marked with `?`).
//!                     - Inside `/Content/<XUID>`: resolve the selected
//!                       title-ID folder; cached at
//!                       `~/.config/xtafkit/user_titles.txt`.
//!                     - Inside an Arcade / XNA / Marketplace / Installer
//!                       folder: bulk-scan every file in the current
//!                       directory; cached at
//!                       `~/.config/xtafkit/user_files.txt`.
//!   s                 Toggle sort order between by-name and by-id; also
//!                     flips which side of the bracket is shown first.
//!   i                 Show volume info
//!   c                 Clean up macOS metadata from current directory
//!   Esc               Cancel a running I/O operation (when busy) / quit (when idle)
//!   q                 Quit

use std::fs;
use std::io::{self, stdout};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, TryRecvError};
use std::time::Duration;

use crossterm::{
    ExecutableCommand,
    cursor::{Hide, Show},
    event::{self, Event, KeyCode, KeyEvent},
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
};

use fatxlib::iso::image::XisoImage;
use fatxlib::partition::format_size;
use fatxlib::stfs::StfsPackage;
use fatxlib::types::FileAttributes;
use fatxlib::volume::FatxVolume;

// ===========================================================================
// Display types
// ===========================================================================

#[allow(dead_code)]
struct DisplayEntry {
    /// Raw on-disk filename. Used for navigation and download paths.
    name: String,
    /// Resolved name (just the name part, without brackets) when the slot
    /// resolver had a match. `None` for unresolved or non-resolvable entries.
    resolved: Option<String>,
    is_dir: bool,
    size: u64,
    modified: String,
    attributes: String,
    first_cluster: u32,
}

impl DisplayEntry {
    /// Whether this entry has a slot-resolved name available.
    fn is_resolved(&self) -> bool {
        self.resolved.is_some()
    }

    /// Render the entry's label for the given sort mode.
    /// `ByName` shows `<resolved> [<raw>]`; `ById` shows `<raw> [<resolved>]`.
    /// Unresolved entries always render as just the raw name.
    fn label(&self, mode: SortMode) -> String {
        match (&self.resolved, mode) {
            (Some(name), SortMode::ByName) => format!("{} [{}]", name, self.name),
            (Some(name), SortMode::ById) => format!("{} [{}]", self.name, name),
            (None, _) => self.name.clone(),
        }
    }

    /// Primary sort key for the given mode.
    fn sort_key<'a>(&'a self, mode: SortMode) -> std::borrow::Cow<'a, str> {
        match (&self.resolved, mode) {
            (Some(name), SortMode::ByName) => std::borrow::Cow::Borrowed(name),
            (_, SortMode::ById) => std::borrow::Cow::Borrowed(&self.name),
            (None, SortMode::ByName) => std::borrow::Cow::Borrowed(&self.name),
        }
    }
}

/// What to do with a local XISO that's being uploaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum XisoUploadAction {
    /// Walk the XISO and stream each file into `<cwd>/<iso-stem>/`.
    /// Default when cwd is anywhere other than directly inside an XUID
    /// folder (e.g. `/Games/`, `/`, an arbitrary user folder).
    Extract,
    /// Convert the XISO into a Games-on-Demand package — writes
    /// `<cwd>/<TitleID>/00007000/<MediaID>{,.data/}`. Default when cwd
    /// is directly inside `/Content/<XUID>/`, since that's exactly
    /// where Xbox 360 BC looks for GoD packages.
    God,
    /// Copy the source ISO byte-for-byte to `<cwd>/<filename>`. Useful
    /// when the user wants to preserve the disc image as-is for later
    /// extraction or conversion elsewhere.
    Raw,
}

/// What to do with a local STFS package that's being uploaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StfsUploadAction {
    /// Walk the package and stream each file into `<cwd>/<DisplayName>/`.
    /// Only offered when `default.xex` is present at the package root —
    /// the only reliable signal that loose extraction produces something
    /// alt-dashboards can launch.
    Extract,
    /// Copy the source bytes byte-for-byte to `<cwd>/<filename>`.
    Raw,
}

/// How to order the directory listing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SortMode {
    /// `<resolved-name> [<raw-id>]`, ordered alphabetically by resolved
    /// name (falling back to raw for unresolved entries).
    ByName,
    /// `<raw-id> [<resolved-name>]`, ordered by the on-disk raw value.
    ById,
}

// ===========================================================================
// Channel protocol
// ===========================================================================

/// Commands sent from UI thread → I/O thread.
enum IoCmd {
    ListDir {
        path: String,
    },
    ReadFile {
        fatx_path: String,
        local_path: PathBuf,
    },
    WriteFile {
        local_path: PathBuf,
        fatx_path: String,
    },
    CopyDir {
        local_path: PathBuf,
        fatx_dest: String,
    },
    /// Open `source` as an XDVDFS image and stream every file inside it into
    /// `dest_dir` on the FATX volume, recreating the directory tree. The dest
    /// directory itself is created by the worker; it must not already exist.
    ExtractXiso {
        source: PathBuf,
        dest_dir: String,
    },
    /// Open `source` as an STFS package and stream every inner file into
    /// `<dest_dir>` on the FATX volume. The dest directory is created by
    /// the worker; it must not already exist.
    ExtractStfsToFatx {
        source: PathBuf,
        dest_dir: String,
    },
    /// Convert `source` (an XDVDFS image) to a Games-on-Demand package
    /// rooted at `dest_dir` on the FATX volume. Writes
    /// `<dest_dir>/<TitleID>/00007000/<MediaID>{,.data/Data0000..N}`.
    /// The worker resolves the human-readable game title from
    /// [`fatxlib::titles`] before writing the CON header.
    ConvertXisoToGod {
        source: PathBuf,
        dest_dir: String,
    },
    Mkdir {
        path: String,
    },
    ResolveTitle {
        /// Path to a title-ID folder (e.g. `/Content/<XUID>/<TitleID>`).
        path: String,
    },
    /// Bulk-scan every STFS file directly inside `path` and cache results.
    /// Used inside Arcade / XNA / Marketplace / Installer folders.
    ScanFolderFiles {
        path: String,
    },
    Delete {
        path: String,
        recursive: bool,
    },
    Rename {
        path: String,
        new_name: String,
    },
    ScanCleanup {
        path: String,
    },
    DeleteCleanup {
        paths: Vec<String>,
    },
    Stats,
    Flush,
    Shutdown,
}

/// Responses sent from I/O thread → UI thread.
#[allow(dead_code)]
enum IoResp {
    DirListing {
        entries: Vec<DisplayEntry>,
        path: String,
    },
    Progress {
        message: String,
    },
    Done {
        message: String,
    },
    Error {
        message: String,
    },
    StatsResult {
        total_clusters: u32,
        free_clusters: u32,
        used_clusters: u32,
        cluster_size: u64,
        free_size: u64,
        used_size: u64,
    },
    CleanupScanResult {
        entries: Vec<(String, bool, u64)>, // (path, is_dir, size)
    },
    Flushed,
    Cancelled {
        message: String,
    },
}

// ===========================================================================
// App state
// ===========================================================================

#[derive(Clone, Copy, PartialEq)]
#[allow(dead_code)]
enum InputMode {
    Normal,
    DownloadPath,
    UploadPath,
    MkdirName,
    RenameName,
    ConfirmDelete,
    ConfirmCleanup,
    /// Three-way prompt after detecting an XISO during upload:
    /// `x` extracts the contents into a stem-named subfolder,
    /// `g` converts to a Games-on-Demand package (Title-ID tree under cwd),
    /// `r` falls back to a raw byte copy of the source file.
    /// The default action on bare Enter depends on cwd context.
    ConfirmXisoUpload,
    /// Two-way prompt after detecting an extractable STFS package during
    /// upload: `x` extracts the contents into `<cwd>/<DisplayName>/`,
    /// `r` falls back to a raw byte copy of the source file.
    ConfirmStfsUpload,
}

struct App {
    cwd: String,
    entries: Vec<DisplayEntry>,
    list_state: ListState,
    status: String,
    status_is_error: bool,
    partition_name: String,
    device_display: String,
    should_quit: bool,
    input_mode: InputMode,
    input_buffer: String,
    input_prompt: String,
    download_dir: PathBuf,
    /// True when an I/O operation is running in the background.
    is_busy: bool,
    /// Shared cancel flag — set by UI, checked by I/O worker.
    cancel_flag: Arc<AtomicBool>,
    /// Pending cleanup paths awaiting user confirmation.
    pending_cleanup: Vec<(String, bool, u64)>,
    /// Pending single-delete target captured when the prompt is opened.
    pending_delete: Option<(String, bool)>,
    /// Local XISO path + default action stashed between the upload prompt
    /// and the three-way confirmation prompt (extract / GoD / raw).
    pending_xiso_upload: Option<(PathBuf, XisoUploadAction)>,
    /// Local STFS path + destination subfolder name stashed between the
    /// upload prompt and the extract/raw confirmation prompt.
    pending_stfs_upload: Option<(PathBuf, StfsExtractTarget)>,
    /// Current listing sort order. Toggleable with `s`.
    sort_mode: SortMode,
}

/// Unescape shell-style backslash escapes in a path string.
/// Converts `Call\ of\ Duty` → `Call of Duty`, `file\\name` → `file\name`.
fn unescape_path(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(&next) = chars.peek() {
                result.push(next);
                chars.next();
            }
        } else {
            result.push(c);
        }
    }
    result
}

/// Sniff whether `path` looks like an Xbox XDVDFS disc image by trying to
/// parse a volume descriptor at one of the known XGD layout offsets.
/// Cheap — only reads a handful of sectors near the volume descriptor.
fn is_xiso(path: &std::path::Path) -> bool {
    match std::fs::File::open(path) {
        Ok(file) => XisoImage::open(file).is_ok(),
        Err(_) => false,
    }
}

/// Information needed to extract an STFS package to the FATX volume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StfsExtractTarget {
    /// Sanitised subfolder name to create under cwd. Derived from the
    /// package's STFS `display_name` (falling back to `title_name`, then
    /// the local filename stem).
    pub dest_name: String,
}

/// Returns `Some(target)` if `path` is a type-1 STFS package containing a
/// depth-0 `default.xex`. The sniff opens the package, validates the
/// header, walks the file table, and closes the file before returning.
///
/// Returns `None` for: not an STFS file, type-0 (read-write) packages,
/// truncated/corrupt headers, packages without a root `default.xex`,
/// and any I/O error during the walk. Errors are swallowed deliberately —
/// the sniff is best-effort; if anything goes wrong the user falls
/// through to the raw-upload path.
fn stfs_extract_target(path: &std::path::Path) -> Option<StfsExtractTarget> {
    let file = std::fs::File::open(path).ok()?;
    let mut pkg = StfsPackage::open(file).ok()?;
    if !pkg.has_default_xex().ok()? {
        return None;
    }
    let display = pkg.header().display_name.trim().to_string();
    let title = pkg.header().title_name.trim().to_string();
    let raw = if !display.is_empty() {
        display
    } else if !title.is_empty() {
        title
    } else {
        path.file_stem()?.to_string_lossy().into_owned()
    };
    let sanitized = sanitize_fatx_filename(&raw);
    if sanitized.is_empty() {
        return None;
    }
    Some(StfsExtractTarget {
        dest_name: sanitized,
    })
}

/// Resolve the destination folder name for an XISO extract by reading the
/// embedded `Default.xex` / `default.xbe` and looking the TitleID up in
/// [`fatxlib::titles`]. Returns the catalog-known game name, sanitized for
/// FATX's filename rules. Returns `None` if the image has no parsable
/// executable, the TitleID isn't in the catalog, or the resulting name is
/// empty after sanitization — callers should fall back to the file stem.
fn xiso_folder_name(path: &std::path::Path) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let mut img = XisoImage::open(file).ok()?;
    let info = img.title_info().ok().flatten()?;
    let resolved = fatxlib::titles::lookup(info.execution_info.title_id)?;
    let sanitized = sanitize_fatx_filename(resolved.name);
    if sanitized.is_empty() {
        None
    } else {
        Some(sanitized)
    }
}

/// Coerce a free-form string into something FATX will accept as a filename.
/// Replaces characters the filesystem rejects with `-`, collapses runs of
/// whitespace, trims edge punctuation, and truncates to FATX's 42-byte
/// filename limit.
fn sanitize_fatx_filename(raw: &str) -> String {
    const MAX_LEN: usize = 42;
    // FATX rejects: < > : " / \ | ? *  (plus controls). Replace with '-'.
    let mut cleaned: String = raw
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '-',
            c if c.is_control() => '-',
            c => c,
        })
        .collect();
    while cleaned.contains("  ") {
        cleaned = cleaned.replace("  ", " ");
    }
    let trimmed = cleaned.trim_matches(['.', ' ']);
    if trimmed.len() <= MAX_LEN {
        trimmed.to_string()
    } else {
        trimmed
            .chars()
            .take(MAX_LEN)
            .collect::<String>()
            .trim_end_matches(['.', ' ', '-'])
            .to_string()
    }
}

fn dirs_or_home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

impl App {
    fn new(partition_name: &str, device_display: &str, cancel_flag: Arc<AtomicBool>) -> Self {
        Self {
            cwd: "/".to_string(),
            entries: Vec::new(),
            list_state: ListState::default(),
            status: "Loading...".to_string(),
            status_is_error: false,
            partition_name: partition_name.to_string(),
            device_display: device_display.to_string(),
            should_quit: false,
            input_mode: InputMode::Normal,
            input_buffer: String::new(),
            input_prompt: String::new(),
            download_dir: dirs_or_home(),
            is_busy: false,
            cancel_flag,
            pending_cleanup: Vec::new(),
            pending_delete: None,
            pending_xiso_upload: None,
            pending_stfs_upload: None,
            sort_mode: SortMode::ByName,
        }
    }

    fn selected_entry(&self) -> Option<&DisplayEntry> {
        self.list_state.selected().and_then(|i| self.entries.get(i))
    }

    fn selected_name(&self) -> Option<String> {
        self.selected_entry().map(|e| e.name.clone())
    }

    fn set_status(&mut self, msg: &str) {
        self.status = msg.to_string();
        self.status_is_error = false;
    }

    fn set_error(&mut self, msg: &str) {
        self.status = msg.to_string();
        self.status_is_error = true;
    }

    fn full_path(&self, name: &str) -> String {
        if self.cwd == "/" {
            format!("/{}", name)
        } else {
            format!("{}/{}", self.cwd, name)
        }
    }

    /// Sort `self.entries` in place using the active [`SortMode`].
    /// Directories always come first; the mode's `sort_key` is the secondary
    /// criterion.
    fn resort_entries(&mut self) {
        let mode = self.sort_mode;
        self.entries.sort_by(|a, b| {
            b.is_dir.cmp(&a.is_dir).then_with(|| {
                a.sort_key(mode)
                    .to_lowercase()
                    .cmp(&b.sort_key(mode).to_lowercase())
            })
        });
    }
}

// ===========================================================================
// I/O Worker Thread
// ===========================================================================

fn io_worker(
    mut vol: FatxVolume<std::fs::File>,
    cmd_rx: mpsc::Receiver<IoCmd>,
    resp_tx: mpsc::Sender<IoResp>,
    cancel_flag: Arc<AtomicBool>,
) {
    while let Ok(cmd) = cmd_rx.recv() {
        match cmd {
            IoCmd::ListDir { path } => {
                let entry = match vol.resolve_path(&path) {
                    Ok(e) => e,
                    Err(e) => {
                        let _ = resp_tx.send(IoResp::Error {
                            message: format!("Error: {}", e),
                        });
                        continue;
                    }
                };

                match vol.read_directory(entry.first_cluster) {
                    Ok(entries) => {
                        // Eager: when listing /Content, probe each personal
                        // XUID for a profile package so the gamertag is
                        // already in the cache by the time we render.
                        if fatxlib::display::folder_slot(&path)
                            == fatxlib::display::FolderSlot::Xuid
                        {
                            let _ = fatxlib::xuids::resolve_profile_xuids(&mut vol, &entries, true);
                        }
                        let display: Vec<DisplayEntry> = entries
                            .iter()
                            .map(|e| {
                                let attr = format!(
                                    "{}{}{}{}",
                                    if e.is_directory() { "d" } else { "-" },
                                    if e.attributes.contains(FileAttributes::READ_ONLY) {
                                        "r"
                                    } else {
                                        "-"
                                    },
                                    if e.attributes.contains(FileAttributes::HIDDEN) {
                                        "h"
                                    } else {
                                        "-"
                                    },
                                    if e.attributes.contains(FileAttributes::SYSTEM) {
                                        "s"
                                    } else {
                                        "-"
                                    },
                                );
                                let raw = e.filename();
                                let resolved =
                                    fatxlib::display::resolved_name_for_path(&path, &raw);
                                DisplayEntry {
                                    name: raw,
                                    resolved,
                                    is_dir: e.is_directory(),
                                    size: e.file_size as u64,
                                    modified: e.write_datetime_str(),
                                    attributes: attr,
                                    first_cluster: e.first_cluster,
                                }
                            })
                            .collect();

                        // The UI thread owns sort order — it can re-sort
                        // on toggle without round-tripping to the worker.
                        let _ = resp_tx.send(IoResp::DirListing {
                            entries: display,
                            path,
                        });
                    }
                    Err(e) => {
                        let _ = resp_tx.send(IoResp::Error {
                            message: format!("Error reading directory: {}", e),
                        });
                    }
                }
            }

            IoCmd::ReadFile {
                fatx_path,
                local_path,
            } => match vol.read_file_by_path(&fatx_path) {
                Ok(data) => {
                    if let Some(parent) = local_path.parent() {
                        let _ = fs::create_dir_all(parent);
                    }
                    match fs::write(&local_path, &data) {
                        Ok(_) => {
                            let _ = resp_tx.send(IoResp::Done {
                                message: format!(
                                    "Downloaded '{}' → {} ({})",
                                    fatx_path,
                                    local_path.display(),
                                    format_size(data.len() as u64)
                                ),
                            });
                        }
                        Err(e) => {
                            let _ = resp_tx.send(IoResp::Error {
                                message: format!("Write error: {}", e),
                            });
                        }
                    }
                }
                Err(e) => {
                    let _ = resp_tx.send(IoResp::Error {
                        message: format!("Read error: {}", e),
                    });
                }
            },

            IoCmd::WriteFile {
                local_path,
                fatx_path,
            } => match fs::read(&local_path) {
                Ok(data) => {
                    let size = data.len() as u64;
                    match vol.create_or_replace_file(&fatx_path, &data) {
                        Ok(_) => {
                            if !flush_or_error(&mut vol, &resp_tx, "Upload flush failed") {
                                let _ = resp_tx.send(IoResp::Done {
                                    message: format!(
                                        "Uploaded '{}' → {} ({})",
                                        local_path.display(),
                                        fatx_path,
                                        format_size(size)
                                    ),
                                });
                            }
                        }
                        Err(e) => {
                            let _ = resp_tx.send(IoResp::Error {
                                message: format!("Upload error: {}", e),
                            });
                        }
                    }
                }
                Err(e) => {
                    let _ = resp_tx.send(IoResp::Error {
                        message: format!("Read local error: {}", e),
                    });
                }
            },

            IoCmd::CopyDir {
                local_path,
                fatx_dest,
            } => {
                // Collect all files first
                let mut file_list: Vec<(PathBuf, String)> = Vec::new();
                collect_files(&local_path, &fatx_dest, &mut file_list);
                let total_files = file_list.len();
                let total_size: u64 = file_list
                    .iter()
                    .filter_map(|(p, _)| fs::metadata(p).ok().map(|m| m.len()))
                    .sum();

                // Create directory structure
                create_dirs_recursive(&mut vol, &local_path, &fatx_dest);

                cancel_flag.store(false, Ordering::Relaxed);
                let mut bytes_done = 0u64;
                let mut files_done = 0usize;
                let mut cancelled = false;
                let mut failed = false;
                let mut files_since_flush = 0usize;
                let mut bytes_since_flush = 0u64;

                for (local_file, fatx_path) in &file_list {
                    if cancel_flag.load(Ordering::Relaxed) {
                        cancelled = true;
                        break;
                    }

                    let file_size = fs::metadata(local_file).map(|m| m.len()).unwrap_or(0);
                    files_done += 1;

                    // Short filename for display
                    let short_name = local_file
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| fatx_path.clone());

                    let _ = resp_tx.send(IoResp::Progress {
                        message: format!(
                            "[{}/{}] {} ({}) — {}/{}",
                            files_done,
                            total_files,
                            short_name,
                            format_size(file_size),
                            format_size(bytes_done),
                            format_size(total_size),
                        ),
                    });

                    let data = match fs::read(local_file) {
                        Ok(d) => d,
                        Err(e) => {
                            let _ = resp_tx.send(IoResp::Error {
                                message: format!("Read {}: {}", local_file.display(), e),
                            });
                            continue;
                        }
                    };

                    match vol.create_or_replace_file(fatx_path, &data) {
                        Ok(_) => {
                            bytes_done += file_size;
                            files_since_flush += 1;
                            bytes_since_flush += file_size;
                        }
                        Err(e) => {
                            let _ = resp_tx.send(IoResp::Error {
                                message: format!("{}: {}", fatx_path, e),
                            });
                            continue;
                        }
                    }

                    if files_since_flush >= 100 || bytes_since_flush >= 256 * 1024 * 1024 {
                        if flush_or_error(&mut vol, &resp_tx, "Periodic flush failed") {
                            failed = true;
                            break;
                        }
                        files_since_flush = 0;
                        bytes_since_flush = 0;
                    }
                }

                if flush_or_error(&mut vol, &resp_tx, "Final flush failed") {
                    failed = true;
                }

                if cancelled {
                    let _ = resp_tx.send(IoResp::Cancelled {
                        message: format!(
                            "Cancelled — {}/{} files uploaded ({})",
                            files_done.saturating_sub(1),
                            total_files,
                            format_size(bytes_done)
                        ),
                    });
                } else if failed {
                    // Error already reported by flush_or_error.
                } else {
                    let _ = resp_tx.send(IoResp::Done {
                        message: format!(
                            "Uploaded {} files ({})",
                            files_done,
                            format_size(bytes_done)
                        ),
                    });
                }
            }

            IoCmd::ExtractXiso { source, dest_dir } => {
                cancel_flag.store(false, Ordering::Relaxed);

                let display_source = source
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| source.display().to_string());

                let file = match fs::File::open(&source) {
                    Ok(f) => f,
                    Err(e) => {
                        let _ = resp_tx.send(IoResp::Error {
                            message: format!("Open {}: {}", source.display(), e),
                        });
                        continue;
                    }
                };
                let mut img = match XisoImage::open(file) {
                    Ok(img) => img,
                    Err(e) => {
                        let _ = resp_tx.send(IoResp::Error {
                            message: format!("Parse {}: {}", source.display(), e),
                        });
                        continue;
                    }
                };
                let plan = match fatxlib::iso::manifest::build_manifest(
                    &mut img,
                    fatxlib::iso::manifest::IsoFilterPolicy {
                        keep_systemupdate: false,
                    },
                ) {
                    Ok(plan) => plan,
                    Err(e) => {
                        let _ = resp_tx.send(IoResp::Error {
                            message: format!("Walk {}: {}", source.display(), e),
                        });
                        continue;
                    }
                };

                // Materialize the destination directory itself before any files.
                if let Err(e) = vol.create_directory(&dest_dir) {
                    let _ = resp_tx.send(IoResp::Error {
                        message: format!("Create '{}': {}", dest_dir, e),
                    });
                    continue;
                }

                let total_files = plan.kept_files();
                let total_bytes = plan.kept_bytes;
                let skipped_files = plan.skipped_files();
                let skipped_bytes = plan.skipped_bytes;

                let mut files_done = 0usize;
                let mut bytes_done: u64 = 0;
                let mut files_since_flush = 0usize;
                let mut bytes_since_flush = 0u64;
                let mut cancelled = false;
                let mut failed = false;

                for entry in plan.kept() {
                    if cancel_flag.load(Ordering::Relaxed) {
                        cancelled = true;
                        break;
                    }

                    // Compose the FATX path; entry.path is image-relative
                    // (no leading slash). Normalize to forward slashes — they
                    // already are, but be defensive.
                    let normalized = entry.path.replace('\\', "/");
                    let fatx_path = format!("{}/{}", dest_dir.trim_end_matches('/'), normalized);

                    // Ensure every parent directory exists.
                    if let Some(parent_end) = fatx_path.rfind('/')
                        && parent_end > 0
                    {
                        let parent = &fatx_path[..parent_end];
                        if let Err(e) = ensure_dir_chain(&mut vol, parent) {
                            let _ = resp_tx.send(IoResp::Error {
                                message: format!("Create dir '{}': {}", parent, e),
                            });
                            failed = true;
                            break;
                        }
                    }

                    files_done += 1;
                    let short_name = normalized
                        .rsplit('/')
                        .next()
                        .unwrap_or(&normalized)
                        .to_string();
                    let _ = resp_tx.send(IoResp::Progress {
                        message: format!(
                            "[{}/{}] {} ({}) — {}/{}",
                            files_done,
                            total_files,
                            short_name,
                            format_size(entry.size),
                            format_size(bytes_done),
                            format_size(total_bytes),
                        ),
                    });

                    let reader = img.file_reader(entry);
                    match vol.create_file_from_reader(&fatx_path, entry.size, reader, None) {
                        Ok(()) => {
                            bytes_done += entry.size;
                            files_since_flush += 1;
                            bytes_since_flush += entry.size;
                        }
                        Err(e) => {
                            let _ = resp_tx.send(IoResp::Error {
                                message: format!("{}: {}", fatx_path, e),
                            });
                            failed = true;
                            break;
                        }
                    }

                    // Flush periodically so a long extract survives a yank.
                    if files_since_flush >= 100 || bytes_since_flush >= 256 * 1024 * 1024 {
                        if flush_or_error(&mut vol, &resp_tx, "Periodic flush failed") {
                            failed = true;
                            break;
                        }
                        files_since_flush = 0;
                        bytes_since_flush = 0;
                    }
                }

                if flush_or_error(&mut vol, &resp_tx, "Final flush failed") {
                    failed = true;
                }

                if cancelled {
                    let _ = resp_tx.send(IoResp::Cancelled {
                        message: format!(
                            "Extract cancelled — {}/{} files written ({})",
                            files_done.saturating_sub(1),
                            total_files,
                            format_size(bytes_done)
                        ),
                    });
                } else if failed {
                    // Error message already sent above; nothing else to do.
                } else {
                    let skipped_note = if skipped_files > 0 {
                        format!(
                            "; skipped $SystemUpdate ({} files, {})",
                            skipped_files,
                            format_size(skipped_bytes)
                        )
                    } else {
                        String::new()
                    };
                    let _ = resp_tx.send(IoResp::Done {
                        message: format!(
                            "Extracted {} → {} ({} files, {}{})",
                            display_source,
                            dest_dir,
                            files_done,
                            format_size(bytes_done),
                            skipped_note,
                        ),
                    });
                }
            }

            IoCmd::ConvertXisoToGod { source, dest_dir } => {
                cancel_flag.store(false, Ordering::Relaxed);

                let display_source = source
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| source.display().to_string());

                let upload_dest = dest_dir.trim_end_matches('/').to_string();

                // Dry-run first so we can resolve the human-readable title
                // and announce the destination before the streaming pass.
                let mut dry_opts = fatxlib::iso::god::ConvertOptions {
                    trim: fatxlib::iso::god::TrimMode::Compact,
                    dry_run: true,
                    ..Default::default()
                };
                let report = match fatxlib::iso::god::convert_iso_to_fatx(
                    &source,
                    &mut vol,
                    &dest_dir,
                    &mut dry_opts,
                ) {
                    Ok(r) => r,
                    Err(e) => {
                        let _ = resp_tx.send(IoResp::Error {
                            message: format!("Parse {}: {}", source.display(), e),
                        });
                        continue;
                    }
                };
                let resolved_name = fatxlib::titles::lookup(report.title_id).map(|t| t.name);

                let _ = resp_tx.send(IoResp::Progress {
                    message: format!(
                        "Converting {} ({}) → {}/{:08X}/00007000/{:08X}...",
                        display_source,
                        resolved_name.unwrap_or("unknown title"),
                        upload_dest,
                        report.title_id,
                        report.media_id,
                    ),
                });

                // Wire progress + cancel hooks. Both closures share the
                // same lifetime so they can co-exist in ConvertOptions.
                let cancel_flag_inner = cancel_flag.clone();
                let abort_fn = move || cancel_flag_inner.load(Ordering::Relaxed);
                let resp_tx_inner = resp_tx.clone();
                let mut last_stage = String::new();
                let mut last_emit_at: Option<std::time::Instant> = None;
                let mut last_emit_bytes: u64 = 0;
                let mut progress_cb = move |stage: &str, current: u64, total: u64| {
                    let stage_changed = stage != last_stage;
                    let now = std::time::Instant::now();

                    // Byte-level stages ("part X/Y"): rate-limit to ~200 ms
                    // intervals, and compute MiB/s from the delta between
                    // emits. Stage transitions always emit so the user sees
                    // each part's first tick immediately.
                    if stage.starts_with("part ") {
                        if !stage_changed
                            && let Some(t) = last_emit_at
                            && now.duration_since(t).as_millis() < 200
                        {
                            return;
                        }
                        let throughput = if !stage_changed {
                            last_emit_at
                                .map(|t| {
                                    let dt = now.duration_since(t).as_secs_f64();
                                    let dbytes = current.saturating_sub(last_emit_bytes);
                                    let rate = (dbytes as f64) / dt / (1024.0 * 1024.0);
                                    format!(" @ {:.1} MiB/s", rate)
                                })
                                .unwrap_or_default()
                        } else {
                            String::new()
                        };
                        let msg = format!(
                            "[{}] {} / {}{}",
                            stage,
                            format_size(current),
                            format_size(total),
                            throughput
                        );
                        let _ = resp_tx_inner.send(IoResp::Progress { message: msg });
                    } else {
                        // Integer-milestone stages (parts / mht / header):
                        // keep the 5 % throttle, render the raw count.
                        let denom = total.max(1);
                        let stride = (denom / 20).max(1);
                        if !stage_changed
                            && current != 0
                            && current != total
                            && !current.is_multiple_of(stride)
                        {
                            return;
                        }
                        let _ = resp_tx_inner.send(IoResp::Progress {
                            message: format!("[{}] {}/{}", stage, current, total),
                        });
                    }

                    last_stage = stage.to_string();
                    last_emit_at = Some(now);
                    last_emit_bytes = current;
                };

                let mut opts = fatxlib::iso::god::ConvertOptions {
                    trim: fatxlib::iso::god::TrimMode::Compact,
                    game_title: resolved_name,
                    dry_run: false,
                    progress: Some(&mut progress_cb),
                    should_abort: Some(&abort_fn),
                };

                match fatxlib::iso::god::convert_iso_to_fatx(
                    &source, &mut vol, &dest_dir, &mut opts,
                ) {
                    Ok(r) => {
                        if flush_or_error(&mut vol, &resp_tx, "GoD flush failed") {
                            continue;
                        }
                        // Rough total: per-part overhead (4 KiB master +
                        // 4 KiB × subparts) plus the CON header. Reporting
                        // the source-side data size is close enough.
                        let _ = resp_tx.send(IoResp::Done {
                            message: format!(
                                "Converted {} → {}/{:08X}/00007000/{:08X} ({} parts, ~{})",
                                display_source,
                                upload_dest,
                                r.title_id,
                                r.media_id,
                                r.part_count,
                                format_size(r.data_size),
                            ),
                        });
                    }
                    Err(e) => {
                        let _ = flush_or_error(&mut vol, &resp_tx, "GoD flush failed");
                        let msg = format!("{}", e);
                        if msg.contains("cancelled") {
                            let _ = resp_tx.send(IoResp::Cancelled {
                                message: format!("GoD conversion cancelled ({})", display_source),
                            });
                        } else {
                            let _ = resp_tx.send(IoResp::Error {
                                message: format!("GoD convert: {}", msg),
                            });
                        }
                    }
                }
            }

            IoCmd::Mkdir { path } => match vol.create_directory(&path) {
                Ok(_) => {
                    if !flush_or_error(&mut vol, &resp_tx, "Mkdir flush failed") {
                        let _ = resp_tx.send(IoResp::Done {
                            message: format!("Created directory '{}'", path),
                        });
                    }
                }
                Err(e) => {
                    let _ = resp_tx.send(IoResp::Error {
                        message: format!("Mkdir error: {}", e),
                    });
                }
            },

            IoCmd::Delete { path, recursive } => {
                let result = if recursive {
                    vol.delete_recursive(&path)
                } else {
                    vol.delete(&path)
                };
                match result {
                    Ok(_) => {
                        if !flush_or_error(&mut vol, &resp_tx, "Delete flush failed") {
                            let _ = resp_tx.send(IoResp::Done {
                                message: format!("Deleted '{}'", path),
                            });
                        }
                    }
                    Err(e) => {
                        let _ = resp_tx.send(IoResp::Error {
                            message: format!("Delete error: {}", e),
                        });
                    }
                }
            }

            IoCmd::Rename { path, new_name } => match vol.rename(&path, &new_name) {
                Ok(_) => {
                    if !flush_or_error(&mut vol, &resp_tx, "Rename flush failed") {
                        let _ = resp_tx.send(IoResp::Done {
                            message: format!("Renamed → '{}'", new_name),
                        });
                    }
                }
                Err(e) => {
                    let _ = resp_tx.send(IoResp::Error {
                        message: format!("Rename error: {}", e),
                    });
                }
            },

            IoCmd::Stats => match vol.stats() {
                Ok(stats) => {
                    let _ = resp_tx.send(IoResp::StatsResult {
                        total_clusters: stats.total_clusters,
                        free_clusters: stats.free_clusters,
                        used_clusters: stats.used_clusters,
                        cluster_size: stats.cluster_size,
                        free_size: stats.free_size,
                        used_size: stats.used_size,
                    });
                }
                Err(e) => {
                    let _ = resp_tx.send(IoResp::Error {
                        message: format!("Stats error: {}", e),
                    });
                }
            },

            IoCmd::ScanCleanup { path } => match vol.scan_macos_metadata_from(&path) {
                Ok(found) => {
                    let entries: Vec<(String, bool, u64)> = found
                        .iter()
                        .map(|e| (e.path.clone(), e.is_dir, e.size))
                        .collect();
                    let _ = resp_tx.send(IoResp::CleanupScanResult { entries });
                }
                Err(e) => {
                    let _ = resp_tx.send(IoResp::Error {
                        message: format!("Scan error: {}", e),
                    });
                }
            },

            IoCmd::DeleteCleanup { paths } => {
                let mut files = 0usize;
                let mut dirs = 0usize;
                let mut bytes = 0u64;
                for path in &paths {
                    let entry = match vol.resolve_path(path) {
                        Ok(e) => e,
                        Err(_) => continue,
                    };
                    if entry.is_directory() {
                        if vol.delete_recursive(path).is_ok() {
                            dirs += 1;
                        }
                    } else {
                        bytes += entry.file_size as u64;
                        if vol.delete(path).is_ok() {
                            files += 1;
                        }
                    }
                }
                if !flush_or_error(&mut vol, &resp_tx, "Cleanup flush failed") {
                    let _ = resp_tx.send(IoResp::Done {
                        message: format!(
                            "Removed {} file(s), {} dir(s), freed {}",
                            files,
                            dirs,
                            format_size(bytes)
                        ),
                    });
                }
            }

            IoCmd::ResolveTitle { path } => {
                use fatxlib::titles::dynamic::{ResolveOutcome, resolve_and_cache};
                let resp = match resolve_and_cache(&mut vol, &path, true) {
                    Ok(ResolveOutcome::Resolved { name, .. }) => IoResp::Done {
                        message: format!("Resolved → {}", name),
                    },
                    Ok(ResolveOutcome::NoStfs) => IoResp::Error {
                        message: "No parseable STFS package in this folder".into(),
                    },
                    Ok(ResolveOutcome::BadTitleIdInPath { last_segment }) => IoResp::Error {
                        message: format!("Not a title-ID folder: {:?}", last_segment),
                    },
                    Err(e) => IoResp::Error {
                        message: format!("Resolve error: {}", e),
                    },
                };
                let _ = resp_tx.send(resp);
            }

            IoCmd::ScanFolderFiles { path } => {
                let resp = match fatxlib::titles::dynamic::scan_folder_files(&mut vol, &path, true)
                {
                    Ok(summary) => IoResp::Done {
                        message: format!(
                            "Scanned: {} resolved, {} skipped",
                            summary.resolved, summary.skipped
                        ),
                    },
                    Err(e) => IoResp::Error {
                        message: format!("Scan error: {}", e),
                    },
                };
                let _ = resp_tx.send(resp);
            }

            IoCmd::ExtractStfsToFatx { source, dest_dir } => {
                cancel_flag.store(false, Ordering::Relaxed);

                let display_source = source
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| source.display().to_string());

                let file = match fs::File::open(&source) {
                    Ok(f) => f,
                    Err(e) => {
                        let _ = resp_tx.send(IoResp::Error {
                            message: format!("Open {}: {}", source.display(), e),
                        });
                        continue;
                    }
                };
                let mut pkg = match fatxlib::stfs::StfsPackage::open(file) {
                    Ok(p) => p,
                    Err(e) => {
                        let _ = resp_tx.send(IoResp::Error {
                            message: format!("Parse {}: {}", source.display(), e),
                        });
                        continue;
                    }
                };

                // Compute total bytes (for progress denominator). If the
                // entries() walk fails here, fall back to 0 — the actual
                // extraction will surface the same error properly.
                let bytes_total: u64 = match pkg.entries() {
                    Ok(es) => es.iter().filter(|e| !e.is_directory).map(|e| e.size).sum(),
                    Err(_) => 0,
                };

                // Throttled progress: send IoResp::Progress at most every 200ms.
                let last_progress = std::cell::Cell::new(std::time::Instant::now());
                let resp_tx_for_cb = resp_tx.clone();
                let cancel_for_cb = Arc::clone(&cancel_flag);
                let cb = move |rel: &str, _size: u64, bytes_done: u64| {
                    if cancel_for_cb.load(Ordering::Relaxed) {
                        return;
                    }
                    if last_progress.get().elapsed().as_millis() > 200 {
                        let _ = resp_tx_for_cb.send(IoResp::Progress {
                            message: format!(
                                "{} ({}/{})",
                                rel,
                                format_size(bytes_done),
                                format_size(bytes_total),
                            ),
                        });
                        last_progress.set(std::time::Instant::now());
                    }
                };

                match fatxlib::stfs::extract::extract_to_fatx(
                    &mut pkg,
                    &mut vol,
                    &dest_dir,
                    Some(&cb),
                    Some(&cancel_flag),
                ) {
                    Ok(report) => {
                        if flush_or_error(&mut vol, &resp_tx, "STFS extract flush failed") {
                            continue;
                        }
                        let _ = resp_tx.send(IoResp::Done {
                            message: format!(
                                "Extracted {} → {} ({} files, {})",
                                display_source,
                                dest_dir,
                                report.files,
                                format_size(report.bytes),
                            ),
                        });
                    }
                    Err(e) => {
                        let _ = flush_or_error(&mut vol, &resp_tx, "STFS extract flush failed");
                        let msg = format!("{}", e);
                        if msg.contains("cancelled") {
                            let _ = resp_tx.send(IoResp::Cancelled {
                                message: format!("STFS extract cancelled: {}", display_source),
                            });
                        } else {
                            let _ = resp_tx.send(IoResp::Error {
                                message: format!("Extract {}: {}", source.display(), msg),
                            });
                        }
                    }
                }
            }

            IoCmd::Flush => {
                if flush_or_error(&mut vol, &resp_tx, "Flush failed") {
                    continue;
                }
                let _ = resp_tx.send(IoResp::Flushed);
            }

            IoCmd::Shutdown => {
                let _ = vol.flush();
                break;
            }
        }
    }
}

/// Recursively collect all files from a local directory into a flat list.
fn collect_files(local_dir: &PathBuf, fatx_dir: &str, out: &mut Vec<(PathBuf, String)>) {
    let entries = match fs::read_dir(local_dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let local_child = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();

        if fatxlib::types::is_macos_metadata(&name) {
            continue;
        }

        let fatx_child = format!("{}/{}", fatx_dir, name);

        if local_child.is_dir() {
            collect_files(&local_child, &fatx_child, out);
        } else if local_child.is_file() {
            out.push((local_child, fatx_child));
        }
    }
}

/// Ensure every directory segment in an absolute FATX path exists, creating
/// any that are missing. Used by [`IoCmd::ExtractXiso`] so an XISO with
/// nested subfolders (e.g. `/Halo/Media/movie.bik`) can drop files anywhere
/// without the caller pre-walking the tree. A pre-existing segment that
/// happens to be a regular file is reported as an error.
fn ensure_dir_chain(
    vol: &mut FatxVolume<std::fs::File>,
    fatx_dir: &str,
) -> fatxlib::error::Result<()> {
    use fatxlib::error::FatxError;

    let trimmed = fatx_dir.trim_end_matches('/');
    if trimmed.is_empty() {
        return Ok(());
    }
    let parts: Vec<&str> = trimmed.split('/').filter(|p| !p.is_empty()).collect();
    let mut acc = String::new();
    for part in parts {
        acc.push('/');
        acc.push_str(part);
        match vol.create_directory(&acc) {
            Ok(()) => {}
            Err(FatxError::FileExists(_)) => {
                let existing = vol.resolve_path(&acc)?;
                if !existing.is_directory() {
                    return Err(FatxError::NotADirectory(acc.clone()));
                }
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Recursively create directory structure on the FATX volume.
fn create_dirs_recursive(vol: &mut FatxVolume<std::fs::File>, local_dir: &PathBuf, fatx_dir: &str) {
    match vol.create_directory(fatx_dir) {
        Ok(_) => {}
        Err(fatxlib::error::FatxError::FileExists(_)) => {}
        Err(_) => return,
    }
    let entries = match fs::read_dir(local_dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        if entry.path().is_dir() {
            let name = entry.file_name().to_string_lossy().to_string();
            if fatxlib::types::is_macos_metadata(&name) {
                continue;
            }
            let fatx_child = format!("{}/{}", fatx_dir, name);
            create_dirs_recursive(vol, &entry.path(), &fatx_child);
        }
    }
}

fn flush_or_error(
    vol: &mut FatxVolume<std::fs::File>,
    resp_tx: &mpsc::Sender<IoResp>,
    context: &str,
) -> bool {
    match vol.flush() {
        Ok(()) => false,
        Err(e) => {
            let _ = resp_tx.send(IoResp::Error {
                message: format!("{}: {}", context, e),
            });
            true
        }
    }
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = stdout().execute(LeaveAlternateScreen);
        let _ = stdout().execute(Show);
    }
}

fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let _ = disable_raw_mode();
        let _ = stdout().execute(LeaveAlternateScreen);
        let _ = stdout().execute(Show);
        default_hook(panic_info);
    }));
}

// ===========================================================================
// Main entry point
// ===========================================================================

pub fn run_browser(
    vol: FatxVolume<std::fs::File>,
    partition_name: &str,
    device_display: &str,
) -> io::Result<()> {
    let cancel_flag = Arc::new(AtomicBool::new(false));

    // Create channels
    let (cmd_tx, cmd_rx) = mpsc::channel::<IoCmd>();
    let (resp_tx, resp_rx) = mpsc::channel::<IoResp>();

    // Spawn I/O worker thread
    let worker_cancel = Arc::clone(&cancel_flag);
    let worker_handle = std::thread::spawn(move || {
        io_worker(vol, cmd_rx, resp_tx, worker_cancel);
    });

    // Setup terminal
    install_panic_hook();
    enable_raw_mode()?;
    stdout().execute(EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout()))?;
    let _terminal_guard = TerminalGuard;

    let mut app = App::new(partition_name, device_display, Arc::clone(&cancel_flag));

    // Initial directory listing
    let _ = cmd_tx.send(IoCmd::ListDir {
        path: "/".to_string(),
    });
    app.is_busy = true;

    // Main loop — non-blocking with 50ms poll
    loop {
        // Show/hide terminal cursor based on input mode
        if app.input_mode != InputMode::Normal {
            stdout().execute(Show)?;
        } else {
            stdout().execute(Hide)?;
        }

        terminal.draw(|frame| ui(frame, &mut app))?;

        if app.should_quit {
            break;
        }

        // Process all pending I/O responses (non-blocking)
        loop {
            match resp_rx.try_recv() {
                Ok(resp) => handle_io_response(&mut app, &cmd_tx, resp),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    app.is_busy = false;
                    app.should_quit = true;
                    app.set_error("I/O worker stopped unexpectedly");
                    break;
                }
            }
        }

        // Poll for key events (50ms timeout — ~20fps refresh)
        if event::poll(Duration::from_millis(50))?
            && let Event::Key(key) = event::read()?
        {
            if app.is_busy {
                // Only allow Esc (cancel) while busy
                if key.code == KeyCode::Esc {
                    app.cancel_flag.store(true, Ordering::Relaxed);
                    app.set_status("Cancelling...");
                }
            } else {
                match app.input_mode {
                    InputMode::Normal => handle_normal_key(&mut app, &cmd_tx, key),
                    _ => handle_input_key(&mut app, &cmd_tx, key),
                }
            }
        }
    }

    // Shutdown I/O worker
    let _ = cmd_tx.send(IoCmd::Shutdown);
    let _ = worker_handle.join();
    Ok(())
}

// ===========================================================================
// Response handler
// ===========================================================================

fn handle_io_response(app: &mut App, cmd_tx: &mpsc::Sender<IoCmd>, resp: IoResp) {
    match resp {
        IoResp::DirListing { entries, path } => {
            let count = entries.len();
            app.cwd = path;
            app.entries = entries;
            app.resort_entries();
            app.is_busy = false;

            if !app.entries.is_empty() {
                app.list_state.select(Some(0));
                app.set_status(&format!(
                    "{} item(s) — ↑↓ navigate, Enter open, d download, u upload, q quit",
                    count
                ));
            } else {
                app.list_state.select(None);
                app.set_status(
                    "(empty directory) — Backspace to go up, u to upload, m to mkdir, q to quit",
                );
            }
        }

        IoResp::Progress { message } => {
            app.set_status(&message);
            // Stay busy — more progress or Done/Error/Cancelled will follow
        }

        IoResp::Done { message } => {
            app.is_busy = false;
            app.set_status(&message);
            // Refresh directory listing
            let _ = cmd_tx.send(IoCmd::ListDir {
                path: app.cwd.clone(),
            });
            app.is_busy = true;
        }

        IoResp::Error { message } => {
            app.is_busy = false;
            app.set_error(&message);
        }

        IoResp::StatsResult {
            total_clusters,
            free_clusters,
            cluster_size: _,
            free_size,
            used_size,
            used_clusters: _,
        } => {
            app.is_busy = false;
            app.set_status(&format!(
                "Volume: {} | Used: {} | Free: {} | Clusters: {}/{}",
                app.partition_name,
                format_size(used_size),
                format_size(free_size),
                total_clusters - free_clusters,
                total_clusters,
            ));
        }

        IoResp::Flushed => {
            app.is_busy = false;
            // Flushed before quit
            app.should_quit = true;
        }

        IoResp::CleanupScanResult { entries } => {
            app.is_busy = false;
            if entries.is_empty() {
                app.set_status("No macOS metadata found.");
            } else {
                let file_count = entries.iter().filter(|e| !e.1).count();
                let dir_count = entries.iter().filter(|e| e.1).count();
                let total_bytes: u64 = entries.iter().map(|e| e.2).sum();
                let names: Vec<&str> = entries.iter().map(|e| e.0.as_str()).collect();
                let preview = if names.len() <= 5 {
                    names.join(", ")
                } else {
                    format!(
                        "{}, ... and {} more",
                        names[..5].join(", "),
                        names.len() - 5
                    )
                };
                app.pending_cleanup = entries;
                app.input_prompt = format!(
                    "Found {} file(s), {} dir(s) ({}) — {}. Delete? (y/n):",
                    file_count,
                    dir_count,
                    format_size(total_bytes),
                    preview,
                );
                app.input_buffer.clear();
                app.input_mode = InputMode::ConfirmCleanup;
            }
        }

        IoResp::Cancelled { message } => {
            app.is_busy = false;
            app.cancel_flag.store(false, Ordering::Relaxed);
            app.set_status(&message);
            // Refresh directory listing
            let _ = cmd_tx.send(IoCmd::ListDir {
                path: app.cwd.clone(),
            });
            app.is_busy = true;
        }
    }
}

// ===========================================================================
// Key handlers (no vol access — send commands via channel)
// ===========================================================================

fn handle_normal_key(app: &mut App, cmd_tx: &mpsc::Sender<IoCmd>, key: KeyEvent) {
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => {
            let _ = cmd_tx.send(IoCmd::Flush);
            app.is_busy = true;
            app.set_status("Flushing...");
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if let Some(sel) = app.list_state.selected()
                && sel > 0
            {
                app.list_state.select(Some(sel - 1));
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if let Some(sel) = app.list_state.selected()
                && sel + 1 < app.entries.len()
            {
                app.list_state.select(Some(sel + 1));
            }
        }
        KeyCode::Home if !app.entries.is_empty() => {
            app.list_state.select(Some(0));
        }
        KeyCode::End if !app.entries.is_empty() => {
            app.list_state.select(Some(app.entries.len() - 1));
        }
        KeyCode::PageDown => {
            if let Some(sel) = app.list_state.selected() {
                let new = (sel + 20).min(app.entries.len().saturating_sub(1));
                app.list_state.select(Some(new));
            }
        }
        KeyCode::PageUp => {
            if let Some(sel) = app.list_state.selected() {
                let new = sel.saturating_sub(20);
                app.list_state.select(Some(new));
            }
        }
        KeyCode::Enter | KeyCode::Right => {
            if let Some(entry) = app.selected_entry() {
                if entry.is_dir {
                    let new_cwd = app.full_path(&entry.name);
                    app.set_status("Loading...");
                    let _ = cmd_tx.send(IoCmd::ListDir { path: new_cwd });
                    app.is_busy = true;
                } else {
                    let name = entry.name.clone();
                    let size = entry.size;
                    app.set_status(&format!(
                        "'{}' — {} — press 'd' to download",
                        name,
                        format_size(size)
                    ));
                }
            }
        }
        KeyCode::Backspace | KeyCode::Left if app.cwd != "/" => {
            let new_cwd = if let Some(pos) = app.cwd.rfind('/') {
                if pos == 0 {
                    "/".to_string()
                } else {
                    app.cwd[..pos].to_string()
                }
            } else {
                "/".to_string()
            };
            app.set_status("Loading...");
            let _ = cmd_tx.send(IoCmd::ListDir { path: new_cwd });
            app.is_busy = true;
        }
        KeyCode::Char('d') => {
            let info = app.selected_entry().map(|e| (e.is_dir, e.name.clone()));
            if let Some((is_dir, name)) = info {
                if is_dir {
                    app.set_error("Cannot download a directory (select a file)");
                    return;
                }
                let default_path = app.download_dir.join(&name);
                app.input_prompt = format!("Save '{}' to:", name);
                app.input_buffer = default_path.to_string_lossy().to_string();
                app.input_mode = InputMode::DownloadPath;
            }
        }
        KeyCode::Char('u') => {
            app.input_prompt = format!("Upload file/directory to '{}':", app.cwd);
            app.input_buffer.clear();
            app.input_mode = InputMode::UploadPath;
        }
        KeyCode::Char('m') => {
            app.input_prompt = "New directory name:".to_string();
            app.input_buffer.clear();
            app.input_mode = InputMode::MkdirName;
        }
        KeyCode::Char('s') => {
            app.sort_mode = match app.sort_mode {
                SortMode::ByName => SortMode::ById,
                SortMode::ById => SortMode::ByName,
            };
            app.resort_entries();
            let mode_label = match app.sort_mode {
                SortMode::ByName => "by name",
                SortMode::ById => "by ID",
            };
            app.set_status(&format!("Sorted {}", mode_label));
        }
        KeyCode::Char('R') => {
            use fatxlib::display::{FolderSlot, folder_slot};
            match folder_slot(&app.cwd) {
                // Inside `/Content/<XUID>` → resolve the selected title-ID folder.
                FolderSlot::TitleId => {
                    let Some(entry) = app.selected_entry() else {
                        return;
                    };
                    if !entry.is_dir {
                        app.set_error("Select a title-ID folder to resolve");
                        return;
                    }
                    let path = app.full_path(&entry.name);
                    app.set_status("Reading STFS header...");
                    let _ = cmd_tx.send(IoCmd::ResolveTitle { path });
                    app.is_busy = true;
                }
                // Inside an Arcade/XNA/Marketplace/Installer folder →
                // bulk-scan every file in the current directory.
                FolderSlot::StfsFile => {
                    app.set_status("Scanning STFS headers...");
                    let _ = cmd_tx.send(IoCmd::ScanFolderFiles {
                        path: app.cwd.clone(),
                    });
                    app.is_busy = true;
                }
                _ => {
                    app.set_error(
                        "R only works inside Content/<XUID> or an STFS content-type folder",
                    );
                }
            }
        }
        KeyCode::Char('D') => {
            if let Some(name) = app.selected_name() {
                let is_dir = app.selected_entry().map(|e| e.is_dir).unwrap_or(false);
                let msg = if is_dir {
                    format!("Delete '{}' and all contents? (y/n):", name)
                } else {
                    format!("Delete '{}'? (y/n):", name)
                };
                app.input_prompt = msg;
                app.input_buffer.clear();
                app.pending_delete = Some((name, is_dir));
                app.input_mode = InputMode::ConfirmDelete;
            }
        }
        KeyCode::Char('r') => {
            if let Some(name) = app.selected_name() {
                app.input_prompt = format!("Rename '{}' to:", name);
                app.input_buffer = name;
                app.input_mode = InputMode::RenameName;
            }
        }
        KeyCode::Char('i') => {
            let _ = cmd_tx.send(IoCmd::Stats);
            app.is_busy = true;
            app.set_status("Loading stats...");
        }
        KeyCode::Char('c') => {
            let _ = cmd_tx.send(IoCmd::ScanCleanup {
                path: app.cwd.clone(),
            });
            app.is_busy = true;
            app.set_status("Scanning for macOS metadata...");
        }
        _ => {}
    }
}

fn handle_input_key(app: &mut App, cmd_tx: &mpsc::Sender<IoCmd>, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => {
            app.input_mode = InputMode::Normal;
            // If the user was answering the XISO extract/raw prompt, drop the
            // stashed path so the next upload starts clean.
            app.pending_xiso_upload = None;
            app.pending_stfs_upload = None;
            app.pending_delete = None;
            app.set_status("Cancelled.");
        }
        KeyCode::Enter => {
            let input = app.input_buffer.clone();
            let mode = app.input_mode;
            app.input_mode = InputMode::Normal;

            match mode {
                InputMode::DownloadPath => {
                    // Download
                    let name = app.selected_name().unwrap_or_default();
                    let fatx_path = app.full_path(&name);
                    let local_path = PathBuf::from(unescape_path(&input));
                    app.download_dir = local_path
                        .parent()
                        .unwrap_or(&PathBuf::from("."))
                        .to_path_buf();
                    app.set_status(&format!("Downloading '{}'...", name));
                    let _ = cmd_tx.send(IoCmd::ReadFile {
                        fatx_path,
                        local_path,
                    });
                    app.is_busy = true;
                }
                InputMode::UploadPath => {
                    // Upload file or directory — unescape shell backslashes
                    // (e.g. Call\ of\ Duty) and trim leading/trailing whitespace
                    // (drag-and-drop into the terminal often appends a space).
                    let path = PathBuf::from(unescape_path(input.trim()));
                    if !path.exists() {
                        app.set_error(&format!("Not found: {}", input));
                        return;
                    }
                    let filename = path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| "file.dat".to_string());

                    if path.is_dir() {
                        let fatx_dest = app.full_path(&filename);
                        app.set_status(&format!("Uploading directory '{}'...", filename));
                        let _ = cmd_tx.send(IoCmd::CopyDir {
                            local_path: path.clone(),
                            fatx_dest,
                        });
                        app.download_dir =
                            path.parent().unwrap_or(&PathBuf::from(".")).to_path_buf();
                        app.is_busy = true;
                    } else if is_xiso(&path) {
                        // Detected an Xbox disc image. Pick the default action
                        // based on cwd: inside a per-user Content folder (where
                        // GoD packages live), default to GoD; everywhere else
                        // default to extract (works for alt dashboards).
                        let default = if fatxlib::display::folder_slot(&app.cwd)
                            == fatxlib::display::FolderSlot::TitleId
                        {
                            XisoUploadAction::God
                        } else {
                            XisoUploadAction::Extract
                        };
                        let prompt = match default {
                            XisoUploadAction::Extract => format!(
                                "Detected XISO '{}'. e(X)tract / (g)oD / (r)aw / Esc:",
                                filename
                            ),
                            XisoUploadAction::God => format!(
                                "Detected XISO '{}'. e(x)tract / (G)oD / (r)aw / Esc:",
                                filename
                            ),
                            XisoUploadAction::Raw => format!(
                                "Detected XISO '{}'. e(x)tract / (g)oD / (R)aw / Esc:",
                                filename
                            ),
                        };
                        app.pending_xiso_upload = Some((path.clone(), default));
                        app.download_dir =
                            path.parent().unwrap_or(&PathBuf::from(".")).to_path_buf();
                        app.input_mode = InputMode::ConfirmXisoUpload;
                        app.input_prompt = prompt;
                        app.input_buffer.clear();
                    } else if let Some(target) = stfs_extract_target(&path) {
                        // ── New STFS two-way prompt ──
                        let prompt = format!(
                            "Detected STFS '{}' (Arcade). e(X)tract / (R)aw / Esc:",
                            path.file_name()
                                .map(|n| n.to_string_lossy().to_string())
                                .unwrap_or_else(|| "package".to_string()),
                        );
                        app.pending_stfs_upload = Some((path.clone(), target));
                        app.download_dir =
                            path.parent().unwrap_or(&PathBuf::from(".")).to_path_buf();
                        app.input_mode = InputMode::ConfirmStfsUpload;
                        app.input_prompt = prompt;
                        app.input_buffer.clear();
                    } else {
                        let fatx_path = app.full_path(&filename);
                        app.set_status(&format!("Uploading '{}'...", filename));
                        let _ = cmd_tx.send(IoCmd::WriteFile {
                            local_path: path.clone(),
                            fatx_path,
                        });
                        app.download_dir =
                            path.parent().unwrap_or(&PathBuf::from(".")).to_path_buf();
                        app.is_busy = true;
                    }
                }
                InputMode::ConfirmXisoUpload => {
                    let (path, default) = match app.pending_xiso_upload.take() {
                        Some(pair) => pair,
                        None => {
                            app.set_error("Internal: missing pending XISO path.");
                            return;
                        }
                    };
                    let trimmed = input.trim();
                    let action = if trimmed.is_empty() {
                        default
                    } else {
                        match trimmed.chars().next().map(|c| c.to_ascii_lowercase()) {
                            Some('x') => XisoUploadAction::Extract,
                            Some('g') => XisoUploadAction::God,
                            Some('r') => XisoUploadAction::Raw,
                            _ => {
                                app.set_error(&format!(
                                    "Unknown choice {:?} — expected x, g, or r.",
                                    trimmed
                                ));
                                return;
                            }
                        }
                    };
                    let filename = path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| "iso".to_string());

                    match action {
                        XisoUploadAction::Extract => {
                            // Prefer a catalog-resolved folder name over the
                            // local filename stem: a disc named `disc1.iso`
                            // with TitleID 0x4D5307E6 should land at
                            // `<cwd>/Halo 3 [4D5307E6]/` rather than `<cwd>/disc1/`.
                            // Falls back to the file stem on catalog miss or
                            // unreadable XEX/XBE.
                            let stem = path
                                .file_stem()
                                .map(|n| n.to_string_lossy().to_string())
                                .unwrap_or_else(|| filename.clone());
                            let resolved = xiso_folder_name(&path).unwrap_or(stem);
                            let dest_dir = app.full_path(&resolved);
                            app.set_status(&format!("Extracting '{}' → {}...", filename, dest_dir));
                            let _ = cmd_tx.send(IoCmd::ExtractXiso {
                                source: path,
                                dest_dir,
                            });
                            app.is_busy = true;
                        }
                        XisoUploadAction::God => {
                            let dest_dir = app.cwd.clone();
                            app.set_status(&format!(
                                "Converting '{}' to GoD under {}...",
                                filename, dest_dir
                            ));
                            let _ = cmd_tx.send(IoCmd::ConvertXisoToGod {
                                source: path,
                                dest_dir,
                            });
                            app.is_busy = true;
                        }
                        XisoUploadAction::Raw => {
                            let fatx_path = app.full_path(&filename);
                            app.set_status(&format!("Uploading '{}' (raw)...", filename));
                            let _ = cmd_tx.send(IoCmd::WriteFile {
                                local_path: path,
                                fatx_path,
                            });
                            app.is_busy = true;
                        }
                    }
                }
                InputMode::MkdirName => {
                    // Mkdir
                    if !input.is_empty() {
                        let path = app.full_path(&input);
                        app.set_status(&format!("Creating '{}'...", input));
                        let _ = cmd_tx.send(IoCmd::Mkdir { path });
                        app.is_busy = true;
                    }
                }
                InputMode::RenameName => {
                    // Rename
                    let old_name = app.selected_name().unwrap_or_default();
                    if !input.is_empty() {
                        let path = app.full_path(&old_name);
                        let _ = cmd_tx.send(IoCmd::Rename {
                            path,
                            new_name: input.clone(),
                        });
                        app.is_busy = true;
                    }
                }
                InputMode::ConfirmDelete => {
                    // Confirm delete
                    if input.eq_ignore_ascii_case("y") || input.eq_ignore_ascii_case("yes") {
                        let (name, is_dir) = match app.pending_delete.take() {
                            Some(pair) => pair,
                            None => {
                                app.set_error("Internal: missing pending delete target.");
                                return;
                            }
                        };
                        let path = app.full_path(&name);
                        app.set_status(&format!("Deleting '{}'...", name));
                        let _ = cmd_tx.send(IoCmd::Delete {
                            path,
                            recursive: is_dir,
                        });
                        app.is_busy = true;
                    } else {
                        app.pending_delete = None;
                        app.set_status("Delete cancelled.");
                    }
                }
                InputMode::ConfirmCleanup => {
                    // Confirm cleanup
                    if input.eq_ignore_ascii_case("y") || input.eq_ignore_ascii_case("yes") {
                        let paths: Vec<String> = app
                            .pending_cleanup
                            .drain(..)
                            .map(|(path, _, _)| path)
                            .collect();
                        let count = paths.len();
                        app.set_status(&format!("Deleting {} metadata entries...", count));
                        let _ = cmd_tx.send(IoCmd::DeleteCleanup { paths });
                        app.is_busy = true;
                    } else {
                        app.pending_cleanup.clear();
                        app.set_status("Cleanup cancelled.");
                    }
                }
                InputMode::ConfirmStfsUpload => {
                    let (path, target) = match app.pending_stfs_upload.take() {
                        Some(pair) => pair,
                        None => {
                            app.set_error("Internal: missing pending STFS path.");
                            return;
                        }
                    };
                    let trimmed = input.trim();
                    let action = if trimmed.is_empty() {
                        StfsUploadAction::Extract
                    } else {
                        match trimmed.chars().next().map(|c| c.to_ascii_lowercase()) {
                            Some('x') => StfsUploadAction::Extract,
                            Some('r') => StfsUploadAction::Raw,
                            _ => {
                                app.set_error(&format!(
                                    "Unknown choice {:?} — expected x or r.",
                                    trimmed
                                ));
                                return;
                            }
                        }
                    };
                    let filename = path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| "stfs".to_string());

                    match action {
                        StfsUploadAction::Extract => {
                            let dest_dir = app.full_path(&target.dest_name);
                            app.set_status(&format!(
                                "Extracting STFS '{}' → {}...",
                                filename, dest_dir
                            ));
                            let _ = cmd_tx.send(IoCmd::ExtractStfsToFatx {
                                source: path,
                                dest_dir,
                            });
                            app.is_busy = true;
                        }
                        StfsUploadAction::Raw => {
                            let fatx_path = app.full_path(&filename);
                            app.set_status(&format!("Uploading '{}' (raw)...", filename));
                            let _ = cmd_tx.send(IoCmd::WriteFile {
                                local_path: path,
                                fatx_path,
                            });
                            app.is_busy = true;
                        }
                    }
                }
                InputMode::Normal => {}
            }
        }
        KeyCode::Backspace => {
            app.input_buffer.pop();
        }
        KeyCode::Char(c) => {
            if matches!(
                app.input_mode,
                InputMode::ConfirmDelete
                    | InputMode::ConfirmCleanup
                    | InputMode::ConfirmXisoUpload
                    | InputMode::ConfirmStfsUpload
            ) {
                app.input_buffer = c.to_string();
            } else {
                app.input_buffer.push(c);
            }
        }
        _ => {}
    }
}

// ===========================================================================
// UI rendering
// ===========================================================================

fn ui(frame: &mut Frame, app: &mut App) {
    let area = frame.area();

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(3),
        ])
        .split(area);

    // -- Header --
    let busy_indicator = if app.is_busy { " ⏳" } else { "" };
    let header_text = format!(
        " {} — {} — {}{}",
        app.partition_name, app.device_display, app.cwd, busy_indicator,
    );
    let header = Paragraph::new(header_text)
        .style(Style::default().fg(Color::White).bg(Color::DarkGray).bold())
        .block(
            Block::default()
                .borders(Borders::BOTTOM)
                .border_style(Style::default().fg(Color::Gray)),
        );
    frame.render_widget(header, chunks[0]);

    // -- File list --
    // Children of cwd are in this slot — used to flag unresolved entries
    // with `?` so the user knows R can resolve them.
    let child_slot = fatxlib::display::folder_slot(&app.cwd);
    let sort_mode = app.sort_mode;
    let items: Vec<ListItem> = app
        .entries
        .iter()
        .map(|e| {
            let icon = if e.is_dir { "📁" } else { "📄" };
            // Resolvable when the entry sits in a slot with an active
            // resolver and we don't yet have a display name for it.
            let resolvable = (child_slot == fatxlib::display::FolderSlot::TitleId && e.is_dir
                || child_slot == fatxlib::display::FolderSlot::StfsFile && !e.is_dir)
                && !e.is_resolved();
            let marker = if resolvable { "?" } else { " " };
            let size_str = if e.is_dir {
                "<DIR>".to_string()
            } else {
                format_size(e.size)
            };
            let line = format!(
                " {} {} {:<41} {:>10}  {}  {}",
                icon,
                marker,
                e.label(sort_mode),
                size_str,
                e.modified,
                e.attributes,
            );
            let style = if e.is_dir {
                Style::default().fg(Color::Cyan).bold()
            } else {
                Style::default().fg(Color::White)
            };
            ListItem::new(line).style(style)
        })
        .collect();

    let file_list = List::new(items)
        .block(
            Block::default()
                .title(" Files ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Gray)),
        )
        .highlight_style(
            Style::default()
                .bg(Color::Rgb(40, 60, 100))
                .fg(Color::White)
                .bold(),
        )
        .highlight_symbol("▸ ");

    frame.render_stateful_widget(file_list, chunks[1], &mut app.list_state);

    // -- Status / Input bar --
    if app.input_mode != InputMode::Normal {
        let input_text = format!(" {} {}", app.input_prompt, app.input_buffer);
        let input_bar = Paragraph::new(input_text)
            .style(
                Style::default()
                    .fg(Color::Yellow)
                    .bg(Color::DarkGray)
                    .bold(),
            )
            .block(
                Block::default()
                    .title(" Input (Enter to confirm, Esc to cancel) ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Yellow).bold()),
            );
        frame.render_widget(input_bar, chunks[2]);

        // Position cursor at end of input text (inside border: +1 x for border, +1 y for border)
        // Text format is: " {prompt} {buffer}" — leading space + prompt + space + buffer
        let text_len = 1 + app.input_prompt.len() + 1 + app.input_buffer.len();
        let cursor_x = chunks[2].x + 1 + text_len as u16; // +1 for left border
        let cursor_y = chunks[2].y + 1; // +1 for top border
        frame.set_cursor_position((cursor_x, cursor_y));
    } else {
        let style = if app.status_is_error {
            Style::default().fg(Color::Red)
        } else {
            Style::default().fg(Color::Green)
        };
        let help = if app.is_busy {
            " Esc: cancel "
        } else {
            " d:download  u:upload  m:mkdir  D:delete  r:rename  R:resolve  s:sort  i:info  c:cleanup  q:quit "
        };
        let status_bar = Paragraph::new(format!(" {}", app.status))
            .style(style)
            .block(
                Block::default()
                    .title(help)
                    .title_style(Style::default().fg(Color::DarkGray))
                    .borders(Borders::TOP)
                    .border_style(Style::default().fg(Color::Gray)),
            );
        frame.render_widget(status_bar, chunks[2]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_unescape_backslash_spaces() {
        assert_eq!(unescape_path(r"Call\ of\ Duty"), "Call of Duty");
    }

    #[test]
    fn test_unescape_no_escapes() {
        assert_eq!(unescape_path("normal_path"), "normal_path");
    }

    #[test]
    fn test_unescape_mixed() {
        assert_eq!(
            unescape_path(r"/Users/josh/My\ Games/Call\ of\ Duty\ -\ Black\ Ops"),
            "/Users/josh/My Games/Call of Duty - Black Ops"
        );
    }

    #[test]
    fn test_unescape_double_backslash() {
        assert_eq!(unescape_path(r"file\\name"), r"file\name");
    }

    #[test]
    fn test_unescape_trailing_backslash() {
        assert_eq!(unescape_path(r"path\"), "path");
    }

    #[test]
    fn test_unescape_empty() {
        assert_eq!(unescape_path(""), "");
    }

    #[test]
    fn test_is_xiso_junk_systemupdate() {
        assert!(fatxlib::iso::policy::is_systemupdate_path("$SystemUpdate"));
        assert!(fatxlib::iso::policy::is_systemupdate_path(
            "$SystemUpdate/su20076000_00000000"
        ));
        assert!(fatxlib::iso::policy::is_systemupdate_path(
            "/$SystemUpdate/anything"
        ));
    }

    #[test]
    fn test_is_xiso_junk_case_insensitive() {
        assert!(fatxlib::iso::policy::is_systemupdate_path(
            "$SYSTEMUPDATE/foo"
        ));
        assert!(fatxlib::iso::policy::is_systemupdate_path(
            "$systemupdate/foo"
        ));
    }

    #[test]
    fn test_sanitize_fatx_filename_replaces_illegal_chars() {
        assert_eq!(
            sanitize_fatx_filename("Halo: Combat Evolved"),
            "Halo- Combat Evolved"
        );
        assert_eq!(
            sanitize_fatx_filename(r"Tom Clancy's R6: Vegas <DEMO>"),
            "Tom Clancy's R6- Vegas -DEMO-"
        );
        assert_eq!(
            sanitize_fatx_filename("path/with\\slashes"),
            "path-with-slashes"
        );
    }

    #[test]
    fn test_sanitize_fatx_filename_truncates_to_42_bytes() {
        let long = "A Really Long Game Subtitle That Definitely Will Not Fit";
        let s = sanitize_fatx_filename(long);
        assert!(s.len() <= 42, "got {:?} ({} bytes)", s, s.len());
        // Truncation should be at a word boundary or just under 42 bytes;
        // never end with a dangling separator.
        assert!(!s.ends_with(' '));
        assert!(!s.ends_with('-'));
        assert!(!s.ends_with('.'));
    }

    #[test]
    fn test_sanitize_fatx_filename_collapses_runs_of_whitespace() {
        assert_eq!(
            sanitize_fatx_filename("Halo  3   Anniversary"),
            "Halo 3 Anniversary"
        );
    }

    #[test]
    fn test_sanitize_fatx_filename_trims_edges() {
        assert_eq!(sanitize_fatx_filename("  Halo 3   "), "Halo 3");
        assert_eq!(sanitize_fatx_filename("...Halo 3..."), "Halo 3");
    }

    #[test]
    fn test_xiso_upload_default_god_inside_xuid_folder() {
        // cwd directly inside /Content/<XUID>/ should default to GoD,
        // because that's where Xbox 360 BC looks for title-id folders.
        assert_eq!(
            fatxlib::display::folder_slot("/Content/0000000000000000"),
            fatxlib::display::FolderSlot::TitleId
        );
        assert_eq!(
            fatxlib::display::folder_slot("/Content/E0001A0BC2E16C4D"),
            fatxlib::display::FolderSlot::TitleId
        );
    }

    #[test]
    fn test_xiso_upload_default_extract_elsewhere() {
        // Anywhere outside `/Content/<XUID>/` should default to extract.
        assert_ne!(
            fatxlib::display::folder_slot("/"),
            fatxlib::display::FolderSlot::TitleId
        );
        assert_ne!(
            fatxlib::display::folder_slot("/Games"),
            fatxlib::display::FolderSlot::TitleId
        );
        assert_ne!(
            fatxlib::display::folder_slot("/Content"),
            fatxlib::display::FolderSlot::TitleId
        );
        // Deeper than the XUID folder: we're inside a title-id folder
        // already, so children are content-type folders — extract default.
        assert_ne!(
            fatxlib::display::folder_slot("/Content/0000000000000000/4D530002"),
            fatxlib::display::FolderSlot::TitleId
        );
    }

    #[test]
    fn test_is_xiso_junk_does_not_match_substring() {
        assert!(!fatxlib::iso::policy::is_systemupdate_path("default.xbe"));
        assert!(!fatxlib::iso::policy::is_systemupdate_path(
            "Media/$SystemUpdate"
        )); // not the first segment
        assert!(!fatxlib::iso::policy::is_systemupdate_path(
            "MyGame$SystemUpdate/foo"
        ));
    }

    #[test]
    fn stfs_extract_target_returns_none_for_non_stfs() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), b"not an stfs package\x00\x00").unwrap();
        assert!(stfs_extract_target(tmp.path()).is_none());
    }

    #[test]
    fn stfs_extract_target_returns_none_for_stfs_without_default_xex() {
        // Build a minimal STFS package with no default.xex at root.
        let bytes = make_synthetic_stfs_no_default_xex();
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), &bytes).unwrap();
        assert!(stfs_extract_target(tmp.path()).is_none());
    }

    #[test]
    fn stfs_extract_target_returns_dest_name_for_arcade_package() {
        let bytes = make_synthetic_stfs_with_default_xex("My Test Game");
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), &bytes).unwrap();
        let target = stfs_extract_target(tmp.path()).expect("should detect");
        // Destination name comes from display_name, sanitized for FATX.
        assert_eq!(target.dest_name, "My Test Game");
    }

    fn make_synthetic_stfs_no_default_xex() -> Vec<u8> {
        // 32 KiB of zeros except the LIVE magic + a valid volume descriptor
        // + a file-table block with one non-default.xex file.
        let mut buf = vec![0u8; 0x1_0000];
        buf[0..4].copy_from_slice(b"LIVE");
        buf[0x379] = 0x24;
        buf[0x37B] = 0x01;
        buf[0x37C..0x37E].copy_from_slice(&1u16.to_be_bytes());
        buf[0x395..0x399].copy_from_slice(&2u32.to_be_bytes());
        // File table at block 0 starts at 0xC000
        let mut e = vec![0u8; 0x40];
        e[..9].copy_from_slice(b"other.bin");
        e[0x28] = 0x09 | 0x40; // length=9, consecutive
        e[0x2C] = 1; // used_blocks
        e[0x2F] = 1; // start_block = 1
        e[0x32..0x34].copy_from_slice(&(-1i16).to_be_bytes());
        e[0x34..0x38].copy_from_slice(&4u32.to_be_bytes());
        buf[0xC000..0xC000 + 0x40].copy_from_slice(&e);
        buf
    }

    fn make_synthetic_stfs_with_default_xex(display_name: &str) -> Vec<u8> {
        let mut buf = vec![0u8; 0x1_0000];
        buf[0..4].copy_from_slice(b"LIVE");
        buf[0x379] = 0x24;
        buf[0x37B] = 0x01;
        buf[0x37C..0x37E].copy_from_slice(&1u16.to_be_bytes());
        buf[0x395..0x399].copy_from_slice(&2u32.to_be_bytes());

        // Write display_name into the locale-0 UTF-16BE slot at 0x411.
        for (i, c) in display_name.encode_utf16().enumerate() {
            let off = 0x411 + i * 2;
            buf[off..off + 2].copy_from_slice(&c.to_be_bytes());
        }

        // File-table block at 0xC000: one entry, default.xex, root.
        let mut e = vec![0u8; 0x40];
        e[..11].copy_from_slice(b"default.xex");
        e[0x28] = 0x0B | 0x40;
        e[0x2C] = 1;
        e[0x2F] = 1;
        e[0x32..0x34].copy_from_slice(&(-1i16).to_be_bytes());
        e[0x34..0x38].copy_from_slice(&4u32.to_be_bytes());
        buf[0xC000..0xC000 + 0x40].copy_from_slice(&e);
        buf
    }
}
