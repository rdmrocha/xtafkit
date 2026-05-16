//! Title-ID → human-readable name resolution.
//!
//! A single merged lookup table covers both Xbox 360 titles
//! ([AdrianCassar gist](https://gist.github.com/AdrianCassar/c0d05a14608168259232b3ed8c77f28c))
//! and Original Xbox titles
//! ([jeltaqq's list](https://github.com/jeltaqq/Xbox-Original-GameList)).
//! The map is generated at build time from `fatxlib/data/*` by `build.rs`.
//!
//! When the same title ID appears in both sources, the Original Xbox name
//! wins (it's derived directly from the disc's `default.xbe` and tends to
//! have better editorial capitalization/punctuation), and `source` is set
//! to [`Source::Both`].
//!
//! Two submodules layer on top of the static catalog:
//!   * [`dynamic`] — read an STFS package header to resolve an unknown ID.
//!   * [`user_cache`] — persist successful dynamic resolutions to disk so
//!     they survive across runs.

pub mod dynamic;
pub mod user_cache;

/// Which catalog(s) sourced this entry. Useful for UI hints like a `[BC]`
/// badge on backwards-compatible titles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Xbox360,
    XboxOriginal,
    Both,
    /// Resolved at runtime via STFS header parse and stored in the user cache.
    User,
}

/// One entry in the merged title catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TitleInfo {
    pub name: &'static str,
    pub source: Source,
}

include!(concat!(env!("OUT_DIR"), "/titles.rs"));

/// Resolve a title ID to its display name and source. The compiled-in
/// catalog wins; if it misses, the runtime user cache (populated by
/// `user_cache::load_from` and the on-demand resolver) is consulted next.
/// Returns `None` if neither source knows this ID.
pub fn lookup(title_id: u32) -> Option<TitleInfo> {
    if let Some(info) = TITLES.get(&title_id).copied() {
        return Some(info);
    }
    user_cache::lookup(title_id).map(|name| TitleInfo {
        // Leak is fine: the user cache is a tiny long-lived map. This avoids
        // changing TitleInfo to own its name string and rippling that
        // through every caller.
        name: Box::leak(name.into_boxed_str()),
        source: Source::User,
    })
}

/// Render a raw on-disk title folder name (e.g. `"4D5307E6"`) as
/// `"<name> [<raw>]"` if known, otherwise just `<raw>` unchanged. Raw case
/// is preserved verbatim — lowercase on disk surfaces as lowercase in the
/// display, since Xbox 360 writes upper-hex and lower-hex would signal a
/// non-standard source worth noticing.
pub fn format_folder(raw: &str) -> String {
    let resolved = u32::from_str_radix(raw, 16)
        .ok()
        .and_then(|id| lookup(id).map(|info| info.name));
    crate::display::format_with_raw(raw, resolved)
}
