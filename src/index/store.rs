#![allow(dead_code)]
use rayon::prelude::*;
use serde::{Serialize, Deserialize};
use crate::mft::types::{FileKind, FileRecord, IndexEvent, JournalCheckpoint};
use crate::mft::reader::{CompactRecord, ScanResult};

/// Directory names (lowercased) that mark a subtree as junk: anything under
/// these needs no indexing, no searching and no cache space. The same list
/// gates both the scan-time prefilter (build) and the live search filter.
pub const JUNK_DIR_NAMES: &[&str] = &[
    "windows",
    "program files",
    "program files (x86)",
    "$recycle.bin",
    "prefetch",
    "appdata",
    "temp",
    "perflogs",
    "debug",
    "bin",
    "obj",
    "node_modules",
    ".git",
    "__pycache__",
    "microsoft",
];

// ── Cache format (disk) ──────────────────────────────────────────────
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedEntry {
    pub file_ref: u64,
    pub parent_ref: u64,
    pub name: String,
    pub kind: FileKind,
}

#[derive(Serialize, Deserialize)]
pub struct CacheData {
    pub entries: Vec<CachedEntry>,
    pub drive_root: String,
    pub checkpoints: Vec<JournalCheckpoint>,
    pub junk_refs: Vec<u64>,
}

// ── Compact in-memory entry (32 bytes) ───────────────────────────────
#[derive(Clone)]
pub struct IndexEntry {
    pub file_ref: u64,
    pub parent_ref: u64,
    pub name_off: u32,
    pub name_lower_off: u32,
    pub name_len: u16,
    pub name_lower_len: u16,
    pub flags: u8, // bit 0 = is_dir
}

impl IndexEntry {
    #[inline]
    pub fn is_dir(&self) -> bool {
        self.flags & 1 != 0
    }

    #[inline]
    pub fn kind(&self) -> FileKind {
        if self.is_dir() { FileKind::Directory } else { FileKind::File }
    }
}

// ── Main index store ─────────────────────────────────────────────────
pub struct IndexStore {
    pub entries: Vec<IndexEntry>,
    pub name_arena: Vec<u8>,
    pub name_lower_arena: Vec<u8>,
    pub ref_lookup: Vec<(u64, u32)>, // sorted by file_ref for binary search
    pub drive_root: String,
    pub checkpoints: Vec<JournalCheckpoint>,
    /// file_refs of every record pruned from a junk subtree at scan time.
    /// Live journal events can re-enter those subtrees (new file under
    /// %TEMP%\x while the app runs); this set lets `is_live_junk` recognize
    /// them even though their parents were never indexed.
    pub junk_refs: std::collections::HashSet<u64>,
    /// Extension → entry indices (lowercase ext, e.g. "py"). Buckets keep
    /// name-sorted order because `entries` is name-sorted. Built lazily;
    /// any live mutation marks it dirty and it is rebuilt before the next
    /// extension search.
    pub ext_index: std::collections::HashMap<String, Vec<u32>>,
    pub ext_dirty: bool,
}

impl IndexStore {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            name_arena: Vec::new(),
            name_lower_arena: Vec::new(),
            ref_lookup: Vec::new(),
            drive_root: String::new(),
            checkpoints: Vec::new(),
            junk_refs: std::collections::HashSet::new(),
            ext_index: std::collections::HashMap::new(),
            ext_dirty: true,
        }
    }

    /// Rebuild every extension bucket in one pass. O(N); only runs at
    /// startup (or lazily after live mutations, once).
    pub fn rebuild_ext_index(&mut self) {
        self.ext_index.clear();
        // Parallel build: each core fills buckets for its own slice of the
        // entries, then we merge the chunk maps in index order.
        let threads = rayon::current_num_threads().max(2);
        let chunk_size = (self.entries.len() / (threads * 2)).max(64);
        let chunks: Vec<std::collections::HashMap<String, Vec<u32>>> = self
            .entries
            .par_chunks(chunk_size)
            .enumerate()
            .map(|(ci, chunk)| {
                let base = ci * chunk_size;
                let mut map: std::collections::HashMap<String, Vec<u32>> =
                    std::collections::HashMap::new();
                for (k, entry) in chunk.iter().enumerate() {
                    let i = base + k;
                    let name_lower = self.name_lower(entry);
                    // File extension = the suffix after the final dot (ignoring
                    // dotfiles like ".gitignore" and names ending in a dot).
                    match name_lower.rfind('.') {
                        Some(pos) if pos + 1 < name_lower.len() && pos == 0 => {}
                        Some(pos) if pos + 1 < name_lower.len() => {
                            map.entry(name_lower[pos + 1..].to_string())
                                .or_default()
                                .push(i as u32);
                        }
                        _ => {}
                    }
                }
                map
            })
            .collect();
        for chunk in chunks {
            for (ext, idxs) in chunk {
                self.ext_index.entry(ext).or_default().extend(idxs);
            }
        }
        self.ext_dirty = false;
    }

    // ── Arena accessors ──────────────────────────────────────────────

    #[inline]
    pub fn name(&self, e: &IndexEntry) -> &str {
        unsafe {
            std::str::from_utf8_unchecked(
                &self.name_arena[e.name_off as usize..(e.name_off as usize + e.name_len as usize)]
            )
        }
    }

    #[inline]
    pub fn name_lower(&self, e: &IndexEntry) -> &str {
        unsafe {
            std::str::from_utf8_unchecked(
                &self.name_lower_arena[e.name_lower_off as usize..(e.name_lower_off as usize + e.name_lower_len as usize)]
            )
        }
    }

    // ── Ref lookup (binary search) ───────────────────────────────────

    pub fn lookup_idx(&self, file_ref: u64) -> Option<u32> {
        self.ref_lookup
            .binary_search_by_key(&file_ref, |&(r, _)| r)
            .ok()
            .map(|pos| self.ref_lookup[pos].1)
    }

    fn rebuild_ref_lookup(&mut self) {
        self.ref_lookup.clear();
        self.ref_lookup.reserve(self.entries.len());
        for (i, e) in self.entries.iter().enumerate() {
            self.ref_lookup.push((e.file_ref, i as u32));
        }
        self.ref_lookup.par_sort_unstable_by_key(|&(r, _)| r);
    }

    // ── Populate from MFT scan ───────────────────────────────────────
}

// ── Parallel indexing chunks ─────────────────────────────────────────
/// One core's slice of the index build: local arenas + entries with
/// chunk-relative offsets. Chunks are merged into the store's arenas
/// sequentially — the merge is memcpy, everything expensive ran parallel.
struct BuiltChunk {
    names: Vec<u8>,
    lowers: Vec<u8>,
    entries: Vec<IndexEntry>,
}

fn build_chunk(chunk: &[CompactRecord], name_data: &[u16]) -> BuiltChunk {
    let mut names = Vec::with_capacity(chunk.len() * 24);
    let mut lowers = Vec::with_capacity(chunk.len() * 24);
    let mut entries = Vec::with_capacity(chunk.len());
    for r in chunk {
        let name_slice =
            &name_data[r.name_off as usize..(r.name_off as usize + r.name_len as usize)];
        let name = String::from_utf16_lossy(name_slice);
        let name_lower = name.to_lowercase();

        let n_off = names.len() as u32;
        let n_len = name.len() as u16;
        names.extend_from_slice(name.as_bytes());

        let nl_off = lowers.len() as u32;
        let nl_len = name_lower.len() as u16;
        lowers.extend_from_slice(name_lower.as_bytes());

        entries.push(IndexEntry {
            file_ref: r.file_ref,
            parent_ref: r.parent_ref,
            name_off: n_off,
            name_lower_off: nl_off,
            name_len: n_len,
            name_lower_len: nl_len,
            flags: if r.is_dir { 1 } else { 0 },
        });
    }
    BuiltChunk {
        names,
        lowers,
        entries,
    }
}

impl IndexStore {
    // ── Populate from MFT scan ───────────────────────────────────────

    pub fn populate_from_scan(&mut self, scan: ScanResult, drive_root: &str) {
        self.drive_root = drive_root.to_string();

        // Pass 1a: map every directory to (lowercased name, parent) so a cheap
        // ancestor walk can prune junk subtrees (node_modules, Windows, AppData,
        // …) BEFORE they enter the index. Skipping them at the source makes the
        // scan faster, the cache smaller and every later search cheaper.
        let mut dirs: std::collections::HashMap<u64, (String, u64)> =
            std::collections::HashMap::with_capacity(scan.records.len() / 5 + 16);
        for r in &scan.records {
            if r.is_dir {
                let name = String::from_utf16_lossy(
                    &scan.name_data[r.name_off as usize..(r.name_off as usize + r.name_len as usize)],
                );
                dirs.insert(r.file_ref, (name.to_lowercase(), r.parent_ref));
            }
        }

        fn junk_chain(parent_ref: u64, dirs: &std::collections::HashMap<u64, (String, u64)>) -> bool {
            let mut current = parent_ref;
            for _ in 0..32 {
                match dirs.get(&current) {
                    Some((name, parent)) => {
                        if JUNK_DIR_NAMES.iter().any(|j| *j == name) {
                            return true;
                        }
                        if *parent == current || *parent == 0 {
                            break;
                        }
                        current = *parent;
                    }
                    None => break,
                }
            }
            false
        }

        let name_data = &scan.name_data;

        // Pass 1b (PARALLEL): prune entire junk subtrees in one parallel sweep
        // across the MFT dump. Only clean records survive to the index.
        let clean: Vec<CompactRecord> = scan
            .records
            .par_iter()
            .filter(|r| !junk_chain(r.parent_ref, &dirs))
            .copied()
            .collect();

        // Remember everything the sweep dropped: live journal events (a file
        // created under a pruned %TEMP% subtree while we run) must still be
        // recognized and filtered, and their parents are gone from the index.
        self.junk_refs = scan
            .records
            .par_iter()
            .filter(|r| junk_chain(r.parent_ref, &dirs))
            .map(|r| r.file_ref)
            .collect();

        // Pass 2 (PARALLEL): decode + lowercase UTF-16 names in per-core
        // chunks, each building its own little arena. Merging afterwards is
        // plain memcpy — every CPU-bound byte of the indexing phase runs on
        // all cores instead of one.
        let threads = rayon::current_num_threads().max(2);
        let chunk_size = (clean.len() / (threads * 2)).max(256);
        let chunks: Vec<BuiltChunk> = clean
            .par_chunks(chunk_size)
            .map(|chunk| build_chunk(chunk, name_data))
            .collect();

        // Merge: shift chunk-local offsets, one big copy per arena.
        let total_names: usize = chunks.iter().map(|c| c.names.len()).sum();
        let total_lowers: usize = chunks.iter().map(|c| c.lowers.len()).sum();
        self.entries.reserve(clean.len());
        self.name_arena.reserve(total_names);
        self.name_lower_arena.reserve(total_lowers);
        for mut c in chunks {
            let base = self.name_arena.len() as u32;
            let base_lower = self.name_lower_arena.len() as u32;
            for e in &mut c.entries {
                e.name_off += base;
                e.name_lower_off += base_lower;
            }
            self.name_arena.extend_from_slice(&c.names);
            self.name_lower_arena.extend_from_slice(&c.lowers);
            self.entries.extend(c.entries);
        }
        self.ext_dirty = true;
    }

    pub fn finalize(&mut self) {
        let store_addr = self as *const IndexStore as usize;
        self.entries.par_sort_unstable_by(|a, b| {
            let s = unsafe { &*(store_addr as *const IndexStore) };
            s.name_lower(a).cmp(s.name_lower(b))
        });
        self.rebuild_ref_lookup();
        self.rebuild_ext_index();
        // Shrink arenas to fit
        self.name_arena.shrink_to_fit();
        self.name_lower_arena.shrink_to_fit();
    }

    // ── Cache serialization ──────────────────────────────────────────

    pub fn to_cache(&self) -> CacheData {
        CacheData {
            entries: self
                .entries
                .par_iter()
                .map(|e| CachedEntry {
                    file_ref: e.file_ref,
                    parent_ref: e.parent_ref,
                    name: self.name(e).to_string(),
                    kind: e.kind(),
                })
                .collect(),
            drive_root: self.drive_root.clone(),
            checkpoints: self.checkpoints.clone(),
            junk_refs: self.junk_refs.iter().copied().collect(),
        }
    }

    pub fn from_cache(cache: CacheData) -> Self {
        let count = cache.entries.len();
        let mut store = Self {
            entries: Vec::with_capacity(count),
            name_arena: Vec::with_capacity(count * 30),
            name_lower_arena: Vec::with_capacity(count * 30),
            ref_lookup: Vec::with_capacity(count),
            drive_root: cache.drive_root,
            checkpoints: cache.checkpoints,
            junk_refs: cache.junk_refs.into_iter().collect(),
            ext_index: std::collections::HashMap::new(),
            ext_dirty: true,
        };

        // The cache load is the hot path of every launch — decode the
        // cached names on all cores, then merge the chunks (memcpy only).
        let threads = rayon::current_num_threads().max(2);
        let chunk_size = (count / (threads * 2)).max(128);
        let chunks: Vec<BuiltChunk> = cache
            .entries
            .par_chunks(chunk_size)
            .map(|chunk| {
                let mut names = Vec::with_capacity(chunk.len() * 24);
                let mut lowers = Vec::with_capacity(chunk.len() * 24);
                let mut entries = Vec::with_capacity(chunk.len());
                for c in chunk {
                    let name_lower = c.name.to_lowercase();

                    let n_off = names.len() as u32;
                    let n_len = c.name.len() as u16;
                    names.extend_from_slice(c.name.as_bytes());

                    let nl_off = lowers.len() as u32;
                    let nl_len = name_lower.len() as u16;
                    lowers.extend_from_slice(name_lower.as_bytes());

                    let flags = match c.kind {
                        FileKind::Directory => 1u8,
                        FileKind::File => 0u8,
                    };
                    entries.push(IndexEntry {
                        file_ref: c.file_ref,
                        parent_ref: c.parent_ref,
                        name_off: n_off,
                        name_lower_off: nl_off,
                        name_len: n_len,
                        name_lower_len: nl_len,
                        flags,
                    });
                }
                BuiltChunk {
                    names,
                    lowers,
                    entries,
                }
            })
            .collect();

        for mut c in chunks {
            let base = store.name_arena.len() as u32;
            let base_lower = store.name_lower_arena.len() as u32;
            for e in &mut c.entries {
                e.name_off += base;
                e.name_lower_off += base_lower;
            }
            store.name_arena.extend_from_slice(&c.names);
            store.name_lower_arena.extend_from_slice(&c.lowers);
            store.entries.extend(c.entries);
        }

        store.rebuild_ref_lookup();
        store.name_arena.shrink_to_fit();
        store.name_lower_arena.shrink_to_fit();
        store.rebuild_ext_index();
        store
    }

    // ── Live mutations ───────────────────────────────────────────────

    /// Append a record to the arenas and build its compact entry.
    fn arena_entry(&mut self, record: &FileRecord) -> IndexEntry {
        let name_lower = record.name.to_lowercase();

        let n_off = self.name_arena.len() as u32;
        let n_len = record.name.len() as u16;
        self.name_arena.extend_from_slice(record.name.as_bytes());

        let nl_off = self.name_lower_arena.len() as u32;
        let nl_len = name_lower.len() as u16;
        self.name_lower_arena.extend_from_slice(name_lower.as_bytes());

        let flags = match record.kind {
            FileKind::Directory => 1u8,
            FileKind::File => 0u8,
        };

        IndexEntry {
            file_ref: record.file_ref,
            parent_ref: record.parent_ref,
            name_off: n_off,
            name_lower_off: nl_off,
            name_len: n_len,
            name_lower_len: nl_len,
            flags,
        }
    }

    /// True when walking the parent chain from `parent_ref` touches a junk
    /// directory name or a ref that the scan-time sweep pruned. Guards live
    /// journal inserts so junk never re-enters the index mid-session.
    pub fn is_live_junk(&self, parent_ref: u64) -> bool {
        let mut current = parent_ref;
        for _ in 0..32 {
            if self.junk_refs.contains(&current) {
                return true;
            }
            let Some(idx) = self.lookup_idx(current) else {
                return false; // parent unknown — not provably junk, keep it
            };
            let e = &self.entries[idx as usize];
            if JUNK_DIR_NAMES
                .iter()
                .any(|j| *j == self.name_lower(e))
            {
                return true;
            }
            let next = e.parent_ref;
            if next == current || next == 0 {
                return false;
            }
            current = next;
        }
        false
    }

    pub fn insert(&mut self, record: FileRecord) {
        // Idempotent: a record supersedes any existing entry with the same
        // file_ref (journals can re-deliver a create/rename across restarts).
        self.entries.retain(|e| e.file_ref != record.file_ref);

        // Live junk guard: a journal event inside a pruned subtree is
        // dropped here, exactly like the scan-time prefilter never let it in.
        if self.is_live_junk(record.parent_ref) {
            self.junk_refs.insert(record.file_ref);
            return;
        }

        let name_lower = record.name.to_lowercase();
        let store_ptr = self as *const IndexStore;
        let pos = self.entries.partition_point(|e| {
            let s = unsafe { &*store_ptr };
            s.name_lower(e) < name_lower.as_str()
        });
        let entry = self.arena_entry(&record);
        self.entries.insert(pos, entry);
        self.rebuild_ref_lookup();
        self.ext_dirty = true;
    }

    pub fn remove(&mut self, file_ref: u64) {
        // Name bytes left as dead space in arena (negligible for rare deletes)
        self.entries.retain(|e| e.file_ref != file_ref);
        self.rebuild_ref_lookup();
        self.ext_dirty = true;
    }

    pub fn rename(&mut self, old_ref: u64, new_record: FileRecord) {
        self.remove(old_ref);
        self.insert(new_record);
    }

    pub fn apply_move(&mut self, file_ref: u64, new_parent_ref: u64, name: String, kind: FileKind) {
        self.remove(file_ref);
        self.insert(FileRecord { file_ref, parent_ref: new_parent_ref, name, kind });
    }

    /// Apply a batch of journal events under a single lock acquisition:
    /// all mutations run first, then sorted-name order and ref_lookup are
    /// restored once (instead of once per event).
    pub fn apply_events(&mut self, events: Vec<IndexEvent>) {
        if events.is_empty() {
            return;
        }

        let mut pending: Vec<FileRecord> = Vec::with_capacity(events.len());
        let mut removed = false;

        for event in events {
            match event {
                IndexEvent::Created(r) => {
                    if !self.is_live_junk(r.parent_ref) {
                        pending.push(r);
                    } else {
                        self.junk_refs.insert(r.file_ref);
                    }
                }
                IndexEvent::Deleted(id) => {
                    let before = self.entries.len();
                    self.entries.retain(|e| e.file_ref != id);
                    self.junk_refs.remove(&id);
                    removed |= self.entries.len() != before;
                }
                IndexEvent::Renamed { old_ref, new_record } => {
                    self.entries.retain(|e| e.file_ref != old_ref);
                    self.junk_refs.remove(&old_ref);
                    if !self.is_live_junk(new_record.parent_ref) {
                        pending.push(new_record);
                    } else {
                        self.junk_refs.insert(new_record.file_ref);
                    }
                }
                IndexEvent::Moved { file_ref, new_parent_ref, name, kind } => {
                    self.entries.retain(|e| e.file_ref != file_ref);
                    let rec = FileRecord { file_ref, parent_ref: new_parent_ref, name, kind };
                    if !self.is_live_junk(rec.parent_ref) {
                        pending.push(rec);
                    } else {
                        self.junk_refs.insert(rec.file_ref);
                    }
                }
                IndexEvent::Checkpoint(_) => {}
            }
        }

        if pending.is_empty() {
            if removed {
                self.rebuild_ref_lookup();
            }
            return;
        }

        // Idempotency: every record in this batch supersedes any pre-existing
        // entry with the same file_ref. This makes re-applying journal events
        // (e.g. after a stale checkpoint) safe instead of duplicating entries.
        let pending_refs: std::collections::HashSet<u64> =
            pending.iter().map(|r| r.file_ref).collect();
        self.entries.retain(|e| !pending_refs.contains(&e.file_ref));

        self.entries.reserve(pending.len());
        for record in &pending {
            let entry = self.arena_entry(record);
            self.entries.push(entry);
        }
        let store_ptr = self as *const IndexStore;
        self.entries.sort_unstable_by(|a, b| {
            let s = unsafe { &*store_ptr };
            s.name_lower(a).cmp(s.name_lower(b))
        });
        self.rebuild_ref_lookup();
        self.ext_dirty = true;
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan_result(records: &[(u64, u64, &str, bool)]) -> ScanResult {
        let mut name_data = Vec::new();
        let mut recs = Vec::new();
        for &(file_ref, parent_ref, name, is_dir) in records {
            let off = name_data.len() as u32;
            let encoded: Vec<u16> = name.encode_utf16().collect();
            name_data.extend_from_slice(&encoded);
            recs.push(CompactRecord {
                file_ref,
                parent_ref,
                name_off: off,
                name_len: encoded.len() as u16,
                is_dir,
            });
        }
        ScanResult {
            records: recs,
            name_data,
        }
    }

    fn names(store: &IndexStore) -> Vec<String> {
        let mut v: Vec<String> = store.entries.iter().map(|e| store.name(e).to_string()).collect();
        v.sort_unstable();
        v
    }

    #[test]
    fn parallel_populate_drops_junk_and_keeps_chains() {
        let mut store = IndexStore::new();
        store.populate_from_scan(
            scan_result(&[
                (1, 0, "Users", true),
                (10, 1, "Alice", true),
                (11, 10, "report.txt", false),
                (12, 10, "photo.PNG", false),
                (13, 0, "node_modules", true),
                (14, 13, "lodash.js", false),
                (15, 0, "Temp", true),
                (16, 15, "cache.tmp", false),
            ]),
            "C:\\",
        );
        store.finalize();

        // 6 survive: Users, Alice, report.txt, photo.PNG, node_modules, Temp
        assert_eq!(
            names(&store),
            vec!["Alice", "Temp", "Users", "node_modules", "photo.PNG", "report.txt"]
        );
        // parent chains survived the chunked merge
        let report = store
            .entries
            .iter()
            .find(|e| store.name(e) == "report.txt")
            .unwrap();
        let pidx = store.lookup_idx(report.parent_ref).unwrap();
        assert_eq!(store.name(&store.entries[pidx as usize]), "Alice");

        // Pruned subtree members are remembered (live journal re-entries are
        // filtered through junk_refs), but surviving junk ROOT dirs are not.
        assert!(store.junk_refs.contains(&14)); // lodash.js under node_modules
        assert!(store.junk_refs.contains(&16)); // cache.tmp under Temp
        assert!(!store.junk_refs.contains(&13)); // node_modules itself kept
    }

    #[test]
    fn cache_roundtrip_parallel_load() {
        let mut store = IndexStore::new();
        store.populate_from_scan(
            scan_result(&[
                (1, 0, "Alpha.TXT", false),
                (2, 1, "beta.md", false),
                (3, 0, "Dir", true),
            ]),
            "C:\\",
        );
        store.finalize();

        let cache = store.to_cache();
        let loaded = IndexStore::from_cache(cache);
        assert_eq!(loaded.entries.len(), 3);
        assert_eq!(loaded.junk_refs, store.junk_refs);
        assert_eq!(
            names(&loaded),
            vec!["Alpha.TXT", "Dir", "beta.md"]
        );
        let beta = loaded
            .entries
            .iter()
            .find(|e| loaded.name(e) == "beta.md")
            .unwrap();
        let pidx = loaded.lookup_idx(beta.parent_ref).unwrap();
        assert_eq!(loaded.name(&loaded.entries[pidx as usize]), "Alpha.TXT");
    }
}