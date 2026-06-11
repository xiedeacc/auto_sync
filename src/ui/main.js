const isTauri = Boolean(window.__TAURI__);
const invoke = isTauri ? window.__TAURI__.core.invoke : invokeWeb;

let cfg = null;
let statuses = [];
let busy = false;
let folderPicker = null;
let scheduleEditor = null;
let excludeEditor = null;
let latestDestinationSchedule = defaultDestinationSchedule();

const el = {
  configPath: document.getElementById("config-path"),
  sourcePanel: document.getElementById("source-panel"),
  message: document.getElementById("message"),
  config: document.getElementById("config"),
  refresh: document.getElementById("refresh"),
  folderModal: document.getElementById("folder-modal"),
  folderPath: document.getElementById("folder-path"),
  folderList: document.getElementById("folder-list"),
  folderUp: document.getElementById("folder-up"),
  folderSelect: document.getElementById("folder-select"),
  folderClose: document.getElementById("folder-close"),
  folderError: document.getElementById("folder-error"),
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
  issueModal: document.getElementById("issue-modal"),
  issueClose: document.getElementById("issue-close"),
  issueSummary: document.getElementById("issue-summary"),
  issueList: document.getElementById("issue-list"),
  excludeModal: document.getElementById("exclude-modal"),
  excludeClose: document.getElementById("exclude-close"),
  excludeAdd: document.getElementById("exclude-add"),
  excludeSource: document.getElementById("exclude-source"),
  excludeList: document.getElementById("exclude-list"),
};

async function loadAll() {
  cfg = await invoke("get_config");
  normalizeConfig(cfg);
  await loadStatus();
  render();
}

async function loadStatus() {
  statuses = await invoke("get_status");
}

function render() {
  el.configPath.textContent = isTauri ? "Linux Tauri GUI" : "Headless Web UI";
  renderSourcePanel();
}

function addSource() {
  cfg.source_groups.push({
    id: nextSourceId(),
    src: "",
    enabled: true,
    mode: "mirror",
    excludes: [],
    destinations: [],
  });
  render();
}

function renderSourcePanel() {
  if (!cfg.source_groups.length) {
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
        <button data-action="sync-all" class="primary">Sync All</button>
        <button data-action="add-source">Add Source</button>
      </div>
    </div>
    <div id="sources-stack" class="sources-stack"></div>
  `;

  const stack = el.sourcePanel.querySelector("#sources-stack");
  cfg.source_groups.forEach((source, sourceIndex) => {
    const group = document.createElement("div");
    group.className = "source-group";
    group.innerHTML = `
    <div class="source-layout">
      <div class="source-card">
        <div class="source-fields">
          <div>
            <label>ID</label>
            <input value="${escapeAttr(source.id)}" data-field="source-id">
          </div>
          <div>
            <label>Source Path</label>
            <input class="path-picker" value="${escapeAttr(source.src)}" data-field="source-src" readonly title="Choose source path">
          </div>
        </div>
        <div class="source-actions">
          <div>
            <label>Latest Cycle</label>
            <input value="${escapeAttr(sourceLatestCycle(source.id))}" readonly>
          </div>
          <button class="exclude-button" data-action="edit-excludes">Excluded ${excludeCountLabel(source)}</button>
          <button data-action="sync-source">Sync</button>
          <button class="danger icon" data-action="remove-source" title="Remove source">x</button>
        </div>
      </div>
      <div class="destination-list">
        <div class="destination-grid">
          <div class="destination-body"></div>
        </div>
      </div>
    </div>
  `;

    stack.appendChild(group);
    bindSourceControls(source, sourceIndex, group);
    renderSyncRows(source, group);
  });
}

function bindSourceControls(source, sourceIndex, group) {
  const idInput = group.querySelector('[data-field="source-id"]');
  const srcInput = group.querySelector('[data-field="source-src"]');
  idInput.oninput = () => {
    source.id = idInput.value;
  };
  idInput.onchange = () => {
    autoSaveConfig().catch((error) => setMessage(String(error)));
  };
  srcInput.onclick = async () => {
    const path = await pickPath(source.src || "/");
    if (path) {
      source.src = path;
      await autoSaveConfig();
      renderSourcePanel();
    }
  };
  group.querySelector('[data-action="remove-source"]').onclick = async () => {
    cfg.source_groups.splice(sourceIndex, 1);
    await autoSaveConfig();
    render();
  };
  group.querySelector('[data-action="edit-excludes"]').onclick = () => {
    openExcludeModal(source);
  };
  group.querySelector('[data-action="sync-source"]').onclick = () => {
    runBusy("Syncing...", async () => {
      await saveConfig();
      statuses = await invoke("sync_source_now", { sourceId: source.id });
      setMessage("");
      render();
    });
  };
  el.sourcePanel.querySelector('[data-action="add-source"]').onclick = addSource;
  el.sourcePanel.querySelector('[data-action="sync-all"]').onclick = syncAllNow;
}

function renderSyncRows(source, group) {
  const body = group.querySelector(".destination-body");
  body.innerHTML = "";

  source.destinations.forEach((dst, dstIndex) => {
    const status = statusFor(source.id, dst.id);
    const row = document.createElement("div");
    row.className = "destination-row";
    const dotClass = statusClass(status);
    const issueCount = status?.issues?.length || 0;
    const dotTitle = dotClass === "yellow"
      ? `${issueCount} changing file${issueCount === 1 ? "" : "s"}`
      : (status?.status || "red");
    row.innerHTML = `
      <div class="destination-id-cell">
        <label>ID</label>
        <button class="dot ${dotClass}" data-action="show-issues" title="${escapeAttr(dotTitle)}" aria-label="${escapeAttr(dotTitle)}"></button>
        <input class="dst-id" value="${escapeAttr(dst.id)}" data-field="dst-id" readonly>
      </div>
      <div>
        <label>Destination Path</label>
        <input class="path-picker dst-path" value="${escapeAttr(dst.path)}" data-field="dst-path" readonly title="Choose destination path">
      </div>
      <span></span>
      <div>
        <label>Schedule</label>
        <button class="schedule-button" data-action="edit-schedule">${escapeHtml(scheduleLabel(dst.schedule))}</button>
      </div>
      <div>
        <label>Cycle</label>
        <input class="destination-readonly" value="${escapeAttr(cycleDisplay(status))}" readonly>
      </div>
      <div>
        <label>Actions</label>
        <div class="destination-actions">
          <button data-action="sync-dst">Sync</button>
          <button class="danger icon" data-action="remove-dst" title="Remove destination">x</button>
        </div>
      </div>
    `;
    row.querySelector('[data-field="dst-path"]').onclick = async () => {
      const path = await pickPath("/", {
        validate: (nextPath) => destinationPathError(source, nextPath, dst),
      });
      if (path) {
        dst.path = path;
        await autoSaveConfig();
        renderSourcePanel();
      }
    };
    row.querySelector('[data-action="remove-dst"]').onclick = async () => {
      source.destinations.splice(dstIndex, 1);
      await autoSaveConfig();
      renderSourcePanel();
    };
    row.querySelector('[data-action="edit-schedule"]').onclick = () => {
      openScheduleModal(dst.schedule, (schedule) => {
        dst.schedule = cloneSchedule(schedule);
        latestDestinationSchedule = cloneSchedule(schedule);
        renderSourcePanel();
        autoSaveConfig().catch((error) => setMessage(String(error)));
      });
    };
    row.querySelector('[data-action="show-issues"]').onclick = () => {
      if (status?.status === "yellow") {
        openIssueModal(status);
      }
    };
    row.querySelector('[data-action="sync-dst"]').onclick = () => {
      runBusy("Syncing...", async () => {
        await saveConfig();
        statuses = await invoke("sync_destination_now", {
          sourceId: source.id,
          destinationId: dst.id,
        });
        setMessage("");
        render();
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
    <span></span>
    <div></div>
    <div></div>
    <div>
      <div class="destination-actions add-only">
        <button class="add-destination-button icon" data-action="add-destination" title="Add destination">+</button>
      </div>
    </div>
  `;
  addRow.querySelector('[data-action="add-destination"]').onclick = async () => {
    const path = await pickPath("/", {
      validate: (nextPath) => destinationPathError(source, nextPath),
    });
    if (path) {
      source.destinations.push({
        id: nextDestinationId(source),
        path,
        enabled: true,
        schedule: cloneSchedule(latestDestinationSchedule),
      });
      await autoSaveConfig();
      renderSourcePanel();
    }
  };
  body.appendChild(addRow);
}

function destinationPathError(source, path, ignoreDst = null) {
  const normalized = normalizeAbsolutePath(path);
  if (hasDestinationPath(source, normalized, ignoreDst)) {
    return `Destination path already exists: ${normalized}`;
  }
  return "";
}

function hasDestinationPath(source, path, ignoreDst = null) {
  const normalized = normalizeAbsolutePath(path);
  return (source.destinations || []).some((dst) =>
    dst !== ignoreDst && normalizeAbsolutePath(dst.path) === normalized
  );
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
  delete nextCfg.schedule;
  for (const source of nextCfg.source_groups || []) {
    source.id = normalizeSourceId(source.id);
    source.excludes = cleanExcludeList(source.excludes || []);
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
  return `${status?.last_verified_cycle_id ?? "-"} / ${status?.target_cycle_id ?? "-"}`;
}

function statusClass(status) {
  if (status?.status === "green") {
    return "green";
  }
  if (status?.status === "yellow") {
    return "yellow";
  }
  return "red";
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

function openIssueModal(status) {
  const issues = status.issues || [];
  el.issueSummary.textContent = `${status.source_id} -> ${status.destination_id}: ${issues.length} changing file${issues.length === 1 ? "" : "s"}`;
  if (!issues.length) {
    el.issueList.innerHTML = `<div class="empty">No file details recorded</div>`;
  } else {
    el.issueList.innerHTML = issues.map((issue) => `
      <div class="issue-row">
        <div class="issue-path">${escapeHtml(issue.rel_path)}</div>
        <div class="issue-meta">cycle ${escapeHtml(issue.cycle_id ?? "-")} · ${escapeHtml(issue.message || issue.issue_kind || "source_changing")}</div>
      </div>
    `).join("");
  }
  el.issueModal.hidden = false;
}

function closeIssueModal() {
  el.issueModal.hidden = true;
}

function openExcludeModal(source) {
  source.excludes = cleanExcludeList(source.excludes || []);
  excludeEditor = { source };
  renderExcludeModal();
  el.excludeModal.hidden = false;
}

function renderExcludeModal() {
  const source = excludeEditor?.source;
  if (!source) {
    return;
  }
  el.excludeSource.textContent = `${source.id || "source"}: ${source.src || "-"}`;
  if (!source.excludes.length) {
    el.excludeList.innerHTML = `<div class="empty">No excluded paths</div>`;
    return;
  }
  el.excludeList.innerHTML = source.excludes.map((path, index) => `
    <div class="exclude-row">
      <div class="exclude-path">${escapeHtml(path)}</div>
      <button class="danger icon" data-action="remove-exclude" data-index="${index}" title="Remove excluded path">x</button>
    </div>
  `).join("");
  for (const button of el.excludeList.querySelectorAll('[data-action="remove-exclude"]')) {
    button.onclick = async () => {
      source.excludes.splice(Number(button.dataset.index), 1);
      source.excludes = cleanExcludeList(source.excludes);
      await autoSaveConfig();
      renderExcludeModal();
      renderSourcePanel();
    };
  }
}

async function addExcludePath() {
  const source = excludeEditor?.source;
  if (!source) {
    return;
  }
  if (!source.src) {
    setMessage("Select source path first");
    return;
  }
  const selected = await pickPath(source.src);
  if (!selected) {
    return;
  }
  const relative = pathToSourceRelative(source.src, selected);
  if (relative === null) {
    setMessage("Excluded path must be inside source");
    return;
  }
  if (!relative) {
    setMessage("Choose a file or child folder inside source");
    return;
  }
  source.excludes = cleanExcludeList([...(source.excludes || []), relative]);
  await autoSaveConfig();
  renderExcludeModal();
  renderSourcePanel();
}

function closeExcludeModal() {
  el.excludeModal.hidden = true;
  excludeEditor = null;
  renderSourcePanel();
}

function statusFor(sourceId, destinationId) {
  return statuses.find((status) =>
    normalizeSourceId(status.source_id) === sourceId && status.destination_id === destinationId
  );
}

async function pickPath(startPath = "/", options = {}) {
  return new Promise(async (resolve) => {
    folderPicker = { resolve, path: startPath || "/", validate: options.validate || null };
    setFolderError("");
    el.folderModal.hidden = false;
    await loadPath(folderPicker.path);
  });
}

async function loadPath(path) {
  try {
    const result = await invoke("browse_paths", { path });
    folderPicker.path = result.path;
    folderPicker.parent = result.parent;
    el.folderPath.textContent = result.path;
    el.folderList.innerHTML = "";
    for (const entry of result.entries) {
      const row = document.createElement("button");
      row.className = `folder-row ${entry.kind === "file" ? "file-row" : "dir-row"}`;
      row.textContent = entry.kind === "dir" ? `${entry.name}/` : entry.name;
      row.onclick = () => {
        if (entry.kind === "dir") {
          loadPath(entry.path);
        } else {
          closeFolderModal(entry.path);
        }
      };
      el.folderList.appendChild(row);
    }
    if (!result.entries.length) {
      el.folderList.innerHTML = `<div class="empty">No entries</div>`;
    }
  } catch (error) {
    setMessage(String(error));
  }
}

function closeFolderModal(value) {
  if (value && folderPicker?.validate) {
    const error = folderPicker.validate(value);
    if (error) {
      setFolderError(error);
      return;
    }
  }
  el.folderModal.hidden = true;
  setFolderError("");
  if (folderPicker) {
    folderPicker.resolve(value);
    folderPicker = null;
  }
}

function setFolderError(text) {
  el.folderError.textContent = text || "";
  el.folderError.hidden = !text;
}

function updateCfgFromForm() {
  delete cfg.schedule;
  cfg.source_groups = (cfg.source_groups || []).map((source) => {
    source.src = trimPathValue(source.src);
    source.excludes = cleanExcludeList(source.excludes || []);
    source.enabled = true;
    source.mode = "mirror";
    source.destinations = (source.destinations || []).map((dst) => {
      dst.path = trimPathValue(dst.path);
      dst.enabled = true;
      dst.schedule = normalizeSchedule(dst.schedule);
      return dst;
    }).filter((dst) => dst.path);
    source.destinations = dedupeDestinationsByPath(source.destinations);
    return source;
  }).filter((source) => source.src);
}

function dedupeDestinationsByPath(destinations) {
  const seen = new Set();
  const cleaned = [];
  for (const dst of destinations || []) {
    const path = normalizeAbsolutePath(dst.path);
    if (!path || seen.has(path)) {
      continue;
    }
    seen.add(path);
    dst.path = path;
    cleaned.push(dst);
  }
  return cleaned;
}

function trimPathValue(value) {
  return String(value ?? "").trim();
}

function cleanExcludeList(values) {
  const seen = new Set();
  const cleaned = [];
  for (const value of values || []) {
    const path = normalizeRelativePath(value);
    if (path && !seen.has(path)) {
      seen.add(path);
      cleaned.push(path);
    }
  }
  return cleaned;
}

function normalizeRelativePath(value) {
  let path = String(value ?? "").trim().replaceAll("\\", "/");
  path = path.replace(/\/+/g, "/").replace(/^\/+/, "").replace(/\/+$/, "");
  const parts = path.split("/").filter((part) => part && part !== ".");
  if (!parts.length || parts.includes("..")) {
    return "";
  }
  return parts.join("/");
}

function normalizeAbsolutePath(value) {
  let path = String(value ?? "").trim().replaceAll("\\", "/");
  path = path.replace(/\/+/g, "/");
  if (path.length > 1) {
    path = path.replace(/\/+$/, "");
  }
  return path;
}

function pathToSourceRelative(sourcePath, selectedPath) {
  const source = normalizeAbsolutePath(sourcePath);
  const selected = normalizeAbsolutePath(selectedPath);
  if (!source || !selected) {
    return null;
  }
  if (selected === source) {
    return "";
  }
  const prefix = source.endsWith("/") ? source : `${source}/`;
  if (!selected.startsWith(prefix)) {
    return null;
  }
  return normalizeRelativePath(selected.slice(prefix.length));
}

function excludeCountLabel(source) {
  const count = cleanExcludeList(source.excludes || []).length;
  return count ? `(${count})` : "";
}

async function saveConfig() {
  updateCfgFromForm();
  cfg = await invoke("save_config_command", { cfg });
  await loadStatus();
}

async function autoSaveConfig() {
  await saveConfig();
  setMessage("");
}

async function syncAllNow() {
  await runBusy("Syncing...", async () => {
    await saveConfig();
    statuses = await invoke("sync_now");
    setMessage("");
    render();
  });
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

el.folderClose.onclick = () => closeFolderModal(null);
el.folderSelect.onclick = () => closeFolderModal(folderPicker?.path || null);
el.folderUp.onclick = () => {
  if (folderPicker?.parent) {
    loadPath(folderPicker.parent);
  }
};

el.scheduleClose.onclick = () => closeScheduleModal(false);
el.scheduleApply.onclick = () => closeScheduleModal(true);
el.cycleMode.onchange = updateScheduleModalFields;
el.configClose.onclick = closeConfigModal;
el.issueClose.onclick = closeIssueModal;
el.excludeClose.onclick = closeExcludeModal;
el.excludeAdd.onclick = () => addExcludePath().catch((error) => setMessage(String(error)));

loadAll().catch((error) => setMessage(String(error)));

async function invokeWeb(command, payload = {}) {
  const routes = {
    get_config: ["GET", "/api/config"],
    save_config_command: ["POST", "/api/config"],
    get_status: ["GET", "/api/status"],
    sync_now: ["POST", "/api/sync-now"],
    sync_source_now: ["POST", "/api/sync-source-now"],
    sync_destination_now: ["POST", "/api/sync-destination-now"],
    browse_paths: ["GET", "/api/browse-paths"],
  };
  const route = routes[command];
  if (!route) {
    throw new Error(`Unsupported command: ${command}`);
  }

  const [method, path] = route;
  let url = path;
  const options = { method, headers: {} };
  if (command === "browse_paths") {
    url = `${path}?path=${encodeURIComponent(payload.path || "/")}`;
  } else if (method !== "GET") {
    options.headers["Content-Type"] = "application/json";
    if (command === "save_config_command") {
      options.body = JSON.stringify(payload.cfg);
    } else if (command === "sync_source_now") {
      options.body = JSON.stringify({ source_id: payload.sourceId || payload.source_id });
    } else if (command === "sync_destination_now") {
      options.body = JSON.stringify({
        source_id: payload.sourceId || payload.source_id,
        destination_id: payload.destinationId || payload.destination_id,
      });
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
