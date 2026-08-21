// ── Tauri v1 IPC core (vendored — contract verified against tauri-1.8.3)
// With withGlobalTauri:false the runtime injects ONLY window.__TAURI_IPC__
// ({cmd, callback, error, payload} — scripts/ipc.js stamps the invoke key
// itself); responses arrive as Rust-eval'd window['_<id>'](<payload>) and
// events as window['<id>']({event, windowLabel, payload}). This is the same
// contract @tauri-apps/api implements; vendored here so the static frontend
// needs no bundler. The __TAURI__ fallbacks keep the page working if the
// flag is re-enabled for local development.
let __cbSeq = 0;
const __regist = (fn, once) => {
  const id = ++__cbSeq;
  const key = `_${id}`;
  window[key] = (resp) => {
    if (once) delete window[key];
    fn(resp);
  };
  return id;
};
const __invokeCore = (cmd, args) =>
  new Promise((resolve, reject) => {
    if (!window.__TAURI_IPC__) {
      reject(new Error("no IPC bridge"));
      return;
    }
    const ok = __regist(resolve, true);
    const err = __regist(reject, true);
    // Wire format (verified against bundle.global.js invoke + hooks.rs
    // InvokePayload flatten): args ride at TOP LEVEL as siblings of
    // cmd/callback/error — NOT under a `payload` key. The backend's
    // #[serde(flatten)] inner cast collects every remaining field and
    // command.rs extracts args by key; a stray `payload:` wrapper makes
    // even no-arg commands reject (unit deserialization fails on a map).
    window.__TAURI_IPC__({ cmd, callback: ok, error: err, ...(args || {}) });
  });
const __listenCore = (event, handler) => {
  const h = __regist(handler);
  // v1.8 built-in modules route through cmd "tauri" + __tauriModule/message
  // (NOT "plugin:event" — that's the 1.9+/v2 surface). The listen invoke
  // resolves with the backend-generated event id, which unlisten needs.
  return __invokeCore("tauri", {
    __tauriModule: "Event",
    message: { cmd: "listen", event, windowLabel: null, handler: h },
  }).then((eventId) => () =>
    __invokeCore("tauri", {
      __tauriModule: "Event",
      message: { cmd: "unlisten", event, eventId },
    })
  );
};
const __emitCore = (event, payload) =>
  __invokeCore("tauri", {
    __tauriModule: "Event",
    message: { cmd: "emit", event, payload },
  });
const __T = window.__TAURI__;
const invoke = __T?.tauri?.invoke || __T?.invoke || __invokeCore;
const listen = (event, handler) =>
  (window.__TAURI_INTERNALS__?.listen || __T?.event?.listen || __listenCore)(event, handler);
const updater =
  __T?.updater || {
    // v1.8 updater is event-driven (verified in bundle.global.js + the
    // backend's updater listener): checkUpdate/installUpdate subscribe for
    // status events and EMIT the trigger event; onUpdaterEvent unwraps the
    // status payload for the persistent listener.
    checkUpdate: () =>
      new Promise((resolve, reject) => {
        let closed = false;
        const offs = [];
        const close = () => {
          closed = true;
          offs.forEach((o) => {
            try {
              o();
            } catch {}
          });
          offs.length = 0;
        };
        const arm = (p) =>
          p
            .then((o) => {
              if (closed) {
                try {
                  o();
                } catch {}
              } else offs.push(o);
            })
            .catch(() => {});
        arm(
          listen("tauri://update-available", (e) => {
            const p = e && e.payload;
            close();
            resolve({ manifest: p, shouldUpdate: true });
          })
        );
        arm(
          listen("tauri://update-status", (e) => {
            const p = e && e.payload;
            if (!p) return;
            if (p.error) {
              const err = p.error;
              close();
              reject(err);
            } else if (p.status === "UPTODATE") {
              close();
              resolve({ shouldUpdate: false });
            }
          })
        );
        __emitCore("tauri://update").catch(() => {});
      }),
    installUpdate: () =>
      new Promise((resolve, reject) => {
        let closed = false;
        const offs = [];
        const close = () => {
          closed = true;
          offs.forEach((o) => {
            try {
              o();
            } catch {}
          });
          offs.length = 0;
        };
        const arm = (p) =>
          p
            .then((o) => {
              if (closed) {
                try {
                  o();
                } catch {}
              } else offs.push(o);
            })
            .catch(() => {});
        arm(
          listen("tauri://update-status", (e) => {
            const p = e && e.payload;
            if (!p) return;
            if (p.error) {
              const err = p.error;
              close();
              reject(err);
            } else if (p.status === "DONE") {
              close();
              resolve();
            }
          })
        );
        __emitCore("tauri://update-install").catch(() => {});
      }),
    onUpdaterEvent: (handler) =>
      listen("tauri://update-status", (e) => handler(e && e.payload)),
  };

// Page-error traps: any uncaught exception or rejection after this point
// gets reported through js_log (works via bundle OR vendored path), so a
// silent boot death is never silent again.
window.addEventListener("error", (ev) =>
  invoke("js_log", {
    msg: "pageerr " + ev.message + " @ " + ev.filename + ":" + ev.lineno,
  }).catch(() => {})
);
window.addEventListener("unhandledrejection", (ev) =>
  invoke("js_log", { msg: "unhd " + String(ev.reason) }).catch(() => {})
);

// Privileged commands (launch_admin, uninstall_app, image_data,
// grab_backdrop) require the per-session nonce (S4): fetched once at boot
// and attached to every privileged call. A page whose static resources were
// tampered with cannot drive them without a live nonce round-trip.
const nonceP = invoke("get_nonce").catch(() => "");
async function privileged(cmd, payload) {
  const nonce = await nonceP;
  return invoke(cmd, { ...(payload || {}), nonce });
}

// Boot-time IPC environment probe (diagnostic). Reports exactly what the
// runtime injected so the withGlobalTauri:false re-attempt can be built
// against facts instead of assumptions. Fully guarded — never raises.
try {
  const env = {
    ipc: typeof window.__TAURI_IPC__,
    postMsg: typeof window.__TAURI_POST_MESSAGE__,
    winIpc: typeof window.ipc,
    tauri: typeof window.__TAURI__,
    pattern: typeof window.__TAURI_PATTERN__,
    key: typeof window.__TAURI_INVOKE_KEY__,
    internals: typeof window.__TAURI_INTERNALS__,
  };
  invoke("js_log", { msg: "env " + JSON.stringify(env) }).catch(() => {});
  invoke("get_nonce")
    .then((n) => invoke("js_log", { msg: "nonce_len " + String(n ? n.length : 0) }))
    .catch(() => {});
  // S4 A/B: prove the vendored __TAURI_IPC__ path can talk to the backend
  // while the bundle keeps the page healthy. If the raw message arrives,
  // js_log itself logs "[js] vendored-arrived" — double confirmation.
  // ok      => vendored core logic is sound; a false-mode failure would be
  //            an injection difference (e.g. __TAURI_IPC__ not present).
  // no-bridge => __TAURI_IPC__ missing even WITH the bundle: ipc.js gate.
  // timeout  => message sent but never answered: shape/invoke-key problem.
  const s4Report = (m) => invoke("js_log", { msg: m }).catch(() => {});
  const s4VendoredTest = (tag) =>
    new Promise((resolve) => {
      if (typeof window.__TAURI_IPC__ !== "function") {
        resolve(tag + " no-bridge");
        return;
      }
      const timer = setTimeout(() => resolve(tag + " timeout"), 3000);
      __invokeCore("js_log", { msg: tag + "-arrived" })
        .then(() => {
          clearTimeout(timer);
          resolve(tag + " ok");
        })
        .catch((e) => {
          clearTimeout(timer);
          resolve(tag + " err " + String(e));
        });
    });
  s4VendoredTest("vendored").then((r) => s4Report("S4-AB: " + r));
  __listenCore("__s4probe", () => {})
    .then(() => s4Report("S4-AB: listen ok"))
    .catch((e) => s4Report("S4-AB: listen err " + String(e)));
} catch (e) {
  invoke("js_log", { msg: "env probe failed: " + String(e) }).catch(() => {});
}

// Theme: follows the OS by default ("system"); a manual dark/light choice
// made in Settings (fs-theme) overrides it.
const themeQuery = window.matchMedia("(prefers-color-scheme: light)");
let userTheme = localStorage.getItem("fs-theme") || "system";
function applySystemTheme() {
  if (userTheme !== "system") return;
  document.documentElement.setAttribute(
    "data-theme",
    themeQuery.matches ? "light" : "dark"
  );
}
if (themeQuery.addEventListener) themeQuery.addEventListener("change", applySystemTheme);
else if (themeQuery.addListener) themeQuery.addListener(applySystemTheme);

// OS frosted glass (Mica/Acrylic) is applied from Rust at startup. If it
// could not be enabled, fall back to CSS-only blur so the palette never
// renders flat over an un-blurred desktop.
if (invoke) {
  invoke("backdrop_ok")
    .then((ok) => {
      if (!ok) document.body.classList.add("css-blur");
    })
    .catch(() => document.body.classList.add("css-blur"));
}

const cardEl = document.querySelector("#card");
const input = document.querySelector("#search");
const statusEl = document.querySelector("#status");
const statusText = document.querySelector("#statusText");
const progressFill = document.querySelector("#progressFill");
const resultsEl = document.querySelector("#results");
const scanNoticeEl = document.getElementById("scanNotice");
const emptyEl = document.querySelector("#empty");
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

// Settings deep links have no file to extract an icon from; give them
// inline glyphs (gear for everything, Wi-Fi/Bluetooth for their own pages).
const svgIcon = (body) =>
  `data:image/svg+xml,${encodeURIComponent(
    `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="rgba(255,255,255,0.9)" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">${body}</svg>`
  )}`;
const SETTINGS_ICON = svgIcon(
  '<circle cx="12" cy="12" r="3"/><path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 0 1 0 2.83 2 2 0 0 1-2.83 0l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-2 2 2 2 0 0 1-2-2v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 0 1-2.83 0 2 2 0 0 1 0-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1-2-2 2 2 0 0 1 2-2h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 0 1 0-2.83 2 2 0 0 1 2.83 0l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 2-2 2 2 0 0 1 2 2v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 0 1 2.83 0 2 2 0 0 1 0 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 2 2 2 2 0 0 1-2 2h-.09a1.65 1.65 0 0 0-1.51 1z"/>'
);
const WIFI_ICON = svgIcon(
  '<path d="M5 13a10 10 0 0 1 14 0"/><path d="M8.5 16.5a5 5 0 0 1 7 0"/><path d="M2 8.82a15 15 0 0 1 20 0"/><line x1="12" y1="20" x2="12.01" y2="20"/>'
);
const BLUETOOTH_ICON = svgIcon('<path d="m7 7 10 10-5 5V2l5 5L7 17"/>');
const SETTINGS_ICON_BY_PATH = {
  "ms-settings:wifi": WIFI_ICON,
  "ms-settings:bluetooth": BLUETOOTH_ICON,
};
function settingsIconFor(path) {
  const low = path.toLowerCase();
  if (!low.startsWith("ms-settings:")) return null;
  return SETTINGS_ICON_BY_PATH[low] || SETTINGS_ICON;
}
// Instant shared glyphs for real file/folder rows: the Windows shell icon
// (folder type, PDF, exe brand, …) still arrives a beat later from the icon
// pool and replaces these — they exist to end the letter-chip wait. They are
// deliberately NOT stored in iconCache: that map also gates the icon observer
// (has → real icon never requested), so caching a placeholder would freeze
// every file row on the generic glyph forever.
const GENERIC_FOLDER_ICON = svgIcon(
  '<path d="M3 7a1 1 0 0 1 1-1h4l2 2h10a1 1 0 0 1 1 1v9a1 1 0 0 1-1 1H4a1 1 0 0 1-1-1z"/>'
);
const GENERIC_FILE_ICON = svgIcon(
  '<path d="M6 3h8l4 4v14a1 1 0 0 1-1 1H6a1 1 0 0 1-1-1V4a1 1 0 0 1 1-1z"/><path d="M14 3v4h4"/>'
);
// Web-search rows (a leading "@") get a dedicated globe mark tinted with the
// app's accent color. This is NOT optional decoration: the row's "path" is a
// search term or domain, and without an icon here the viewport icon fetch
// extracts a FILE icon for it — the generic white sheet with the blue
// rectangle — and swaps it in over the globe chip. Seeding it into the icon
// cache also keeps that fetch from ever running for web rows. Built per
// render so it follows a runtime accent change (Windows accent / theme).
function webSearchIconUri() {
  const hex =
    getComputedStyle(document.documentElement).getPropertyValue("--accent-blue").trim() ||
    ACCENT_DEFAULTS.hex;
  const svg =
    `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="${hex}" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">` +
    `<circle cx="12" cy="12" r="9"/><ellipse cx="12" cy="12" rx="9" ry="3.6"/><ellipse cx="12" cy="12" rx="3.6" ry="9"/></svg>`;
  return `data:image/svg+xml,${encodeURIComponent(svg)}`;
}
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
      endBootSettleWhenReady();
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
  // hard list guard. Settings subpages ("Settings: Wi-Fi", ...) are searchable
  // but are NOT apps: they never appear in the browse list.
  if (!q) {
    return appPool.filter((a) => !a.name.startsWith("Settings: ")).slice(0, MAX_ITEMS);
  }
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
  // Path queries resolve against the live filesystem, not the pools —
  // painting stale app/file rows under them would just flicker.
  if (isPathQuery(q)) return false;
  // A leading "@" is a web search: one row, always rendered (even before
  // the pools load on a first run). No fetched suggestions — the row is
  // exactly what the user typed. Enter opens it in the browser.
  if (q.startsWith("@")) {
    const web = q.slice(1).trim();
    if (web) {
      const looksLikeUrl =
        /^[\w-]+(\.[\w-]+)+$/.test(web) || /^https?:\/\/.+/i.test(web);
      items = [{
        kind: "web",
        name: looksLikeUrl ? `Open ${web}` : "Open in browser",
        domain: looksLikeUrl ? hostOf(web) : web,
        path: web,
      }];
      selected = 0;
      render();
      return true;
    }
  }
  const canPaint = (appPoolLoaded && appPool.length > 0) || fileCache.size > 0;
  if (!canPaint) return false;
  // Math queries paint instantly — no backend round-trip needed.
  const math = mathEnabled ? tryMath(q) : null;
  if (math) {
    items = [math];
    selected = 0;
    render();
    return true;
  }
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

// First-run scan state: the backend's index is still being built. The
// notice owns the results column only while the query is empty; typing
// brings back normal app/file search over the partial index immediately.
let indexReady = false;
let firstScanActive = false; // corrected on the first status poll; false means no notice-flash on warm starts
let scanNoticeShown = false;
function syncScanNotice() {
  if (!scanNoticeEl) return;
  const show = appState === "ready" && firstScanActive && !indexReady && !input.value.trim();
  if (show === scanNoticeShown) return;
  scanNoticeShown = show;
  if (show) {
    // render()/replaceChildren may have detached the notice — re-insert.
    if (!scanNoticeEl.isConnected) resultsEl.prepend(scanNoticeEl);
    scanNoticeEl.classList.add("visible");
    for (const el of [...resultsEl.children]) {
      if (el !== scanNoticeEl) el.remove();
    }
    // Nothing to preview while the notice owns the column either.
    document.body.classList.add("no-results");
    fileTotal = 0;
  } else {
    scanNoticeEl.classList.remove("visible");
    document.body.classList.remove("no-results");
    // The scan finished while the notice stood in for the app list —
    // restore it (only when nothing is typed).
    if (appState === "ready" && !input.value.trim()) paintFromPools("");
  }
}

async function runSearch() {
  if (!invoke) return;
  const query = input.value.trim();
  const seq = ++searchSeq;

  // A leading "@" never touches the index — paintFromPools owns the single
  // web-search row and Enter hands the query to the browser directly.
  if (query.startsWith("@") && query.length > 1) return;

  // Path queries bypass the index entirely — the backend walks the live
  // filesystem (Exact hit → one row; partial path → bounded subtree walk).
  if (isPathQuery(query)) {
    try {
      const pl = await invoke("search_path", { query });
      if (seq !== searchSeq) return;
      items = pl || [];
      selected = 0;
      fileTotal = items.length;
      render();
      return;
    } catch (error) {
      console.error("path search failed:", error);
      // Fall through to the normal search rather than blanking the row.
    }
  }

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

// ── Inline web search ───────────────────────────────────────────────────
// Triggered by a leading "@": a single row showing exactly what the user
// typed (never fetched suggestions). Enter/click opens it in the browser.
function hostOf(url) {
  try {
    return new URL(url).hostname.replace(/^www\./, "");
  } catch {
    return "";
  }
}

async function loadMoreFiles() {
  if (loadingMore) return;
  const query = input.value.trim();
  if (query.length < MIN_FILE_QUERY_LEN) return;
  if (isPathQuery(query)) return; // path walks are served in one bounded shot
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

// Long paths get middle-truncated ("C:\Users\me\…\reports\q3.xlsx"); the
// full path stays available in the row's title tooltip.
function truncatePath(p) {
  if (!p) return "";
  if (p.length <= 48) return p;
  const parts = p.split(/[\\/]/).filter(Boolean);
  if (parts.length < 3) {
    // "aumid:..."-style ids and separator-less paths can't be
    // middle-truncated on a folder boundary — clip the middle instead.
    return p.slice(0, 22) + "…" + p.slice(-22);
  }
  let head = "";
  if (parts[0] && parts[0].length === 2 && parts[0][1] === ":") {
    head = parts[0] + "\\" + parts[1] + "\\";
  } else {
    head = parts[0] + "\\";
  }
  const tail = parts.slice(-2).join("\\");
  const out = head + "…\\" + tail;
  return out.length < p.length ? out : p;
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
  const math = [];
  const apps = [];
  const dirs = [];
  const files = [];
  const web = [];
  for (const item of items) {
    if (item.kind === "math") math.push(item);
    else if (item.kind === "app") apps.push(item);
    else if (item.kind === "dir") dirs.push(item);
    else if (item.kind === "web") web.push(item);
    else if (item.kind !== "more") files.push(item);
  }
  const groups = [];
  if (web.length) groups.push({ label: "Web", rows: web });
  if (math.length) groups.push({ label: "Calculation", rows: math });
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
// Requeue a path whose icon answer didn't arrive in a batch (slow extraction
// or a cold start), capped so a genuinely broken path stops being retried
// after two misses. Without this a single transient timeout would leave the
// row on its letter chip for the whole render.
const iconRetries = new Map();
function retryIconPath(path) {
  const key = path.toLowerCase();
  if (key.startsWith("ms-settings:")) return;
  const tries = (iconRetries.get(key) || 0) + 1;
  if (tries <= 2 && !iconCache.has(key)) {
    iconRetries.set(key, tries);
    iconQueue.add(path);
  }
}

const iconObserver = new IntersectionObserver(
  (entries) => {
    for (const en of entries) {
      if (!en.isIntersecting) continue;
      iconObserver.unobserve(en.target);
      const p = en.target.dataset.path;
      if (!p || iconCache.has(p.toLowerCase())) continue;
      if (p.toLowerCase().startsWith("ms-settings:")) continue;
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
            // The preview pane mirrors the selected row — refresh it when the
            // real icon lands, or it keeps the letter chip until the user
            // happens to move the selection.
            if (rowEls[selected] === img.closest(".result")) renderPreview();
          }
        }
      }
      // Paths missing from the answer get retried (bounded) instead of being
      // left on their letter chip.
      for (const path of batch) {
        if (!map || !(path in map)) retryIconPath(path);
      }
    } catch {
      for (const path of batch) retryIconPath(path);
    }
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

function setState(state) {
  if (state === appState) return;
  appState = state;
  if (state === "scan") {
    scanStateEl.classList.add("visible");
    cardEl.style.display = "none";
    if (!scanStartAt) scanStartAt = Date.now();
    armScanClock();
    if (scanRetryBtn) {
      scanRetryBtn.disabled = false;
      scanRetryBtn.textContent = "Try again";
    }
  } else {
    disarmScanClock();
    scanStateEl.classList.remove("visible");
    cardEl.style.display = "";
    scanStartAt = 0;
    input.focus();
    // The palette just appeared (possibly after a long scan): re-measure the
    // height now that the card is visible, or the list opens at the stale
    // scan-page height (~3 rows) until the next interaction re-syncs it.
    syncCardHeight();
  }
}

function tickScanClock() {
  if (appState !== "scan" || !scanStartAt || !scanElapsed) return;
  const s = Math.max(1, Math.round((Date.now() - scanStartAt) / 1000));
  const mm = String(Math.floor(s / 60)).padStart(2, "0");
  const ss = String(s % 60).padStart(2, "0");
  scanElapsed.textContent = `${mm}:${ss}`;
}

// The scan clock only ticks while the scan page is visible: arm it on
// entering "scan", clear it when leaving, so a session parked on the ready
// state doesn't keep a 1 Hz timer alive for the whole process lifetime.
let scanClockTimer = null;
function armScanClock() {
  if (scanClockTimer) return;
  scanClockTimer = setInterval(tickScanClock, 1000);
  tickScanClock();
}
function disarmScanClock() {
  if (scanClockTimer) { clearInterval(scanClockTimer); scanClockTimer = null; }
}

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

/* The card's height follows its content but `height: auto` can't be
   transitioned, so we measure and set it explicitly — debounced by ~90ms so
   fast typing settles before the card glides. The probe (auto → read →
   px) runs with transitions OFF inside a single synchronous frame, so it
   never paints; the glide itself is then driven with the Web Animations
   API from the previous px height, so every results-count change animates
   the card's bottom edge. */
const cardWinEl = document.querySelector(".launcher-window");
let cardHeightTimer = null;
let cardHeightShown = null;
// Whether cardHeightShown was measured while the palette was VISIBLE. The
// 1.5s status poll re-runs setState("ready") → syncCardHeight even while
// hidden, measuring the emptied list at the 210px floor — without this flag
// every reopen glided 210 → full from that poisoned baseline.
let cardHeightShownVisible = false;
/* Height glides are suspended until the initial post-boot settle finishes
   (first show + app pool rendered + a short grace). A cold-boot summon that
   beats page load would otherwise watch the card animate 210px → full — the
   "roll-down". Snapping during the settle reads as "opened"; after the
   settle, normal glides resume within the same session. */
let bootSettle = true;
let bootShown = false;
let bootSettleTimer = null;
function endBootSettleWhenReady() {
  if (!bootSettle || !bootShown || !appPoolLoaded || bootSettleTimer) return;
  bootSettleTimer = setTimeout(() => { bootSettle = false; }, 400);
}
const reduceMotion = window.matchMedia("(prefers-reduced-motion: reduce)");
function syncCardHeight() {
  clearTimeout(cardHeightTimer);
  cardHeightTimer = setTimeout(measureCardHeight, 90);
}
function measureCardHeight() {
  // While the scan page is up the card measures ~210px (its content); pin
  // that and the ready flip would open at "3 rows" until something else
  // re-syncs. Skip the pin entirely here — setState("ready") re-syncs it
  // once the palette is actually visible.
  if (scanStateEl.classList.contains("visible")) return;
  // Probe in "auto" (a px value can't measure content), then restore —
  // the deduped value must be re-set, otherwise the inline style is left
  // at "auto" and the card stops tracking its content.
  cardWinEl.classList.add("no-height-transition");
  cardWinEl.style.height = "auto";
  // Compact mode + empty query = just the search bar: allow the card to
  // shrink to the header instead of the normal 210px floor.
  const minH = document.body.classList.contains("compact-empty") ? 52 : 210;
  const h = Math.max(minH, Math.min(cardWinEl.scrollHeight, 520));
  cardWinEl.style.height = h + "px";
  cardWinEl.classList.remove("no-height-transition");
  const prev = cardHeightShown;
  const prevVisible = cardHeightShownVisible;
  cardHeightShown = h;
  cardHeightShownVisible = !document.hidden;
  if (prev != null && prevVisible && prev !== h && !reduceMotion.matches && !bootSettle) {
    cardWinEl.animate(
      [{ height: prev + "px" }, { height: h + "px" }],
      { duration: 180, easing: "cubic-bezier(0.16, 1, 0.3, 1)" }
    );
  }
}
// The debounce exists for fast typing; a math-query flip (panes dropping out
// of layout) must re-measure on the spot, or the card sits at its old height
// with a visible gap under the hero.
let mathWasOpen = false;
function syncCardHeightNow() {
  clearTimeout(cardHeightTimer);
  measureCardHeight();
}

function renderNow() {
  const query = input.value;
  // Math rows are synthesized per render from the current query: "2*8"
  // becomes "2*8 = 16" as the top result. Strip stale math rows first.
  items = items.filter((it) => it.kind !== "math");
  const math = mathEnabled ? tryMath(query) : null;
  if (math) items.unshift(math);
  // A math query turns the tool into a calculator: the hero spans the whole
  // card width (the preview pane drops out of layout) until the query clears.
  document.body.classList.toggle("math-open", !!math);
  // Compact mode: an empty query collapses the tool to just the search bar.
  document.body.classList.toggle("compact-empty", compactMode && !math && !input.value.trim());
  statusEl.style.display = "none";
  const groups = groupItems(items);
  const fragment = document.createDocumentFragment();
  rowEls = [];
  let flatIndex = 0;
  const rendered = new Set();

  for (const group of groups) {
    if (group.label !== "Calculation") {
      const header = document.createElement("div");
      header.className = "group-label";
      header.textContent = group.label;
      fragment.appendChild(header);
    }

    for (const item of group.rows) {
      // Math queries render as a hero card (like a calculator display): the
      // expression as tokens (numbers in chips, operators in red), a
      // "Calculator Result" badge, and the answer big and bold below.
      if (item.kind === "math") {
        let el = rowPool.get(item.path);
        if (!el) {
          el = document.createElement("div");
          el.className = "result calc-hero-row";
          const hero = document.createElement("div");
          hero.className = "calc-hero";
          const head = document.createElement("div");
          head.className = "calc-head";
          const tokens = document.createElement("div");
          tokens.className = "calc-tokens";
          const badge = document.createElement("span");
          badge.className = "calc-badge";
          badge.textContent = "Calculator Result";
          const result = document.createElement("div");
          result.className = "calc-result";
          head.appendChild(tokens);
          head.appendChild(badge);
          hero.appendChild(head);
          hero.appendChild(result);
          el.appendChild(hero);
          el._tokensEl = tokens;
          el._resultEl = result;
          rowPool.set(item.path, el);
        }
        rendered.add(item.path);
        el.dataset.index = flatIndex;
        el.dataset.path = item.path;
        el._item = item;
        if (el._expr !== item.expr) {
          el._tokensEl.replaceChildren(mathTokens(item.expr));
          el._expr = item.expr;
        }
        if (el._value !== item.value) {
          const n = Number(item.value);
          el._resultEl.textContent = Number.isFinite(n) ? n.toLocaleString() : item.value;
          el._value = item.value;
        }
        fragment.appendChild(el);
        rowEls[flatIndex] = el;
        flatIndex += 1;
        continue;
      }

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
        text.className = "text";
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
        el._nameEl.title = "";
        if (item.kind === "more") {
          el._nameEl.appendChild(document.createTextNode(item.name));
        } else {
          highlightInto(el._nameEl, item.name || item.path, query);
        }
        el._name = item.name;
        el._q = query;
        const isWeb = item.kind === "web";
        if (isWeb !== !!el._chipGlobe) {
          el._chipGlobe = isWeb;
          el._chip.innerHTML = isWeb
            ? "<svg viewBox=\"0 0 24 24\"><circle cx=\"12\" cy=\"12\" r=\"9\"/><ellipse cx=\"12\" cy=\"12\" rx=\"9\" ry=\"3.6\"/><ellipse cx=\"12\" cy=\"12\" rx=\"3.6\" ry=\"9\"/></svg>"
            : "";
        }
        el._chip.classList.toggle("chip-globe", isWeb);
        if (isWeb) {
          el._chip.style.removeProperty("--chip-hue");
        } else {
          el._chip.textContent = (item.name || item.path || "?")[0].toUpperCase();
          const seed = item.name || item.path || "";
          let h = 7;
          for (let i = 0; i < seed.length; i++) h = (h * 31 + seed.charCodeAt(i)) >>> 0;
          el._chip.style.setProperty("--chip-hue", String(h % 360));
        }
      }
      // No paths in result rows — apps, files and folders show name only
      // (the full path lives in the preview pane). Only the synthesized
      // "more" row keeps its count label; web rows show the domain.
      const pathText =
        item.kind === "more"
          ? item.remainingLabel
          : item.kind === "web"
            ? item.domain || ""
            : "";
      if (el._path !== pathText) {
        el._pathEl.textContent = pathText;
        el._pathEl.style.display = pathText ? "" : "none";
        el._pathEl.title = "";
        el._path = pathText;
      }
      const tagText = item.kind === "app" ? "App" : item.kind === "more" ? "More" : item.kind === "math" ? "Math" : item.kind === "web" ? "Web" : item.is_dir ? "Folder" : "File";
      if (el._tagText !== tagText) {
        el._tag.textContent = tagText;
        el._tagText = tagText;
      }
      const iconKey = item.path.toLowerCase();
      let uri = iconCache.get(iconKey);
      if (item.kind === "web") {
        // Search term/domain — there is no file to extract. Show the accent-
        // colored globe mark and seed the cache so the viewport observer
        // skips it (no bogus file-icon request, no generic-icon swap-in).
        uri = webSearchIconUri();
        iconCache.set(iconKey, uri);
      } else if (!uri && item.kind === "app") {
        uri = settingsIconFor(item.path);
      } else if (
        !uri &&
        item.kind !== "app" &&
        item.kind !== "more" &&
        item.kind !== "math"
      ) {
        // Real file/folder row: paint an instant shared glyph (folder or
        // document sheet) instead of a letter chip; the per-type shell icon
        // still extracts in the background and replaces it. Not cached — see
        // the GENERIC_* constants' comment.
        uri = item.is_dir ? GENERIC_FOLDER_ICON : GENERIC_FILE_ICON;
      }
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
  // With nothing to preview the pane adds nothing but an empty plate —
  // collapse it along with the "No results" state (renderPreview also
  // falls back to the empty plate when no row is selected).
  document.body.classList.toggle("no-results", flatIndex === 0 && input.value.trim().length > 0);
  const mathOpen = !!math;
  if (mathOpen !== mathWasOpen) {
    mathWasOpen = mathOpen;
    syncCardHeightNow();
  } else {
    syncCardHeight();
  }
  for (let i = 0; i < rowEls.length; i++) {
    const el = rowEls[i];
    if (el && !el._observed && !el.classList.contains("more") && !el.classList.contains("math")) {
      el._observed = true;
      iconObserver.observe(el);
    }
  }
  setNameTooltips();
  updateSelection();
}

// Full name on hover, but ONLY when the row actually clipped it ("…").
function setNameTooltips() {
  for (const el of rowEls) {
    if (!el || !el._nameEl) continue;
    const n = el._nameEl;
    if (n.scrollWidth > n.clientWidth + 2) n.title = n.textContent;
    else n.title = "";
  }
}

function updateSelection(scroll = true) {
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
  // Mouse hover/click must NOT scroll: hovering a bottom row would nudge the
  // list (scrollIntoView "nearest") and push the top group label out of
  // view. Only keyboard navigation scrolls the selection into view.
  if (active && active.scrollIntoView && scroll) {
    active.scrollIntoView({ block: "nearest" });
  }
  renderPreview();
}

/* ── Settings (the header gear opens these in place of the preview) ───── */
const previewPaneEl = document.getElementById("previewPane");
const pvSize = document.getElementById("pvSize");
const pvModified = document.getElementById("pvModified");
const pvPublisher = document.getElementById("pvPublisher");
const pvVersion = document.getElementById("pvVersion");
const settingsBtn = document.getElementById("settingsBtn");
const setPreviewSwitch = document.getElementById("setPreview");
const setAlpha = document.getElementById("setAlpha");
const setAlphaVal = document.getElementById("setAlphaVal");
const setThemeBtns = document.querySelectorAll("#setTheme button");
const setAccentSwitch = document.getElementById("setAccent");
const setAutostartSwitch = document.getElementById("setAutostart");
const setCompactSwitch = document.getElementById("setCompact");
const setRadius = document.getElementById("setRadius");
const setRadiusVal = document.getElementById("setRadiusVal");
const pvImgEl = document.getElementById("pvImg");
const pvImgCache = new Map(); // path -> base64 data URI
let previewHidden = localStorage.getItem("fs-preview-hidden") === "1";
let previewTimer = null;

// The pane is a flex sibling of #results (see styles.css): toggling the
// `preview-off` class animates its width/padding/opacity with a pure CSS
// transition. No native window resize, no overlay — the results column
// flexes to fill, so there is nothing to glitch and closing animates too.
function applyPreviewVisibility() {
  document.body.classList.toggle("preview-off", previewHidden);
  if (setPreviewSwitch) setPreviewSwitch.setAttribute("aria-checked", String(!previewHidden));
}

applyPreviewVisibility();

if (settingsBtn) {
  settingsBtn.addEventListener("click", () => {
    // A math query is a calculator — the card belongs to the result box
    // (both side panes drop out). Opening settings while it is active would
    // do nothing visible and pop open later when the query clears.
    if (document.body.classList.contains("math-open")) return;
    const open = !document.body.classList.contains("settings-open");
    document.body.classList.toggle("settings-open", open);
    settingsBtn.setAttribute("aria-pressed", String(open));
    if (!open) renderPreview();
    syncCardHeight();
  });
}

if (setPreviewSwitch) {
  setPreviewSwitch.addEventListener("click", () => {
    previewHidden = !previewHidden;
    localStorage.setItem("fs-preview-hidden", previewHidden ? "1" : "0");
    document.body.classList.toggle("preview-off", previewHidden);
    setPreviewSwitch.setAttribute("aria-checked", String(!previewHidden));
    renderPreview();
  });
}

// Instant math: typing "2*8" evaluates locally. Off = plain file search.
const setMathSwitch = document.getElementById("setMath");
let mathEnabled = localStorage.getItem("fs-math") !== "0";
if (setMathSwitch) {
  setMathSwitch.setAttribute("aria-checked", String(mathEnabled));
  setMathSwitch.addEventListener("click", () => {
    mathEnabled = !mathEnabled;
    localStorage.setItem("fs-math", mathEnabled ? "1" : "0");
    setMathSwitch.setAttribute("aria-checked", String(mathEnabled));
    paintFromPools(input.value);
    scheduleSearch();
  });
}

// Window transparency: the bg colors are composed here (they depend on the
// active theme) and set inline on <body>, which outranks the :root defaults.
// Setting only --window-alpha does NOT work: custom properties resolve their
// var() references at the declaration site (:root), not per element.
// The slider value is TRANSPARENCY: 0% = solid panel, 100% = nearly
// invisible. Stored under fs-alpha2 (a versioned key — the pre-2 build
// stored opacity and inverted >50 on every load, which felt reversed).
// Defaults are theme-scoped: 14% in light, 7% in dark — but only while the
// user has not moved the slider (fs-alpha2 set); a manual value wins.
const rawAlpha = parseInt(localStorage.getItem("fs-alpha2"), 10);
const alphaTouched = Number.isFinite(rawAlpha);
const alphaThemeDefault = () =>
  document.documentElement.getAttribute("data-theme") === "light" ? 14 : 7;
let alphaValue = 35;
function applyAlpha(v, persist = true) {
  alphaValue = v;
  const a = Math.max(0.12, 1 - v / 100);
  const dark = document.documentElement.getAttribute("data-theme") !== "light";
  const base = dark ? [28, 28, 30] : [250, 250, 252];
  const preview = dark ? [22, 23, 26] : [243, 243, 246];
  const pa = Math.max(0.12, a - 0.05);
  document.body.style.setProperty("--bg-window", `rgba(${base[0]}, ${base[1]}, ${base[2]}, ${a})`);
  document.body.style.setProperty("--bg-preview", `rgba(${preview[0]}, ${preview[1]}, ${preview[2]}, ${pa})`);
  document.body.style.setProperty("--window-alpha", String(a));
  if (setAlpha) {
    setAlpha.value = String(v);
    updateSliderFill(setAlpha);
  }
  if (setAlphaVal) setAlphaVal.textContent = v + "%";
  if (persist) localStorage.setItem("fs-alpha2", String(v));
}
applyAlpha(
  alphaTouched ? Math.max(0, Math.min(100, rawAlpha)) : alphaThemeDefault(),
  alphaTouched
);

// Frosted blur strength: 0% = no blur, 100% = heavy glass. Maps to a px
// radius via --blur-px (the glass layer's filter consumes the variable —
// the card itself has no backdrop-filter; blur is a single pass).
// Same theme-scoped defaulting as transparency: 56% light / 45% dark until
// the user moves the slider (fs-blur set).
const setBlur = document.getElementById("setBlur");
const setBlurVal = document.getElementById("setBlurVal");
const rawBlur = parseInt(localStorage.getItem("fs-blur"), 10);
const blurTouched = Number.isFinite(rawBlur);
const blurThemeDefault = () =>
  document.documentElement.getAttribute("data-theme") === "light" ? 56 : 45;
let blurValue = 50;
function applyBlur(v, persist = true) {
  blurValue = v;
  const px = Math.round((v / 100) * 40); // 0 → 0px, 100 → 40px
  document.body.style.setProperty("--blur-px", px + "px");
  if (setBlur) {
    setBlur.value = String(v);
    updateSliderFill(setBlur);
  }
  if (setBlurVal) setBlurVal.textContent = v + "%";
  if (persist) localStorage.setItem("fs-blur", String(v));
}
applyBlur(
  blurTouched ? Math.max(0, Math.min(100, rawBlur)) : blurThemeDefault(),
  blurTouched
);

if (setAlpha) {
  setAlpha.addEventListener("input", () => applyAlpha(Number(setAlpha.value)));
}
if (setBlur) {
  setBlur.addEventListener("input", () => applyBlur(Number(setBlur.value)));
}

// Real frosted glass: the backend captures the desktop behind the window
// (while it was hidden) and we layer it behind the card, CSS-filter-blurred.
// The backdrop-filter rules can't see the desktop through a transparent
// WebView2 window — this layer is what actually makes blur visible.
const glassLayerEl = document.getElementById("glassLayer");
let glassBackdrop = null; // { uri, w, h } from grab_backdrop
let glassRect = null;
let glassOb = null; // ResizeObserver driving the glass rect (U1)
let glassResizeHandler = null;

// The glass layer tracks the card (or the scan card while the palette is
// hidden) so the blur stays INSIDE the search tool: same rect, same radius,
// only a 24px invisible bleed for blur sampling, trimmed by clip-path.
function syncGlassRect() {
  if (!glassLayerEl) return;
  let target = cardWinEl;
  if (!target || getComputedStyle(target).display === "none") {
    target = document.querySelector("#scanState .fr-card") || null;
  }
  if (!target) return;
  const l = target.offsetLeft;
  const t = target.offsetTop;
  const w = target.offsetWidth;
  const h = target.offsetHeight;
  if (
    glassRect &&
    glassRect.l === l && glassRect.t === t &&
    glassRect.w === w && glassRect.h === h
  ) {
    return;
  }
  glassRect = { l, t, w, h };
  const bleed = 24;
  glassLayerEl.style.left = l - bleed + "px";
  glassLayerEl.style.top = t - bleed + "px";
  glassLayerEl.style.width = w + bleed * 2 + "px";
  glassLayerEl.style.height = h + bleed * 2 + "px";
  if (glassBackdrop) {
    glassLayerEl.style.backgroundPosition = `-${l - bleed}px -${t - bleed}px`;
    glassLayerEl.style.backgroundSize = `${glassBackdrop.w}px ${glassBackdrop.h}px`;
  }
}

// U1: no standing animation-frame loop. A ResizeObserver fires only when the
// card (or the scan card while the palette is hidden) actually changes size;
// a window resize listener covers position-only shifts. syncGlassRect still
// dedups identical rects, so repeated events cost nothing.
function startGlassLoop() {
  if (glassOb || !glassLayerEl) return;
  const scanHost = document.querySelector("#scanState");
  const targets = [cardWinEl, scanHost, scanHost && scanHost.querySelector(".fr-card")].filter(Boolean);
  glassOb = new ResizeObserver(() => syncGlassRect());
  targets.forEach((t) => glassOb.observe(t));
  glassResizeHandler = () => syncGlassRect();
  window.addEventListener("resize", glassResizeHandler);
  syncGlassRect();
}

function stopGlassLoop() {
  if (glassOb) {
    glassOb.disconnect();
    glassOb = null;
  }
  if (glassResizeHandler) {
    window.removeEventListener("resize", glassResizeHandler);
    glassResizeHandler = null;
  }
}

function applyBackdrop(g) {
  if (!g || !g.uri || !glassLayerEl) return;
  if (!/^data:image\/jpeg;base64,[A-Za-z0-9+/=]+$/.test(g.uri)) return;
  glassBackdrop = { uri: g.uri, w: g.w_css, h: g.h_css };
  glassLayerEl.style.backgroundImage = `url("${g.uri}")`;
  glassRect = null;
  syncGlassRect();
}

function clearBackdrop() {
  glassBackdrop = null;
  if (glassLayerEl) glassLayerEl.style.backgroundImage = "";
}

function refreshBackdrop() {
  if (!invoke || !glassLayerEl) return;
  privileged("grab_backdrop").then(applyBackdrop).catch(() => {});
}
refreshBackdrop();
// Rust pushes each fresh capture before the window shows, so the JPEG
// decode overlaps the hidden period — no stale background ever flashes in.
listen("backdrop", (e) => applyBackdrop(e.payload));

// Rust emits this BEFORE hiding (webview still visible → JS runs at full
// speed), so the query/selection reset is done before the window ever
// repaints hidden. The next show is a fresh launcher with zero visible
// flash. Covers every hide path: hotkey, Esc, losing focus, close.
// scrollTop is explicitly reset too — replaceChildren alone leaves a
// residual scroll when the new list is about as tall as the old one,
// which pushes the first group label ("Applications") out of view.
listen("spotlight-hide", async () => {
  // The next show must be a fresh launcher: close Settings too, or the
  // panel would still be open on the next summon.
  if (document.body.classList.contains("settings-open")) {
    document.body.classList.remove("settings-open");
    if (settingsBtn) settingsBtn.setAttribute("aria-pressed", "false");
  }
  input.value = "";
  selected = 0;
  searchSeq += 1; // cancel any in-flight search page
  fileTotal = 0;
  resultsEl.scrollTop = 0;
  // Hidden-state cleanup (RAM): the empty-query browse can be a 500-row
  // list with ~500 decoded icons — re-running the search on hide kept every
  // one of those rows alive invisibly, plus the decoded preview bitmap and
  // the blurred glass texture. Drop all three now instead. The next show
  // rebuilds everything from scratch: window focus → input focus →
  // runSearchSafe() (items is empty), refreshBackdrop() re-grabs the
  // desktop, and re-previewing re-fetches from the capped thumb cache.
  items = [];
  rowEls = [];
  resultsEl.replaceChildren();
  if (pvImgEl) pvImgEl.removeAttribute("src");
  clearBackdrop();
  resultsEl.scrollTop = 0;
  // Reset the card height so the next summon opens directly at the height of
  // the initial apps list — without gliding up from the previous search's
  // (possibly much smaller) height on every reopen.
  cardWinEl.style.height = "auto";
  cardHeightShown = null;
});

// Theme: dark / light / system (system follows the OS live).
function applyThemeChoice() {
  const t =
    userTheme === "system"
      ? themeQuery.matches ? "light" : "dark"
      : userTheme;
  document.documentElement.setAttribute("data-theme", t);
  for (const btn of setThemeBtns) btn.classList.toggle("active", btn.dataset.themeChoice === userTheme);
  // Theme-scoped defaults: while the user has not moved a slider, switching
  // theme re-applies its default (14/56 light, 7/45 dark). A manual value
  // persists unchanged.
  applyAlpha(alphaTouched ? alphaValue : alphaThemeDefault(), false);
  applyBlur(blurTouched ? blurValue : blurThemeDefault(), false);
}
for (const btn of setThemeBtns) {
  btn.addEventListener("click", () => {
    userTheme = btn.dataset.themeChoice;
    localStorage.setItem("fs-theme", userTheme);
    applyThemeChoice();
  });
}
applyThemeChoice();

/* ── Accent: optionally adopt the Windows accent color ─────────────────── */
const ACCENT_DEFAULTS = { hex: "#007aff", rgb: "0, 122, 255" };
let accentMatch = localStorage.getItem("fs-accent") === "1";

function applyAccent() {
  const root = document.documentElement;
  if (setAccentSwitch) setAccentSwitch.setAttribute("aria-checked", String(accentMatch));
  if (!accentMatch) {
    root.style.setProperty("--accent-blue", ACCENT_DEFAULTS.hex);
    root.style.setProperty("--accent-blue-rgb", ACCENT_DEFAULTS.rgb);
    return;
  }
  invoke("get_accent_color")
    .then((hex) => {
      if (!hex) return; // registry said no accent — keep the default
      const r = parseInt(hex.slice(1, 3), 16);
      const g = parseInt(hex.slice(3, 5), 16);
      const b = parseInt(hex.slice(5, 7), 16);
      if (Number.isNaN(r) || Number.isNaN(g) || Number.isNaN(b)) return;
      root.style.setProperty("--accent-blue", hex);
      root.style.setProperty("--accent-blue-rgb", `${r}, ${g}, ${b}`);
    })
    .catch(() => {});
}
if (setAccentSwitch) {
  setAccentSwitch.addEventListener("click", () => {
    accentMatch = !accentMatch;
    localStorage.setItem("fs-accent", accentMatch ? "1" : "0");
    applyAccent();
  });
}
applyAccent();

/* ── Start with Windows (shell:startup shortcut, no admin needed) ─────── */
if (setAutostartSwitch) {
  invoke("autostart_enabled")
    .then((on) => setAutostartSwitch.setAttribute("aria-checked", String(!!on)))
    .catch(() => {});
  setAutostartSwitch.addEventListener("click", () => {
    const want = setAutostartSwitch.getAttribute("aria-checked") !== "true";
    invoke("set_autostart", { enabled: want })
      .then(() => setAutostartSwitch.setAttribute("aria-checked", String(want)))
      .catch((error) => {
        setAutostartSwitch.setAttribute("aria-checked", String(!want));
        showActionError("Start with Windows", error);
      });
  });
}

/* ── Summon hotkey (Ctrl+Space / Alt+Space, persisted by the backend) ───
   Segmented control, same language as the Theme row. */
const setHotkeyBtns = document.querySelectorAll("#setHotkey button");
if (setHotkeyBtns.length) {
  invoke("get_hotkey")
    .then((h) => {
      for (const btn of setHotkeyBtns)
        btn.classList.toggle("active", btn.dataset.hotkeyChoice === h);
    })
    .catch(() => {});
  for (const btn of setHotkeyBtns) {
    btn.addEventListener("click", () => {
      const want = btn.dataset.hotkeyChoice;
      invoke("set_hotkey", { name: want })
        .then(() => {
          for (const b of setHotkeyBtns) b.classList.toggle("active", b === btn);
        })
        .catch((error) => showActionError("Hotkey", error));
    });
  }
}

/* ── Corner radius (CSS var only — the sheet is transparent, so the card's
   border-radius IS the window's visible corner shape) ─────────────────── */

// Drives the custom slider fill: --fill is the accent portion of the
// track's gradient, computed from the input's own min/max so the radius
// slider (0–32) fills proportionally like the two 0–100 ones.
function updateSliderFill(el) {
  const min = parseFloat(el.min) || 0;
  const max = parseFloat(el.max) || 100;
  const pct = ((parseFloat(el.value) - min) / (max - min)) * 100;
  el.style.setProperty("--fill", pct + "%");
}

let cornerRadius = Math.min(32, Math.max(0, Number(localStorage.getItem("fs-radius")) || 16));function applyRadius() {
  document.documentElement.style.setProperty("--radius-window", `${cornerRadius}px`);
  if (setRadius) {
    setRadius.value = String(cornerRadius);
    updateSliderFill(setRadius);
  }
  if (setRadiusVal) setRadiusVal.textContent = `${cornerRadius}px`;
}
if (setRadius) {
  setRadius.addEventListener("input", () => {
    cornerRadius = Number(setRadius.value);
    localStorage.setItem("fs-radius", String(cornerRadius));
    applyRadius();
  });
}
applyRadius();

/* ── Compact mode: until something is typed, show ONLY the search bar ─── */
let compactMode = localStorage.getItem("fs-compact") === "1";
function applyCompact() {
  if (setCompactSwitch) setCompactSwitch.setAttribute("aria-checked", String(compactMode));
  renderNow(); // re-evaluate compact-empty (query may have changed)
}
if (setCompactSwitch) {
  setCompactSwitch.addEventListener("click", () => {
    compactMode = !compactMode;
    localStorage.setItem("fs-compact", compactMode ? "1" : "0");
    applyCompact();
  });
}
applyCompact();

/* ── Math (Spotlight-style calculator) ─────────────────────────────────── */
// Pure arithmetic queries (digits + operators, no letters) evaluate locally:
// typing "2*8" shows "2*8 = 16" as the top result. ^ is exponentiation.
// Path-queries resolve against the live filesystem instead of the index:
// drive/UNC paths, %ENV% vars, ~ and bare aliases all work, so pruned trees
// (AppData, Program Files, node_modules, ...) stay reachable. Anything else
// is a normal index query. Bare aliases/drive letters also engage.
const PATH_QUERY_RE = /^(?:[a-zA-Z]:[\\/]|\\\\|~[\\/]|appdata[\\/]|localappdata[\\/]|temp[\\/]|userprofile[\\/]|programfilesx86[\\/]|programfiles[\\/]|program files[\\/]|program files \(x86\)[\\/]|windows[\\/]|system32[\\/]|%[^%\\/]+%[\\/])/i;
const PATH_BARE_RE = /^(?:[a-zA-Z]:|~|%[^%\\/]+%)$/i;
function isPathQuery(s) {
  // Explorer copies paths wrapped in quotes — strip them for detection; the
  // backend strips them again before resolving.
  const t = s.replace(/^"+|"+$/g, "");
  // Any backslash is path intent: filenames can't contain one, so nothing a
  // real index query means gets hijacked. Covers "ansh\Downloads\folder"
  // (profile-relative) and every quoted paste form too.
  return PATH_QUERY_RE.test(t) || PATH_BARE_RE.test(t) || t.includes("\\");
}

const MATH_RE = /^[0-9+\-*/().%\s^]+$/;
// Safe expression evaluator for the math-query feature. The input charset
// is locked down by MATH_RE (digits + operators + parens only), but CSP
// script-src 'self' (no 'unsafe-eval') blocks new Function(), so a tiny
// recursive-descent parser computes the value instead — no eval anywhere.
// Precedence: unary +/- < ^ (right-assoc) < * / % < + -, parens override.
function evalMath(s) {
  let i = 0;
  const peek = () => (i < s.length ? s[i] : "");
  const skip = () => { while (i < s.length && /\s/.test(s[i])) i++; };
  const num = () => {
    const start = i;
    while (i < s.length && /[0-9.]/.test(s[i])) i++;
    const tok = s.slice(start, i);
    if (!tok || (tok.match(/\./g) || []).length > 1 || tok.replace(".", "").length === 0) throw new Error("bad number");
    return parseFloat(tok);
  };
  const primary = () => {
    skip();
    const ch = peek();
    if (ch === "(") {
      i++;
      const v = expr();
      skip();
      if (peek() !== ")") throw new Error("unbalanced parens");
      i++;
      return v;
    }
    if (/[0-9.]/.test(ch)) return num();
    throw new Error("unexpected char");
  };
  const expr = () => {
    let v = term();
    for (;;) {
      skip();
      const ch = peek();
      if (ch === "+") { i++; v += term(); }
      else if (ch === "-") { i++; v -= term(); }
      else break;
    }
    return v;
  };
  const term = () => {
    let v = unary();
    for (;;) {
      skip();
      const ch = peek();
      if (ch === "*") { i++; v *= unary(); }
      else if (ch === "/") { i++; v /= unary(); }
      else if (ch === "%") { i++; v %= unary(); }
      else break;
    }
    return v;
  };
  const unary = () => {
    skip();
    const ch = peek();
    if (ch === "-") { i++; return -unary(); }
    if (ch === "+") { i++; return +unary(); }
    return power();
  };
  const power = () => {
    const base = primary();
    skip();
    if (peek() === "^") {
      i++;
      return Math.pow(base, unary());
    }
    return base;
  };
  skip();
  const v = expr();
  skip();
  if (i < s.length) throw new Error("trailing chars");
  return v;
}

function tryMath(query) {
  const s = query.trim();
  if (!s || s.length > 80 || !MATH_RE.test(s)) return null;
  if (!/\d/.test(s) || !/[+\-*/%^]/.test(s)) return null;
  let value;
  try {
    value = evalMath(s);
  } catch {
    return null;
  }
  if (typeof value !== "number" || !Number.isFinite(value)) return null;
  const text =
    Number.isInteger(value)
      ? String(value)
      : String(Math.round(value * 1e9) / 1e9).replace(/\.?0+$/, "");
  return { kind: "math", name: `${s} = ${text}`, path: `math:${s}`, expr: s, value: text === "-0" ? "0" : text };
}

// Turn an expression like "100*25" into display tokens: numbers as chips,
// operators as red symbols (× ÷), exactly like a calculator's readout.
function mathTokens(expr) {
  const parts = expr.split(/([+\-*/%^])/g).filter((t) => t.trim() !== "");
  const frag = document.createDocumentFragment();
  for (const part of parts) {
    const t = part.trim();
    const span = document.createElement("span");
    if (/^[+\-*/%^]$/.test(t)) {
      span.className = "tag-op";
      span.textContent = t === "*" ? "×" : t === "/" ? "÷" : t;
    } else {
      span.className = "tag-num";
      span.textContent = t;
    }
    frag.appendChild(span);
  }
  return frag;
}

async function copyText(text) {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch {
    const ta = document.createElement("textarea");
    ta.value = text;
    ta.style.cssText = "position:fixed;opacity:0";
    document.body.appendChild(ta);
    ta.select();
    let ok = false;
    try {
      ok = document.execCommand("copy");
    } catch {}
    ta.remove();
    return ok;
  }
}

function acceptMathResult(item) {
  // Enter/click on a math row replaces the query with the result
  // (Spotlight convention) and re-searches from there.
  input.value = item.value;
  selected = 0;
  fileTotal = 0;
  lastNavKeyAt = Date.now();
  paintFromPools(item.value);
  scheduleSearch();
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

// Detail rows whose value is "—" (nothing to show) collapse instead of
// rendering a dash. Called after every metadata fill.
function refreshMetaVisibility() {
  for (const row of document.querySelectorAll(".meta-row")) {
    const value = row.querySelector(".meta-value");
    const text = value ? value.textContent.trim() : "";
    row.classList.toggle("pv-empty", text === "" || text === "—");
  }
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
    refreshMetaVisibility();
  }, 60);
}

function renderPreview() {
  // Called on every selection change (and on search re-render). All DOM
  // work is confined to the pane; the stat itself is debounced + skipped
  // entirely while the pane is hidden.
  syncCardHeight();
  const row = rowEls[selected];
  const item = row && row._item;
  const pane = previewPaneEl;
  if (!pane) return;
  if (
    previewHidden ||
    document.body.classList.contains("settings-open") ||
    !item ||
    item.kind === "math"
  ) {
    pane.classList.add("empty-preview");
    return;
  }
  pane.classList.remove("empty-preview");

  const title = document.getElementById("pvTitle");
  const type = document.getElementById("pvType");
  const path = document.getElementById("pvPath");
  const iconEl = document.getElementById("pvIcon");

  title.textContent = item.name || item.path;
  title.title = item.name || item.path || "";
  type.textContent =
    item.kind === "app"
      ? "Application"
      : item.kind === "more"
        ? "More results"
        : item.kind === "web"
          ? "Web search"
          : item.is_dir
            ? "Folder"
            : "File";
  pane.classList.toggle("pv-app", item.kind === "app");
  pane.classList.toggle("pv-plain", !!item.is_dir);
  pane.classList.toggle("pv-web", item.kind === "web");
  const snippetEl = document.getElementById("pvSnippet");
  if (snippetEl) snippetEl.textContent = item.kind === "web" ? item.snippet || "" : "";
  path.textContent = truncatePath(item.path || "");
  path.title = item.path || "";

  // Action buttons per item type: apps get admin + location (+ uninstall
  // only when the registry knows an uninstaller), files/folders get a single
  // "Open file location" (the path box is gone — the button replaces it).
  const isFile = item.kind === "file" || item.kind === "dir";
  // Non-app selections MUST re-enable the buttons: previewMetaApp (apps
  // only) can leave them disabled (UWP / missing target) and nothing else
  // ever resets them, which made "Open file location" dead for every file
  // selected after a UWP app.
  if (openLocBtn) openLocBtn.disabled = false;
  if (adminBtn) adminBtn.disabled = false;
  if (adminBtn) adminBtn.style.display = item.kind === "app" ? "" : "none";
  if (openLocBtn) openLocBtn.style.display = item.kind === "app" || isFile ? "" : "none";
  if (uninstallBtn) uninstallBtn.style.display = "none";
  currentSelection = { item, info: null };

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

  // Real image files show the picture itself (cached per path) instead of
  // the generic icon. The icon row stays underneath, hidden, so a failed
  // load or a mid-flight selection change falls back cleanly.
  const IMAGE_EXT_RE = /\.(png|jpe?g|gif|webp|bmp|svg|ico)$/i;
  if (item.kind === "file" && item.path && IMAGE_EXT_RE.test(item.path) && pvImgEl) {
    iconEl.style.display = "none";
    pvImgEl.hidden = false;
    const cached = pvImgCache.get(item.path);
    if (cached) {
      pvImgEl.src = cached;
      pvImgEl.alt = item.name || "";
    } else {
      // Cold preview: keep the previous picture on screen while the new
      // thumbnail loads (scaled JPEG decode ≈ 15-40 ms now) — blanking the
      // pane first turned that into a visible "Loading…" flash per row.
      const selPath = item.path;
      privileged("image_data", { path: item.path })
        .then((uri) => {
          if (!currentSelection || currentSelection.item.path !== selPath) return;
          if (!uri) {
            // Undecodable/oversized: fall back to the row's icon like
            // non-image files instead of leaving a dead pane.
            pvImgEl.hidden = true;
            pvImgEl.removeAttribute("src");
            iconEl.style.display = "";
            return;
          }
          if (!uri.startsWith("data:")) return;
          pvImgCache.set(selPath, uri);
          // Bounded cache: a handful of recent previews is plenty; evicting
          // the oldest keeps the map from growing without limit over a long
          // session (its decoded images are dropped with it).
          if (pvImgCache.size > 6) pvImgCache.delete(pvImgCache.keys().next().value);
          if (currentSelection.item.path === selPath) {
            pvImgEl.src = uri;
            pvImgEl.alt = currentSelection.item.name || "";
          }
        })
        .catch(() => {});
    }
  } else if (pvImgEl) {
    pvImgEl.hidden = true;
    pvImgEl.removeAttribute("src");
    iconEl.style.display = "";
  }

  const hasStat = item.kind === "file" || item.kind === "dir" || item.kind === "app";
  if (item.kind === "app") {
    previewMetaApp(item);
  } else if (hasStat && item.path) {
    previewMeta(item.path);
    pvPublisher.textContent = "—";
    pvVersion.textContent = "—";
  } else {
    pvSize.textContent = "—";
    pvModified.textContent = "—";
  }
  refreshMetaVisibility();
}





// The previewed item: { item, info } — info (exe target / publisher /
// uninstall entry) is filled in by app_info for apps, null otherwise.
let currentSelection = null;

async function previewMetaApp(item) {
  clearTimeout(previewTimer);
  previewTimer = setTimeout(async () => {
    if (previewHidden || !item || !invoke) return;
    // The selection may have moved on while app_info was in flight.
    const rowNow = rowEls[selected];
    if (!rowNow || rowNow._item !== item) return;
    pvSize.textContent = "…";
    pvModified.textContent = "…";
    try {
      const info = await invoke("app_info", { name: item.name, path: item.path });
      currentSelection = { item, info };
      const pvPath = document.getElementById("pvPath");
      const target = info.target || "";
      if (target && target !== item.path) {
        pvPath.textContent = truncatePath(target);
        pvPath.title = target;
      }
      pvSize.textContent = info.size ? fmtSize(info.size) : "—";
      pvModified.textContent = info.modified_secs ? fmtRelative(info.modified_secs) : "—";
      pvPublisher.textContent = info.publisher || "—";
      pvVersion.textContent = info.version || "—";
      // UWP/Store apps are still actionable: "open file location" shows the
      // Apps folder (WindowsApps is ACL-protected), "run as administrator"
      // resolves the package's real exe and elevates it.
      if (openLocBtn) openLocBtn.disabled = !target && !info.is_uwp;
      if (adminBtn) adminBtn.disabled = false;
      // Uninstall exists only for installed/downloaded apps (registry entry);
      // system apps like Command Prompt have none — no button at all.
      if (uninstallBtn) uninstallBtn.style.display = info.uninstall_string ? "" : "none";
    } catch {
      pvSize.textContent = "—";
      pvModified.textContent = "—";
      pvPublisher.textContent = "—";
      pvVersion.textContent = "—";
      if (openLocBtn) openLocBtn.disabled = false;
      if (adminBtn) adminBtn.disabled = false;
    }
    refreshMetaVisibility();
  }, 60);
}

// Preview actions: run elevated, reveal in Explorer, uninstall.
async function runAppAction(cmd, payload) {
  const cur = currentSelection;
  if (!cur) return;
  try {
    if (cmd === "launch_admin" || cmd === "uninstall_app") {
      await privileged(cmd, payload);
    } else {
      await invoke(cmd, payload);
    }
    await invoke("hide_window");
  } catch (error) {
    showActionError(cur.item.name, error);
  }
}

const adminBtn = document.getElementById("pvAdmin");
const openLocBtn = document.getElementById("pvOpenLoc");
const uninstallBtn = document.getElementById("pvUninstall");
if (adminBtn) {
  adminBtn.addEventListener("click", () => {
    const cur = currentSelection;
    if (cur) runAppAction("launch_admin", { path: cur.item.path });
  });
}
if (openLocBtn) {
  openLocBtn.addEventListener("click", () => {
    const cur = currentSelection;
    if (!cur) return;
    const target =
      cur.info && cur.info.target && !cur.info.is_uwp ? cur.info.target : cur.item.path;
    runAppAction("open_parent", { path: target });
  });
}
if (uninstallBtn) {
  uninstallBtn.addEventListener("click", () => {
    const cur = currentSelection;
    if (cur) runAppAction("uninstall_app", { name: cur.item.name, path: cur.item.path });
  });
}
const pvOpenWebBtn = document.getElementById("pvOpenWeb");
if (pvOpenWebBtn) {
  pvOpenWebBtn.addEventListener("click", async () => {
    const cur = currentSelection;
    if (!cur || cur.item.kind !== "web") return;
    try {
      await invoke("open_web_search", { query: cur.item.path });
      await invoke("hide_window");
    } catch (error) {
      showActionError(cur.item.name || cur.item.path, error);
    }
  });
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
  if (item.kind === "math") {
    if (item.value != null) acceptMathResult(item);
    return;
  }
  if (item.kind === "more") {
    await loadMoreFiles();
    return;
  }
  if (item.kind === "web") {
    try {
      await invoke("open_web_search", { query: item.path });
      await invoke("hide_window");
    } catch (error) {
      showActionError(item.name || item.path, error);
    }
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
    if (cmd === "launch_admin") {
      await privileged(cmd, { path: item.path });
    } else {
      await invoke(cmd, { path: item.path });
    }
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
  // Typing while Settings is open closes the panel so the query's results
  // take over the results column immediately (same close path as the gear).
  if (document.body.classList.contains("settings-open")) {
    document.body.classList.remove("settings-open");
    if (settingsBtn) settingsBtn.setAttribute("aria-pressed", "false");
    renderPreview();
    syncCardHeight();
  }
  paintFromPools(input.value); // instant, zero IPC
  scheduleSearch(); // authoritative backfill
  syncScanNotice(); // hide the first-run notice once the user types
});

// The launcher never blocks on the index: while the cache loads or a
// rebuild runs it stays fully usable with what is already discoverable
// (the app pool + whatever files are indexed so far — the backend serves
// the partially-built store) and the status strip reports progress. The
// scan page remains only for genuinely fatal states: a zero-file index
// (missing admin rights / no NTFS drives) or an unreachable backend.
async function refreshStatus() {
  if (!invoke) return;
  try {
    const status = await invoke("get_index_status");
    firstScanActive = !!(status && status.first_scan);
    // The app pool lives on the backend and only changes on install or
    // uninstall; the rev counter tells us when to re-fetch it (cheap).
    if (status && typeof status.apps_rev === "number" && status.apps_rev !== appPoolRev) {
      appPoolRev = status.apps_rev;
      loadApps(true);
    }
    const fatal = !!(status && /No files were indexed|No NTFS drives/.test(status.message || ""));
    if (!status || !status.ready || fatal) {
      if (fatal) {
        setState("scan");
        scanTitle.textContent = "Index unavailable";
        scanSub.textContent =
          "Finder could not read any files. Make sure it is running as Administrator, then try again.";
        scanStatusText.textContent = status.message || "Index unavailable";
        return;
      }
      setState("ready");
      statusEl.style.display = "";
      progressFill.style.display = "block";
      if (status && typeof status.progress === "number" && status.progress >= 0) {
        progressFill.classList.remove("indeterminate");
        progressFill.style.width = Math.round(status.progress * 100) + "%";
      } else {
        // No known total (direct MFT read, counting phase): the bar sweeps
        // indefinitely while the record counter keeps climbing.
        progressFill.classList.add("indeterminate");
      }
      statusText.textContent = (status && status.message) || "Indexing…";
      syncScanNotice();
      return;
    }
    // Ready: ONLY the palette.
    indexReady = true;
    setState("ready");
    statusEl.style.display = "none";
    syncScanNotice();
  } catch (error) {
    setState("scan");
    scanStatusText.textContent = `Backend unavailable: ${error}`;
  }
}

let lastNavKeyAt = 0; // hover never yanks the selection right after a keystroke

// The UI is a product surface, not a debug surface: no right-click context
// menu (WebView2's includes "Inspect"), no devtools shortcuts. Devtools are
// compiled out of release builds anyway; this kills the menu itself.
document.addEventListener("contextmenu", (event) => event.preventDefault());
window.addEventListener("keydown", (event) => {
  if (
    event.key === "F12" ||
    (event.ctrlKey && event.shiftKey && /^[iIcCjJ]$/.test(event.key))
  ) {
    event.preventDefault();
  }
});

// Clicking anywhere outside the card (the transparent window margins) acts
// like Esc: reset the query and hide. Clicks on the desktop itself already
// dismiss via the window's focus-loss handler.
document.addEventListener("mousedown", (event) => {
  if (event.button !== 0) return;
  if (event.target.closest(".launcher-window") || event.target.closest(".fr-card")) return;
  if (input.value.trim()) {
    input.value = "";
    selected = 0;
    fileTotal = 0;
    lastNavKeyAt = Date.now();
    paintFromPools("");
    syncScanNotice();
  }
  if (invoke) invoke("hide_window");
});

// Webview-level safety net: some outside clicks never surface as a page
// mousedown (nor as a Rust Focused(false) — e.g. clicking another
// always-on-top window), but they always blur the webview. The backdrop is
// deliberately NOT cleared here: while hidden it is invisible anyway, and
// keeping it lets a reused Rust-side grab skip its emit entirely — rapid
// open/close cycles otherwise queue a multi-MB image event per show into
// the throttled hidden renderer (the memory accumulation source).
window.addEventListener("blur", () => {
  // The first scan keeps the window up even without focus (a freshly
  // elevated process is often never granted foreground); a blur there is
  // not a dismiss, and the glass loop must keep tracking the card.
  if (firstScanActive && !indexReady) return;
  stopGlassLoop();
  if (invoke) invoke("hide_window");
});

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
      syncScanNotice();
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
    if (item && item.kind === "math" && item.value != null) {
      event.preventDefault();
      if (await copyText(item.value)) {
        statusEl.style.display = "";
        progressFill.style.display = "none";
        statusText.textContent = "Result copied to clipboard";
        setTimeout(() => {
          statusEl.style.display = "none";
        }, 1500);
      }
      return;
    }
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
    const mathItem = rowEls[selected] && rowEls[selected]._item;
    if (mathItem && mathItem.kind === "math" && mathItem.value != null) {
      acceptMathResult(mathItem);
      return;
    }
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
    updateSelection(false);
  }
});

// Traditional behavior: a single click selects the row (highlight moves,
// preview follows); a double click opens it. Keyboard (Enter etc.) and the
// preview action buttons still open on their own.
resultsEl.addEventListener("click", (event) => {
  const row = event.target.closest(".result");
  if (!row) return;
  selected = Number(row.dataset.index);
  updateSelection(false);
});

resultsEl.addEventListener("dblclick", (event) => {
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
  if (!bootShown) {
    bootShown = true;
    endBootSettleWhenReady();
    // Hard backstop: even if the app pool never arrives (failed IPC),
    // glides must come back instead of staying off forever.
    setTimeout(() => { bootSettle = false; }, 3000);
  }
  // Selecting the whole query on every show wipes it on the next keystroke;
  // do it once per session only.
  if (!firstInitDone) {
    focusInitDone = true;
    input.select();
  }
  refreshStatus();
  refreshBackdrop();
  startGlassLoop();
});

// The glass loop normally starts on focus — but a freshly elevated first
// launch is often never granted foreground, so kick it at boot too (it
// self-stops on blur, except while the first scan holds the window up).
startGlassLoop();

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

// ── Updater (Tauri built-in) — auto-check for updates once per week ── ──
// The backend push side is publish-update.ps1 + latest.json (signed NSIS
// installer). This client listens for update events, checks the feed at most
// once per 7 days (timestamp kept in localStorage), and lets the user install.
// (`updater` itself is declared at the top with the vendored IPC core.)
const updateBanner = document.querySelector("#updateBanner");
const updateVersionEl = document.querySelector("#updateVersion");
const updateBtnEl = document.querySelector("#updateBtn");
const updateDismissEl = document.querySelector("#updateDismiss");
const updateProgressEl = document.querySelector("#updateProgress");
const UPDATE_WEEK_MS = 7 * 24 * 60 * 60 * 1000;
const UPDATE_KEY = "finder_last_update_check";
let updateInProgress = false;

function hideUpdateBanner() {
  if (updateBanner) updateBanner.classList.remove("show");
}
function showUpdateBanner() {
  if (updateBanner) updateBanner.classList.add("show");
}

async function installUpdateNow() {
  if (!updater || updateInProgress) return;
  updateInProgress = true;
  if (updateBtnEl) {
    updateBtnEl.disabled = true;
    updateBtnEl.textContent = "Downloading…";
  }
  if (updateProgressEl) updateProgressEl.hidden = false;
  try {
    await updater.installUpdate();
    hideUpdateBanner();
  } catch (error) {
    console.error("update install failed:", error);
    if (updateBtnEl && updateBtnEl.textContent !== "Installing…") {
      updateBtnEl.textContent = "Retry update";
      updateBtnEl.disabled = false;
    }
    if (updateProgressEl) updateProgressEl.hidden = true;
    updateInProgress = false;
  }
}

const checkUpdateBtn = document.querySelector("#checkUpdateBtn");
const updateCheckStatus = document.querySelector("#updateCheckStatus");

async function manualUpdateCheck() {
  if (!updater) return; // dev build without global API: no-op
  if (checkUpdateBtn) checkUpdateBtn.disabled = true;
  if (updateCheckStatus) updateCheckStatus.textContent = "Checking…";
  try {
    const result = await updater.checkUpdate();
    if (updateCheckStatus) {
      const newer = result && result.shouldUpdate;
      const ver = newer && result.manifest ? `v${result.manifest.version}` : "Up to date";
      updateCheckStatus.textContent = newer ? `${ver} — Update now` : ver;
      setTimeout(() => { updateCheckStatus.textContent = ""; }, 5000);
    }
  } catch (error) {
    console.error("manual update check failed:", error);
    if (updateCheckStatus) {
      updateCheckStatus.textContent = "Check failed";
      setTimeout(() => { updateCheckStatus.textContent = ""; }, 5000);
    }
  } finally {
    if (checkUpdateBtn) checkUpdateBtn.disabled = false;
  }
}

function setupUpdater() {
  if (!updater || !updateBanner) return; // dev build without global API: no-op

  // Permanent action handlers first, so events that arrive during the first
  // check are already wired up.
  if (updateBtnEl) updateBtnEl.addEventListener("click", installUpdateNow);
  if (updateDismissEl) updateDismissEl.addEventListener("click", hideUpdateBanner);
  if (checkUpdateBtn) checkUpdateBtn.addEventListener("click", manualUpdateCheck);

  updater
    .onUpdaterEvent((event) => {
      const status = event && event.status;
      if (status === "UPDATE_AVAILABLE") {
        const v = event.body && event.body.version ? event.body.version : "";
        if (updateVersionEl) updateVersionEl.textContent = v ? `v${v}` : "";
        if (updateProgressEl) updateProgressEl.hidden = true;
        showUpdateBanner();
      } else if (status === "UPTODATE" || status === "UPDATE_NOT_AVAILABLE") {
        // Quiet by design — no nag when there's nothing new.
      } else if (status === "ERROR") {
        console.error("updater error:", event.error || event);
        // Only hide when nothing was in flight; keep the banner if we were
        // mid-install so the user can retry.
        if (!updateInProgress) hideUpdateBanner();
      } else if (status === "DOWNLOAD_PROGRESS") {
        if (updateProgressEl && event.data) {
          const total = event.data.contentLength;
          const got = event.data.chunkLength;
          const pct = total ? Math.round((got / total) * 100) : 0;
          updateProgressEl.textContent = `Downloading… ${pct}%`;
        }
      } else if (status === "DOWNLOADED" || status === "INSTALLING") {
        if (updateBtnEl) updateBtnEl.textContent = "Installing…";
        if (updateProgressEl) updateProgressEl.textContent = "Installing…";
      } else if (status === "INSTALLED" || status === "DONE") {
        hideUpdateBanner();
        updateInProgress = false;
      }
    })
    .catch((error) => console.error("updater listener failed:", error));

  // Once per week: touch the network only when a full week has passed since
  // the last check (empty/missing mark counts as "overdue", so the very first
  // run after this ships checks too). Persisted so restarts don't re-check.
  let last = 0;
  try {
    last = Number(localStorage.getItem(UPDATE_KEY)) || 0;
  } catch (error) {}
  if (Date.now() - last < UPDATE_WEEK_MS) return;
  try {
    localStorage.setItem(UPDATE_KEY, String(Date.now()));
  } catch (error) {}
  updater.checkUpdate().catch((error) => console.error("update check failed:", error));
}

setupUpdater();

// Status polling is adaptive: while the index is being built the bar moves
// in small steps every 250 ms (with a width transition that tapers them into
// a continuous glide); otherwise a slow 1.5 s heartbeat is plenty.
function scheduleStatusPoll() {
  const fast = firstScanActive && !indexReady;
  setTimeout(async () => {
    await refreshStatus();
    scheduleStatusPoll();
  }, fast ? 250 : 1500);
}
scheduleStatusPoll();
loadApps();
input.focus();

// First-show handshake: the window is held hidden by Rust until the page has
// painted and (best-effort) applied the frosted-glass backdrop, so the cold
// start never flashes the card over raw desktop. The short settle delay lets
// refreshBackdrop()'s grab complete before we raise the curtain.
setTimeout(() => {
  if (invoke) invoke("frontend_loaded").catch(() => {});
}, 400);