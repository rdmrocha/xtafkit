//! ISO → Games-on-Demand conversion pipeline.
//!
//! Vendored from [QAston/iso2god-rs `xdvdfx` branch](https://github.com/QAston/iso2god-rs/tree/xdvdfx)
//! (parent: [iliazeus/iso2god-rs](https://github.com/iliazeus/iso2god-rs);
//! both MIT-licensed). Local deviations from upstream:
//!
//! - `anyhow::Error` → [`crate::error::FatxError`] so errors flow through
//!   the same channel as the rest of fatxlib.
//! - Upstream's `src/executable/` lives at [`crate::executable`] now — it
//!   gets shared with [`crate::xiso`] for folder-name resolution and isn't
//!   specific to GoD conversion.
//! - Intra-crate `use crate::god` imports rewritten to
//!   `use crate::iso2god::god`.
//! - The original `src/game_list/` (4.9 KLOC of compiled-in title catalog) is
//!   dropped; fatxlib already has a richer catalog via [`crate::titles`].
//! - The upstream binary (`src/bin/iso2god.rs`) lives elsewhere — fatxlib only
//!   provides the library surface; the CLI/TUI wraps it in `xtafkit`.
//!
//! See `NOTICE` at the repo root for the full attribution.

pub mod god;

mod convert;
pub use convert::{
    ConvertOptions, ConvertReport, SOURCE_BUFFER_SIZE, TrimMode, convert_iso, convert_iso_to_fatx,
};

/// Single hot-path SHA-1 entry point used by [`god::HashList`] and
/// [`god::ConHeaderBuilder`]. With the `openssl-hash` feature (default on)
/// this routes to `openssl::sha::sha1`, which uses ARMv8 SHA on Apple
/// Silicon and SHA-NI on x86. Without the feature it falls back to the
/// portable-Rust `sha1` crate.
///
/// On hardware that exposes accelerated SHA-1, the OpenSSL path can be
/// measurably faster for large workloads. Disable the feature to drop
/// the dependency if the build environment can't reach a system OpenSSL.
#[inline]
pub(crate) fn sha1_digest(data: &[u8]) -> [u8; 20] {
    #[cfg(feature = "openssl-hash")]
    {
        openssl::sha::sha1(data)
    }
    #[cfg(not(feature = "openssl-hash"))]
    {
        use sha1::{Digest, Sha1};
        Sha1::digest(data).into()
    }
}
