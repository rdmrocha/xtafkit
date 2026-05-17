use crate::error::{FatxError, Result};

use super::prepare::PreparedSource;
use super::{
    BLOCK_SIZE, BLOCKS_PER_PART, ConHeaderBuilder, ConvertOptions, ConvertReport, HashList,
};

pub(crate) trait GodSink {
    fn begin(&mut self, source: &PreparedSource) -> Result<()>;
    fn write_part<'a>(
        &mut self,
        source: &PreparedSource,
        part_index: u64,
        opts: &mut ConvertOptions<'a>,
    ) -> Result<()>;
    fn read_master_hash(&mut self, source: &PreparedSource, part_index: u64) -> Result<HashList>;
    fn write_master_hash(
        &mut self,
        source: &PreparedSource,
        part_index: u64,
        mht: &HashList,
    ) -> Result<()>;
    fn last_part_size(&self, source: &PreparedSource) -> Result<u64>;
    fn write_con_header(&mut self, source: &PreparedSource, con_bytes: Vec<u8>) -> Result<()>;
    fn flush_after_parts(&mut self) -> Result<()> {
        Ok(())
    }
    fn flush_after_mht(&mut self) -> Result<()> {
        Ok(())
    }
    fn flush_after_header(&mut self) -> Result<()> {
        Ok(())
    }
}

pub(crate) fn run_conversion<'a, S: GodSink>(
    source: &PreparedSource,
    sink: &mut S,
    opts: &mut ConvertOptions<'a>,
    cancel_ctx: &str,
) -> Result<ConvertReport> {
    if source.report.part_count == 0 {
        return Err(FatxError::Other(format!(
            "{cancel_ctx}: source has no data to convert"
        )));
    }

    sink.begin(source)?;

    if let Some(cb) = opts.progress.as_deref_mut() {
        cb("parts", 0, source.report.part_count);
    }
    for part_index in 0..source.report.part_count {
        if let Some(abort) = opts.should_abort
            && abort()
        {
            return Err(FatxError::Other(format!("{cancel_ctx}: cancelled")));
        }
        sink.write_part(source, part_index, opts)?;
        if let Some(cb) = opts.progress.as_deref_mut() {
            cb("parts", part_index + 1, source.report.part_count);
        }
    }
    sink.flush_after_parts()?;

    if let Some(cb) = opts.progress.as_deref_mut() {
        cb("mht", 0, source.report.part_count);
    }
    let mut mht = sink.read_master_hash(source, source.report.part_count - 1)?;
    for prev_part_index in (0..source.report.part_count - 1).rev() {
        if let Some(abort) = opts.should_abort
            && abort()
        {
            return Err(FatxError::Other(format!("{cancel_ctx}: cancelled")));
        }
        let mut prev_mht = sink.read_master_hash(source, prev_part_index)?;
        prev_mht.add_hash(&mht.digest());
        sink.write_master_hash(source, prev_part_index, &prev_mht)?;
        mht = prev_mht;
        if let Some(cb) = opts.progress.as_deref_mut() {
            cb(
                "mht",
                source.report.part_count - prev_part_index,
                source.report.part_count,
            );
        }
    }
    sink.flush_after_mht()?;

    if let Some(cb) = opts.progress.as_deref_mut() {
        cb("header", 0, 1);
    }
    let last_part_size = sink.last_part_size(source)?;
    let con_header = build_con_header(source, &mht.digest(), opts.game_title, last_part_size);
    sink.write_con_header(source, con_header)?;
    sink.flush_after_header()?;
    if let Some(cb) = opts.progress.as_deref_mut() {
        cb("header", 1, 1);
    }

    Ok(source.report)
}

pub(crate) fn build_con_header(
    source: &PreparedSource,
    mht_digest: &[u8; 20],
    game_title: Option<&str>,
    last_part_size: u64,
) -> Vec<u8> {
    let mut con_header = ConHeaderBuilder::new()
        .with_execution_info(&source.exe_info)
        .with_block_counts(source.report.block_count as u32, 0)
        .with_data_parts_info(
            source.report.part_count as u32,
            last_part_size + (source.report.part_count - 1) * BLOCK_SIZE * 0xa290,
        )
        .with_content_type(source.content_type)
        .with_mht_hash(mht_digest);
    if let Some(title) = game_title {
        con_header = con_header.with_game_title(title);
    }
    con_header.finalize()
}

pub(crate) fn part_payload_bytes(data_size: u64, part_index: u64) -> u64 {
    let part_start = part_index
        .saturating_mul(BLOCKS_PER_PART)
        .saturating_mul(BLOCK_SIZE);
    data_size
        .saturating_sub(part_start)
        .min(BLOCKS_PER_PART * BLOCK_SIZE)
}
