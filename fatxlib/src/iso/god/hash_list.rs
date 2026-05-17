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
