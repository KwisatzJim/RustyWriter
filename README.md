# RustyWriter

A minimal, fast, cross-platform disk-imaging tool — the same job as
balenaEtcher, written in Rust with a Tauri GUI. Targets macOS and
Linux for v1.

<img width="906" height="765" alt="Screenshot 2026-08-10 at 5 21 51 PM" src="https://github.com/user-attachments/assets/fc7caa7a-0256-46c9-b223-50b671ed232e" />


## Architecture

The interesting design problem in a tool like this is: writing to a
raw block device needs root/admin, but you really don't want your
whole GUI (webview, JS engine, network stack) running elevated.
Etcher solves this by shelling out to a privileged child process for
the actual write, and RustyWriter does the same:

```
┌─────────────────────────┐   1. stages image to /tmp          ┌───────────────────────────┐
│   rustywriter (Tauri)   │      (decompress if needed)         │                            │
│   runs unprivileged     │                                     │                            │
│                          │   2. pkexec / osascript ──────────▶│   rustywriter-helper       │
│  - lists removable      │                                     │   runs as root, briefly    │
│    drives (read-only,   │                                     │                            │
│    no privilege needed) │◀──── tails a progress file ────────│  - unmounts the device     │
│  - file picker           │                                    │  - writes the staged file  │
│  - decompresses/stages   │                                    │    to the device           │
│    the picked image      │                                    │  - re-reads + hashes to    │
│  - renders progress      │                                    │    verify (optional)       │
└─────────────────────────┘                                     └───────────────────────────┘
```

**Why staging happens in the unprivileged app, not the helper:** macOS
gates read access to Desktop/Documents/Downloads/iCloud Drive/network
volumes behind a permission system keyed to the *executable*, not the
user ID — running as root doesn't bypass it. The native file-picker
dialog grants a one-time read exception to whichever process shows
it (the Tauri app), not to some other process launched moments later.
So the app reads and decompresses the picked image itself — wherever
it lives — into a plain scratch file under the system temp dir (never
one of the protected folders), and only hands the privileged helper
that already-local path. This also shrinks what runs elevated down to
"copy a file to a device and verify," which is a nice side benefit
independent of the permissions issue: less code running as root is
just good practice.

**Why a file for progress, not stdout:** on macOS the helper is
launched via `osascript -e 'do shell script "..." with administrator
privileges'`, which buffers the child's entire stdout until it exits
and doesn't support streaming stdin either. That makes a live
progress bar over stdout/stdin impossible. Instead the helper appends
newline-delimited JSON to a temp file, and the Tauri app polls that
file like `tail -f`, re-emitting each line as a `flash-progress`
window event the frontend listens for. This works identically on
Linux (`pkexec`) and macOS. Progress during the app's own staging
step uses the same event, emitted directly (no file needed there,
since it's all in-process).

**Image pipeline:** format is sniffed from magic bytes, not the file
extension (a renamed file shouldn't corrupt a flash). Gzip, xz, and
raw images are streamed straight through a decompressor into the
staging file in 4 MiB chunks. Zip is handled the same way, using the
exact uncompressed size the zip central directory records for that
entry.

**Verification:** a SHA-256 hash is computed over the staged file
while it's being written to the device (free — no extra pass).
After writing, the helper re-reads exactly that many bytes back off
the device, hashes them, and compares digests.

**Safety:** `devices.rs` never lists a non-removable/internal disk —
on macOS anything where `diskutil info` reports `Internal: true` is
excluded outright; on Linux, only devices under `/sys/block` with
`removable == 1` are listed. The GUI additionally requires an
explicit confirm-modal click naming the exact drive and its size
before anything is touched.

## Project layout

```
RustyWriter/
├── helper/            # privileged CLI worker (rustywriter-helper)
│                       # only ever touches: a staged plain file, the
│                       # device, and the progress file
├── src-tauri/          # the Tauri app (unprivileged)
│   ├── src/
│   │   ├── main.rs        # commands: list_devices, file_size, start_flash
│   │   ├── devices.rs     # removable-drive enumeration (macOS/Linux)
│   │   ├── image_source.rs # sniffs format, decompresses/stages the
│   │   │                    # picked image into a plain temp file
│   │   └── flash.rs       # stages the image, spawns the helper
│   │                        # elevated, tails its progress
│   ├── tauri.conf.json
│   └── capabilities/
├── ui/                 # plain HTML/CSS/JS frontend (no npm needed)
└── scripts/
    └── prepare-sidecar.sh
```

## Building

You'll need:
- A current Rust toolchain (`rustup` recommended)
- The [Tauri CLI](https://tauri.app): `cargo install tauri-cli --version "^2"`
- Tauri's usual system prerequisites for your OS (WebKitGTK + friends
  on Linux, Xcode command line tools on macOS) — see
  https://v2.tauri.app/start/prerequisites/

There's no npm/node step — the frontend is plain HTML/CSS/JS served
directly, so `frontendDist` in `tauri.conf.json` just points at `ui/`.

### Dev

```bash
cargo build -p rustywriter-helper   # build the helper once
cargo tauri dev
```

In dev mode, `flash.rs` finds `rustywriter-helper` sitting next to the
main binary in the shared workspace `target/` directory automatically
— no extra setup needed.

### Release build

Packaged apps need the helper bundled as a Tauri "sidecar", which
requires it to be named with the host's target-triple suffix
(`rustywriter-helper-x86_64-apple-darwin`, etc). Run the staging
script first:

```bash
./scripts/prepare-sidecar.sh
cargo tauri build
```

**On Linux, use `./scripts/build-linux-release.sh` instead** of the
two commands above. On Arch-family distros (CachyOS, Arch,
EndeavourOS, etc), `cargo tauri build`'s AppImage step fails with
`failed to run linuxdeploy` because the system `strip` (from newer
binutils) produces ELF sections linuxdeploy's own bundled `strip`
doesn't recognize - this is an upstream linuxdeploy/binutils gap, not
a RustyWriter bug, and the standard workaround is building with
`NO_STRIP=true` set. The script bakes that in, so it's just:

```bash
./scripts/build-linux-release.sh
```

If it still fails after that, it's usually FUSE - AppImages need it
to mount themselves at bundle time:

```bash
sudo pacman -S fuse2      # Arch/CachyOS
sudo apt install fuse     # Debian/Ubuntu/Pop!_OS
```

## macOS: Full Disk Access is required, and it's not optional

Writing to a raw whole-disk device (`/dev/rdiskN`) has required an
explicit, user-granted **Full Disk Access** permission since macOS
Catalina — deliberately, to stop a compromised or malicious root
process from silently wiping disks. **Running as root does not
bypass this.** There's no way to code around it; the person running
RustyWriter has to grant it once, manually:

1. **System Settings → Privacy & Security → Full Disk Access**
2. Click **+**, press **Cmd+Shift+G**, and enter the path to the
   helper binary - in a dev build that's:
   ```
   <project>/target/debug/rustywriter-helper
   ```
   (a packaged app's helper lives at whatever path
   `scripts/prepare-sidecar.sh` staged it to)
3. Toggle it on, then try flashing again.

This grant is tied to the binary's code signature. An unsigned/
ad-hoc-signed dev build can have the grant silently invalidated by a
rebuild, requiring you to re-add it in System Settings. A release
build signed with a stable Developer ID certificate doesn't have this
problem - the grant persists across rebuilds/updates. Worth
mentioning this clearly in-app (a first-run hint, or a dedicated error
message when the device open fails with "Operation not permitted") so
it isn't mysterious to whoever downloads a build of this later.

## Safety features

- **Large-drive warning**: any removable drive at or above 128 GB
  (the same rough heuristic balenaEtcher uses) gets a visible "Large"
  badge in the drive list, plus an extra warning banner and a
  required confirmation checkbox in the erase dialog before the
  "Yes, erase and write" button becomes clickable. The idea is to
  catch the specific mistake of an accidentally-listed external SSD
  or secondary data drive getting selected instead of a small flash
  drive - not to slow down normal USB stick flashing.
- **Drag-and-drop**: dropping an image file anywhere in the window
  selects it, same as the file picker. This uses Tauri's native
  webview drag-drop event API (`getCurrentWebview().onDragDropEvent`)
  rather than plain HTML5 `ondrop`, because `dragDropEnabled` in
  `tauri.conf.json` makes the webview intercept OS file drops before
  they'd ever reach a DOM drop event.

## Known v1 limitations / next steps

- **Progress percentage while staging gzip/xz images**: the
  decompressed size isn't known ahead of time for streaming gzip/xz,
  so the staging bar falls back to showing bytes processed instead of
  a percentage until it's done. Once staging finishes, the actual
  write-to-device progress always shows a true percentage, since the
  staged file's exact size is known by then. Zip images show a true
  percentage throughout, since the zip central directory records the
  exact uncompressed size up front.
- **Staging needs free disk space** equal to the image's decompressed
  size in the OS temp directory (typically `/tmp`), since the whole
  image is materialized there before writing starts.
- **Windows isn't implemented yet.** The helper's device-write and
  hashing logic is portable, but `devices.rs` (drive enumeration) and
  `platform.rs` (unmount/eject) both need Windows-specific
  implementations (`IOCTL_DISK_GET_DRIVE_GEOMETRY` /
  `IOCTL_VOLUME_*` for enumeration, `DeviceIoControl` with
  `FSCTL_LOCK_VOLUME` for exclusive access, and elevation via a UAC
  prompt instead of pkexec/osascript).
- **No cancel button yet** — once a flash starts, it runs to
  completion or failure. Adding cancellation means having the helper
  watch for a sentinel file or signal between chunk writes and abort
  cleanly (closing the device handle mid-write is safe; the drive is
  just left partially written, same as pulling it during any other
  flash tool).
- **No drag-and-drop image selection**, just the file picker button —
  easy to add to `main.js` later.

## A note on sandboxed verification

The `rustywriter-helper` crate (all the byte-level image/device/hash
logic — the highest-risk, most novel part of this project) was
compiled and type-checked while writing this. The Tauri/GUI half
(`src-tauri`) could not be fully compiled in the environment this was
written in, since it needs system WebKitGTK packages that weren't
available there — run `cargo tauri dev` locally as your first step to
shake out anything that needs adjusting on your actual machine.
