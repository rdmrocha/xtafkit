//! TUI file browser for FATX volumes.
//!
//! Architecture: two-thread model with channel communication.
//! - UI thread: draws the terminal, reads keypresses, sends commands
//! - I/O thread: owns FatxVolume, executes commands, sends responses
//!
//! Keyboard:
//!   ↑/↓       Navigate file list
//!   Enter      Open directory / select file
//!   Backspace  Go up one directory
//!   d          Download selected file to local disk
//!   u          Upload a local file or directory to current directory
//!   n          Create new directory
//!   D          Delete selected file/directory
//!   r          Rename selected file/directory
//!   i          Show volume info
//!   c          Clean up macOS metadata from current directory
//!   Esc        Cancel running operation / Quit
//!   q          Quit

use std::fs;
use std::io::{self, stdout};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use crossterm::{
    cursor::{Hide, Show},
    event::{self, Event, KeyCode, KeyEvent},
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    ExecutableCommand,
};
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
};

use fatxlib::partition::format_size;
use fatxlib::types::FileAttributes;
use fatxlib::volume::FatxVolume;

// ===========================================================================
// Display types
// ===========================================================================

#[allow(dead_code)]
struct DisplayEntry {
    name: String,
    is_dir: bool,
    size: u64,
    modified: String,
    attributes: String,
    first_cluster: u32,
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
    Mkdir {
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

#[derive(PartialEq)]
#[allow(dead_code)]
enum InputMode {
    Normal,
    DownloadPath,
    UploadPath,
    MkdirName,
    RenameName,
    ConfirmDelete,
    ConfirmCleanup,
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
                        let mut display: Vec<DisplayEntry> = entries
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
                                DisplayEntry {
                                    name: e.filename(),
                                    is_dir: e.is_directory(),
                                    size: e.file_size as u64,
                                    modified: e.write_datetime_str(),
                                    attributes: attr,
                                    first_cluster: e.first_cluster,
                                }
                            })
                            .collect();

                        display.sort_by(|a, b| {
                            b.is_dir
                                .cmp(&a.is_dir)
                                .then(a.name.to_lowercase().cmp(&b.name.to_lowercase()))
                        });

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
                            let _ = vol.flush();
                            let _ = resp_tx.send(IoResp::Done {
                                message: format!(
                                    "Uploaded '{}' → {} ({})",
                                    local_path.display(),
                                    fatx_path,
                                    format_size(size)
                                ),
                            });
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
                        if let Err(e) = vol.flush() {
                            let _ = resp_tx.send(IoResp::Error {
                                message: format!("Periodic flush failed: {}", e),
                            });
                            cancelled = true;
                            break;
                        }
                        files_since_flush = 0;
                        bytes_since_flush = 0;
                    }
                }

                let _ = vol.flush();

                if cancelled {
                    let _ = resp_tx.send(IoResp::Cancelled {
                        message: format!(
                            "Cancelled — {}/{} files uploaded ({})",
                            files_done.saturating_sub(1),
                            total_files,
                            format_size(bytes_done)
                        ),
                    });
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

            IoCmd::Mkdir { path } => match vol.create_directory(&path) {
                Ok(_) => {
                    let _ = vol.flush();
                    let _ = resp_tx.send(IoResp::Done {
                        message: format!("Created directory '{}'", path),
                    });
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
                        let _ = vol.flush();
                        let _ = resp_tx.send(IoResp::Done {
                            message: format!("Deleted '{}'", path),
                        });
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
                    let _ = vol.flush();
                    let _ = resp_tx.send(IoResp::Done {
                        message: format!("Renamed → '{}'", new_name),
                    });
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
                let _ = vol.flush();
                let _ = resp_tx.send(IoResp::Done {
                    message: format!(
                        "Removed {} file(s), {} dir(s), freed {}",
                        files,
                        dirs,
                        format_size(bytes)
                    ),
                });
            }

            IoCmd::Flush => {
                let _ = vol.flush();
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
    enable_raw_mode()?;
    stdout().execute(EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout()))?;

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
        while let Ok(resp) = resp_rx.try_recv() {
            handle_io_response(&mut app, &cmd_tx, resp);
        }

        // Poll for key events (50ms timeout — ~20fps refresh)
        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
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
    }

    // Shutdown I/O worker
    let _ = cmd_tx.send(IoCmd::Shutdown);
    let _ = worker_handle.join();

    // Restore terminal
    disable_raw_mode()?;
    stdout().execute(LeaveAlternateScreen)?;

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
                    "(empty directory) — Backspace to go up, u to upload, n to mkdir, q to quit",
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
            if let Some(sel) = app.list_state.selected() {
                if sel > 0 {
                    app.list_state.select(Some(sel - 1));
                }
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if let Some(sel) = app.list_state.selected() {
                if sel + 1 < app.entries.len() {
                    app.list_state.select(Some(sel + 1));
                }
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
        KeyCode::Enter => {
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
            let default = app.download_dir.to_string_lossy().to_string();
            app.input_prompt = format!("Upload file/directory to '{}':", app.cwd);
            app.input_buffer = default;
            app.input_mode = InputMode::UploadPath;
        }
        KeyCode::Char('n') => {
            app.input_prompt = "New directory name:".to_string();
            app.input_buffer.clear();
            app.input_mode = InputMode::MkdirName;
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
            app.set_status("Cancelled.");
        }
        KeyCode::Enter => {
            let input = app.input_buffer.clone();
            app.input_mode = InputMode::Normal;

            // Dispatch based on what mode we were in (using the prompt to determine)
            if app.input_prompt.starts_with("Save '") {
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
            } else if app.input_prompt.starts_with("Upload ") {
                // Upload file or directory — unescape shell backslashes (e.g. Call\ of\ Duty)
                let path = PathBuf::from(unescape_path(&input));
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
                } else {
                    let fatx_path = app.full_path(&filename);
                    app.set_status(&format!("Uploading '{}'...", filename));
                    let _ = cmd_tx.send(IoCmd::WriteFile {
                        local_path: path.clone(),
                        fatx_path,
                    });
                }
                app.download_dir = path.parent().unwrap_or(&PathBuf::from(".")).to_path_buf();
                app.is_busy = true;
            } else if app.input_prompt.starts_with("New directory") {
                // Mkdir
                if !input.is_empty() {
                    let path = app.full_path(&input);
                    app.set_status(&format!("Creating '{}'...", input));
                    let _ = cmd_tx.send(IoCmd::Mkdir { path });
                    app.is_busy = true;
                }
            } else if app.input_prompt.starts_with("Rename '") {
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
            } else if app.input_prompt.starts_with("Delete '") {
                // Confirm delete
                if input.eq_ignore_ascii_case("y") || input.eq_ignore_ascii_case("yes") {
                    let name = app.selected_name().unwrap_or_default();
                    let is_dir = app.selected_entry().map(|e| e.is_dir).unwrap_or(false);
                    let path = app.full_path(&name);
                    app.set_status(&format!("Deleting '{}'...", name));
                    let _ = cmd_tx.send(IoCmd::Delete {
                        path,
                        recursive: is_dir,
                    });
                    app.is_busy = true;
                } else {
                    app.set_status("Delete cancelled.");
                }
            } else if app.input_prompt.contains("Delete? (y/n):") {
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
        }
        KeyCode::Backspace => {
            app.input_buffer.pop();
        }
        KeyCode::Char(c) => {
            if app.input_mode == InputMode::ConfirmDelete
                || app.input_mode == InputMode::ConfirmCleanup
            {
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
    let items: Vec<ListItem> = app
        .entries
        .iter()
        .map(|e| {
            let icon = if e.is_dir { "📁" } else { "📄" };
            let size_str = if e.is_dir {
                "<DIR>".to_string()
            } else {
                format_size(e.size)
            };
            let line = format!(
                " {} {:<42} {:>10}  {}  {}",
                icon, e.name, size_str, e.modified, e.attributes,
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
            .style(Style::default().fg(Color::LightYellow).bg(Color::Blue))
            .block(
                Block::default()
                    .title(" Input (Enter to confirm, Esc to cancel) ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::LightYellow)),
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
            " d:download  u:upload  n:mkdir  D:delete  r:rename  i:info  c:cleanup  q:quit "
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
}
