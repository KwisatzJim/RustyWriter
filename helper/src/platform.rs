use anyhow::{Context, Result};
use std::path::Path;
use std::process::Command;

/// Unmount every filesystem living on this device so the write isn't
/// fighting the OS (or getting silently rejected) partway through.
/// This must fail closed: writing while even one target partition is
/// mounted can corrupt both the image and the mounted filesystem.
#[cfg(target_os = "macos")]
pub fn unmount_device(device: &Path) -> Result<()> {
    let output = Command::new("diskutil")
        .arg("unmountDisk")
        .arg(device)
        .output()
        .context("running diskutil unmountDisk")?;

    let mounted = macos_mounted_partitions(device)?;
    if !mounted.is_empty() {
        let detail = String::from_utf8_lossy(if output.stderr.is_empty() {
            &output.stdout
        } else {
            &output.stderr
        });
        anyhow::bail!(
            "could not safely unmount {}. Still mounted: {}. diskutil said: {}",
            device.display(),
            mounted.join(", "),
            detail.trim()
        );
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn macos_mounted_partitions(device: &Path) -> Result<Vec<String>> {
    let name = device
        .file_name()
        .and_then(|name| name.to_str())
        .context("invalid macOS device path")?;
    let disk_name = name.strip_prefix('r').unwrap_or(name);
    let device_prefix = format!("/dev/{disk_name}");

    let output = Command::new("mount")
        .output()
        .context("checking mounted filesystems after diskutil unmountDisk")?;
    if !output.status.success() {
        anyhow::bail!("could not verify that the target is unmounted");
    }

    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.split_once(" on ").map(|(source, _)| source))
        .filter(|source| {
            *source == device_prefix
                || source
                    .strip_prefix(&device_prefix)
                    .is_some_and(|suffix| suffix.starts_with('s'))
        })
        .map(str::to_string)
        .collect())
}

#[cfg(target_os = "macos")]
pub fn eject_device(device: &Path) -> Result<()> {
    let output = Command::new("diskutil")
        .arg("eject")
        .arg(device)
        .output()
        .context("running diskutil eject")?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(if output.stderr.is_empty() {
            &output.stdout
        } else {
            &output.stderr
        });
        anyhow::bail!("diskutil eject reported: {}", detail.trim());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub fn unmount_device(device: &Path) -> Result<()> {
    let dev_name = device
        .file_name()
        .and_then(|n| n.to_str())
        .context("invalid device path")?;

    let block_dir = Path::new("/sys/block").join(dev_name);
    let mut device_nodes = vec![device.to_path_buf()];
    if let Ok(entries) = std::fs::read_dir(&block_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with(dev_name) && name != dev_name {
                device_nodes.push(Path::new("/dev").join(name.as_ref()));
            }
        }
    }

    let mounted_before = linux_mounted_device_nodes(&device_nodes)?;
    for node in &mounted_before {
        // The final mount-table check below is authoritative. This
        // command may report failure if an automounter raced us, but
        // writing is allowed only if the subsequent check is clean.
        let _ = Command::new("umount").arg(node).status();
    }

    let mounted_after = linux_mounted_device_nodes(&device_nodes)?;
    if !mounted_after.is_empty() {
        anyhow::bail!(
            "could not safely unmount {}. Still mounted: {}",
            device.display(),
            mounted_after
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn linux_mounted_device_nodes(device_nodes: &[std::path::PathBuf]) -> Result<Vec<std::path::PathBuf>> {
    use std::collections::HashSet;
    use std::os::linux::fs::MetadataExt;

    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo")
        .context("reading /proc/self/mountinfo to verify that the target is unmounted")?;
    let mounted_numbers: HashSet<(u64, u64)> = mountinfo
        .lines()
        .filter_map(|line| line.split_whitespace().nth(2))
        .filter_map(|numbers| numbers.split_once(':'))
        .filter_map(|(major, minor)| Some((major.parse().ok()?, minor.parse().ok()?)))
        .collect();

    Ok(device_nodes
        .iter()
        .filter(|node| {
            std::fs::metadata(node)
                .map(|metadata| {
                    let device_number = metadata.st_rdev();
                    mounted_numbers.contains(&(
                        libc::major(device_number) as u64,
                        libc::minor(device_number) as u64,
                    ))
                })
                .unwrap_or(false)
        })
        .cloned()
        .collect())
}

#[cfg(target_os = "linux")]
pub fn eject_device(_device: &Path) -> Result<()> {
    // No universal equivalent of diskutil eject on Linux; a sync in
    // the caller after the write is what actually matters for safety.
    Ok(())
}
