//! Slot-aware folder-name formatting for human-display surfaces.
//!
//! On an Xbox 360 FATX volume, folder names in different positions of the
//! `Content/<XUID>/<TitleID>/<ContentType>/<file>` tree represent different
//! kinds of identifiers. This module classifies a parent path into a
//! [`FolderSlot`] and dispatches to the right resolver in
//! [`crate::titles`], [`crate::content_types`], or [`crate::xuids`].
//!
//! Format contract (shared across slot resolvers):
//!   * Resolvable ID → `"<name> [<raw>]"`
//!   * Unresolvable  → `<raw>` unchanged (no case normalization, no prefixes)
//!
//! The NFS server intentionally does **not** call into here — over NFS the
//! folder name is the path key and must round-trip losslessly. This module
//! is for the CLI, TUI, and any other human-facing surface.

/// Classification of a directory listing's children based on the parent path
/// in the `Content/<XUID>/<TitleID>/<ContentType>/` tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FolderSlot {
    /// Children of `/Content` — 16-hex XUID folders.
    Xuid,
    /// Children of `/Content/<XUID>` — 8-hex title-ID folders.
    TitleId,
    /// Children of `/Content/<XUID>/<TitleID>` — 8-hex content-type folders.
    ContentType,
    /// Anything else — render filenames as-is.
    File,
}

/// Determine which slot the *children* of `parent_path` occupy.
///
/// Path comparison on the `Content` root is case-insensitive to match FATX's
/// case-insensitive filename semantics.
pub fn folder_slot(parent_path: &str) -> FolderSlot {
    let trimmed = parent_path.trim_matches('/');
    if trimmed.is_empty() {
        return FolderSlot::File;
    }
    let parts: Vec<&str> = trimmed.split('/').collect();
    let in_content_tree = parts
        .first()
        .map(|s| s.eq_ignore_ascii_case("Content"))
        .unwrap_or(false);
    if !in_content_tree {
        return FolderSlot::File;
    }
    match parts.len() {
        1 => FolderSlot::Xuid,
        2 => FolderSlot::TitleId,
        3 => FolderSlot::ContentType,
        _ => FolderSlot::File,
    }
}

/// Render a raw folder name in the slot implied by its parent path.
pub fn format_for_path(parent_path: &str, raw: &str) -> String {
    match folder_slot(parent_path) {
        FolderSlot::Xuid => crate::xuids::format_folder(raw),
        FolderSlot::TitleId => crate::titles::format_folder(raw),
        FolderSlot::ContentType => crate::content_types::format_folder(raw),
        FolderSlot::File => raw.to_string(),
    }
}

/// Shared format helper — keeps the bracket/raw-passthrough behavior in one
/// place. Internal to the crate; slot modules call it from their
/// `format_folder` wrappers.
pub(crate) fn format_with_raw(raw: &str, resolved: Option<&str>) -> String {
    match resolved {
        Some(name) => format!("{name} [{raw}]"),
        None => raw.to_string(),
    }
}
