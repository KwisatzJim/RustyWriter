use serde::Serialize;
#[cfg(target_os = "macos")]
use std::process::Command;

#[derive(Serialize, Clone, Debug)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
    /// Path to hand to the helper for writing. On macOS this is the
    /// *raw* device (/dev/rdiskN) — writing through the raw device
    /// instead of the buffered one is the difference between minutes
    /// and tens of minutes for a multi-GB image.
    pub write_path: String,
    pub size_bytes: u64,
}

// ---------------------------------------------------------------- macOS --

#[cfg(target_os = "macos")]
pub fn list_devices() -> anyhow::Result<Vec<DeviceInfo>> {
    let output = Command::new("diskutil").args(["list", "-plist"]).output()?;
    let plist_val: plist::Value = plist::from_bytes(&output.stdout)?;

    let whole_disks = plist_val
        .as_dictionary()
        .and_then(|d| d.get("WholeDisks"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let mut devices = Vec::new();
    for disk in whole_disks {
        if let Some(id) = disk.as_string() {
            match disk_info(id) {
                Ok(Some(info)) => devices.push(info),
                Ok(None) => {} // internal disk - deliberately excluded
                Err(_) => {}   // couldn't read info for this disk - skip it
            }
        }
    }
    Ok(devices)
}

#[cfg(target_os = "macos")]
fn disk_info(id: &str) -> anyhow::Result<Option<DeviceInfo>> {
    let output = Command::new("diskutil").args(["info", "-plist", id]).output()?;
    let plist_val: plist::Value = plist::from_bytes(&output.stdout)?;
    let dict = plist_val
        .as_dictionary()
        .ok_or_else(|| anyhow::anyhow!("unexpected diskutil output for {id}"))?;

    // Never, ever show an internal disk in the picker. This is the
    // single most important safety check in the whole app.
    let internal = dict.get("Internal").and_then(|v| v.as_boolean()).unwrap_or(true);
    if internal {
        return Ok(None);
    }

    // Mounted DMGs also report Internal=false and Removable=true, so
    // checking only the Internal flag makes every attached disk image
    // appear as a flash target. They are virtual devices, not physical
    // media, and must never clutter the picker or reach the writer.
    let is_virtual = dict
        .get("VirtualOrPhysical")
        .and_then(|v| v.as_string())
        == Some("Virtual");
    let is_disk_image = dict.get("BusProtocol").and_then(|v| v.as_string())
        == Some("Disk Image");
    if is_virtual || is_disk_image {
        return Ok(None);
    }

    let name = dict
        .get("MediaName")
        .and_then(|v| v.as_string())
        .unwrap_or(id)
        .to_string();
    let size_bytes = dict
        .get("TotalSize")
        .and_then(|v| v.as_unsigned_integer())
        .unwrap_or(0);

    Ok(Some(DeviceInfo {
        id: id.to_string(),
        name,
        write_path: format!("/dev/r{id}"),
        size_bytes,
    }))
}

// --------------------------------------------------------------- Linux --

#[cfg(target_os = "linux")]
pub fn list_devices() -> anyhow::Result<Vec<DeviceInfo>> {
    let mut devices = Vec::new();
    let root_backing_disks = linux_root_backing_disks();

    for entry in std::fs::read_dir("/sys/block")? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();

        // Loop devices, device-mapper, RAID and zram are never what
        // someone means by "flash a USB drive".
        if name.starts_with("loop")
            || name.starts_with("zram")
            || name.starts_with("dm-")
            || name.starts_with("md")
        {
            continue;
        }

        let removable = std::fs::read_to_string(entry.path().join("removable"))
            .unwrap_or_default()
            .trim()
            == "1";
        let usb_attached = std::fs::canonicalize(entry.path())
            .map(|path| {
                path.components().any(|component| {
                    component
                        .as_os_str()
                        .to_str()
                        .is_some_and(|part| part.starts_with("usb"))
                })
            })
            .unwrap_or(false);
        if !removable && !usb_attached {
            continue;
        }

        // A Linux installation can itself be booted from USB. Never
        // offer the physical disk backing the current root filesystem,
        // including roots layered through LUKS/device-mapper.
        if root_backing_disks.contains(&name) {
            continue;
        }

        let size_sectors: u64 = std::fs::read_to_string(entry.path().join("size"))
            .unwrap_or_default()
            .trim()
            .parse()
            .unwrap_or(0);
        let size_bytes = size_sectors * 512;

        let vendor = std::fs::read_to_string(entry.path().join("device/vendor"))
            .unwrap_or_default()
            .trim()
            .to_string();
        let model = std::fs::read_to_string(entry.path().join("device/model"))
            .unwrap_or_default()
            .trim()
            .to_string();
        let display_name = format!("{vendor} {model}").trim().to_string();
        let display_name = if display_name.is_empty() { name.clone() } else { display_name };

        devices.push(DeviceInfo {
            id: name.clone(),
            name: display_name,
            write_path: format!("/dev/{name}"),
            size_bytes,
        });
    }

    Ok(devices)
}

#[cfg(target_os = "linux")]
fn linux_root_backing_disks() -> std::collections::HashSet<String> {
    let Some(device_number) = std::fs::read_to_string("/proc/self/mountinfo")
        .ok()
        .and_then(|mountinfo| {
            mountinfo.lines().find_map(|line| {
                let fields: Vec<_> = line.split_whitespace().collect();
                (fields.get(4) == Some(&"/"))
                    .then(|| fields.get(2).map(|value| value.to_string()))
                    .flatten()
            })
        })
    else {
        return std::collections::HashSet::new();
    };

    let mut disks = std::collections::HashSet::new();
    collect_linux_backing_disks(
        &std::path::Path::new("/sys/dev/block").join(device_number),
        &mut disks,
    );
    disks
}

#[cfg(target_os = "linux")]
fn collect_linux_backing_disks(
    device: &std::path::Path,
    disks: &mut std::collections::HashSet<String>,
) {
    let Ok(device) = std::fs::canonicalize(device) else {
        return;
    };

    let slaves = device.join("slaves");
    if let Ok(entries) = std::fs::read_dir(slaves) {
        let mut found_slave = false;
        for entry in entries.flatten() {
            found_slave = true;
            collect_linux_backing_disks(&entry.path(), disks);
        }
        if found_slave {
            return;
        }
    }

    let components: Vec<_> = device.components().collect();
    if let Some(block_index) = components
        .iter()
        .position(|component| component.as_os_str() == "block")
    {
        if let Some(name) = components
            .get(block_index + 1)
            .and_then(|component| component.as_os_str().to_str())
        {
            disks.insert(name.to_string());
        }
    }
}
