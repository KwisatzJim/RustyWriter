use anyhow::Context;
use serde::Deserialize;
use std::path::PathBuf;
use std::process::Stdio;
use tauri::{AppHandle, Emitter};
use tokio::io::{AsyncBufReadExt, AsyncSeekExt};
use tokio::process::Command;

#[derive(Deserialize)]
pub struct FlashRequest {
    pub image_path: String,
    pub device_path: String,
    pub verify: bool,
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
fn build_elevated_command(helper: &PathBuf, args: &[String]) -> Command {
    let mut cmd = Command::new("pkexec");
    cmd.arg(helper);
    cmd.args(args);
    cmd
}

#[cfg(target_os = "macos")]
fn shell_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(target_os = "macos")]
fn build_elevated_command(helper: &PathBuf, args: &[String]) -> Command {
    // `do shell script ... with administrator privileges` runs a
    // single shell string and pops the native macOS auth dialog -
    // exactly the UX we want, but it means we build and quote the
    // command line ourselves rather than passing an argv array.
    let mut full = shell_quote(&helper.to_string_lossy());
    for a in args {
        full.push(' ');
        full.push_str(&shell_quote(a));
    }
    let script = format!("do shell script {} with administrator privileges", shell_quote(&full));

    let mut cmd = Command::new("osascript");
    cmd.arg("-e").arg(script);
    cmd
}

#[tauri::command]
pub async fn start_flash(app: AppHandle, req: FlashRequest) -> Result<(), String> {
    run_flash(app, req).await.map_err(|e| format!("{e:#}"))
}

async fn run_flash(app: AppHandle, req: FlashRequest) -> anyhow::Result<()> {
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
    let stage_app = app.clone();
    let staged = tokio::task::spawn_blocking(move || {
        crate::image_source::stage_image(std::path::Path::new(&req.image_path), move |written, total| {
            let _ = stage_app.emit(
                "flash-progress",
                serde_json::json!({ "phase": "staging", "bytes_processed": written, "total_bytes": total }),
            );
        })
    })
    .await
    .context("staging task panicked")?
    .context("reading/decompressing the selected image")?;

    let run_result = run_helper(&app, &helper, &staged.path, &req.device_path, req.verify).await;

    let _ = tokio::fs::remove_file(&staged.path).await;
    #[cfg(target_os = "linux")]
    {
        let _ = tokio::fs::remove_file(&helper).await;
    }
    run_result
}

#[cfg(target_os = "linux")]
async fn stage_helper_binary(helper: &std::path::Path) -> anyhow::Result<PathBuf> {
    let dest = crate::shared_temp_dir().join(format!("rustywriter-helper-{}", uuid::Uuid::new_v4()));
    tokio::fs::copy(helper, &dest)
        .await
        .context("copying the helper binary out of the app bundle")?;

    use std::os::unix::fs::PermissionsExt;
    let mut perms = tokio::fs::metadata(&dest).await?.permissions();
    perms.set_mode(0o755);
    tokio::fs::set_permissions(&dest, perms).await?;

    Ok(dest)
}

async fn run_helper(
    app: &AppHandle,
    helper: &PathBuf,
    staged_image: &std::path::Path,
    device_path: &str,
    verify: bool,
) -> anyhow::Result<()> {
    let progress_path = crate::shared_temp_dir().join(format!(
        "rustywriter-progress-{}.jsonl",
        uuid::Uuid::new_v4()
    ));
    // Deliberately not pre-creating this file. A hardened pkexec
    // doesn't necessarily grant the elevated process CAP_DAC_OVERRIDE,
    // so a root process can still fail to write to a file some other
    // user already owns, even under /tmp. Letting the helper (which
    // really is root) create the file itself avoids that entirely -
    // it'll own what it creates. The tailer below already handles the
    // file not existing yet by retrying, so this is a safe no-op
    // ordering change.

    let mut args = vec![
        "--image".to_string(),
        staged_image.to_string_lossy().to_string(),
        "--device".to_string(),
        device_path.to_string(),
        "--progress-file".to_string(),
        progress_path.to_string_lossy().to_string(),
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
    // This cleanup can silently fail now that the helper (root) owns
    // the file instead of us: /tmp's sticky bit restricts deletion to
    // the file's owner (or root), so a non-root process can't remove
    // a root-owned file there even though the directory itself is
    // world-writable. Harmless either way - just a leftover temp file
    // instead of a functional problem.
    let _ = tokio::fs::remove_file(&progress_path).await;
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
