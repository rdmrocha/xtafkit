use crate::error::{FatxError, Result};
use crate::iso2god::god::ContentType;
// NB: ContentType lives in `iso2god::god` because it's part of the GoD
// container format's CON header. We pull it in here only because TitleInfo
// reports it alongside the execution info. If iso2god ever moves out to a
// sibling crate, this `use` becomes the seam to revisit.
use byteorder::{BE, LE, ReadBytesExt};
use std::io::{Read, Seek, SeekFrom};
use xdvdfs::{blockdev::BlockDeviceRead, layout::VolumeDescriptor};

pub mod xbe;
pub mod xex;

#[derive(Clone, Debug)]
pub struct TitleExecutionInfo {
    pub media_id: u32,
    pub version: u32,
    pub base_version: u32,
    pub title_id: u32,
    pub platform: u8,
    pub executable_type: u8,
    pub disc_number: u8,
    pub disc_count: u8,
}

pub struct TitleInfo {
    pub content_type: ContentType,
    pub execution_info: TitleExecutionInfo,
}

impl TitleExecutionInfo {
    pub fn from_xex<R: Read>(mut reader: R) -> Result<TitleExecutionInfo> {
        Ok(TitleExecutionInfo {
            media_id: reader.read_u32::<BE>().map_err(FatxError::Io)?,
            version: reader.read_u32::<BE>().map_err(FatxError::Io)?,
            base_version: reader.read_u32::<BE>().map_err(FatxError::Io)?,
            title_id: reader.read_u32::<BE>().map_err(FatxError::Io)?,
            platform: reader.read_u8().map_err(FatxError::Io)?,
            executable_type: reader.read_u8().map_err(FatxError::Io)?,
            disc_number: reader.read_u8().map_err(FatxError::Io)?,
            disc_count: reader.read_u8().map_err(FatxError::Io)?,
        })
    }

    pub fn from_xbe<R: Read + Seek>(mut reader: R) -> Result<TitleExecutionInfo> {
        reader.seek(SeekFrom::Current(8)).map_err(FatxError::Io)?;
        let title_id = reader.read_u32::<LE>().map_err(FatxError::Io)?;

        reader.seek(SeekFrom::Current(164)).map_err(FatxError::Io)?;
        let version = reader.read_u32::<LE>().map_err(FatxError::Io)?;

        Ok(TitleExecutionInfo {
            media_id: 0,
            version,
            base_version: 0,
            title_id,
            platform: 0,
            executable_type: 0,
            disc_number: 1,
            disc_count: 1,
        })
    }
}

impl TitleInfo {
    pub fn from_image<R: BlockDeviceRead<E> + Seek, E: std::fmt::Debug>(
        xiso: &mut R,
        volume: VolumeDescriptor,
    ) -> Result<TitleInfo> {
        if let Ok(direntnode) = volume.root_table.walk_path(xiso, "Default.xex") {
            let mut data = direntnode
                .node
                .dirent
                .read_data_all(xiso)
                .map_err(|e| FatxError::Other(format!("xdvdfs read Default.xex: {e:?}")))?;
            let mut data_slice = std::io::Cursor::new(data.as_mut());

            let default_xex_header = xex::XexHeader::read(&mut data_slice)
                .map_err(|e| FatxError::Other(format!("error reading default.xex: {e}")))?;
            let execution_info = default_xex_header.fields.execution_info.ok_or_else(|| {
                FatxError::Other("no execution info in default.xex header".to_string())
            })?;

            Ok(TitleInfo {
                content_type: ContentType::GamesOnDemand,
                execution_info,
            })
        } else if let Ok(direntnode) = volume.root_table.walk_path(xiso, "default.xbe") {
            let mut data = direntnode
                .node
                .dirent
                .read_data_all(xiso)
                .map_err(|e| FatxError::Other(format!("xdvdfs read default.xbe: {e:?}")))?;
            let mut data_slice = std::io::Cursor::new(data.as_mut());
            let default_xbe_header = xbe::XbeHeader::read(&mut data_slice)
                .map_err(|e| FatxError::Other(format!("error reading default.xbe: {e}")))?;
            let execution_info = default_xbe_header.fields.execution_info.ok_or_else(|| {
                FatxError::Other("no execution info in default.xbe header".to_string())
            })?;

            Ok(TitleInfo {
                content_type: ContentType::XboxOriginal,
                execution_info,
            })
        } else {
            Err(FatxError::Other(
                "no executable found in this image".to_string(),
            ))
        }
    }
}
