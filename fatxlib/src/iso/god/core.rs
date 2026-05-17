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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executable::TitleExecutionInfo;
    use crate::iso::god::prepare::test_prepared_source;
    use std::io::Cursor;

    struct TestSink {
        masters: Vec<HashList>,
        writes: Vec<(u64, [u8; 20])>,
        con_header: Option<Vec<u8>>,
        last_part_size: u64,
    }

    impl TestSink {
        fn new(masters: Vec<HashList>, last_part_size: u64) -> Self {
            Self {
                masters,
                writes: Vec::new(),
                con_header: None,
                last_part_size,
            }
        }
    }

    impl GodSink for TestSink {
        fn begin(&mut self, _source: &PreparedSource) -> Result<()> {
            Ok(())
        }

        fn write_part<'a>(
            &mut self,
            _source: &PreparedSource,
            _part_index: u64,
            _opts: &mut ConvertOptions<'a>,
        ) -> Result<()> {
            Ok(())
        }

        fn read_master_hash(
            &mut self,
            _source: &PreparedSource,
            part_index: u64,
        ) -> Result<HashList> {
            self.masters
                .get(part_index as usize)
                .cloned()
                .ok_or_else(|| FatxError::Other("missing master".to_string()))
        }

        fn write_master_hash(
            &mut self,
            _source: &PreparedSource,
            part_index: u64,
            mht: &HashList,
        ) -> Result<()> {
            self.writes.push((part_index, mht.digest()));
            if let Some(slot) = self.masters.get_mut(part_index as usize) {
                *slot = mht.clone();
            }
            Ok(())
        }

        fn last_part_size(&self, _source: &PreparedSource) -> Result<u64> {
            Ok(self.last_part_size)
        }

        fn write_con_header(&mut self, _source: &PreparedSource, con_bytes: Vec<u8>) -> Result<()> {
            self.con_header = Some(con_bytes);
            Ok(())
        }
    }

    fn hash_list_from_seed(seed: u8) -> HashList {
        let mut bytes = [0u8; 4096];
        bytes[..20].fill(seed);
        HashList::read(Cursor::new(bytes)).expect("seeded hash list")
    }

    #[test]
    fn mht_back_chain_updates_previous_part() {
        let report = ConvertReport {
            title_id: 0x4D53_07E6,
            media_id: 0x1234_5678,
            content_type: super::super::ContentType::GamesOnDemand,
            part_count: 3,
            block_count: 3,
            data_size: BLOCK_SIZE * BLOCKS_PER_PART * 2,
        };
        let source = test_prepared_source(
            report,
            TitleExecutionInfo {
                media_id: 0x1234_5678,
                version: 1,
                base_version: 0,
                title_id: 0x4D53_07E6,
                platform: 0xFF,
                executable_type: 0,
                disc_number: 1,
                disc_count: 1,
            },
            super::super::ContentType::GamesOnDemand,
        );
        let part0 = hash_list_from_seed(0x11);
        let part1 = hash_list_from_seed(0x22);
        let part2 = hash_list_from_seed(0x33);
        let mut sink = TestSink::new(
            vec![part0.clone(), part1.clone(), part2.clone()],
            BLOCK_SIZE,
        );
        let mut opts = ConvertOptions::default();

        let result = run_conversion(&source, &mut sink, &mut opts, "test").expect("conversion");
        assert_eq!(result.part_count, 3);
        assert_eq!(sink.writes.len(), 2);

        assert_eq!(sink.writes[0].0, 1);
        assert_eq!(
            sink.writes[0].1,
            [
                0x6a, 0xb0, 0xca, 0x46, 0x1a, 0x72, 0xcf, 0x70, 0x51, 0x79, 0xa1, 0xf3, 0x08, 0x8b,
                0x54, 0xfa, 0x4d, 0xd4, 0xfa, 0x24,
            ]
        );
        assert_eq!(sink.writes[1].0, 0);
        assert_eq!(
            sink.writes[1].1,
            [
                0xe9, 0x63, 0x9e, 0x2e, 0x25, 0xf0, 0x3c, 0x58, 0xa2, 0x07, 0xb7, 0xad, 0x9b, 0x27,
                0x4e, 0xa5, 0x5b, 0x61, 0x15, 0x0d,
            ]
        );
        assert!(sink.con_header.is_some());
    }
}
