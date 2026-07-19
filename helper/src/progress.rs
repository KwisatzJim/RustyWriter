use serde::Serialize;
use std::fs::File;
use std::io::{self, Write};
use std::path::Path;

/// One line of this gets appended to the progress file every time
/// something worth telling the UI about happens. The Tauri app tails
/// this file (like `tail -f`) and re-emits each line as a window event.
///
/// Using a plain file instead of stdout is deliberate: on macOS the
/// helper is launched via `osascript ... with administrator privileges`,
/// which buffers the child's stdout until it exits. A file works
/// identically on both platforms and survives across process spawns.
#[derive(Serialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum Progress {
    Starting,
    Unmounting,
    Flashing {
        bytes_written: u64,
        total_bytes: u64,
    },
    Verifying {
        bytes_verified: u64,
        total_bytes: u64,
    },
    Ejecting,
    Done {
        success: bool,
        verified: bool,
    },
    Error {
        message: String,
    },
}

pub struct ProgressWriter {
    file: File,
}

impl ProgressWriter {
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = File::options().create(true).append(true).open(path)?;
        Ok(Self { file })
    }

    pub fn emit(&mut self, progress: &Progress) {
        // Progress reporting must never crash the flash itself, so we
        // swallow write errors here (e.g. if the GUI process died and
        // deleted the temp file out from under us).
        if let Ok(mut line) = serde_json::to_string(progress) {
            line.push('\n');
            let _ = self.file.write_all(line.as_bytes());
            let _ = self.file.flush();
        }
    }
}
