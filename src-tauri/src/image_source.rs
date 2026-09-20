use anyhow::{Context, Result};
use flate2::read::GzDecoder;
use std::fs::{self, File};
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};
use xz2::read::XzDecoder;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImageFormat {
    Raw,
    Gzip,
    Xz,
    Zip,
}

fn select_zip_entry<R: Read + Seek>(archive: &mut zip::ZipArchive<R>) -> Result<usize> {
    let mut files = Vec::new();
    for index in 0..archive.len() {
        let entry = archive.by_index(index).context("reading zip entry")?;
        if !entry.is_dir() {
            files.push((index, entry.name().to_string(), entry.size()));
        }
    }

    if files.is_empty() {
        anyhow::bail!("zip archive contains no files");
    }
    if files.len() == 1 {
        if files[0].2 == 0 {
            anyhow::bail!("the only file in the zip archive is empty");
        }
        return Ok(files[0].0);
    }

    let image_entries: Vec<_> = files
        .iter()
        .filter(|(_, name, size)| {
            let name = name.to_ascii_lowercase();
            *size > 0 && (name.ends_with(".img") || name.ends_with(".iso") || name.ends_with(".raw"))
        })
        .collect();
    if image_entries.len() == 1 {
        return Ok(image_entries[0].0);
    }

    let names = files
        .iter()
        .take(8)
        .map(|(_, name, _)| name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    anyhow::bail!(
        "zip archive does not contain one unambiguous disk image (.img, .iso, or .raw). Files found: {names}"
    )
}

fn detect_format(path: &Path) -> Result<ImageFormat> {
    let mut f = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut magic = [0u8; 6];
    let n = f.read(&mut magic).context("reading image header")?;
    let magic = &magic[..n];

    if magic.starts_with(&[0x1f, 0x8b]) {
        Ok(ImageFormat::Gzip)
    } else if magic.starts_with(&[0xFD, b'7', b'z', b'X', b'Z', 0x00]) {
        Ok(ImageFormat::Xz)
    } else if magic.starts_with(b"PK\x03\x04") || magic.starts_with(b"PK\x05\x06") {
        Ok(ImageFormat::Zip)
    } else {
        Ok(ImageFormat::Raw)
    }
}

pub struct StagedImage {
    pub path: PathBuf,
    _cleanup: TempFileCleanup,
}

#[derive(Clone, Copy)]
pub struct StageLimits {
    pub target_bytes: u64,
    pub temporary_bytes: u64,
}

#[cfg(unix)]
pub fn available_staging_bytes() -> Result<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let temp_dir = crate::shared_temp_dir();
    let path = CString::new(temp_dir.as_os_str().as_bytes()).context("invalid temporary path")?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    let result = unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) };
    if result != 0 {
        return Err(std::io::Error::last_os_error()).context("checking free temporary-disk space");
    }
    let stats = unsafe { stats.assume_init() };
    let available = (stats.f_bavail as u64).saturating_mul(stats.f_frsize);
    // Do not consume the final 256 MiB of the system's temporary
    // volume. Leaving a reserve prevents staging from destabilizing
    // the OS and gives cleanup enough room to complete normally.
    Ok(available.saturating_sub(256 * 1024 * 1024))
}

struct TempFileCleanup {
    path: PathBuf,
}

impl Drop for TempFileCleanup {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Reads the user-picked image - wherever it lives, including
/// Downloads/Desktop/Documents - decompressing it if needed, into a
/// scratch file under the system temp dir.
///
/// This deliberately runs in the *unprivileged* app process. The
/// native file-open dialog grants this process (and only this
/// process) a one-time permission to read the exact file the person
/// picked; a privileged helper launched moments later via
/// pkexec/osascript is a different process and was never granted
/// anything, elevated or not - macOS's protected-folders permission
/// model is keyed to the executable, not the user ID. Staging the
/// image here, into `/tmp` (which isn't one of the protected
/// folders), means the helper never has to touch the original path
/// at all.
pub fn stage_image(
    source_path: &Path,
    limits: StageLimits,
    mut should_cancel: impl FnMut() -> bool,
    mut on_progress: impl FnMut(u64, Option<u64>),
) -> Result<StagedImage> {
    let dest_path = crate::shared_temp_dir().join(format!("rustywriter-stage-{}.img", uuid::Uuid::new_v4()));
    stage_image_to(
        source_path,
        dest_path,
        limits,
        &mut should_cancel,
        &mut on_progress,
    )
}

fn stage_image_to(
    source_path: &Path,
    dest_path: PathBuf,
    limits: StageLimits,
    mut should_cancel: impl FnMut() -> bool,
    mut on_progress: impl FnMut(u64, Option<u64>),
) -> Result<StagedImage> {
    let format = detect_format(source_path)?;
    // Create the cleanup guard before opening or decoding anything.
    // From this point onward, every return path removes a partial file.
    let cleanup = TempFileCleanup {
        path: dest_path.clone(),
    };
    let mut dest = File::create(&dest_path).context("creating staging file")?;

    // Decompressed size isn't knowable up front for streaming
    // gzip/xz, so total is only a hint - the caller shows an
    // indeterminate progress state when it's None.
    let total_hint = match format {
        ImageFormat::Raw => fs::metadata(source_path).ok().map(|m| m.len()),
        ImageFormat::Gzip | ImageFormat::Xz => None,
        ImageFormat::Zip => None, // resolved from the entry itself below
    };

    let mut written: u64 = 0;
    let mut buf = vec![0u8; 4 * 1024 * 1024];

    macro_rules! copy_all {
        ($reader:expr, $total:expr) => {{
            let mut reader = $reader;
            loop {
                if should_cancel() {
                    anyhow::bail!("operation cancelled by the user; no data was written");
                }
                let n = reader.read(&mut buf).context("reading source image")?;
                if n == 0 {
                    break;
                }
                let next_size = written.saturating_add(n as u64);
                if next_size > limits.target_bytes {
                    anyhow::bail!(
                        "the decompressed image is larger than the selected drive ({} bytes)",
                        limits.target_bytes
                    );
                }
                if next_size > limits.temporary_bytes {
                    anyhow::bail!(
                        "not enough temporary-disk space to stage this image safely; {} bytes are available after reserving 256 MiB for the system",
                        limits.temporary_bytes
                    );
                }
                dest.write_all(&buf[..n]).context("writing staging file")?;
                written += n as u64;
                on_progress(written, $total);
            }
        }};
    }

    match format {
        ImageFormat::Raw => copy_all!(File::open(source_path)?, total_hint),
        ImageFormat::Gzip => copy_all!(GzDecoder::new(File::open(source_path)?), total_hint),
        ImageFormat::Xz => copy_all!(XzDecoder::new(File::open(source_path)?), total_hint),
        ImageFormat::Zip => {
            let file = File::open(source_path)?;
            let mut archive = zip::ZipArchive::new(file).context("reading zip archive")?;
            let entry_index = select_zip_entry(&mut archive)?;
            let total = archive.by_index(entry_index)?.size();
            copy_all!(archive.by_index(entry_index)?, Some(total));
        }
    }

    dest.flush().context("flushing staging file")?;
    Ok(StagedImage {
        path: dest_path,
        _cleanup: cleanup,
    })
}

#[cfg(test)]
mod tests {
    use super::{select_zip_entry, stage_image_to, StageLimits};
    use std::fs;
    use std::io::{Cursor, Write};

    fn zip_with_files(files: &[(&str, &[u8])]) -> zip::ZipArchive<Cursor<Vec<u8>>> {
        let cursor = Cursor::new(Vec::new());
        let mut writer = zip::ZipWriter::new(cursor);
        let options = zip::write::FileOptions::default();
        for (name, contents) in files {
            writer.start_file(*name, options).unwrap();
            writer.write_all(contents).unwrap();
        }
        zip::ZipArchive::new(writer.finish().unwrap()).unwrap()
    }

    #[test]
    fn selects_the_only_image_among_zip_documentation() {
        let mut archive = zip_with_files(&[("README.txt", b"help"), ("system.img", b"image")]);
        let selected = select_zip_entry(&mut archive).unwrap();
        assert_eq!(archive.by_index(selected).unwrap().name(), "system.img");
    }

    #[test]
    fn rejects_a_zip_with_multiple_possible_images() {
        let mut archive = zip_with_files(&[("first.img", b"one"), ("second.iso", b"two")]);
        let error = select_zip_entry(&mut archive).unwrap_err();
        assert!(error.to_string().contains("unambiguous"));
    }

    #[test]
    fn removes_partial_staging_file_when_decompression_fails() {
        let id = uuid::Uuid::new_v4();
        let source = crate::shared_temp_dir().join(format!("rustywriter-test-{id}.gz"));
        let destination = crate::shared_temp_dir().join(format!("rustywriter-test-{id}.img"));
        fs::write(&source, [0x1f, 0x8b, 0x00]).unwrap();

        let result = stage_image_to(
            &source,
            destination.clone(),
            StageLimits {
                target_bytes: u64::MAX,
                temporary_bytes: u64::MAX,
            },
            || false,
            |_, _| {},
        );

        let _ = fs::remove_file(source);
        assert!(result.is_err());
        assert!(!destination.exists());
    }

    #[test]
    fn stops_before_a_staged_image_exceeds_its_limit() {
        let id = uuid::Uuid::new_v4();
        let source = crate::shared_temp_dir().join(format!("rustywriter-test-{id}.img"));
        let destination = crate::shared_temp_dir().join(format!("rustywriter-test-{id}-out.img"));
        fs::write(&source, b"too large").unwrap();

        let result = stage_image_to(
            &source,
            destination.clone(),
            StageLimits {
                target_bytes: 4,
                temporary_bytes: u64::MAX,
            },
            || false,
            |_, _| {},
        );

        let _ = fs::remove_file(source);
        assert!(result.err().unwrap().to_string().contains("selected drive"));
        assert!(!destination.exists());
    }

    #[test]
    fn cancellation_removes_the_partial_staging_file() {
        let id = uuid::Uuid::new_v4();
        let source = crate::shared_temp_dir().join(format!("rustywriter-test-{id}.img"));
        let destination = crate::shared_temp_dir().join(format!("rustywriter-test-{id}-out.img"));
        fs::write(&source, b"image data").unwrap();

        let result = stage_image_to(
            &source,
            destination.clone(),
            StageLimits {
                target_bytes: u64::MAX,
                temporary_bytes: u64::MAX,
            },
            || true,
            |_, _| {},
        );

        let _ = fs::remove_file(source);
        assert!(result.err().unwrap().to_string().contains("cancelled"));
        assert!(!destination.exists());
    }
}
