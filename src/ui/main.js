const invoke = window.__TAURI__.core.invoke;
const dialogOpen = window.__TAURI__.dialog.open;

let cfg = null;
let busy = false;

const el = {
  configPath: document.getElementById("config-path"),
  scheduleMode: document.getElementById("schedule-mode"),
  scheduleTime: document.getElementById("schedule-time"),
  scheduleWeekday: document.getElementById("schedule-weekday"),
  sources: document.getElementById("sources"),
  statusBody: document.getElementById("status-body"),
  statusCount: document.getElementById("status-count"),
  message: document.getElementById("message"),
  refresh: document.getElementById("refresh"),
  sync: document.getElementById("sync"),
  save: document.getElementById("save"),
  addSource: document.getElementById("add-source"),
};

async function loadAll() {
  cfg = await invoke("get_config");
  renderConfig();
  await loadStatus();
}

async function loadStatus() {
  const rows = await invoke("get_status");
  el.statusBody.innerHTML = "";
  el.statusCount.textContent = rows.length ? `${rows.length} destination(s)` : "";

  if (!rows.length) {
    const tr = document.createElement("tr");
    tr.innerHTML = `<td colspan="4" class="empty">No destinations configured</td>`;
    el.statusBody.appendChild(tr);
    return;
  }

  for (const row of rows) {
    const tr = document.createElement("tr");
    const dotClass = row.status === "green" ? "green" : "red";
    tr.innerHTML = `
      <td><span class="dot ${dotClass}"></span>${escapeHtml(row.status)}</td>
      <td>${escapeHtml(row.source_id)}</td>
      <td>${escapeHtml(row.destination_id)}<div class="subtle">${escapeHtml(row.status_reason)}</div></td>
      <td>${row.last_verified_cycle_id ?? "-"} / ${row.latest_closed_cycle_id ?? "-"}</td>
    `;
    el.statusBody.appendChild(tr);
  }
}

function renderConfig() {
  el.configPath.textContent = "Linux Tauri GUI";
  el.scheduleMode.value = cfg.schedule.mode;
  el.scheduleTime.value = cfg.schedule.time;
  el.scheduleWeekday.value = cfg.schedule.weekday || "monday";
  el.sources.innerHTML = "";

  if (!cfg.source_groups.length) {
    const div = document.createElement("div");
    div.className = "empty";
    div.textContent = "No sources configured";
    el.sources.appendChild(div);
    return;
  }

  cfg.source_groups.forEach((source, index) => {
    el.sources.appendChild(renderSource(source, index));
  });
}

function renderSource(source, index) {
  const div = document.createElement("div");
  div.className = "source";
  div.innerHTML = `
    <div class="source-grid">
      <div>
        <label>ID</label>
        <input value="${escapeAttr(source.id)}" data-field="id">
      </div>
      <div>
        <label>Source Path</label>
        <input value="${escapeAttr(source.src)}" data-field="src">
      </div>
      <button data-action="browse-source">Browse</button>
      <button class="danger icon" data-action="remove-source" title="Remove source">x</button>
    </div>
    <div class="dst-list" data-dsts></div>
    <button class="add-dst" data-action="add-dst">Add Destination</button>
  `;

  div.querySelector('[data-field="id"]').oninput = (event) => {
    source.id = event.target.value;
  };
  div.querySelector('[data-field="src"]').oninput = (event) => {
    source.src = event.target.value;
  };
  div.querySelector('[data-action="browse-source"]').onclick = async () => {
    const path = await pickFolder();
    if (path) {
      source.src = path;
      renderConfig();
    }
  };
  div.querySelector('[data-action="remove-source"]').onclick = () => {
    cfg.source_groups.splice(index, 1);
    renderConfig();
  };
  div.querySelector('[data-action="add-dst"]').onclick = () => {
    source.destinations.push({
      id: `dst_${source.destinations.length + 1}`,
      path: "",
      enabled: true,
    });
    renderConfig();
  };

  const dstRoot = div.querySelector("[data-dsts]");
  source.destinations.forEach((dst, dstIndex) => {
    dstRoot.appendChild(renderDestination(source, dst, dstIndex));
  });
  return div;
}

function renderDestination(source, dst, dstIndex) {
  const div = document.createElement("div");
  div.className = "dst";
  div.innerHTML = `
    <div>
      <label>ID</label>
      <input value="${escapeAttr(dst.id)}" data-field="id">
    </div>
    <div>
      <label>Destination Path</label>
      <input value="${escapeAttr(dst.path)}" data-field="path">
    </div>
    <button data-action="browse-dst">Browse</button>
    <button class="danger icon" data-action="remove-dst" title="Remove destination">x</button>
  `;

  div.querySelector('[data-field="id"]').oninput = (event) => {
    dst.id = event.target.value;
  };
  div.querySelector('[data-field="path"]').oninput = (event) => {
    dst.path = event.target.value;
  };
  div.querySelector('[data-action="browse-dst"]').onclick = async () => {
    const path = await pickFolder();
    if (path) {
      dst.path = path;
      renderConfig();
    }
  };
  div.querySelector('[data-action="remove-dst"]').onclick = () => {
    source.destinations.splice(dstIndex, 1);
    renderConfig();
  };
  return div;
}

async function pickFolder() {
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

function updateCfgFromForm() {
  cfg.schedule.mode = el.scheduleMode.value;
  cfg.schedule.time = el.scheduleTime.value;
  cfg.schedule.weekday = el.scheduleWeekday.value || "monday";

  for (const source of cfg.source_groups) {
    source.enabled = true;
    source.mode = "mirror";
    for (const dst of source.destinations) {
      dst.enabled = true;
    }
  }
}

async function saveConfig() {
  updateCfgFromForm();
  cfg = await invoke("save_config_command", { cfg });
  renderConfig();
  await loadStatus();
}

function setBusy(nextBusy) {
  busy = nextBusy;
  el.refresh.disabled = busy;
  el.sync.disabled = busy;
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

el.addSource.onclick = () => {
  cfg.source_groups.push({
    id: `source_${cfg.source_groups.length + 1}`,
    src: "",
    enabled: true,
    mode: "mirror",
    destinations: [],
  });
  renderConfig();
};

el.refresh.onclick = async () => {
  if (busy) return;
  try {
    setBusy(true);
    setMessage("");
    await loadAll();
  } catch (error) {
    setMessage(String(error));
  } finally {
    setBusy(false);
  }
};

el.save.onclick = async () => {
  if (busy) return;
  try {
    setBusy(true);
    setMessage("");
    await saveConfig();
  } catch (error) {
    setMessage(String(error));
  } finally {
    setBusy(false);
  }
};

el.sync.onclick = async () => {
  if (busy) return;
  try {
    setBusy(true);
    setMessage("Sync running...");
    await saveConfig();
    await invoke("sync_now");
    await loadStatus();
    setMessage("");
  } catch (error) {
    setMessage(String(error));
  } finally {
    setBusy(false);
  }
};

loadAll().catch((error) => setMessage(String(error)));
