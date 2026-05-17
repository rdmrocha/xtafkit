use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::error::{FatxError, Result};
use crate::executable::TitleInfo;
use crate::iso::compact::build_compact_source;

use super::{
    BLOCK_SIZE, BLOCKS_PER_PART, ContentType, ConvertOptions, ConvertReport, SOURCE_BUFFER_SIZE,
    TrimMode,
};

pub(crate) trait ReadSeek: Read + Seek {}

impl<T: Read + Seek> ReadSeek for T {}

pub(crate) struct PreparedSource {
    pub(crate) report: ConvertReport,
    pub(crate) exe_info: crate::executable::TitleExecutionInfo,
    pub(crate) content_type: ContentType,
    reader: ReaderSource,
}

enum ReaderSource {
    Raw {
        source_iso: PathBuf,
        root_offset: u64,
    },
    Compact {
        source_iso: PathBuf,
        compact: crate::iso::compact::CompactSource,
    },
}

impl PreparedSource {
    pub(crate) fn open_reader(&self) -> Result<Box<dyn ReadSeek + '_>> {
        self.reader.open_reader()
    }
}

impl ReaderSource {
    fn open_reader(&self) -> Result<Box<dyn ReadSeek + '_>> {
        match self {
            Self::Raw {
                source_iso,
                root_offset,
            } => {
                let mut iso = File::open(source_iso).map_err(FatxError::Io)?;
                iso.seek(SeekFrom::Start(*root_offset))
                    .map_err(FatxError::Io)?;
                Ok(Box::new(iso))
            }
            Self::Compact {
                source_iso,
                compact,
            } => Ok(Box::new(compact.open_reader(source_iso)?)),
        }
    }
}

pub(crate) fn prepare_source(
    source_iso: &Path,
    opts: &ConvertOptions<'_>,
) -> Result<PreparedSource> {
    if matches!(opts.trim, TrimMode::Compact) {
        let compact = build_compact_source(source_iso, opts.should_abort)?;
        let report = build_report(
            compact.exe_info().title_id,
            compact.exe_info().media_id,
            compact.content_type(),
            compact.data_size(),
        );
        return Ok(PreparedSource {
            exe_info: compact.exe_info().clone(),
            content_type: compact.content_type(),
            report,
            reader: ReaderSource::Compact {
                source_iso: source_iso.to_path_buf(),
                compact,
            },
        });
    }

    let source_iso_file_meta = std::fs::metadata(source_iso).map_err(FatxError::Io)?;
    let img = File::open(source_iso).map_err(FatxError::Io)?;
    let xiso = BufReader::with_capacity(SOURCE_BUFFER_SIZE, img);
    let mut xiso = xdvdfs::blockdev::OffsetWrapper::new(xiso)
        .map_err(|e| FatxError::Other(format!("xdvdfs offset detect: {e:?}")))?;
    let volume = xdvdfs::read::read_volume(&mut xiso)
        .map_err(|e| FatxError::Other(format!("xdvdfs read_volume: {e:?}")))?;
    let title_info = TitleInfo::from_image(&mut xiso, volume)?;
    let exe_info = title_info.execution_info;
    let content_type = title_info.content_type;
    let root_offset = {
        xiso.seek(SeekFrom::Start(0)).map_err(FatxError::Io)?;
        xiso.get_mut().stream_position().map_err(FatxError::Io)?
    };
    let data_size = match opts.trim {
        TrimMode::PreserveLayout => volume
            .root_table
            .file_tree(&mut xiso)
            .map_err(|e| FatxError::Other(format!("xdvdfs file_tree: {e:?}")))?
            .iter()
            .map(|dirent| {
                if dirent.1.node.dirent.data.is_empty() {
                    return 0;
                }
                let offset = dirent
                    .1
                    .node
                    .dirent
                    .data
                    .offset::<std::io::Error>(0)
                    .unwrap_or(0);
                offset + dirent.1.node.dirent.data.size() as u64
            })
            .max()
            .unwrap_or(0),
        TrimMode::None => source_iso_file_meta.len() - root_offset,
        TrimMode::Compact => unreachable!("compact handled before metadata pass"),
    };
    let report = build_report(
        exe_info.title_id,
        exe_info.media_id,
        content_type,
        data_size,
    );
    Ok(PreparedSource {
        exe_info,
        content_type,
        report,
        reader: ReaderSource::Raw {
            source_iso: source_iso.to_path_buf(),
            root_offset,
        },
    })
}

fn build_report(
    title_id: u32,
    media_id: u32,
    content_type: ContentType,
    data_size: u64,
) -> ConvertReport {
    let block_count = data_size.div_ceil(BLOCK_SIZE);
    let part_count = block_count.div_ceil(BLOCKS_PER_PART);
    ConvertReport {
        title_id,
        media_id,
        content_type,
        part_count,
        block_count,
        data_size,
    }
}
