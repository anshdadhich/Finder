use rayon::prelude::*;
use crate::index::store::IndexStore;

const APP_EXTENSIONS: &[&str] = &["exe", "lnk", "msi", "appx", "msix"];
const APP_PATH_MARKERS: &[&str] = &[
    "\\program files\\", "\\program files (x86)\\",
    "\\start menu\\", "\\desktop\\", "\\appdata\\",
];

#[derive(Debug, Clone)]
pub struct SearchResult {
    pub full_path: std::path::PathBuf,
    pub name: String,
    pub rank: u8,
    pub is_dir: bool,
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
    let mut results: Vec<SearchResult> = Vec::with_capacity(limit);

    for &(idx, base_rank) in &candidates {
        let entry = &entries[idx as usize];
        let full_path = build_path(entry.file_ref, store);

        if !excluded_dirs.is_empty() {
            let path_lower = full_path.to_string_lossy().to_lowercase();
            if excluded_dirs.iter().any(|ex| path_lower.starts_with(ex.as_str())) {
                continue;
            }
        }

        let name_lower = store.name_lower(entry);
        let rank = if base_rank <= 2 {
            let ext_is_app = name_lower
                .rsplit('.')
                .next()
                .map(|e| APP_EXTENSIONS.contains(&e))
                .unwrap_or(false);
            if ext_is_app {
                let path_lower = full_path.to_string_lossy().to_lowercase();
                if APP_PATH_MARKERS.iter().any(|m| path_lower.contains(m)) { 0 } else { base_rank }
            } else {
                base_rank
            }
        } else {
            base_rank
        };

        results.push(SearchResult {
            full_path,
            name: store.name(entry).to_string(),
            rank,
            is_dir: entry.is_dir(),
        });
    }

    results.sort_unstable_by_key(|r| r.rank);
    results.truncate(limit);
    results
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
