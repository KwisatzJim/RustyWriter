mod platform;
mod progress;

use anyhow::{Context, Result};
use clap::Parser;
use progress::{Progress, ProgressWriter};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::Read;
use std::io::Write;
use std::path::PathBuf;

const CHUNK_SIZE: usize = 4 * 1024 * 1024; // 4 MiB
const PROGRESS_EVERY_BYTES: u64 = 16 * 1024 * 1024; // don't spam the UI

/// RustyWriter privileged worker. Not intended to be run directly -
/// the RustyWriter app launches this via pkexec/osascript.
///
/// By design, this binary only ever touches: the given `--image` path
/// (always a plain, already-decompressed scratch file the app staged
/// under the system temp dir - never the original file the user
/// picked), the target device, and the progress file. Keeping the
/// privileged surface this small is deliberate.
#[derive(Parser)]
#[command(name = "rustywriter-helper")]
struct Args {
    /// Path to a plain (already decompressed) image file.
    #[arg(long)]
    image: PathBuf,

    /// Path to the target device. On macOS this should be the raw
    /// device (/dev/rdiskN) for reasonable write speed; on Linux the
    /// block device (/dev/sdX).
    #[arg(long)]
    device: PathBuf,

    /// Re-read the device after writing and compare a hash against
    /// the source image.
    #[arg(long, default_value_t = false)]
    verify: bool,

    /// Newline-delimited JSON progress gets appended here. The caller
    /// (the Tauri app) tails this file.
    #[arg(long)]
    progress_file: PathBuf,
}

fn main() {
    let args = Args::parse();
    let mut progress = match ProgressWriter::open(&args.progress_file) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("could not open progress file: {e}");
            std::process::exit(1);
        }
    };

    if let Err(e) = run(&args, &mut progress) {
        progress.emit(&Progress::Error {
            message: format!("{e:#}"),
        });
        std::process::exit(1);
    }
}

fn run(args: &Args, progress: &mut ProgressWriter) -> Result<()> {
    if !args.image.exists() {
        anyhow::bail!("staged image file is missing: {}", args.image.display());
    }
    if !args.device.exists() {
        anyhow::bail!("device does not exist: {}", args.device.display());
    }

    progress.emit(&Progress::Starting);

    progress.emit(&Progress::Unmounting);
    platform::unmount_device(&args.device).context("unmounting target device")?;

    let total_bytes = std::fs::metadata(&args.image)
        .context("reading staged image metadata")?
        .len();
    let mut source = File::open(&args.image).context("opening staged image")?;

    let mut device_file = File::options().write(true).open(&args.device).map_err(|e| {
        #[cfg(target_os = "macos")]
        if e.raw_os_error() == Some(1) {
            // EPERM opening a raw disk device on macOS almost always
            // means Full Disk Access hasn't been granted to this
            // binary - root privileges don't bypass this particular
            // protection. See README.md for the exact steps.
            return anyhow::anyhow!(
                "macOS blocked raw access to {} (Operation not permitted). This binary needs \
                 Full Disk Access: System Settings -> Privacy & Security -> Full Disk Access -> \
                 add this exact binary, then try again. Running as root doesn't bypass this.",
                args.device.display()
            );
        }
        anyhow::Error::from(e).context(format!(
            "opening device {} - is it write-protected, already unplugged, or still busy from unmounting?",
            args.device.display()
        ))
    })?;

    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; CHUNK_SIZE];
    let mut bytes_written: u64 = 0;
    let mut last_reported: u64 = 0;

    loop {
        let n = source.read(&mut buf).context("reading from staged image")?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        device_file
            .write_all(&buf[..n])
            .context("writing to device - is it write-protected or already unplugged?")?;
        bytes_written += n as u64;

        if bytes_written - last_reported >= PROGRESS_EVERY_BYTES {
            last_reported = bytes_written;
            progress.emit(&Progress::Flashing {
                bytes_written,
                total_bytes,
            });
        }
    }
    // Final progress line so the UI lands exactly on 100%, not
    // wherever the last periodic tick happened to land.
    progress.emit(&Progress::Flashing {
        bytes_written,
        total_bytes,
    });

    device_file.flush().context("flushing writes to device")?;
    if let Err(e) = device_file.sync_all() {
        // macOS's sync_all() uses F_FULLFSYNC, which raw disk device
        // nodes (/dev/rdiskN) don't support - it fails with ENOTTY
        // ("Inappropriate ioctl for device") even though the write
        // itself already succeeded (raw devices are unbuffered at
        // the OS level, unlike regular files). Fall back to a plain
        // fsync, and if even that isn't supported, proceed anyway -
        // we're already past the point where the data matters.
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            let rc = unsafe { libc::fsync(device_file.as_raw_fd()) };
            if rc != 0 {
                eprintln!("warning: could not sync device after writing, continuing anyway: {e}");
            }
        }
        #[cfg(not(unix))]
        {
            eprintln!("warning: could not sync device after writing, continuing anyway: {e}");
        }
    }
    drop(device_file);

    let source_hash = hasher.finalize();
    let mut verified = false;

    if args.verify {
        let mut device_read = File::open(&args.device).context("reopening device for verification")?;
        let mut verify_hasher = Sha256::new();
        let mut remaining = bytes_written;
        let mut verified_bytes: u64 = 0;
        let mut last_reported_verify: u64 = 0;

        while remaining > 0 {
            let want = remaining.min(CHUNK_SIZE as u64) as usize;
            let n = device_read
                .read(&mut buf[..want])
                .context("reading back from device during verification")?;
            if n == 0 {
                anyhow::bail!(
                    "device returned less data than was written ({} bytes short)",
                    remaining
                );
            }
            verify_hasher.update(&buf[..n]);
            verified_bytes += n as u64;
            remaining -= n as u64;

            if verified_bytes - last_reported_verify >= PROGRESS_EVERY_BYTES {
                last_reported_verify = verified_bytes;
                progress.emit(&Progress::Verifying {
                    bytes_verified: verified_bytes,
                    total_bytes: bytes_written,
                });
            }
        }
        progress.emit(&Progress::Verifying {
            bytes_verified: verified_bytes,
            total_bytes: bytes_written,
        });

        if verify_hasher.finalize() != source_hash {
            anyhow::bail!(
                "verification failed: the data on the device doesn't match the source image"
            );
        }
        verified = true;
    }

    progress.emit(&Progress::Ejecting);
    platform::eject_device(&args.device).ok();

    progress.emit(&Progress::Done {
        success: true,
        verified,
    });
    Ok(())
}
