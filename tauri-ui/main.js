const invoke = window.__TAURI__?.tauri?.invoke || window.__TAURI__?.invoke;

const input = document.querySelector("#search");
const statusEl = document.querySelector("#status");
const statusText = document.querySelector("#statusText");
const progressFill = document.querySelector("#progressFill");
const resultsEl = document.querySelector("#results");

let items = [];
let selected = 0;
let timer = 0;
let userInteracted = false;

const iconCache = new Map();

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
    statusEl.style.display = input.value && items.length ? "none" : "block";
  } catch (error) {
    statusEl.textContent = `Backend unavailable: ${error}`;
    statusEl.style.display = "block";
  }
}

async function runSearch() {
  if (!invoke) return;
  const query = input.value.trim();
  if (!query) {
    items = [];
    selected = 0;
    render();
    await refreshStatus();
    return;
  }

  const [apps, files] = await Promise.all([
    invoke("search_apps", { query }),
    invoke("search_files", { query }),
  ]);
  items = [...apps, ...files];
  selected = 0;
  render();
}

function scheduleSearch() {
  clearTimeout(timer);
  timer = setTimeout(runSearch, 10);
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
  if (dirs.length) groups.push({ label: "Folders", rows: dirs });
  if (files.length) groups.push({ label: "Files", rows: files });
  return groups;
}

function lazyIcon(row, item) {
  const img = row.querySelector(".icon");
  const key = (item.kind === "app" ? item.path : item.path).toLowerCase();
  if (iconCache.has(key)) {
    img.src = iconCache.get(key);
    return;
  }
  invoke("get_icon", { path: item.path }).then((uri) => {
    if (uri) {
      iconCache.set(key, uri);
      if (img.isConnected) img.src = uri;
    }
  });
}

function render() {
  resultsEl.innerHTML = "";
  const groups = groupItems();
  statusEl.style.display = items.length || input.value ? "none" : "block";

  let flatIndex = 0;
  for (const group of groups) {
    const header = document.createElement("div");
    header.className = "group-label";
    header.textContent = group.label;
    resultsEl.appendChild(header);

    for (const item of group.rows) {
      const row = document.createElement("div");
      row.className = `result${flatIndex === selected ? " selected" : ""}`;
      row.dataset.index = flatIndex;
      row.innerHTML = `
        <img class="icon" alt="" />
        <div>
          <div class="name"></div>
          <div class="path"></div>
        </div>
        <span class="badge">${item.kind === "app" ? "⌘" : item.kind === "dir" ? "DIR" : "FILE"}</span>
      `;
      row.querySelector(".name").textContent = item.name || item.path;
      row.querySelector(".path").textContent = item.path;
      lazyIcon(row, item);
      row.addEventListener("mouseenter", () => {
        selected = flatIndex;
        render();
      });
      row.addEventListener("click", () => {
        selected = flatIndex;
        openSelected(false);
      });
      resultsEl.appendChild(row);
      flatIndex += 1;
    }
  }

  resultsEl.scrollTop = 0;
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
input.addEventListener("focus", () => {
  userInteracted = true;
});

window.addEventListener("keydown", async (event) => {
  if (event.key === "Escape") {
    event.preventDefault();
    if (invoke) await invoke("hide_window");
    return;
  }

  if (event.key === "ArrowDown") {
    event.preventDefault();
    selected = Math.min(selected + 1, items.length - 1);
    render();
    return;
  }

  if (event.key === "ArrowUp") {
    event.preventDefault();
    selected = Math.max(selected - 1, 0);
    render();
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

window.addEventListener("focus", () => {
  input.focus();
  input.select();
  refreshStatus();
});

setInterval(refreshStatus, 1000);
refreshStatus();
input.focus();