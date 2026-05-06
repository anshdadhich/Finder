const { invoke } = window.__TAURI__.core;
const { getCurrentWindow } = window.__TAURI__.window;
const { listen } = window.__TAURI__.event;

const searchInput = document.getElementById("searchInput");
const resultsEl = document.getElementById("results");
const settingsBtn = document.getElementById("settingsBtn");
const settingsDialog = document.getElementById("settingsDialog");
const caseSensitiveEl = document.getElementById("caseSensitive");
const excludedPathsEl = document.getElementById("excludedPaths");
const saveSettingsBtn = document.getElementById("saveSettingsBtn");

let results = [];
let selectedIndex = 0;
let caseSensitive = false;
let debounceId = null;

const badgeColor = {
  APP: "rgba(56, 210, 120, 0.85)",
  DOC: "rgba(255, 157, 90, 0.85)",
  IMG: "rgba(192, 108, 255, 0.85)",
  VID: "rgba(237, 96, 96, 0.85)",
  AUD: "rgba(80, 166, 255, 0.85)",
  ZIP: "rgba(229, 190, 70, 0.85)",
  DIR: "rgba(105, 139, 255, 0.85)",
  FILE: "rgba(132, 132, 153, 0.85)"
};

function renderResults() {
  resultsEl.innerHTML = "";
  results.forEach((item, idx) => {
    const li = document.createElement("li");
    li.className = "result-item" + (idx === selectedIndex ? " active" : "");
    li.dataset.index = String(idx);
    li.innerHTML = `
      <div class="result-name">${item.name || item.fullPath}</div>
      <span class="badge" style="background:${badgeColor[item.kind] || badgeColor.FILE}">${item.kind}</span>
      <div class="result-path">${item.fullPath}</div>
    `;
    li.addEventListener("mousemove", () => {
      selectedIndex = idx;
      renderResults();
    });
    li.addEventListener("dblclick", () => openSelected(false));
    resultsEl.appendChild(li);
  });
}

async function runSearch() {
  const query = searchInput.value.trim();
  if (!query) {
    results = [];
    selectedIndex = 0;
    renderResults();
    return;
  }
  results = await invoke("search", {
    query,
    limit: 60,
    caseSensitive
  });
  selectedIndex = 0;
  renderResults();
}

async function openSelected(folderOnly) {
  const item = results[selectedIndex];
  if (!item) return;
  await invoke("open_result", {
    path: item.fullPath,
    folderOnly
  });
  await getCurrentWindow().hide();
}

async function loadSettings() {
  const settings = await invoke("get_settings");
  caseSensitive = !!settings.caseSensitive;
  caseSensitiveEl.checked = caseSensitive;
  excludedPathsEl.value = (settings.excludedPaths || []).join("\n");
}

async function saveSettings() {
  const excludedPaths = excludedPathsEl.value
    .split(/\r?\n/)
    .map((v) => v.trim())
    .filter(Boolean);

  caseSensitive = !!caseSensitiveEl.checked;

  await invoke("save_settings", {
    payload: {
      excludedPaths,
      caseSensitive
    }
  });
  settingsDialog.close();
  await runSearch();
}

searchInput.addEventListener("input", () => {
  clearTimeout(debounceId);
  debounceId = setTimeout(runSearch, 70);
});

searchInput.addEventListener("keydown", async (event) => {
  if (event.key === "ArrowDown") {
    if (results.length > 0) {
      selectedIndex = Math.min(selectedIndex + 1, results.length - 1);
      renderResults();
    }
    event.preventDefault();
    return;
  }
  if (event.key === "ArrowUp") {
    if (results.length > 0) {
      selectedIndex = Math.max(selectedIndex - 1, 0);
      renderResults();
    }
    event.preventDefault();
    return;
  }
  if (event.key === "Enter") {
    await openSelected(event.ctrlKey);
    event.preventDefault();
    return;
  }
  if (event.key === "Escape") {
    await getCurrentWindow().hide();
  }
});

settingsBtn.addEventListener("click", async () => {
  await loadSettings();
  settingsDialog.showModal();
});

saveSettingsBtn.addEventListener("click", saveSettings);

listen("focus-search", () => {
  searchInput.focus();
  searchInput.select();
});

window.addEventListener("DOMContentLoaded", async () => {
  await loadSettings();
  searchInput.focus();
});
