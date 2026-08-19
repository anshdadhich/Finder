#![allow(dead_code)]
use rayon::prelude::*;
use serde::{Serialize, Deserialize};
use bincode::Options as BincodeOptions;
use crate::mft::types::{FileKind, FileRecord, IndexEvent, JournalCheckpoint, NtfsDrive};
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

/// Directory names (lowercased) that mark a path as living under the user's
/// profile ("C:\Users\me\Documents\…"). Stored per entry at insert time so
/// ranking never has to rebuild a path just to sort user-dir results higher.
pub const USER_DIR_NAMES: &[&str] = &[
    "users", "documents", "downloads", "desktop", "pictures", "videos", "music",
];

// ── Cache format (disk) ──────────────────────────────────────────────
/// Cache format version. Bumped on any on-disk layout change; readers
/// reject caches whose magic/version differ and fall back to a full scan.
pub const CACHE_MAGIC: [u8; 4] = *b"FSKC";
pub const CACHE_FORMAT_VERSION: u32 = 3;

/// Hard cap on how many bytes a cache may decode to before it is rejected as
/// tampered/oversized. Real caches scale with file count: a 3 M-file index
/// decodes to ~240 MB (20 MB zstd on disk). 1.5 GB keeps several-fold
/// headroom for very large disks without reopening unbounded-allocation risk.
/// Applied as a `Take` on every decode path (GUI + CLI, zstd/lz4/block) plus
/// bincode's own `with_limit`, so a hostile `finder_cache.bin` can never make
/// the always-elevated process allocate unbounded memory and abort before the
/// structural checks run.
pub const MAX_CACHE_DECODED: u64 = 1536 * 1024 * 1024;

/// A volume's root path, as indexed. Entries carry a `drive` index into
/// this list, so file_refs (per-volume MFT record numbers) never collide
/// and paths are always built against the right volume root.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriveRoot {
    pub letter: char,
    pub root: String,
}

/// Cache v2: the raw in-memory index, persisted verbatim. No per-entry
/// String materialization on save and no UTF-16 decode + re-lowercase on
/// load — the arenas travel as-is and only the derived lookup structures
/// (ref_lookup, ext_index) are rebuilt after deserialization.
#[derive(Serialize, Deserialize)]
pub struct CacheData {
    pub magic: [u8; 4],
    pub version: u32,
    /// Raw entries with absolute offsets into `name_arena` /
    /// `name_lower_arena` — valid as-is because the arenas are stored
    /// byte-for-byte.
    pub entries: Vec<IndexEntry>,
    pub name_arena: Vec<u8>,
    pub name_lower_arena: Vec<u8>,
    /// Per-drive roots; `IndexEntry.drive` indexes into this list.
    pub drive_roots: Vec<DriveRoot>,
    pub checkpoints: Vec<JournalCheckpoint>,
    /// file_refs of every record pruned from a junk subtree at scan time,
    /// grouped per drive (same drive semantics as the entries).
    pub junk_refs: Vec<Vec<u64>>,
}

// ── Compact in-memory entry (32 bytes) ───────────────────────────────
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexEntry {
    pub file_ref: u64,
    pub parent_ref: u64,
    pub name_off: u32,
    pub name_lower_off: u32,
    pub name_len: u16,
    pub name_lower_len: u16,
    pub flags: u8, // bit 0 = is_dir
    /// Path depth below the volume root (capped at 15) — computed once at
    /// insert time so ranking never rebuilds the parent chain per candidate.
    pub depth: u8,
    /// 1 when the entry lives under a `USER_DIR_NAMES` ancestor (or is itself
    /// one of those directories) — the stored form of the old per-query
    /// `path_lower.contains("\\users\\")`-style check.
    pub user_path: u8,
    /// Volume this entry belongs to; index into `IndexStore.drive_roots`.
    pub drive: u8,
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
    /// Per-drive ref → entry-index tables, each sorted by file_ref for
    /// binary search. Separate buckets because MFT record numbers are
    /// per-volume and collide across drives.
    pub ref_lookup: Vec<Vec<(u64, u32)>>,
    /// Volume roots, ordered; `IndexEntry.drive` indexes here.
    pub drive_roots: Vec<DriveRoot>,
    pub checkpoints: Vec<JournalCheckpoint>,
    /// Per-drive file_refs of every record pruned from a junk subtree at
    /// scan time. Live journal events can re-enter those subtrees (new file
    /// under %TEMP%\x while the app runs); this set lets `is_live_junk`
    /// recognize them even though their parents were never indexed.
    pub junk_refs: Vec<std::collections::HashSet<u64>>,
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
            drive_roots: Vec::new(),
            checkpoints: Vec::new(),
            junk_refs: Vec::new(),
            ext_index: std::collections::HashMap::new(),
            ext_dirty: true,
        }
    }

    /// Index of the drive with `letter`, registering it (root filled in
    /// later by populate_from_scan when the scan succeeds) on first sight.
    /// Events can arrive for a volume whose scan failed; the fallback root
    /// still yields a usable (letter-qualified) path for those entries.
    fn drive_index(&mut self, letter: char) -> u8 {
        if let Some(i) = self.drive_roots.iter().position(|d| d.letter == letter) {
            return i as u8;
        }
        self.drive_roots.push(DriveRoot {
            letter,
            root: format!("{}:\\", letter),
        });
        (self.drive_roots.len() - 1) as u8
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
                    // dotfiles like ".gitignore" and names ending in a dot —
                    // EXTRA: dot-named DIRECTORIES (.config, .ssh, .cache) DO
                    // get bucketed under the post-dot name, so `.config` finds
                    // the folder, not just app.config-style files).
                    match name_lower.rfind('.') {
                        Some(pos) if pos + 1 < name_lower.len() && pos == 0 && entry.is_dir() => {
                            map.entry(name_lower[1..].to_string())
                                .or_default()
                                .push(i as u32);
                        }
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

    // ── Ref lookup (per-drive binary search) ─────────────────────────

    pub fn lookup_idx(&self, drive: u8, file_ref: u64) -> Option<u32> {
        let bucket = self.ref_lookup.get(drive as usize)?;
        bucket
            .binary_search_by_key(&file_ref, |&(r, _)| r)
            .ok()
            .map(|pos| bucket[pos].1)
    }

    fn rebuild_ref_lookup(&mut self) {
        let mut buckets: Vec<Vec<(u64, u32)>> = Vec::with_capacity(self.drive_roots.len().max(1));
        for (i, e) in self.entries.iter().enumerate() {
            let d = e.drive as usize;
            while buckets.len() <= d {
                buckets.push(Vec::new());
            }
            buckets[d].push((e.file_ref, i as u32));
        }
        for b in &mut buckets {
            b.par_sort_unstable_by_key(|&(r, _)| r);
        }
        self.ref_lookup = buckets;
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

/// Scan-time path metadata for one record: walks the ancestor chain through
/// the lowercased `dirs` map (the same map junk pruning walks) and returns
/// `(depth, user_path)` — or `None` when any ancestor is a junk directory, in
/// which case the record must not enter the index. Only real components below
/// the volume root count toward depth; the root directory itself
/// (parent == self or parent == 0) contributes no separator.
fn path_meta_from_dirs(
    parent_ref: u64,
    dirs: &std::collections::HashMap<u64, (String, u64)>,
) -> Option<(u8, u8)> {
    let mut depth: u8 = 0;
    let mut user: u8 = 0;
    let mut current = parent_ref;
    for _ in 0..32 {
        match dirs.get(&current) {
            Some((name, parent)) => {
                if JUNK_DIR_NAMES.iter().any(|j| *j == name) {
                    return None;
                }
                if USER_DIR_NAMES.iter().any(|m| *m == name) {
                    user = 1;
                }
                if *parent == current || *parent == 0 {
                    break; // root directory — contributes no separator
                }
                depth = depth.saturating_add(1);
                current = *parent;
            }
            None => break,
        }
    }
    Some(((depth + 1).min(15), user))
}

fn build_chunk(chunk: &[(CompactRecord, u8, u8)], name_data: &[u16], drive: u8) -> BuiltChunk {
    let mut names = Vec::with_capacity(chunk.len() * 24);
    let mut lowers = Vec::with_capacity(chunk.len() * 24);
    let mut entries = Vec::with_capacity(chunk.len());
    for (r, depth, user_from_ancestors) in chunk {
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

        // A directory whose own lowercased name is a user marker counts as a
        // user path too ("C:\foo\desktop\"), matching the old contains-check
        // semantics where the trailing slash of a directory name matched.
        let user = if r.is_dir && USER_DIR_NAMES.iter().any(|m| *m == name_lower) {
            (*user_from_ancestors) | 1
        } else {
            *user_from_ancestors
        };

        entries.push(IndexEntry {
            file_ref: r.file_ref,
            parent_ref: r.parent_ref,
            name_off: n_off,
            name_lower_off: nl_off,
            name_len: n_len,
            name_lower_len: nl_len,
            flags: if r.is_dir { 1 } else { 0 },
            depth: *depth,
            user_path: user,
            drive,
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

    pub fn populate_from_scan(&mut self, scan: ScanResult, drive: &NtfsDrive) {
        // Register the volume and remember its authoritative root.
        let drive_idx = self.drive_index(drive.letter);
        self.drive_roots[drive_idx as usize].root = drive.root.clone();

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
        // across the MFT dump, stamping each surviving record's depth and
        // user-folder metadata in the same walk. Only clean records survive
        // to the index.
        let clean: Vec<(CompactRecord, u8, u8)> = scan
            .records
            .par_iter()
            .filter_map(|r| path_meta_from_dirs(r.parent_ref, &dirs).map(|(d, u)| (*r, d, u)))
            .collect();

        // Remember everything the sweep dropped: live journal events (a file
        // created under a pruned %TEMP% subtree while we run) must still be
        // recognized and filtered, and their parents are gone from the index.
        // Kept per drive — file_refs collide across volumes.
        let pruned: std::collections::HashSet<u64> = scan
            .records
            .par_iter()
            .filter(|r| junk_chain(r.parent_ref, &dirs))
            .map(|r| r.file_ref)
            .collect();
        if self.junk_refs.len() <= drive_idx as usize {
            self.junk_refs
                .resize(drive_idx as usize + 1, std::collections::HashSet::new());
        }
        self.junk_refs[drive_idx as usize].extend(pruned);

        // Pass 2 (PARALLEL): decode + lowercase UTF-16 names in per-core
        // chunks, each building its own little arena. Merging afterwards is
        // plain memcpy — every CPU-bound byte of the indexing phase runs on
        // all cores instead of one.
        let threads = rayon::current_num_threads().max(2);
        let chunk_size = (clean.len() / (threads * 2)).max(256);
        let chunks: Vec<BuiltChunk> = clean
            .par_chunks(chunk_size)
            .map(|chunk| build_chunk(chunk, name_data, drive_idx))
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

    /// Cache v2 save: the in-memory arenas and entries travel verbatim
    /// (plain memcpy clones) — no per-file String materialization, so this
    /// is a large-but-fast blob instead of millions of allocations.
    pub fn to_cache(&self) -> CacheData {
        CacheData {
            magic: CACHE_MAGIC,
            version: CACHE_FORMAT_VERSION,
            entries: self.entries.clone(),
            name_arena: self.name_arena.clone(),
            name_lower_arena: self.name_lower_arena.clone(),
            drive_roots: self.drive_roots.clone(),
            checkpoints: self.checkpoints.clone(),
            junk_refs: self
                .junk_refs
                .iter()
                .map(|s| s.iter().copied().collect())
                .collect(),
        }
    }

    /// Cache v2 load. The arenas are already in final form, so there is no
    /// UTF-16 decode and no re-lowercasing — only the derived structures
    /// (ref_lookup, ext_index) get rebuilt. Returns None for anything that
    /// is not a valid v2 cache (wrong magic/version, out-of-bounds entry,
    /// empty roots); callers treat that as "corrupt" and rescan.
    pub fn from_cache(cache: CacheData) -> Option<Self> {
        if cache.magic != CACHE_MAGIC || cache.version != CACHE_FORMAT_VERSION {
            return None;
        }
        if cache.drive_roots.is_empty() {
            return None;
        }
        // Every entry must point inside its arenas and at a known drive —
        // guards against truncated/tampered files (and any v1 remnant that
        // happened to survive the magic check).
        let valid = cache.entries.par_iter().all(|e| {
            (e.drive as usize) < cache.drive_roots.len()
                && (e.name_off as usize + e.name_len as usize) <= cache.name_arena.len()
                && (e.name_lower_off as usize + e.name_lower_len as usize)
                    <= cache.name_lower_arena.len()
        });
        if !valid {
            return None;
        }
        // The arena bytes must be valid UTF-8: `name()`/`name_lower()` slice
        // them with `from_utf8_unchecked` (deliberately zero-copy), so a
        // tampered cache must never reach those unsafes with bad bytes.
        if std::str::from_utf8(&cache.name_arena).is_err()
            || std::str::from_utf8(&cache.name_lower_arena).is_err()
        {
            return None;
        }

        let mut store = Self {
            entries: cache.entries,
            name_arena: cache.name_arena,
            name_lower_arena: cache.name_lower_arena,
            ref_lookup: Vec::new(),
            drive_roots: cache.drive_roots,
            checkpoints: cache.checkpoints,
            junk_refs: cache
                .junk_refs
                .into_iter()
                .map(|v| v.into_iter().collect())
                .collect(),
            ext_index: std::collections::HashMap::new(),
            ext_dirty: true,
        };
        store.rebuild_ref_lookup();
        // ext_index is derived and lazily repaired under `ext_dirty` on the
        // first search command — skip the eager O(N) build so cache load
        // stays lean (the CLI's ext filter does its own suffix pass and
        // never touches ext_index).
        Some(store)
    }

    // ── Live mutations ───────────────────────────────────────────────

    /// Append a record to the arenas and build its compact entry. `depth` /
    /// `user_path` come from `path_meta`/`path_meta_from_dirs` computed by the
    /// caller.
    fn arena_entry(&mut self, record: &FileRecord, drive: u8, depth: u8, user_path: u8) -> IndexEntry {
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
            depth,
            user_path,
            drive,
        }
    }

    /// Per-drive junk set, growing the vec on first use for a drive.
    fn junk_refs_for(&mut self, drive: u8) -> &mut std::collections::HashSet<u64> {
        let i = drive as usize;
        if self.junk_refs.len() <= i {
            self.junk_refs
                .resize(i + 1, std::collections::HashSet::new());
        }
        &mut self.junk_refs[i]
    }

    /// True when walking the parent chain from `parent_ref` touches a junk
    /// directory name or a ref that the scan-time sweep pruned. Guards live
    /// journal inserts so junk never re-enters the index mid-session. The
    /// walk stays inside `drive`'s ref space — never crosses volumes.
    pub fn is_live_junk(&self, parent_ref: u64, drive: u8) -> bool {
        let mut current = parent_ref;
        for _ in 0..32 {
            if self
                .junk_refs
                .get(drive as usize)
                .map_or(false, |s| s.contains(&current))
            {
                return true;
            }
            let Some(idx) = self.lookup_idx(drive, current) else {
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

    /// Path depth + user-folder bit for a record, derived once at insert time
    /// by walking the parent chain through the store's ref lookup (same rules
    /// as `path_meta_from_dirs`). The root directory (parent == self or 0)
    /// contributes no depth; ancestors not yet indexed simply stop the count,
    /// matching the approximation the old per-query walk produced.
    pub fn path_meta(&self, drive: u8, parent_ref: u64, own_name_lower: &str, own_is_dir: bool) -> (u8, u8) {
        let mut depth: u8 = 0;
        let mut user: u8 = 0;
        let mut current = parent_ref;
        for _ in 0..32 {
            let Some(idx) = self.lookup_idx(drive, current) else {
                break;
            };
            let e = &self.entries[idx as usize];
            if USER_DIR_NAMES.iter().any(|m| *m == self.name_lower(e)) {
                user = 1;
            }
            if e.parent_ref == current || e.parent_ref == 0 {
                break; // root directory — contributes no separator
            }
            depth = depth.saturating_add(1);
            current = e.parent_ref;
        }
        if own_is_dir && USER_DIR_NAMES.iter().any(|m| *m == own_name_lower) {
            user = 1;
        }
        ((depth + 1).min(15), user)
    }

    pub fn insert(&mut self, drive: u8, record: FileRecord) {
        // Idempotent: a record supersedes any existing entry with the same
        // file_ref on the same drive (journals can re-deliver a create/rename
        // across restarts).
        self.entries
            .retain(|e| !(e.drive == drive && e.file_ref == record.file_ref));

        // Live junk guard: a journal event inside a pruned subtree is
        // dropped here, exactly like the scan-time prefilter never let it in.
        if self.is_live_junk(record.parent_ref, drive) {
            self.junk_refs_for(drive).insert(record.file_ref);
            return;
        }

        let name_lower = record.name.to_lowercase();
        let (depth, user_path) = self.path_meta(
            drive,
            record.parent_ref,
            &name_lower,
            matches!(record.kind, FileKind::Directory),
        );
        let store_ptr = self as *const IndexStore;
        let pos = self.entries.partition_point(|e| {
            let s = unsafe { &*store_ptr };
            s.name_lower(e) < name_lower.as_str()
        });
        let entry = self.arena_entry(&record, drive, depth, user_path);
        self.entries.insert(pos, entry);
        self.rebuild_ref_lookup();
        self.ext_dirty = true;
    }

    pub fn remove(&mut self, drive: u8, file_ref: u64) {
        // Name bytes left as dead space in arena (negligible for rare deletes)
        self.entries
            .retain(|e| !(e.drive == drive && e.file_ref == file_ref));
        self.junk_refs_for(drive).remove(&file_ref);
        self.rebuild_ref_lookup();
        self.ext_dirty = true;
    }

    pub fn rename(&mut self, drive: u8, old_ref: u64, new_record: FileRecord) {
        self.remove(drive, old_ref);
        self.insert(drive, new_record);
    }

    pub fn apply_move(&mut self, drive: u8, file_ref: u64, new_parent_ref: u64, name: String, kind: FileKind) {
        self.remove(drive, file_ref);
        self.insert(drive, FileRecord { file_ref, parent_ref: new_parent_ref, name, kind });
    }

    /// Apply a batch of journal events under a single lock acquisition:
    /// all mutations run first, then sorted-name order and ref_lookup are
    /// restored once (instead of once per event). Every data event carries
    /// its drive letter; all removes/inserts stay in that drive's space.
    ///
    /// W4 splicing: the batch's winning records are materialized into the
    /// arenas, name-sorted (small), and merged into the name-sorted
    /// `entries` with ONE linear pass — no whole-index re-sort. ref_lookup
    /// is delta-merged instead of rebuilt: stale refs drop per bucket,
    /// surviving indices shift by the count of spliced entries that landed
    /// at or before them, and new (ref, final index) pairs are inserted in
    /// ref order. A churn burst therefore costs ~O(N + k·log k) instead of
    /// O(N log N) per flush (k = batch size, N = index size).
    pub fn apply_events(&mut self, events: Vec<IndexEvent>) {
        if events.is_empty() {
            return;
        }

        // Pass 1: classify every event. `removes` collects every ref that
        // must vanish from the index (deletes, rename sources, moved refs);
        // `newest` keeps the winning record per ref after intra-batch
        // collapse — CREATE + RENAME for one file ref burst into a single
        // batch, and without the dedup both would be inserted.
        let mut removes: std::collections::HashSet<(u8, u64)> =
            std::collections::HashSet::new();
        let mut newest: Vec<(u8, FileRecord)> = Vec::with_capacity(events.len());
        let mut newest_seen: std::collections::HashMap<(u8, u64), usize> =
            std::collections::HashMap::new();
        let mut push_newest = |d: u8, record: FileRecord| {
            if let Some(&slot) = newest_seen.get(&(d, record.file_ref)) {
                newest[slot] = (d, record);
            } else {
                newest_seen.insert((d, record.file_ref), newest.len());
                newest.push((d, record));
            }
        };

        for event in events {
            match event {
                IndexEvent::Created { drive_letter, record } => {
                    let d = self.drive_index(drive_letter);
                    if !self.is_live_junk(record.parent_ref, d) {
                        push_newest(d, record);
                    } else {
                        self.junk_refs_for(d).insert(record.file_ref);
                    }
                }
                IndexEvent::Deleted { drive_letter, file_ref } => {
                    let d = self.drive_index(drive_letter);
                    removes.insert((d, file_ref));
                    self.junk_refs_for(d).remove(&file_ref);
                }
                IndexEvent::Renamed { drive_letter, old_ref, new_record } => {
                    let d = self.drive_index(drive_letter);
                    removes.insert((d, old_ref));
                    self.junk_refs_for(d).remove(&old_ref);
                    if !self.is_live_junk(new_record.parent_ref, d) {
                        push_newest(d, new_record);
                    } else {
                        self.junk_refs_for(d).insert(new_record.file_ref);
                    }
                }
                IndexEvent::Moved { drive_letter, file_ref, new_parent_ref, name, kind } => {
                    let d = self.drive_index(drive_letter);
                    removes.insert((d, file_ref));
                    let rec = FileRecord { file_ref, parent_ref: new_parent_ref, name, kind };
                    if !self.is_live_junk(rec.parent_ref, d) {
                        push_newest(d, rec);
                    } else {
                        self.junk_refs_for(d).insert(rec.file_ref);
                    }
                }
                IndexEvent::Checkpoint(_) => {}
            }
        }

        // Pass 2: one retain covers every ref to drop — deletes AND replaced
        // records (journal replay can re-deliver a create for a ref that
        // already exists; the old entry must not survive beside the
        // re-inserted one). Surviving entries keep name-sorted order.
        if !newest.is_empty() {
            removes.extend(newest.iter().map(|(d, r)| (*d, r.file_ref)));
        }
        let before = self.entries.len();
        let mut removed_any = false;
        if !removes.is_empty() {
            self.entries.retain(|e| !removes.contains(&(e.drive, e.file_ref)));
            removed_any = self.entries.len() != before;
        }

        if newest.is_empty() {
            // Pure deletes / junk drops: order is untouched, only the ref
            // buckets need refreshing (and only if something vanished). The
            // extension buckets would dangle past the trimmed entries — mark
            // ext_index dirty so the next ext search rebuilds it instead of
            // indexing out of bounds.
            if removed_any {
                self.rebuild_ref_lookup();
                self.ext_dirty = true;
            }
            return;
        }

        // Pass 3: materialize the new records into the arenas. depth/user
        // stay placeholders until the whole batch is visible to the ancestor
        // walk (a batch can create a directory and its children in one
        // drain). The small set is then name-sorted for the merge.
        let mut new_entries: Vec<IndexEntry> = newest
            .iter()
            .map(|(d, record)| self.arena_entry(record, *d, 0, 0))
            .collect();
        new_entries.sort_unstable_by(|a, b| self.name_lower(a).cmp(self.name_lower(b)));

        // Pass 4: one linear merge restores name-sorted order (splice, not
        // re-sort). For every spliced record we keep its insertion slot (the
        // old-coordinate cursor value when it was emitted — every surviving
        // entry at/after it shifts by one) and its final merged index.
        // Ties (duplicate names across directories, e.g. "report.txt") go
        // old-first, which the slot bookkeeping below matches exactly.
        let old_len = self.entries.len();
        let mut merged: Vec<IndexEntry> = Vec::with_capacity(old_len + new_entries.len());
        let mut spliced: Vec<(u8, u64, usize, usize)> = Vec::with_capacity(new_entries.len());
        {
            let mut i = 0;
            let mut j = 0;
            while i < old_len && j < new_entries.len() {
                if self.name_lower(&self.entries[i]) <= self.name_lower(&new_entries[j]) {
                    merged.push(self.entries[i].clone());
                    i += 1;
                } else {
                    let (slot, at) = (i, merged.len());
                    spliced.push((new_entries[j].drive, new_entries[j].file_ref, slot, at));
                    merged.push(new_entries[j].clone());
                    j += 1;
                }
            }
            while j < new_entries.len() {
                let (slot, at) = (old_len, merged.len());
                spliced.push((new_entries[j].drive, new_entries[j].file_ref, slot, at));
                merged.push(new_entries[j].clone());
                j += 1;
            }
            while i < old_len {
                merged.push(self.entries[i].clone());
                i += 1;
            }
        }
        self.entries = merged;

        // Pass 5: delta-merge ref buckets instead of a full rebuild. The
        // stored index of every surviving entry moves in two steps — each
        // removed ref compresses the space below it, each spliced record
        // expands it (at its insertion slot, in post-compression
        // coordinates) — then the batch's new (ref, final index) pairs are
        // inserted in ref order. Indices are global (one entries vec), so
        // every drive's bucket is re-based even when only one drive changed.
        let slots: Vec<usize> = spliced.iter().map(|&(_, _, s, _)| s).collect();
        // final index of each spliced record (pass 6 stamps depth/user
        // there) plus the per-drive (ref, final index) pairs for the
        // ordered bucket inserts below.
        let mut final_at: std::collections::HashMap<(u8, u64), usize> =
            std::collections::HashMap::with_capacity(spliced.len());
        let mut refs_by_drive: std::collections::HashMap<u8, Vec<(u64, u32)>> =
            std::collections::HashMap::new();
        for &(d, r, _, at) in &spliced {
            final_at.insert((d, r), at);
            refs_by_drive.entry(d).or_default().push((r, at as u32));
        }
        for (_, refs) in &mut refs_by_drive {
            refs.sort_unstable_by_key(|&(r, _)| r);
        }
        // Pre-batch (global) index of every ref this batch removed — the
        // compression half of the shift. Refs missing from the old buckets
        // simply never existed.
        let mut removed_idx: Vec<usize> = Vec::new();
        for &(d, r) in &removes {
            if let Some(bucket) = self.ref_lookup.get(d as usize) {
                if let Ok(p) = bucket.binary_search_by_key(&r, |&(br, _)| br) {
                    removed_idx.push(bucket[p].1 as usize);
                }
            }
        }
        removed_idx.sort_unstable();
        let compress = |i: usize| removed_idx.partition_point(|&ri| ri <= i);

        let mut drives: Vec<u8> = (0..self.ref_lookup.len() as u8).collect();
        drives.extend(spliced.iter().map(|&(d, _, _, _)| d));
        drives.extend(removes.iter().map(|&(d, _)| d));
        drives.sort_unstable();
        drives.dedup();
        while self.ref_lookup.len() as u8 <= *drives.last().unwrap_or(&0) {
            self.ref_lookup.push(Vec::new());
        }
        for d in drives {
            let bucket = &mut self.ref_lookup[d as usize];
            bucket.retain(|&(r, _)| !removes.contains(&(d, r)));
            for (_, idx) in bucket.iter_mut() {
                let i = *idx as usize;
                let c = compress(i); // surviving entries always keep c <= i
                *idx = (i - c + slots.partition_point(|&s| s <= i - c)) as u32;
            }
            if let Some(refs) = refs_by_drive.get(&d) {
                for &(r, idx) in refs {
                    let at = bucket.partition_point(|&(br, _)| br < r);
                    bucket.insert(at, (r, idx));
                }
            }
        }

        // Pass 6: stamp depth/user for the spliced records. The batch is
        // fully visible in the ref buckets now, so a parent created in the
        // same batch is found by the ancestor walk.
        for (d, record) in &newest {
            let lower = record.name.to_lowercase();
            let (depth, user_path) = self.path_meta(
                *d,
                record.parent_ref,
                &lower,
                matches!(record.kind, FileKind::Directory),
            );
            let at = final_at[&(*d, record.file_ref)];
            self.entries[at].depth = depth;
            self.entries[at].user_path = user_path;
        }
        self.ext_dirty = true;
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

/// Decode a cache file previously written by `save_cache` (v4 zstd frame) or
/// a legacy v3 lz4 frame back into its raw serialized form. `None` means the
/// file is missing, carries an unknown magic, or the frame failed to decode —
/// callers treat that as "discard and rescan" (a format bump intentionally
/// invalidates old files, causing exactly one fresh scan).
///
/// Both framings stream through `bincode::DefaultOptions` with a decode cap
/// (`MAX_CACHE_DECODED`) applied twice: once on the decompressed bytes pulled
/// from the decoder (a `Take`) and once inside bincode itself, so a hostile or
/// corrupt cache can never force an unbounded allocation in the always-elevated
/// host process before the structural checks in `from_cache` run.
pub fn load_cache_file(path: &std::path::Path) -> Option<CacheData> {
    use std::io::{BufReader, Read, Seek};
    let file = std::fs::File::open(path).ok()?;
    let mut r = BufReader::new(file);
    let mut magic = [0u8; 4];
    if r.read_exact(&mut magic).is_err() {
        return None;
    }
    if magic == [0x28, 0xB5, 0x2F, 0xFD] {
        // v4: zstd frame. The 4 magic bytes were consumed above — rewind so
        // the decoder starts at the frame boundary (it validates the magic
        // itself; starting 4 bytes in reads "Unknown frame descriptor").
        if r.seek(std::io::SeekFrom::Start(0)).is_err() {
            return None;
        }
        let mut dec = zstd::stream::read::Decoder::new(r).ok()?;
        bincode::DefaultOptions::new()
            .with_fixint_encoding()
            .with_limit(MAX_CACHE_DECODED)
            .deserialize_from::<_, CacheData>(Read::take(&mut dec, MAX_CACHE_DECODED))
            .ok()
    } else if magic == [0x04, 0x22, 0x4D, 0x18] {
        // v3: legacy lz4 frame.
        if r.seek(std::io::SeekFrom::Start(0)).is_err() {
            return None;
        }
        let mut dec = lz4_flex::frame::FrameDecoder::new(r);
        bincode::DefaultOptions::new()
            .with_fixint_encoding()
            .with_limit(MAX_CACHE_DECODED)
            .deserialize_from::<_, CacheData>(Read::take(&mut dec, MAX_CACHE_DECODED))
            .ok()
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ntfs_drive(letter: char) -> NtfsDrive {
        NtfsDrive {
            letter,
            root: format!("{}:\\", letter),
            device_path: format!("\\\\.\\{}:", letter),
        }
    }

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
            &ntfs_drive('C'),
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
        let pidx = store.lookup_idx(0, report.parent_ref).unwrap();
        assert_eq!(store.name(&store.entries[pidx as usize]), "Alice");

        // Pruned subtree members are remembered (live journal re-entries are
        // filtered through junk_refs), but surviving junk ROOT dirs are not.
        assert!(store.junk_refs[0].contains(&14)); // lodash.js under node_modules
        assert!(store.junk_refs[0].contains(&16)); // cache.tmp under Temp
        assert!(!store.junk_refs[0].contains(&13)); // node_modules itself kept
    }

    #[test]
    fn populate_stamps_depth_and_user_path() {
        // X1 producer: the scan sweep stamps depth + user-folder metadata in
        // the same ancestor walk that prunes junk, using a root directory
        // record like the real MFT (parent == self), so counts equal the old
        // `full_path.matches('\\')` semantics.
        let mut store = IndexStore::new();
        store.populate_from_scan(
            scan_result(&[
                (5, 5, "", true),                 // C:\ root
                (1, 5, "Users", true),
                (10, 1, "Alice", true),
                (11, 10, "report.txt", false),
                (20, 5, "dev", true),
                (21, 20, "deep", true),
                (22, 21, "root_like.py", false),
                (30, 50, "orphan.txt", false),    // parent absent from the scan
            ]),
            &ntfs_drive('C'),
        );
        store.finalize();

        fn meta(s: &IndexStore, name: &str) -> (u8, u8) {
            let e = s.entries.iter().find(|e| s.name(e) == name).unwrap();
            (e.depth, e.user_path)
        }
        assert_eq!(meta(&store, "report.txt"), (3, 1));   // Users\Alice
        assert_eq!(meta(&store, "Users"), (1, 1));         // own dir == user marker
        assert_eq!(meta(&store, "root_like.py"), (3, 0)); // dev\deep, no marker
        assert_eq!(meta(&store, "orphan.txt"), (1, 0));   // unresolved parent
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
            &ntfs_drive('C'),
        );
        store.finalize();

        let cache = store.to_cache();
        let loaded = IndexStore::from_cache(cache).unwrap();
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
        let pidx = loaded.lookup_idx(0, beta.parent_ref).unwrap();
        assert_eq!(loaded.name(&loaded.entries[pidx as usize]), "Alpha.TXT");
    }

    #[test]
    fn apply_events_collapses_create_and_rename_in_one_batch() {
        let mut store = IndexStore::new();
        store.populate_from_scan(
            scan_result(&[(1, 0, "Users", true)]),
            &ntfs_drive('C'),
        );
        store.finalize();

        // One user action (mkdir + rename) bursts CREATE + RENAME records for
        // the same file ref into a single applier batch. Both must collapse to
        // the newest record — never duplicate entries.
        store.apply_events(vec![
            IndexEvent::Created {
                drive_letter: 'C',
                record: FileRecord {
                    file_ref: 42,
                    parent_ref: 1,
                    name: "New Folder".to_string(),
                    kind: FileKind::Directory,
                },
            },
            IndexEvent::Moved {
                drive_letter: 'C',
                file_ref: 42,
                new_parent_ref: 1,
                name: "finder-final-name".to_string(),
                kind: FileKind::Directory,
            },
        ]);

        assert_eq!(store.entries.len(), 2); // Users + folder (no duplicate)
        let hits = store.entries.iter().filter(|e| e.file_ref == 42).count();
        assert_eq!(hits, 1);
        let hit = store.entries.iter().find(|e| e.file_ref == 42).unwrap();
        assert_eq!(store.name(hit), "finder-final-name");
    }

    #[test]
    fn apply_events_splices_sorted_and_delta_merges_ref_lookup() {
        // W4: one applier drain mixing a directory and a file born inside it
        // (batch-parent visibility), a rename, a move, and a delete. The
        // entry order must stay name-sorted and every bucket index must
        // resolve back to its own (drive, file_ref) — the splice + shift +
        // ordered-insert bookkeeping in aggregate.
        let mut store = IndexStore::new();
        store.populate_from_scan(
            scan_result(&[
                (1, 0, "Users", true),
                (10, 1, "Alice", true),
                (11, 10, "report.txt", false),
                (12, 10, "photo.PNG", false),
                (13, 0, "node_modules", true),
                (14, 13, "lodash.js", false),
            ]),
            &ntfs_drive('C'),
        );
        store.finalize();
        assert_eq!(store.entries.len(), 5); // node_modules kept, lodash.js pruned

        store.apply_events(vec![
            IndexEvent::Created {
                drive_letter: 'C',
                record: FileRecord {
                    file_ref: 20,
                    parent_ref: 1,
                    name: "zeta".to_string(),
                    kind: FileKind::Directory,
                },
            },
            IndexEvent::Created {
                drive_letter: 'C',
                record: FileRecord {
                    file_ref: 21,
                    parent_ref: 20,
                    name: "app.js".to_string(),
                    kind: FileKind::File,
                },
            },
            IndexEvent::Renamed {
                drive_letter: 'C',
                old_ref: 10,
                new_record: FileRecord {
                    file_ref: 10,
                    parent_ref: 1,
                    name: "Bob".to_string(),
                    kind: FileKind::Directory,
                },
            },
            IndexEvent::Moved {
                drive_letter: 'C',
                file_ref: 11,
                new_parent_ref: 20,
                name: "report.txt".to_string(),
                kind: FileKind::File,
            },
            IndexEvent::Deleted { drive_letter: 'C', file_ref: 12 },
        ]);

        // Post-splice order is exactly the name-sorted one — no late sort.
        let ordered: Vec<String> = store.entries.iter().map(|e| store.name(e).to_string()).collect();
        assert_eq!(
            ordered,
            vec![
                "app.js".to_string(),
                "Bob".to_string(),
                "node_modules".to_string(),
                "report.txt".to_string(),
                "Users".to_string(),
                "zeta".to_string(),
            ]
        );
        for w in store.entries.windows(2) {
            assert!(store.name_lower(&w[0]) <= store.name_lower(&w[1]));
        }
        // Every bucket entry resolves back to its own record; every record
        // is reachable through its drive's bucket.
        for (d, bucket) in store.ref_lookup.iter().enumerate() {
            for &(r, idx) in bucket {
                assert!((idx as usize) < store.entries.len());
                let e = &store.entries[idx as usize];
                assert_eq!(e.drive, d as u8);
                assert_eq!(e.file_ref, r);
            }
        }
        for (i, e) in store.entries.iter().enumerate() {
            assert_eq!(store.lookup_idx(e.drive, e.file_ref), Some(i as u32));
        }
        // depth/user were stamped against the completed batch: app.js and
        // report.txt sit under zeta (created in the same drain) under Users.
        fn meta(s: &IndexStore, name: &str) -> (u8, u8) {
            let e = s.entries.iter().find(|e| s.name(e) == name).unwrap();
            (e.depth, e.user_path)
        }
        assert_eq!(meta(&store, "app.js"), (2, 1));
        assert_eq!(meta(&store, "report.txt"), (2, 1));
        assert_eq!(meta(&store, "Bob"), (1, 1));
        assert!(store.lookup_idx(0, 12).is_none()); // photo.PNG deleted
    }

    #[test]
    fn delete_only_batch_refreshes_lookup_and_dirty_ext_index() {
        // Deletes-only drains skip the splice; they must still re-base the
        // ref buckets and mark ext_index dirty — its stored indices would
        // otherwise dangle past the trimmed entries and OOB an ext search.
        let mut store = IndexStore::new();
        store.populate_from_scan(
            scan_result(&[
                (1, 0, "Users", true),
                (10, 1, "a.txt", false),
                (11, 1, "b.txt", false),
            ]),
            &ntfs_drive('C'),
        );
        store.finalize();
        assert!(!store.ext_dirty);

        store.apply_events(vec![IndexEvent::Deleted { drive_letter: 'C', file_ref: 10 }]);
        assert!(store.ext_dirty);
        assert!(store.lookup_idx(0, 10).is_none());
        assert_eq!(store.lookup_idx(0, 11), Some(0)); // b.txt slides to index 0
        assert_eq!(store.entries.len(), 2);
    }

    #[test]
    fn random_event_batches_keep_index_invariants() {
        // Deterministic LCG — any failure reproduces. Mixed create/delete/
        // rename/move batches on two drives that share file_refs exercise
        // the splice + compression + expansion bookkeeping far harder than
        // any hand-written case.
        let mut rng: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = move || {
            rng = rng
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (rng >> 32) as u32
        };
        let mut store = IndexStore::new();
        store.populate_from_scan(
            scan_result(&[
                (1, 0, "Users", true),
                (2, 0, "dev", true),
                (10, 1, "a.txt", false),
                (11, 1, "b.txt", false),
                (12, 2, "c.bin", false),
            ]),
            &ntfs_drive('C'),
        );
        store.populate_from_scan(
            scan_result(&[
                (1, 0, "Games", true),
                (10, 1, "d.exe", false),
            ]),
            &ntfs_drive('D'),
        );
        store.finalize();

        let names = ["alpha", "beta", "gamma", "delta", "epsilon", "zeta.txt", "iota.pdf"];
        for _ in 0..150 {
            let mut batch = Vec::new();
            let n_ops = 1 + (next() % 5) as usize;
            for _ in 0..n_ops {
                let drive = if next() % 2 == 0 { 'C' } else { 'D' };
                let rec = 1 + (next() % 20) as u64;
                let name = names[(next() as usize) % names.len()].to_string();
                match next() % 4 {
                    0 => batch.push(IndexEvent::Created {
                        drive_letter: drive,
                        record: FileRecord {
                            file_ref: rec,
                            parent_ref: 1 + (next() % 2) as u64,
                            name,
                            kind: if next() % 3 == 0 {
                                FileKind::Directory
                            } else {
                                FileKind::File
                            },
                        },
                    }),
                    1 => batch.push(IndexEvent::Deleted {
                        drive_letter: drive,
                        file_ref: rec,
                    }),
                    2 => batch.push(IndexEvent::Renamed {
                        drive_letter: drive,
                        old_ref: rec,
                        new_record: FileRecord {
                            file_ref: rec,
                            parent_ref: 1 + (next() % 2) as u64,
                            name,
                            kind: FileKind::File,
                        },
                    }),
                    _ => batch.push(IndexEvent::Moved {
                        drive_letter: drive,
                        file_ref: rec,
                        new_parent_ref: 1 + (next() % 2) as u64,
                        name,
                        kind: FileKind::File,
                    }),
                }
            }
            store.apply_events(batch);

            // Invariants after every batch:
            // 1. entries stay name-sorted by the lowercased key.
            for w in store.entries.windows(2) {
                assert!(store.name_lower(&w[0]) <= store.name_lower(&w[1]), "unsorted");
            }
            // 2. no duplicate (drive, file_ref).
            let mut seen = std::collections::HashSet::new();
            for e in &store.entries {
                assert!(seen.insert((e.drive, e.file_ref)), "duplicate ref");
            }
            // 3. every bucket entry resolves into its own record.
            for (d, bucket) in store.ref_lookup.iter().enumerate() {
                for &(r, idx) in bucket {
                    assert!((idx as usize) < store.entries.len(), "dangling idx");
                    let e = &store.entries[idx as usize];
                    assert_eq!(e.drive, d as u8);
                    assert_eq!(e.file_ref, r);
                }
            }
            // 4. every record is reachable through its drive's bucket.
            for (i, e) in store.entries.iter().enumerate() {
                assert_eq!(store.lookup_idx(e.drive, e.file_ref), Some(i as u32));
            }
            // 5. stamped metadata stays in range.
            for e in &store.entries {
                assert!(e.depth >= 1 && e.depth <= 15);
                assert!(e.user_path <= 1);
            }
        }
    }

    #[test]
    fn multi_drive_refs_do_not_collide_and_paths_use_own_root() {
        let mut store = IndexStore::new();
        // Both drives use the same file_refs (1 = root dir, 10 = child) —
        // MFT record numbers are per-volume, so collisions are the norm.
        store.populate_from_scan(
            scan_result(&[
                (1, 0, "Users", true),
                (10, 1, "a.txt", false),
            ]),
            &ntfs_drive('C'),
        );
        store.populate_from_scan(
            scan_result(&[
                (1, 0, "Games", true),
                (10, 1, "b.exe", false),
            ]),
            &ntfs_drive('D'),
        );
        store.finalize();

        assert_eq!(store.drive_roots.len(), 2);
        assert_eq!(store.drive_roots[0].root, "C:\\");
        assert_eq!(store.drive_roots[1].root, "D:\\");

        let a = store.entries.iter().find(|e| store.name(e) == "a.txt").unwrap();
        let b = store.entries.iter().find(|e| store.name(e) == "b.exe").unwrap();
        assert_eq!(a.drive, 0);
        assert_eq!(b.drive, 1);

        // Same file_ref on both drives resolves inside its own volume.
        assert_eq!(store.name(&store.entries[store.lookup_idx(0, 10).unwrap() as usize]), "a.txt");
        assert_eq!(store.name(&store.entries[store.lookup_idx(1, 10).unwrap() as usize]), "b.exe");

        // Paths are built against each volume's own root.
        assert_eq!(
            crate::index::search::build_path(a, &store).to_string_lossy(),
            r"C:\Users\a.txt"
        );
        assert_eq!(
            crate::index::search::build_path(b, &store).to_string_lossy(),
            r"D:\Games\b.exe"
        );

        // Cache round-trips the per-drive layout.
        let cache = store.to_cache();
        let loaded = IndexStore::from_cache(cache).unwrap();
        assert_eq!(loaded.drive_roots.len(), 2);
        assert_eq!(loaded.junk_refs, store.junk_refs);
        let loaded_b = loaded.entries.iter().find(|e| loaded.name(e) == "b.exe").unwrap();
        assert_eq!(
            crate::index::search::build_path(loaded_b, &loaded).to_string_lossy(),
            r"D:\Games\b.exe"
        );
    }

    #[test]
    fn live_events_stay_in_their_drive() {
        let mut store = IndexStore::new();
        store.populate_from_scan(
            scan_result(&[
                (1, 0, "Users", true),
                (10, 1, "old.txt", false),
            ]),
            &ntfs_drive('C'),
        );
        store.populate_from_scan(
            scan_result(&[
                (1, 0, "Games", true),
                (10, 1, "keep.exe", false),
            ]),
            &ntfs_drive('D'),
        );
        store.finalize();

        // Deleting ref 10 on drive C must not touch drive D's ref 10.
        store.apply_events(vec![IndexEvent::Deleted {
            drive_letter: 'C',
            file_ref: 10,
        }]);
        assert!(store.entries.iter().any(|e| e.drive == 1 && store.name(e) == "keep.exe"));
        assert!(!store.entries.iter().any(|e| e.drive == 0 && store.name(e) == "old.txt"));

        // Creating ref 10 on drive D (same ref as deleted C entry) lands on D.
        store.apply_events(vec![IndexEvent::Created {
            drive_letter: 'D',
            record: FileRecord {
                file_ref: 10,
                parent_ref: 1,
                name: "new.txt".to_string(),
                kind: FileKind::File,
            },
        }]);
        let new_entries: Vec<&IndexEntry> =
            store.entries.iter().filter(|e| e.file_ref == 10).collect();
        assert_eq!(new_entries.len(), 1);
        assert_eq!(new_entries[0].drive, 1);
        assert_eq!(store.name(new_entries[0]), "new.txt");
    }

    #[test]
    fn cache_roundtrips_checkpoints_with_drive_letters() {
        let mut store = IndexStore::new();
        store.populate_from_scan(
            scan_result(&[(1, 0, "Alpha.TXT", false)]),
            &ntfs_drive('C'),
        );
        store.finalize();
        store.checkpoints.push(JournalCheckpoint {
            next_usn: 1_234_567,
            journal_id: 0xDEAD_BEEF,
            drive_letter: 'C',
        });
        store.checkpoints.push(JournalCheckpoint {
            next_usn: 99,
            journal_id: 0xCAFE_F00D,
            drive_letter: 'D',
        });

        // Checkpoints must survive the v2 cache round-trip verbatim — the
        // whole startup catch-up mechanism depends on it.
        let loaded = IndexStore::from_cache(store.to_cache()).unwrap();
        assert_eq!(
            loaded.checkpoints,
            vec![
                JournalCheckpoint {
                    next_usn: 1_234_567,
                    journal_id: 0xDEAD_BEEF,
                    drive_letter: 'C',
                },
                JournalCheckpoint {
                    next_usn: 99,
                    journal_id: 0xCAFE_F00D,
                    drive_letter: 'D',
                },
            ]
        );
    }

    #[test]
    fn dump_real_cache_checkpoints() {
        let la = std::env::var("LOCALAPPDATA").unwrap();
        let path = std::path::Path::new(&la)
            .join("Finder")
            .join("index")
            .join("finder_cache.bin");
        println!("path: {}", path.display());
        let file = std::fs::File::open(&path).unwrap();
        let mut dec =
            zstd::stream::read::Decoder::new(std::io::BufReader::new(file)).unwrap();
        let mut buf = Vec::new();
        std::io::Read::read_to_end(&mut dec, &mut buf).unwrap();
        println!("decoded: {} bytes", buf.len());
        // Cache files are fixint-encoded (bincode 1.3 free functions):
        // magic(4) then version u32 as 4 LE bytes, value <= 255 at byte[4]. A
        // deliberate format bump (v2 -> v3) makes old files structurally
        // unreadable, so bail gracefully instead of panicking — the app will
        // discard the stale file and rescan on next boot.
        if buf.len() < 5 || buf[4] as u32 != super::CACHE_FORMAT_VERSION {
            println!(
                "on-disk cache is an older format (byte[4]={}, current v{}); running the GUI once rebuilds it",
                buf.get(4).copied().unwrap_or(0),
                super::CACHE_FORMAT_VERSION
            );
            return;
        }
        let cache: super::CacheData = bincode::deserialize(&buf).unwrap();
        let store = IndexStore::from_cache(cache).expect("from_cache");
        println!("entries: {}", store.entries.len());
        println!("checkpoints: {:#?}", store.checkpoints);
        let needle = std::env::var("FINDER_NEEDLE").unwrap_or_default();
        if !needle.is_empty() {
            let mut hits = 0usize;
            for e in store.entries.iter() {
                let n = store.name(e);
                if n.to_lowercase().contains(&needle.to_lowercase()) {
                    let parent = store
                        .lookup_idx(e.drive, e.parent_ref)
                        .map(|i| store.name(&store.entries[i as usize]).to_string())
                        .unwrap_or_default();
                    println!(
                        "HIT: name='{}' file_ref={} parent_ref={} drive={} parentName='{}' ",
                        n, e.file_ref, e.parent_ref, e.drive, parent
                    );
                    hits += 1;
                }
            }
            println!("hits: {}", hits);
        }
    }

    #[test]
    fn cache_rejects_foreign_format() {
        let mut store = IndexStore::new();
        store.populate_from_scan(
            scan_result(&[(1, 0, "a.txt", false)]),
            &ntfs_drive('C'),
        );
        store.finalize();

        let mut cache = store.to_cache();
        cache.version = 1;
        assert!(IndexStore::from_cache(cache).is_none());

        let mut cache = store.to_cache();
        cache.magic = *b"OLDC";
        assert!(IndexStore::from_cache(cache).is_none());
    }

    #[test]
    fn cache_file_v4_zstd_roundtrip_matches_gui_save_and_load() {
        let mut store = IndexStore::new();
        store.populate_from_scan(
            scan_result(&[
                (1, 0, "Users", true),
                (10, 1, "doc.txt", false),
                (11, 1, "photo.png", false),
            ]),
            &ntfs_drive('C'),
        );
        store.finalize();
        let cache = store.to_cache();

        let path = std::env::temp_dir().join(format!(
            "finder_test_v4_{}.bin",
            std::process::id()
        ));
        {
            // Exact `save_cache` shape: zstd level-3 frame + plain
            // `bincode::serialize_into` of the CacheData.
            let file = std::fs::File::create(&path).unwrap();
            let mut enc =
                zstd::stream::write::Encoder::new(std::io::BufWriter::new(file), 3).unwrap();
            bincode::serialize_into(&mut enc, &cache).unwrap();
            let _ = enc.finish().unwrap();
        }

        let loaded = super::load_cache_file(&path).expect("v4 zstd cache must decode");
        let loaded_store = IndexStore::from_cache(loaded).unwrap();
        assert_eq!(
            names(&loaded_store),
            vec!["Users", "doc.txt", "photo.png"]
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn cache_file_v3_lz4_roundtrip_is_still_readable() {
        let mut store = IndexStore::new();
        store.populate_from_scan(
            scan_result(&[(1, 0, "Legacy", true), (2, 1, "old.bin", false)]),
            &ntfs_drive('D'),
        );
        store.finalize();
        let cache = store.to_cache();

        let path = std::env::temp_dir().join(format!(
            "finder_test_v3_{}.bin",
            std::process::id()
        ));
        {
            let file = std::fs::File::create(&path).unwrap();
            let mut enc = lz4_flex::frame::FrameEncoder::new(std::io::BufWriter::new(file));
            bincode::serialize_into(&mut enc, &cache).unwrap();
            let _ = enc.finish().unwrap();
        }

        let loaded = super::load_cache_file(&path).expect("v3 lz4 cache must decode");
        let loaded_store = IndexStore::from_cache(loaded).unwrap();
        assert_eq!(names(&loaded_store), vec!["Legacy", "old.bin"]);
        let _ = std::fs::remove_file(&path);
    }

    /// Opt-in diagnostic: set `FINDER_CACHE_PROBE=<path>` to run the exact
    /// production loader against a real saved cache file (warm-start path).
    /// Skipped entirely when the variable is absent, so normal test runs and
    /// CI are unaffected.
    #[test]
    fn probe_real_saved_cache() {
        let Ok(path) = std::env::var("FINDER_CACHE_PROBE") else {
            return;
        };
        let cache = super::load_cache_file(std::path::Path::new(&path))
            .expect("real cache file must decode (warm-start path)");
        eprintln!(
            "PROBE: decoded {} entries / {} drives from {}",
            cache.entries.len(),
            cache.drive_roots.len(),
            path
        );
        let store = IndexStore::from_cache(cache)
            .expect("real cache must pass from_cache structural checks");
        assert!(store.len() > 0);
    }
}
