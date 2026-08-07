const invoke = window.__TAURI__?.tauri?.invoke || window.__TAURI__?.invoke;

const input = document.querySelector("#search");
const statusEl = document.querySelector("#status");
const statusText = document.querySelector("#statusText");
const progressFill = document.querySelector("#progressFill");
const resultsEl = document.querySelector("#results");

let items = [];
let selected = 0;
let debounceTimer = 0;
let searchSeq = 0;
let lastSearchAt = 0;

const iconCache = new Map();
let rowEls = [];

const MAX_APPS = 16;
const MAX_DIRS = 8;
const MAX_FILES = 24;

const SEARCH_GAP_MS = 120;
const MIN_FILE_QUERY_LEN = 2;

if (!invoke) {
  statusEl.textContent = "Tauri API unavailable. Rebuild and restart the app.";
}

async function refreshStatus() {
  if (!invoke) return;
  try {
    const status = await invoke("get_index_status");
    const message = status.message || (status.ready ? "Ready" : "Indexing...");
    statusText.textContent = status.ready ? "" : message;
    progressFill.style.width = status.ready ? "100%" : `${18 + (Date.now() / 80) % 72}%`;
  } catch (error) {
    statusEl.textContent = `Backend unavailable: ${error}`;
  }
}

async function runSearch() {
  if (!invoke) return;
  const query = input.value.trim();
  const seq = ++searchSeq;

  // Apps are cheap: show them immediately so results feel responsive while typing.
  const appList = await invoke("search_apps", { query });
  if (seq !== searchSeq) return;
  items = appList.slice(0, MAX_APPS);
  selected = 0;
  render();

  // Files need a full index scan — skip it for too-short queries and only after
  // the app list has already been rendered.
  if (query.length < MIN_FILE_QUERY_LEN) return;

  const files = await invoke("search_files", { query });
  if (seq !== searchSeq) return;
  items = [...appList.slice(0, MAX_APPS)];
  for (const f of files) {
    if (items.length >= 60) break;
    items.push(f);
  }
  selected = 0;
  render();
}

function scheduleSearch() {
  clearTimeout(debounceTimer);
  const now = Date.now();
  const idle = now - lastSearchAt;
  if (idle >= SEARCH_GAP_MS) {
    lastSearchAt = now;
    runSearch();
  } else {
    debounceTimer = setTimeout(() => {
      lastSearchAt = Date.now();
      runSearch();
    }, SEARCH_GAP_MS - idle);
  }
}

function groupItems() {
  const apps = [];
  const dirs = [];
  const files = [];
  for (const item of items) {
    if (item.kind === "app") apps.push(item);
    else if (item.kind === "dir") dirs.push(item);
    else files.push(item);
  }
  const groups = [];
  if (apps.length) groups.push({ label: "Applications", rows: apps });
  if (dirs.length) groups.push({ label: "Folders", rows: dirs.slice(0, MAX_DIRS) });
  if (files.length) groups.push({ label: "Files", rows: files.slice(0, MAX_FILES) });
  return groups;
}

function requestIcons(rows) {
  const wanted = [];
  for (const item of rows) {
    const key = item.path.toLowerCase();
    if (!iconCache.has(key)) wanted.push(item.path);
  }
  if (!wanted.length) return;
  invoke("get_icons", { paths: wanted }).then((map) => {
    if (!map) return;
    for (const [path, uri] of Object.entries(map)) {
      iconCache.set(path.toLowerCase(), uri);
      const img = rowEls.find((el) => el && el.dataset.path === path)?.querySelector(".icon");
      if (img) img.src = uri;
    }
  }).catch(() => {});
}

function render() {
  resultsEl.innerHTML = "";
  rowEls = [];
  const groups = groupItems();
  statusEl.style.display = "none";

  const fragment = document.createDocumentFragment();
  let flatIndex = 0;

  for (const group of groups) {
    const header = document.createElement("div");
    header.className = "group-label";
    header.textContent = group.label;
    fragment.appendChild(header);

    for (const item of group.rows) {
      const row = document.createElement("div");
      row.className = "result";
      row.dataset.index = flatIndex;
      row.dataset.path = item.path;

      const img = document.createElement("img");
      img.className = "icon";
      img.alt = "";
      const iconKey = item.path.toLowerCase();
      if (iconCache.has(iconKey)) img.src = iconCache.get(iconKey);

      const text = document.createElement("div");
      const name = document.createElement("div");
      name.className = "name";
      name.textContent = item.name || item.path;
      const path = document.createElement("div");
      path.className = "path";
      path.textContent = item.kind === "app" ? "" : item.path;
      text.appendChild(name);
      text.appendChild(path);

      row.appendChild(img);
      row.appendChild(text);
      fragment.appendChild(row);
      rowEls[flatIndex] = row;
      flatIndex += 1;
    }
  }

  resultsEl.appendChild(fragment);
  requestIcons(items.slice(0, flatIndex));
  updateSelection();
}

function updateSelection() {
  for (let i = 0; i < rowEls.length; i++) {
    const el = rowEls[i];
    if (!el) continue;
    el.classList.toggle("selected", i === selected);
  }
  const active = rowEls[selected];
  if (active && active.scrollIntoView) {
    active.scrollIntoView({ block: "nearest" });
  }
}

async function openSelected(parent) {
  const item = items[selected];
  if (!item) {
    const query = input.value.trim();
    if (query) {
      await invoke("open_web_search", { query });
      await invoke("hide_window");
    }
    return;
  }
  if (item.kind === "app") {
    if (parent) {
      await invoke("open_parent", { path: item.path });
    } else {
      await invoke("launch_app", { path: item.path });
    }
    await invoke("hide_window");
    return;
  }
  await invoke(parent ? "open_parent" : "open_path", { path: item.path });
  await invoke("hide_window");
}

input.addEventListener("input", scheduleSearch);

window.addEventListener("keydown", async (event) => {
  if (event.key === "Escape") {
    event.preventDefault();
    if (invoke) await invoke("hide_window");
    return;
  }

  if (event.key === "ArrowDown") {
    event.preventDefault();
    if (selected < rowEls.length - 1) {
      selected += 1;
      updateSelection();
    }
    return;
  }

  if (event.key === "ArrowUp") {
    event.preventDefault();
    if (selected > 0) {
      selected -= 1;
      updateSelection();
    }
    return;
  }

  if (event.key === "Enter") {
    event.preventDefault();
    const query = input.value.trim();
    if (query.includes(".com") && !items.length) {
      await invoke("open_web_search", { query });
      await invoke("hide_window");
      return;
    }
    await openSelected(event.ctrlKey);
  }
});

resultsEl.addEventListener("mousemove", (event) => {
  const row = event.target.closest(".result");
  if (!row) return;
  const idx = Number(row.dataset.index);
  if (idx !== selected && Number.isInteger(idx)) {
    selected = idx;
    updateSelection();
  }
});

resultsEl.addEventListener("click", (event) => {
  const row = event.target.closest(".result");
  if (!row) return;
  selected = Number(row.dataset.index);
  openSelected(event.ctrlKey);
});

window.addEventListener("focus", () => {
  input.focus();
  input.select();
  refreshStatus();
});

input.addEventListener("focus", () => {
  if (!items.length) runSearch();
});

setInterval(refreshStatus, 1500);
refreshStatus();
input.focus();
