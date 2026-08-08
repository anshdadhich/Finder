const invoke = window.__TAURI__?.tauri?.invoke || window.__TAURI__?.invoke;

// Follow the OS theme live (head inline script handles first paint).
const themeQuery = window.matchMedia("(prefers-color-scheme: light)");
function applySystemTheme() {
  document.documentElement.setAttribute(
    "data-theme",
    themeQuery.matches ? "light" : "dark"
  );
}
if (themeQuery.addEventListener) themeQuery.addEventListener("change", applySystemTheme);
else if (themeQuery.addListener) themeQuery.addListener(applySystemTheme);

const cardEl = document.querySelector("#card");
const input = document.querySelector("#search");
const statusEl = document.querySelector("#status");
const statusText = document.querySelector("#statusText");
const progressFill = document.querySelector("#progressFill");
const resultsEl = document.querySelector("#results");
const emptyEl = document.querySelector("#empty");
const hintsEl = document.querySelector("#hints");
const scanStateEl = document.querySelector("#scanState");
const scanTitle = document.querySelector("#scanTitle");
const scanSub = document.querySelector("#scanSub");
const scanStatusText = document.querySelector("#scanStatusText");
const scanElapsed = document.querySelector("#scanElapsed");
const scanRetryBtn = document.querySelector("#scanRetry");
const scanQuitBtn = document.querySelector("#scanQuit");
let scanStartAt = 0;

let items = [];
let selected = 0;
let debounceTimer = 0;
let searchSeq = 0;
let lastSearchAt = 0;
let firstInitDone = false;

const iconCache = new Map();
let rowEls = [];
const rowPool = new Map();

const MAX_APPS = 16;
const MAX_FILES = 500; // display cap; off-screen rows are skipped by CSS
const MAX_TOTAL_FILES = 3000; // hard guard against one crushing query
const MAX_ITEMS = MAX_APPS + MAX_FILES + 8;
const FILE_PAGE = 100; // matches the backend page size

const SEARCH_GAP_MS = 90;
const MIN_FILE_QUERY_LEN = 2;
const MAX_FILE_CACHE = 6;

// ── Client-side pools ──────────────────────────────────────────────────
// Apps are small enough to hold locally: every keystroke ranks them
// instantly, no IPC. File answers are cached by query (~6) so refining a
// query paints from the previous answer while the server backfills.
const appPool = [];
let appPoolLoaded = false;
let appPoolRev = -1;
const fileCache = new Map();

async function loadApps(force) {
  if (!force && appPoolLoaded) return;
  try {
    const all = await invoke("get_all_apps");
    if (all && all.length) {
      appPool.length = 0;
      appPool.push(...all);
      appPoolLoaded = true;
      if (!items.length) runSearchSafe();
    }
  } catch (error) {
    console.error("app pool failed:", error);
  }
}

// Mirrors the Rust `app_rank` scoring: exact > start > word-start > contains.
function rankApp(nameLower, q) {
  if (q === "") return 0;
  if (nameLower === q) return 0;
  if (nameLower.startsWith(q)) return 1;
  if (nameLower.split(/[ \-_.(\[]/).some((w) => w.startsWith(q))) return 2;
  if (nameLower.includes(q)) return 3;
  return -1;
}

// Subsequence scorer (fzy-lite): higher is better, -1 = no match.
function fuzzyApp(nameLower, q) {
  if (q.length < 2) return -1;
  let idx = 0;
  let prev = -2;
  let score = 0;
  for (let i = 0; i < q.length; i++) {
    const pos = nameLower.indexOf(q[i], idx);
    if (pos === -1) return -1;
    const atBoundary = pos === 0 || /[ \-_.(\[]/.test(nameLower[pos - 1]);
    score += atBoundary ? 4 : pos === prev + 1 ? 3 : Math.max(0, 3 - (pos - prev));
    prev = pos;
    idx = pos + 1;
  }
  return score;
}

function clientApps(query) {
  const q = query.trim().toLowerCase();
  // Empty query = "browse all installed apps" — no 16-item cap, just the
  // hard list guard. Typed queries keep the ranked top-16.
  if (!q) return appPool.slice(0, MAX_ITEMS);
  const scored = [];
  for (const app of appPool) {
    const name = app.name.toLowerCase();
    let rank = rankApp(name, q);
    let fz = -1;
    if (rank < 0) {
      fz = fuzzyApp(name, q);
      if (fz >= 0) rank = 4;
    }
    if (rank >= 0) scored.push([rank, -fz, name, app]);
  }
  scored.sort((a, b) => a[0] - b[0] || a[1] - b[1] || a[2].localeCompare(b[2], "en", { sensitivity: "base" }));
  return scored.slice(0, MAX_APPS).map((s) => s[3]);
}

function bestCacheKey(qLower) {
  let best = null;
  for (const key of fileCache.keys()) {
    if (qLower.startsWith(key) && (best === null || key.length > best.length)) best = key;
  }
  return best;
}

function clientFiles(query) {
  const q = query.toLowerCase();
  const key = bestCacheKey(q) || q;
  const entry = fileCache.get(key);
  if (!entry || !entry.items) return [];
  const out = [];
  for (const f of entry.items) {
    if (f.name.toLowerCase().includes(q) || f.path.toLowerCase().includes(q)) {
      out.push(f);
      if (out.length >= MAX_ITEMS) break;
    }
  }
  return out;
}

function cacheFiles(query, page) {
  fileCache.set(query.toLowerCase(), page);
  while (fileCache.size > MAX_FILE_CACHE) {
    const first = fileCache.keys().next().value;
    fileCache.delete(first);
  }
}

// Instant, no-IPC render from the pools. Safe to call per keystroke.
function paintFromPools(query) {
  const q = query.trim();
  const canPaint = (appPoolLoaded && appPool.length > 0) || fileCache.size > 0;
  if (!canPaint) return false;
  const parts = [];
  if (appPoolLoaded && appPool.length) {
    for (const app of clientApps(q)) {
      if (parts.length >= MAX_ITEMS) break;
      parts.push(app);
    }
  }
  if (q.length >= MIN_FILE_QUERY_LEN) {
    for (const f of clientFiles(q)) {
      if (parts.length >= MAX_ITEMS) break;
      parts.push(f);
    }
  }
  if (!parts.length) return false;
  items = parts;
  selected = 0;
  render();
  return true;
}

// ── File paging state: total for the current query (0 = unknown) ────
let fileTotal = 0;
let loadingMore = false;

async function runSearch() {
  if (!invoke) return;
  const query = input.value.trim();
  const seq = ++searchSeq;

  const appList = appPoolLoaded
    ? clientApps(query)
    : await invoke("search_apps", { query });
  if (seq !== searchSeq) return;

  if (query.length < MIN_FILE_QUERY_LEN) {
    // Apps-only answer (or fallback when the pool wasn't ready in time).
    // Empty query keeps the FULL app list, not the ranked top-16.
    fileTotal = 0;
    items = appList.slice(0, query ? MAX_APPS : MAX_ITEMS);
    render();
    return;
  }

  // Skip the intermediate apps-only render if the pools already painted a
  // full picture — jumping straight to the final list avoids flicker.
  if (!appPoolLoaded) {
    items = appList.slice(0, MAX_APPS);
    render();
  }

  const page = await invoke("search_files", { query, offset: 0 });
  if (seq !== searchSeq) return;
  const files = page.items || [];
  fileTotal = page.total || 0;
  cacheFiles(query, { items: files, total: fileTotal });

  const all = appList.slice(0, MAX_APPS);
  for (const f of files) {
    if (all.length >= MAX_ITEMS) break;
    all.push(f);
  }
  items = all;
  render();
}

async function runSearchSafe() {
  try {
    await runSearch();
  } catch (error) {
    if (!invoke) return;
    statusEl.style.display = "";
    progressFill.style.display = "none";
    statusText.textContent = `Search failed: ${error}`;
  }
}

async function loadMoreFiles() {
  if (loadingMore) return;
  const query = input.value.trim();
  if (query.length < MIN_FILE_QUERY_LEN) return;
  const seq = searchSeq; // drop the page if the query moved on mid-flight
  const seen = items.filter((it) => it.kind === "file");
  if (seen.length >= MAX_TOTAL_FILES) {
    if (fileTotal > seen.length) fileTotal = seen.length; // reachable cap
    render();
    return;
  }
  loadingMore = true;
  try {
    const page = await invoke("search_files", { query, offset: seen.length });
    if (seq !== searchSeq || query !== input.value.trim()) return; // stale page
    const files = page.items || [];
    fileTotal = page.total || fileTotal;
    if (files.length) {
      // Dedupe against the CURRENT list (the authoritative render may have
      // replaced `items` while the page was in flight).
      const existing = new Set(items.filter((it) => it.path).map((it) => it.path));
      for (const f of files) {
        if (!existing.has(f.path)) items.push(f);
      }
      cacheFiles(query, { items: files, total: fileTotal });
      render();
    }
  } catch (error) {
    console.error("load more failed:", error);
  } finally {
    loadingMore = false;
  }
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
    else if (item.kind !== "more") files.push(item);
  }
  const groups = [];
  if (apps.length) groups.push({ label: "Applications", rows: apps });
  if (dirs.length) groups.push({ label: "Folders", rows: dirs });
  if (files.length) {
    const shown = files.length;
    const more = Math.max(0, fileTotal - shown);
    if (more > 0 && shown < MAX_TOTAL_FILES) {
      files.push({
        kind: "more",
        name: "Show more results",
        path: "more:" + input.value.trim().toLowerCase(),
        remainingLabel: `${more.toLocaleString()} more ${more === 1 ? "file" : "files"} · ↵ to load`,
      });
    }
    groups.push({
      label: fileTotal > 0 ? `Files · ${fileTotal.toLocaleString()}` : "Files",
      rows: files,
    });
  }
  return groups;
}

// ── Icon pipeline ───────────────────────────────────────────────────────
// Only rows that actually ENTER the viewport ask for an icon, in small
// batches, so a 500-row answer costs ~12 extraction calls instead of one
// giant blocking one. Until the real icon lands every row shows a colored
// letter chip (instant, zero IPC), so nothing ever reads as "loading".
const iconQueue = new Set();
let iconTimer = null;
const ICON_BATCH = 12;

const iconObserver = new IntersectionObserver(
  (entries) => {
    for (const en of entries) {
      if (!en.isIntersecting) continue;
      iconObserver.unobserve(en.target);
      const p = en.target.dataset.path;
      if (!p || iconCache.has(p.toLowerCase())) continue;
      iconQueue.add(p);
      scheduleIconDrain();
    }
  },
  { root: resultsEl, rootMargin: "200px 0px" }
);

function scheduleIconDrain() {
  if (iconTimer || !iconQueue.size) return;
  iconTimer = setTimeout(async () => {
    iconTimer = null;
    const batch = [];
    for (const p of iconQueue) {
      batch.push(p);
      iconQueue.delete(p);
      if (batch.length >= ICON_BATCH) break;
    }
    try {
      const map = await invoke("get_icons", { paths: batch });
      if (map) {
        for (const [path, uri] of Object.entries(map)) {
          iconCache.set(path.toLowerCase(), uri);
          const img =
            rowEls &&
            rowEls.find((el) => el && el.dataset.path === path)?.querySelector(".icon");
          if (img && img.src !== uri) {
            img.src = uri;
            img.closest(".result")?.classList.add("has-icon");
          }
        }
      }
    } catch {}
    if (iconQueue.size) scheduleIconDrain();
  }, 30);
}

// Highlight with a node cap: at most 3 match segments to keep node churn low
// while still giving clear visual feedback.
function highlightInto(parent, text, query) {
  const q = query.toLowerCase();
  if (!q) {
    parent.appendChild(document.createTextNode(text));
    return;
  }
  const lower = text.toLowerCase();
  let pos = 0;
  let matched = 0;
  let i = lower.indexOf(q);
  while (i !== -1 && matched < 3) {
    if (i > pos) parent.appendChild(document.createTextNode(text.slice(pos, i)));
    const mark = document.createElement("span");
    mark.className = "hl";
    mark.textContent = text.slice(i, i + q.length);
    parent.appendChild(mark);
    pos = i + q.length;
    matched += 1;
    i = lower.indexOf(q, pos);
  }
  if (pos < text.length) parent.appendChild(document.createTextNode(text.slice(pos)));
}

// ── Exclusive app states ───────────────────────────────────────────────
// "scan": the index is being built — ONLY the scanning page exists.
// "ready": the index is usable — ONLY the palette exists. Never both.
let appState = "unknown";
cardEl.style.display = "none"; // nothing renders until the first status arrives

function setState(state) {
  if (state === appState) return;
  appState = state;
  if (state === "scan") {
    scanStateEl.classList.add("visible");
    cardEl.style.display = "none";
    if (!scanStartAt) scanStartAt = Date.now();
    tickScanClock();
    if (scanRetryBtn) {
      scanRetryBtn.disabled = false;
      scanRetryBtn.textContent = "Try again";
    }
  } else {
    scanStateEl.classList.remove("visible");
    cardEl.style.display = "";
    scanStartAt = 0;
    input.focus();
  }
}

function tickScanClock() {
  if (appState !== "scan" || !scanStartAt || !scanElapsed) return;
  const s = Math.max(1, Math.round((Date.now() - scanStartAt) / 1000));
  const mm = String(Math.floor(s / 60)).padStart(2, "0");
  const ss = String(s % 60).padStart(2, "0");
  scanElapsed.textContent = `${mm}:${ss}`;
}
setInterval(tickScanClock, 1000);

// Row pool: rows keyed by path survive between renders. replaceChildren only
// re-parents existing nodes on the common path, so steady-state typing never
// creates DOM nodes — only diffs (name/text/icon) are touched.
//
// Renders go through a rAF gate: even if several sources (keystroke paint +
// authoritative answer) fire in the same frame, the DOM is touched at most
// once per frame, which removes the double-paint jank.
let renderQueued = false;
function render() {
  if (renderQueued) return;
  renderQueued = true;
  requestAnimationFrame(() => {
    renderQueued = false;
    renderNow();
  });
}

function renderNow() {
  const query = input.value;
  statusEl.style.display = "none";
  const groups = groupItems(items);
  const fragment = document.createDocumentFragment();
  rowEls = [];
  let flatIndex = 0;
  const rendered = new Set();

  for (const group of groups) {
    const header = document.createElement("div");
    header.className = "group-label";
    header.textContent = group.label;
    fragment.appendChild(header);

    for (const item of group.rows) {
      let el = rowPool.get(item.path);
      if (!el) {
        el = document.createElement("div");
        el.className = "result";
        const chip = document.createElement("span");
        chip.className = "chip";
        const img = document.createElement("img");
        img.className = "icon";
        img.alt = "";
        const text = document.createElement("div");
        const name = document.createElement("div");
        name.className = "name";
        const path = document.createElement("div");
        path.className = "path";
        const tag = document.createElement("span");
        tag.className = "tag";
        text.appendChild(name);
        text.appendChild(path);
        el.appendChild(chip);
        el.appendChild(img);
        el.appendChild(text);
        el.appendChild(tag);
        el._chip = chip;
        el._img = img;
        el._nameEl = name;
        el._pathEl = path;
        el._tag = tag;
        rowPool.set(item.path, el);
      }
      rendered.add(item.path);
      el.dataset.index = flatIndex;
      el.dataset.path = item.path;
      el._item = item;

      if (el._name !== item.name || el._q !== query) {
        el._nameEl.textContent = "";
        if (item.kind === "more") {
          el._nameEl.appendChild(document.createTextNode(item.name));
        } else {
          highlightInto(el._nameEl, item.name || item.path, query);
        }
        el._name = item.name;
        el._q = query;
        const initial = (item.name || item.path || "?")[0].toUpperCase();
        el._chip.textContent = initial;
        let h = 7;
        const seed = item.name || item.path || "";
        for (let i = 0; i < seed.length; i++) h = (h * 31 + seed.charCodeAt(i)) >>> 0;
        el._chip.style.setProperty("--chip-hue", String(h % 360));
      }
      const pathText = item.kind === "app" ? "" : item.kind === "more" ? item.remainingLabel : item.path;
      if (el._path !== pathText) {
        el._pathEl.textContent = pathText;
        el._path = pathText;
      }
      const tagText = item.kind === "app" ? "App" : item.kind === "more" ? "More" : item.is_dir ? "Folder" : "File";
      if (el._tagText !== tagText) {
        el._tag.textContent = tagText;
        el._tagText = tagText;
      }
      const iconKey = item.path.toLowerCase();
      const uri = iconCache.get(iconKey);
      if (el._iconKey !== iconKey) {
        el._iconKey = iconKey;
        if (uri) {
          el._img.src = uri;
          el.classList.add("has-icon");
        } else {
          el._img.removeAttribute("src");
          el.classList.remove("has-icon");
        }
      } else if (uri && el._img.src !== uri) {
        el._img.src = uri;
        el.classList.add("has-icon");
      }

      el.classList.toggle("more", item.kind === "more");
      fragment.appendChild(el);
      rowEls[flatIndex] = el;
      flatIndex += 1;
    }
  }

  // Bound the pool so long sessions don't grow it unboundedly.
  if (rowPool.size > MAX_ITEMS + 40) {
    for (const key of rowPool.keys()) {
      if (!rendered.has(key)) {
        const stale = rowPool.get(key);
        if (stale) iconObserver.unobserve(stale);
        rowPool.delete(key);
      }
    }
  }

  resultsEl.replaceChildren(fragment);
  emptyEl.classList.toggle("visible", flatIndex === 0 && input.value.trim().length > 0);
  hintsEl.classList.toggle("visible", flatIndex > 0);
  for (let i = 0; i < rowEls.length; i++) {
    const el = rowEls[i];
    if (el && !el._observed && !el.classList.contains("more")) {
      el._observed = true;
      iconObserver.observe(el);
    }
  }
  updateSelection();
}

function updateSelection() {
  // The list may shrink between renders; never let the highlight (or Enter)
  // point at an index that no longer exists.
  if (selected < 0) selected = 0;
  if (selected >= rowEls.length) selected = Math.max(0, rowEls.length - 1);
  for (let i = 0; i < rowEls.length; i++) {
    const el = rowEls[i];
    if (!el) continue;
    el.classList.toggle("selected", i === selected);
  }
  const active = rowEls[selected];
  if (active && active.scrollIntoView) {
    active.scrollIntoView({ block: "nearest" });
  }
  renderPreview();
}

/* ── Preview pane ───────────────────────────────────────────────────────── */
const previewToggle = document.getElementById("previewToggle");
const previewPaneEl = document.getElementById("previewPane");
const pvSize = document.getElementById("pvSize");
const pvModified = document.getElementById("pvModified");
let previewHidden = localStorage.getItem("fs-preview-hidden") === "1";
let previewTimer = null;

function applyPreviewVisibility() {
  document.body.classList.toggle("preview-off", previewHidden);
  if (previewToggle) previewToggle.setAttribute("aria-pressed", String(!previewHidden));
}

// The window width follows the pane: wide (820) with preview, compact (560)
// without. JS steps the size frame-by-frame via IPC so the growth reads as
// one motion; onDone fires after the final frame.
let widthAnim = null;
function setWindowWidth(wide) {
  if (!invoke) return;
  invoke("resize_palette", { width: wide ? 820 : 560 }).catch(() => {});
}
function animateWindowWidth(wide, onDone) {
  if (widthAnim) cancelAnimationFrame(widthAnim);
  widthAnim = null;
  if (!invoke) {
    if (onDone) onDone();
    return;
  }
  const from = wide ? 560 : 820;
  const to = wide ? 820 : 560;
  const t0 = performance.now();
  const DUR = 230;
  const step = () => {
    const p = Math.min(1, (performance.now() - t0) / DUR);
    const e = 1 - Math.pow(1 - p, 3);
    const w = Math.round(from + (to - from) * e);
    invoke("resize_palette", { width: w }).catch(() => {});
    if (p < 1) {
      widthAnim = requestAnimationFrame(step);
    } else {
      widthAnim = null;
      if (onDone) onDone();
    }
  };
  step();
}

applyPreviewVisibility();
if (previewHidden) setWindowWidth(false);

if (previewToggle) {
  previewToggle.addEventListener("click", () => {
    const hiding = !previewHidden;
    previewHidden = hiding;
    localStorage.setItem("fs-preview-hidden", hiding ? "1" : "0");
    previewToggle.setAttribute("aria-pressed", String(!hiding));
    if (!hiding) {
      // Expanding: the pane appears right away and grows with the window.
      document.body.classList.remove("preview-off");
      renderPreview();
      animateWindowWidth(true);
    } else {
      // Collapsing: keep the pane laid out so the shrinking window clips it
      // away naturally; only then switch to full-width results.
      animateWindowWidth(false, () => {
        document.body.classList.add("preview-off");
        renderPreview();
      });
    }
  });
}

function fmtSize(bytes) {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1048576) return `${(bytes / 1024).toFixed(1)} KB`;
  if (bytes < 1073741824) return `${(bytes / 1048576).toFixed(1)} MB`;
  return `${(bytes / 1073741824).toFixed(2)} GB`;
}

function fmtRelative(secs) {
  const age = Math.max(0, Date.now() / 1000 - secs);
  if (age < 45) return "just now";
  if (age < 3600) return `${Math.floor(age / 60)} min ago`;
  if (age < 86400) return `${Math.floor(age / 3600)} hr ago`;
  return `${Math.floor(age / 86400)} days ago`;
}

async function previewMeta(path) {
  clearTimeout(previewTimer);
  previewTimer = setTimeout(async () => {
    if (previewHidden || !path || !invoke) return;
    try {
      const info = await invoke("file_preview", { path });
      pvSize.textContent = info && info.is_dir ? "—" : fmtSize(info.size);
      pvModified.textContent = info && info.modified_secs ? fmtRelative(info.modified_secs) : "—";
    } catch (error) {
      pvSize.textContent = "—";
      pvModified.textContent = "—";
    }
  }, 60);
}

function renderPreview() {
  // Called on every selection change (and on search re-render). All DOM
  // work is confined to the pane; the stat itself is debounced + skipped
  // entirely while the pane is hidden.
  const row = rowEls[selected];
  const item = row && row._item;
  const pane = previewPaneEl;
  if (!pane) return;
  if (previewHidden || !item) {
    pane.classList.add("empty-preview");
    return;
  }
  pane.classList.remove("empty-preview");

  const title = document.getElementById("pvTitle");
  const type = document.getElementById("pvType");
  const path = document.getElementById("pvPath");
  const iconEl = document.getElementById("pvIcon");
  const snippet = document.getElementById("pvSnippet");

  title.textContent = item.name || item.path;
  type.textContent =
    item.kind === "app" ? "Application" : item.kind === "more" ? "More results" : item.is_dir ? "Folder" : "File";
  pane.classList.toggle("pv-app", item.kind === "app");
  path.textContent = item.path || "";
  path.title = item.path || "";

  if (row.classList.contains("has-icon") && row._img && row._img.src) {
    iconEl.textContent = "";
    const img = new Image();
    img.src = row._img.src;
    img.classList.add("pv-img");
    iconEl.appendChild(img);
  } else if (row._chip) {
    iconEl.innerHTML = "";
    const chipText = document.createElement("span");
    chipText.className = "pv-initial";
    chipText.textContent = row._chip.textContent;
    iconEl.appendChild(chipText);
    iconEl.style.setProperty("--chip-hue", row._chip.style.getPropertyValue("--chip-hue"));
  }

  const hasStat = item.kind === "file" || item.kind === "dir" || item.kind === "app";
  snippet.textContent = item.kind === "file" || item.kind === "dir" ? item.path || "" : "";
  if (hasStat && item.path) {
    previewMeta(item.path);
  } else {
    pvSize.textContent = "—";
    pvModified.textContent = "—";
  }
}

// The displayed row order (grouped: Applications → Folders → Files) is NOT
// the backend order, so actions always run against the item attached to the
// highlighted row itself. This also makes the synthesized "more" row a real,
// actionable item instead of a dead cell.
async function openSelected(mode) {
  const row = rowEls[selected];
  const item = (row && row._item) || items[selected];
  if (!item) {
    const query = input.value.trim();
    if (query) {
      try {
        await invoke("open_web_search", { query });
        await invoke("hide_window");
      } catch (error) {
        showActionError(query, error);
      }
    }
    return;
  }
  if (item.kind === "more") {
    await loadMoreFiles();
    return;
  }
  const cmd =
    mode === "parent"
      ? "open_parent"
      : mode === "admin"
        ? "launch_admin"
        : mode === "props"
          ? "open_properties"
          : item.kind === "app"
            ? "launch_app"
            : "open_path";
  try {
    await invoke(cmd, { path: item.path });
    await invoke("hide_window");
  } catch (error) {
    // Keep the palette open and say why instead of silently doing nothing.
    showActionError(item.name || item.path, error);
  }
}

function showActionError(what, error) {
  statusEl.style.display = "";
  progressFill.style.display = "none";
  statusText.textContent = `Could not open "${what}": ${error}`;
}

input.addEventListener("input", () => {
  paintFromPools(input.value); // instant, zero IPC
  scheduleSearch(); // authoritative backfill
});

async function refreshStatus() {
  if (!invoke) return;
  try {
    const status = await invoke("get_index_status");
    // The app pool lives on the backend and only changes on install or
    // uninstall; the rev counter tells us when to re-fetch it (cheap).
    if (status && typeof status.apps_rev === "number" && status.apps_rev !== appPoolRev) {
      appPoolRev = status.apps_rev;
      loadApps(true);
    }
    // Scanning (first install or cache rebuild): ONLY the scanning page.
    if (!status || !status.ready) {
      setState("scan");
      const first = !!(status && status.first_scan);
      scanTitle.textContent = first ? "Welcome to FastSeek" : "Indexing files";
      scanSub.textContent = first
        ? "This is your first launch — FastSeek is scanning and indexing your drives. It only happens once."
        : "FastSeek is rebuilding its file index. Search returns once it's ready.";
      scanStatusText.textContent = (status && status.message) || "Scanning your drives…";
      return;
    }
    // Ready: ONLY the palette.
    setState("ready");
    statusEl.style.display = "none";
  } catch (error) {
    setState("scan");
    scanStatusText.textContent = `Backend unavailable: ${error}`;
  }
}

let lastNavKeyAt = 0; // hover never yanks the selection right after a keystroke

window.addEventListener("keydown", async (event) => {
  if (event.key === "Escape") {
    event.preventDefault();
    // First Esc clears the query (Raycast/Spotlight convention); a second
    // Esc (empty query) hides the window.
    if (input.value.trim()) {
      input.value = "";
      selected = 0;
      fileTotal = 0;
      lastNavKeyAt = Date.now();
      paintFromPools("");
      scheduleSearch();
    } else if (invoke) {
      await invoke("hide_window");
    }
    return;
  }

  if (
    event.ctrlKey &&
    !event.altKey &&
    !event.shiftKey &&
    (event.key === "c" || event.key === "C") &&
    input.selectionStart === input.selectionEnd
  ) {
    // Ctrl+C without a text selection in the box copies the highlighted row.
    const item = rowEls[selected] && rowEls[selected]._item;
    if (item && invoke) {
      event.preventDefault();
      try {
        await invoke("copy_path", { path: item.path });
        statusEl.style.display = "";
        progressFill.style.display = "none";
        statusText.textContent = "Path copied to clipboard";
        setTimeout(() => {
          statusEl.style.display = "none";
        }, 1500);
      } catch {}
    }
    return;
  }

  if (event.key === "ArrowDown") {
    event.preventDefault();
    lastNavKeyAt = Date.now();
    if (selected < rowEls.length - 1) {
      selected += 1;
      updateSelection();
    }
    return;
  }

  if (event.key === "ArrowUp") {
    event.preventDefault();
    lastNavKeyAt = Date.now();
    if (selected > 0) {
      selected -= 1;
      updateSelection();
    }
    return;
  }

  if (event.key === "PageDown") {
    event.preventDefault();
    lastNavKeyAt = Date.now();
    const step = event.shiftKey ? 10 : 12;
    selected = Math.min(Math.max(rowEls.length - 1, 0), (selected < 0 ? 0 : selected) + step);
    updateSelection();
    return;
  }

  if (event.key === "PageUp") {
    event.preventDefault();
    lastNavKeyAt = Date.now();
    const step = event.shiftKey ? 10 : 12;
    selected = Math.max(0, selected - step);
    updateSelection();
    return;
  }

  if (event.key === "Home") {
    event.preventDefault();
    lastNavKeyAt = Date.now();
    selected = 0;
    updateSelection();
    return;
  }

  if (event.key === "End") {
    event.preventDefault();
    lastNavKeyAt = Date.now();
    selected = Math.max(rowEls.length - 1, 0);
    updateSelection();
    return;
  }

  if (event.key === "Enter") {
    event.preventDefault();
    const query = input.value.trim();
    if (event.altKey) {
      await openSelected("props");
      return;
    }
    if (event.shiftKey) {
      await openSelected("admin");
      return;
    }
    if (query.includes(".com") && !items.length) {
      await invoke("open_web_search", { query });
      await invoke("hide_window");
      return;
    }
    await openSelected(event.ctrlKey ? "parent" : "open");
  }
});

resultsEl.addEventListener("mousemove", (event) => {
  if (Date.now() - lastNavKeyAt < 300) return;
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
  openSelected(event.ctrlKey ? "parent" : "open");
});

// Raycast-style pagination: reaching the bottom of the list pulls the next
// page automatically (plus ↵/click on the "Show more" row).
resultsEl.addEventListener(
  "scroll",
  () => {
    // The "more" row exists only in the rendered DOM (grouped order), never
    // in `items`, so detect it on the rendered rows.
    if (resultsEl.scrollTop + resultsEl.clientHeight >= resultsEl.scrollHeight - 120) {
      const hasMore = rowEls.some((el) => el && el.classList.contains("more"));
      if (hasMore) loadMoreFiles();
    }
  },
  { passive: true }
);

window.addEventListener("focus", () => {
  input.focus();
  // Selecting the whole query on every show wipes it on the next keystroke;
  // do it once per session only.
  if (!firstInitDone) {
    focusInitDone = true;
    input.select();
  }
  refreshStatus();
});

input.addEventListener("focus", () => {
  if (!items.length) runSearchSafe();
});

if (scanRetryBtn) {
  scanRetryBtn.addEventListener("click", async () => {
    if (!invoke || scanRetryBtn.disabled) return;
    scanRetryBtn.disabled = true;
    scanRetryBtn.textContent = "Rebuilding…";
    scanStartAt = Date.now();
    try {
      await invoke("rebuild_index");
    } catch (error) {
      scanStatusText.textContent = `Rebuild failed: ${error}`;
      scanRetryBtn.disabled = false;
      scanRetryBtn.textContent = "Try again";
    }
  });
}
if (scanQuitBtn) {
  scanQuitBtn.addEventListener("click", () => {
    if (invoke) invoke("quit_app");
  });
}

setInterval(refreshStatus, 1500);
refreshStatus();
loadApps();
input.focus();