// Prevents an extra terminal window from appearing on Windows in release
// builds (harmless no-op on macOS/Linux).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod devices;
mod flash;
mod image_source;

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

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![list_devices, file_size, flash::start_flash])
        .run(tauri::generate_context!())
        .expect("error while running RustyWriter");
}
