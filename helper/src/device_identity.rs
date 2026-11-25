use std::io;

#[cfg(target_os = "linux")]
pub fn device_identity(name: &str) -> io::Result<u64> {
    std::fs::read_to_string(
        std::path::Path::new("/sys/block")
            .join(name)
            .join("diskseq"),
    )?
    .trim()
    .parse()
    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

#[cfg(target_os = "macos")]
pub fn device_identity(name: &str) -> io::Result<u64> {
    use std::ffi::{c_char, c_void, CString};

    #[link(name = "IOKit", kind = "framework")]
    extern "C" {
        fn IOBSDNameMatching(port: u32, options: u32, name: *const c_char) -> *mut c_void;
        fn IOServiceGetMatchingService(port: u32, matching: *mut c_void) -> u32;
        fn IORegistryEntryGetRegistryEntryID(entry: u32, id: *mut u64) -> i32;
        fn IOObjectRelease(object: u32) -> i32;
    }

    let name = CString::new(name)?;
    // A zero port selects the default IOKit port. The service lookup consumes
    // the matching dictionary, and we release the returned service reference.
    unsafe {
        let matching = IOBSDNameMatching(0, 0, name.as_ptr());
        if matching.is_null() {
            return Err(io::Error::other("could not create IOKit device query"));
        }
        let service = IOServiceGetMatchingService(0, matching);
        if service == 0 {
            return Err(io::Error::new(io::ErrorKind::NotFound, "disk disappeared"));
        }
        let mut id = 0;
        let status = IORegistryEntryGetRegistryEntryID(service, &mut id);
        IOObjectRelease(service);
        if status != 0 {
            return Err(io::Error::other(format!(
                "could not read IOKit device identity: {status}"
            )));
        }
        Ok(id)
    }
}

#[cfg(target_os = "linux")]
pub fn mounted_btrfs_disks() -> io::Result<std::collections::HashSet<String>> {
    let mut disks = std::collections::HashSet::new();
    let filesystems = match std::fs::read_dir("/sys/fs/btrfs") {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(disks),
        Err(error) => return Err(error),
    };
    for entry in filesystems {
        let devices = entry?.path().join("devices");
        if devices.try_exists()? {
            for device in std::fs::read_dir(devices)? {
                collect_linux_backing_disks(&device?.path(), &mut disks);
            }
        }
    }
    Ok(disks)
}

#[cfg(target_os = "linux")]
pub fn collect_linux_backing_disks(
    device: &std::path::Path,
    disks: &mut std::collections::HashSet<String>,
) {
    let Ok(mut device) = std::fs::canonicalize(device) else {
        return;
    };
    if device.join("partition").exists() {
        device.pop();
    }
    if let Ok(slaves) = std::fs::read_dir(device.join("slaves")) {
        let mut has_slaves = false;
        for slave in slaves.flatten() {
            has_slaves = true;
            collect_linux_backing_disks(&slave.path(), disks);
        }
        if has_slaves {
            return;
        }
    }
    if let Some(name) = device.file_name().and_then(|name| name.to_str()) {
        disks.insert(name.to_string());
    }
}
