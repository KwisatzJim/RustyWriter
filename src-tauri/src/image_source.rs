use anyhow::{Context, Result};
use flate2::read::GzDecoder;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use xz2::read::XzDecoder;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImageFormat {
    Raw,
    Gzip,
    Xz,
    Zip,
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
pub fn stage_image(source_path: &Path, mut on_progress: impl FnMut(u64, Option<u64>)) -> Result<StagedImage> {
    let format = detect_format(source_path)?;
    let dest_path = crate::shared_temp_dir().join(format!("rustywriter-stage-{}.img", uuid::Uuid::new_v4()));
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
                let n = reader.read(&mut buf).context("reading source image")?;
                if n == 0 {
                    break;
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
            let entry_index = (0..archive.len())
                .find(|&i| archive.by_index(i).map(|f| !f.is_dir()).unwrap_or(false))
                .context("zip archive contains no files")?;
            let total = archive.by_index(entry_index)?.size();
            copy_all!(archive.by_index(entry_index)?, Some(total));
        }
    }

    dest.flush().context("flushing staging file")?;
    Ok(StagedImage {
        path: dest_path,
    })
}
