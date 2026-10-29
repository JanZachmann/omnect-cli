use anyhow::{Context, Result};
use std::fs::File;
use std::io::Seek;
use std::io::SeekFrom;
use std::path::Path;

pub const SECTOR_SIZE: u64 = 512;

pub struct PartitionData {
    pub num: u32,
    pub start: u64,
    pub count: u64,
}

pub fn get_partition_data<P: AsRef<Path>>(path: P, partition_num: u32) -> Result<PartitionData> {
    let path = path.as_ref();
    let mut file =
        File::open(path).with_context(|| format!("failed to open image: {}", path.display()))?;

    // Try GPT first (validates CRC32 — more robust than signature check).
    // Capture any error so we can attach it as context if MBR also fails.
    let gpt_err = match gptman::GPT::find_from(&mut file) {
        Ok(gpt) => {
            let entry = gpt
                .iter()
                .find(|(i, _)| *i == partition_num)
                .map(|(_, e)| e)
                .with_context(|| format!("GPT partition {partition_num} out of range"))?;
            anyhow::ensure!(entry.is_used(), "GPT partition {partition_num} is not used");
            anyhow::ensure!(
                entry.ending_lba >= entry.starting_lba,
                "GPT partition {partition_num} has invalid LBA range (ending_lba < starting_lba)"
            );
            // the callers count in SECTOR_SIZE units
            let scale = gpt.sector_size / SECTOR_SIZE;
            return Ok(PartitionData {
                num: partition_num,
                start: entry.starting_lba * scale,
                count: (entry.ending_lba - entry.starting_lba + 1) * scale,
            });
        }
        Err(e) => e,
    };

    // Try MBR — iter() includes logical partitions (5, 6, ...)
    file.seek(SeekFrom::Start(0))
        .context("failed to seek to start of image")?;
    let mbr = mbrman::MBR::read_from(&mut file, SECTOR_SIZE as u32)
        .with_context(|| format!("image is neither valid GPT nor MBR (GPT error: {gpt_err})"))?;
    for (i, p) in mbr.iter() {
        if u32::try_from(i).ok() == Some(partition_num) && p.is_used() {
            return Ok(PartitionData {
                num: partition_num,
                start: p.starting_lba as u64,
                count: p.sectors as u64,
            });
        }
    }
    anyhow::bail!("partition {partition_num} not found in image")
}

pub fn is_gpt<P: AsRef<Path>>(path: P) -> Result<bool> {
    let mut file = File::open(path.as_ref()).context("is_gpt: failed to open image")?;
    Ok(gptman::GPT::find_from(&mut file).is_ok())
}

#[cfg(test)]
mod tests {
    use crate::file::partition::*;
    use gptman::{GPT, GPTPartitionEntry};

    const GPT_SECTOR_SIZE: u64 = 4096;
    const DISK_SECTORS: u64 = 64;
    const PART_NUM: u32 = 1;
    const PART_SECTORS: u64 = 8;
    const GUID: [u8; 16] = [0xff; 16];

    #[test]
    fn get_partition_data_converts_4k_gpt_sectors() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("image.wic");
        let mut file = File::create(&path).expect("create image");
        file.set_len(DISK_SECTORS * GPT_SECTOR_SIZE)
            .expect("set image size");
        let mut gpt = GPT::new_from(&mut file, GPT_SECTOR_SIZE, GUID).expect("create gpt");
        let start = gpt.header.first_usable_lba;
        gpt[PART_NUM] = GPTPartitionEntry {
            partition_type_guid: GUID,
            unique_partition_guid: GUID,
            starting_lba: start,
            ending_lba: start + PART_SECTORS - 1,
            attribute_bits: 0,
            partition_name: "part".into(),
        };
        gpt.write_into(&mut file).expect("write gpt");

        let data = get_partition_data(&path, PART_NUM).expect("partition data");
        let scale = GPT_SECTOR_SIZE / SECTOR_SIZE;
        assert_eq!(data.start, start * scale);
        assert_eq!(data.count, PART_SECTORS * scale);
    }
}
