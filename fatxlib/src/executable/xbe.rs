use crate::error::{FatxError, Result};
use crate::executable::TitleExecutionInfo;
use byteorder::{LE, ReadBytesExt};
use std::io::{Read, Seek, SeekFrom};

pub struct XbeHeader {
    // We only need these fields to get the cert address
    pub dw_base_addr: u32,
    pub dw_certificate_addr: u32,
    pub fields: XbeHeaderFields,
}

#[derive(Clone, Default, Debug)]
pub struct XbeHeaderFields {
    pub execution_info: Option<TitleExecutionInfo>,
}

impl XbeHeader {
    pub fn read<R: Read + Seek>(mut reader: R) -> Result<XbeHeader> {
        Self::check_magic_bytes(&mut reader)?;

        // Offset 0x0104
        reader.seek(SeekFrom::Current(256)).map_err(FatxError::Io)?;
        let dw_base_addr = reader.read_u32::<LE>().map_err(FatxError::Io)?;

        // Offset 0x0118
        reader.seek(SeekFrom::Current(16)).map_err(FatxError::Io)?;
        let dw_certificate_addr = reader.read_u32::<LE>().map_err(FatxError::Io)?;

        let offset = reader.stream_position().map_err(FatxError::Io)? - 284;
        let cert_address = dw_certificate_addr - dw_base_addr;
        reader
            .seek(SeekFrom::Start(offset + (cert_address as u64)))
            .map_err(FatxError::Io)?;

        Ok(XbeHeader {
            dw_base_addr,
            dw_certificate_addr,
            fields: XbeHeaderFields {
                execution_info: Some(TitleExecutionInfo::from_xbe(reader)?),
            },
        })
    }

    fn check_magic_bytes<R: Read + Seek>(mut reader: R) -> Result<()> {
        let mut magic_bytes = [0u8; 4];
        reader.read_exact(&mut magic_bytes).map_err(FatxError::Io)?;

        if &magic_bytes != b"XBEH" {
            return Err(FatxError::Other(
                "missing 'XBEH' magic bytes in XBE header".to_string(),
            ));
        }

        Ok(())
    }
}
