use crate::file::{COPY_CHUNK_SIZE, IMAGE_BLOCK_SIZE, is_zero};
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::{
    fs,
    ops::RangeInclusive,
    os::unix::fs::FileExt,
    path::{Path, PathBuf},
    process::Command,
};

const BMAP_EXTENSION: &str = "bmap";
const COMPRESSION_EXTENSIONS: [&str; 5] = ["xz", "gz", "gzip", "bz2", "bzip2"];
const SHA256_CHECKSUM_TYPE: &str = "sha256";

/// The block ranges of an image that a bmap file maps.
#[derive(Debug)]
pub struct Bmap {
    pub block_size: u64,
    pub image_size: u64,
    pub ranges: Vec<RangeInclusive<u64>>,
    /// The sha256 of each range, if the bmap has them.
    pub sha256: Option<Vec<String>>,
}

impl Bmap {
    pub fn read(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path)
            .context(format!("Bmap::read: cannot read {}", path.display()))?;
        let doc = roxmltree::Document::parse(&text)
            .context(format!("Bmap::read: invalid xml in {}", path.display()))?;
        let root = doc.root_element();
        let child = |name: &str| {
            root.children()
                .find(|n| n.has_tag_name(name))
                .context(format!("Bmap::read: no {name} in {}", path.display()))
        };
        let number = |name: &str| -> Result<u64> {
            child(name)?
                .text()
                .unwrap_or_default()
                .trim()
                .parse()
                .context(format!("Bmap::read: invalid {name} in {}", path.display()))
        };

        let block_size = number("BlockSize")?;
        anyhow::ensure!(block_size > 0, "Bmap::read: BlockSize is 0");
        let image_size = number("ImageSize")?;
        let range_nodes: Vec<_> = child("BlockMap")?
            .children()
            .filter(|n| n.has_tag_name("Range"))
            .collect();
        let ranges = range_nodes
            .iter()
            .map(|n| parse_range(n.text().unwrap_or_default()))
            .collect::<Result<_>>()?;
        let sha256 = child("ChecksumType")
            .ok()
            .and_then(|n| n.text())
            .is_some_and(|t| t.trim() == SHA256_CHECKSUM_TYPE)
            .then(|| {
                range_nodes
                    .iter()
                    .map(|n| n.attribute("chksum").map(str::to_owned))
                    .collect::<Option<_>>()
            })
            .flatten();

        Ok(Self {
            block_size,
            image_size,
            ranges,
            sha256,
        })
    }

    pub fn whole_image(image_size: u64) -> Self {
        let block_size = IMAGE_BLOCK_SIZE as u64;
        let ranges = match image_size {
            0 => vec![],
            size => vec![RangeInclusive::new(0, (size - 1) / block_size)],
        };

        Self {
            block_size,
            image_size,
            ranges,
            sha256: None,
        }
    }
}

fn parse_range(text: &str) -> Result<RangeInclusive<u64>> {
    let text = text.trim();
    let (first, last) = text.split_once('-').unwrap_or((text, text));
    let parse = |s: &str| {
        s.trim()
            .parse::<u64>()
            .context(format!("parse_range: invalid range {text}"))
    };
    let (first, last) = (parse(first)?, parse(last)?);
    anyhow::ensure!(first <= last, "parse_range: invalid range {text}");

    Ok(first..=last)
}

/// Returns `explicit`, else `<image>.bmap`, else `<image without compression extension>.bmap`,
/// whichever exists first.
pub fn find_input_bmap(image: &Path, explicit: Option<&Path>) -> Result<Option<PathBuf>> {
    if let Some(path) = explicit {
        anyhow::ensure!(
            path.is_file(),
            "find_input_bmap: {} not found",
            path.display()
        );
        return Ok(Some(path.to_path_buf()));
    }

    let with_bmap_extension = |p: &Path| {
        let mut path = p.as_os_str().to_owned();
        path.push(format!(".{BMAP_EXTENSION}"));
        PathBuf::from(path)
    };
    let mut candidates = vec![with_bmap_extension(image)];
    if image
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| COMPRESSION_EXTENSIONS.contains(&e))
    {
        candidates.push(with_bmap_extension(&image.with_extension("")));
    }

    Ok(candidates.into_iter().find(|p| p.is_file()))
}

/// Fails if a range of `image_file` does not match its checksum in `bmap`, so the bmap of
/// another image with the same size is not used.
pub fn verify_checksums(image_file: &Path, bmap: &Bmap) -> Result<()> {
    let sha256 = bmap
        .sha256
        .as_ref()
        .context("verify_checksums: the bmap has no sha256 range checksums")?;
    let image = fs::File::open(image_file).context(format!(
        "verify_checksums: cannot open {}",
        image_file.display()
    ))?;
    let image_len = image
        .metadata()
        .context("verify_checksums: cannot get image size")?
        .len();
    check_layout(bmap, image_len)?;

    let mut buf = vec![0u8; COPY_CHUNK_SIZE];
    for (range, expected) in bmap.ranges.iter().zip(sha256) {
        let end = ((range.end() + 1) * bmap.block_size).min(image_len);
        let mut pos = range.start() * bmap.block_size;
        let mut hasher = Sha256::new();
        while pos < end {
            let n = usize::try_from((end - pos).min(COPY_CHUNK_SIZE as u64))?;
            image
                .read_exact_at(&mut buf[..n], pos)
                .context("verify_checksums: cannot read image")?;
            hasher.update(&buf[..n]);
            pos += n as u64;
        }
        let actual = hex(&hasher.finalize());
        anyhow::ensure!(
            actual.eq_ignore_ascii_case(expected.trim()),
            "verify_checksums: range {range:?} of the image does not match the bmap. It is the \
             bmap of another image, or of this image before a command without -b changed it. \
             Pass the right bmap with --input-bmap."
        );
    }

    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Fails if `bmap` is for an image of another size or maps blocks outside the image.
fn check_layout(bmap: &Bmap, image_len: u64) -> Result<()> {
    anyhow::ensure!(
        bmap.image_size == image_len,
        "check_layout: bmap is for an image of {} bytes, the image has {image_len}",
        bmap.image_size
    );
    let image_blocks = image_len.div_ceil(bmap.block_size);
    if let Some(range) = bmap.ranges.iter().find(|r| *r.end() >= image_blocks) {
        anyhow::bail!("check_layout: range {range:?} is outside the image");
    }

    Ok(())
}

/// Writes the zero blocks in the ranges of `bmap` as data. A zero block can be a hole, and a
/// bmap created from the image would skip it, so a bmap-based flash would keep what an earlier
/// installation left there.
pub fn allocate_ranges(image_file: &Path, bmap: &Bmap) -> Result<()> {
    let image = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(image_file)
        .context(format!(
            "allocate_ranges: cannot open {}",
            image_file.display()
        ))?;
    let image_len = image
        .metadata()
        .context("allocate_ranges: cannot get image size")?
        .len();
    check_layout(bmap, image_len)?;

    let image_block_size = IMAGE_BLOCK_SIZE as u64;
    let mut buf = vec![0u8; COPY_CHUNK_SIZE];
    for range in &bmap.ranges {
        // check whole image blocks, also when the bmap uses a smaller block size
        let start = range.start() * bmap.block_size / image_block_size * image_block_size;
        let end = ((range.end() + 1) * bmap.block_size)
            .next_multiple_of(image_block_size)
            .min(image_len);

        let mut pos = start;
        while pos < end {
            let n = usize::try_from((end - pos).min(COPY_CHUNK_SIZE as u64))?;
            let chunk = &mut buf[..n];
            image
                .read_exact_at(chunk, pos)
                .context("allocate_ranges: cannot read image")?;
            write_zero_blocks(&image, chunk, pos)?;
            pos += n as u64;
        }
    }

    Ok(())
}

fn write_zero_blocks(image: &fs::File, chunk: &[u8], pos: u64) -> Result<()> {
    let blocks: Vec<(u64, &[u8])> = if is_zero(chunk) {
        vec![(pos, chunk)]
    } else {
        chunk
            .chunks(IMAGE_BLOCK_SIZE)
            .enumerate()
            .filter(|(_, block)| is_zero(block))
            .map(|(i, block)| (pos + (i * IMAGE_BLOCK_SIZE) as u64, block))
            .collect()
    };

    blocks.into_iter().try_for_each(|(pos, block)| {
        image
            .write_all_at(block, pos)
            .context("allocate_ranges: cannot write image")
    })
}

pub fn generate_bmap_file(image_file: &str) -> Result<()> {
    let mut bmaptool = Command::new("bmaptool");
    bmaptool
        .arg("create")
        .arg("-o")
        .arg(format!("{image_file}.{BMAP_EXTENSION}"))
        .arg(image_file);
    let status = bmaptool
        .status()
        .context(format!("generate_bmap_file: cannot run {bmaptool:?}"))?;
    anyhow::ensure!(status.success(), "generate_bmap_file: {bmaptool:?} failed");

    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::file::bmap::*;

    const BLOCK_SIZE: u64 = IMAGE_BLOCK_SIZE as u64;
    const IMAGE_BLOCKS: u64 = 16;
    const DATA_BLOCK: u64 = 0;
    const ZERO_BLOCKS: RangeInclusive<u64> = 2..=3;
    const UNMAPPED_BLOCK: u64 = 5;
    const SMALL_BLOCK_SIZE: u64 = 512;
    const PARTIAL_BLOCK_LEN: usize = 100;
    const DATA_BYTE: u8 = 0x5a;
    // content an earlier installation left on the device
    const OLD_CONTENT_BYTE: u8 = 0xa5;
    const YOCTO_BMAP: &str = "testfiles/image.wic.bmap";
    const YOCTO_IMAGE_SIZE: u64 = 30408704;
    const YOCTO_MAPPED_BLOCKS: u64 = 77;

    fn bmap_xml(image_size: u64, block_map: &str) -> String {
        format!(
            "<?xml version=\"1.0\" ?><bmap version=\"2.0\"><ImageSize>{image_size}</ImageSize>\
             <BlockSize>{BLOCK_SIZE}</BlockSize><BlockMap>{block_map}</BlockMap></bmap>"
        )
    }

    fn write_bmap(path: &Path, image_size: u64, ranges: &[RangeInclusive<u64>]) {
        let block_map: String = ranges
            .iter()
            .map(|r| format!("<Range>{}-{}</Range>", r.start(), r.end()))
            .collect();
        fs::write(path, bmap_xml(image_size, &block_map)).expect("write bmap");
    }

    /// A sparse image with data in `DATA_BLOCK` and holes everywhere else.
    fn sparse_image(dir: &Path) -> PathBuf {
        let path = dir.join("image.wic");
        let image = fs::File::create(&path).expect("create image");
        image
            .set_len(IMAGE_BLOCKS * BLOCK_SIZE)
            .expect("set image size");
        image
            .write_all_at(&[DATA_BYTE; IMAGE_BLOCK_SIZE], DATA_BLOCK * BLOCK_SIZE)
            .expect("write data block");
        path
    }

    fn created_bmap(image: &Path) -> Bmap {
        let image = image.to_str().expect("image path");
        generate_bmap_file(image).expect("generate bmap");
        Bmap::read(Path::new(&format!("{image}.{BMAP_EXTENSION}"))).expect("read bmap")
    }

    fn is_mapped(bmap: &Bmap, block: u64) -> bool {
        bmap.ranges.iter().any(|r| r.contains(&block))
    }

    /// Writes the blocks `bmap` maps onto a device that still holds old content, as a bmap
    /// flash does.
    fn bmap_flash(image: &Path, bmap: &Bmap, device: &Path) {
        let image = fs::read(image).expect("read image");
        let mut content = vec![OLD_CONTENT_BYTE; image.len()];
        for range in &bmap.ranges {
            let start = usize::try_from(range.start() * bmap.block_size).expect("start");
            let end = usize::try_from((range.end() + 1) * bmap.block_size).expect("end");
            let end = end.min(image.len());
            content[start..end].copy_from_slice(&image[start..end]);
        }
        fs::write(device, content).expect("write device");
    }

    #[test]
    fn read_parses_yocto_bmap() {
        let bmap = Bmap::read(Path::new(YOCTO_BMAP)).expect("read bmap");

        assert_eq!(bmap.block_size, BLOCK_SIZE);
        assert_eq!(bmap.image_size, YOCTO_IMAGE_SIZE);
        let mapped: u64 = bmap.ranges.iter().map(|r| r.end() - r.start() + 1).sum();
        assert_eq!(mapped, YOCTO_MAPPED_BLOCKS);
        assert_eq!(
            bmap.sha256.map(|c| c.len()),
            Some(bmap.ranges.len()),
            "one checksum per range"
        );
    }

    #[test]
    fn read_parses_single_blocks_and_spans() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("image.wic.bmap");
        let block_map = "<Range chksum=\"0\"> 3 </Range><Range chksum=\"0\"> 5-7 </Range>";
        fs::write(&path, bmap_xml(BLOCK_SIZE, block_map)).expect("write bmap");

        let bmap = Bmap::read(&path).expect("read bmap");
        assert_eq!(bmap.ranges, vec![3..=3, 5..=7]);
    }

    #[test]
    fn read_has_no_checksums_without_sha256_checksum_type() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("image.wic.bmap");
        fs::write(&path, bmap_xml(BLOCK_SIZE, "<Range>0</Range>")).expect("write bmap");

        assert_eq!(Bmap::read(&path).expect("read bmap").sha256, None);
    }

    #[test]
    fn read_rejects_invalid_bmaps() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("image.wic.bmap");
        let invalid = [
            "<bmap><ImageSize>1</ImageSize><BlockMap/></bmap>".to_string(),
            bmap_xml(BLOCK_SIZE, "<Range>7-5</Range>"),
            bmap_xml(BLOCK_SIZE, "<Range>x</Range>"),
            "<bmap>".to_string(),
        ];

        for xml in invalid {
            fs::write(&path, &xml).expect("write bmap");
            assert!(Bmap::read(&path).is_err(), "accepted {xml}");
        }
    }

    #[test]
    fn find_input_bmap_takes_explicit_path() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let image = dir.path().join("image.wic");
        let explicit = dir.path().join("other.bmap");

        assert!(find_input_bmap(&image, Some(&explicit)).is_err());
        fs::write(&explicit, "").expect("write bmap");
        assert_eq!(
            find_input_bmap(&image, Some(&explicit)).expect("find bmap"),
            Some(explicit)
        );
    }

    #[test]
    fn find_input_bmap_looks_next_to_the_image() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let image = dir.path().join("image.wic.xz");
        let uncompressed_bmap = dir.path().join("image.wic.bmap");
        let compressed_bmap = dir.path().join("image.wic.xz.bmap");

        assert_eq!(find_input_bmap(&image, None).expect("find bmap"), None);
        fs::write(&uncompressed_bmap, "").expect("write bmap");
        assert_eq!(
            find_input_bmap(&image, None).expect("find bmap"),
            Some(uncompressed_bmap)
        );
        fs::write(&compressed_bmap, "").expect("write bmap");
        assert_eq!(
            find_input_bmap(&image, None).expect("find bmap"),
            Some(compressed_bmap)
        );
    }

    #[test]
    fn allocate_ranges_maps_zero_ranges_of_the_input_bmap() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let image = sparse_image(dir.path());
        let input_bmap = dir.path().join("input.bmap");
        write_bmap(
            &input_bmap,
            IMAGE_BLOCKS * BLOCK_SIZE,
            &[DATA_BLOCK..=DATA_BLOCK, ZERO_BLOCKS],
        );
        let input = Bmap::read(&input_bmap).expect("read bmap");

        let before = created_bmap(&image);
        assert!(ZERO_BLOCKS.clone().all(|b| !is_mapped(&before, b)));

        allocate_ranges(&image, &input).expect("allocate ranges");
        let after = created_bmap(&image);
        assert!(ZERO_BLOCKS.clone().all(|b| is_mapped(&after, b)));
        assert!(!is_mapped(&after, UNMAPPED_BLOCK));

        let device = dir.path().join("device");
        bmap_flash(&image, &after, &device);
        let device = fs::read(&device).expect("read device");
        let zero_start = usize::try_from(ZERO_BLOCKS.start() * BLOCK_SIZE).expect("start");
        let zero_end = usize::try_from((ZERO_BLOCKS.end() + 1) * BLOCK_SIZE).expect("end");
        assert!(device[zero_start..zero_end].iter().all(|b| *b == 0));
    }

    #[test]
    fn allocate_ranges_maps_whole_image() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let image = sparse_image(dir.path());

        allocate_ranges(&image, &Bmap::whole_image(IMAGE_BLOCKS * BLOCK_SIZE))
            .expect("allocate ranges");
        assert_eq!(created_bmap(&image).ranges, vec![0..=IMAGE_BLOCKS - 1]);
    }

    #[test]
    fn allocate_ranges_maps_whole_blocks_for_a_smaller_bmap_block_size() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let image = sparse_image(dir.path());
        // starts in DATA_BLOCK and ends in the hole after it
        let first = (DATA_BLOCK + 1) * BLOCK_SIZE / SMALL_BLOCK_SIZE - 1;
        let input = Bmap {
            block_size: SMALL_BLOCK_SIZE,
            image_size: IMAGE_BLOCKS * BLOCK_SIZE,
            ranges: vec![RangeInclusive::new(first, first + 2)],
            sha256: None,
        };

        allocate_ranges(&image, &input).expect("allocate ranges");
        assert!(is_mapped(&created_bmap(&image), DATA_BLOCK + 1));
    }

    #[test]
    fn verify_checksums_accepts_bmap_of_the_image() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let image = sparse_image(dir.path());
        // a mapped last block that is shorter than the block size
        fs::OpenOptions::new()
            .write(true)
            .open(&image)
            .expect("open image")
            .write_all_at(&[DATA_BYTE; PARTIAL_BLOCK_LEN], IMAGE_BLOCKS * BLOCK_SIZE)
            .expect("write partial block");

        verify_checksums(&image, &created_bmap(&image)).expect("verify checksums");
    }

    #[test]
    fn verify_checksums_rejects_changed_image() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let image = sparse_image(dir.path());
        let bmap = created_bmap(&image);
        fs::OpenOptions::new()
            .write(true)
            .open(&image)
            .expect("open image")
            .write_all_at(&[!DATA_BYTE], DATA_BLOCK * BLOCK_SIZE)
            .expect("change data block");

        let err = verify_checksums(&image, &bmap).expect_err("changed image accepted");
        assert!(err.to_string().contains("--input-bmap"), "{err}");
    }

    #[test]
    fn verify_checksums_rejects_bmap_without_checksums() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let image = sparse_image(dir.path());
        let mut bmap = created_bmap(&image);
        bmap.sha256 = None;

        assert!(verify_checksums(&image, &bmap).is_err());
    }

    #[test]
    fn allocate_ranges_rejects_bmap_of_another_image() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let image = sparse_image(dir.path());

        let other_size = Bmap::whole_image((IMAGE_BLOCKS + 1) * BLOCK_SIZE);
        assert!(allocate_ranges(&image, &other_size).is_err());

        let outside = Bmap {
            block_size: BLOCK_SIZE,
            image_size: IMAGE_BLOCKS * BLOCK_SIZE,
            ranges: vec![RangeInclusive::new(IMAGE_BLOCKS, IMAGE_BLOCKS)],
            sha256: None,
        };
        assert!(allocate_ranges(&image, &outside).is_err());
    }
}
