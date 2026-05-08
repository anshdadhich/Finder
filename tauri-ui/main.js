const invoke = window.__TAURI__?.tauri?.invoke || window.__TAURI__?.invoke;

const input = document.querySelector("#search");
const statusEl = document.querySelector("#status");
const statusText = document.querySelector("#statusText");
const progressFill = document.querySelector("#progressFill");
const resultsEl = document.querySelector("#results");
const card = document.querySelector("#card");
const settings = document.querySelector("#settings");
const cogIcon = document.querySelector("#cogIcon");

let results = [];
let selected = 0;
let timer = 0;
let userInteracted = false;
let hidAfterReady = false;

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
    statusEl.style.display = input.value && results.length ? "none" : "block";

    if (status.ready && !hidAfterReady && !userInteracted && !input.value) {
      hidAfterReady = true;
      setTimeout(() => invoke("hide_window"), 900);
    }
  } catch (error) {
    statusEl.textContent = `Backend unavailable: ${error}`;
    statusEl.style.display = "block";
  }
}

async function runSearch() {
  if (!invoke) return;
  const query = input.value.trim();
  if (!query) {
    results = [];
    selected = 0;
    render();
    await refreshStatus();
    return;
  }

  results = await invoke("search_files", { query });
  selected = 0;
  render();
}

function scheduleSearch() {
  clearTimeout(timer);
  timer = setTimeout(runSearch, 10);
}

function render() {
  resultsEl.innerHTML = "";
  statusEl.style.display = results.length || input.value ? "none" : "block";

  for (const [index, item] of results.entries()) {
    const row = document.createElement("div");
    row.className = `result${index === selected ? " selected" : ""}`;
    row.dataset.index = index;
    row.innerHTML = `
      <div class="icon">${item.is_dir ? "DIR" : "FILE"}</div>
      <div>
        <div class="name"></div>
        <div class="path"></div>
      </div>
    `;
    row.querySelector(".name").textContent = item.name;
    row.querySelector(".path").textContent = item.path;
    row.addEventListener("mouseenter", () => {
      selected = index;
      render();
    });
    row.addEventListener("dblclick", () => openSelected(false));
    resultsEl.appendChild(row);
  }
}

settings.addEventListener("click", () => {
  card.classList.toggle("active");
  cogIcon.style.transform = card.classList.contains("active") ? "rotate(45deg)" : "rotate(0deg)";
});

async function openSelected(parent) {
  const item = results[selected];
  if (!item) {
    const query = input.value.trim();
    if (query) {
      await invoke("open_web_search", { query });
      await invoke("hide_window");
    }
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
    selected = Math.min(selected + 1, results.length - 1);
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
    if (query.includes(".com")) {
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
