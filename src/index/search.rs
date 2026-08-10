use rayon::prelude::*;
use crate::index::store::IndexStore;
use std::collections::BTreeSet;

const USER_PATH_MARKERS: &[&str] = &[
    "\\users\\", "\\documents\\", "\\downloads\\",
    "\\desktop\\", "\\pictures\\", "\\videos\\", "\\music\\"
];

#[derive(Debug, Clone)]
pub struct AppInfo {
    pub name: String,
    pub path: Option<String>,
    pub version: Option<String>,
}

/// Secondary sort key for results inside the same rank tier.
/// Field order below also defines the ordering (lower = better).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct ResultMeta {
    rank: u8,
    boundary: u8, // 0 = match starts at a word boundary
    user: u8,     // 0 = located under a user folder
    depth: u8,    // shallower path wins
    name_len: u8, // shorter name wins
    ext_prio: u8, // document/code extensions first
}

/// True when `q` appears in `name` at the start or right after a non-alphanumeric separator.
fn word_prefix_match(name: &str, q: &str) -> bool {
    if q.is_empty() {
        return true;
    }
    let bytes = name.as_bytes();
    let w = q.len();
    let mut start = 0usize;
    while start < name.len() {
        match name[start..].find(q) {
            Some(rel) => {
                let abs = start + rel;
                if abs == 0 || !bytes[abs - 1].is_ascii_alphanumeric() {
                    return true;
                }
                start = abs + w;
            }
            None => return false,
        }
    }
    false
}

/// Extension importance tiers: commonly searched document/code types first.
fn ext_priority(name_lower: &str) -> u8 {
    let ext = name_lower.rsplit('.').next().unwrap_or("");
    match ext {
        "txt" | "md" | "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx"
        | "csv" | "rtf" | "html" | "css" | "js" | "ts" | "json" | "yml" | "yaml"
        | "toml" | "ini" | "log" | "rs" | "c" | "h" | "cpp" | "hpp" | "py"
        | "go" | "java" | "cs" | "rb" | "php" | "sql" | "bat" | "cmd" | "ps1"
        | "ipynb" => 0,
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "svg" | "webp" | "ico"
        | "mp3" | "mp4" | "mkv" | "avi" | "mov" | "wav" | "flac" => 1,
        "zip" | "rar" | "7z" | "tar" | "gz" | "bz2" | "iso" => 2,
        _ => 3,
    }
}

pub fn get_installed_apps() -> Vec<AppInfo> {
    let mut apps = Vec::new();
    let mut seen = BTreeSet::new();
    
    let keys = [
        ("HKLM", "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Uninstall"),
        ("HKLM", "SOFTWARE\\Wow6432Node\\Microsoft\\Windows\\CurrentVersion\\Uninstall"),
        ("HKCU", "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Uninstall"),
    ];
    
    for (hive, path) in &keys {
        let key = match *hive {
            "HKLM" => winreg::RegKey::predef(winreg::enums::HKEY_LOCAL_MACHINE),
            "HKCU" => winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER),
            _ => continue,
        };
        
        let subkey = match key.open_subkey(path) {
            Ok(k) => k,
            Err(_) => continue,
        };
        
        for name in subkey.enum_keys().filter_map(|x| x.ok()) {
            let app_key = match subkey.open_subkey(&name) {
                Ok(k) => k,
                Err(_) => continue,
            };
            
            let display_name: String = app_key.get_value("DisplayName").unwrap_or_default();
            if display_name.is_empty() || seen.contains(&display_name) {
                continue;
            }
            seen.insert(display_name.clone());
            
            let install_location: Option<String> = app_key.get_value("InstallLocation").ok();
            let display_version: Option<String> = app_key.get_value("DisplayVersion").ok();
            
            apps.push(AppInfo {
                name: display_name,
                path: install_location,
                version: display_version,
            });
        }
    }
    
    apps.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    apps
}

#[derive(Debug, Clone)]
pub struct SearchResult {
    pub full_path: std::path::PathBuf,
    pub name: String,
    pub rank: u8,
    pub is_dir: bool,
    pub modified_time: Option<std::time::SystemTime>, // New field
    pub file_type_priority: u8, // New field
}

/// Paged search outcome. `total` is the exact match count for extension
/// queries (the whole class is knowable in one hash lookup); generic
/// queries report 0 ("unknown"), matching their one-shot cap.
pub struct PagedSearch {
    pub results: Vec<SearchResult>,
    pub total: usize,
}

/// Extension implied by `query`: a literal dot-extension (".py") or a bare
/// short word that happens to be an extension present in the index ("py",
/// "md", "ini"). Dot-prefixed queries may be any length.
fn extension_of(store: &IndexStore, q: &str) -> Option<String> {
    let ext = if q.starts_with('.') && q.len() >= 3 {
        Some(&q[1..])
    } else if !q.contains('.') && q.len() >= 2 && q.len() <= 6 {
        Some(q)
    } else {
        None
    }?;
    store.ext_index.contains_key(ext).then(|| ext.to_string())
}

/// Extension-class search: every file with that extension is a match, so the
/// whole bucket is ranked once (parallel), sorted, and sliced by page. This
/// is what makes ".py" return *all* python files instead of the first 500
/// name-containment hits the generic path can afford.
fn search_by_ext(
    store: &IndexStore,
    ext: &str,
    q: &str,
    limit: usize,
    skip: usize,
    excluded_dirs: &[String],
) -> (Vec<SearchResult>, usize) {
    let Some(bucket) = store.ext_index.get(ext) else {
        return (Vec::new(), 0);
    };

    let mut ranked: Vec<(ResultMeta, SearchResult)> = bucket
        .par_iter()
        .filter_map(|&idx| {
            let entry = &store.entries[idx as usize];
            if is_junk_chain(store, entry) {
                return None;
            }
            let full_path = build_path(entry, store);
            let path_lower = full_path.to_string_lossy().to_lowercase();
            if !excluded_dirs.is_empty()
                && excluded_dirs.iter().any(|ex| path_lower.starts_with(ex.as_str()))
            {
                return None;
            }

            let name_lower = store.name_lower(entry);
            let base_rank = if name_lower == q {
                1u8
            } else if name_lower.starts_with(q) {
                2
            } else {
                3
            };
            let boundary = base_rank <= 2 || word_prefix_match(name_lower, q);
            let user = USER_PATH_MARKERS.iter().any(|m| path_lower.contains(m));
            let depth = path_lower.matches('\\').count().min(15) as u8;
            let name_len = (name_lower.len() as u32).min(255) as u8;

            Some((
                ResultMeta {
                    rank: base_rank,
                    boundary: if boundary { 0 } else { 1 },
                    user: if user { 0 } else { 1 },
                    depth,
                    name_len,
                    ext_prio: 0, // every row shares the queried extension
                },
                SearchResult {
                    full_path,
                    name: store.name(entry).to_string(),
                    rank: base_rank,
                    is_dir: entry.is_dir(),
                    modified_time: None,
                    file_type_priority: 0,
                },
            ))
        })
        .collect();

    ranked.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    let total = ranked.len(); // filtered total — junk rows never counted
    let results = ranked
        .into_iter()
        .skip(skip)
        .take(limit)
        .map(|(_, r)| r)
        .collect();
    (results, total)
}

/// `search` plus extension-class paging. See `PagedSearch`.
pub fn search_paged(
    store: &IndexStore,
    query: &str,
    limit: usize,
    skip: usize,
    case_sensitive: bool,
    excluded_dirs: &[String],
) -> PagedSearch {
    let q = if case_sensitive {
        query.to_string()
    } else {
        query.to_lowercase()
    };
    if q.is_empty() {
        return PagedSearch {
            results: Vec::new(),
            total: 0,
        };
    }

    if !case_sensitive {
        if let Some(ext) = extension_of(store, &q) {
            let (results, total) = search_by_ext(store, &ext, &q, limit, skip, excluded_dirs);
            return PagedSearch { results, total };
        }
    }

    // ── Generic path: whole-index ranking with totals & real pagination ──
    generic_paged(store, &q, limit, skip, excluded_dirs)
}

/// Backward-compatible single-page wrapper (tests / callers that don't page).
pub fn search(
    store: &IndexStore,
    query: &str,
    limit: usize,
    case_sensitive: bool,
    excluded_dirs: &[String],
) -> Vec<SearchResult> {
    search_paged(store, query, limit, 0, case_sensitive, excluded_dirs).results
}

/// Junk-empty without building a full path string: walk the parent chain
/// through the sorted file_ref lookup (inside the entry's own drive — refs
/// collide across volumes) and compare each ancestor directory's lowercased
/// name against the compact junk list. Deterministic, allocation free,
/// ~1µs per row — so page windows of 100 rows cost microseconds.
fn is_junk_chain(store: &IndexStore, entry: &crate::index::store::IndexEntry) -> bool {
    let drive = entry.drive;
    let mut current = entry.parent_ref;
    for _ in 0..32 {
        // Parents pruned at scan time are not in the lookup; recognize them
        // through the recorded junk ref set for this drive.
        if store
            .junk_refs
            .get(drive as usize)
            .map_or(false, |s| s.contains(&current))
        {
            return true;
        }
        let Some(idx) = store.lookup_idx(drive, current) else {
            break;
        };
        let e = &store.entries[idx as usize];
        if !e.is_dir() {
            break;
        }
        let name = store.name_lower(e);
        if crate::index::store::JUNK_DIR_NAMES.iter().any(|j| *j == name) {
            return true;
        }
        let next = e.parent_ref;
        if next == current || next == 0 {
            break;
        }
        current = next;
    }
    false
}

/// Generic (non-extension) paged search.
///
/// The Raycast part: one parallel scan ranks *every* match into a compact
/// u64 key (tier | boundary | name length | entry index), the top of the
/// ranked list is sorted once, and pages are deterministic slices of that
/// same order. There is no "first 500 contains hits" window anymore — every
/// query has a real total and every page walks the same ranked list.
fn generic_paged(
    store: &IndexStore,
    q: &str,
    limit: usize,
    skip: usize,
    excluded_dirs: &[String],
) -> PagedSearch {
    let entries = &store.entries;
    let name_lower_arena = &store.name_lower_arena;

    let mut keys: Vec<u64> = entries
        .par_iter()
        .enumerate()
        .filter_map(|(idx, entry)| {
            let n = unsafe {
                std::str::from_utf8_unchecked(
                    &name_lower_arena[entry.name_lower_off as usize
                        ..(entry.name_lower_off as usize + entry.name_lower_len as usize)],
                )
            };
            let tier: Option<u64> = if n == q {
                Some(1u64)
            } else if n.starts_with(q) {
                Some(2u64)
            } else if word_prefix_match(n, q) {
                Some(3u64)
            } else if n.contains(q) {
                Some(4u64)
            } else {
                None
            };
            tier.map(|t| {
                (t << 44) | ((n.len().min(255) as u64) << 32) | (idx as u32 as u64)
            })
        })
        .collect();

    let total = keys.len();
    keys.par_sort_unstable();

    // Page: clean rows only. Junk ancestors are skipped but never counted,
    // so the offset for the next page stays consistent.
    let mut results: Vec<SearchResult> = Vec::with_capacity(limit);
    let mut used_refs: std::collections::HashSet<u64> =
        std::collections::HashSet::with_capacity(limit + 16);
    let mut clean_seen = 0usize;

    for &key in &keys {
        if results.len() >= limit {
            break;
        }
        let idx = (key & 0xFFFF_FFFF) as usize;
        let entry = &entries[idx];
        if is_junk_chain(store, entry) {
            continue;
        }
        let full_path = build_path(entry, store);
        let path_lower = full_path.to_string_lossy().to_lowercase();
        if !excluded_dirs.is_empty()
            && excluded_dirs.iter().any(|ex| path_lower.starts_with(ex.as_str()))
        {
            continue;
        }
        if clean_seen < skip {
            clean_seen += 1;
            continue;
        }
        let name_lower = store.name_lower(entry);
        let ext_prio = ext_priority(name_lower);
        used_refs.insert(entry.file_ref);
        results.push(SearchResult {
            full_path,
            name: store.name(entry).to_string(),
            rank: ((key >> 40) & 0xF) as u8,
            is_dir: entry.is_dir(),
            modified_time: None,
            file_type_priority: ext_prio,
        });
    }

    // Fuzzy bottom pass, only when the short search ran dry (abbreviations
    // like "vsc" never appear as substring matches). Page 1 only: the fuzzy
    // candidate stream is ranked per query, so later pages would re-select
    // the same rows and duplicate them; page 1 already surfaces everything
    // worth seeing, and plain (deterministic) rows serve the rest.
    if q.len() >= 2 && skip == 0 && results.len() < limit {
        fuzzy_fill(store, q, excluded_dirs, &used_refs, &mut results, limit);
    }

    PagedSearch { results, total }
}

/// Lay a fuzzy (fzy) ranking over the whole name arena, appending the best
/// non-duplicate, non-junk, non-excluded candidates as a bottom rank tier.
fn fuzzy_fill(
    store: &IndexStore,
    q: &str,
    excluded_dirs: &[String],
    used_refs: &std::collections::HashSet<u64>,
    results: &mut Vec<SearchResult>,
    limit: usize,
) {
    use fuzzy_matcher::FuzzyMatcher;
    let matcher = fuzzy_matcher::skim::SkimMatcherV2::default();

    let need = (limit - results.len()).min(200);
    if need == 0 {
        return;
    }

    let entries = &store.entries;
    let name_lower_arena = &store.name_lower_arena;

    let mut best: Vec<(i64, u32)> = entries
        .par_iter()
        .enumerate()
        .filter_map(|(idx, entry)| {
            if used_refs.contains(&entry.file_ref) {
                return None;
            }
            let n = unsafe {
                std::str::from_utf8_unchecked(
                    &name_lower_arena[entry.name_lower_off as usize
                        ..(entry.name_lower_off as usize + entry.name_lower_len as usize)],
                )
            };
            matcher.fuzzy_match(n, q).map(|score| (score, idx as u32))
        })
        .collect();
    best.par_sort_unstable_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    best.truncate(need);

    for (_, idx) in best {
        if results.len() >= limit {
            break;
        }
        let entry = &entries[idx as usize];
        if is_junk_chain(store, entry) {
            continue;
        }
        let full_path = build_path(entry, store);
        let path_lower = full_path.to_string_lossy().to_lowercase();
        if !excluded_dirs.is_empty()
            && excluded_dirs.iter().any(|ex| path_lower.starts_with(ex.as_str()))
        {
            continue;
        }
        results.push(SearchResult {
            full_path,
            name: store.name(entry).to_string(),
            rank: 5,
            is_dir: entry.is_dir(),
            modified_time: None,
            file_type_priority: ext_priority(store.name_lower(entry)),
        });
    }
}

pub fn apps(_store: &IndexStore, limit: usize) -> Vec<SearchResult> {
    // Enumerating the registry is expensive (~tens of ms). Cache the result
    // for the process lifetime so repeated calls are instant.
    static CACHE: std::sync::OnceLock<Vec<AppInfo>> = std::sync::OnceLock::new();
    let installed = CACHE.get_or_init(get_installed_apps);
    installed.iter().take(limit).map(|app| {
        SearchResult {
            full_path: std::path::PathBuf::from(app.path.clone().unwrap_or_default()),
            name: app.name.clone(),
            rank: 0,
            is_dir: false,
            modified_time: None,
            file_type_priority: 0,
        }
    }).collect()
}

/// Iterative path builder — walks the parent chain via the entry's own
/// drive's sorted ref_lookup, rooted at that volume's root.
pub fn build_path(entry: &crate::index::store::IndexEntry, store: &IndexStore) -> std::path::PathBuf {
    let drive = entry.drive;
    let root = store
        .drive_roots
        .get(drive as usize)
        .map(|d| d.root.as_str())
        .unwrap_or("C:\\");
    let mut components: Vec<&str> = Vec::with_capacity(16);
    let mut current = entry.file_ref;

    for _ in 0..64 {
        match store.lookup_idx(drive, current) {
            Some(idx) => {
                let e = &store.entries[idx as usize];
                components.push(store.name(e));
                if e.parent_ref == current {
                    break;
                }
                current = e.parent_ref;
            }
            None => break,
        }
    }

    components.reverse();
    let mut path = std::path::PathBuf::from(root);
    for comp in components {
        path.push(comp);
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::store::{IndexEntry, IndexStore};
    use crate::mft::types::FileKind;

    fn store(items: &[(u64, &str, u64, FileKind)]) -> IndexStore {
        let mut s = IndexStore {
            entries: Vec::new(),
            name_arena: Vec::new(),
            name_lower_arena: Vec::new(),
            ref_lookup: Vec::new(),
            drive_roots: vec![crate::index::store::DriveRoot {
                letter: 'C',
                root: "C:\\".to_string(),
            }],
            checkpoints: Vec::new(),
            junk_refs: vec![std::collections::HashSet::new()],
            ext_index: std::collections::HashMap::new(),
            ext_dirty: true,
        };
        for &(fr, name, parent, ref kind) in items {
            let lower = name.to_lowercase();
            let n_off = s.name_arena.len() as u32;
            let nl_off = s.name_lower_arena.len() as u32;
            s.name_arena.extend_from_slice(name.as_bytes());
            s.name_lower_arena.extend_from_slice(lower.as_bytes());
            s.entries.push(IndexEntry {
                file_ref: fr,
                parent_ref: parent,
                name_off: n_off,
                name_lower_off: nl_off,
                name_len: name.len() as u16,
                name_lower_len: lower.len() as u16,
                flags: if *kind == FileKind::Directory { 1 } else { 0 },
                drive: 0,
            });
        }
        // Keep the same sorted-by-lowercase-name invariant the production
        // store maintains (finalize / insert / apply_events).
        let store_ptr = &s as *const IndexStore;
        s.entries.sort_unstable_by(|a, b| {
            let st = unsafe { &*store_ptr };
            st.name_lower(a).cmp(st.name_lower(b))
        });
        let mut pairs: Vec<(u64, u32)> = s
            .entries
            .iter()
            .enumerate()
            .map(|(i, e)| (e.file_ref, i as u32))
            .collect();
        pairs.sort_unstable_by_key(|&(r, _)| r);
        s.ref_lookup = vec![pairs];
        s
    }

    #[test]
    fn ordering_brings_good_matches_first() {
        // dirs: 30 = node_modules (junk), 40 = Users (user path marker)
        let store = store(&[
            (40, "Users", 0, FileKind::Directory),
            (30, "node_modules", 0, FileKind::Directory),
            (1, "Report.pdf", 40, FileKind::File),        // prefix + user dir
            (2, "report.txt", 0, FileKind::File),          // prefix, root-level
            (3, "report_final.docx", 0, FileKind::File),   // prefix match
            (4, "deptreport.pdf", 0, FileKind::File),      // mid-word contains
            (5, "report.js", 30, FileKind::File),          // junk, filtered
        ]);
        let results = search(&store, "report", 10, false, &[]);
        let names: Vec<_> = results.iter().map(|r| r.name.as_str()).collect();
        assert!(names.iter().all(|n| *n != "report.js"));
        assert_eq!(names[0], "Report.pdf");   // prefix match first, in user dir
        assert_eq!(names[1], "report.txt");   // prefix match, root-level (shorter name)
        assert_eq!(names[2], "report_final.docx"); // boundary beat mid-word
        assert_eq!(names[3], "deptreport.pdf");
    }

    #[test]
    fn fuzzy_fallback_surfaces_abbreviations() {
        // "vsc" is an abbreviation — no literal name contains it, so only the
        // fuzzy fallback can surface Visual Studio Code.
        let store = store(&[
            (1, "Visual Studio Code.lnk", 0, FileKind::File),
            (2, "Vim.exe", 0, FileKind::File),
            (3, "somethingelse.txt", 0, FileKind::File),
        ]);
        let results = search(&store, "vsc", 10, false, &[]);
        let names: Vec<&str> = results.iter().map(|r| r.name.as_str()).collect();
        assert!(names.contains(&"Visual Studio Code.lnk"));
    }

    #[test]
    fn fuzzy_never_bumps_literal_matches() {
        // Prefix matches must stay in front of any fuzzy-filled results.
        let store = store(&[
            (1, "Visual Studio Code.lnk", 0, FileKind::File),
            (2, "visualstudio.exe", 0, FileKind::File),
            (3, "Vim.exe", 0, FileKind::File),
        ]);
        let results = search(&store, "vis", 10, false, &[]);
        let names: Vec<&str> = results.iter().map(|r| r.name.as_str()).collect();
        // Both literal prefix matches lead the list.
        assert!(names.contains(&"Visual Studio Code.lnk"));
        assert!(names.contains(&"visualstudio.exe"));
        assert!(names[0] == "Visual Studio Code.lnk" || names[0] == "visualstudio.exe");
        assert!(names[1] == "Visual Studio Code.lnk" || names[1] == "visualstudio.exe");
    }

    #[test]
    fn ext_search_returns_all_matches_with_paging() {
        let names: Vec<String> = (0..300).map(|i| format!("mod_{:03}.py", i)).collect();
        let mut items: Vec<(u64, &str, u64, FileKind)> = names
            .iter()
            .enumerate()
            .map(|(i, n)| (1000 + i as u64, n.as_str(), 40, FileKind::File))
            .collect();
        items.push((99, "notes.txt", 40, FileKind::File));

        let mut s = store(&items);
        s.rebuild_ext_index();

        let p1 = search_paged(&s, ".py", 100, 0, false, &[]);
        assert_eq!(p1.total, 300);
        assert_eq!(p1.results.len(), 100);

        let p2 = search_paged(&s, ".py", 100, 100, false, &[]);
        assert_eq!(p2.results.len(), 100);

        let p3 = search_paged(&s, ".py", 100, 200, false, &[]);
        assert_eq!(p3.results.len(), 100);

        let names1: std::collections::HashSet<String> =
            p1.results.iter().map(|r| r.name.clone()).collect();
        let names2: std::collections::HashSet<String> =
            p2.results.iter().map(|r| r.name.clone()).collect();
        let names3: std::collections::HashSet<String> =
            p3.results.iter().map(|r| r.name.clone()).collect();
        assert_eq!(names1.len(), 100);
        assert!(names1.is_disjoint(&names2));
        assert!(names1.is_disjoint(&names3));
        assert!(names2.is_disjoint(&names3));

        // Bare extension word behaves the same.
        let bare = search_paged(&s, "py", 100, 0, false, &[]);
        assert_eq!(bare.total, 300);
        assert_eq!(bare.results.len(), 100);

        // Unrelated extension still works and reports its exact total.
        let txt = search_paged(&s, ".txt", 100, 0, false, &[]);
        assert_eq!(txt.total, 1);
        assert_eq!(txt.results.len(), 1);
        assert_eq!(txt.results[0].name, "notes.txt");
    }

    #[test]
    fn ext_search_is_ranked_and_dotfiles_do_not_leak() {
        let items = [
            (1, "setup.py", 40, FileKind::File),
            (2, "PyGame.py", 40, FileKind::File),
            (3, ".pyproject", 40, FileKind::File), // dotfile: no extension bucket
            (4, "init.py", 40, FileKind::File),
        ];
        let mut s = store(&items);
        s.rebuild_ext_index();

        let page = search_paged(&s, ".py", 10, 0, false, &[]);
        // ".pyproject" is not a .py file — only 3 true matches.
        assert_eq!(page.total, 3);
        assert_eq!(page.results.len(), 3);

        // Bare form: names starting with the query ("PyGame") rank first.
        let bare = search_paged(&s, "py", 10, 0, false, &[]);
        assert_eq!(bare.total, 3);
        assert_eq!(bare.results[0].name, "PyGame.py");
    }

    #[test]
    fn ext_index_covers_all_files_but_skips_junk() {
        let items = [
            (40, "Users", 0, FileKind::Directory),
            (30, "node_modules", 0, FileKind::Directory),
            (1, "app.py", 40, FileKind::File),
            (2, "tool.py", 30, FileKind::File), // under node_modules (junk)
        ];
        let mut s = store(&items);
        s.rebuild_ext_index();

        // Total counts only searchable rows; junk is filtered from both the
        // count and the page so "N more" math stays honest.
        let page = search_paged(&s, ".py", 10, 0, false, &[]);
        assert_eq!(page.total, 1);
        assert_eq!(page.results.len(), 1);
        assert_eq!(page.results[0].name, "app.py");
        assert_eq!(page.results[0].name, "app.py");
    }

    #[test]
    #[ignore]
    fn perf_large_index() {
        // Synthetic ~300k-entry index. Run with:
        //   cargo test --lib perf_large_index -- --ignored --nocapture
        use std::time::Instant;

        let n = 300_000u32;
        let mut s = IndexStore {
            entries: Vec::new(),
            name_arena: Vec::new(),
            name_lower_arena: Vec::new(),
            ref_lookup: Vec::new(),
            drive_roots: vec![crate::index::store::DriveRoot {
                letter: 'C',
                root: "C:\\".to_string(),
            }],
            checkpoints: Vec::new(),
            junk_refs: vec![std::collections::HashSet::new()],
            ext_index: std::collections::HashMap::new(),
            ext_dirty: true,
        };
        let base = 100u64;
        for i in 0..n {
            let stem = match i % 6 {
                0 => "report",
                1 => "desktop",
                2 => "chrome",
                3 => "asset_v2",
                _ => "notes",
            };
            let dir = match i % 3 {
                0 => "folder_a",
                1 => "folder_b",
                _ => "downloads",
            };
            let name = format!("{}_{}_{}.txt", stem, dir, i);
            let lower = name.to_lowercase();
            let n_off = s.name_arena.len() as u32;
            let nl_off = s.name_lower_arena.len() as u32;
            s.name_arena.extend_from_slice(name.as_bytes());
            s.name_lower_arena.extend_from_slice(lower.as_bytes());
            s.entries.push(IndexEntry {
                file_ref: base + i as u64,
                parent_ref: base,
                name_off: n_off,
                name_lower_off: nl_off,
                name_len: name.len() as u16,
                name_lower_len: lower.len() as u16,
                flags: 0,
                drive: 0,
            });
        }
        s.finalize();

        let mut total_ms = 0.0f64;
        for q in ["report", "desktop", "chrome", "notes", "asset", "zzzz"] {
            let t0 = Instant::now();
            let res = search(&s, q, 300, false, &[]);
            let dt = t0.elapsed().as_secs_f64() * 1000.0;
            total_ms += dt;
            println!("query={:?} hits={} took {:.2}ms", q, res.len(), dt);
        }
        let avg = total_ms / 6.0;
        println!("avg {:.2}ms over 6 queries (300k entries)", avg);
        assert!(avg < 50.0, "search slower than expected: {:.1}ms avg", avg);
    }
}
