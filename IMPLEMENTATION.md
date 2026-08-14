# Finder — Implementation Details

A comprehensive record of how Finder works under the hood — every subsystem with its real
data structures, algorithms, memory profile, and security posture. Written from the code
(source at `src/`, UI at `tauri-ui/`), shipping state as of commit `ca100d5`.

---

## 1. Overview

Finder is a Windows-native launcher in the style of Spotlight/Raycast.

- **Backend**: Rust + Tauri v1 (WebView2 for the UI), `windows` 0.60 crate for Win32, raw
  NTFS MFT scanning for whole-drive indexing (~3M records in ~28 s), Win32 global hotkey
  (`RegisterHotKey`), frameless transparent always-on-top window.
- **Frontend**: vanilla JS/CSS/HTML in `tauri-ui/`, no framework, Tauri global API
  (`withGlobalTauri`).
- **Elevation**: started **as administrator** by design (full-drive reads, launching apps
  elevated). Security implications in §9.

### File map

| File | Lines | Role |
|---|---|---|
| `src/gui/main.rs` | 3448 | window, hotkey loop, tray, all `#[tauri::command]`s, backdrop pipeline, app icon cache |
| `src/index/store.rs` | 897 | in-memory index: arenas, chunks, cache format v2, live event application |
| `src/index/search.rs` | 785 | scoring, paging, extension search, fuzzy fallback, app pool |
| `src/index/apps.rs` | 356 | WinRT installed-app enumeration + Start Menu .lnk scanning |
| `src/mft/reader.rs` | 520 | MFT ioctl scan → `CompactRecord` stream, junk pruning, 12 workers |
| `src/mft/watcher.rs` | 513 | USN change journal → live `IndexEvent` stream |
| `src/mft/types.rs` | 87 | `FileRecord`, `IndexEvent`, `JournalCheckpoint` |
| `src/utils/drives.rs` | 61 | drive enumeration |
| `src/main.rs` | 592 | binary bootstrap, self-relocation to `%LOCALAPPDATA%`, single-instance |
| `tauri-ui/main.js` | 72.5 KB | all UI logic |
| `tauri-ui/index.html` | 12.4 KB | markup |
| `tauri-ui/styles.css` | 29.7 KB | theming (CSS vars), layout, states |

---

## 2. Process lifecycle

### 2.1 Boot sequence

1. **Self-relocation** (`src/main.rs`): if the exe wasn't launched from `%LOCALAPPDATA%`
   (or the self-update path), it copies itself there and relaunches — the updater can then
   overwrite the original.
2. **Single-instance**: a named mutex; a second instance pokes the first (shows the launcher)
   and exits.
3. **Logging**: `%LOCALAPPDATA%\Finder\log.txt`, monotonic `+seconds.millis pid=…` lines.
4. **Index load** (`store.rs::from_cache`): cache file read, arenas travel byte-for-byte;
   only `ref_lookup` + `ext_index` are rebuilt (parallel). Cache format is versioned with a
   magic; foreign formats are rejected (a stale/older version triggers a fresh scan).
5. **App pool** (`apps.rs`): WinRT `GetInstalledApps`-style enumeration **plus** Start Menu
   `.lnk` scanning; each app logs `app-pool | name | aumid/target | score`.
6. **Hotkey**: reads `HKCU\Software\Finder\Hotkey` (default `ctrl+space`), spawns the
   dedicated message loop thread: `RegisterHotKey` + `GetMessage` + tray icon +
   `CreateWindowExW` hidden hotkey window (log `hotkey registered: …`).
7. **Watchers** (`watcher.rs`): opens the USN journal per drive (log `index ready —
   watchers starting`).
8. **Frontend handshake**: JS pulls the cached backdrop (`grab_backdrop`), subscribes to
   pools and state, flips from the scan card to the launcher card on `state=ready`.

### 2.2 Show / hide

```
hotkey/tray ─▶ show_spotlight() [main.rs]:
  1. strip_system_menu()    — WS_SYSMENU removal + SWP_FRAMECHANGED (§4.3)
  2. position_spotlight()   — center X; y = 12% of monitor height (clamped)
  3. capture_backdrop()     — desktop grab behind the window (§4.2)
  4. emit "backdrop"        — ONLY when the grab is fresh (fingerprint gate)
  5. window.show() + set_focus()

hide paths (all → hide_spotlight()):
  - hotkey re-press · Esc on empty query (JS → invoke("hide_window"))
  - Rust Focused(false) / CloseRequested
  - JS window "blur" safety net (stopGlassLoop + hide_window)
hide_spotlight():
  1. emit "spotlight-hide" BEFORE window.hide() — the webview is still alive,
     so the JS reset runs at full speed (zero visible flash)
  2. window.hide()
```

**Reset-on-hide** (JS `spotlight-hide` listener): clears query/selection/footer, bumps the
`searchSeq` counter (cancels in-flight search pages), resets `#results` scrollTop before and
after repaint, closes the Settings panel. The backdrop image is deliberately **kept** —
that's what lets a reused Rust-side grab skip its emit entirely.

---

## 3. Window & compositing

### 3.1 Native window

`tauri.conf.json`: 1050×690 logical, fixed size, `decorations:false`, `transparent:true`,
`alwaysOnTop:true`, `skipTaskbar:true`, initially hidden. The webview body has transparent
margins (`padding: 85px 70px`) — room for the card shadow, and clicks in the margin hit the
click-outside-to-hide handler.

- **Corner radius**: the native sheet is never rounded; the visible rounding is the card's
  CSS `border-radius: var(--radius-window)` (Settings slider, 0–32px) mirrored in the glass
  layer's `clip-path: inset(24px round …)`.
- **Height**: JS `measureCardHeight()`: `height:auto` → read `scrollHeight` → clamp
  210…520 (52 floor in compact-empty) → write back with a 180 ms animation.

### 3.2 The frosted-glass backdrop pipeline

**Why**: a transparent WebView2 can't use CSS `backdrop-filter` (Chromium samples only the
page's own pixels — the real desktop is not composited under a transparent webview). So Rust
captures the desktop and CSS blurs *that image*.

**Every link** (`capture_backdrop`, `src/gui/main.rs`):

```
show ─▶ BitBlt (screen DC → memory bitmap, WINDOW-SIZED ONLY: ~1312×862 device px at
        150% DPI — never the whole screen; captured while hidden so it shows the desktop
        behind the launcher, not the launcher)
      ▶ GetDIBits → BGRA buffer → BGRA→RGBA swap
      ▶ imageops::thumbnail → half resolution (656×431)
      ▶ JpegEncoder::new_with_quality(Q=55)   [JPEG, not PNG: ~10× smaller]
      ▶ base64 → data:image/jpeg;base64,…  (~100–250 KB after downscale+q55)
      ▶ cached in global BACKDROP (one string, replaced in place)
      ▶ if fresh: window.emit("backdrop", grab)   [window still hidden]
JS:   listen("backdrop") → applyBackdrop() → .glass-layer backgroundImage = URI
CSS:  .glass-layer { position:fixed; z-index:-1; clip-path: inset(24px round …);
                     filter: blur(var(--blur-px,20px)) saturate(1.8) }
      syncGlassRect() (rAF loop) pins it to the card rect (+24px bleed for blur
      sampling, trimmed by the clip-path). w_css×h_css shipped with the grab keep the
      downscaled image stretched to full CSS size — the blur hides the upscaling.
```

**Cost gates** (why RAM stays flat under hotkey mashing):

1. **TTL fast-path** — grab reused if < 800 ms old AND same window rect (no capture, no
   emit; the webview already holds it).
2. **dHash fingerprint** — after the TTL, a 64×36 `StretchBlt` thumbnail is captured and
   reduced to a 2268-bit dHash ("pixel brighter than its left neighbour", luminance-based).
   Hamming distance ≤ 24 → desktop unchanged → reuse silently, refresh timestamp, zero IPC.
   dHash (not a raw checksum) tolerates cursor blinks/clock digits; a real desktop change
   flips hundreds of bits.
3. **Rect key invalidation** — `GetWindowRect` equality doubles as monitor/DPI/layout
   change detection (any of those move/resize the window).
4. **Backdrop survives hide** — the JS blur handler no longer clears it, so reuse needs no
   re-emit. Boot still pulls it via `grab_backdrop`.

Failure handling: `backdrop capture failed: …` → show proceeds with last known grab or none.

### 3.3 Anti system-menu strip

`strip_system_menu()`: `SetWindowLongPtrW(GWL_STYLE, style & ~WS_SYSMENU)` **plus**
`SetWindowPos(SWP_FRAMECHANGED)` — the framechanged flag is mandatory; without it the
non-client frame (and the Alt+Space menu) silently survives the style change. Re-applied on
every show because some DPI/fullscreen paths rebuild the frame. `window.hwnd()` returns
tauri's own windows-crate HWND — the raw pointer is unwrapped and re-wrapped in our 0.60
HWND type.

---

## 4. Settings system

| Setting | Storage | Backend | Mechanism |
|---|---|---|---|
| Theme dark/light/system | `localStorage fs-theme` | — | `data-theme` attr → CSS var swap; theme re-applies default alpha/blur unless sliders were touched |
| Match Windows accent | `localStorage fs-accent` | `get_accent_color` | WinRT UISettings via one-shot hidden PowerShell (correct for "auto from background"), DWM `HKCU…\DWM\AccentColor` (0xAABBGGRR) fallback → `--accent-blue` + `--accent-blue-rgb` |
| Transparency | `localStorage fs-alpha` | — | `--window-alpha` computed into every bg color (theme-scoped defaults) |
| Blur | `localStorage fs-blur` | — | `--blur-px` → glass filter |
| Corner radius | `localStorage fs-radius` | — | `--radius-window` (0–32) |
| Preview pane | `localStorage fs-preview-hidden` | — | body class `preview-off` |
| Instant math | `localStorage fs-math` | — | local evaluator (no IPC) |
| Compact mode | `localStorage fs-compact` | — | body class `compact-empty` (query empty) → 400px slim bar, normal placement |
| Summon hotkey | `HKCU\Software\Finder\Hotkey` | `get_hotkey`/`set_hotkey` | unregister old + register new live; only `ctrl+space`/`alt+space` (Win+Space reserved) |
| Start with Windows | Startup folder .lnk | `get_autostart`/`set_autostart` | WScript.Shell → `%APPDATA%\Microsoft\Windows\Start Menu\Programs\Startup\Finder.lnk` |

**Settings panel** (redesigned): full-width flat panel that takes over the card below the
search bar (results + preview hidden) — APPEARANCE / WINDOW / BEHAVIOR muted uppercase
section labels, hairline row separators, fixed right control column (switches, segmented
controls, sliders all aligned), scrollable with the results-style slim scrollbar. Esc/hide
closes it.

**Preview pane** (310px right column): large icon, or the real image file via `image_data`
(16 MB cap, extension whitelist, per-path base64 cache capped at 12 entries); flat meta rows
(Where / Size / Modified / Publisher / Version — hairline separated) and flat action rows
(Run / Run as administrator / Open file location / Uninstall / Reveal), labels flush with
the meta keys.

---

## 5. Indexing pipeline

### 5.1 Scan (`mft/reader.rs`)

- MFT read via the **ioctl path** (`FSCTL_ENUM_USN_DATA`-style / raw MFT ioctl — log:
  `scan drive C: 2885165 records via ioctl (read+parse 26.26s, index 1.41s; workers 12)`).
- 12 worker threads parse records into `CompactRecord`s; the name payloads are **UTF-16
  raw** (`&[u16]`) — no per-record String allocation during the scan.
- **Junk pruning** at scan time (`store.rs::populate_from_scan`): known junk subtrees
  (`%TEMP%`, `$Recycle.Bin`, Windows update caches…) are dropped by parent-ref chains; a
  `junk_refs` set per drive records the pruned roots so **live events can still recognize
  re-created files inside them** (`is_live_junk`).

### 5.2 In-memory index (`store.rs`) — the RAM story

The index is **fully resident in memory**; this is the bulk of the ~350 MB baseline.

| Structure | Per-entry bytes | 2.9M records |
|---|---|---|
| `entries: Vec<IndexEntry>` | 32 B (struct: 8+8+4+4+2+2+1+1, padded to align 8) | **~93 MB** |
| `name_arena: Vec<u8>` (UTF-16LE, original case) | preallocated 24 B/name (`chunk.len()*24`, store.rs:247) | ~58–70 MB |
| `name_lower_arena: Vec<u8>` (lowercased duplicate) | same | ~58–70 MB |
| `ref_lookup: Vec<Vec<(u64,u32)>>` (per-drive, sorted, binary search) | 16 B/entry (u64+u32 padding) | **~46 MB** |
| `ext_index: HashMap<String, Vec<u32>>` (extension → entry idx, lazy+dirty-tracked) | 4 B/file + Vec overhead | ~15 MB |
| `junk_refs: Vec<HashSet<u64>>` (pruned per drive) | 0.5–1.2M refs | ~8–20 MB |
| `drive_roots`, `checkpoints` | — | KB |

**2.9M records ≈ 280–320 MB resident — essentially 100 bytes per record.**

`IndexEntry` (32 B): `file_ref:u64, parent_ref:u64, name_off:u32, name_lower_off:u32,
name_len:u16, name_lower_len:u16, flags:u8 (bit0=is_dir), drive:u8`.

The **arenas** are the key design: names are offsets into two byte buffers, so there are no
2.9M heap Strings (that would be ~300 MB extra). `build_chunk` constructs the arenas in
parallel chunks; `to_cache`/`from_cache` (cache v2) persist the arenas **verbatim** — no
UTF-16 decode/re-lowercase on save or load; only the derived tables are rebuilt (parallel,
with a format-magic + version gate that rejects foreign caches). The whole blob is
additionally **lz4-compressed** (`lz4_flex`) at rest — the price is the large save/load
transients in §5.4.

**Live updates** (`apply_events`, `watcher.rs`): USN journal events arrive as `IndexEvent`
(Created/Deleted/Renamed/Moved/Checkpoint), each carrying its `drive_letter` (MFT refs are
per-volume and collide across drives). Mutations mark `ext_index` dirty (rebuilt before the
next extension search); `Checkpoint` records persist the USN position so a restart never
loses or duplicates changes.

### 5.3 Peak scan memory (transient, ~28 s)

During `populate_from_scan` (`store.rs:285–380`): `ScanResult` records+raw name data
(~220 MB), the `dirs: HashMap<u64,(String,u64)>` parent map (~600K heap Strings, ~43 MB),
the `clean`/`pruned` vecs (~90 MB), and the parallel `BuiltChunk` arenas while the
destination arenas are still filling (~2× arena cost) — **≈700–800 MB worst case**,
everything except the store itself dropped after `finalize`.

### 5.4 Cache save / load transients

- **Save (streamed, no spike)** — `save_cache` (`gui/main.rs:3130`): the snapshot clone
  (~217 MB) is still taken so the read lock releases before serialization, but the
  serialize + lz4 happen in **one streaming pass** (`bincode::serialize_into` into a
  `FrameEncoder` over the temp file) — no ~217 MB serialized Vec, no ~240 MB compressed
  Vec. Old ~950 MB transient is gone; the only remaining copy is the snapshot itself.
- **Load (~250 MB one-shot, down from ~480–520)**: `FrameDecoder` + `bincode::deserialize_from`
  straight off disk — the full-blob decompress allocation is gone; deserialize still
  allocates `CacheData` (~230 MB) before the fields move into the store.
  Cache format is now an lz4 frame stream; old prepended-size caches fail the header
  check → one fresh rescan.

---

## 6. Search (`index/search.rs` + path mode in `gui/main.rs`)

### 6.0 Path mode (queries that look like paths)

Queries matching `PATH_QUERY_RE`/`PATH_BARE_RE` (frontend) never touch the index —
they resolve against the **live filesystem**, so pruned trees (AppData, Program
Files, node_modules) stay reachable:

- Input forms: `C:\...`, UNC `\\server\...`, `%ENV%\...` (any tokens, expanded in
  Rust), `~` → user profile, bare aliases `appdata | localappdata | temp |
  userprofile | programfiles | programfilesx86 | windows | system32`.
- Exactly-existing path → a single row (file or folder; Enter opens).
- Partial path → chop to the deepest existing dir, then a **bounded** recursive
  walk of just the remainder (≤25K entries visited, ≤5K per dir, depth ≤3, ≤200
  rows; reparse points skipped so junctions can't loop). Multi-segment remainders
  can't match (names contain no `\`), so `%appdata%\Default\Prefer` never drowns
  in every "Default" folder.
- Rows are ordinary `file`/`dir` `UiResult`s — icons, preview, copy, open_parent,
  properties all work on real paths. New command `search_path` (`gui/main.rs:229`,
  registered in the handler); walk caps make it ~tens of ms.

### 6.1 Query classification

`extension_of`: a literal `.py` (≥3 chars) or a bare 2–6 char word that exists in
`ext_index` → **extension search**. Everything else → generic.

### 6.2 Extension search

`search_by_ext`: one hash lookup → the whole bucket (all files of that ext) is ranked in
**parallel**, sorted, sliced by page. That's why `.py` returns *all* python files instead of
the first 500 containment hits. Dot-named DIRECTORIES (`.config`, `.ssh`) join their
bucket (post-dot name), so a `.config` query surfaces the folder at rank 1 — dot-FILES
(`.gitignore`) stay bucket-free by design and only match via generic containment.

### 6.3 Generic search (`generic_paged`) — the hot path

**Every keystroke** triggers a full parallel scan of the whole index:

1. `par_iter` over all 2.9M entries, tier classification from `name_lower_arena`:
   `exact (1) → prefix (2) → word-prefix (3) → contains (4)`, packed into one u64:
   `(tier << 44) | (len.min(255) << 32) | idx` — a single 8-byte key carries sort rank.
2. `par_sort_unstable` — the whole match vector sorted by (tier, length, arena order).
3. Paging loop: junk chains skipped, excluded dirs (case-folded prefix match on the
   rebuilt full path) skipped, then `build_path` (parent-ref chain walk with binary search
   per level) + name + ranks → `SearchResult`.
4. **Fuzzy bottom pass** (`fuzzy_fill`, page 1 only, short queries): fzy-style abbreviation
   ranking over the name arena appends the best non-duplicates — catches "vsc"→VSCode.
5. `total`: exact count for extension queries (one hash lookup); 0 for generic.

**Per-keystroke allocation profile**: the match Vec holds *every* match — 8–23 MB plus a
full sort for a broad query like `e`; the perf gate (`search.rs:719`) asserts <50 ms on
300K entries, so at 2.9M records broad queries land at ~200–500 ms, scaling with cores
(a bounded top-k here is the main remaining lever — §8.3 #4, deferred). `fuzzy_fill` is
already bounded (§8.3 #3): per-worker min-heaps keep ≤200 candidates and the fzy matcher
is process-wide. `search_by_ext` builds a full path per bucket row during ranking (the
`user`/`depth` sort keys need it) but materializes names/PathBufs only for the sliced
page — a 300K-row bucket no longer allocates 300K `SearchResult`s per keystroke.

`SearchResult` = `PathBuf + name:String + rank:u8 + is_dir + modified_time + file_type_priority`.

**App pool**: `apps()` is cached in a `OnceLock` — the pool is enumerated once per process,
never per keystroke.

### 6.4 Path building

`build_path` walks `parent_ref` chains via `ref_lookup` binary search (O(depth·log n) per
result), assembles the path from the drive root down — no cached path strings, so paths are
always live and correct after renames.

---

## 7. Frontend architecture (`tauri-ui/main.js`)

- **Pools**: JS holds the app pool in memory; results render locally — the common keystroke
  path costs zero IPC for rendering, only the debounced backend search.
- **Search flow**: `scheduleSearch` (≈90 ms debounce) → `runSearchSafe` (per-input
  `searchSeq` guard drops stale responses) → renders into `#results` from the returned
  page; `fileTotal` footer for extension queries.
- **Icons**: `IntersectionObserver` (200 px rootMargin) enqueues icons only for
  near-viewport rows; a batched drain (`ICON_BATCH`) invokes the backend (base64 data URIs);
  `ms-settings:` rows are skipped; rows are fully rebuilt per render (GC-friendly).
- **Instant math**: local evaluator; results render in the math tier.
- **Compact mode**: empty query + setting → card collapses to a 400px slim bar at the normal
  position; typing restores the full card (width/height transitions).
- **Glass sync**: rAF loop tracks the card rect for the glass layer; stopped on window
  blur; re-synced when a fresh backdrop arrives.
- **Single-registration discipline**: every listener (`spotlight-hide`, `backdrop`),
  interval (`tickScanClock` 1 s, `refreshStatus` 1.5 s), and the rAF loop is registered once
  at boot; debounce timers clear-before-rewrite. This was verified during the memory audit.

---

## 8. Performance & memory — findings and levers

### 8.1 Where the RAM actually goes (~350 MB baseline)

1. **The index: ~280–320 MB resident** — ~100 B/record (math in §5.2: entries 93 +
   arenas ~128 + ref_lookup 46 + ext_index ~15 + junk_refs 8–20).
2. **App-pool icons: 5–60 MB** — every `AppEntry` embeds a 256 px base64 PNG URI at
   startup (`gui/main.rs:941`), then `icon_cache` clones them all again (`gui/main.rs:1541`)
   and never evicts — every distinct path iconed later lives forever too.
3. WebView2/Chromium (renderer+compositor+GPU) — a few tens of MB.
4. Backdrop: one ~100–250 KB string (trivial).

### 8.2 Verified-steady paths (no leak)

Hotkey mashing is flat now: backdrop gated by TTL + dHash + downscale + q55; listeners,
loops, timers all single-registration; preview-image cache capped at 12; icon cache bounded
by app count; pools cached in Rust (`OnceLock`) and JS.

### 8.3 Optimization levers (no behavior loss)

| # | Lever | What it removes | Effort / risk | Status |
|---|---|---|---|---|
| 1 | **Kill the save triple-copy** — serialize the store's fields directly (no `CacheData` clone; serialize each field into one writer) instead of `to_cache()` clone → serialize → lz4 | ~500 MB off the recurring (every 30 s) ~950 MB save spike; ~30–40 % faster saves | Low: `store.rs:400`, `gui/main.rs:3130`; format-compatible | ✅ **Done** (streamed `serialize_into → FrameEncoder`; snapshot clone kept, lock released before serialize; cache format → lz4 frame) |
| 2 | **`search_by_ext` rank before building** — collect `(ResultMeta, idx)` only, sort, slice the page, then `build_path`/clone names for the ~100 visible rows | ~300K path builds + 300K lowercase allocs per `.ext` keystroke on big buckets | Low: `search.rs:165–222`; zero behavior change | ✅ **Done** — note: the *rank* pass still builds (and drops) the path because `user`/`depth` are sort keys; only name/PathBuf materialization is deferred to the page |
| 3 | **`fuzzy_fill` bounded heap** — `BinaryHeap` capped at `need` (≤200) instead of collecting all matches | per-keystroke multi-MB fuzzy Vec + O(M log M) sort → ~3 KB + O(M log 200) | Low: `search.rs:426–443` | ✅ **Done** — per-worker min-heaps (`Reverse`), plus `SkimMatcherV2` hoisted to a process `OnceLock` (bigger win than the heap) |
| 4 | **`generic_paged` bounded top-k** — count-then-second-pass (or per-tier budget) instead of materializing *all* matches | 8–23 MB keys + full sort per keystroke for broad queries; fixes the ~200–500 ms worst case | Medium: `search.rs:323–351`; keep tier semantics | ⏳ deferred (not in scope) |
| 5 | **Icon cache: single owner + cap + smaller size** — `AppEntry.icon` owns the URI (drop the startup clone into `icon_cache`), cap `icon_cache` (512-entry LRU), use 32–48 px row icons | 5–60 MB startup duplicate + unbounded growth; 10–50× smaller IPC strings | Low: `gui/main.rs:628, 941, 1541` | ⏳ deferred |
| 6 | `UiResult.kind` as `&'static str` instead of `"file".to_string()` per row | per-row String allocs | Trivial: `gui/main.rs:212` | ✅ **Done** (`kind: &'static str`) |
| 7 | `search_apps`: lowercase the path once per `AppEntry` (or lowercase the `freq` key at insert) | ~40 KB of String allocs per keystroke | Trivial: `gui/main.rs:230, 313` | ⏳ deferred (trivial absolute cost) |
| 8 | Load-path: decompress into the deserialize input (`bincode::deserialize_from` over a reader) | ~230 MB off the one-shot load peak | Low: `gui/main.rs:2971` | ✅ **Done** (came free with #1's frame decoder) |
| 9 | Drop `name_lower_arena` (lowercase on the fly) or mmap the arenas | ~60–130 MB steady, or near-zero resident pages under pressure | High: format v3 + reindex / remap discipline — do after 1–5 | ⏳ deferred |

### 8.4 Speed notes (measured behavior, no work needed)

- Generic query = full 2.9M-entry parallel scan per keystroke; narrow queries are tens
  of ms, broad matches (short/common letters) land at ~200–500 ms with an 8–23 MB
  transient — the bounded top-k (lever #4) would remove both; fuzzy + ext paths are done.
- Extension queries are O(bucket) — instant *after* the deferral in lever #2 keeps the
  per-row path/name builds on the sliced page only.
- Icons: lazy + batched (viewport-only) — no startup icon storm.
- Backdrop capture ~3 ms (thumbnail) / ~15 ms (full) — off the hotkey-critical path (show
  proceeds with the cached grab).

---

## 9. Security posture

**Context**: the process is elevated and holds a full filesystem index — the renderer must
never be treated as a trust boundary. The surface is small (no remote navigation, no
`window.open`, no eval, local assets only — verified in `main.js`).

### Findings

| # | Severity | Finding | Location | Fix | Status |
|---|---|---|---|---|---|
| 1 | **High** (conditional) | `launch_uwp_elevated` interpolates `pfn` — derived from the renderer-supplied AUMID (the part before `!`) — into a PowerShell `-Command` **string** (`$_.PackageFamilyName -eq '{pfn}'`). A `'` breaks out; `exe` is already charset-validated (`aumid_exe_token`) but `pfn` is not. Compromised renderer → PowerShell **as Administrator**. | `main.rs:409–419` | Validate `pfn` against `^[A-Za-z0-9._-]+$` (or double the `'`) before interpolation; `-EncodedCommand` is the stronger fix | ✅ **Fixed** — charset gate + script travels via `-EncodedCommand` (base64 UTF-16LE) |
| 2 | **Medium** (EoP chain) | `uninstall_app` executes the registry `UninstallString` verbatim via `cmd /C`. `find_uninstall_entry` scans **HKCU first** — any non-elevated process can plant a fake entry (DisplayName + UninstallString) that the elevated app later runs as Administrator. | `main.rs:3312–3389` | Prefer HKLM entries; run uninstallers unelevated (or via `runas` UAC consent), or strip/validate the string | ✅ **Fixed** — `cmd.exe` gone: quote-aware first-token split + `raw_arg` passthrough (child's own argv parser), interpreter/bare-name refusal, HKLM-first hive order (per-user HKCU fallback retained) |
| 3 | Medium | `image_data` checks size+extension **after** `File::open`; the 16 MB check is TOCTOU (growing file between `metadata()` and `read_to_end`), and any file *named* `*.png` anywhere on the drive is base64'd to the renderer. | `main.rs:521–551` | Validate ext before open; hard cap with `take(16 MB+1)`; only serve paths present in the index | ✅ **Fixed** — ext-before-open, ADS `:` rejected, `symlink_metadata` regular-file-only (no symlinks/devices/pipes), `take()` hard cap |
| 4 | Medium | `csp: null` + `withGlobalTauri: true` — no CSP on the WebView2 page, while `window.__TAURI__` exposes 26 commands (incl. `launch_admin`, `uninstall_app`) to any injected script. No `eval` exists, so a strict CSP is safe to add. | `tauri.conf.json:8, 49` | `default-src 'self'; script-src 'self'; img-src 'self' data:; style-src 'self' 'unsafe-inline'; connect-src ipc: http://ipc.localhost` | ✅ **Fixed** — strict CSP set; the inline theme script moved to external `theme.js` so `script-src 'self'` works |
| 5 | Low/Medium | `set_autostart` interpolates self-derived paths into a PowerShell single-quoted script with no `'` escaping — a path containing `'` breaks or mutates the script. | `main.rs:503–509` | Escape `'` as `''` (or use `-EncodedCommand`) | ✅ **Fixed** — PowerShell removed entirely; `IShellLinkW` + `IPersistFile` write the `.lnk` directly |
| 6 | Low (DoS, elevated) | `parse_file_record` reads `record[aoff+16..aoff+22]` with no `alen >= 22` guard → a malformed MFT record (crafted USB/VHD) panics the scan thread. | `reader.rs:437–455` | Add `aoff + 22 <= record.len()` / `alen >= 24` guards | ✅ **Fixed** — `alen >= 22` gate before the resident-attribute header reads |
| 7 | Low (UB) | FSCTL-fallback scan builds a `u16` slice via `from_raw_parts` from kernel offsets without the watcher's bounds check (`name_offset > rec_len \|\| name_bytes > rec_len - name_offset`). | `reader.rs:288–318` vs `watcher.rs:457` | Copy the watcher's check before `from_raw_parts` | ✅ **Fixed** — `name_offset + name_len*2 > rec_len → break` before `from_raw_parts` |
| 8 | Low (DoS) | Startup hotkey is read from `HKCU\Software\Finder\Hotkey` without the runtime allowlist check — any same-user process can write a garbage accelerator and break summon at boot. | `main.rs:1758–1765` vs `1786` | Validate `hotkey_name()` against the same allowlist before `register` | ✅ **Fixed** — validated at read; invalid values rejected with a log line and ctrl+space fallback |
| 9 | Low (defense-in-depth) | `mathTokens()` is the only `innerHTML` with interpolated values (math-only input today — `MATH_RE` filters it); `glassLayerEl.style.backgroundImage` is the only CSS-string sink (Rust-built `data:image/jpeg;base64,` today). | `main.js:628, 990` | Build tokens with `createElement`/`textContent`; JS-side check of the `data:image/jpeg;base64,` prefix | ✅ **Fixed** — tokens built as DOM nodes (`replaceChildren`), backdrop URI regex-gated before assignment |
| 10 | Info | Elevated-by-design (full-drive index + `runas` launches); updater signed, https-only, passive — correct as-is. | everywhere / `tauri.conf.json:38–47` | Documented tradeoff; #1–#4 keep the renderer path hardened | ✅ as-is |

### Verified-clean

- **DOM injection**: results/meta/status/preview render with `textContent`/`createTextNode`
  (row names, chips, tags, tooltips via `.title`); the only two `innerHTML` uses are
  `mathTokens()` (math-filtered) and an empty-string clear. No `document.write`, no eval,
  no `window.open`/`location`, no inline handlers, no external resources.
- **Command construction**: all `explorer`/`rundll32`/`ShellExecuteW` launches pass paths as
  arguments (no shell); only the flagged PowerShell/cmd cases interpolate. `open_web_search`
  restricts schemes to http/https. NTFS names can't start with `/` or contain `:`, so no
  switch injection into `explorer /select,`.
- **Allowlist**: `window.hide/show/setFocus`, `globalShortcut`, `clipboard.writeText` only;
  the 26 custom commands are the real surface and are listed above. Tray = Show / Re-Index /
  Quit, fixed. Hotkey handler toggles show/hide only.
- **Updater**: minisign signature verification with embedded pubkey (no private material in
  repo — the publish script reads the password from an uncommitted local file), https-only.
- No hardcoded tokens/keys/passwords anywhere.

---

## 10. Operational notes

- The exe cannot be overwritten while running (it's elevated) — quit from the tray before
  rebuilding.
- Logs: `%LOCALAPPDATA%\Finder\log.txt`.
- Cache/scan timeline example (one machine): 2,885,165 records, read+parse 26.26 s, index
  1.41 s, finalize 0.12 s, save 0.33 s — index ready in 28.13 s.
- Rebuild loop: quit from tray → `cargo build --release --bin finder-gui` → relaunch
  via `Start-Process … -Verb RunAs`.