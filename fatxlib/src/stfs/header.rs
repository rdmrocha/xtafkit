//! STFS (Secure Transacted File System) header parser.
//!
//! STFS is the container format used by Xbox 360 packages (CON / LIVE / PIRS).
//! We only parse the metadata at the *start* of the file — title ID, title
//! name, display name — to power on-demand title resolution when the bundled
//! catalog doesn't have a name for a folder.
//!
//! Layout (from <https://free60.org/System-Software/Formats/STFS/>):
//! ```text
//!   0x0000-0x0003  Magic  "CON ", "LIVE", or "PIRS"
//!   0x0360-0x0363  Title ID, u32 big-endian
//!   0x0411-0x0690  Display Name, 18 locales × 0x80 bytes UTF-16BE
//!   0x1691-0x1710  Title Name,   0x80 bytes UTF-8
//! ```
//!
//! We need the first `MIN_HEADER_BYTES` bytes of a package to parse fully.

/// Minimum bytes required to read the full set of metadata we care about
/// (through the end of the title-name field).
pub const MIN_HEADER_BYTES: usize = 0x1691 + 0x80;

const MAGIC_CON: [u8; 4] = *b"CON ";
const MAGIC_LIVE: [u8; 4] = *b"LIVE";
const MAGIC_PIRS: [u8; 4] = *b"PIRS";

const TITLE_ID_OFFSET: usize = 0x0360;
const DISPLAY_NAME_OFFSET: usize = 0x0411;
const DISPLAY_NAME_LEN: usize = 0x80;
const TITLE_NAME_OFFSET: usize = 0x1691;
const TITLE_NAME_LEN: usize = 0x80;

/// Parsed STFS metadata. We expose just what's needed to label a folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StfsHeader {
    pub magic: [u8; 4],
    pub title_id: u32,
    /// UTF-8 title name at offset 0x1691. Often the consistent "game"-level
    /// name (e.g. `"Halo 3"`) shared across that title's DLC packages.
    pub title_name: String,
    /// Locale-0 UTF-16BE display name at offset 0x0411. Often the
    /// package-specific name (e.g. `"Halo 3 Multiplayer Map Pack"`).
    pub display_name: String,
}

impl StfsHeader {
    /// Best display label: prefer `title_name`, fall back to `display_name`.
    pub fn best_name(&self) -> &str {
        if !self.title_name.is_empty() {
            &self.title_name
        } else {
            &self.display_name
        }
    }
}

/// Parse an STFS header from the prefix of a package file. Returns `None`
/// for unknown magic or for buffers shorter than [`MIN_HEADER_BYTES`].
pub fn parse_header(bytes: &[u8]) -> Option<StfsHeader> {
    if bytes.len() < MIN_HEADER_BYTES {
        return None;
    }

    let mut magic = [0u8; 4];
    magic.copy_from_slice(&bytes[0..4]);
    if magic != MAGIC_CON && magic != MAGIC_LIVE && magic != MAGIC_PIRS {
        return None;
    }

    let title_id = u32::from_be_bytes(
        bytes[TITLE_ID_OFFSET..TITLE_ID_OFFSET + 4]
            .try_into()
            .ok()?,
    );

    let display_name =
        decode_utf16be(&bytes[DISPLAY_NAME_OFFSET..DISPLAY_NAME_OFFSET + DISPLAY_NAME_LEN]);

    let title_name =
        decode_utf8_padded(&bytes[TITLE_NAME_OFFSET..TITLE_NAME_OFFSET + TITLE_NAME_LEN]);

    Some(StfsHeader {
        magic,
        title_id,
        title_name,
        display_name,
    })
}

fn decode_utf16be(bytes: &[u8]) -> String {
    let mut chars = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks_exact(2) {
        let cu = u16::from_be_bytes([pair[0], pair[1]]);
        if cu == 0 {
            break;
        }
        chars.push(cu);
    }
    String::from_utf16_lossy(&chars)
        .trim_end_matches('\0')
        .to_string()
}

fn decode_utf8_padded(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}
