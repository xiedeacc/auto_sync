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
// The Info modal's "Type"/"Summary" rows come from the last task-log entry for
// the open destination; refreshed on a timer while the modal is open.
let dstLogTask = null;
let dstLogTaskTimer = null;
const scanReports = {};
const scanPending = {};
let checkingScans = false;

function scanReportKey(sourceId, destinationId) {
  return `${sourceId}|${destinationId}`;
}

// Fallback only: a pending compare that the task log shows neither running nor
// terminally failed (it silently never started, or was consumed by a repair)
// must not pin the row's stop button forever. Once nothing has looked alive
// for this long, give up. This is NOT the primary running check — that comes
// authoritatively from the task log (see checkPendingScans).
const SCAN_PENDING_GRACE_MS = 30000;

// Newest task of `kind` for (sourceId, destinationId) across every machine's
// task log. destination_id may be a comma-joined list for a cross-machine
// cycle, so match by membership. Returns null when none is found.
function newestTaskFor(machines, sourceId, destinationId, kind) {
  let best = null;
  for (const machine of machines || []) {
    for (const task of machine.tasks || []) {
      if (task.source_id !== sourceId) continue;
      if (kind && task.kind !== kind) continue;
      const ids = String(task.destination_id || "").split(",").map((id) => id.trim());
      if (!ids.includes(destinationId)) continue;
      if (!best || Date.parse(task.started_at) > Date.parse(best.started_at)) {
        best = task;
      }
    }
  }
  return best;
}

// While a background compare is pending, follow its authoritative lifecycle in
// the task log (get_all_tasks, which aggregates this machine AND every managed
// runtime machine) rather than guessing from runtime scan attribution. A
// compare for a remote-managed source executes on the destination's machine,
// so its "running" state never appears in this UI's local runtime scan list —
// the old heuristic then wrongly declared it "no longer running" after 30s
// while it was in fact still running elsewhere. The task log is the source of
// truth for running/ended, so we ask it directly.
async function checkPendingScans() {
  if (checkingScans) return;
  const entries = Object.entries(scanPending);
  if (!entries.length) return;
  checkingScans = true;
  try {
    let taskMachines = null;
    try {
      taskMachines = await invoke("get_all_tasks", { limit: 50 });
    } catch (_error) {
      taskMachines = null; // fall back to the runtime-scan signal this tick
    }
    for (const [key, info] of entries) {
      const source = findSourceById(info.sourceId);
      const dst = source
        && (source.destinations || []).find((item) => item.id === info.destinationId);
      // Authoritative: is a compare for this pair still running on ANY machine?
      const compareTask = taskMachines
        ? newestTaskFor(taskMachines, info.sourceId, info.destinationId, "compare")
        : null;
      const authoritativeRunning = Boolean(compareTask && compareTask.status === "running");
      if (authoritativeRunning || (source && dst && compareScanRunning(source, dst))) {
        info.lastSeenRunning = Date.now();
      }
      let report;
      try {
        report = await invoke("scan_report", {
          sourceId: info.sourceId,
          destinationId: info.destinationId,
        });
      } catch (_error) {
        continue;
      }
      if (!report || !report.scanned_at || report.scanned_at === info.prev) {
        // The task log says it is still running: keep waiting, never give up.
        if (authoritativeRunning) continue;
        // Ended without producing a new report: if the task log recorded a
        // terminal failure for our compare, surface it precisely instead of
        // the generic timeout message. Guard on started_at (with LAN clock
        // skew tolerance) so a stale earlier failure is not mistaken for ours.
        const terminalFailure = compareTask
          && compareTask.status !== "running"
          && compareTask.status !== "success"
          && Date.parse(compareTask.started_at) >= (info.startedAt || 0) - 60000;
        if (terminalFailure) {
          delete scanPending[key];
          setTransientMessage(
            `Compare ${info.sourceId} -> ${info.destinationId} ${compareTask.status}`
              + (compareTask.error ? `: ${compareTask.error}` : ""),
            15000,
          );
          updateDstControls();
          continue;
        }
        // No running compare and no terminal failure on record: fall back to
        // the grace window (measured from when we last saw it alive) to retire
        // a compare that silently never started.
        const lastAlive = info.lastSeenRunning || info.startedAt || 0;
        if (Date.now() - lastAlive > SCAN_PENDING_GRACE_MS) {
          delete scanPending[key];
          if (statusMessage.startsWith("Compare running")) {
            setTransientMessage(
              `Compare ${info.sourceId} -> ${info.destinationId} is no longer running`
                + " (no new report was produced)",
            );
          }
          updateDstControls();
        }
        continue;
      }
      if (report && report.scanned_at && report.scanned_at !== info.prev) {
        scanReports[key] = report;
        delete scanPending[key];
        // The "Compare running" notice must not outlive the compare: replace
        // it with the outcome (auto-clearing) once the report lands.
        if (statusMessage.startsWith("Compare running")) {
          if (report.error) {
            setTransientMessage(
              `Compare ${info.sourceId} -> ${info.destinationId} failed: ${report.error}`,
              15000,
            );
          } else {
            const total = Number(report.to_add || 0) + Number(report.to_update || 0)
              + Number(report.to_delete || 0) + Number(report.type_mismatch || 0)
              + Number(report.metadata || 0);
            setTransientMessage(
              `Compare ${info.sourceId} -> ${info.destinationId} finished: `
                + `${total} difference${total === 1 ? "" : "s"}`,
              15000,
            );
          }
        }
        updateDstControls();
        renderDestinationLogModal();
        refreshStatusOnly().catch(() => {});
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
  statusConfigError: document.getElementById("status-config-error"),
  statusBuild: document.getElementById("status-build"),
  tasks: document.getElementById("tasks"),
  tasksModal: document.getElementById("tasks-modal"),
  tasksClose: document.getElementById("tasks-close"),
  tasksSummary: document.getElementById("tasks-summary"),
  tasksList: document.getElementById("tasks-list"),
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
  settingsSyncZfsDiff: document.getElementById("settings-sync-zfsdiff"),
  settingsSyncDebug: document.getElementById("settings-sync-debug"),
  settingsAutostart: document.getElementById("settings-autostart"),
  settingsCloseToTray: document.getElementById("settings-close-to-tray"),
  settingsSyncTimeout: document.getElementById("settings-sync-timeout"),
  settingsSyncBwlimit: document.getElementById("settings-sync-bwlimit"),
  settingsTcpPool: document.getElementById("settings-tcp-pool"),
  dstSyncModal: document.getElementById("dst-sync-modal"),
  dstSyncClose: document.getElementById("dst-sync-close"),
  dstSyncSave: document.getElementById("dst-sync-save"),
  dstSyncReset: document.getElementById("dst-sync-reset"),
  dstSyncMirror: document.getElementById("dst-sync-mirror"),
  dstSyncChecksum: document.getElementById("dst-sync-checksum"),
  dstSyncZfsDiff: document.getElementById("dst-sync-zfsdiff"),
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
  updateDstControls();
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

// Last status_epoch seen on the 1s runtime poll. The epoch bumps when local
// sync state changes or a peer pushes a status notification; a change means
// destination statuses moved, so refresh them immediately instead of waiting
// out the slower status poll.
let lastStatusEpoch = null;

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
    const epoch = runtimeStatus && runtimeStatus.status_epoch;
    if (typeof epoch === "number" && epoch !== lastStatusEpoch) {
      const first = lastStatusEpoch === null;
      lastStatusEpoch = epoch;
      if (!first) {
        refreshStatusOnly().catch(() => {});
      }
    }
  } catch (_) {
    runtimeStatus = null;
    updateStatusBar();
  } finally {
    runtimeStatusPolling = false;
  }
}

function sourceRestartNotice(sourceId) {
  const view = (statuses || []).find(
    (status) => status.source_id === sourceId && status.restart_notice_at,
  );
  return view
    ? { at: view.restart_notice_at, gapStart: view.restart_gap_started || null }
    : null;
}

function updateStatusUi() {
  for (const group of el.sourcePanel.querySelectorAll(".source-group")) {
    const sourceId = group.dataset.sourceId;
    const latest = group.querySelector(".source-latest-cycle");
    if (latest) {
      latest.value = sourceLatestCycle(sourceId);
    }
    const restartButton = group.querySelector('[data-action="restart-notice"]');
    if (restartButton) {
      const notice = sourceRestartNotice(sourceId);
      restartButton.hidden = !notice;
      if (notice) {
        restartButton.title =
          `auto_sync on this source's machine restarted at ${formatScanTime(notice.at)}` +
          `${notice.gapStart ? `; changes since ${formatScanTime(notice.gapStart)}` : "; changes made while it was down"}` +
          " may be unrecorded. Run Compare or Full on this source to verify" +
          " — or click to dismiss this notice.";
      }
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
    const removeSource = group.querySelector('[data-action="remove-source"]');
    if (removeSource) {
      // Match the per-destination delete guard: block removing a source while
      // ANY of its destinations has a task running (sync or compare), so the
      // config can't be yanked out from under an in-flight pass. Stop it first.
      const source = findSourceById(sourceId);
      const active = !!source
        && (source.destinations || []).some((dst) => dstActivity(source, dst).active);
      removeSource.disabled = busy || active;
      removeSource.title = active
        ? "A task is running for this source — stop it first"
        : "Remove source";
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
    const paused = !!(configDestination(sourceId, destinationId) || {}).paused;
    const dot = row.querySelector(".dot");
    if (dot) {
      const dotClass = statusClass(status);
      const issueCount = status && status.issues ? status.issues.length : 0;
      const dotTitle = status && status.status_reason === "paused"
        ? "Paused"
        : (dotClass === "yellow"
          ? `${issueCount} changing file${issueCount === 1 ? "" : "s"}`
          : ((status && status.status) || "red"));
      dot.className = `dot ${dotClass}`;
      dot.title = dotTitle;
      dot.setAttribute("aria-label", dotTitle);
    }
    const syncSelect = row.querySelector('[data-action="sync-dst"]');
    if (syncSelect) {
      const blocked = isSyncOrderBlocked(status);
      const activity = activityForSourceId(sourceId);
      const syncing = activityIsSyncing(activity);
      syncSelect.disabled = busy || blocked || unavailable || syncing || paused;
      syncSelect.title = paused
        ? "Paused — resume to sync"
        : (unavailable
          ? unavailableLabel(status)
          : (blocked ? blockedByLabel(status) : (syncing ? activitySyncingLabel(activity) : "Sync")));
    }
    const logButton = row.querySelector('[data-action="show-dst-log"]');
    if (logButton) {
      const activity = activityForSourceId(sourceId);
      const iconState = destinationLogIconState(status, activity);
      logButton.className = `destination-log-button icon destination-log-${iconState.kind}`;
      logButton.title = iconState.title;
      logButton.setAttribute("aria-label", iconState.title);
    }
    const repairButton = row.querySelector('[data-action="repair-scan"]');
    if (repairButton) {
      const diffs = Number(status && status.scan_differences) || 0;
      repairButton.hidden = diffs === 0;
      repairButton.disabled = paused;
      if (diffs > 0) {
        const when = (status && status.scan_at) || "";
        repairButton.title =
          `Compare found ${diffs} difference${diffs === 1 ? "" : "s"}` +
          `${when ? ` (${when})` : ""} — click to sync just these paths`;
      }
    }
  }
  updateDstControls();
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
    renderScanLikeStatus(scanStatusLabel(lastRuntimeScan), lastRuntimeScan);
  } else if (busy && lastRuntimeScan) {
    renderScanLikeStatus(scanStatusLabel(lastRuntimeScan), lastRuntimeScan);
  } else {
    if (!busy) {
      lastRuntimeScan = null;
    }
    const message = statusBusyMessage || statusMessage || "Ready";
    el.statusText.textContent = message;
    el.statusText.title = message;
  }

  updateConfigErrorIndicator();

  const build = runtimeStatus && runtimeStatus.build;
  const commit = (build && build.commit) || "unknown";
  const time = (build && build.commit_time_beijing) || "unknown";
  const buildText = `${commit} · ${time}`;
  el.statusBuild.textContent = buildText;
  el.statusBuild.title = buildText;
}

function updateConfigErrorIndicator() {
  if (!el.statusConfigError) {
    return;
  }
  const errors = (runtimeStatus && runtimeStatus.config_errors) || [];
  if (!errors.length) {
    el.statusConfigError.hidden = true;
    el.statusConfigError.textContent = "";
    el.statusConfigError.title = "";
    return;
  }
  const label =
    errors.length === 1 ? "1 config issue" : `${errors.length} config issues`;
  el.statusConfigError.hidden = false;
  el.statusConfigError.textContent = `⚠ ${label}`;
  el.statusConfigError.title = errors.join("\n");
}

function scanStatusLabel(scan) {
  return scan && scan.kind === "compare" ? "Comparing" : "Checking changes";
}

// Activity attributable to one destination row: a compare pending from this
// UI, or the scan/transfer running on the source's execution machine and
// scoped to this destination. Drives the row's stop-button swap.
function dstActivity(source, dst) {
  if (!source || !dst) {
    return { active: false, scope: null };
  }
  if (scanPending[scanReportKey(source.id, dst.id)]) {
    return { active: true, scope: "compare" };
  }
  const runtime = activityRuntime(activityForSource(source), source);
  if (!runtime) {
    return { active: false, scope: null };
  }
  for (const scan of runtimeScans(runtime)) {
    if (scan.source_id === source.id && scan.destination_id === dst.id) {
      return { active: true, scope: scan.kind === "compare" ? "compare" : "sync" };
    }
  }
  const transfer = runtime.transfer;
  if (
    transfer &&
    transfer.destination_id === dst.id &&
    (!transfer.source_id || transfer.source_id === source.id)
  ) {
    return { active: true, scope: "sync" };
  }
  return { active: false, scope: null };
}

// Every live walk on a runtime: newer daemons report all concurrent walks in
// `scans`; fall back to the single `scan` slot for older peers.
function runtimeScans(runtime) {
  if (!runtime) {
    return [];
  }
  if (Array.isArray(runtime.scans) && runtime.scans.length) {
    return runtime.scans;
  }
  return runtime.scan ? [runtime.scan] : [];
}

// True while a compare walk for this destination is visible on the source's
// execution machine (used to detect a pending compare that silently ended).
function compareScanRunning(source, dst) {
  const runtime = activityRuntime(activityForSource(source), source);
  return runtimeScans(runtime).some(
    (scan) =>
      scan.kind === "compare"
      && scan.source_id === source.id
      && scan.destination_id === dst.id,
  );
}

function configDestination(sourceId, destinationId) {
  const source = findSourceById(sourceId);
  return source && (source.destinations || []).find((item) => item.id === destinationId);
}

// The pause/resume button is a control over an in-progress sync, not a
// standing config toggle: it appears only while a sync for this destination
// is actually running (as a pause ⏸), or while the destination is paused (as
// a resume ▶, the only way back). Idle-and-not-paused shows nothing — there
// is no sync to pause. Deleting is blocked while a task runs.
function updateDstControls() {
  for (const row of el.sourcePanel.querySelectorAll(".destination-row")) {
    const source = findSourceById(row.dataset.sourceId);
    const dst = source
      && (source.destinations || []).find((item) => item.id === row.dataset.destinationId);
    const removeButton = row.querySelector('[data-action="remove-dst"]');
    if (!removeButton) {
      continue;
    }
    const act = dstActivity(source, dst);
    const syncActive = act.active && act.scope !== "compare";
    const pauseButton = row.querySelector('[data-action="pause-dst"]');
    if (pauseButton) {
      // The button's icon/style already reflect dst.paused from the last
      // render; here we only decide whether it is shown at all.
      pauseButton.hidden = !((dst && dst.paused) || syncActive);
    }
    removeButton.disabled = act.active;
    removeButton.title = act.active
      ? (act.scope === "compare"
        ? "A compare is running — wait for it to finish"
        : "A sync is running — pause it first")
      : "Remove destination";
  }
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
    <span class="source-drag-handle" draggable="true" title="Drag to reorder" aria-label="Drag to reorder source">⠿</span>
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
          <span class="source-cycle-cell">
            <button class="restart-notice-button icon" data-action="restart-notice" title="" aria-label="Daemon restart notice" hidden><span class="destination-log-icon" aria-hidden="true">i</span></button>
            <input class="source-latest-cycle" value="${escapeAttr(sourceLatestCycle(source.id))}" readonly>
          </span>
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
  setupSourceReorder(stack);
}

// Drag-and-drop reordering of the source cards. Only the grip handle is
// draggable (so inputs/buttons stay usable); on drop we move the source within
// cfg.source_groups, renumber `order`, persist, and re-render.
function setupSourceReorder(stack) {
  let draggingId = null;
  const clearMarkers = () => {
    stack.querySelectorAll(".source-group").forEach((card) => {
      card.classList.remove("dragging", "drop-before", "drop-after");
    });
  };
  stack.querySelectorAll(".source-drag-handle").forEach((handle) => {
    const card = handle.closest(".source-group");
    handle.addEventListener("dragstart", (event) => {
      draggingId = card.dataset.sourceId;
      card.classList.add("dragging");
      if (event.dataTransfer) {
        event.dataTransfer.effectAllowed = "move";
        try { event.dataTransfer.setData("text/plain", draggingId); } catch (_e) {}
      }
    });
    handle.addEventListener("dragend", () => {
      draggingId = null;
      clearMarkers();
    });
  });
  const dropTarget = (event) => {
    const over = event.target.closest && event.target.closest(".source-group");
    if (!over || over.dataset.sourceId === draggingId) return null;
    const rect = over.getBoundingClientRect();
    return { over, before: event.clientY < rect.top + rect.height / 2 };
  };
  stack.addEventListener("dragover", (event) => {
    if (!draggingId) return;
    event.preventDefault();
    if (event.dataTransfer) event.dataTransfer.dropEffect = "move";
    stack.querySelectorAll(".source-group").forEach((card) => {
      card.classList.remove("drop-before", "drop-after");
    });
    const target = dropTarget(event);
    if (target) target.over.classList.add(target.before ? "drop-before" : "drop-after");
  });
  stack.addEventListener("drop", (event) => {
    if (!draggingId) return;
    event.preventDefault();
    const target = dropTarget(event);
    const movedId = draggingId;
    draggingId = null;
    clearMarkers();
    if (target) reorderSource(movedId, target.over.dataset.sourceId, target.before);
  });
}

async function reorderSource(movedId, targetId, before) {
  const groups = cfg.source_groups;
  const from = groups.findIndex((source) => source.id === movedId);
  if (from < 0) return;
  const [moved] = groups.splice(from, 1);
  let insertAt = groups.findIndex((source) => source.id === targetId);
  if (insertAt < 0) insertAt = groups.length;
  else if (!before) insertAt += 1;
  groups.splice(insertAt, 0, moved);
  groups.forEach((source, index) => { source.order = index; });
  await autoSaveConfig();
  render();
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
  group.querySelector('[data-action="remove-source"]').onclick = async (event) => {
    if (event.currentTarget.disabled) {
      return;
    }
    // Guard the race between a task starting and updateStatusUi disabling the
    // button: never remove a source whose destination has a task in flight.
    const active = (source.destinations || []).some((dst) => dstActivity(source, dst).active);
    if (active) {
      setMessage("A task is running for this source — stop it (pause the destination) first");
      return;
    }
    cfg.source_groups.splice(sourceIndex, 1);
    await autoSaveConfig();
    render();
  };
  group.querySelector('[data-action="edit-excludes"]').onclick = () => {
    openExcludeModal(source);
  };
  group.querySelector('[data-action="restart-notice"]').onclick = async (event) => {
    const button = event.currentTarget;
    const notice = sourceRestartNotice(source.id);
    if (!notice || button.disabled) {
      return;
    }
    const dismiss = window.confirm(
      `The sync daemon for ${source.id} restarted at ${formatScanTime(notice.at)}; `
        + "changes made while it was down may be unrecorded.\n\n"
        + "OK = dismiss this notice (accept the risk).\n"
        + "Cancel = keep it; run Compare or Full to verify instead.",
    );
    if (!dismiss) {
      return;
    }
    button.disabled = true;
    try {
      await invoke("dismiss_restart_notice", { sourceId: source.id });
      button.hidden = true;
      setTransientMessage(`Restart notice for ${source.id} dismissed`);
      refreshStatusOnly().catch(() => {});
    } catch (error) {
      setMessage(String(error));
    } finally {
      button.disabled = false;
    }
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
    // A sync already in flight is fine: the backend queues the request and
    // the engine picks it up right after the current pass.
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
        <span class="dst-info-cell">
          <button class="repair-scan-button icon" data-action="repair-scan" title="" aria-label="Sync compare differences" hidden>&#8646;</button>
          <button class="pause-dst-button icon ${dst.paused ? "paused" : ""}" data-action="pause-dst" title="${dst.paused ? "Resume automatic sync" : "Pause sync (stops the running task and holds new ones)"}" aria-label="${dst.paused ? "Resume automatic sync" : "Pause sync"}" hidden>${dst.paused ? "&#9654;" : "&#9208;"}</button>
          <button class="destination-log-button icon destination-log-${escapeAttr(logIconState.kind)}" data-action="show-dst-log" title="${escapeAttr(logIconState.title)}" aria-label="${escapeAttr(logIconState.title)}"><span class="destination-log-icon" aria-hidden="true">i</span></button>
        </span>
        <button class="schedule-button" data-action="edit-schedule">${escapeHtml(scheduleLabel(dst.schedule))}</button>
        <input class="destination-readonly destination-cycle" value="${escapeAttr(cycleDisplay(status))}" readonly>
        <button class="sync-config-button icon" data-action="edit-dst-sync" title="${escapeAttr(destinationSyncTitle(dst))}">&#9881;</button>
        <select class="destination-sync-select" data-action="sync-dst" title="Sync">
          <option value="">Sync</option>
          <option value="incremental">Incremental</option>
          <option value="full">Full</option>
          <option value="scan">Compare</option>
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
    row.querySelector('[data-action="pause-dst"]').onclick = async (event) => {
      const button = event.currentTarget;
      if (button.disabled) {
        return;
      }
      button.disabled = true;
      const pausing = !dst.paused;
      try {
        // Persist the flag FIRST: the scheduler must already see the pause
        // when the running task dies, or it would immediately restart it.
        dst.paused = pausing;
        await autoSaveConfig();
        if (pausing) {
          const act = dstActivity(source, dst);
          if (act.active) {
            await invoke("cancel_activity", {
              scope: act.scope,
              sourceId: source.id,
              destinationId: dst.id,
            });
          }
          setTransientMessage(`Paused ${source.id} -> ${dst.id}`);
        } else {
          setTransientMessage(`Resumed ${source.id} -> ${dst.id}`);
        }
        await loadRuntimeStatus().catch(() => {});
        renderSourcePanel();
      } catch (error) {
        dst.paused = !pausing;
        setMessage(String(error));
      } finally {
        button.disabled = false;
      }
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
      // One uniform Info view for every task type (sync, compare, idle).
      openDestinationLogModal(source, dst);
    };
    row.querySelector('[data-action="repair-scan"]').onclick = () => {
      const latestStatus = statusFor(source.id, dst.id);
      const diffs = Number(latestStatus && latestStatus.scan_differences) || 0;
      if (!diffs) {
        return;
      }
      if (isDestinationUnavailable(latestStatus)) {
        setMessage(unavailableLabel(latestStatus));
        return;
      }
      runBusy(`Syncing ${diffs} compare difference${diffs === 1 ? "" : "s"} for ${dst.id}...`, async () => {
        await saveConfig();
        statuses = await invoke("sync_destination_now", {
          sourceId: source.id,
          destinationId: dst.id,
          mode: "repair_scan",
        });
        setMessage("");
        render();
      }, { showMainMessage: false });
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
      // A sync already in flight is fine: the backend queues the request and
      // the engine picks it up right after the current pass.
      if (mode === "scan") {
        // The scan runs in the background (it can take many minutes on a large
        // tree and must not block the backup). Kick it off and poll for the
        // report; the info icon opens live progress on demand — no popup is
        // forced on the user.
        const key = scanReportKey(source.id, dst.id);
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
            startedAt: Date.now(),
            lastSeenRunning: Date.now(),
          };
          if (previous) {
            scanReports[key] = previous;
          }
          setMessage(`Compare running for ${source.id} -> ${dst.id} — click its info icon for progress.`);
          updateDstControls();
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
      dst.paused = dst.paused === true;
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
    autostart: app.autostart !== false,
    close_to_tray: app.close_to_tray !== false,
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
    zfs_diff: sync.zfs_diff !== false,
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
    time: "19:00",
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
  const text = String(value || "19:00");
  const match = /^(\d{1,2}):(\d{2})(?::\d{2})?$/.exec(text);
  if (!match) {
    return "19:00";
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
  // verified / latest: how far this destination lags behind the source's
  // newest closed cycle (a scheduled destination catches up at its schedule).
  const latest = valueOr(
    status && status.latest_closed_cycle_id,
    valueOr(status && status.target_cycle_id, "-"),
  );
  return `${valueOr(status && status.last_verified_cycle_id, "-")} / ${latest}`;
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
  // A realtime schedule has no meaningful time; show the default so switching
  // to Daily/Weekly starts from it instead of a stale leftover value.
  const timeForField = draft.mode === "realtime"
    ? defaultDestinationSchedule().time
    : draft.time;
  el.cycleTime.value = formatScheduleTime(timeForField);
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
      time: normalizeScheduleTime(el.cycleTime.value || "19:00"),
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
  el.settingsSyncZfsDiff.checked = sync.zfs_diff !== false;
  el.settingsSyncDebug.checked = sync.debug_logs;
  el.settingsSyncTimeout.value = String(sync.transfer_timeout_secs || 120);
  el.settingsSyncBwlimit.value = String(sync.bwlimit_kbps || 0);
  el.settingsTcpPool.value = String(cfg.app.tcp_connection_pool_size ?? 100);
  el.settingsAutostart.checked = cfg.app.autostart !== false;
  el.settingsCloseToTray.checked = cfg.app.close_to_tray !== false;
  el.settingsModal.hidden = false;
}

function closeSettingsModal() {
  el.settingsModal.hidden = true;
}

async function saveSettings() {
  updateCfgFromForm();
  cfg.app = normalizeAppConfig(cfg.app || {});
  cfg.app.tcp_connection_pool_size = clampInteger(el.settingsTcpPool.value, 0, 10000);
  cfg.app.autostart = el.settingsAutostart.checked;
  cfg.app.close_to_tray = el.settingsCloseToTray.checked;
  const baseSync = normalizeNativeSyncConfig(cfg.app.sync || {});
  cfg.app.sync = normalizeNativeSyncConfig({
    ...baseSync,
    mirror: el.settingsSyncMirror.checked,
    checksum: el.settingsSyncChecksum.checked,
    zfs_diff: el.settingsSyncZfsDiff.checked,
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
  el.dstSyncZfsDiff.checked = sync.zfs_diff !== false;
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
    zfs_diff: el.dstSyncZfsDiff.checked,
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
  { kind: "metadata", label: "Metadata (permissions differ)", field: "metadata" },
];
const SCAN_DIFF_MODAL_CAP = 50;

function openDestinationLogModal(source, dst) {
  dstLogViewer = { sourceId: source.id, destinationId: dst.id };
  dstLogTask = null;
  renderDestinationLogModal();
  el.dstLogModal.hidden = false;
  // The Summary row's compare stats come from the last stored report; pull it
  // if we don't already have a fresh one cached from a Scan run just now.
  const key = scanReportKey(source.id, dst.id);
  if (!scanReports[key]) {
    invoke("scan_report", { sourceId: source.id, destinationId: dst.id })
      .then((report) => {
        if (report) {
          scanReports[key] = report;
          renderDestinationLogModal();
        }
      })
      .catch(() => {});
  }
  refreshDstLogTask();
  if (!dstLogTaskTimer) {
    dstLogTaskTimer = setInterval(refreshDstLogTask, 3000);
  }
}

function closeDestinationLogModal() {
  el.dstLogModal.hidden = true;
  dstLogViewer = null;
  dstLogTask = null;
  if (dstLogTaskTimer) {
    clearInterval(dstLogTaskTimer);
    dstLogTaskTimer = null;
  }
}

// Find the newest task-log entry for the open destination across all machines
// (the executing machine stores it; a cross-machine cycle's destination_id is
// a comma-joined list).
async function refreshDstLogTask() {
  if (!dstLogViewer) {
    return;
  }
  const { sourceId, destinationId } = dstLogViewer;
  let machines;
  try {
    machines = await invoke("get_all_tasks", { limit: 50 });
  } catch (error) {
    return; // keep the last known task
  }
  if (!dstLogViewer || dstLogViewer.sourceId !== sourceId
    || dstLogViewer.destinationId !== destinationId) {
    return; // the user switched destinations mid-fetch
  }
  let best = null;
  for (const machine of machines || []) {
    for (const task of machine.tasks || []) {
      if (task.source_id !== sourceId) {
        continue;
      }
      const ids = String(task.destination_id || "").split(",").map((id) => id.trim());
      if (!ids.includes(destinationId)) {
        continue;
      }
      if (!best || Date.parse(task.started_at) > Date.parse(best.started_at)) {
        best = task;
      }
    }
  }
  dstLogTask = best;
  renderDestinationLogModal();
}

// ---------------------------------------------------------------------------
// Tasks modal: running and recent tasks from this machine and every managed
// runtime machine (each machine stores its own task log; remote logs are
// fetched live from the remote).
// ---------------------------------------------------------------------------

let tasksPollTimer = null;

function openTasksModal() {
  el.tasksModal.hidden = false;
  el.tasksSummary.textContent = "Loading tasks...";
  el.tasksList.innerHTML = "";
  refreshTasksModal();
  if (!tasksPollTimer) {
    tasksPollTimer = setInterval(refreshTasksModal, 3000);
  }
}

function closeTasksModal() {
  el.tasksModal.hidden = true;
  if (tasksPollTimer) {
    clearInterval(tasksPollTimer);
    tasksPollTimer = null;
  }
}

async function refreshTasksModal() {
  if (!el.tasksModal || el.tasksModal.hidden) {
    return;
  }
  let machines;
  try {
    machines = await invoke("get_all_tasks", { limit: 100 });
  } catch (error) {
    el.tasksSummary.textContent = String(error);
    return;
  }
  if (el.tasksModal.hidden) {
    return;
  }
  const running = machines.reduce(
    (total, machine) =>
      total + (machine.tasks || []).filter((task) => task.status === "running").length,
    0,
  );
  el.tasksSummary.textContent =
    `${running} running · newest first · each machine keeps its last 100 finished tasks`;
  el.tasksList.innerHTML = machines.map(renderMachineTasks).join("");
}

function renderMachineTasks(machine) {
  const label = machine.local ? "local" : (machine.label || machine.machine_id);
  const title = `<div class="tasks-machine-title">${escapeHtml(label)}</div>`;
  if (machine.error) {
    return `<div class="tasks-machine">${title}<div class="empty">${escapeHtml(machine.error)}</div></div>`;
  }
  const tasks = machine.tasks || [];
  if (!tasks.length) {
    return `<div class="tasks-machine">${title}<div class="empty">No recorded tasks</div></div>`;
  }
  return `
    <div class="tasks-machine">
      ${title}
      <div class="task-row task-row-head">
        <span>ID</span><span>Status</span><span>Kind</span><span>Task</span><span>Started</span><span>Duration</span><span>Result</span>
      </div>
      ${tasks.map(renderTaskRow).join("")}
    </div>`;
}

function renderTaskRow(task) {
  const target = task.destination_id
    ? `${task.source_id} -> ${task.destination_id}`
    : task.source_id;
  const result = taskResultLabel(task);
  return `
    <div class="task-row">
      <span class="task-id">#${escapeHtml(String(task.id ?? ""))}</span>
      <span class="task-status task-status-${escapeAttr(task.status)}">${escapeHtml(task.status)}</span>
      <span>${escapeHtml(taskKindLabel(task.kind))}</span>
      <span class="task-target" title="${escapeAttr(target)}">${escapeHtml(target)}</span>
      <span>${escapeHtml(formatScanTime(task.started_at))}</span>
      <span>${escapeHtml(taskDurationLabel(task))}</span>
      <span class="task-result" title="${escapeAttr(result)}">${escapeHtml(result)}</span>
    </div>`;
}

function taskKindLabel(kind) {
  switch (String(kind || "").trim()) {
    case "compare": return "Compare";
    case "incremental": return "Incremental";
    case "full": return "Full";
    case "repair_scan": return "Repair";
    default: return kind || "-";
  }
}

function taskDurationLabel(task) {
  if (task.status === "running") {
    const started = Date.parse(task.started_at);
    return Number.isFinite(started)
      ? `${formatDurationMs(Date.now() - started)}…`
      : "running";
  }
  return task.duration_ms === null || task.duration_ms === undefined
    ? "-"
    : formatDurationMs(task.duration_ms);
}

function formatDurationMs(ms) {
  const secs = Math.max(0, Math.round(Number(ms) / 1000));
  if (secs < 60) {
    return `${secs}s`;
  }
  const mins = Math.floor(secs / 60);
  if (mins < 60) {
    return `${mins}m ${secs % 60}s`;
  }
  return `${Math.floor(mins / 60)}h ${mins % 60}m`;
}

function taskResultLabel(task) {
  const parts = [];
  if (task.kind === "compare") {
    const diffs = Number(task.differences || 0);
    if (task.status === "success") {
      parts.push(`${diffs} difference${diffs === 1 ? "" : "s"}`);
    }
    if (Number(task.entries_scanned || 0) > 0) {
      parts.push(`${task.entries_scanned} entries`);
    }
  } else if (Number(task.files_synced || 0) > 0) {
    parts.push(`${task.files_synced} file${task.files_synced === 1 ? "" : "s"}`);
  }
  if (task.error) {
    parts.push(task.error);
  }
  return parts.join(" · ") || "-";
}

function renderDestinationLogModal() {
  if (!dstLogViewer || !el.dstLogModal || el.dstLogModal.hidden) {
    return;
  }
  const source = findSourceById(dstLogViewer.sourceId);
  const dst = source && (source.destinations || []).find((item) => item.id === dstLogViewer.destinationId);
  if (el.dstLogTitle) {
    el.dstLogTitle.textContent = "Info";
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
  // This destination's own compare walk from the live list; an unattributed
  // walk (older peer) is accepted as long as it is not a sync's tree walk.
  const scan = runtimeScans(runtime).find(
    (item) =>
      item.kind !== "sync"
      && (!item.source_id || item.source_id === source.id)
      && (!item.destination_id || item.destination_id === dst.id),
  ) || null;
  const report = scanReports[scanReportKey(source.id, dst.id)];
  // A fixed row set for EVERY task type — same rows, same height whether this
  // is a sync, a compare, or idle. Type/Snapshot/Summary carry the per-task
  // specifics.
  const rows = [
    ["Task", `${source.id} -> ${dst.id}`],
    ["Path", `${machinePathLabel(source.machine_id, source.src)}  →  ${machinePathLabel(dst.machine_id, dst.path)}`],
    ["Status", destinationStatusText(status)],
    ["Cycle", cycleDisplay(status)],
    ["Type", infoTypeLabel(source, dst, runtime, dstLogTask)],
    ["Phase", infoPhaseLabel(source, dst, runtime, transfer, scan)],
    ["Snapshot", infoSnapshotLabel(source, dst, transfer, scan)],
    ["Summary", infoSummaryLabel(source, dst, report, dstLogTask)],
  ];
  el.dstLogSummary.textContent = "";
  el.dstLogList.innerHTML = rows.map(([key, value]) => `
    <div class="dst-log-row">
      <div class="dst-log-key">${escapeHtml(key)}</div>
      <div class="dst-log-value" title="${escapeAttr(value || "-")}">${escapeHtml(value || "-")}</div>
    </div>
  `).join("");
}

// Type: the running task's kind if one is active, else the last task's kind.
function infoTypeLabel(source, dst, runtime, task) {
  const act = dstActivity(source, dst);
  if (act.active) {
    return act.scope === "compare"
      ? "Compare"
      : (syncKindLabel(runtime && runtime.sync_kind) || "Sync");
  }
  return task ? taskKindLabel(task.kind) : "-";
}

// Phase: which stage the running task is in right now. The live scan/transfer
// progress is authoritative when present (a walk or a file copy is definitely
// happening); otherwise fall back to the engine's coarse phase flag, which
// covers the stages that emit no progress (zfs diff, verifying, preparing).
function infoPhaseLabel(source, dst, runtime, transfer, scan) {
  const act = dstActivity(source, dst);
  if (!act.active) {
    return "-";
  }
  if (transfer) {
    return "Transferring";
  }
  if (scan) {
    return act.scope === "compare" ? "Comparing" : "Scanning";
  }
  const phase = runtime && runtime.sync_phase;
  if (phase) {
    return phaseLabel(phase);
  }
  return act.scope === "compare" ? "Comparing" : "Preparing";
}

function phaseLabel(phase) {
  switch (String(phase || "").trim()) {
    case "zfs diff": return "zfs diff";
    case "scanning": return "Scanning";
    case "transferring": return "Transferring";
    case "verifying": return "Verifying";
    case "preparing": return "Preparing";
    default: return phase;
  }
}

// Snapshot: live metrics of the current task at this instant.
function infoSnapshotLabel(source, dst, transfer, scan) {
  const act = dstActivity(source, dst);
  if (act.scope === "compare") {
    if (scan) {
      const entries = Number(scan.entries_seen || 0);
      const path = compactStatusPath(scan.current_path || scan.root_path || "", 44);
      return `${entries} entries${path ? ` · ${path}` : ""}`;
    }
    return "scanning…";
  }
  if (act.scope === "sync") {
    if (transfer) {
      const file = compactStatusPath(transfer.rel_path || "-", 40);
      const speed = formatBytesPerSecond(transfer.bytes_per_sec || 0);
      const progress = formatTransferProgress(transfer);
      return `${file} · ${speed}${progress ? ` · ${progress}` : ""}`;
    }
    return "preparing…";
  }
  return "idle";
}

// Summary: final-result statistics of the current or last task.
//  - Compare: how many entries differ (broken down by kind) and how many
//    matched. Prefers the cached scan report (has the per-kind breakdown);
//    falls back to the task log's own differences/entries_scanned counters
//    (e.g. a cross-machine compare with no report cached locally).
//  - Sync: how many files were synced, and — since the task log's `differences`
//    column carries the failed-file count for a sync — how many failed.
function infoSummaryLabel(source, dst, report, task) {
  const act = dstActivity(source, dst);
  const isCompare = act.scope === "compare" || (task && task.kind === "compare");
  if (isCompare && report) {
    if (report.error) {
      return `compare failed: ${report.error}`;
    }
    const total = (report.to_add || 0) + (report.to_update || 0) + (report.to_delete || 0)
      + (report.type_mismatch || 0) + (report.metadata || 0);
    const matched = Number(report.in_sync || 0);
    if (total === 0) {
      return matched > 0 ? `0 differences (${matched} matched)` : "0 differences";
    }
    const parts = [];
    if (report.to_add) parts.push(`+${report.to_add}`);
    if (report.to_update) parts.push(`~${report.to_update}`);
    if (report.to_delete) parts.push(`−${report.to_delete}`);
    if (report.type_mismatch) parts.push(`!${report.type_mismatch}`);
    if (report.metadata) parts.push(`#${report.metadata}`);
    const matchedNote = matched > 0 ? `, ${matched} matched` : "";
    return `${total} difference${total === 1 ? "" : "s"} (${parts.join(" ")})${matchedNote}`;
  }
  if (task) {
    if (task.status === "running") {
      return "running…";
    }
    if (task.kind === "compare") {
      if (task.error) return `compare failed: ${task.error}`;
      const diffs = Number(task.differences || 0);
      const scanned = Number(task.entries_scanned || 0);
      const scannedNote = scanned > 0 ? ` · ${scanned} compared` : "";
      return diffs === 0
        ? `0 differences${scannedNote}`
        : `${diffs} difference${diffs === 1 ? "" : "s"}${scannedNote}`;
    }
    // Sync task: files_synced = succeeded, differences = failed (see the
    // engine's task_finish, which stashes the failed-file count there).
    const synced = Number(task.files_synced || 0);
    const failed = Number(task.differences || 0);
    if (failed > 0) {
      const attempted = synced + failed;
      const tail = task.error ? ` (${task.error})` : "";
      return `${synced}/${attempted} files synced · ${failed} failed${tail}`;
    }
    if (task.error && task.status !== "success") {
      return synced > 0
        ? `${synced} synced, then failed: ${task.error}`
        : `failed: ${task.error}`;
    }
    if (synced > 0) {
      return `${synced} file${synced === 1 ? "" : "s"} synced`;
    }
    return task.status === "success" ? "no changes" : (task.status || "-");
  }
  return "-";
}

function scanKindLabel(kind) {
  switch (kind) {
    case "add": return "+ add";
    case "update": return "~ update";
    case "delete": return "- delete";
    case "type_mismatch": return "! type";
    case "metadata": return "# perms";
    default: return kind;
  }
}

function formatScanTime(value) {
  if (!value) return "-";
  const date = new Date(value);
  return Number.isNaN(date.getTime()) ? value : date.toLocaleString();
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

function syncKindLabel(kind) {
  switch (String(kind || "").trim()) {
    case "incremental": return "Incremental";
    case "full": return "Full";
    case "scan": return "Compare";
    default: return "";
  }
}

function activitySyncingLabel(activity) {
  const label = (activity && activity.label) || "machine";
  const runtime = activity && activity.runtime;
  const kind = syncKindLabel(runtime && runtime.sync_kind);
  return `Sync already in progress on ${label}${kind ? ` (${kind})` : ""}`;
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
  const kind = syncKindLabel(runtime && runtime.sync_kind);
  return kind ? `syncing (${kind})` : "syncing";
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
  // Display order: sort by the persisted `order` (stable, so equal/missing
  // values keep current array order), then renumber contiguously so the array
  // position — which edits and drag-reordering both index into — always
  // matches the order field.
  cfg.source_groups.forEach((source, index) => {
    if (typeof source.order !== "number" || Number.isNaN(source.order)) {
      source.order = index;
    }
  });
  cfg.source_groups.sort((a, b) => a.order - b.order);
  cfg.source_groups.forEach((source, index) => { source.order = index; });
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
  updateStatusUi();
}

function setMessage(text) {
  statusMessage = text || "";
  updateStatusBar();
}

// Informational notices (stop requested, compare finished) must not sit in
// the status bar forever: auto-clear unless something else replaced them.
let transientMessageTimer = null;
function setTransientMessage(text, ms = 10000) {
  setMessage(text);
  if (transientMessageTimer) {
    clearTimeout(transientMessageTimer);
  }
  transientMessageTimer = setTimeout(() => {
    transientMessageTimer = null;
    if (statusMessage === text) {
      setMessage("");
    }
  }, ms);
}

function destinationSyncStatusMessage(source, mode) {
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
bindButtonClick(el.tasks, openTasksModal);
el.tasksClose.onclick = closeTasksModal;
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
    cancel_activity: ["POST", "/api/cancel-activity"],
    dismiss_restart_notice: ["POST", "/api/dismiss-restart-notice"],
    get_all_tasks: ["GET", "/api/all-tasks"],
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
  } else if (command === "get_all_tasks") {
    url = `${path}?limit=${encodeURIComponent(payload.limit || 100)}`;
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
    } else if (command === "cancel_activity") {
      options.body = JSON.stringify({
        scope: payload.scope || null,
        source_id: payload.sourceId || payload.source_id || null,
        destination_id: payload.destinationId || payload.destination_id || null,
        propagate: true,
      });
    } else if (command === "dismiss_restart_notice") {
      options.body = JSON.stringify({
        source_id: payload.sourceId || payload.source_id,
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
