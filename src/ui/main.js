const isTauri = Boolean(window.__TAURI__);
const invoke = isTauri ? window.__TAURI__.core.invoke : invokeWeb;
const dialogOpen = isTauri ? window.__TAURI__.dialog.open : null;

let cfg = null;
let statuses = [];
let activeTab = "0";
let busy = false;
let folderPicker = null;
let scheduleEditor = null;
let latestDestinationSchedule = defaultDestinationSchedule();

const el = {
  configPath: document.getElementById("config-path"),
  sourcePanel: document.getElementById("source-panel"),
  message: document.getElementById("message"),
  config: document.getElementById("config"),
  refresh: document.getElementById("refresh"),
  save: document.getElementById("save"),
  folderModal: document.getElementById("folder-modal"),
  folderPath: document.getElementById("folder-path"),
  folderList: document.getElementById("folder-list"),
  folderUp: document.getElementById("folder-up"),
  folderSelect: document.getElementById("folder-select"),
  folderClose: document.getElementById("folder-close"),
  scheduleModal: document.getElementById("schedule-modal"),
  scheduleClose: document.getElementById("schedule-close"),
  scheduleApply: document.getElementById("schedule-apply"),
  cycleMode: document.getElementById("cycle-mode"),
  cycleTime: document.getElementById("cycle-time"),
  cycleWeekday: document.getElementById("cycle-weekday"),
  cycleWeekdayField: document.getElementById("cycle-weekday-field"),
  configModal: document.getElementById("config-modal"),
  configClose: document.getElementById("config-close"),
  configView: document.getElementById("config-view"),
};

async function loadAll() {
  cfg = await invoke("get_config");
  normalizeConfig(cfg);
  await loadStatus();
  if (!cfg.source_groups[Number(activeTab)]) {
    activeTab = cfg.source_groups.length ? "0" : "";
  }
  render();
}

async function loadStatus() {
  statuses = await invoke("get_status");
}

function render() {
  el.configPath.textContent = isTauri ? "Linux Tauri GUI" : "Headless Web UI";
  renderSourcePanel();
}

function tabButton(label, tabId) {
  const button = document.createElement("button");
  button.className = tabId === activeTab ? "tab active" : "tab";
  button.textContent = label;
  button.onclick = () => {
    activeTab = tabId;
    render();
  };
  return button;
}

function addSource() {
  cfg.source_groups.push({
    id: nextSourceId(),
    src: "",
    enabled: true,
    mode: "mirror",
    destinations: [],
  });
  activeTab = String(cfg.source_groups.length - 1);
  render();
}

function renderSourcePanel() {
  const sourceIndex = Number(activeTab);
  const source = cfg.source_groups[sourceIndex];
  if (!source) {
    el.sourcePanel.hidden = false;
    el.sourcePanel.innerHTML = `
      <div class="section-head">
        <h2>Source</h2>
        <button data-action="add-source">Add Source</button>
      </div>
      <div class="empty">No sources configured</div>
    `;
    el.sourcePanel.querySelector('[data-action="add-source"]').onclick = addSource;
    return;
  }

  el.sourcePanel.hidden = false;
  el.sourcePanel.innerHTML = `
    <div class="section-head">
      <h2>Source</h2>
      <div class="row-actions">
        <button data-action="add-source">Add Source</button>
      </div>
    </div>
    <nav id="source-tabs" class="tabs"></nav>
    <div class="source-layout">
      <div class="source-card">
        <div>
          <label>ID</label>
          <input value="${escapeAttr(source.id)}" data-field="source-id">
        </div>
        <div>
          <label>Source Path</label>
          <input class="path-picker" value="${escapeAttr(source.src)}" data-field="source-src" readonly title="Choose source folder">
        </div>
        <div>
          <label>Latest Cycle</label>
          <input value="${escapeAttr(sourceLatestCycle(source.id))}" readonly>
        </div>
        <button class="danger icon" data-action="remove-source" title="Remove source">x</button>
      </div>
      <div class="destination-list">
        <div class="destination-grid">
          <label>Destination</label>
          <label>Schedule</label>
          <label>Cycle</label>
          <label>Reason</label>
          <label>Actions</label>
          <div id="sync-body" class="destination-body"></div>
        </div>
      </div>
    </div>
  `;

  renderSourceTabs();
  bindSourceControls(source, sourceIndex);
  renderSyncRows(source, sourceIndex);
}

function renderSourceTabs() {
  const tabs = document.getElementById("source-tabs");
  tabs.innerHTML = "";
  cfg.source_groups.forEach((source, index) => {
    tabs.appendChild(tabButton(source.id || `src_${index + 1}`, String(index)));
  });
}

function bindSourceControls(source, sourceIndex) {
  const idInput = el.sourcePanel.querySelector('[data-field="source-id"]');
  const srcInput = el.sourcePanel.querySelector('[data-field="source-src"]');
  idInput.oninput = () => {
    source.id = idInput.value;
    renderSourceTabs();
  };
  srcInput.onclick = async () => {
    const path = await pickFolder(source.src || "/");
    if (path) {
      source.src = path;
      renderSourcePanel();
    }
  };
  el.sourcePanel.querySelector('[data-action="remove-source"]').onclick = () => {
    cfg.source_groups.splice(sourceIndex, 1);
    activeTab = cfg.source_groups.length ? String(Math.max(0, sourceIndex - 1)) : "";
    render();
  };
  el.sourcePanel.querySelector('[data-action="add-source"]').onclick = addSource;
}

function renderSyncRows(source, sourceIndex) {
  const body = document.getElementById("sync-body");
  body.innerHTML = "";

  source.destinations.forEach((dst, dstIndex) => {
    const status = statusFor(source.id, dst.id);
    const row = document.createElement("div");
    row.className = "destination-row";
    const dotClass = status?.status === "green" ? "green" : "red";
    row.innerHTML = `
      <div>
        <div class="destination-cell">
          <span class="dot ${dotClass}" title="${escapeAttr(status?.status || "red")}"></span>
          <input class="dst-id" value="${escapeAttr(dst.id)}" data-field="dst-id" readonly>
          <input class="path-picker dst-path" value="${escapeAttr(dst.path)}" data-field="dst-path" readonly title="Choose destination folder">
        </div>
      </div>
      <div><button class="schedule-button" data-action="edit-schedule">${escapeHtml(scheduleLabel(dst.schedule))}</button></div>
      <div>${escapeHtml(cycleDisplay(status))}</div>
      <div>${escapeHtml(status?.status_reason || "not_verified")}</div>
      <div><button class="danger icon" data-action="remove-dst" title="Remove destination">x</button></div>
    `;
    row.querySelector('[data-field="dst-path"]').onclick = async () => {
      const path = await pickFolder("/");
      if (path) {
        dst.path = path;
        renderSourcePanel();
      }
    };
    row.querySelector('[data-action="remove-dst"]').onclick = () => {
      source.destinations.splice(dstIndex, 1);
      renderSourcePanel();
    };
    row.querySelector('[data-action="edit-schedule"]').onclick = () => {
      openScheduleModal(dst.schedule, (schedule) => {
        dst.schedule = cloneSchedule(schedule);
        latestDestinationSchedule = cloneSchedule(schedule);
        renderSourcePanel();
      });
    };
    body.appendChild(row);
  });

  appendAddDestinationRow(body, source);
}

function appendAddDestinationRow(body, source) {
  const addRow = document.createElement("div");
  addRow.className = "destination-row add-destination-row";
  addRow.innerHTML = `
    <div></div>
    <div></div>
    <div></div>
    <div></div>
    <div>
      <button class="add-destination-button icon" data-action="add-destination" title="Add destination">+</button>
    </div>
  `;
  addRow.querySelector('[data-action="add-destination"]').onclick = async () => {
    const path = await pickFolder("/");
    if (path) {
      source.destinations.push({
        id: nextDestinationId(source),
        path,
        enabled: true,
        schedule: cloneSchedule(latestDestinationSchedule),
      });
      renderSourcePanel();
    }
  };
  body.appendChild(addRow);
}

function nextDestinationId(source) {
  let maxId = 0;
  for (const dst of source.destinations) {
    const match = /^dst_(\d+)$/.exec(dst.id || "");
    if (match) {
      maxId = Math.max(maxId, Number(match[1]));
    }
  }
  return `dst_${maxId + 1}`;
}

function nextSourceId() {
  let maxId = 0;
  for (const source of cfg.source_groups || []) {
    const id = normalizeSourceId(source.id || "");
    const match = /^src_(\d+)$/.exec(id);
    if (match) {
      maxId = Math.max(maxId, Number(match[1]));
    }
  }
  return `src_${maxId + 1}`;
}

function normalizeSourceId(id) {
  return String(id || "").replace(/^source_(\d+)$/, "src_$1");
}

function sourceLatestCycle(sourceId) {
  const cycles = statuses
    .filter((status) => normalizeSourceId(status.source_id) === sourceId)
    .map((status) => status.latest_closed_cycle_id)
    .filter((cycle) => cycle !== null && cycle !== undefined);
  if (!cycles.length) {
    return "-";
  }
  return String(Math.max(...cycles));
}

function normalizeConfig(nextCfg) {
  nextCfg.schedule = nextCfg.schedule || defaultDestinationSchedule();
  for (const source of nextCfg.source_groups || []) {
    source.id = normalizeSourceId(source.id);
    for (const dst of source.destinations || []) {
      dst.schedule = normalizeSchedule(dst.schedule);
      latestDestinationSchedule = cloneSchedule(dst.schedule);
    }
  }
}

function defaultDestinationSchedule() {
  return {
    mode: "realtime",
    time: "02:00",
    timezone: "local",
    weekday: "monday",
    sync_current_cycle_manually: false,
  };
}

function normalizeSchedule(schedule) {
  return {
    ...defaultDestinationSchedule(),
    ...(schedule || {}),
    time: normalizeScheduleTime(schedule?.time || defaultDestinationSchedule().time),
    weekday: schedule?.weekday || "monday",
  };
}

function cloneSchedule(schedule) {
  return { ...normalizeSchedule(schedule) };
}

function scheduleLabel(schedule) {
  const next = normalizeSchedule(schedule);
  if (next.mode === "daily") {
    return formatScheduleTime(next.time);
  }
  if (next.mode === "weekly") {
    return `${weekdayAbbrev(next.weekday)} ${formatScheduleTime(next.time)}`;
  }
  return "Realtime";
}

function normalizeScheduleTime(value) {
  const text = String(value || "02:00");
  const match = /^(\d{1,2}):(\d{2})(?::\d{2})?$/.exec(text);
  if (!match) {
    return "02:00";
  }
  const hour = Math.min(23, Number(match[1]));
  const minute = Math.min(59, Number(match[2]));
  return `${String(hour).padStart(2, "0")}:${String(minute).padStart(2, "0")}`;
}

function formatScheduleTime(value) {
  return normalizeScheduleTime(value);
}

function weekdayAbbrev(value) {
  const weekdays = {
    monday: "Mon",
    mon: "Mon",
    tuesday: "Tue",
    tue: "Tue",
    wednesday: "Wed",
    wed: "Wed",
    thursday: "Thu",
    thu: "Thu",
    friday: "Fri",
    fri: "Fri",
    saturday: "Sat",
    sat: "Sat",
    sunday: "Sun",
    sun: "Sun",
  };
  return weekdays[String(value || "monday").toLowerCase()] || "Mon";
}

function cycleDisplay(status) {
  return `${status?.last_verified_cycle_id ?? "-"} / ${status?.latest_closed_cycle_id ?? "-"}`;
}

function openScheduleModal(schedule, onApply) {
  const draft = cloneSchedule(schedule);
  scheduleEditor = { draft, onApply };
  el.cycleMode.value = draft.mode;
  el.cycleTime.value = formatScheduleTime(draft.time);
  el.cycleWeekday.value = draft.weekday || "monday";
  updateScheduleModalFields();
  el.scheduleModal.hidden = false;
}

function updateScheduleModalFields() {
  const mode = el.cycleMode.value;
  const scheduled = mode !== "realtime";
  el.cycleTime.parentElement.hidden = !scheduled;
  el.cycleWeekdayField.hidden = mode !== "weekly";
}

function closeScheduleModal(apply) {
  if (apply && scheduleEditor) {
    const schedule = normalizeSchedule({
      mode: el.cycleMode.value,
      time: normalizeScheduleTime(el.cycleTime.value || "02:00"),
      timezone: "local",
      weekday: el.cycleWeekday.value || "monday",
      sync_current_cycle_manually: false,
    });
    scheduleEditor.onApply(schedule);
  }
  el.scheduleModal.hidden = true;
  scheduleEditor = null;
}

function openConfigModal() {
  updateCfgFromForm();
  el.configView.textContent = JSON.stringify(cfg, null, 2);
  el.configModal.hidden = false;
}

function closeConfigModal() {
  el.configModal.hidden = true;
}

function statusFor(sourceId, destinationId) {
  return statuses.find((status) =>
    normalizeSourceId(status.source_id) === sourceId && status.destination_id === destinationId
  );
}

async function pickFolder(startPath = "/") {
  if (isTauri) {
    try {
      const selected = await dialogOpen({
        directory: true,
        multiple: false,
        title: "Choose folder",
      });
      return Array.isArray(selected) ? selected[0] : selected;
    } catch (error) {
      setMessage(String(error));
      return null;
    }
  }
  return pickWebFolder(startPath);
}

async function pickWebFolder(startPath) {
  return new Promise(async (resolve) => {
    folderPicker = { resolve, path: startPath || "/" };
    el.folderModal.hidden = false;
    await loadFolder(folderPicker.path);
  });
}

async function loadFolder(path) {
  try {
    const result = await invoke("browse_dirs", { path });
    folderPicker.path = result.path;
    folderPicker.parent = result.parent;
    el.folderPath.textContent = result.path;
    el.folderList.innerHTML = "";
    for (const entry of result.entries) {
      const row = document.createElement("button");
      row.className = "folder-row";
      row.textContent = entry.name;
      row.onclick = () => loadFolder(entry.path);
      el.folderList.appendChild(row);
    }
    if (!result.entries.length) {
      el.folderList.innerHTML = `<div class="empty">No subdirectories</div>`;
    }
  } catch (error) {
    setMessage(String(error));
  }
}

function closeFolderModal(value) {
  el.folderModal.hidden = true;
  if (folderPicker) {
    folderPicker.resolve(value);
    folderPicker = null;
  }
}

function updateCfgFromForm() {
  cfg.schedule = normalizeSchedule(cfg.schedule);
  for (const source of cfg.source_groups) {
    source.enabled = true;
    source.mode = "mirror";
    for (const dst of source.destinations) {
      dst.enabled = true;
      dst.schedule = normalizeSchedule(dst.schedule);
    }
  }
}

async function saveConfig() {
  updateCfgFromForm();
  cfg = await invoke("save_config_command", { cfg });
  await loadStatus();
}

async function runBusy(message, fn) {
  if (busy) return;
  try {
    setBusy(true);
    setMessage(message || "");
    await fn();
  } catch (error) {
    setMessage(String(error));
  } finally {
    setBusy(false);
  }
}

function setBusy(nextBusy) {
  busy = nextBusy;
  el.config.disabled = busy;
  el.refresh.disabled = busy;
  el.save.disabled = busy;
}

function setMessage(text) {
  el.message.textContent = text || "";
}

function escapeHtml(value) {
  return String(value ?? "").replace(/[&<>"']/g, (ch) => ({
    "&": "&amp;",
    "<": "&lt;",
    ">": "&gt;",
    '"': "&quot;",
    "'": "&#39;",
  })[ch]);
}

function escapeAttr(value) {
  return escapeHtml(value).replace(/`/g, "&#96;");
}

el.config.onclick = openConfigModal;
el.refresh.onclick = () => runBusy("", loadAll);

el.save.onclick = () => runBusy("Saving...", async () => {
  await saveConfig();
  setMessage("");
  render();
});

el.folderClose.onclick = () => closeFolderModal(null);
el.folderSelect.onclick = () => closeFolderModal(folderPicker?.path || null);
el.folderUp.onclick = () => {
  if (folderPicker?.parent) {
    loadFolder(folderPicker.parent);
  }
};

el.scheduleClose.onclick = () => closeScheduleModal(false);
el.scheduleApply.onclick = () => closeScheduleModal(true);
el.cycleMode.onchange = updateScheduleModalFields;
el.configClose.onclick = closeConfigModal;

loadAll().catch((error) => setMessage(String(error)));

async function invokeWeb(command, payload = {}) {
  const routes = {
    get_config: ["GET", "/api/config"],
    save_config_command: ["POST", "/api/config"],
    get_status: ["GET", "/api/status"],
    browse_dirs: ["GET", "/api/browse-dirs"],
  };
  const route = routes[command];
  if (!route) {
    throw new Error(`Unsupported command: ${command}`);
  }

  const [method, path] = route;
  let url = path;
  const options = { method, headers: {} };
  if (command === "browse_dirs") {
    url = `${path}?path=${encodeURIComponent(payload.path || "/")}`;
  } else if (method !== "GET") {
    options.headers["Content-Type"] = "application/json";
    if (command === "save_config_command") {
      options.body = JSON.stringify(payload.cfg);
    } else {
      options.body = JSON.stringify(payload);
    }
  }

  const response = await fetch(url, options);
  if (!response.ok) {
    throw new Error(await response.text());
  }
  return await response.json();
}
