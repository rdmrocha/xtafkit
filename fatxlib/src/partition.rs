//! Xbox partition table detection and probing.
//!
//! Both the original Xbox and Xbox 360 use fixed partition layouts (no MBR/GPT).
//! The offsets are hardcoded in the console's kernel. This module checks all
//! known offsets for both generations and reports which ones have valid magic.

use std::io::{Read, Seek, SeekFrom, Write};

use log::{debug, info};

use crate::error::FatxError;
use crate::error::Result;
use crate::types::*;

/// A detected FATX/XTAF partition on a device.
#[derive(Debug, Clone)]
pub struct DetectedPartition {
    pub name: String,
    pub offset: u64,
    pub size: u64,
    pub has_valid_magic: bool,
    /// Which magic was found ("FATX", "XTAF", or "none").
    pub magic: String,
    pub generation: XboxGeneration,
}

/// Probe a device for FATX/XTAF partitions at all known Xbox & Xbox 360 offsets.
///
/// `device_size` is the total device size in bytes. On macOS you should get this
/// via `platform::get_block_device_size()` for raw devices, since `seek(End(0))`
/// returns 0 on `/dev/rdiskN`.
pub fn detect_xbox_partitions<T: Read + Write + Seek>(
    device: &mut T,
    device_size: u64,
) -> Result<Vec<DetectedPartition>> {
    if device_size == 0 {
        return Err(FatxError::Other(
            "device size must be supplied for raw devices on macOS".to_string(),
        ));
    }
    info!(
        "Device size: {} (0x{:X} bytes)",
        format_size(device_size),
        device_size
    );
    let mut results = Vec::new();
    // Track which offsets we've already checked to avoid duplicates
    // (OG Xbox offset 0x80000 overlaps with 360 System Cache)
    let mut seen_offsets = std::collections::HashSet::new();

    let all_parts = all_known_partitions();
    debug!("Checking {} known partition offsets", all_parts.len());

    for part in all_parts {
        if seen_offsets.contains(&part.offset) {
            debug!(
                "Skipping duplicate offset 0x{:X} ({})",
                part.offset, part.name
            );
            continue;
        }
        seen_offsets.insert(part.offset);

        if part.offset >= device_size {
            debug!(
                "Skipping '{}' at 0x{:X} — beyond device size",
                part.name, part.offset
            );
            continue;
        }

        let size = if part.size == 0 {
            device_size - part.offset
        } else {
            part.size.min(device_size - part.offset)
        };

        let (valid, magic_str) = probe_magic(device, part.offset)?;
        debug!(
            "Partition '{}' at 0x{:X} (size {}): {}",
            part.name,
            part.offset,
            format_size(size),
            if valid { &magic_str } else { "no magic" }
        );

        results.push(DetectedPartition {
            name: part.name.to_string(),
            offset: part.offset,
            size,
            has_valid_magic: valid,
            magic: magic_str,
            generation: part.generation,
        });
    }

    let valid_count = results.iter().filter(|p| p.has_valid_magic).count();
    info!(
        "Checked {} offsets, {} with valid magic",
        results.len(),
        valid_count
    );

    Ok(results)
}

/// Probe for FATX/XTAF magic at offset 0 (i.e., the device itself is a volume).
pub fn probe_fatx_at_start<T: Read + Seek>(device: &mut T) -> Result<bool> {
    let (valid, _) = probe_magic(device, 0)?;
    Ok(valid)
}

/// Check for FATX or XTAF magic at the given offset.
/// Returns (is_valid, magic_string).
///
/// IMPORTANT: macOS raw devices (/dev/rdiskN) require reads aligned to the
/// device block size (typically 512 bytes). We always read a full sector
/// and then check the first 4 bytes.
fn probe_magic<T: Read + Seek>(device: &mut T, offset: u64) -> Result<(bool, String)> {
    // Align the read to a 512-byte sector boundary
    let sector_offset = offset & !0x1FF; // round down to sector
    let byte_within_sector = (offset - sector_offset) as usize;

    device.seek(SeekFrom::Start(sector_offset))?;
    let mut sector = [0u8; 512];
    match device.read_exact(&mut sector) {
        Ok(()) => {
            let magic: [u8; 4] = [
                sector[byte_within_sector],
                sector[byte_within_sector + 1],
                sector[byte_within_sector + 2],
                sector[byte_within_sector + 3],
            ];
            if magic == FATX_MAGIC {
                Ok((true, "FATX".to_string()))
            } else if magic == XTAF_MAGIC {
                Ok((true, "XTAF".to_string()))
            } else {
                Ok((
                    false,
                    format!(
                        "{:02X} {:02X} {:02X} {:02X}",
                        magic[0], magic[1], magic[2], magic[3]
                    ),
                ))
            }
        }
        Err(_) => Ok((false, "read error".to_string())),
    }
}

/// Scan a device sector-by-sector for FATX/XTAF magic signatures.
/// This is a brute-force approach for non-standard partition layouts.
///
/// `device_size` is the total device size in bytes. On macOS raw devices
/// should pass `platform::get_block_device_size()` instead of relying on
/// `seek(End(0))`.
pub fn scan_for_fatx<T: Read + Write + Seek>(
    device: &mut T,
    device_size: u64,
    max_offset: u64,
) -> Result<Vec<u64>> {
    if device_size == 0 {
        return Err(FatxError::Other(
            "device size must be supplied for raw devices on macOS".to_string(),
        ));
    }
    let scan_limit = max_offset.min(device_size);
    let mut found = Vec::new();

    info!(
        "Scanning for FATX/XTAF signatures up to offset 0x{:X}...",
        scan_limit
    );

    let mut offset = 0u64;
    while offset < scan_limit {
        let (valid, _) = probe_magic(device, offset)?;
        if valid {
            info!("Found magic at offset 0x{:X}", offset);
            found.push(offset);
        }
        offset += SECTOR_SIZE;
    }

    Ok(found)
}

/// Format a byte count as a human-readable size string.
pub fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    const TB: u64 = 1024 * GB;

    if bytes >= TB {
        format!("{:.2} TB", bytes as f64 / TB as f64)
    } else if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.2} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.2} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} bytes", bytes)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    #[test]
    fn detect_partitions_requires_explicit_size_when_autodetect_is_zero() {
        let mut cursor = Cursor::new(vec![]);
        assert!(detect_xbox_partitions(&mut cursor, 0).is_err());
    }

    #[test]
    fn scan_for_fatx_requires_explicit_size_when_autodetect_is_zero() {
        let mut cursor = Cursor::new(vec![]);
        assert!(scan_for_fatx(&mut cursor, 0, 0).is_err());
    }

    #[test]
    fn detect_partitions_finds_valid_magic_with_explicit_size() {
        let mut image = vec![0u8; 0x90000];
        image[0x80000..0x80004].copy_from_slice(&FATX_MAGIC);
        let mut cursor = Cursor::new(image);

        let parts = detect_xbox_partitions(&mut cursor, 0x90000).expect("detect partitions");
        let part = parts
            .iter()
            .find(|part| part.offset == 0x80000)
            .expect("360 System Cache partition");

        assert!(part.has_valid_magic);
        assert_eq!(part.magic, "FATX");
    }

    #[test]
    fn scan_for_fatx_finds_valid_magic_with_explicit_size() {
        let mut image = vec![0u8; 0x90000];
        image[0x80000..0x80004].copy_from_slice(&FATX_MAGIC);
        let mut cursor = Cursor::new(image);

        let offsets = scan_for_fatx(&mut cursor, 0x90000, 0x90000).expect("scan");
        assert!(offsets.contains(&0x80000));
    }
}
