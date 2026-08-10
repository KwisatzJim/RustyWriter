// Prevents an extra terminal window from appearing on Windows in release
// builds (harmless no-op on macOS/Linux).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod devices;
mod flash;
mod image_source;

/// A temp directory guaranteed reachable by *both* this unprivileged
/// process and the privileged helper launched via pkexec/osascript.
///
/// `std::env::temp_dir()` isn't safe for that purpose: on modern
/// systemd-based Linux distros, `$TMPDIR` is frequently set to a
/// per-user private runtime directory like `/run/user/1000/`, which
/// is mode 0700 - readable only by that exact user, not even by root.
/// Anything staged there for the elevated helper to pick up (the
/// progress file, the staged image, the helper binary copy staged out
/// of an AppImage's FUSE mount) would fail with a permission error
/// that has nothing to do with the file's own permissions. Plain
/// `/tmp` is mode 1777 - world-writable with the sticky bit - and
/// reachable by any user including root, so anything crossing the
/// privilege boundary uses this instead of `std::env::temp_dir()`.
pub(crate) fn shared_temp_dir() -> std::path::PathBuf {
    if cfg!(unix) {
        std::path::PathBuf::from("/tmp")
    } else {
        std::env::temp_dir()
    }
}

#[tauri::command]
fn list_devices() -> Result<Vec<devices::DeviceInfo>, String> {
    devices::list_devices().map_err(|e| format!("{e:#}"))
}

#[tauri::command]
fn file_size(path: String) -> Result<u64, String> {
    std::fs::metadata(&path)
        .map(|m| m.len())
        .map_err(|e| format!("couldn't read {path}: {e}"))
}

/// WebKitGTK's DMABUF renderer frequently fails to initialize on Linux
/// systems with NVIDIA drivers (proprietary or open-source) and some
/// Wayland/X11 combinations - and instead of erroring, it silently
/// renders nothing, leaving a blank white window. This is a
/// long-standing WebKitGTK issue affecting every framework that uses
/// the system webview on Linux, not something specific to this app;
/// see https://v2.tauri.app/develop/debug/linux-graphics/
///
/// Must run before GTK/WebKitGTK initializes, so this is the very
/// first thing `main` does. Respects any value the person has already
/// set in their environment, so it stays overridable for anyone
/// debugging graphics issues themselves.
#[cfg(target_os = "linux")]
fn apply_linux_webkit_workarounds() {
    // SAFETY: called at the very start of main(), before any other
    // threads exist, so there's no concurrent access to worry about.
    unsafe {
        if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none() {
            std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
        }
        if std::env::var_os("WEBKIT_DISABLE_COMPOSITING_MODE").is_none() {
            std::env::set_var("WEBKIT_DISABLE_COMPOSITING_MODE", "1");
        }
    }
}

fn main() {
    #[cfg(target_os = "linux")]
    apply_linux_webkit_workarounds();

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![list_devices, file_size, flash::start_flash])
        .run(tauri::generate_context!())
        .expect("error while running RustyWriter");
}
