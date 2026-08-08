# FastSeek — Production Plan & Backlog

Status legend: `[done] [doing] [todo] [blocked]`
Loop: **audit → plan → build → verify → repeat**. Every round: 2+ reviewer lenses (UX, features, bug hunt) + online research → work items here → implement → test → relaunch.

## Principles (non-negotiable)

1. **No functionality is removed.** Every existing command/feature stays working each round (verified by `cargo test` + manual checklist).
2. **Relevant first, everything reachable**: like macOS/Windows search, *every* matching file is reachable (real totals + paging) and the most likely target is in the top rows.
3. **Ship-ready bar** (from shipping checks + PowerToys Run / Wox / Raycast / Windows UX research):
   - Cold launch (hotkey → visible) **< 600 ms**, warm < 200 ms
   - Global hotkey works on first launch; no silent failures
   - Second instance focuses the running app (single-instance)
   - Tray icon + menu; Quit actually quits; Close-to-tray semantics
   - Auto-start opt-in, first-run onboarding, settings that matter
   - No dead-ends: empty state, no-results state, error states always have a next step

## 2. Current state inventory (verified against code)

**Search backend** `[done]`: MFT scan (`mft/reader.rs` scan_direct), parallel populate + prefiltered junk (`index/store.rs`), whole-index ranking with totals + paging (`index/search.rs` generic_paged + ext buckets), fuzzy bottom tier, per-question exact totals, `search_files(query, offset) → {items,total}`.

**UI core** `[done]`: exclusive scan/ready states, rAF render gate + row pool, chips→icons (IntersectionObserver + batching), "Files · N" headers, show-more row, keyboard nav (↑↓ Enter Esc Ctrl/Shift/Alt+Enter), spotlight position, blur-hides, hint bar.

**System integration** `[partial]`: tray menu (Show/Quit), hotkeys Super+Space / Ctrl+Space, blur hide, close→save cache.
Gaps: single-instance, tray Quit flushes cache (currently `std::process::exit(0)` skips it), hotkey conflict handling, auto-start.

**Scan/onboarding screen** `[partial]`: `#scanState` full-window page with title/sub/status/progress; first-run vs rebuild copy. Gaps: no elapsed time/ETA, no error detail, no way to rebuild from UI, thin visual quality.

## 3. Roadmap (execution order)

### PHASE A — System integration (ship blockers)
- [x] A0 Global hotkeys + tray + blur-hide + spotlight position (`gui/main.rs`)
- [ ] A1 **Single-instance** — mutex; second launch shows/focuses existing instance
- [ ] A2 Tray Quit flushes cache gracefully (no `process::exit(0)`)
- [ ] A3 Hotkey register failure → log + still functional via tray; retry registration
- [ ] A4 Measure cold/warm show latency; shave frontend first paint (< 600 ms cold)
- [ ] A5 Auto-start opt-in (HKCU\Run) in settings (Phase D UI)

### PHASE B — First-run / index experience (the "installation screen")
- [ ] B1 **Scan screen polish**: elapsed timer, phase-aware copy, animated indeterminate bar, fail-list of unreadable drives, smooth enter/exit transitions
- [ ] B2 **Rebuild affordance**: tray menu "Re-index" + palette hint; deletes cache, re-enters scan state
- [ ] B3 Cache summary in footer: "N files · rebuilt Xm ago" (persisted ts in cache)
- [ ] B4 First-run: shrink prints (one-time show), post-scan "Ready" micro-moment

### PHASE C — Relevance & completeness ("every file, relevant first")
- [ ] C1 **mtime into the index** (MFT record field; cache v2) → "Modified recently" tier + recency boost — big relevance win
- [ ] C2 **Multi-word queries**: split on spaces; per-word prefix/fuzzy match; boundary scoring — "show me report", "download vscode zip"
- [ ] C3 Single-char queries: enable 1-char with ranked prefix heads
- [ ] C4 Empty-palette state: recent apps + top recent files instead of blank (SaaS palette pattern)
- [ ] C5 **Actions on rows**: Copy path (Ctrl+C), open folder, properties, admin — + context menu on right-click; hover ≠ selection
- [ ] C6 No-results dead end: offer web search + closest-match suggestion
- [ ] C7 Verify totals/paging invariants (audits pending) — no cap regressions

### PHASE D — UX/UI polish
- [ ] D1 Clear (×) button in search field (Windows guidance), prompt "Type to search"
- [ ] D2 Focus-visible ring, selected-row hint animation, 120 ms transitions, reduced-motion support
- [ ] D3 Scrollbar + hover styles, row density option
- [ ] D4 Placement: display-with-cursor (PowerToys-like), position remembered per monitor
- [ ] D5 Settings surface (theme accent, hotkey, exclusions, startup, index mgmt)
- [ ] D6 Accessibility: ARIA listbox/option, contrast pass

### PHASE E — Ship
- [ ] E1 NSIS installer (bundle.active=true → installer, icon, version bump 1.0.0)
- [ ] E2 Crash reporter: panic hook → `%LOCALAPPDATA%\FastSeek\logs`, in-app error surface (no silent death)
- [ ] E3 README + manual acceptance checklist (Phase A–D verify items)
- [ ] E4 Release build validation: cold/warm clock, memory, no zombie processes

## 4. Verification (run daily)

```sh
cargo test                     # 8+ tests: search paging, junk chains, cache roundtrip
node --check tauri-ui/main.js
# Manual: launch → hotkey → type → paging → actions → tray Quit → launch cache
```

## 5. Findings log (from review cycles)
_Appended as audit agents report; each = ID | severity | who | finding | fix | status._

### Round 1 audits (4 agents: frontend bugs / backend bugs / features / UX) — Aug 8

| ID | Sev | src | Finding | Fix / status |
|---|---|---|---|---|
| F1 | P0 | FE | Enter/click launches the WRONG item: `items[selected]` vs re-grouped `rowEls` order | **FIXED** — rows carry `el._item`; actions resolve via the row (main.js) |
| F2 | P0 | FE | "Show more results" row dead: opens Google/random file | **FIXED** — more row is a real item; Enter/click calls `loadMoreFiles()` |
| F3 | P1 | FE | Scroll-to-bottom paging never fired (`items.some(kind==="more")` dead code) | **FIXED** — checks rendered rows for `.more` |
| F4 | P1 | FE | `loadMoreFiles` offset/dedupe race (stale snapshot, no seq guard) | **FIXED** — seq guard + re-dedupe vs current `items` |
| F5 | P1 | FE | Ordinary queries "can't page" | Verified OK — `generic_paged` returns real totals (this audit read stale code) |
| F6 | P2 | FE | Apps frecency lost client-side (clientApps ignores freq; 16 vs 10 cap) | Open — align caps/ranks (Phase C) |
| F7 | P2 | FE | Unhandled promise rejections in search/launch paths | **FIXED** — try/catch + inline error bar; window hides only on success |
| F8 | P2 | FE | `iconCache` unbounded | **FIXED** — capped at 2000 (LRU-ish) |
| F9 | P2 | FE | MAX_TOTAL_FILES clamp "lies" in header | Open — show true total, clamp only loading |
| F10 | P2 | FE | `input.select()` wipes query on every focus | **FIXED** — once per session |
| F11 | P2 | FE | Stale `selected` after list shrinks; hover yanks keyboard selection | **FIXED** — clamp in updateSelection + 300ms hover suppress |
| B1 | P0 | Backend | Multi-drive: one shared `drive_root` + shared `ref_lookup` → C: paths resolve to D: refs; can launch the WRONG program | Open — Top priority backlog (per-drive refs/roots; cache v2) |
| B2 | P0 | Backend | Cache-save race: same `tmp.<pid>` name, two writers → torn cache | **FIXED** — unique tmp + `SAVE_LOCK` + seq (main.rs `write_atomic`) |
| B3 | P0 | Backend | Watcher dies silently (journal reset) → stale "ready" index forever | **FIXED** — heartbeat + GUI watchdog flips to scan state |
| B4 | P0 | Backend | Live junk leak: journal inserts under pruned subtrees re-enter index | **FIXED** — `junk_refs` recorded at scan; `is_live_junk` on insert/apply_events |
| B5 | P0 | Backend | Ext totals raw while pages junk-filtered → wrong "N more"/offset drift | **FIXED** — `search_by_ext` returns filtered total |
| B6 | P0 | Backend | Fuzzy fill duplicates across pages | **FIXED** — fuzzy stream only on page 1 (deterministic pages) |
| B7 | P1 | Backend | USN record OOB read (name offset/len untrusted) | **FIXED** — rec_len + name bounds guards (watcher.rs) |
| B8 | P1 | Backend | Lock starvation: applier blocked while long search holds read lock | Open — snapshot-search refactor (Ph. C7) |
| B9 | P1 | Backend | Unbounded cache deserialize (OOM on huge cache file) | Open — cap entries on load |
| U1 | P0 | UX | Non-admin run = empty dead index with a working-looking palette | **FIXED** — manifest always embedded (elevated); 0-files → scan page + "Try again" |
| U2 | P0 | UX | Scan screen dead end: no retry/quit/elapsed | **FIXED** — elapsed clock + Try again (rebuild) + Quit |
| U3 | P0 | UX | Hotkeys Win+Space/Ctrl+Space collide with OS/IME | Partial — added Ctrl+Alt+Space fallback; conflict banner open |
| U4 | P0 | UX | No single-instance guard (two indexes, two watchers, cache fights) | **FIXED** — named mutex + focus existing window |
| U5 | P1 | UX | Tray Quit `exit(0)` skipped cache save | **FIXED** — save + `app.exit(0)`; tray adds "Re-Index Files" |
| U6 | P1 | UX | Cache in %TEMP% wiped by cleanup → rescan storms | **FIXED** — LOCALAPPDATA + migration |
| U7 | P1 | UX | Dir paging: folders capped 10, no "more" | **FIXED** — dirs no longer sliced (bounded by MAX_ITEMS) |
| U8 | P1 | UX | Esc only closed; no clear; no PgUp/PgDn/Home/End; no Ctrl+C | **FIXED** — Esc clears→closes, paging keys, Ctrl+C copies path |
| U9 | P1 | UX | Folder "number … bottom" coarseness | Open — Phase C2 |
| U10 | P2 | UX | **multi-monitor placement**: centers on window monitor, not cursor | Open — Phase D4 |
| P1 | — | Features | Onboarding/installer/updater/auto-start/settings/theme | Backlog — Phase E/D |

## 6. Open backlog (from audits, ordered by impact)

1. [P0] **Multi-drive correctness** (B1): per-drive ref_lookup + drive_root resolution; cache v2 (entry.drive); tests. Launching a wrong program is unacceptable on 2-drive boxes.
2. [P1] Frecency persistence (apps across sessions) + drive app freq into client ranking (F6, U-app).
3. [P1] mtime into the index (relevance + "recent files") — Phase C1, needs cache v2 same bump as #1.
4. [P1] Snapshot search to unblock the applier (B7).
5. [P1] Ext totals/live: caps on cache load (B9).
6. [P2] Hotkey conflict detection + in-app banner; theme auto (dark mode); accent; multi-monitor (cursor) placement; auto-start opt-in.