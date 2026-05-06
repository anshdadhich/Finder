#![allow(dead_code)]
use serde::{Serialize, Deserialize};
use crate::mft::types::{FileKind, FileRecord, JournalCheckpoint};
use crate::mft::reader::ScanResult;

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
        }
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
        self.ref_lookup.sort_unstable_by_key(|&(r, _)| r);
    }

    // ── Populate from MFT scan ───────────────────────────────────────

    pub fn populate_from_scan(&mut self, scan: ScanResult, drive_root: &str) {
        self.drive_root = drive_root.to_string();
        let count = scan.records.len();
        self.entries.reserve(count);
        // Estimate ~30 bytes avg per name
        self.name_arena.reserve(count * 30);
        self.name_lower_arena.reserve(count * 30);

        for r in &scan.records {
            let name_slice = &scan.name_data[r.name_off as usize..(r.name_off as usize + r.name_len as usize)];
            let name = String::from_utf16_lossy(name_slice);
            let name_lower = name.to_lowercase();

            let n_off = self.name_arena.len() as u32;
            let n_len = name.len() as u16;
            self.name_arena.extend_from_slice(name.as_bytes());

            let nl_off = self.name_lower_arena.len() as u32;
            let nl_len = name_lower.len() as u16;
            self.name_lower_arena.extend_from_slice(name_lower.as_bytes());

            self.entries.push(IndexEntry {
                file_ref: r.file_ref,
                parent_ref: r.parent_ref,
                name_off: n_off,
                name_lower_off: nl_off,
                name_len: n_len,
                name_lower_len: nl_len,
                flags: if r.is_dir { 1 } else { 0 },
            });
        }
    }

    pub fn finalize(&mut self) {
        let store_ptr = self as *const IndexStore;
        self.entries.sort_unstable_by(|a, b| {
            let s = unsafe { &*store_ptr };
            s.name_lower(a).cmp(s.name_lower(b))
        });
        self.rebuild_ref_lookup();
        // Shrink arenas to fit
        self.name_arena.shrink_to_fit();
        self.name_lower_arena.shrink_to_fit();
    }

    // ── Cache serialization ──────────────────────────────────────────

    pub fn to_cache(&self) -> CacheData {
        CacheData {
            entries: self.entries.iter().map(|e| CachedEntry {
                file_ref: e.file_ref,
                parent_ref: e.parent_ref,
                name: self.name(e).to_string(),
                kind: e.kind(),
            }).collect(),
            drive_root: self.drive_root.clone(),
            checkpoints: self.checkpoints.clone(),
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
        };

        for c in cache.entries {
            let name_lower = c.name.to_lowercase();

            let n_off = store.name_arena.len() as u32;
            let n_len = c.name.len() as u16;
            store.name_arena.extend_from_slice(c.name.as_bytes());

            let nl_off = store.name_lower_arena.len() as u32;
            let nl_len = name_lower.len() as u16;
            store.name_lower_arena.extend_from_slice(name_lower.as_bytes());

            let flags = match c.kind {
                FileKind::Directory => 1u8,
                FileKind::File => 0u8,
            };

            store.entries.push(IndexEntry {
                file_ref: c.file_ref,
                parent_ref: c.parent_ref,
                name_off: n_off,
                name_lower_off: nl_off,
                name_len: n_len,
                name_lower_len: nl_len,
                flags,
            });
        }

        store.rebuild_ref_lookup();
        store.name_arena.shrink_to_fit();
        store.name_lower_arena.shrink_to_fit();
        store
    }

    // ── Live mutations ───────────────────────────────────────────────

    pub fn insert(&mut self, record: FileRecord) {
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

        let entry = IndexEntry {
            file_ref: record.file_ref,
            parent_ref: record.parent_ref,
            name_off: n_off,
            name_lower_off: nl_off,
            name_len: n_len,
            name_lower_len: nl_len,
            flags,
        };

        let store_ptr = self as *const IndexStore;
        let pos = self.entries.partition_point(|e| {
            let s = unsafe { &*store_ptr };
            s.name_lower(e) < name_lower.as_str()
        });
        self.entries.insert(pos, entry);
        self.rebuild_ref_lookup();
    }

    pub fn remove(&mut self, file_ref: u64) {
        // Name bytes left as dead space in arena (negligible for rare deletes)
        self.entries.retain(|e| e.file_ref != file_ref);
        self.rebuild_ref_lookup();
    }

    pub fn rename(&mut self, old_ref: u64, new_record: FileRecord) {
        self.remove(old_ref);
        self.insert(new_record);
    }

    pub fn apply_move(&mut self, file_ref: u64, new_parent_ref: u64, name: String, kind: FileKind) {
        self.remove(file_ref);
        self.insert(FileRecord { file_ref, parent_ref: new_parent_ref, name, kind });
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
}