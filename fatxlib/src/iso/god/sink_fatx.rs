use std::io::{Cursor, Read, Seek, Write};

use crate::error::{FatxError, Result};
use crate::volume::FatxVolume;

use super::core::{GodSink, part_payload_bytes};
use super::prepare::PreparedSource;
use super::{BLOCK_SIZE, ConvertOptions, HashList, SUBPART_SIZE, SUBPARTS_PER_PART};

pub(crate) struct FatxSink<'a, T: Read + Seek + Write> {
    vol: &'a mut FatxVolume<T>,
    dest_dir: &'a str,
    data_dir: Option<String>,
    con_header_path: Option<String>,
    part_buf: Vec<u8>,
    master_lists: Vec<HashList>,
    last_part_size: u64,
}

impl<'a, T: Read + Seek + Write> FatxSink<'a, T> {
    pub(crate) fn new(vol: &'a mut FatxVolume<T>, dest_dir: &'a str) -> Self {
        Self {
            vol,
            dest_dir,
            data_dir: None,
            con_header_path: None,
            part_buf: vec![0u8; MAX_PART_BYTES],
            master_lists: Vec::new(),
            last_part_size: 0,
        }
    }

    fn data_dir(&self) -> Result<&str> {
        self.data_dir
            .as_deref()
            .ok_or_else(|| FatxError::Other("fatx sink not initialized".to_string()))
    }

    fn con_header_path(&self) -> Result<&str> {
        self.con_header_path
            .as_deref()
            .ok_or_else(|| FatxError::Other("fatx sink not initialized".to_string()))
    }

    fn part_path(&self, part_index: u64) -> Result<String> {
        Ok(format!("{}/Data{:04}", self.data_dir()?, part_index))
    }
}

impl<T: Read + Seek + Write> GodSink for FatxSink<'_, T> {
    fn begin(&mut self, source: &PreparedSource) -> Result<()> {
        let title_id_str = format!("{:08X}", source.exe_info.title_id);
        let content_type_str = format!("{:08X}", source.content_type as u32);
        let media_id_str = match source.content_type {
            super::ContentType::GamesOnDemand => format!("{:08X}", source.exe_info.media_id),
            super::ContentType::XboxOriginal => format!("{:08X}", source.exe_info.title_id),
        };
        let dest_root = self.dest_dir.trim_end_matches('/');
        let title_dir = format!("{}/{}", dest_root, title_id_str);
        let content_dir = format!("{}/{}", title_dir, content_type_str);
        let con_header_path = format!("{}/{}", content_dir, media_id_str);
        let data_dir = format!("{}/{}.data", content_dir, media_id_str);

        ensure_fatx_dir(self.vol, &title_dir)?;
        ensure_fatx_dir(self.vol, &content_dir)?;
        ensure_fatx_dir(self.vol, &data_dir)?;
        self.data_dir = Some(data_dir);
        self.con_header_path = Some(con_header_path);
        self.master_lists.clear();
        self.master_lists.reserve(source.report.part_count as usize);
        self.last_part_size = 0;
        Ok(())
    }

    fn write_part<'a>(
        &mut self,
        source: &PreparedSource,
        part_index: u64,
        opts: &mut ConvertOptions<'a>,
    ) -> Result<()> {
        let remaining_bytes = part_payload_bytes(source.report.data_size, part_index);
        let mut iso = source.open_reader()?;
        let (len, master) =
            fill_part_buf(&mut iso, part_index, remaining_bytes, &mut self.part_buf)?;
        let part_path = self.part_path(part_index)?;
        let reader = Cursor::new(&self.part_buf[..len]);

        let mut outer = opts.progress.take();
        let part_idx_now = part_index;
        let part_count_now = source.report.part_count;
        {
            let mut inner = |bytes: u64, total: u64| {
                if let Some(cb) = outer.as_deref_mut() {
                    let stage = format!("part {}/{}", part_idx_now + 1, part_count_now);
                    cb(&stage, bytes, total);
                }
            };
            self.vol
                .create_file_from_reader(&part_path, len as u64, reader, Some(&mut inner))?;
        }
        opts.progress = outer;

        self.master_lists.push(master);
        self.last_part_size = len as u64;
        Ok(())
    }

    fn read_master_hash(&mut self, _source: &PreparedSource, part_index: u64) -> Result<HashList> {
        self.master_lists
            .get(part_index as usize)
            .cloned()
            .ok_or_else(|| FatxError::Other(format!("missing FATX part {}", part_index)))
    }

    fn write_master_hash(
        &mut self,
        _source: &PreparedSource,
        part_index: u64,
        mht: &HashList,
    ) -> Result<()> {
        let slot = self
            .master_lists
            .get_mut(part_index as usize)
            .ok_or_else(|| FatxError::Other(format!("missing FATX part {}", part_index)))?;
        *slot = mht.clone();
        let part_path = self.part_path(part_index)?;
        overwrite_part_master(self.vol, &part_path, mht.bytes())
    }

    fn last_part_size(&self, _source: &PreparedSource) -> Result<u64> {
        Ok(self.last_part_size)
    }

    fn write_con_header(&mut self, _source: &PreparedSource, con_bytes: Vec<u8>) -> Result<()> {
        let con_len = con_bytes.len() as u64;
        let path = self.con_header_path()?.to_string();
        self.vol
            .create_file_from_reader(&path, con_len, Cursor::new(con_bytes), None)
    }

    fn flush_after_parts(&mut self) -> Result<()> {
        let _ = self.vol.flush();
        Ok(())
    }

    fn flush_after_mht(&mut self) -> Result<()> {
        let _ = self.vol.flush();
        Ok(())
    }

    fn flush_after_header(&mut self) -> Result<()> {
        let _ = self.vol.flush();
        Ok(())
    }
}

const MAX_PART_BYTES: usize = 4096 + (SUBPARTS_PER_PART as usize) * (4096 + SUBPART_SIZE as usize);

fn fill_part_buf<R: Read + Seek>(
    data_volume: &mut R,
    part_index: u64,
    remaining_bytes: u64,
    out: &mut [u8],
) -> Result<(usize, HashList)> {
    data_volume
        .seek_relative((part_index * super::BLOCKS_PER_PART * BLOCK_SIZE) as i64)
        .map_err(FatxError::Io)?;

    let mut master = HashList::new();
    let mut cursor = 4096usize;
    let mut subpart_buf = vec![0u8; SUBPART_SIZE as usize];
    let mut bytes_left = remaining_bytes;

    for _ in 0..SUBPARTS_PER_PART {
        if bytes_left == 0 {
            break;
        }
        let want = (subpart_buf.len() as u64).min(bytes_left) as usize;
        let mut got = 0usize;
        while got < want {
            let n = data_volume
                .read(&mut subpart_buf[got..want])
                .map_err(FatxError::Io)?;
            if n == 0 {
                break;
            }
            got += n;
        }
        if got == 0 {
            break;
        }
        let subpart = &subpart_buf[..got];
        let mut sub_hash = HashList::new();
        for block in subpart.chunks(BLOCK_SIZE as usize) {
            sub_hash.add_block_hash(block);
        }
        out[cursor..cursor + 4096].copy_from_slice(sub_hash.bytes());
        cursor += 4096;
        out[cursor..cursor + got].copy_from_slice(subpart);
        cursor += got;
        bytes_left -= got as u64;
        master.add_block_hash(sub_hash.bytes());
        if got < want {
            break;
        }
    }

    out[0..4096].copy_from_slice(master.bytes());
    Ok((cursor, master))
}

fn overwrite_part_master<T>(
    vol: &mut FatxVolume<T>,
    path: &str,
    new_master: &[u8; 4096],
) -> Result<()>
where
    T: Read + Seek + Write,
{
    let entry = vol.resolve_path(path)?;
    let first_cluster = entry.first_cluster;
    let cluster_size = vol.superblock.cluster_size() as usize;
    let mut cluster_buf = vec![0u8; cluster_size];
    vol.read_cluster(first_cluster, &mut cluster_buf)?;
    cluster_buf[..new_master.len()].copy_from_slice(new_master);
    vol.write_cluster(first_cluster, &cluster_buf)?;
    Ok(())
}

fn ensure_fatx_dir<T>(vol: &mut FatxVolume<T>, path: &str) -> Result<()>
where
    T: Read + Seek + Write,
{
    match vol.create_directory(path) {
        Ok(()) => Ok(()),
        Err(FatxError::FileExists(_)) => {
            let existing = vol.resolve_path(path)?;
            if !existing.is_directory() {
                return Err(FatxError::NotADirectory(path.to_string()));
            }
            Ok(())
        }
        Err(e) => Err(e),
    }
}
