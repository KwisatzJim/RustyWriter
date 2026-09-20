use anyhow::Context;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter, State};
use tokio::io::{AsyncBufReadExt, AsyncSeekExt};
use tokio::process::Command;

#[cfg(target_os = "linux")]
struct StagedHelper {
    path: PathBuf,
}

struct ProgressFile {
    path: PathBuf,
}

impl ProgressFile {
    async fn create() -> anyhow::Result<Self> {
        let path = crate::shared_temp_dir().join(format!(
            "rustywriter-progress-{}.jsonl",
            uuid::Uuid::new_v4()
        ));
        tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .await
            .context("creating the progress file")?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // The app owns the file and can delete it. The elevated
            // helper only needs append access, not permission to read
            // earlier progress or replace the path.
            tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o622))
                .await
                .context("setting progress-file permissions")?;
        }

        Ok(Self { path })
    }
}

impl Drop for ProgressFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub struct FlashControl {
    active: AtomicBool,
    cancelled: Arc<AtomicBool>,
    cancel_file: Mutex<Option<PathBuf>>,
}

impl Default for FlashControl {
    fn default() -> Self {
        Self {
            active: AtomicBool::new(false),
            cancelled: Arc::new(AtomicBool::new(false)),
            cancel_file: Mutex::new(None),
        }
    }
}

#[cfg(target_os = "linux")]
impl Drop for StagedHelper {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[derive(Deserialize)]
pub struct FlashRequest {
    pub image_path: String,
    pub device_id: String,
    pub verify: bool,
}

fn validate_flash_target<'a>(
    devices: &'a [crate::devices::DeviceInfo],
    requested_id: &str,
    image_size: u64,
) -> anyhow::Result<&'a crate::devices::DeviceInfo> {
    if image_size == 0 {
        anyhow::bail!("the selected image is empty; no data was written");
    }

    let device = find_flash_target(devices, requested_id)?;

    if image_size > device.size_bytes {
        anyhow::bail!(
            "the decompressed image is {} bytes, but {} only holds {} bytes; no data was written",
            image_size,
            device.name,
            device.size_bytes
        );
    }

    Ok(device)
}

fn find_flash_target<'a>(
    devices: &'a [crate::devices::DeviceInfo],
    requested_id: &str,
) -> anyhow::Result<&'a crate::devices::DeviceInfo> {
    let device = devices
        .iter()
        .find(|device| device.id == requested_id)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "the selected target is no longer an allowed removable drive; reconnect it, refresh the drive list, and select it again"
            )
        })?;

    if device.size_bytes == 0 {
        anyhow::bail!(
            "could not determine the capacity of {}; no data was written",
            device.name
        );
    }
    Ok(device)
}

/// Resolves the path to the privileged helper binary.
///
/// Tauri's sidecar mechanism places `externalBin` binaries in the
/// *same directory as the main executable* - `Contents/MacOS/` inside
/// a packaged .app on macOS, right next to `target/debug/rustywriter`
/// in a dev build, same idea on Linux. This holds in both cases, so
/// there's no need to special-case dev vs. release here (an earlier
/// version of this function incorrectly looked in the app's
/// Resources directory for release builds, which is where regular
/// bundled resources live but not where sidecars land).
fn helper_binary_path(_app: &AppHandle) -> anyhow::Result<PathBuf> {
    let bin_name = if cfg!(windows) { "rustywriter-helper.exe" } else { "rustywriter-helper" };
    let mut exe = std::env::current_exe()?;
    exe.pop();
    exe.push(bin_name);
    if exe.exists() {
        return Ok(exe);
    }
    // Workspace target dirs sometimes differ by one level depending on
    // how `cargo tauri dev` was invoked; check the parent too.
    let mut alt = std::env::current_exe()?;
    alt.pop();
    alt.pop();
    alt.push(bin_name);
    Ok(alt)
}

#[cfg(target_os = "linux")]
fn build_elevated_command(helper: &Path, args: &[String]) -> Command {
    let mut cmd = Command::new("pkexec");
    cmd.arg(helper);
    cmd.args(args);
    cmd
}

#[cfg(target_os = "macos")]
fn build_elevated_command(helper: &Path, args: &[String]) -> Command {
    // `do shell script ... with administrator privileges` runs a
    // shell command and pops the native macOS auth dialog. Pass the
    // executable and arguments to osascript as a real argv array, then
    // let AppleScript's `quoted form` escape each value. In particular,
    // this prevents shell expansion of $, backticks, quotes, and other
    // characters that must never become root-level commands.
    let script = r#"
on run argv
    set shellCommand to ""
    repeat with anArgument in argv
        if shellCommand is not "" then set shellCommand to shellCommand & space
        set shellCommand to shellCommand & quoted form of (contents of anArgument)
    end repeat
    do shell script shellCommand with administrator privileges
end run
"#;

    let mut cmd = Command::new("osascript");
    cmd.arg("-e").arg(script).arg(helper).args(args);
    cmd
}

#[tauri::command]
pub async fn start_flash(
    app: AppHandle,
    control: State<'_, FlashControl>,
    req: FlashRequest,
) -> Result<(), String> {
    if control
        .active
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Err("another flash operation is already active".to_string());
    }

    control.cancelled.store(false, Ordering::Release);
    let cancel_path = crate::shared_temp_dir().join(format!(
        "rustywriter-cancel-{}",
        uuid::Uuid::new_v4()
    ));
    *control.cancel_file.lock().expect("cancel mutex poisoned") = Some(cancel_path.clone());

    let result = run_flash(
        app,
        req,
        Arc::clone(&control.cancelled),
        cancel_path.clone(),
    )
    .await
    .map_err(|e| format!("{e:#}"));

    let _ = tokio::fs::remove_file(cancel_path).await;
    *control.cancel_file.lock().expect("cancel mutex poisoned") = None;
    control.active.store(false, Ordering::Release);
    result
}

#[tauri::command]
pub fn cancel_flash(control: State<'_, FlashControl>) -> Result<(), String> {
    if !control.active.load(Ordering::Acquire) {
        return Err("there is no active flash operation to cancel".to_string());
    }

    control.cancelled.store(true, Ordering::Release);
    if let Some(path) = control
        .cancel_file
        .lock()
        .map_err(|_| "cancel state is unavailable")?
        .as_ref()
    {
        std::fs::write(path, b"cancel").map_err(|e| format!("could not request cancellation: {e}"))?;
    }
    Ok(())
}

async fn run_flash(
    app: AppHandle,
    req: FlashRequest,
    cancelled: Arc<AtomicBool>,
    cancel_path: PathBuf,
) -> anyhow::Result<()> {
    let helper = helper_binary_path(&app)?;
    if !helper.exists() {
        anyhow::bail!(
            "couldn't find the rustywriter-helper binary at {} - see README for the sidecar build step",
            helper.display()
        );
    }

    // AppImages mount their contents via FUSE without `allow_other`,
    // which means only the invoking user's own processes can read
    // those files - not even root. Since pkexec runs as root, it
    // can't reach a helper binary sitting inside an AppImage's mount
    // point (paths like /tmp/.mount_XXXXXX/...), failing with a
    // plain "Permission denied" that has nothing to do with the
    // helper's own permissions. Copying it out to a normal temp file
    // first sidesteps the FUSE restriction entirely. This only
    // matters on Linux - macOS .app bundles are ordinary files on
    // disk, not a FUSE mount, so pkexec's macOS equivalent
    // (osascript) never runs into this.
    #[cfg(target_os = "linux")]
    let helper = stage_helper_binary(&helper).await?;

    // Stage the image (decompressing if needed) into a plain temp
    // file *before* the privileged helper is launched. This runs in
    // this unprivileged process, which is the one the OS granted
    // permission to read the exact file the person picked - the
    // helper launched afterward never touches the original path, so
    // it never runs into macOS's protected-folder (Desktop/Documents/
    // Downloads) permission model at all.
    let initial_devices = crate::devices::list_devices().context("refreshing the removable drive list")?;
    let initial_device = find_flash_target(&initial_devices, &req.device_id)?;
    let staging_space = crate::image_source::available_staging_bytes()?;
    let stage_limits = crate::image_source::StageLimits {
        target_bytes: initial_device.size_bytes,
        temporary_bytes: staging_space,
    };

    let stage_app = app.clone();
    let stage_cancelled = Arc::clone(&cancelled);
    let staged = tokio::task::spawn_blocking(move || {
        crate::image_source::stage_image(
            std::path::Path::new(&req.image_path),
            stage_limits,
            move || stage_cancelled.load(Ordering::Acquire),
            move |written, total| {
                let _ = stage_app.emit(
                    "flash-progress",
                    serde_json::json!({ "phase": "staging", "bytes_processed": written, "total_bytes": total }),
                );
            },
        )
    })
    .await
    .context("staging task panicked")?
    .context("reading/decompressing the selected image")?;

    // Device names such as /dev/sdb and disk4 can be reused after a
    // drive is unplugged. Re-enumerate after the potentially lengthy
    // staging step, accept only a device that is still on the backend's
    // removable-drive allow-list, and use the freshly discovered write
    // path rather than a path supplied by JavaScript.
    let run_result = async {
        let staged_size = tokio::fs::metadata(&staged.path)
            .await
            .context("reading the staged image size")?
            .len();
        let current_devices =
            crate::devices::list_devices().context("refreshing the removable drive list")?;
        let device = validate_flash_target(&current_devices, &req.device_id, staged_size)?;

        run_helper(
            &app,
            helper_path(&helper),
            &staged.path,
            &device.write_path,
            &cancel_path,
            req.verify,
        )
        .await
    }
    .await;

    run_result
}

#[cfg(target_os = "linux")]
fn helper_path(helper: &StagedHelper) -> &Path {
    &helper.path
}

#[cfg(target_os = "macos")]
fn helper_path(helper: &PathBuf) -> &Path {
    helper
}

#[cfg(target_os = "linux")]
async fn stage_helper_binary(helper: &std::path::Path) -> anyhow::Result<StagedHelper> {
    let dest = crate::shared_temp_dir().join(format!("rustywriter-helper-{}", uuid::Uuid::new_v4()));
    let staged = StagedHelper { path: dest };
    tokio::fs::copy(helper, &staged.path)
        .await
        .context("copying the helper binary out of the app bundle")?;

    use std::os::unix::fs::PermissionsExt;
    let mut perms = tokio::fs::metadata(&staged.path).await?.permissions();
    perms.set_mode(0o755);
    tokio::fs::set_permissions(&staged.path, perms).await?;

    Ok(staged)
}

async fn run_helper(
    app: &AppHandle,
    helper: &Path,
    staged_image: &std::path::Path,
    device_path: &str,
    cancel_path: &Path,
    verify: bool,
) -> anyhow::Result<()> {
    let progress_file = ProgressFile::create().await?;
    let progress_path = &progress_file.path;

    let mut args = vec![
        "--image".to_string(),
        staged_image.to_string_lossy().to_string(),
        "--device".to_string(),
        device_path.to_string(),
        "--progress-file".to_string(),
        progress_path.to_string_lossy().to_string(),
        "--cancel-file".to_string(),
        cancel_path.to_string_lossy().to_string(),
    ];
    if verify {
        args.push("--verify".to_string());
    }

    let mut cmd = build_elevated_command(helper, &args);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .context("launching the privileged helper (the auth dialog may have been cancelled)")?;

    // pkexec/osascript print their own errors (a cancelled auth
    // prompt, no polkit agent available, etc) to stderr rather than
    // through our progress-file protocol, since that failure can
    // happen before the helper ever runs at all. Capture it so it
    // reaches the app's error banner instead of only a terminal the
    // person may not be watching.
    let mut child_stderr = child.stderr.take().expect("stderr was piped");
    let stderr_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = tokio::io::AsyncReadExt::read_to_end(&mut child_stderr, &mut buf).await;
        String::from_utf8_lossy(&buf).trim().to_string()
    });

    let tail_app = app.clone();
    let tail_path = progress_path.clone();
    let tailer = tokio::spawn(async move {
        tail_progress_file(tail_app, tail_path).await;
    });

    let status = child.wait().await.context("waiting for the helper process")?;
    // The helper writes its last progress line (often the one with
    // the actual error message) right before exiting. The tailer
    // polls every 150ms, so give it one more beat to catch up before
    // we stop it - otherwise a real, specific error can get lost and
    // the UI falls back to a generic message.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    tailer.abort();
    let captured_stderr = stderr_task.await.unwrap_or_default();

    if !status.success() {
        if captured_stderr.is_empty() {
            anyhow::bail!(
                "flashing failed - see the progress log emitted just before this for the reason"
            );
        }
        anyhow::bail!("flashing failed: {captured_stderr}");
    }
    Ok(())
}

/// Polls the progress file like `tail -f` and re-emits each JSON line
/// as a `flash-progress` window event for the frontend to consume.
async fn tail_progress_file(app: AppHandle, path: PathBuf) {
    let mut position: u64 = 0;
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        let Ok(mut file) = tokio::fs::File::open(&path).await else {
            continue;
        };
        if file.seek(std::io::SeekFrom::Start(position)).await.is_err() {
            continue;
        }

        let mut reader = tokio::io::BufReader::new(file);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) => break, // caught up - wait for more
                Ok(n) => {
                    position += n as u64;
                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim()) {
                        let _ = app.emit("flash-progress", value);
                    }
                }
                Err(_) => break,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{validate_flash_target, ProgressFile};
    use crate::devices::DeviceInfo;

    fn removable_drive(size_bytes: u64) -> DeviceInfo {
        DeviceInfo {
            id: "test-drive".to_string(),
            name: "Test Drive".to_string(),
            write_path: "/dev/test-drive".to_string(),
            size_bytes,
        }
    }

    #[test]
    fn rejects_a_target_that_is_no_longer_listed() {
        let error = validate_flash_target(&[], "test-drive", 1024).unwrap_err();
        assert!(error.to_string().contains("no longer"));
    }

    #[test]
    fn rejects_an_empty_image() {
        let devices = [removable_drive(2048)];
        let error = validate_flash_target(&devices, "test-drive", 0).unwrap_err();
        assert!(error.to_string().contains("empty"));
    }

    #[test]
    fn rejects_an_image_larger_than_the_drive() {
        let devices = [removable_drive(1024)];
        let error = validate_flash_target(&devices, "test-drive", 2048).unwrap_err();
        assert!(error.to_string().contains("only holds 1024 bytes"));
    }

    #[test]
    fn accepts_an_image_that_fits() {
        let devices = [removable_drive(2048)];
        let selected = validate_flash_target(&devices, "test-drive", 1024).unwrap();
        assert_eq!(selected.write_path, "/dev/test-drive");
    }

    #[tokio::test]
    async fn progress_file_is_removed_when_its_guard_is_dropped() {
        let progress = ProgressFile::create().await.unwrap();
        let path = progress.path.clone();
        assert!(path.exists());

        drop(progress);

        assert!(!path.exists());
    }
}
