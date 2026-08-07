use rayon::prelude::*;
use crate::index::store::IndexStore;
use std::collections::BTreeSet;

const APP_EXTENSIONS: &[&str] = &["exe", "lnk", "msi", "appx", "msix"];
const APP_PATH_MARKERS: &[&str] = &[
    "\\program files\\", "\\program files (x86)\\",
    "\\start menu\\", "\\desktop\\", "\\appdata\\",
];

const JUNK_PATH_MARKERS: &[&str] = &[
   "\\windows\\", "\\program files\\", "\\program files (x86)\\",
    "\\$recycle.bin\\", "\\prefetch\\", "\\appdata\\local\\temp\\",
    "\\appdata\\local\\microsoft\\windows\\temporary internet files\\",
    "\\perflogs\\", "\\debug\\", "\\bin\\", "\\obj\\",
    "\\node_modules\\", "\\.git\\", "\\__pycache__\\"
];

// Add these constants
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

pub fn search(
    store: &IndexStore,
    query: &str,
    limit: usize,
    case_sensitive: bool,
    excluded_dirs: &[String],
) -> Vec<SearchResult> {
    if query.is_empty() {
        return Vec::new();
    }

    let q = if case_sensitive { query.to_string() } else { query.to_lowercase() };

    // ── Phase 1: lightweight name-only matching ──────────────────────────
    let entries = &store.entries;
    let name_lower_arena = &store.name_lower_arena;
    let name_arena = &store.name_arena;

    let mut candidates: Vec<(u32, u8)> = entries
        .par_iter()
        .enumerate()
        .filter_map(|(idx, entry)| {
            let name_cmp = if case_sensitive {
                unsafe { std::str::from_utf8_unchecked(&name_arena[entry.name_off as usize..(entry.name_off as usize + entry.name_len as usize)]) }
            } else {
                unsafe { std::str::from_utf8_unchecked(&name_lower_arena[entry.name_lower_off as usize..(entry.name_lower_off as usize + entry.name_lower_len as usize)]) }
            };

            let rank = if name_cmp == q { 1u8 }
                else if name_cmp.starts_with(&q) { 2 }
                else if name_cmp.contains(q.as_str()) { 3 }
                else { return None; };

            Some((idx as u32, rank))
        })
        .collect();

    // ── Phase 2: sort by rank, keep overshoot buffer ─────────────────
    candidates.sort_unstable_by_key(|&(_, rank)| rank);
    let overshoot = (limit * 5).max(1000);
    candidates.truncate(overshoot);

    // ── Phase 3: build paths + exclusions + app promotion ────────────
    let mut ranked: Vec<(ResultMeta, SearchResult)> = Vec::with_capacity(candidates.len());

    for &(idx, base_rank) in &candidates {
        let entry = &entries[idx as usize];
        let full_path = build_path(entry.file_ref, store);
        let path_lower = full_path.to_string_lossy().to_lowercase();
        if JUNK_PATH_MARKERS.iter().any(|m| path_lower.contains(m)) {
            continue;
        }

        if !excluded_dirs.is_empty() {
            let path_lower = full_path.to_string_lossy().to_lowercase();
            if excluded_dirs.iter().any(|ex| path_lower.starts_with(ex.as_str())) {
                continue;
            }
        }

        let name_lower = store.name_lower(entry);
        let ext_prio = ext_priority(name_lower);
        let rank = if base_rank <= 2 {
            let ext = name_lower.rsplit('.').next().unwrap_or("");
            if APP_EXTENSIONS.contains(&ext) {
                if APP_PATH_MARKERS.iter().any(|m| path_lower.contains(m)) {
                    0  // Installed app - top priority
                } else if !JUNK_PATH_MARKERS.iter().any(|m| path_lower.contains(m)) {
                    1  // App file but not in standard path
                } else {
                    base_rank  // App file in junk path, don't promote
                }
            } else {
                base_rank
            }
        } else {
            base_rank
        };

        let boundary = base_rank <= 2 || word_prefix_match(name_lower, &q);
        let user = USER_PATH_MARKERS.iter().any(|m| path_lower.contains(m));
        let depth = path_lower.matches('\\').count().min(15) as u8;
        let name_len = (name_lower.len() as u32).min(255) as u8;

        ranked.push((
            ResultMeta {
                rank,
                boundary: if boundary { 0 } else { 1 },
                user: if user { 0 } else { 1 },
                depth,
                name_len,
                ext_prio,
            },
            SearchResult {
                full_path,
                name: store.name(entry).to_string(),
                rank,
                is_dir: entry.is_dir(),
                modified_time: None,
                file_type_priority: ext_prio,
            },
        ));
    }

    ranked.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    ranked.into_iter().take(limit).map(|(_, r)| r).collect()
}

pub fn apps(_store: &IndexStore, _limit: usize) -> Vec<SearchResult> {
    let installed = get_installed_apps();
    installed.into_iter().take(_limit).map(|app| {
        SearchResult {
            full_path: std::path::PathBuf::from(app.path.unwrap_or_default()),
            name: app.name,
            rank: 0,
            is_dir: false,
            modified_time: None,
            file_type_priority: 0,
        }
    }).collect()
}

/// Iterative path builder — walks parent chain via sorted ref_lookup.
pub fn build_path(file_ref: u64, store: &IndexStore) -> std::path::PathBuf {
    let mut components: Vec<&str> = Vec::with_capacity(16);
    let mut current = file_ref;

    for _ in 0..64 {
        match store.lookup_idx(current) {
            Some(idx) => {
                let entry = &store.entries[idx as usize];
                components.push(store.name(entry));
                if entry.parent_ref == current {
                    break;
                }
                current = entry.parent_ref;
            }
            None => break,
        }
    }

    components.reverse();
    let mut path = std::path::PathBuf::from(&store.drive_root);
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
            drive_root: "C:\\".to_string(),
            checkpoints: Vec::new(),
        };
        for &(fr, name, parent, ref kind) in items {
            let lower = name.to_lowercase();
            let n_off = s.name_arena.len() as u32;
            let nl_off = s.name_lower_arena.len() as u32;
            s.name_arena.extend_from_slice(name.as_bytes());
            s.name_lower_arena.extend_from_slice(lower.as_bytes());
            let i = s.entries.len();
            s.entries.push(IndexEntry {
                file_ref: fr,
                parent_ref: parent,
                name_off: n_off,
                name_lower_off: nl_off,
                name_len: name.len() as u16,
                name_lower_len: lower.len() as u16,
                flags: if *kind == FileKind::Directory { 1 } else { 0 },
            });
            s.ref_lookup.push((fr, i as u32));
        }
        s.ref_lookup.sort_unstable_by_key(|&(r, _)| r);
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
}
