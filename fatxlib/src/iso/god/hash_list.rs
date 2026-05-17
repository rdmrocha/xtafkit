use std::io::{Read, Write};

use crate::error::{FatxError, Result};

use super::sha1_digest;

#[derive(Clone)]
pub struct HashList {
    buffer: [u8; 4096],
    len: usize,
}

impl Default for HashList {
    fn default() -> Self {
        Self::new()
    }
}

impl HashList {
    pub fn bytes(&self) -> &[u8; 4096] {
        &self.buffer
    }

    pub fn new() -> HashList {
        HashList {
            buffer: [0u8; 4096],
            len: 0,
        }
    }

    pub fn read<R: Read>(mut reader: R) -> Result<HashList> {
        let mut buffer = [0u8; 4096];
        reader.read_exact(&mut buffer).map_err(FatxError::Io)?;

        let len = buffer
            .chunks(20)
            .position(|c| *c == [0u8; 20])
            .map(|p| p * 20)
            .unwrap_or(buffer.len());

        Ok(HashList { buffer, len })
    }

    pub fn add_hash(&mut self, hash: &[u8; 20]) {
        self.buffer[self.len..self.len + 20].copy_from_slice(hash);
        self.len += 20;
    }

    pub fn add_block_hash(&mut self, block: &[u8]) {
        self.add_hash(&sha1_digest(block))
    }

    pub fn digest(&self) -> [u8; 20] {
        sha1_digest(&self.buffer)
    }

    pub fn write<W: Write>(&self, mut writer: W) -> Result<()> {
        writer.write_all(&self.buffer).map_err(FatxError::Io)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    #[test]
    fn read_preserves_fixed_bytes_and_len() {
        let mut bytes = [0u8; 4096];
        bytes[..20].fill(0x11);
        bytes[40..60].fill(0x22);

        let list = HashList::read(Cursor::new(bytes)).expect("read hash list");
        assert_eq!(&list.bytes()[..20], &[0x11; 20]);
        assert_eq!(&list.bytes()[20..40], &[0x00; 20]);
        assert_eq!(&list.bytes()[40..60], &[0x22; 20]);
    }

    #[test]
    fn write_emits_exact_fixed_buffer() {
        let mut list = HashList::new();
        list.add_hash(&[0x11; 20]);
        list.add_hash(&[0x22; 20]);

        let mut out = Vec::new();
        list.write(&mut out).expect("write hash list");

        assert_eq!(out.len(), 4096);
        assert_eq!(&out[..20], &[0x11; 20]);
        assert_eq!(&out[20..40], &[0x22; 20]);
        assert!(out[40..].iter().all(|b| *b == 0));
    }

    #[test]
    fn digest_matches_known_zero_page() {
        let list = HashList::new();
        assert_eq!(
            list.digest(),
            [
                0x1c, 0xea, 0xf7, 0x3d, 0xf4, 0x0e, 0x53, 0x1d, 0xf3, 0xbf, 0xb2, 0x6b, 0x4f, 0xb7,
                0xcd, 0x95, 0xfb, 0x7b, 0xff, 0x1d,
            ]
        );
    }
}
