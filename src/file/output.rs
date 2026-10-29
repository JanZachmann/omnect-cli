use crate::file::{bmap, compression::Compression};
use anyhow::{Context, Result};
use std::{
    fs,
    path::{Path, PathBuf},
};

const TMP_DIR_PREFIX: &str = ".omnect-cli-";

/// Returns `explicit`, else `image` if it has an extension of `target`, else `image` without the
/// extension of its `source` compression, with the extension of `target` appended. Fails if
/// `explicit` has no extension of `target`, or a compression extension while `target` is none.
pub fn output_image(
    image: &Path,
    source: Option<&Compression>,
    target: Option<&Compression>,
    explicit: Option<&Path>,
) -> Result<PathBuf> {
    if let Some(path) = explicit {
        match target {
            Some(c) => anyhow::ensure!(
                c.matches_extension(path),
                "{} has no extension of the -p compression; name it e.g. {}",
                path.display(),
                c.compressed_path(path).display()
            ),
            None => anyhow::ensure!(
                !Compression::has_extension(path),
                "{} has a compression extension, but the image is written uncompressed; pass -p",
                path.display()
            ),
        }
        return Ok(path.to_path_buf());
    }
    if target.is_some_and(|c| c.matches_extension(image)) {
        return Ok(image.to_path_buf());
    }

    let mut output = image.to_path_buf();
    if source.is_some_and(|c| c.matches_extension(image)) {
        output.set_extension("");
    }
    Ok(match target {
        Some(c) => c.compressed_path(&output),
        None => output,
    })
}

/// Fails if writing `output_image` and its bmap would overwrite a file the user did not name,
/// replace the bmap of an image that stays, or leave a bmap that `bmaptool` would use for the
/// output image and that does not match it.
pub fn check_conflicts(
    input_image: &Path,
    output_image: &Path,
    explicit_output: bool,
    output_bmap: bool,
    input_bmap: Option<&Path>,
) -> Result<()> {
    let in_place = same_file(input_image, output_image)?;

    anyhow::ensure!(
        explicit_output || in_place || !output_image.exists(),
        "{} exists; choose the output image with --output-image",
        output_image.display()
    );
    if output_bmap {
        let target_bmap = bmap::bmap_path(output_image);
        if !in_place {
            let found_bmap = bmap::find_input_bmap(input_image, None)?;
            for bmap in input_bmap.into_iter().chain(found_bmap.as_deref()) {
                anyhow::ensure!(
                    !same_file(bmap, &target_bmap)?,
                    "{} is the bmap of {} and would be replaced; choose another output image \
                     with --output-image",
                    bmap.display(),
                    input_image.display()
                );
            }
        }
    } else if let Some(stale_bmap) = bmap::find_input_bmap(output_image, None)? {
        anyhow::bail!(
            "{} would not match {}; pass -b to write a matching bmap",
            stale_bmap.display(),
            output_image.display()
        );
    }

    Ok(())
}

/// Writes `dest` as a new file in its directory and renames it, so `dest` stays unchanged if
/// `write` fails. A symlink is followed, and an existing `dest` keeps its permissions.
pub fn replace(dest: &Path, write: impl FnOnce(&Path) -> Result<()>) -> Result<()> {
    let dest = resolve(dest)?;
    let dir = dest
        .parent()
        .context(format!("{} has no directory", dest.display()))?;
    let tmp_dir = tempfile::Builder::new()
        .prefix(TMP_DIR_PREFIX)
        .tempdir_in(dir)
        .context(format!("cannot create a temp dir in {}", dir.display()))?;
    let tmp_file = tmp_dir.path().join(
        dest.file_name()
            .context(format!("{} has no file name", dest.display()))?,
    );

    write(&tmp_file)?;
    if let Ok(metadata) = fs::metadata(&dest) {
        fs::set_permissions(&tmp_file, metadata.permissions())
            .context(format!("cannot set permissions of {}", tmp_file.display()))?;
    }
    fs::rename(&tmp_file, &dest).context(format!(
        "cannot rename {} to {}",
        tmp_file.display(),
        dest.display()
    ))
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
    use std::os::unix::fs::{PermissionsExt, symlink};

    const IMAGE: &str = "image.wic";
    const IMAGE_XZ: &str = "image.wic.xz";
    const OTHER_IMAGE: &str = "device.wic";
    const XZ_LEVEL: u32 = 9;
    const OLD_CONTENT: &[u8] = b"old image";
    const NEW_CONTENT: &[u8] = b"new image";
    const IMAGE_MODE: u32 = 0o644;
    const MODE_MASK: u32 = 0o777;

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
        let output = |image, source, target| {
            output_image(image, source, target, None).expect("output image")
        };

        assert_eq!(output(image, None, None), image);
        assert_eq!(output(image_xz, Some(&xz), None), image);
        assert_eq!(output(image, None, Some(&xz)), image_xz);
        assert_eq!(output(image_xz, Some(&xz), Some(&xz)), image_xz);
    }

    #[test]
    fn output_image_keeps_every_extension_of_the_same_compression() {
        for (image, compression) in [
            ("image.wic.gz", Compression::gzip),
            ("image.wic.gzip", Compression::gzip),
            ("image.wic.bz2", Compression::bzip2),
            ("image.wic.bzip2", Compression::bzip2),
        ] {
            let image = Path::new(image);
            assert_eq!(
                output_image(image, Some(&compression), Some(&compression), None)
                    .expect("output image"),
                image
            );
        }
    }

    #[test]
    fn output_image_strips_only_the_source_compression_extension() {
        let image = Path::new("image.bin");
        let xz = Compression::xz {
            compression_level: XZ_LEVEL,
        };

        assert_eq!(
            output_image(image, Some(&xz), None, None).expect("output image"),
            image
        );
    }

    #[test]
    fn output_image_takes_explicit_path_with_matching_extension() {
        let xz = Compression::xz {
            compression_level: XZ_LEVEL,
        };
        let other_xz = Path::new("device.wic.xz");

        assert_eq!(
            output_image(
                Path::new(IMAGE_XZ),
                Some(&xz),
                None,
                Some(Path::new(OTHER_IMAGE))
            )
            .expect("uncompressed"),
            Path::new(OTHER_IMAGE)
        );
        assert_eq!(
            output_image(Path::new(IMAGE), None, Some(&xz), Some(other_xz)).expect("compressed"),
            other_xz
        );
    }

    #[test]
    fn output_image_rejects_explicit_path_with_wrong_extension() {
        let xz = Compression::xz {
            compression_level: XZ_LEVEL,
        };

        assert!(
            output_image(
                Path::new(IMAGE),
                None,
                Some(&xz),
                Some(Path::new(OTHER_IMAGE))
            )
            .is_err()
        );
        assert!(
            output_image(
                Path::new(IMAGE),
                None,
                Some(&xz),
                Some(Path::new("device.wic.gz"))
            )
            .is_err()
        );
        assert!(
            output_image(
                Path::new(IMAGE_XZ),
                Some(&xz),
                None,
                Some(Path::new("device.wic.xz"))
            )
            .is_err()
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
    fn check_conflicts_rejects_a_stale_bmap_bmaptool_finds() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let image = dir.path().join(IMAGE);
        touch(&image);
        touch(&dir.path().join("image.bmap"));

        assert!(check_conflicts(&image, &image, false, false, None).is_err());
    }

    #[test]
    fn check_conflicts_rejects_a_stale_bmap_of_the_uncompressed_image() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let image_xz = dir.path().join(IMAGE_XZ);
        let image_bmap = bmap::bmap_path(&dir.path().join(IMAGE));
        touch(&image_xz);
        touch(&image_bmap);

        // bmaptool uses image.wic.bmap for image.wic.xz
        assert!(check_conflicts(&image_xz, &image_xz, false, false, None).is_err());
        check_conflicts(&image_xz, &image_xz, false, true, Some(&image_bmap)).expect("with -b");
    }

    #[test]
    fn check_conflicts_rejects_replacing_the_bmap_next_to_the_input_image() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let image_xz = dir.path().join(IMAGE_XZ);
        let image = dir.path().join(IMAGE);
        let other_bmap = dir.path().join("other.bmap");
        touch(&image_xz);
        touch(&bmap::bmap_path(&image));
        touch(&other_bmap);

        // --input-bmap and --no-input-bmap
        assert!(check_conflicts(&image_xz, &image, false, true, Some(&other_bmap)).is_err());
        assert!(check_conflicts(&image_xz, &image, false, true, None).is_err());
    }

    #[test]
    fn replace_keeps_dest_when_write_fails() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let image = dir.path().join(IMAGE);
        fs::write(&image, OLD_CONTENT).expect("write image");

        assert!(
            replace(&image, |tmp| {
                fs::write(tmp, NEW_CONTENT).expect("write tmp");
                anyhow::bail!("write failed")
            })
            .is_err()
        );
        assert_eq!(fs::read(&image).expect("read image"), OLD_CONTENT);
        assert_eq!(
            fs::read_dir(dir.path()).expect("read dir").count(),
            1,
            "temp dir was left"
        );
    }

    #[test]
    fn replace_keeps_permissions_and_symlink() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let image = dir.path().join(IMAGE);
        let link = dir.path().join(OTHER_IMAGE);
        fs::write(&image, OLD_CONTENT).expect("write image");
        fs::set_permissions(&image, fs::Permissions::from_mode(IMAGE_MODE))
            .expect("set permissions");
        symlink(&image, &link).expect("create symlink");

        replace(&link, |tmp| Ok(fs::write(tmp, NEW_CONTENT)?)).expect("replace");

        assert!(link.is_symlink());
        assert_eq!(fs::read(&image).expect("read image"), NEW_CONTENT);
        assert_eq!(
            fs::metadata(&image).expect("metadata").permissions().mode() & MODE_MASK,
            IMAGE_MODE
        );
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
