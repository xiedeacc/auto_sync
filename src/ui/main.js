const invoke = invokeBackend;
const STATUS_POLL_MS = 5000;
const RUNTIME_STATUS_POLL_MS = 1000;

let cfg = null;
let statuses = [];
let runtimeStatus = null;
let syncActivity = { machines: [] };
let lastRuntimeScan = null;
let machineStatus = { online: 1, total: 1, machines: [] };
let busy = false;
let statusBusyMessage = "";
let statusMessage = "";
let statusPolling = false;
let statusPollTimer = null;
let runtimeStatusPollTimer = null;
let runtimeStatusPolling = false;
let folderPicker = null;
let scheduleEditor = null;
let excludeEditor = null;
let dstSyncEditor = null;
let dstLogViewer = null;
const scanReports = {};
const scanPending = {};
let checkingScans = false;

function scanReportKey(sourceId, destinationId) {
  return `${sourceId}|${destinationId}`;
}

// While a background scan is running, poll for its report (identified by a new
// scanned_at) and surface it in the info panel when it completes.
async function checkPendingScans() {
  if (checkingScans) return;
  const entries = Object.entries(scanPending);
  if (!entries.length) return;
  checkingScans = true;
  try {
    for (const [key, info] of entries) {
      let report;
      try {
        report = await invoke("scan_report", {
          sourceId: info.sourceId,
          destinationId: info.destinationId,
        });
      } catch (_error) {
        continue;
      }
      if (report && report.scanned_at && report.scanned_at !== info.prev) {
        scanReports[key] = report;
        delete scanPending[key];
        renderDestinationLogModal();
      }
    }
  } finally {
    checkingScans = false;
  }
}
let latestDestinationSchedule = defaultDestinationSchedule();
let activeSourceTab = "sources";
let machineHostLocked = false;

const el = {
  configPath: document.getElementById("config-path"),
  sourcePanel: document.getElementById("source-panel"),
  readme: document.getElementById("readme"),
  readmeModal: document.getElementById("readme-modal"),
  readmeClose: document.getElementById("readme-close"),
  config: document.getElementById("config"),
  statusConfig: document.getElementById("status-config"),
  statusText: document.getElementById("status-text"),
  statusBuild: document.getElementById("status-build"),
  refresh: document.getElementById("refresh"),
  machineStatus: document.getElementById("machine-status"),
  folderModal: document.getElementById("folder-modal"),
  folderMachine: document.getElementById("folder-machine"),
  folderPath: document.getElementById("folder-path"),
  folderList: document.getElementById("folder-list"),
  folderUp: document.getElementById("folder-up"),
  folderSelect: document.getElementById("folder-select"),
  folderAddDirectoryRow: document.getElementById("folder-add-directory-row"),
  folderAddDirectory: document.getElementById("folder-add-directory"),
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
  settingsModal: document.getElementById("settings-modal"),
  settingsClose: document.getElementById("settings-close"),
  settingsSave: document.getElementById("settings-save"),
  settingsSyncMirror: document.getElementById("settings-sync-mirror"),
  settingsSyncChecksum: document.getElementById("settings-sync-checksum"),
  settingsSyncDebug: document.getElementById("settings-sync-debug"),
  settingsSyncTimeout: document.getElementById("settings-sync-timeout"),
  settingsSyncBwlimit: document.getElementById("settings-sync-bwlimit"),
  settingsTcpPool: document.getElementById("settings-tcp-pool"),
  dstSyncModal: document.getElementById("dst-sync-modal"),
  dstSyncClose: document.getElementById("dst-sync-close"),
  dstSyncSave: document.getElementById("dst-sync-save"),
  dstSyncReset: document.getElementById("dst-sync-reset"),
  dstSyncMirror: document.getElementById("dst-sync-mirror"),
  dstSyncChecksum: document.getElementById("dst-sync-checksum"),
  dstSyncDebug: document.getElementById("dst-sync-debug"),
  dstSyncTimeout: document.getElementById("dst-sync-timeout"),
  dstSyncBwlimit: document.getElementById("dst-sync-bwlimit"),
  machineModal: document.getElementById("machine-modal"),
  machineClose: document.getElementById("machine-close"),
  machineList: document.getElementById("machine-list"),
  machineDiscover: document.getElementById("machine-discover"),
  machineAdd: document.getElementById("machine-add"),
  machineId: document.getElementById("machine-id"),
  machineName: document.getElementById("machine-name"),
  machineAlias: document.getElementById("machine-alias"),
  machineHost: document.getElementById("machine-host"),
  machinePort: document.getElementById("machine-port"),
  machineSshUser: document.getElementById("machine-ssh-user"),
  machineSshPort: document.getElementById("machine-ssh-port"),
  machineOs: document.getElementById("machine-os"),
  machineInstallDir: document.getElementById("machine-install-dir"),
  issueModal: document.getElementById("issue-modal"),
  issueClose: document.getElementById("issue-close"),
  issueSummary: document.getElementById("issue-summary"),
  issueList: document.getElementById("issue-list"),
  dstLogModal: document.getElementById("dst-log-modal"),
  dstLogTitle: document.getElementById("dst-log-title"),
  dstLogClose: document.getElementById("dst-log-close"),
  dstLogSummary: document.getElementById("dst-log-summary"),
  dstLogList: document.getElementById("dst-log-list"),
  scanDiffModal: document.getElementById("scan-diff-modal"),
  scanDiffTitle: document.getElementById("scan-diff-title"),
  scanDiffClose: document.getElementById("scan-diff-close"),
  scanDiffSummary: document.getElementById("scan-diff-summary"),
  scanDiffModalList: document.getElementById("scan-diff-modal-list"),
  excludeModal: document.getElementById("exclude-modal"),
  excludeClose: document.getElementById("exclude-close"),
  excludeAdd: document.getElementById("exclude-add"),
  excludeSource: document.getElementById("exclude-source"),
  excludeList: document.getElementById("exclude-list"),
};

async function loadAll() {
  if (!cfg) {
    cfg = defaultUiConfig();
    render();
  }
  const errors = [];
  try {
    const nextCfg = await invoke("get_config");
    cfg = nextCfg;
    normalizeConfig(cfg);
  } catch (error) {
    errors.push(String(error));
  }
  render();
  try {
    await loadStatus();
  } catch (error) {
    statuses = [];
    errors.push(String(error));
  }
  try {
    await loadRuntimeStatus();
  } catch (error) {
    runtimeStatus = null;
    errors.push(String(error));
  }
  try {
    await loadSyncActivity();
  } catch (error) {
    syncActivity = { machines: [] };
    errors.push(String(error));
  }
  try {
    await loadMachines(false);
  } catch (error) {
    machineStatus = { online: 0, total: 0, machines: [] };
    updateMachineStatusUi();
    errors.push(String(error));
  }
  render();
  startStatusPolling();
  if (errors.length) {
    setMessage(errors.join(" | "));
  }
}

async function loadStatus() {
  statuses = await invoke("get_status");
}

async function loadRuntimeStatus() {
  runtimeStatus = await invoke("get_runtime_status");
  updateStatusBar();
  renderDestinationLogModal();
  checkPendingScans();
}

async function loadSyncActivity() {
  syncActivity = await invoke("get_sync_activity");
  updateStatusUi();
  renderDestinationLogModal();
}

async function loadMachines(discover = false) {
  machineStatus = await invoke(discover ? "discover_machines" : "get_machines");
  updateMachineStatusUi();
}

function startStatusPolling() {
  if (!statusPollTimer) {
    statusPollTimer = setInterval(refreshStatusOnly, STATUS_POLL_MS);
  }
  if (!runtimeStatusPollTimer) {
    runtimeStatusPollTimer = setInterval(refreshRuntimeStatusOnly, RUNTIME_STATUS_POLL_MS);
  }
}

async function refreshStatusOnly() {
  if (busy || statusPolling || !cfg) {
    return;
  }
  statusPolling = true;
  try {
    const errors = [];
    try {
      statuses = await invoke("get_status");
    } catch (error) {
      statuses = [];
      errors.push(String(error));
    }
    try {
      await loadRuntimeStatus();
    } catch (error) {
      runtimeStatus = null;
      updateStatusBar();
      errors.push(String(error));
    }
    try {
      await loadSyncActivity();
    } catch (error) {
      syncActivity = { machines: [] };
      errors.push(String(error));
    }
    try {
      await loadMachines(false);
    } catch (error) {
      errors.push(String(error));
    }
    try {
      updateStatusUi();
    } catch (error) {
      errors.push(String(error));
    }
    if (errors.length) {
      setMessage(errors.join(" | "));
    }
  } finally {
    statusPolling = false;
  }
}

async function refreshRuntimeStatusOnly() {
  if (runtimeStatusPolling) {
    return;
  }
  runtimeStatusPolling = true;
  try {
    await loadRuntimeStatus();
    if (dstLogViewer && el.dstLogModal && !el.dstLogModal.hidden) {
      await loadSyncActivity();
    }
  } catch (_) {
    runtimeStatus = null;
    updateStatusBar();
  } finally {
    runtimeStatusPolling = false;
  }
}

function updateStatusUi() {
  for (const group of el.sourcePanel.querySelectorAll(".source-group")) {
    const sourceId = group.dataset.sourceId;
    const latest = group.querySelector(".source-latest-cycle");
    if (latest) {
      latest.value = sourceLatestCycle(sourceId);
    }
    const sourceSync = group.querySelector('[data-action="sync-source"]');
    if (sourceSync) {
      const unavailable = sourceHasUnavailableDestination(sourceId);
      const blocked = sourceHasBlockedDestination(sourceId);
      const activity = activityForSourceId(sourceId);
      const syncing = activityIsSyncing(activity);
      sourceSync.disabled = busy || blocked || unavailable || syncing;
      sourceSync.title = unavailable
        ? "Destination unavailable"
        : (blocked ? "Blocked by sync order" : (syncing ? activitySyncingLabel(activity) : "Sync source"));
    }
  }

  for (const row of el.sourcePanel.querySelectorAll(".destination-row")) {
    const sourceId = row.dataset.sourceId;
    const destinationId = row.dataset.destinationId;
    const status = statusFor(sourceId, destinationId);
    const unavailable = isDestinationUnavailable(status);
    row.classList.toggle("destination-unavailable", unavailable);
    const cycle = row.querySelector(".destination-cycle");
    if (cycle) {
      cycle.value = cycleDisplay(status);
    }
    const dot = row.querySelector(".dot");
    if (dot) {
      const dotClass = statusClass(status);
      const issueCount = status && status.issues ? status.issues.length : 0;
      const dotTitle = dotClass === "yellow"
        ? `${issueCount} changing file${issueCount === 1 ? "" : "s"}`
        : ((status && status.status) || "red");
      dot.className = `dot ${dotClass}`;
      dot.title = dotTitle;
      dot.setAttribute("aria-label", dotTitle);
    }
    const syncSelect = row.querySelector('[data-action="sync-dst"]');
    if (syncSelect) {
      const blocked = isSyncOrderBlocked(status);
      const activity = activityForSourceId(sourceId);
      const syncing = activityIsSyncing(activity);
      syncSelect.disabled = busy || blocked || unavailable || syncing;
      syncSelect.title = unavailable
        ? unavailableLabel(status)
        : (blocked ? blockedByLabel(status) : (syncing ? activitySyncingLabel(activity) : "Sync"));
    }
    const logButton = row.querySelector('[data-action="show-dst-log"]');
    if (logButton) {
      const activity = activityForSourceId(sourceId);
      const iconState = destinationLogIconState(status, activity);
      logButton.className = `destination-log-button icon destination-log-${iconState.kind}`;
      logButton.title = iconState.title;
      logButton.setAttribute("aria-label", iconState.title);
    }
  }
}

function updateStatusBar() {
  const transfer = runtimeStatus && runtimeStatus.transfer;
  if (transfer) {
    const destination = transferDestinationLabel(transfer);
    const file = transfer.rel_path || "-";
    const fileLabel = compactStatusPath(file, 56);
    const speed = formatBytesPerSecond(transfer.bytes_per_sec || 0);
    const progress = formatTransferProgress(transfer);
    const title = `Destination: ${destination}\nFile: ${file}\nSpeed: ${speed}${progress ? `\n${progress}` : ""}`;
    el.statusText.innerHTML = `
      <span class="status-transfer" title="${escapeAttr(title)}">
        <span class="status-transfer-part status-transfer-label">Backing up</span>
        <span class="status-transfer-part">${escapeHtml(destination)}</span>
        <span class="status-transfer-part status-transfer-main">${escapeHtml(fileLabel)}</span>
        <span class="status-transfer-part status-transfer-speed">${escapeHtml(speed)}</span>
      </span>
    `;
    el.statusText.title = title;
  } else if (runtimeStatus && runtimeStatus.scan) {
    lastRuntimeScan = {
      ...runtimeStatus.scan,
      received_at_ms: Date.now(),
    };
    renderScanLikeStatus("Checking changes", lastRuntimeScan);
  } else if (busy && lastRuntimeScan) {
    renderScanLikeStatus("Checking changes", lastRuntimeScan);
  } else {
    if (!busy) {
      lastRuntimeScan = null;
    }
    const message = statusBusyMessage || statusMessage || "Ready";
    el.statusText.textContent = message;
    el.statusText.title = message;
  }

  const build = runtimeStatus && runtimeStatus.build;
  const commit = (build && build.commit) || "unknown";
  const time = (build && build.commit_time_beijing) || "unknown";
  const buildText = `${commit} · ${time}`;
  el.statusBuild.textContent = buildText;
  el.statusBuild.title = buildText;
}

function renderScanLikeStatus(verb, scan) {
  const current = displayPath(scan.current_path || scan.root_path || "source");
  const root = displayPath(scan.root_path || current);
  const count = Number(scan.entries_seen || 0);
  const suffix = count ? ` · ${count} entries` : "";
  const title = `${verb}: ${current}\nRoot: ${root}${count ? `\nEntries: ${count}` : ""}`;
  const label = `${verb} ${compactStatusPath(current, 86)}${suffix}`;
  el.statusText.innerHTML = `<span class="status-scan" title="${escapeAttr(title)}">${escapeHtml(label)}</span>`;
  el.statusText.title = title;
}

function updateMachineStatusUi() {
  const online = valueOr(machineStatus && machineStatus.online, 1);
  const total = valueOr(machineStatus && machineStatus.total, 1);
  el.machineStatus.textContent = `Machines ${online}/${total}`;
  el.machineStatus.title = "Manage LAN machines";
  if (!el.machineModal.hidden) {
    renderMachineModal();
  }
  renderFolderMachineOptions();
}

function openMachineModal(event) {
  preventDefault(event);
  renderMachineModal();
  el.machineModal.hidden = false;
}

function closeMachineModal() {
  el.machineModal.hidden = true;
}

function renderMachineModal() {
  const machines = (machineStatus && machineStatus.machines) || [];
  const selectedId = cleanMachineId(el.machineId.value);
  if (!machines.length) {
    el.machineList.innerHTML = `<div class="empty">No machines discovered</div>`;
  } else {
    el.machineList.innerHTML = `
      <div class="machine-row machine-row-head">
        <span></span>
        <span>Name</span>
        <span>Host</span>
        <span>Port</span>
        <span>SSH</span>
        <span>OS</span>
        <span></span>
      </div>
      ${machines.map((machine) => `
        <div class="machine-row ${machine.id === selectedId ? "machine-row-selected" : ""}" data-id="${escapeAttr(machine.id)}" title="Edit machine">
          <span class="machine-dot ${machine.online ? "online" : ""}" title="${machine.online ? "Online" : "Offline"}"></span>
          <div class="machine-name-cell">
            <div class="machine-name">${escapeHtml(machinePrimaryName(machine))}</div>
            <div class="machine-meta">${escapeHtml(machineSecondaryName(machine))}</div>
          </div>
          <div class="machine-cell" title="${escapeAttr(machine.host)}">${escapeHtml(machine.host)}</div>
          <div class="machine-cell">${escapeHtml(String(machine.port || "-"))}</div>
          <div class="machine-cell">${escapeHtml(machineSshLabel(machine))}</div>
          <div class="machine-cell">${escapeHtml(machine.os || "-")}</div>
          <button class="danger icon" data-action="remove-machine" data-id="${escapeAttr(machine.id)}" title="Delete machine" ${machine.id === "local" ? "disabled" : ""}>x</button>
        </div>
      `).join("")}
    `;
    for (const row of el.machineList.querySelectorAll(".machine-row[data-id]")) {
      row.onclick = () => {
        const machine = machines.find((item) => item.id === row.dataset.id);
        if (!machine) {
          return;
        }
        selectMachineForEdit(machine);
      };
    }
    for (const button of el.machineList.querySelectorAll('[data-action="remove-machine"]')) {
      button.onclick = (event) => {
        event.stopPropagation();
        removeMachine(button.dataset.id).catch((error) => setMessage(String(error)));
      };
    }
  }
  syncMachineFormLock(machines);
}

function selectMachineForEdit(machine) {
  el.machineId.value = machine.id || "";
  el.machineName.value = machine.name || machine.id || "";
  el.machineAlias.value = machine.alias_name || "";
  el.machineHost.value = machine.host || "";
  el.machinePort.value = machine.port || 18765;
  el.machineSshUser.value = machine.ssh_user || "";
  el.machineSshPort.value = machine.ssh_port || 22;
  el.machineOs.value = machine.os || "linux";
  el.machineInstallDir.value = machine.install_dir || defaultInstallDirForOs(machine.os);
  setMachineHostLocked(machineHostShouldLock(machine));
  renderMachineModal();
}

function clearMachineForm() {
  el.machineId.value = "";
  el.machineName.value = "";
  el.machineAlias.value = "";
  el.machineHost.value = "";
  el.machinePort.value = 18765;
  el.machineSshUser.value = "root";
  el.machineSshPort.value = 22;
  el.machineOs.value = "linux";
  el.machineInstallDir.value = defaultInstallDirForOs("linux");
  setMachineHostLocked(false);
}

function syncMachineFormLock(machines) {
  const selectedId = cleanMachineId(el.machineId.value);
  if (!selectedId) {
    setMachineHostLocked(false);
    return;
  }
  const selected = machines.find((machine) => machine.id === selectedId);
  if (selected) {
    setMachineHostLocked(machineHostShouldLock(selected));
  }
}

function machineHostShouldLock(machine) {
  return Boolean(machine && machine.online);
}

function setMachineHostLocked(locked) {
  machineHostLocked = locked;
  el.machineHost.disabled = locked;
  el.machineHost.title = locked ? "Detected online machine host cannot be changed" : "";
}

function machineSshLabel(machine) {
  if (machine.id === "local" && !machine.ssh_user) {
    return "-";
  }
  const port = machine.ssh_port || 22;
  const user = machine.ssh_user === "Administrator" ? "Admin" : machine.ssh_user;
  return user ? `${user}:${port}` : String(port);
}

async function discoverMachines() {
  await runBusy("Discovering machines...", async () => {
    await loadMachines(true);
    setMessage("");
  });
}

async function addMachine() {
  const host = trimPathValue(el.machineHost.value);
  if (!host) {
    setMessage("Machine host is required");
    return;
  }
  const port = Number(el.machinePort.value || 18765);
  const id = cleanMachineId(el.machineId.value) || machineIdFromEndpoint(host, port);
  const machine = {
    id,
    alias_name: cleanMachineId(el.machineAlias.value),
    name: trimPathValue(el.machineName.value) || id,
    host,
    port,
    ssh_user: trimPathValue(el.machineSshUser.value),
    ssh_port: Number(el.machineSshPort.value || 22),
    os: el.machineOs.value || "linux",
    install_dir: trimPathValue(el.machineInstallDir.value) || defaultInstallDirForOs(el.machineOs.value),
    enabled: true,
    manual: true,
  };
  cfg = await invoke("add_machine", { machine });
  normalizeConfig(cfg);
  await loadMachines(false);
  setMachineHostLocked(machineHostLocked);
  setMessage("");
}

async function removeMachine(machineId) {
  const id = cleanMachineId(machineId);
  if (!id || id === "local") {
    return;
  }
  cfg = await invoke("remove_machine", { machineId: id });
  normalizeConfig(cfg);
  if (cleanMachineId(el.machineId.value) === id) {
    clearMachineForm();
  }
  await loadMachines(false);
  setMessage("");
}

function render() {
  el.configPath.textContent = "";
  renderSourcePanel();
}

function addSource() {
  cfg.source_groups.push({
    id: nextSourceId(),
    machine_id: "local",
    src: "",
    add_directory: false,
    enabled: true,
    mode: "mirror",
    excludes: [],
    destinations: [],
  });
  render();
}

function renderSourcePanel() {
  el.sourcePanel.hidden = false;
  el.sourcePanel.innerHTML = `
    <div class="section-head">
      <h2>Source</h2>
      <div class="row-actions">
        <button data-action="sync-all" class="primary">Sync All</button>
        <button data-action="add-source">Add Source</button>
      </div>
    </div>
    <div class="panel-tabs" role="tablist" aria-label="Source settings">
      <button class="${activeSourceTab === "sources" ? "active" : ""}" data-tab="sources" role="tab">Sources</button>
      <button class="${activeSourceTab === "order" ? "active" : ""}" data-tab="order" role="tab">Order</button>
    </div>
    <div id="source-tab-body"></div>
  `;

  el.sourcePanel.querySelector('[data-action="add-source"]').onclick = addSource;
  el.sourcePanel.querySelector('[data-action="sync-all"]').onclick = syncAllNow;
  for (const tab of el.sourcePanel.querySelectorAll("[data-tab]")) {
    tab.onclick = () => {
      activeSourceTab = tab.dataset.tab;
      renderSourcePanel();
    };
  }

  if (activeSourceTab === "order") {
    renderSyncOrderPanel(el.sourcePanel.querySelector("#source-tab-body"));
    return;
  }

  const body = el.sourcePanel.querySelector("#source-tab-body");
  if (!cfg.source_groups.length) {
    body.innerHTML = `<div class="empty">No sources configured</div>`;
    return;
  }

  body.innerHTML = `
    <div id="sources-stack" class="sources-stack"></div>
  `;

  const stack = body.querySelector("#sources-stack");
  cfg.source_groups.forEach((source, sourceIndex) => {
    const sourcePathIsLocked = sourcePathLocked(source);
    const sourcePathDisplay = machinePathLabel(source.machine_id, source.src);
    const group = document.createElement("div");
    group.className = "source-group";
    group.dataset.sourceId = source.id;
    group.innerHTML = `
    <div class="source-layout">
      <div class="sync-row source-row">
        <div class="row-left">
          <label>ID</label>
          <label>Source Path</label>
          <input class="readonly-id" value="${escapeAttr(source.id)}" data-field="source-id" readonly>
          <input class="path-picker ${sourcePathIsLocked ? "path-picker-locked" : ""}" value="${escapeAttr(sourcePathDisplay)}" data-field="source-src" readonly title="${escapeAttr(sourcePathDisplay)}">
        </div>
        <div class="row-right source-right">
          <label>Latest Cycle</label>
          <span></span>
          <span></span>
          <span></span>
          <input class="source-latest-cycle" value="${escapeAttr(sourceLatestCycle(source.id))}" readonly>
          <button class="exclude-button" data-action="edit-excludes">Excluded ${excludeCountLabel(source)}</button>
          <button class="source-sync-button" data-action="sync-source" title="Sync source">Sync</button>
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

function renderSyncOrderPanel(container) {
  normalizeConfig(cfg);
  const tasks = syncTaskOptions();
  const analysis = analyzeSyncOrderGraph();
  const optionsHtml = tasks.map((task) =>
    `<option value="${escapeAttr(task.key)}">${escapeHtml(task.label)}</option>`
  ).join("");
  const statusClass = analysis.cycle.length ? "order-status error" : "order-status ok";
  const statusText = analysis.cycle.length
    ? `Cycle detected: ${analysis.cycle.join(" > ")}`
    : "DAG valid";

  if (tasks.length < 2) {
    container.innerHTML = `
      <div class="order-panel">
        <div class="empty">Add at least two sync destinations to configure order</div>
      </div>
    `;
    return;
  }

  container.innerHTML = `
    <div class="order-panel">
      <div class="order-add sync-row">
        <div class="order-selects">
          <label>Before</label>
          <label>After</label>
          <select data-field="new-before">${optionsHtml}</select>
          <select data-field="new-after">${optionsHtml}</select>
        </div>
        <button data-action="add-order" class="primary">Add</button>
      </div>
      <div class="${statusClass}">${escapeHtml(statusText)}</div>
      <div class="order-list"></div>
      <div class="dag-wrap">${renderDagSvg(analysis)}</div>
    </div>
  `;

  const beforeSelect = container.querySelector('[data-field="new-before"]');
  const afterSelect = container.querySelector('[data-field="new-after"]');
  if (tasks[1]) {
    afterSelect.value = tasks[1].key;
  }
  container.querySelector('[data-action="add-order"]').onclick = async () => {
    const before = beforeSelect.value;
    const after = afterSelect.value;
    if (!before || !after || before === after) {
      setMessage("Choose two different sync tasks");
      return;
    }
    cfg.sync_order = cleanSyncOrder([
      ...(cfg.sync_order || []),
      { before: keyToTaskRef(before), after: keyToTaskRef(after) },
    ]);
    await saveSyncOrderDraft();
  };
  renderSyncOrderRows(container.querySelector(".order-list"), optionsHtml);
}

function renderSyncOrderRows(container, optionsHtml) {
  if (!cfg.sync_order.length) {
    container.innerHTML = `<div class="empty">No sync order rules</div>`;
    return;
  }
  container.innerHTML = cfg.sync_order.map((rule, index) => `
    <div class="order-rule">
      <select data-field="rule-before" data-index="${index}">${optionsHtml}</select>
      <span class="order-arrow">&gt;</span>
      <select data-field="rule-after" data-index="${index}">${optionsHtml}</select>
      <button class="danger icon" data-action="remove-order" data-index="${index}" title="Remove order">x</button>
    </div>
  `).join("");

  cfg.sync_order.forEach((rule, index) => {
    container.querySelector(`[data-field="rule-before"][data-index="${index}"]`).value = ruleEndpointToKey(rule.before);
    container.querySelector(`[data-field="rule-after"][data-index="${index}"]`).value = ruleEndpointToKey(rule.after);
  });

  for (const select of container.querySelectorAll("select")) {
    select.onchange = async () => {
      const rule = cfg.sync_order[Number(select.dataset.index)];
      rule.before = keyToTaskRef(container.querySelector(`[data-field="rule-before"][data-index="${select.dataset.index}"]`).value);
      rule.after = keyToTaskRef(container.querySelector(`[data-field="rule-after"][data-index="${select.dataset.index}"]`).value);
      cfg.sync_order = cleanSyncOrder(cfg.sync_order || []);
      await saveSyncOrderDraft();
    };
  }
  for (const button of container.querySelectorAll('[data-action="remove-order"]')) {
    button.onclick = async () => {
      cfg.sync_order.splice(Number(button.dataset.index), 1);
      await saveSyncOrderDraft();
    };
  }
}

async function saveSyncOrderDraft() {
  const analysis = analyzeSyncOrderGraph();
  if (analysis.cycle.length) {
    setMessage(`Cycle detected: ${analysis.cycle.join(" > ")}`);
    renderSourcePanel();
    return;
  }
  try {
    await autoSaveConfig();
  } catch (error) {
    setMessage(String(error));
  }
  renderSourcePanel();
}

function bindSourceControls(source, sourceIndex, group) {
  const srcInput = group.querySelector('[data-field="source-src"]');
  srcInput.onclick = async () => {
    if (sourcePathLocked(source)) {
      setMessage(machinePathLabel(source.machine_id, source.src));
      return;
    }
    const selected = await pickPath(source.src || defaultPathForMachine(source.machine_id), {
      machineId: source.machine_id || "local",
      showAddDirectory: true,
      addDirectory: !!source.add_directory,
    });
    if (selected) {
      source.machine_id = selected.machine_id;
      source.src = selected.path;
      source.add_directory = !!selected.add_directory;
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
    if (sourceHasUnavailableDestination(source.id)) {
      setMessage("Source sync is disabled because a destination is unavailable");
      updateStatusUi();
      return;
    }
    if (sourceHasBlockedDestination(source.id)) {
      setMessage("Source sync is blocked by sync order");
      updateStatusUi();
      return;
    }
    const activity = activityForSource(source);
    if (activityIsSyncing(activity)) {
      setMessage(activitySyncingLabel(activity));
      updateStatusUi();
      return;
    }
    runBusy("Checking changes...", async () => {
      await saveConfig();
      statuses = await invoke("sync_source_now", { sourceId: source.id });
      setMessage("");
      render();
    }, { showMainMessage: false });
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
    row.className = "sync-row destination-row";
    row.dataset.sourceId = source.id;
    row.dataset.destinationId = dst.id;
    const dotClass = statusClass(status);
    const issueCount = status && status.issues ? status.issues.length : 0;
    const dotTitle = dotClass === "yellow"
      ? `${issueCount} changing file${issueCount === 1 ? "" : "s"}`
      : ((status && status.status) || "red");
    const logIconState = destinationLogIconState(status, activityForSource(source));
    row.innerHTML = `
      <div class="row-left">
        <label>ID</label>
        <label>Destination Path</label>
        <div class="destination-id-cell">
          <button class="dot ${dotClass}" data-action="show-issues" title="${escapeAttr(dotTitle)}" aria-label="${escapeAttr(dotTitle)}"></button>
          <input class="dst-id readonly-id" value="${escapeAttr(dst.id)}" data-field="dst-id" readonly>
        </div>
        <input class="path-picker dst-path" value="${escapeAttr(machinePathLabel(dst.machine_id, dst.path))}" data-field="dst-path" readonly title="${escapeAttr(machineLabel(dst.machine_id))}">
      </div>
      <div class="row-right destination-right">
        <span aria-hidden="true"></span>
        <label>Schedule</label>
        <label>Cycle</label>
        <span aria-hidden="true"></span>
        <label class="actions-label">Sync</label>
        <button class="destination-log-button icon destination-log-${escapeAttr(logIconState.kind)}" data-action="show-dst-log" title="${escapeAttr(logIconState.title)}" aria-label="${escapeAttr(logIconState.title)}"><span class="destination-log-icon" aria-hidden="true">i</span></button>
        <button class="schedule-button" data-action="edit-schedule">${escapeHtml(scheduleLabel(dst.schedule))}</button>
        <input class="destination-readonly destination-cycle" value="${escapeAttr(cycleDisplay(status))}" readonly>
        <button class="sync-config-button icon" data-action="edit-dst-sync" title="${escapeAttr(destinationSyncTitle(dst))}">&#9881;</button>
        <select class="destination-sync-select" data-action="sync-dst" title="Sync">
          <option value="">Sync</option>
          <option value="incremental">Incremental</option>
          <option value="changed_since">Changed Since</option>
          <option value="full">Full</option>
          <option value="scan">Compare (no changes)</option>
        </select>
        <button class="danger icon" data-action="remove-dst" title="Remove destination">x</button>
      </div>
    `;
    row.querySelector('[data-field="dst-path"]').onclick = async () => {
      const selected = await pickPath(dst.path || defaultPathForMachine(dst.machine_id), {
        machineId: dst.machine_id || source.machine_id || "local",
        validate: (next) => destinationPathError(source, next.path, dst, next.machine_id),
      });
      if (selected) {
        dst.machine_id = selected.machine_id;
        dst.path = selected.path;
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
    row.querySelector('[data-action="edit-dst-sync"]').onclick = () => {
      openDestinationSyncModal(dst, () => {
        renderSourcePanel();
        autoSaveConfig().catch((error) => setMessage(String(error)));
      });
    };
    row.querySelector('[data-action="show-issues"]').onclick = () => {
      const latestStatus = statusFor(source.id, dst.id);
      if (latestStatus && latestStatus.status === "yellow") {
        openIssueModal(latestStatus);
      }
    };
    row.querySelector('[data-action="show-dst-log"]').onclick = () => {
      openDestinationLogModal(source, dst);
    };
    row.querySelector('[data-action="sync-dst"]').onchange = (event) => {
      const mode = event.currentTarget.value;
      event.currentTarget.value = "";
      if (!mode) {
        return;
      }
      const latestStatus = statusFor(source.id, dst.id);
      if (isDestinationUnavailable(latestStatus)) {
        setMessage(unavailableLabel(latestStatus));
        updateStatusUi();
        return;
      }
      if (isSyncOrderBlocked(latestStatus)) {
        setMessage(blockedByLabel(latestStatus));
        updateStatusUi();
        return;
      }
      const activity = activityForSource(source);
      if (activityIsSyncing(activity)) {
        setMessage(activitySyncingLabel(activity));
        updateStatusUi();
        return;
      }
      if (mode === "scan") {
        // The scan runs in the background (it can take many minutes on a large
        // tree and must not block the backup). Open the info panel so live
        // progress shows, kick it off, and poll for the report when it lands.
        const key = scanReportKey(source.id, dst.id);
        openDestinationLogModal(source, dst, "scan");
        runBusy(`Starting compare ${source.id} -> ${dst.id}...`, async () => {
          await saveConfig();
          const previous = await invoke("scan_destination_now", {
            sourceId: source.id,
            destinationId: dst.id,
          });
          scanPending[key] = {
            sourceId: source.id,
            destinationId: dst.id,
            prev: (previous && previous.scanned_at) || "",
          };
          if (previous) {
            scanReports[key] = previous;
          }
          setMessage("Compare running — progress and result appear in the info panel.");
          renderDestinationLogModal();
        }, { showMainMessage: false });
        return;
      }
      runBusy(destinationSyncStatusMessage(source, mode), async () => {
        await saveConfig();
        statuses = await invoke("sync_destination_now", {
          sourceId: source.id,
          destinationId: dst.id,
          mode,
        });
        setMessage("");
        render();
      }, { showMainMessage: false });
    };
    body.appendChild(row);
  });

  appendAddDestinationRow(body, source);
  updateStatusUi();
}

function appendAddDestinationRow(body, source) {
  const addRow = document.createElement("div");
  addRow.className = "sync-row add-destination-row";
  addRow.innerHTML = `
    <div></div>
    <div class="destination-actions add-only">
      <button class="add-destination-button icon" data-action="add-destination" title="Add destination">+</button>
    </div>
  `;
  addRow.querySelector('[data-action="add-destination"]').onclick = async () => {
    const selected = await pickPath(defaultPathForMachine(source.machine_id), {
      machineId: source.machine_id || "local",
      validate: (next) => destinationPathError(source, next.path, null, next.machine_id),
    });
    if (selected) {
      source.destinations.push({
        id: nextDestinationId(source),
        machine_id: selected.machine_id,
        path: selected.path,
        enabled: true,
        schedule: cloneSchedule(latestDestinationSchedule),
      });
      await autoSaveConfig();
      renderSourcePanel();
    }
  };
  body.appendChild(addRow);
}

function destinationPathError(source, path, ignoreDst = null, machineId = "local") {
  const normalized = normalizeAbsolutePath(path);
  if (hasDestinationPath(source, normalized, ignoreDst, machineId)) {
    return `Destination path already exists: ${normalized}`;
  }
  return "";
}

function sourcePathLocked(source) {
  return (source.destinations || []).length > 0;
}

function hasDestinationPath(source, path, ignoreDst = null, machineId = "local") {
  const normalized = normalizeAbsolutePath(path);
  const machineKey = machineReferenceKey(machineId);
  return (source.destinations || []).some((dst) =>
    dst !== ignoreDst && machineReferenceKey(dst.machine_id) === machineKey && normalizeAbsolutePath(dst.path) === normalized
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
  nextCfg.app = normalizeAppConfig(nextCfg.app || {});
  nextCfg.machines = normalizeMachines(nextCfg.machines || []);
  for (const source of nextCfg.source_groups || []) {
    source.id = normalizeSourceId(source.id);
    source.machine_id = machineIdOrLocal(source.machine_id);
    source.add_directory = source.add_directory !== false;
    source.excludes = cleanExcludeList(source.excludes || []);
    for (const dst of source.destinations || []) {
      dst.machine_id = machineIdOrLocal(dst.machine_id);
      dst.schedule = normalizeSchedule(dst.schedule);
      dst.sync = normalizeOptionalNativeSyncConfig(dst.sync);
      latestDestinationSchedule = cloneSchedule(dst.schedule);
    }
  }
  nextCfg.sync_order = cleanSyncOrder(nextCfg.sync_order || []);
}

function normalizeAppConfig(app) {
  return {
    data_db: app.data_db || "conf/state/auto_sync.sqlite",
    log_dir: app.log_dir || "logs",
    status_log_interval_secs: Number(app.status_log_interval_secs || 300),
    port: Number(app.port || appPortFromLegacyBind(app.web_bind) || 18765),
    tcp_connection_pool_size: Number(app.tcp_connection_pool_size ?? 100),
    sync: normalizeNativeSyncConfig(app.sync || {}),
  };
}

function appPortFromLegacyBind(value) {
  const text = trimPathValue(value);
  if (!text) {
    return 0;
  }
  const parts = text.split(":");
  return Number(parts[parts.length - 1] || 0);
}

function normalizeNativeSyncConfig(sync) {
  return {
    mirror: sync.mirror !== false,
    checksum: sync.checksum === true,
    debug_logs: sync.debug_logs === true,
    transfer_timeout_secs: Number(sync.transfer_timeout_secs || 120),
    bwlimit_kbps: Number(sync.bwlimit_kbps || 0),
    max_parallel_transfers: Number(sync.max_parallel_transfers ?? 16),
    modify_window_secs: Number(sync.modify_window_secs ?? 1),
    fsync: sync.fsync === true,
  };
}

function normalizeOptionalNativeSyncConfig(sync) {
  if (!sync || typeof sync !== "object") {
    return undefined;
  }
  return normalizeNativeSyncConfig(sync);
}

function normalizeMachines(values) {
  const seen = new Set();
  const machines = [
    {
      id: "local",
      alias_name: "",
      name: "This machine",
      host: "127.0.0.1",
      port: 18765,
      ssh_user: "",
      ssh_port: 22,
      os: "linux",
      install_dir: defaultInstallDirForOs("linux"),
      enabled: true,
      manual: true,
    },
    ...(values || []),
  ];
  const cleaned = [];
  for (const machine of machines) {
    const id = cleanMachineId(machine.id || "");
    if (!id || seen.has(id)) {
      continue;
    }
    seen.add(id);
    cleaned.push({
      id,
      alias_name: cleanMachineId(machine.alias_name || ""),
      name: trimPathValue(machine.name) || id,
      host: trimPathValue(machine.host) || "127.0.0.1",
      port: Number(machine.port || machine.web_port || 18765),
      ssh_user: trimPathValue(machine.ssh_user),
      ssh_port: Number(machine.ssh_port || 22),
      os: trimPathValue(machine.os) || "linux",
      install_dir: trimPathValue(machine.install_dir) || defaultInstallDirForOs(machine.os),
      enabled: machine.enabled !== false,
      manual: machine.manual !== false,
    });
  }
  return cleaned;
}

function cleanMachineId(value) {
  return String(value || "").trim().replace(/[^A-Za-z0-9_-]/g, "_");
}

function machineIdFromEndpoint(host, machinePort) {
  const hostId = cleanMachineId(host).replace(/^_+|_+$/g, "");
  const port = Number(machinePort || 18765);
  return hostId ? `machine_${hostId}_${port}` : `machine_${port}`;
}

function defaultInstallDirForOs(os) {
  return String(os || "").toLowerCase() === "windows" ? "C:/auto_sync" : "/opt/auto_sync";
}

function machineIdOrLocal(value) {
  return String(value || "").trim() || "local";
}

function machineReferenceKey(value = "local") {
  const id = machineIdOrLocal(value);
  const machine = findMachineByReference(id);
  if (machine) {
    return String(machine.id || id).toLowerCase();
  }
  return id.toLowerCase();
}

function machineLabel(machineId = "local") {
  const id = machineIdOrLocal(machineId);
  const machines = (machineStatus && machineStatus.machines) || (cfg && cfg.machines) || [];
  const machine = findMachineByReference(id);
  return machine ? `Machine: ${machinePrimaryName(machine)}` : `Machine: ${id}`;
}

function machinePrimaryName(machine) {
  if (!machine) {
    return "";
  }
  const alias = trimPathValue(machine.alias_name);
  if (alias) {
    return alias;
  }
  const hostname = machineHostname(machine);
  if (hostname) {
    return hostname;
  }
  const host = trimPathValue(machine.host);
  if (host) {
    return host;
  }
  return machine.id || "";
}

function machineSecondaryName(machine) {
  return machineHostname(machine);
}

function machineHostname(machine) {
  const name = trimPathValue(machine && machine.name);
  if (!name || name === "local") {
    return "";
  }
  return name;
}

function findMachineByReference(value = "local") {
  const id = machineIdOrLocal(value);
  const machines = (machineStatus && machineStatus.machines && machineStatus.machines.length)
    ? machineStatus.machines
    : ((cfg && cfg.machines) || []);
  if (id === "local") {
    return machines.find((item) => item.id === "local");
  }
  return machines.find((item) => trimPathValue(item.alias_name).toLowerCase() === id.toLowerCase())
    || machines.find((item) => String(item.id || "").toLowerCase() === id.toLowerCase())
    || machines.find((item) => trimPathValue(item.host).toLowerCase() === id.toLowerCase());
}

function machinePathLabel(machineId, path) {
  const id = machineIdOrLocal(machineId);
  if (id === "local") {
    return `local: ${displayPath(path)}`;
  }
  const machine = findMachineByReference(id);
  const name = machine ? machinePrimaryName(machine) : id;
  return `${name}: ${displayPath(path)}`;
}

function transferDestinationLabel(transfer) {
  if (!transfer) {
    return "-";
  }
  const endpoint = findDestinationEndpoint(transfer.destination_id);
  if (endpoint) {
    return machinePathLabel(endpoint.machine_id, transfer.destination_path || endpoint.path || transfer.destination_id);
  }
  if (transfer.destination_path) {
    return displayPath(transfer.destination_path);
  }
  return transfer.destination_id || "-";
}

function findDestinationEndpoint(destinationId) {
  for (const source of (cfg && cfg.source_groups) || []) {
    for (const dst of source.destinations || []) {
      if (dst.id === destinationId) {
        return dst;
      }
    }
  }
  return null;
}

function syncTaskOptions() {
  const tasks = [];
  for (const source of cfg.source_groups || []) {
    for (const dst of source.destinations || []) {
      const key = makeTaskKey(source.id, dst.id);
      tasks.push({
        key,
        label: key,
        source,
        destination: dst,
      });
    }
  }
  return tasks;
}

function cleanSyncOrder(rules) {
  const taskKeys = new Set(syncTaskOptions().map((task) => task.key));
  const seen = new Set();
  const cleaned = [];
  for (const rule of rules || []) {
    const before = ruleEndpointToKey(rule.before);
    const after = ruleEndpointToKey(rule.after);
    const edge = `${before}>${after}`;
    if (!before || !after || before === after || !taskKeys.has(before) || !taskKeys.has(after) || seen.has(edge)) {
      continue;
    }
    seen.add(edge);
    cleaned.push({
      before: keyToTaskRef(before),
      after: keyToTaskRef(after),
    });
  }
  return cleaned;
}

function analyzeSyncOrderGraph() {
  const tasks = syncTaskOptions();
  const taskKeys = tasks.map((task) => task.key);
  const graph = new Map(taskKeys.map((key) => [key, []]));
  const indegree = new Map(taskKeys.map((key) => [key, 0]));
  const edges = [];

  for (const rule of cleanSyncOrder(cfg.sync_order || [])) {
    const before = ruleEndpointToKey(rule.before);
    const after = ruleEndpointToKey(rule.after);
    graph.get(before).push(after);
    indegree.set(after, indegree.get(after) + 1);
    edges.push([before, after]);
  }

  const levels = new Map(taskKeys.map((key) => [key, 0]));
  const queue = taskKeys.filter((key) => indegree.get(key) === 0);
  let visited = 0;
  for (let index = 0; index < queue.length; index += 1) {
    const key = queue[index];
    visited += 1;
    for (const next of graph.get(key)) {
      levels.set(next, Math.max(levels.get(next), levels.get(key) + 1));
      indegree.set(next, indegree.get(next) - 1);
      if (indegree.get(next) === 0) {
        queue.push(next);
      }
    }
  }

  const cycle = visited === taskKeys.length ? [] : detectSyncOrderCycle(graph, taskKeys);
  if (cycle.length) {
    taskKeys.forEach((key, index) => levels.set(key, index % 3));
  }

  return { tasks, edges, levels, cycle };
}

function detectSyncOrderCycle(graph, taskKeys) {
  const visiting = new Set();
  const visited = new Set();
  const stack = [];

  function visit(key) {
    if (visiting.has(key)) {
      const start = stack.indexOf(key);
      return [...stack.slice(start), key];
    }
    if (visited.has(key)) {
      return [];
    }
    visiting.add(key);
    stack.push(key);
    for (const next of graph.get(key) || []) {
      const cycle = visit(next);
      if (cycle.length) {
        return cycle;
      }
    }
    stack.pop();
    visiting.delete(key);
    visited.add(key);
    return [];
  }

  for (const key of taskKeys) {
    const cycle = visit(key);
    if (cycle.length) {
      return cycle;
    }
  }
  return [];
}

function renderDagSvg(analysis) {
  const nodeWidth = 132;
  const nodeHeight = 34;
  const columnGap = 72;
  const rowGap = 18;
  const grouped = new Map();
  for (const task of analysis.tasks) {
    const level = analysis.levels.get(task.key) || 0;
    if (!grouped.has(level)) {
      grouped.set(level, []);
    }
    grouped.get(level).push(task);
  }
  const maxLevel = Math.max(0, ...grouped.keys());
  const maxRows = Math.max(1, ...[...grouped.values()].map((items) => items.length));
  const width = Math.max(360, (maxLevel + 1) * nodeWidth + maxLevel * columnGap + 32);
  const height = Math.max(96, maxRows * nodeHeight + (maxRows - 1) * rowGap + 32);
  const positions = new Map();
  for (const [level, items] of grouped) {
    items.forEach((task, index) => {
      positions.set(task.key, {
        x: 16 + level * (nodeWidth + columnGap),
        y: 16 + index * (nodeHeight + rowGap),
      });
    });
  }

  const lines = analysis.edges.map(([from, to]) => {
    const a = positions.get(from);
    const b = positions.get(to);
    if (!a || !b) {
      return "";
    }
    return `<line x1="${a.x + nodeWidth}" y1="${a.y + nodeHeight / 2}" x2="${b.x}" y2="${b.y + nodeHeight / 2}" marker-end="url(#dag-arrow)" />`;
  }).join("");
  const nodes = analysis.tasks.map((task) => {
    const pos = positions.get(task.key);
    const cycleClass = analysis.cycle.includes(task.key) ? " cycle" : "";
    return `
      <g class="dag-node${cycleClass}" transform="translate(${pos.x} ${pos.y})">
        <rect width="${nodeWidth}" height="${nodeHeight}" rx="6"></rect>
        <text x="10" y="22">${escapeHtml(task.label)}</text>
      </g>
    `;
  }).join("");

  return `
    <svg class="dag-svg" viewBox="0 0 ${width} ${height}" role="img" aria-label="Sync order DAG">
      <defs>
        <marker id="dag-arrow" viewBox="0 0 8 8" refX="7" refY="4" markerWidth="7" markerHeight="7" orient="auto-start-reverse">
          <path d="M 0 0 L 8 4 L 0 8 z"></path>
        </marker>
      </defs>
      <g class="dag-lines">${lines}</g>
      <g>${nodes}</g>
    </svg>
  `;
}

function makeTaskKey(sourceId, destinationId) {
  return `${sourceId || ""}:${destinationId || ""}`;
}

function ruleEndpointToKey(endpoint) {
  if (!endpoint) {
    return "";
  }
  if (typeof endpoint === "string") {
    return endpoint;
  }
  return makeTaskKey(endpoint.source_id, endpoint.destination_id);
}

function keyToTaskRef(key) {
  const [source_id, destination_id] = String(key || "").split(":");
  return { source_id: source_id || "", destination_id: destination_id || "" };
}

function defaultDestinationSchedule() {
  return {
    mode: "realtime",
    time: "14:00",
    timezone: "local",
    weekday: "monday",
    sync_current_cycle_manually: false,
  };
}

const WEEKDAYS = ["monday", "tuesday", "wednesday", "thursday", "friday", "saturday", "sunday"];

function normalizeWeekday(value) {
  const lower = String(value || "monday").trim().toLowerCase();
  return WEEKDAYS.includes(lower) ? lower : "monday";
}

function normalizeSchedule(schedule) {
  const defaults = defaultDestinationSchedule();
  const next = Object.assign({}, defaults, schedule || {});
  next.time = normalizeScheduleTime((schedule && schedule.time) || defaults.time);
  next.weekday = normalizeWeekday(schedule && schedule.weekday);
  return next;
}

function cloneSchedule(schedule) {
  return Object.assign({}, normalizeSchedule(schedule));
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

function destinationSyncTitle(dst) {
  if (!dst.sync) {
    return "Sync options: global";
  }
  const sync = normalizeNativeSyncConfig(dst.sync);
  const mode = sync.mirror ? "Mirror" : "No mirror";
  const checksum = sync.checksum ? "checksum" : "mtime/size";
  const limit = sync.bwlimit_kbps > 0 ? `${sync.bwlimit_kbps} KB/s` : "unlimited";
  return `Sync options: ${mode}, ${checksum}, ${limit}`;
}

function normalizeScheduleTime(value) {
  const text = String(value || "14:00");
  const match = /^(\d{1,2}):(\d{2})(?::\d{2})?$/.exec(text);
  if (!match) {
    return "14:00";
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
  return `${valueOr(status && status.last_verified_cycle_id, "-")} / ${valueOr(status && status.target_cycle_id, "-")}`;
}

function statusClass(status) {
  if (status && status.status === "green") {
    return "green";
  }
  if (status && status.status === "yellow") {
    return "yellow";
  }
  return "red";
}

function destinationLogIconState(status, activity) {
  if (isPathUnavailable(status)) {
    return { kind: "gray", title: pathUnavailableLabel(status) };
  }
  if (activityIsSyncing(activity)) {
    if (destinationHasError(status) || (activity && activity.error)) {
      return { kind: "red", title: destinationStatusText(status) || "Sync error" };
    }
    return { kind: "yellow", title: activitySyncingLabel(activity) };
  }
  if (status && status.status === "green") {
    return { kind: "green", title: "Destination synced" };
  }
  if (status && status.status === "yellow") {
    const issueCount = status && status.issues ? status.issues.length : 0;
    return {
      kind: "yellow",
      title: `${issueCount} changing file${issueCount === 1 ? "" : "s"}`,
    };
  }
  return { kind: "red", title: destinationStatusText(status) || "Destination needs attention" };
}

function openScheduleModal(schedule, onApply) {
  const draft = cloneSchedule(schedule);
  scheduleEditor = { draft, onApply };
  el.cycleMode.value = draft.mode;
  el.cycleTime.value = formatScheduleTime(draft.time);
  el.cycleWeekday.value = normalizeWeekday(draft.weekday);
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
      time: normalizeScheduleTime(el.cycleTime.value || "14:00"),
      timezone: "local",
      weekday: normalizeWeekday(el.cycleWeekday.value),
      sync_current_cycle_manually: false,
    });
    scheduleEditor.onApply(schedule);
  }
  el.scheduleModal.hidden = true;
  scheduleEditor = null;
}

function openReadmeModal() {
  el.readmeModal.hidden = false;
}

function closeReadmeModal() {
  el.readmeModal.hidden = true;
}

function openConfigModal() {
  updateCfgFromForm();
  el.configView.textContent = JSON.stringify(cfg, null, 2);
  el.configModal.hidden = false;
}

function closeConfigModal() {
  el.configModal.hidden = true;
}

function openSettingsModal(event) {
  preventDefault(event);
  updateCfgFromForm();
  cfg.app = normalizeAppConfig(cfg.app || {});
  const sync = cfg.app.sync;
  el.settingsSyncMirror.checked = sync.mirror;
  el.settingsSyncChecksum.checked = sync.checksum;
  el.settingsSyncDebug.checked = sync.debug_logs;
  el.settingsSyncTimeout.value = String(sync.transfer_timeout_secs || 120);
  el.settingsSyncBwlimit.value = String(sync.bwlimit_kbps || 0);
  el.settingsTcpPool.value = String(cfg.app.tcp_connection_pool_size ?? 100);
  el.settingsModal.hidden = false;
}

function closeSettingsModal() {
  el.settingsModal.hidden = true;
}

async function saveSettings() {
  updateCfgFromForm();
  cfg.app = normalizeAppConfig(cfg.app || {});
  cfg.app.tcp_connection_pool_size = clampInteger(el.settingsTcpPool.value, 0, 10000);
  const baseSync = normalizeNativeSyncConfig(cfg.app.sync || {});
  cfg.app.sync = normalizeNativeSyncConfig({
    ...baseSync,
    mirror: el.settingsSyncMirror.checked,
    checksum: el.settingsSyncChecksum.checked,
    debug_logs: el.settingsSyncDebug.checked,
    transfer_timeout_secs: clampInteger(el.settingsSyncTimeout.value, 1, 86400),
    bwlimit_kbps: clampInteger(el.settingsSyncBwlimit.value, 0, 10_000_000),
  });
  await autoSaveConfig();
  closeSettingsModal();
}

function openDestinationSyncModal(dst, onApply) {
  updateCfgFromForm();
  cfg.app = normalizeAppConfig(cfg.app || {});
  const inherited = !dst.sync;
  const sync = normalizeNativeSyncConfig(dst.sync || cfg.app.sync || {});
  dstSyncEditor = { dst, onApply, baseSync: sync };
  el.dstSyncMirror.checked = sync.mirror;
  el.dstSyncChecksum.checked = sync.checksum;
  el.dstSyncDebug.checked = sync.debug_logs;
  el.dstSyncTimeout.value = String(sync.transfer_timeout_secs || 120);
  el.dstSyncBwlimit.value = String(sync.bwlimit_kbps || 0);
  el.dstSyncReset.disabled = inherited;
  el.dstSyncModal.hidden = false;
}

function closeDestinationSyncModal() {
  el.dstSyncModal.hidden = true;
  dstSyncEditor = null;
}

async function saveDestinationSync() {
  if (!dstSyncEditor || !dstSyncEditor.dst) {
    closeDestinationSyncModal();
    return;
  }
  const baseSync = normalizeNativeSyncConfig(dstSyncEditor.baseSync || {});
  dstSyncEditor.dst.sync = normalizeNativeSyncConfig({
    ...baseSync,
    mirror: el.dstSyncMirror.checked,
    checksum: el.dstSyncChecksum.checked,
    debug_logs: el.dstSyncDebug.checked,
    transfer_timeout_secs: clampInteger(el.dstSyncTimeout.value, 1, 86400),
    bwlimit_kbps: clampInteger(el.dstSyncBwlimit.value, 0, 10_000_000),
  });
  const onApply = dstSyncEditor.onApply;
  closeDestinationSyncModal();
  if (onApply) {
    onApply();
  } else {
    await autoSaveConfig();
  }
}

async function resetDestinationSync() {
  if (!dstSyncEditor || !dstSyncEditor.dst) {
    closeDestinationSyncModal();
    return;
  }
  delete dstSyncEditor.dst.sync;
  const onApply = dstSyncEditor.onApply;
  closeDestinationSyncModal();
  if (onApply) {
    onApply();
  } else {
    await autoSaveConfig();
  }
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
        <div class="issue-meta">cycle ${escapeHtml(valueOr(issue.cycle_id, "-"))} · ${escapeHtml(issue.message || issue.issue_kind || "source_changing")}</div>
      </div>
    `).join("");
  }
  el.issueModal.hidden = false;
}

function closeIssueModal() {
  el.issueModal.hidden = true;
}

const SCAN_DIFF_KINDS = [
  { kind: "add", label: "Add (missing on dst)", field: "to_add" },
  { kind: "update", label: "Update (content differs)", field: "to_update" },
  { kind: "delete", label: "Delete (extra on dst)", field: "to_delete" },
  { kind: "type_mismatch", label: "Type mismatch", field: "type_mismatch" },
];
const SCAN_DIFF_MODAL_CAP = 50;

function openDestinationLogModal(source, dst, mode = "log") {
  dstLogViewer = {
    sourceId: source.id,
    destinationId: dst.id,
    mode,
  };
  renderDestinationLogModal();
  el.dstLogModal.hidden = false;
  // In scan mode, pull the last stored report (from this or a prior session) if
  // we don't already have a fresh one cached from a Scan run just now.
  const key = scanReportKey(source.id, dst.id);
  if (mode === "scan" && !scanReports[key]) {
    invoke("scan_report", { sourceId: source.id, destinationId: dst.id })
      .then((report) => {
        if (report) {
          scanReports[key] = report;
          renderDestinationLogModal();
        }
      })
      .catch(() => {});
  }
}

function closeDestinationLogModal() {
  el.dstLogModal.hidden = true;
  dstLogViewer = null;
}

function renderDestinationLogModal() {
  if (!dstLogViewer || !el.dstLogModal || el.dstLogModal.hidden) {
    return;
  }
  const source = findSourceById(dstLogViewer.sourceId);
  const dst = source && (source.destinations || []).find((item) => item.id === dstLogViewer.destinationId);
  const scanMode = dstLogViewer.mode === "scan";
  if (el.dstLogTitle) {
    el.dstLogTitle.textContent = scanMode ? "Compare" : "Destination Log";
  }
  if (!source || !dst) {
    el.dstLogSummary.textContent = "Destination no longer exists";
    el.dstLogList.innerHTML = "";
    return;
  }
  const status = statusFor(source.id, dst.id);
  const activity = activityForSource(source);
  const runtime = activityRuntime(activity, source);
  const transfer = matchingTransfer(runtime, dst);
  const scan = runtime && runtime.scan;
  const rows = [
    ["Task", `${source.id} -> ${dst.id}`],
    ["Source Path", machinePathLabel(source.machine_id, source.src)],
    ["Destination Path", machinePathLabel(dst.machine_id, dst.path)],
    ["Status", destinationStatusText(status)],
    ["Cycle", cycleDisplay(status)],
  ];
  if (scanMode) {
    // Scan view: only scan progress + report, never the sync runtime/transfer.
    if (scan) {
      rows.push(["Comparing", scan.current_path || scan.root_path || "-"]);
      rows.push(["Entries", String(Number(scan.entries_seen || 0))]);
    }
  } else {
    // Destination Log view: sync runtime/transfer, never the scan report.
    if (activity && activity.error) {
      rows.push(["Runtime", activity.error]);
    } else if (runtime && runtime.syncing) {
      rows.push(["Runtime", runtimeSyncLabel(runtime)]);
    } else {
      rows.push(["Runtime", "idle"]);
    }
    if (transfer) {
      rows.push(["File", transfer.rel_path || "-"]);
      rows.push(["Speed", formatBytesPerSecond(transfer.bytes_per_sec || 0)]);
      rows.push(["Progress", formatTransferProgress(transfer) || "-"]);
    } else {
      rows.push(["Current file", "-"]);
      rows.push(["Speed", "-"]);
    }
  }
  el.dstLogSummary.textContent = "";
  el.dstLogList.innerHTML = rows.map(([key, value]) => `
    <div class="dst-log-row">
      <div class="dst-log-key">${escapeHtml(key)}</div>
      <div class="dst-log-value">${escapeHtml(value || "-")}</div>
    </div>
  `).join("") + (scanMode ? renderScanReportSection(source, dst) : "");
  if (scanMode) {
    el.dstLogList.querySelectorAll("[data-scan-kind]").forEach((button) => {
      button.onclick = () => openScanDiffModal(source, dst, button.getAttribute("data-scan-kind"));
    });
  }
}

function scanKindLabel(kind) {
  switch (kind) {
    case "add": return "+ add";
    case "update": return "~ update";
    case "delete": return "- delete";
    case "type_mismatch": return "! type";
    default: return kind;
  }
}

function formatScanTime(value) {
  if (!value) return "-";
  const date = new Date(value);
  return Number.isNaN(date.getTime()) ? value : date.toLocaleString();
}

function renderScanReportSection(source, dst) {
  const report = scanReports[scanReportKey(source.id, dst.id)];
  if (!report) {
    return "";
  }
  const total = (report.to_add || 0) + (report.to_update || 0) + (report.to_delete || 0) + (report.type_mismatch || 0);
  const title = `<div class="dst-log-section-title">Last compare — ${escapeHtml(formatScanTime(report.scanned_at))} (${total} difference${total === 1 ? "" : "s"})</div>`;
  const diffRows = SCAN_DIFF_KINDS.map(({ kind, label, field }) => {
    const count = report[field] || 0;
    const button = count > 0
      ? `<button class="scan-diff-view" data-scan-kind="${escapeAttr(kind)}">View</button>`
      : "";
    return `
      <div class="dst-log-row">
        <div class="dst-log-key">${escapeHtml(label)}</div>
        <div class="dst-log-value scan-count-value">${escapeHtml(String(count))}${button}</div>
      </div>
    `;
  }).join("");
  const extraRows = [
    ["In sync", report.in_sync],
    ["Source / Dst entries", `${report.source_entries || 0} / ${report.dst_entries || 0}`],
  ].map(([key, value]) => `
    <div class="dst-log-row">
      <div class="dst-log-key">${escapeHtml(key)}</div>
      <div class="dst-log-value">${escapeHtml(String(value ?? 0))}</div>
    </div>
  `).join("");
  const footer = total === 0
    ? `<div class="scan-diff-empty">Source and destination are in sync.</div>`
    : "";
  return title + diffRows + extraRows + footer;
}

function openScanDiffModal(source, dst, kind) {
  const report = scanReports[scanReportKey(source.id, dst.id)];
  if (!report) {
    return;
  }
  const meta = SCAN_DIFF_KINDS.find((item) => item.kind === kind);
  const total = report[meta ? meta.field : ""] || 0;
  const paths = (report.differences || []).filter((diff) => diff.kind === kind);
  const shown = paths.slice(0, SCAN_DIFF_MODAL_CAP);
  el.scanDiffTitle.textContent = meta ? meta.label : "Differences";
  const moreNote = total > shown.length
    ? ` (showing first ${shown.length} of ${total})`
    : ` (${total})`;
  el.scanDiffSummary.textContent = `${source.id} -> ${dst.id}${moreNote}`;
  el.scanDiffModalList.innerHTML = shown.length
    ? shown.map((diff) => `
        <div class="scan-diff-row scan-diff-${escapeAttr(diff.kind)}">
          <span class="scan-diff-ftype">${escapeHtml(diff.file_type || "")}</span>
          <span class="scan-diff-path" title="${escapeAttr(diff.rel_path)}">${escapeHtml(diff.rel_path)}</span>
        </div>
      `).join("")
    : `<div class="scan-diff-empty">No paths.</div>`;
  el.scanDiffModal.hidden = false;
}

function closeScanDiffModal() {
  el.scanDiffModal.hidden = true;
}

function openExcludeModal(source) {
  source.excludes = cleanExcludeList(source.excludes || []);
  excludeEditor = { source };
  renderExcludeModal();
  el.excludeModal.hidden = false;
}

function renderExcludeModal() {
  const source = excludeEditor && excludeEditor.source;
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
  const source = excludeEditor && excludeEditor.source;
  if (!source) {
    return;
  }
  if (!source.src) {
    setMessage("Select source path first");
    return;
  }
  const selected = await pickPath(source.src, { machineId: source.machine_id || "local" });
  if (!selected) {
    return;
  }
  if (machineIdOrLocal(selected.machine_id) !== machineIdOrLocal(source.machine_id)) {
    setMessage("Excluded path must be on the source machine");
    return;
  }
  const relative = pathToSourceRelative(source.src, selected.path);
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

function findSourceById(sourceId) {
  return ((cfg && cfg.source_groups) || []).find((source) => source.id === sourceId);
}

function activityForSourceId(sourceId) {
  const source = findSourceById(sourceId);
  return activityForSource(source);
}

function activityForSource(source) {
  if (!source) {
    return null;
  }
  return activityForMachine(source.machine_id);
}

function activityForMachine(machineIdValue) {
  const machineId = machineIdOrLocal(machineIdValue);
  const key = machineReferenceKey(machineId);
  const machines = (syncActivity && syncActivity.machines) || [];
  if (machineId === "local") {
    return machines.find((machine) => machine.local || machine.machine_id === "local") || {
      machine_id: "local",
      label: "local",
      local: true,
      runtime: runtimeStatus,
      error: null,
    };
  }
  return machines.find((machine) => machineReferenceKey(machine.machine_id) === key)
    || machines.find((machine) => machineReferenceKey(machine.label) === key)
    || null;
}

function activityRuntime(activity, source) {
  if (activity && activity.local) {
    return runtimeStatus || activity.runtime;
  }
  if (activity && activity.runtime) {
    return activity.runtime;
  }
  if (source && machineIdOrLocal(source.machine_id) === "local") {
    return runtimeStatus;
  }
  return null;
}

function activityIsSyncing(activity) {
  return Boolean(activity && activity.runtime && activity.runtime.syncing);
}

function activitySyncingLabel(activity) {
  const label = (activity && activity.label) || "machine";
  return `Sync already in progress on ${label}`;
}

function matchingTransfer(runtime, dst) {
  const transfer = runtime && runtime.transfer;
  if (!transfer || !dst) {
    return null;
  }
  return transfer.destination_id === dst.id ? transfer : null;
}

function destinationStatusText(status) {
  if (!status) {
    return "unknown";
  }
  const reason = String(status.status_reason || "").trim();
  return reason ? `${status.status}: ${reason}` : status.status;
}

function runtimeSyncLabel(runtime) {
  const kind = String((runtime && runtime.sync_kind) || "").trim();
  if (!kind) {
    return "syncing";
  }
  const labels = {
    incremental: "incremental",
    full: "full",
    changed_since: "changed since",
    automatic: "automatic",
  };
  return `syncing (${labels[kind] || kind})`;
}

function sourceHasBlockedDestination(sourceId) {
  return statuses.some((status) =>
    normalizeSourceId(status.source_id) === sourceId && isSyncOrderBlocked(status)
  );
}

function sourceHasUnavailableDestination(sourceId) {
  return statuses.some((status) =>
    normalizeSourceId(status.source_id) === sourceId && isDestinationUnavailable(status)
  );
}

function isSyncOrderBlocked(status) {
  return String((status && status.status_reason) || "").startsWith("blocked_by_sync_order:");
}

function blockedByLabel(status) {
  const reason = String((status && status.status_reason) || "");
  const blocker = reason.slice("blocked_by_sync_order:".length);
  return blocker ? `Blocked by ${blocker}` : "Blocked by sync order";
}

function isDestinationUnavailable(status) {
  return isPathUnavailable(status);
}

function isPathUnavailable(status) {
  if (!status || status.status !== "red") {
    return false;
  }
  const reason = String(status.status_reason || "").toLowerCase();
  return [
    "source path does not exist",
    "source path is not a directory",
    "source offline",
    "source unavailable",
    "destination path does not exist",
    "destination path is not a directory",
    "destination file path is a directory",
    "destination file path has no parent",
    "destination is not writable",
    "destination offline",
    "destination unavailable",
    "no such file or directory",
    "permission denied",
    "read-only file system",
    "transport endpoint is not connected",
    "stale file handle",
    "input/output error",
  ].some((text) => reason.includes(text));
}

function destinationHasError(status) {
  if (!status) {
    return false;
  }
  if (status.status === "red") {
    return !isPathUnavailable(status);
  }
  const reason = String(status.status_reason || "").toLowerCase();
  return reason.includes("failed") || reason.includes("error");
}

function unavailableLabel(status) {
  const reason = String((status && status.status_reason) || "").trim();
  return reason ? `Destination unavailable: ${reason}` : "Destination unavailable";
}

function pathUnavailableLabel(status) {
  const reason = String((status && status.status_reason) || "").trim();
  return reason ? `Path unavailable: ${reason}` : "Path unavailable";
}

function renderFolderMachineOptions() {
  if (!el.folderMachine) {
    return;
  }
  const machines = machineStatus && machineStatus.machines && machineStatus.machines.length
    ? machineStatus.machines
    : normalizeMachines((cfg && cfg.machines) || []);
  const selectable = machines.filter((machine) => machine.online !== false);
  el.folderMachine.innerHTML = selectable.map((machine) => `
    <option value="${escapeAttr(machine.id)}">${escapeHtml(machine.id === "local" ? "local" : machinePrimaryName(machine))}${machine.online === false ? " (offline)" : ""}</option>
  `).join("");
  if (folderPicker) {
    if (!selectable.some((machine) => machineReferenceKey(machine.id) === machineReferenceKey(folderPicker.machineId))) {
      folderPicker.machineId = selectable[0] ? selectable[0].id : "local";
    }
    el.folderMachine.value = folderPicker.machineId || "local";
  }
}

function defaultPathForMachine(machineId = "local") {
  const machine = findMachineByReference(machineId);
  return String((machine && machine.os) || "").toLowerCase() === "windows" ? "C:\\" : "/";
}

async function pickPath(startPath = "/", options = {}) {
  return new Promise(async (resolve) => {
    folderPicker = {
      resolve,
      path: startPath || "/",
      machineId: machineIdOrLocal(options.machineId),
      requestId: 0,
      validate: options.validate || null,
      showAddDirectory: options.showAddDirectory === true,
      addDirectory: options.addDirectory === true,
    };
    if (el.folderAddDirectoryRow && el.folderAddDirectory) {
      el.folderAddDirectoryRow.hidden = !folderPicker.showAddDirectory;
      el.folderAddDirectory.checked = folderPicker.addDirectory;
    }
    setFolderError("");
    renderFolderMachineOptions();
    el.folderModal.hidden = false;
    await loadPath(folderPicker.path);
  });
}

async function loadPath(path) {
  if (!folderPicker) {
    return;
  }
  const machineId = machineIdOrLocal(folderPicker.machineId);
  const requestId = (folderPicker.requestId || 0) + 1;
  folderPicker.requestId = requestId;
  folderPicker.machineId = machineId;
  folderPicker.path = path || defaultPathForMachine(machineId);
  folderPicker.parent = null;
  el.folderPath.textContent = folderPicker.path;
  el.folderList.innerHTML = `<div class="empty">Loading...</div>`;
  try {
    const result = await invoke("browse_paths", {
      path: folderPicker.path,
      machineId,
      machine_id: machineId,
    });
    if (
      !folderPicker ||
      folderPicker.requestId !== requestId ||
      machineIdOrLocal(folderPicker.machineId) !== machineId
    ) {
      return;
    }
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
          closeFolderModal({ machine_id: folderPicker.machineId, path: entry.path });
        }
      };
      el.folderList.appendChild(row);
    }
    if (!result.entries.length) {
      el.folderList.innerHTML = `<div class="empty">No entries</div>`;
    }
  } catch (error) {
    if (!folderPicker || folderPicker.requestId !== requestId) {
      return;
    }
    el.folderList.innerHTML = `<div class="empty">Failed to load path</div>`;
    setMessage(String(error));
  }
}

function closeFolderModal(value) {
  if (value && folderPicker && folderPicker.validate) {
    const error = folderPicker.validate(value);
    if (error) {
      setFolderError(error);
      return;
    }
  }
  el.folderModal.hidden = true;
  setFolderError("");
  if (folderPicker) {
    folderPicker.resolve(value ? {
      machine_id: value.machine_id || folderPicker.machineId,
      path: value.path || value,
      add_directory: folderPicker.showAddDirectory && el.folderAddDirectory
        ? el.folderAddDirectory.checked
        : undefined,
    } : null);
    folderPicker = null;
  }
}

function setFolderError(text) {
  el.folderError.textContent = text || "";
  el.folderError.hidden = !text;
}

function updateCfgFromForm() {
  delete cfg.schedule;
  cfg.machines = normalizeMachines(cfg.machines || []);
  cfg.source_groups = (cfg.source_groups || []).map((source) => {
    source.machine_id = machineIdOrLocal(source.machine_id);
    source.src = trimPathValue(source.src);
    source.add_directory = source.add_directory === true;
    source.excludes = cleanExcludeList(source.excludes || []);
    source.enabled = true;
    source.mode = "mirror";
    source.destinations = (source.destinations || []).map((dst) => {
      dst.machine_id = machineIdOrLocal(dst.machine_id);
      dst.path = trimPathValue(dst.path);
      dst.enabled = true;
      dst.schedule = normalizeSchedule(dst.schedule);
      dst.sync = normalizeOptionalNativeSyncConfig(dst.sync);
      return dst;
    }).filter((dst) => dst.path);
    source.destinations = dedupeDestinationsByPath(source.destinations);
    return source;
  }).filter((source) => source.src);
  cfg.sync_order = cleanSyncOrder(cfg.sync_order || []);
}

function dedupeDestinationsByPath(destinations) {
  const seen = new Set();
  const cleaned = [];
  for (const dst of destinations || []) {
    const path = normalizeAbsolutePath(dst.path);
    const key = `${machineIdOrLocal(dst.machine_id)}:${path}`;
    if (!path || seen.has(key)) {
      continue;
    }
    seen.add(key);
    dst.path = path;
    cleaned.push(dst);
  }
  return cleaned;
}

function trimPathValue(value) {
  return String(valueOr(value, "")).trim();
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
  let path = String(valueOr(value, "")).trim().replace(/\\/g, "/");
  path = path.replace(/\/+/g, "/").replace(/^\/+/, "").replace(/\/+$/, "");
  const parts = path.split("/").filter((part) => part && part !== ".");
  if (!parts.length || parts.includes("..")) {
    return "";
  }
  return parts.join("/");
}

function normalizeAbsolutePath(value) {
  let path = String(valueOr(value, "")).trim().replace(/\\/g, "/");
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
  await runBusy("Checking changes...", async () => {
    await saveConfig();
    statuses = await invoke("sync_now");
    setMessage("");
    render();
  }, { showMainMessage: false });
}

async function runBusy(message, fn, options = {}) {
  if (busy) return;
  const showMainMessage = options.showMainMessage !== false;
  try {
    setBusy(true);
    statusBusyMessage = message || "";
    if (showMainMessage) {
      setMessage(message || "");
    } else {
      updateStatusBar();
    }
    await fn();
  } catch (error) {
    setMessage(String(error));
  } finally {
    statusBusyMessage = "";
    setBusy(false);
    updateStatusBar();
  }
}

function setBusy(nextBusy) {
  busy = nextBusy;
  el.config.disabled = busy;
  el.statusConfig.disabled = busy;
  el.settingsSave.disabled = busy;
  el.refresh.disabled = busy;
  updateStatusUi();
}

function setMessage(text) {
  statusMessage = text || "";
  updateStatusBar();
}

function destinationSyncStatusMessage(source, mode) {
  if (mode === "changed_since") {
    return "Scanning source changes...";
  }
  if (mode !== "full") {
    return "Checking changes...";
  }
  return "Scanning...";
}

function displayPath(value) {
  const path = String(valueOr(value, ""));
  return path.replace(/^\\\\\?\\UNC\\/i, "\\\\").replace(/^\\\\\?\\/, "");
}

function compactStatusPath(value, maxChars) {
  const path = displayPath(value);
  if (path.length <= maxChars) {
    return path;
  }
  const separator = path.includes("\\") ? "\\" : "/";
  let prefix = "";
  let rest = path;
  const drive = path.match(/^[A-Za-z]:\\/);
  if (drive) {
    prefix = drive[0];
    rest = path.slice(prefix.length);
  } else if (path.startsWith("\\\\")) {
    const parts = path.slice(2).split(/[\\/]+/);
    if (parts.length >= 2) {
      prefix = `\\\\${parts[0]}\\${parts[1]}\\`;
      rest = parts.slice(2).join(separator);
    }
  } else if (path.startsWith("/")) {
    prefix = "/";
    rest = path.slice(1);
  }

  const parts = rest.split(/[\\/]+/).filter(Boolean);
  if (parts.length <= 2) {
    return `${path.slice(0, Math.max(0, maxChars - 3))}...`;
  }

  let headCount = Math.min(4, parts.length - 1);
  let tailCount = Math.min(3, parts.length - headCount);
  let compact = renderCompactPath(prefix, parts, separator, headCount, tailCount);
  while (compact.length > maxChars && headCount > 1) {
    headCount -= 1;
    compact = renderCompactPath(prefix, parts, separator, headCount, tailCount);
  }
  while (compact.length > maxChars && tailCount > 1) {
    tailCount -= 1;
    compact = renderCompactPath(prefix, parts, separator, headCount, tailCount);
  }
  if (compact.length <= maxChars) {
    return compact;
  }
  const tail = parts[parts.length - 1];
  const headBudget = Math.max(0, maxChars - tail.length - separator.length - 3);
  return `${path.slice(0, headBudget)}...${separator}${tail}`;
}

function renderCompactPath(prefix, parts, separator, headCount, tailCount) {
  const head = parts.slice(0, headCount).join(separator);
  const tail = parts.slice(parts.length - tailCount).join(separator);
  const left = head ? `${prefix}${head}` : prefix.replace(/[\\/]$/, "");
  return `${left}${separator}...${separator}${tail}`;
}

function formatTransferProgress(transfer) {
  const total = Number(transfer.total_bytes || 0);
  if (!total) {
    return "";
  }
  const transferred = Number(transfer.transferred_bytes || 0);
  const percent = Math.min(100, Math.max(0, (transferred / total) * 100));
  return `${formatBytes(transferred)} / ${formatBytes(total)} (${percent.toFixed(0)}%)`;
}

function formatBytesPerSecond(value) {
  return `${formatBytes(value)}/s`;
}

function formatBytes(value) {
  let size = Math.max(0, Number(value || 0));
  const units = ["B", "KB", "MB", "GB", "TB"];
  let unit = 0;
  while (size >= 1024 && unit < units.length - 1) {
    size /= 1024;
    unit += 1;
  }
  const digits = unit === 0 || size >= 100 ? 0 : size >= 10 ? 1 : 2;
  return `${size.toFixed(digits)} ${units[unit]}`;
}

function defaultUiConfig() {
  return {
    app: normalizeAppConfig({}),
    machines: [],
    source_groups: [],
    sync_order: [],
  };
}

function valueOr(value, fallback) {
  return value === null || value === undefined ? fallback : value;
}

function clampInteger(value, min, max) {
  const parsed = Number.parseInt(String(value || "0"), 10);
  if (!Number.isFinite(parsed)) {
    return min;
  }
  return Math.min(max, Math.max(min, parsed));
}

function preventDefault(event) {
  if (event && event.preventDefault) {
    event.preventDefault();
  }
}

function getTauriInvoke() {
  if (
    window.__TAURI__ &&
    window.__TAURI__.core &&
    typeof window.__TAURI__.core.invoke === "function"
  ) {
    return window.__TAURI__.core.invoke;
  }
  return null;
}

function escapeHtml(value) {
  return String(valueOr(value, "")).replace(/[&<>"']/g, (ch) => ({
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

function bindButtonClick(button, handler) {
  button.addEventListener("click", (event) => {
    event.preventDefault();
    handler(event);
  });
}

bindButtonClick(el.readme, openReadmeModal);
bindButtonClick(el.config, openConfigModal);
bindButtonClick(el.statusConfig, openSettingsModal);
bindButtonClick(el.refresh, () => runBusy("", loadAll));
bindButtonClick(el.machineStatus, openMachineModal);
window.autoSyncOpenMachines = openMachineModal;
window.autoSyncCloseMachines = (event) => {
  preventDefault(event);
  closeMachineModal();
};
window.autoSyncRefreshMachines = (event) => {
  preventDefault(event);
  return discoverMachines();
};

el.folderClose.onclick = () => closeFolderModal(null);
el.folderSelect.onclick = () => closeFolderModal(folderPicker ? {
  machine_id: folderPicker.machineId,
  path: folderPicker.path,
} : null);
el.folderUp.onclick = () => {
  if (folderPicker && folderPicker.parent) {
    loadPath(folderPicker.parent);
  }
};
el.folderMachine.onchange = () => {
  if (!folderPicker) {
    return;
  }
  folderPicker.machineId = el.folderMachine.value || "local";
  folderPicker.path = defaultPathForMachine(folderPicker.machineId);
  folderPicker.parent = null;
  setFolderError("");
  loadPath(folderPicker.path);
};

el.scheduleClose.onclick = () => closeScheduleModal(false);
el.scheduleApply.onclick = () => closeScheduleModal(true);
el.cycleMode.onchange = updateScheduleModalFields;
el.readmeClose.onclick = closeReadmeModal;
el.configClose.onclick = closeConfigModal;
el.settingsClose.onclick = closeSettingsModal;
el.settingsSave.onclick = () => saveSettings().catch((error) => setMessage(String(error)));
el.dstSyncClose.onclick = closeDestinationSyncModal;
el.dstSyncSave.onclick = () => saveDestinationSync().catch((error) => setMessage(String(error)));
el.dstSyncReset.onclick = () => resetDestinationSync().catch((error) => setMessage(String(error)));
el.machineClose.onclick = closeMachineModal;
el.machineDiscover.onclick = () => discoverMachines().catch((error) => setMessage(String(error)));
el.machineAdd.onclick = () => addMachine().catch((error) => setMessage(String(error)));
el.issueClose.onclick = closeIssueModal;
el.dstLogClose.onclick = closeDestinationLogModal;
el.scanDiffClose.onclick = closeScanDiffModal;
el.excludeClose.onclick = closeExcludeModal;
el.excludeAdd.onclick = () => addExcludePath().catch((error) => setMessage(String(error)));

window.addEventListener("error", (event) => {
  setMessage(event.message || String(event.error || event));
});
window.addEventListener("unhandledrejection", (event) => {
  setMessage(String(event.reason || "Unhandled promise rejection"));
});

loadAll().catch((error) => setMessage(String(error)));

async function invokeBackend(command, payload = {}) {
  const tauriInvoke = getTauriInvoke();
  if (tauriInvoke) {
    return await tauriInvoke(command, payload);
  }
  return await invokeWeb(command, payload);
}

async function invokeWeb(command, payload = {}) {
  const routes = {
    get_config: ["GET", "/api/config"],
    save_config_command: ["POST", "/api/config"],
    get_machines: ["GET", "/api/machines"],
    discover_machines: ["GET", "/api/machines/discover"],
    add_machine: ["POST", "/api/machines"],
    remove_machine: ["DELETE", "/api/machines"],
    get_status: ["GET", "/api/status"],
    get_runtime_status: ["GET", "/api/runtime-status"],
    get_sync_activity: ["GET", "/api/sync-activity"],
    sync_now: ["POST", "/api/sync-now"],
    sync_source_now: ["POST", "/api/sync-source-now"],
    sync_destination_now: ["POST", "/api/sync-destination-now"],
    scan_destination_now: ["POST", "/api/scan-destination-now"],
    scan_report: ["GET", "/api/scan-report"],
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
    const machineId = payload.machineId || payload.machine_id || "local";
    url = `${path}?path=${encodeURIComponent(payload.path || "/")}&machine_id=${encodeURIComponent(machineId)}`;
  } else if (command === "scan_report") {
    const sourceId = payload.sourceId || payload.source_id;
    const destinationId = payload.destinationId || payload.destination_id;
    url = `${path}?source_id=${encodeURIComponent(sourceId)}&destination_id=${encodeURIComponent(destinationId)}`;
  } else if (command === "remove_machine") {
    const machineId = payload.machineId || payload.machine_id;
    url = `${path}/${encodeURIComponent(machineId || "")}`;
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
        mode: payload.mode || payload.syncMode || payload.sync_mode || "incremental",
      });
    } else if (command === "scan_destination_now") {
      options.body = JSON.stringify({
        source_id: payload.sourceId || payload.source_id,
        destination_id: payload.destinationId || payload.destination_id,
      });
    } else if (command === "add_machine") {
      options.body = JSON.stringify(payload.machine);
    } else {
      options.body = JSON.stringify(payload);
    }
  }

  const isTauriAssetOrigin = location.hostname === "tauri.localhost";
  const apiBase = (location.protocol === "http:" || location.protocol === "https:") && !isTauriAssetOrigin
    ? ""
    : "http://127.0.0.1:18765";
  const response = await fetch(`${apiBase}${url}`, options);
  if (!response.ok) {
    throw new Error(await response.text());
  }
  return await response.json();
}
