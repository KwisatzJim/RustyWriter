const { invoke } = window.__TAURI__.core;
const { open } = window.__TAURI__.dialog;
const { listen } = window.__TAURI__.event;

const state = {
  image: null, // { path, name, sizeBytes }
  devices: [],
  selectedDeviceId: null,
};

// Same idea as balenaEtcher's own "large drive" heuristic: flash
// drives are almost always well under this size, so a removable
// device above it is more likely to be an accidentally-listed
// external SSD, a secondary internal data drive, or similar - exactly
// the kind of drive you do NOT want to erase by mistake.
const LARGE_DRIVE_BYTES = 128 * 1024 ** 3; // 128 GB

const el = (id) => document.getElementById(id);

function formatBytes(bytes) {
  if (bytes == null) return "unknown size";
  const units = ["B", "KB", "MB", "GB", "TB"];
  let n = bytes;
  let i = 0;
  while (n >= 1024 && i < units.length - 1) {
    n /= 1024;
    i++;
  }
  return `${n.toFixed(i === 0 ? 0 : 1)} ${units[i]}`;
}

function updateFlashButtonState() {
  el("flash-btn").disabled = !(state.image && state.selectedDeviceId);
}

// ---- Step 1: image picker ------------------------------------------------

async function setImage(path) {
  let sizeBytes = null;
  try {
    sizeBytes = await invoke("file_size", { path });
  } catch (e) {
    console.warn("couldn't read file size", e);
  }

  const name = path.split(/[\\/]/).pop();
  state.image = { path, name, sizeBytes };

  const summary = el("image-summary");
  summary.classList.remove("summary-empty");
  summary.textContent = `${name} — ${formatBytes(sizeBytes)}`;
  updateFlashButtonState();
}

el("pick-image-btn").addEventListener("click", async () => {
  const picked = await open({
    multiple: false,
    filters: [
      { name: "Disk images", extensions: ["img", "iso", "gz", "xz", "zip"] },
    ],
  });
  if (!picked) return;
  const path = Array.isArray(picked) ? picked[0] : picked;
  await setImage(path);
});

// Native drag-and-drop: with dragDropEnabled the webview intercepts OS
// file drops before they'd ever reach a normal HTML5 "drop" event, so
// this goes through Tauri's own webview event API instead.
(async () => {
  const { getCurrentWebview } = window.__TAURI__.webview;
  const overlay = el("drag-overlay");

  await getCurrentWebview().onDragDropEvent((event) => {
    const kind = event.payload.type;
    if (kind === "over" || kind === "enter") {
      overlay.classList.remove("hidden");
    } else if (kind === "leave") {
      overlay.classList.add("hidden");
    } else if (kind === "drop") {
      overlay.classList.add("hidden");
      const paths = event.payload.paths || [];
      if (paths.length > 0) setImage(paths[0]);
    }
  });
})();

// ---- Step 2: device list --------------------------------------------------

async function loadDevices() {
  try {
    state.devices = await invoke("list_devices");
  } catch (e) {
    console.error("failed to list devices", e);
    state.devices = [];
  }
  renderDevices();
}

function renderDevices() {
  const list = el("device-list");
  const empty = el("device-empty");
  list.innerHTML = "";

  if (state.devices.length === 0) {
    empty.style.display = "block";
    return;
  }
  empty.style.display = "none";

  for (const dev of state.devices) {
    const li = document.createElement("li");
    li.className = "device-item";
    if (dev.id === state.selectedDeviceId) li.classList.add("selected");
    const isLarge = dev.size_bytes >= LARGE_DRIVE_BYTES;
    li.classList.add(isLarge ? "large-drive" : "safe-drive");
    li.innerHTML = `
      <span class="drive-dot ${isLarge ? "large" : "safe"}"></span>
      <span class="device-name">${dev.name}</span>
      <span class="device-meta">${dev.write_path} · ${formatBytes(dev.size_bytes)}${isLarge ? '<span class="large-badge">Large</span>' : ""}</span>
    `;
    li.addEventListener("click", () => {
      state.selectedDeviceId = dev.id;
      renderDevices();
      updateFlashButtonState();
    });
    list.appendChild(li);
  }
}

el("refresh-devices-btn").addEventListener("click", loadDevices);
loadDevices();

// ---- Step 3: confirm + flash ----------------------------------------------

function selectedDevice() {
  return state.devices.find((d) => d.id === state.selectedDeviceId) || null;
}

el("flash-btn").addEventListener("click", () => {
  const dev = selectedDevice();
  if (!dev || !state.image) return;

  el("confirm-detail").textContent =
    `${state.image.name}  →  ${dev.name} (${dev.write_path}, ${formatBytes(dev.size_bytes)})`;

  const isLarge = dev.size_bytes >= LARGE_DRIVE_BYTES;
  const warning = el("large-drive-warning");
  const confirmRow = el("large-drive-confirm-row");
  const confirmCheckbox = el("large-drive-confirm-checkbox");
  const proceedBtn = el("confirm-proceed-btn");

  warning.classList.toggle("hidden", !isLarge);
  confirmRow.classList.toggle("hidden", !isLarge);
  confirmCheckbox.checked = false;
  proceedBtn.disabled = isLarge; // large drives require the checkbox first

  confirmCheckbox.onchange = () => {
    proceedBtn.disabled = isLarge && !confirmCheckbox.checked;
  };

  el("confirm-modal").classList.remove("hidden");
});

el("confirm-cancel-btn").addEventListener("click", () => {
  el("confirm-modal").classList.add("hidden");
});

el("confirm-proceed-btn").addEventListener("click", () => {
  el("confirm-modal").classList.add("hidden");
  startFlash();
});

// ---- Progress view ----------------------------------------------------

let unlistenProgress = null;
let resultShown = false;

async function startFlash() {
  const dev = selectedDevice();
  el("setup-view").classList.add("hidden");
  el("progress-view").classList.remove("hidden");
  el("progress-phase-label").textContent = "Reading image\u2026";
  el("progress-detail").textContent = "You'll see an authentication prompt once the image is ready.";
  el("progress-fill").style.width = "0%";
  el("result-banner").classList.add("hidden");
  el("done-btn").classList.add("hidden");
  resultShown = false;

  unlistenProgress = await listen("flash-progress", (event) => onProgress(event.payload));

  try {
    await invoke("start_flash", {
      req: {
        image_path: state.image.path,
        device_path: dev.write_path,
        verify: el("verify-checkbox").checked,
      },
    });
  } catch (e) {
    // If the progress stream already surfaced a specific reason (the
    // usual case), don't stomp on it with this generic message - the
    // Rust side's own error here is deliberately vague ("see the
    // progress log") because the real detail lives in the last
    // progress event, not in this exception.
    if (!resultShown) showResult(false, String(e));
  } finally {
    if (unlistenProgress) unlistenProgress();
  }
}

function onProgress(payload) {
  const phase = payload.phase;
  const fill = el("progress-fill");
  const label = el("progress-phase-label");
  const detail = el("progress-detail");

  if (phase === "staging") {
    label.textContent = "Reading image\u2026";
    if (payload.total_bytes != null) {
      const pct = Math.min(100, (payload.bytes_processed / payload.total_bytes) * 100);
      fill.style.width = `${pct}%`;
      detail.textContent = `${formatBytes(payload.bytes_processed)} of ${formatBytes(payload.total_bytes)} (${pct.toFixed(0)}%)`;
    } else {
      detail.textContent = `${formatBytes(payload.bytes_processed)} processed`;
    }
  } else if (phase === "starting") {
    label.textContent = "Starting\u2026";
  } else if (phase === "unmounting") {
    label.textContent = "Unmounting drive\u2026";
  } else if (phase === "flashing") {
    label.textContent = "Writing image\u2026";
    const pct = Math.min(100, (payload.bytes_written / payload.total_bytes) * 100);
    fill.style.width = `${pct}%`;
    detail.textContent = `${formatBytes(payload.bytes_written)} of ${formatBytes(payload.total_bytes)} (${pct.toFixed(0)}%)`;
  } else if (phase === "verifying") {
    label.textContent = "Verifying\u2026";
    const pct = Math.min(100, (payload.bytes_verified / payload.total_bytes) * 100);
    fill.style.width = `${pct}%`;
    detail.textContent = `${formatBytes(payload.bytes_verified)} of ${formatBytes(payload.total_bytes)} verified`;
  } else if (phase === "ejecting") {
    label.textContent = "Ejecting drive\u2026";
    fill.style.width = "100%";
  } else if (phase === "done") {
    fill.style.width = "100%";
    label.textContent = "Complete";
    showResult(true, payload.verified ? "Written and verified successfully." : "Written successfully.");
  } else if (phase === "error") {
    showResult(false, payload.message);
  }
}

function showResult(success, message) {
  resultShown = true;
  const banner = el("result-banner");
  banner.classList.remove("hidden", "success", "error");
  banner.classList.add(success ? "success" : "error");
  banner.textContent = message;
  el("done-btn").classList.remove("hidden");
  el("progress-phase-label").textContent = success ? "Done" : "Failed";
}

el("done-btn").addEventListener("click", () => {
  el("progress-view").classList.add("hidden");
  el("setup-view").classList.remove("hidden");
  state.image = null;
  state.selectedDeviceId = null;
  const summary = el("image-summary");
  summary.classList.add("summary-empty");
  summary.textContent = "No image selected";
  updateFlashButtonState();
  loadDevices();
});
