const { invoke } = window.__TAURI__.core;
const { open } = window.__TAURI__.dialog;
const { listen } = window.__TAURI__.event;

const state = {
  image: null, // { path, name, sizeBytes }
  devices: [],
  selectedDeviceId: null,
  currentStep: 0,
};

// Same idea as balenaEtcher's own "large drive" heuristic: flash
// drives are almost always well under this size, so a removable
// device above it is more likely to be an accidentally-listed
// external SSD, a secondary internal data drive, or similar - exactly
// the kind of drive you do NOT want to erase by mistake.
const LARGE_DRIVE_BYTES = 128 * 1024 ** 3; // 128 GB

const el = (id) => document.getElementById(id);

// ---- Appearance ----------------------------------------------------------

const THEME_STORAGE_KEY = "rustywriter-theme";
const systemTheme = window.matchMedia("(prefers-color-scheme: light)");
let themePreference = "system";

function applyTheme(preference) {
  themePreference = ["system", "light", "dark"].includes(preference) ? preference : "system";
  const resolved = themePreference === "system"
    ? (systemTheme.matches ? "light" : "dark")
    : themePreference;
  document.documentElement.dataset.theme = resolved;
  el("theme-select").value = themePreference;
}

try {
  applyTheme(localStorage.getItem(THEME_STORAGE_KEY) || "system");
} catch (e) {
  console.warn("couldn't read the saved appearance", e);
  applyTheme("system");
}

el("theme-select").addEventListener("change", (event) => {
  applyTheme(event.target.value);
  try {
    localStorage.setItem(THEME_STORAGE_KEY, themePreference);
  } catch (e) {
    console.warn("couldn't save the appearance", e);
  }
});

systemTheme.addEventListener("change", () => {
  if (themePreference === "system") applyTheme("system");
});

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
  const dev = selectedDevice();
  const isLarge = dev && dev.size_bytes >= LARGE_DRIVE_BYTES;
  el("drive-continue-btn").disabled = !dev;
  el("flash-btn").disabled = !(
    state.image && dev && (!isLarge || el("large-drive-confirm-checkbox").checked)
  );
}

function showWizardStep(step, { focus = true } = {}) {
  if (step > 0 && !state.image) step = 0;
  if (step > 1 && !selectedDevice()) step = 1;
  state.currentStep = step;

  document.querySelectorAll(".wizard-screen").forEach((screen) => {
    const active = Number(screen.dataset.screen) === step;
    screen.hidden = !active;
    screen.classList.toggle("active", active);
    if (active && focus) screen.querySelector("h2")?.focus();
  });

  document.querySelectorAll(".step-marker").forEach((marker) => {
    const markerStep = Number(marker.dataset.goStep);
    marker.classList.toggle("active", markerStep === step);
    marker.classList.toggle("complete", markerStep < step);
    marker.disabled = (markerStep === 1 && !state.image) ||
      (markerStep === 2 && !(state.image && selectedDevice()));
    if (markerStep === step) marker.setAttribute("aria-current", "step");
    else marker.removeAttribute("aria-current");
  });

  if (step === 2) updateReview();
}

document.querySelectorAll(".step-marker").forEach((marker) => {
  marker.addEventListener("click", () => showWizardStep(Number(marker.dataset.goStep)));
});

// ---- Step 1: image picker ------------------------------------------------

async function setImage(path) {
  const summary = el("image-summary");
  try {
    const sizeBytes = await invoke("file_size", { path });
    const name = path.split(/[\\/]/).pop();
    state.image = { path, name, sizeBytes };

    summary.classList.remove("summary-empty");
    summary.textContent = `${name} — ${formatBytes(sizeBytes)}`;
    el("drive-image-name").textContent = name;
    el("drive-image-meta").textContent = formatBytes(sizeBytes);
    updateFlashButtonState();
    showWizardStep(1);
    return true;
  } catch (e) {
    console.warn("couldn't select image", e);
    state.image = null;
    summary.classList.add("summary-empty");
    summary.textContent = `Could not select that image: ${String(e)}`;
    updateFlashButtonState();
    return false;
  }
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
  const overlay = el("drag-overlay");
  try {
    const { getCurrentWebview } = window.__TAURI__.webview;
    await getCurrentWebview().onDragDropEvent(async (event) => {
      const kind = event.payload.type;
      if (kind === "over" || kind === "enter") {
        overlay.classList.remove("hidden");
      } else if (kind === "leave") {
        overlay.classList.add("hidden");
      } else if (kind === "drop") {
        overlay.classList.add("hidden");
        const paths = event.payload.paths || [];
        if (paths.length > 0) await setImage(paths[0]);
      }
    });
  } catch (e) {
    console.error("couldn't initialize drag-and-drop", e);
    overlay.classList.add("hidden");
    document.querySelector(".dropzone-hint").textContent =
      "Drag-and-drop is unavailable; use Choose file instead.";
  }
})();

// ---- Step 2: device list --------------------------------------------------

let deviceLoadRevision = 0;

async function loadDevices() {
  const revision = ++deviceLoadRevision;
  const refreshButton = el("refresh-devices-btn");
  const empty = el("device-empty");
  refreshButton.disabled = true;
  empty.textContent = "Scanning for removable drives…";
  empty.style.display = "block";

  try {
    const devices = await invoke("list_devices");
    if (revision !== deviceLoadRevision) return;

    state.devices = devices;
    if (!state.devices.some((device) => device.id === state.selectedDeviceId)) {
      state.selectedDeviceId = null;
    }
    empty.textContent = "No removable physical drives detected";
  } catch (e) {
    if (revision !== deviceLoadRevision) return;
    console.error("failed to list devices", e);
    state.devices = [];
    state.selectedDeviceId = null;
    empty.textContent = `Could not scan for removable drives: ${String(e)}`;
  } finally {
    if (revision === deviceLoadRevision) {
      refreshButton.disabled = false;
      renderDevices();
      updateFlashButtonState();
      showWizardStep(state.currentStep, { focus: false });
    }
  }
}

function renderDevices() {
  const list = el("device-list");
  const empty = el("device-empty");
  list.replaceChildren();

  if (state.devices.length === 0) {
    empty.style.display = "block";
    return;
  }
  empty.style.display = "none";

  for (const dev of state.devices) {
    const li = document.createElement("li");
    li.className = "device-item";
    li.tabIndex = 0;
    li.setAttribute("role", "radio");
    const isSelected = dev.id === state.selectedDeviceId;
    li.setAttribute("aria-checked", String(isSelected));
    if (isSelected) li.classList.add("selected");
    const isLarge = dev.size_bytes >= LARGE_DRIVE_BYTES;
    li.classList.add(isLarge ? "large-drive" : "safe-drive");

    const dot = document.createElement("span");
    dot.className = `drive-dot ${isLarge ? "large" : "safe"}`;

    const name = document.createElement("span");
    name.className = "device-name";
    name.textContent = dev.name;

    const meta = document.createElement("span");
    meta.className = "device-meta";
    meta.append(document.createTextNode(`${dev.write_path} · ${formatBytes(dev.size_bytes)}`));
    if (isLarge) {
      const badge = document.createElement("span");
      badge.className = "large-badge";
      badge.textContent = "Large";
      meta.append(badge);
    }

    li.append(dot, name, meta);
    const selectDrive = () => {
      state.selectedDeviceId = dev.id;
      renderDevices();
      updateFlashButtonState();
      showWizardStep(state.currentStep, { focus: false });
      const selected = [...list.children].find((item) => item.getAttribute("aria-checked") === "true");
      selected?.focus();
    };
    li.addEventListener("click", selectDrive);
    li.addEventListener("keydown", (event) => {
      if (event.key === "Enter" || event.key === " ") {
        event.preventDefault();
        selectDrive();
      }
    });
    list.appendChild(li);
  }
}

el("refresh-devices-btn").addEventListener("click", loadDevices);
el("change-image-btn").addEventListener("click", () => showWizardStep(0));
el("drive-back-btn").addEventListener("click", () => showWizardStep(0));
el("drive-continue-btn").addEventListener("click", () => showWizardStep(2));
el("review-back-btn").addEventListener("click", () => showWizardStep(1));
loadDevices();

// ---- Step 3: review + flash ---------------------------------------------

function selectedDevice() {
  return state.devices.find((d) => d.id === state.selectedDeviceId) || null;
}

function updateReview() {
  const dev = selectedDevice();
  if (!dev || !state.image) return;
  el("review-image-name").textContent = state.image.name;
  el("review-image-meta").textContent = formatBytes(state.image.sizeBytes);
  el("review-device-name").textContent = dev.name;
  el("review-device-meta").textContent = `${dev.write_path} · ${formatBytes(dev.size_bytes)}`;

  const isLarge = dev.size_bytes >= LARGE_DRIVE_BYTES;
  el("large-drive-warning").classList.toggle("hidden", !isLarge);
  el("large-drive-confirm-row").classList.toggle("hidden", !isLarge);
  if (!isLarge) el("large-drive-confirm-checkbox").checked = false;
  updateFlashButtonState();
}

el("large-drive-confirm-checkbox").addEventListener("change", updateFlashButtonState);
el("flash-btn").addEventListener("click", startFlash);

// ---- Progress view ----------------------------------------------------

let unlistenProgress = null;
let resultShown = false;

function setProgressPercent(percent) {
  const value = Math.max(0, Math.min(100, percent));
  el("progress-fill").style.width = `${value}%`;
  el("progress-track").setAttribute("aria-valuenow", String(Math.round(value)));
  el("progress-track").removeAttribute("aria-valuetext");
}

async function startFlash() {
  const dev = selectedDevice();
  const largeDriveNeedsConfirmation = dev &&
    dev.size_bytes >= LARGE_DRIVE_BYTES &&
    !el("large-drive-confirm-checkbox").checked;
  if (!state.image || !dev || largeDriveNeedsConfirmation) return;
  el("setup-view").classList.add("hidden");
  el("progress-view").classList.remove("hidden");
  el("progress-phase-label").textContent = "Reading image\u2026";
  el("progress-detail").textContent = "You'll see an authentication prompt once the image is ready.";
  setProgressPercent(0);
  el("result-banner").classList.add("hidden");
  el("done-btn").classList.add("hidden");
  el("cancel-btn").classList.remove("hidden");
  el("cancel-btn").disabled = false;
  el("cancel-btn").textContent = "Cancel";
  resultShown = false;

  unlistenProgress = await listen("flash-progress", (event) => onProgress(event.payload));

  try {
    await invoke("start_flash", {
      req: {
        image_path: state.image.path,
        device_id: dev.id,
        verify: el("verify-checkbox").checked,
      },
    });
  } catch (e) {
    // If the progress stream already surfaced a specific reason (the
    // usual case), don't stomp on it with this generic message - the
    // Rust side's own error here is deliberately vague ("see the
    // progress log") because the real detail lives in the last
    // progress event, not in this exception.
    if (!resultShown) {
      const message = String(e);
      if (message.toLowerCase().includes("cancelled by the user")) {
        showCancelled("Operation cancelled. No write was started.");
      } else {
        showResult(false, message);
      }
    }
  } finally {
    if (unlistenProgress) unlistenProgress();
  }
}

function onProgress(payload) {
  const phase = payload.phase;
  const label = el("progress-phase-label");
  const detail = el("progress-detail");

  if (phase === "staging") {
    label.textContent = "Reading image\u2026";
    if (payload.total_bytes != null) {
      const pct = Math.min(100, (payload.bytes_processed / payload.total_bytes) * 100);
      setProgressPercent(pct);
      detail.textContent = `${formatBytes(payload.bytes_processed)} of ${formatBytes(payload.total_bytes)} (${pct.toFixed(0)}%)`;
    } else {
      detail.textContent = `${formatBytes(payload.bytes_processed)} processed`;
      el("progress-track").removeAttribute("aria-valuenow");
      el("progress-track").setAttribute(
        "aria-valuetext",
        `${formatBytes(payload.bytes_processed)} processed; total size is not yet known`,
      );
    }
  } else if (phase === "starting") {
    label.textContent = "Starting\u2026";
  } else if (phase === "unmounting") {
    label.textContent = "Unmounting drive\u2026";
  } else if (phase === "flashing") {
    label.textContent = "Writing image\u2026";
    const pct = Math.min(100, (payload.bytes_written / payload.total_bytes) * 100);
    setProgressPercent(pct);
    detail.textContent = `${formatBytes(payload.bytes_written)} of ${formatBytes(payload.total_bytes)} (${pct.toFixed(0)}%)`;
  } else if (phase === "verifying") {
    label.textContent = "Verifying\u2026";
    const pct = Math.min(100, (payload.bytes_verified / payload.total_bytes) * 100);
    setProgressPercent(pct);
    detail.textContent = `${formatBytes(payload.bytes_verified)} of ${formatBytes(payload.total_bytes)} verified`;
  } else if (phase === "ejecting") {
    label.textContent = "Ejecting drive\u2026";
    setProgressPercent(100);
  } else if (phase === "done") {
    setProgressPercent(100);
    label.textContent = "Complete";
    const successMessage = payload.verified ? "Written and verified successfully." : "Written successfully.";
    if (payload.eject_warning) {
      showWarning(`${successMessage} ${payload.eject_warning}`);
    } else {
      showResult(true, successMessage);
    }
  } else if (phase === "error") {
    showResult(false, payload.message);
  } else if (phase === "cancelled") {
    showCancelled("Operation cancelled. The target may contain a partial image and should be flashed again before use.");
  }
}

function showResult(success, message) {
  resultShown = true;
  const banner = el("result-banner");
  banner.classList.remove("hidden", "success", "error", "warning");
  banner.classList.add(success ? "success" : "error");
  banner.textContent = message;
  el("done-btn").classList.remove("hidden");
  el("cancel-btn").classList.add("hidden");
  el("progress-phase-label").textContent = success ? "Done" : "Failed";
}

function showCancelled(message) {
  showResult(false, message);
  el("progress-phase-label").textContent = "Cancelled";
}

function showWarning(message) {
  resultShown = true;
  const banner = el("result-banner");
  banner.classList.remove("hidden", "success", "error", "warning");
  banner.classList.add("warning");
  banner.textContent = message;
  el("done-btn").classList.remove("hidden");
  el("cancel-btn").classList.add("hidden");
  el("progress-phase-label").textContent = "Complete with warning";
}

el("cancel-btn").addEventListener("click", async () => {
  const button = el("cancel-btn");
  button.disabled = true;
  button.textContent = "Cancelling…";
  el("progress-phase-label").textContent = "Cancelling…";
  try {
    await invoke("cancel_flash");
  } catch (e) {
    showResult(false, `Could not cancel: ${String(e)}`);
  }
});

el("done-btn").addEventListener("click", () => {
  el("progress-view").classList.add("hidden");
  el("setup-view").classList.remove("hidden");
  state.image = null;
  state.selectedDeviceId = null;
  const summary = el("image-summary");
  summary.classList.add("summary-empty");
  summary.textContent = "No image selected";
  el("drive-image-name").textContent = "No image selected";
  el("drive-image-meta").textContent = "";
  el("large-drive-confirm-checkbox").checked = false;
  updateFlashButtonState();
  showWizardStep(0);
  loadDevices();
});

showWizardStep(0, { focus: false });
