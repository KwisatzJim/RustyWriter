use anyhow::{Context, Result};
use std::path::Path;
use std::process::Command;

/// Unmount every filesystem living on this device so the write isn't
/// fighting the OS (or getting silently rejected) partway through.
/// Best-effort: a partition that was never mounted isn't an error.
#[cfg(target_os = "macos")]
pub fn unmount_device(device: &Path) -> Result<()> {
    // Deliberately best-effort: `diskutil unmountDisk` is known to
    // sometimes exit nonzero even when it printed a success message
    // and the unmount genuinely worked (background daemons like
    // Spotlight or Time Machine touching the disk right after,
    // hidden EFI/recovery volumes, etc). Bailing here on exit status
    // alone produces false failures. If the disk is truly still
    // busy, opening it for writing a few lines down will fail with
    // a clear "resource busy" error - that's the real signal.
    let output = Command::new("diskutil")
        .arg("unmountDisk")
        .arg(device)
        .output()
        .context("running diskutil unmountDisk")?;
    if !output.status.success() {
        eprintln!(
            "diskutil unmountDisk reported a non-zero exit for {} (continuing anyway): {}",
            device.display(),
            String::from_utf8_lossy(&output.stdout).trim()
        );
    }
    Ok(())
}

#[cfg(target_os = "macos")]
pub fn eject_device(device: &Path) -> Result<()> {
    // Best-effort - a failed eject shouldn't turn a successful flash
    // into a reported failure.
    let _ = Command::new("diskutil").arg("eject").arg(device).status();
    Ok(())
}

#[cfg(target_os = "linux")]
pub fn unmount_device(device: &Path) -> Result<()> {
    let dev_name = device
        .file_name()
        .and_then(|n| n.to_str())
        .context("invalid device path")?;

    let block_dir = Path::new("/sys/block").join(dev_name);
    if let Ok(entries) = std::fs::read_dir(&block_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with(dev_name) && name != dev_name {
                let part_path = format!("/dev/{}", name);
                // Ignore failures: it's fine if a partition wasn't mounted.
                let _ = Command::new("umount").arg(&part_path).status();
            }
        }
    }
    // The whole disk itself is occasionally mounted directly too.
    let _ = Command::new("umount").arg(device).status();
    Ok(())
}

#[cfg(target_os = "linux")]
pub fn eject_device(_device: &Path) -> Result<()> {
    // No universal equivalent of diskutil eject on Linux; a sync in
    // the caller after the write is what actually matters for safety.
    Ok(())
}
