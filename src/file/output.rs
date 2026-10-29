use crate::file::{bmap, compression::Compression};
use anyhow::{Context, Result};
use std::{
    fs,
    path::{Path, PathBuf},
};

/// Returns `explicit`, else `image` without its last extension if it is compressed, with the
/// extension of `target` appended.
pub fn output_image(
    image: &Path,
    source_compressed: bool,
    target: Option<&Compression>,
    explicit: Option<&Path>,
) -> PathBuf {
    if let Some(path) = explicit {
        return path.to_path_buf();
    }

    let mut output = image.to_path_buf();
    if source_compressed {
        output.set_extension("");
    }
    match target {
        Some(c) => c.compressed_path(&output),
        None => output,
    }
}

/// Fails if writing `output_image` and its bmap would overwrite a file the user did not name,
/// replace the bmap of an image that stays, or leave a bmap that no longer matches the image.
pub fn check_conflicts(
    input_image: &Path,
    output_image: &Path,
    explicit_output: bool,
    output_bmap: bool,
    input_bmap: Option<&Path>,
) -> Result<()> {
    let in_place = same_file(input_image, output_image)?;
    let target_bmap = bmap::bmap_path(output_image);

    anyhow::ensure!(
        explicit_output || in_place || !output_image.exists(),
        "{} exists; choose the output image with --output-image",
        output_image.display()
    );
    if output_bmap {
        if let Some(input_bmap) = input_bmap {
            anyhow::ensure!(
                in_place || !same_file(input_bmap, &target_bmap)?,
                "{} is the bmap of {} and would be replaced; choose the output image with \
                 --output-image",
                input_bmap.display(),
                input_image.display()
            );
        }
    } else {
        anyhow::ensure!(
            !target_bmap.exists(),
            "{} would no longer match {}; pass -b to update it, or remove it",
            target_bmap.display(),
            output_image.display()
        );
    }

    Ok(())
}

fn same_file(a: &Path, b: &Path) -> Result<bool> {
    Ok(resolve(a)? == resolve(b)?)
}

/// The absolute path of `path` with symlinks resolved. A file that does not exist yet is
/// resolved through its directory.
fn resolve(path: &Path) -> Result<PathBuf> {
    if path.exists() {
        return fs::canonicalize(path).context(format!("cannot resolve {}", path.display()));
    }
    let dir = match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    };
    let name = path
        .file_name()
        .context(format!("{} has no file name", path.display()))?;

    Ok(fs::canonicalize(dir)
        .context(format!("cannot resolve {}", dir.display()))?
        .join(name))
}

#[cfg(test)]
mod tests {
    use crate::file::output::*;

    const IMAGE: &str = "image.wic";
    const IMAGE_XZ: &str = "image.wic.xz";
    const OTHER_IMAGE: &str = "device.wic";
    const XZ_LEVEL: u32 = 9;

    fn touch(path: &Path) {
        fs::write(path, []).expect("create file");
    }

    #[test]
    fn output_image_follows_compression() {
        let image = Path::new(IMAGE);
        let image_xz = Path::new(IMAGE_XZ);
        let xz = Compression::xz {
            compression_level: XZ_LEVEL,
        };

        assert_eq!(output_image(image, false, None, None), image);
        assert_eq!(output_image(image_xz, true, None, None), image);
        assert_eq!(output_image(image, false, Some(&xz), None), image_xz);
        assert_eq!(output_image(image_xz, true, Some(&xz), None), image_xz);
        assert_eq!(
            output_image(image_xz, true, Some(&xz), Some(Path::new(OTHER_IMAGE))),
            Path::new(OTHER_IMAGE)
        );
    }

    #[test]
    fn check_conflicts_allows_in_place_with_bmap() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let image = dir.path().join(IMAGE);
        let image_bmap = bmap::bmap_path(&image);
        touch(&image);
        touch(&image_bmap);

        check_conflicts(&image, &image, false, true, Some(&image_bmap)).expect("no conflict");
    }

    #[test]
    fn check_conflicts_rejects_existing_derived_image() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let image_xz = dir.path().join(IMAGE_XZ);
        let image = dir.path().join(IMAGE);
        touch(&image_xz);
        touch(&image);

        assert!(check_conflicts(&image_xz, &image, false, false, None).is_err());
        check_conflicts(&image_xz, &image, true, false, None).expect("explicit output");
    }

    #[test]
    fn check_conflicts_rejects_replacing_the_input_bmap() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let image_xz = dir.path().join(IMAGE_XZ);
        let image = dir.path().join(IMAGE);
        let image_bmap = bmap::bmap_path(&image);
        touch(&image_xz);
        touch(&image_bmap);

        assert!(check_conflicts(&image_xz, &image, false, true, Some(&image_bmap)).is_err());
        let other = dir.path().join(OTHER_IMAGE);
        check_conflicts(&image_xz, &other, true, true, Some(&image_bmap)).expect("other output");
    }

    #[test]
    fn check_conflicts_rejects_leaving_a_stale_bmap() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let image = dir.path().join(IMAGE);
        touch(&image);
        touch(&bmap::bmap_path(&image));

        assert!(check_conflicts(&image, &image, false, false, None).is_err());
    }

    #[test]
    fn same_file_resolves_relative_and_missing_paths() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let image = dir.path().join(IMAGE);
        touch(&image);
        let relative = dir.path().join(".").join(IMAGE);
        let missing = dir.path().join(OTHER_IMAGE);

        assert!(same_file(&image, &relative).expect("same file"));
        assert!(same_file(&missing, &missing).expect("same file"));
        assert!(!same_file(&image, &missing).expect("same file"));
    }
}
