#![windows_subsystem = "windows"]
#![allow(dead_code)]

mod window;
mod hotkey;

use std::sync::Arc;
use parking_lot::RwLock;
use crossbeam_channel::unbounded;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::WindowsAndMessaging::{FindWindowW,PostMessageW};

use fastsearch::index::store::IndexStore;
use fastsearch::mft::reader::MftReader;
use fastsearch::mft::watcher::UsnWatcher;
use fastsearch::mft::types::IndexEvent;
use fastsearch::utils::drives::get_ntfs_drives;
use windows::Win32::Foundation::GetLastError;
use windows::Win32::Foundation::ERROR_ALREADY_EXISTS;
use windows::Win32::Foundation::{WPARAM, LPARAM};


fn main() {
    // ── Single-instance guard ────────────────────────────────────────────────
    // If another instance is already running, tell it to show/toggle its window
    // and then exit immediately — no second process ever starts.
    unsafe {
        let mutex_name: Vec<u16> = "FastSeek_SingleInstance_Mutex\0"
            .encode_utf16().collect();
        let _h_mutex = CreateMutexW(
            None, true,
            windows::core::PCWSTR(mutex_name.as_ptr()),
        );
        if GetLastError() == ERROR_ALREADY_EXISTS {
            // Find the existing window and ask it to toggle visibility
            if let Ok(hwnd) = FindWindowW(windows::core::w!("FastSeekWnd"), None) {
                if !hwnd.0.is_null() {
                    // WM_TOGGLE_WINDOW = WM_USER + 3
                    let _ = PostMessageW(
                        Some(hwnd),
                        0x0400 + 3,
                        WPARAM(0),
                        LPARAM(0),
                    );
                }
            }
            std::process::exit(0);
        }
    }

    // ── Discover NTFS drives ─────────────────────────────────────────────────
    let drives = get_ntfs_drives();
    if drives.is_empty() {
        std::process::exit(1);
    }

    // ── Index setup ──────────────────────────────────────────────────────────
    let index: Arc<RwLock<IndexStore>> = Arc::new(RwLock::new(IndexStore::new()));
    let (tx, rx) = unbounded();
    let cache_path = std::env::temp_dir().join("fastseek_cache.bin");

    // Load from cache (with delta catch-up) or do a full MFT scan
    let cache_loaded = load_or_scan(&index, &drives, &cache_path);
    if !cache_loaded {
        full_scan(&index, &drives, &cache_path);
    }

    // ── Live USN watchers (one thread per drive) ──────────────────────────────
    let live_cps: Arc<parking_lot::Mutex<Vec<fastsearch::mft::types::JournalCheckpoint>>> =
        Arc::new(parking_lot::Mutex::new(index.read().checkpoints.clone()));

    for drive in &drives {
        let tx2   = tx.clone();
        let d2    = drive.clone();
        let cps2  = Arc::clone(&live_cps);
        std::thread::spawn(move || {
            if let Ok(mut w) = UsnWatcher::new(&d2, tx2) {
                w.run_shared(cps2);
            }
        });
    }

    // ── Index update consumer ─────────────────────────────────────────────────
    let idx2 = Arc::clone(&index);
    std::thread::spawn(move || {
        for event in rx {
            let mut s = idx2.write();
            match event {
                IndexEvent::Created(r)       => s.insert(r),
                IndexEvent::Deleted(id)      => s.remove(id),
                IndexEvent::Renamed { old_ref, new_record }
                                             => s.rename(old_ref, new_record),
                IndexEvent::Moved { file_ref, new_parent_ref, name, kind }
                                             => s.apply_move(file_ref, new_parent_ref, name, kind),
            }
        }
    });

    // ── Ctrl-C / shutdown: persist cache ─────────────────────────────────────
    let idx3  = Arc::clone(&index);
    let cps3  = Arc::clone(&live_cps);
    let cp2   = cache_path.clone();
    ctrlc::set_handler(move || {
        let mut s = idx3.write();
        s.checkpoints = cps3.lock().clone();
        save_cache(&s, &cp2);
        std::process::exit(0);
    }).ok();

    // ── Exclusions ────────────────────────────────────────────────────────────
    let excluded = load_exclusions();

    // ── Win32 GUI (blocks until window is closed) ─────────────────────────────
    window::run(index, excluded);
}

// ── Cache helpers ─────────────────────────────────────────────────────────────

fn load_or_scan(
    index:      &Arc<RwLock<IndexStore>>,
    drives:     &[fastsearch::mft::types::NtfsDrive],
    cache_path: &std::path::Path,
) -> bool {
    if !cache_path.exists() { return false; }

    let compressed = match std::fs::read(cache_path) {
        Ok(b)  => b,
        Err(_) => return false,
    };
    let bytes = match lz4_flex::decompress_size_prepended(&compressed) {
        Ok(b)  => b,
        Err(_) => return false,
    };
    let cache = match bincode::deserialize::<fastsearch::index::store::CacheData>(&bytes) {
        Ok(c)  => c,
        Err(_) => { let _ = std::fs::remove_file(cache_path); return false; }
    };

    let checkpoints = cache.checkpoints.clone();
    *index.write() = IndexStore::from_cache(cache);

    if checkpoints.is_empty() { return true; }

    // Delta catch-up from USN journal
    let (dtx, drx) = unbounded::<IndexEvent>();
    for drive in drives {
        let cp = checkpoints.iter().find(|c| c.drive_letter == drive.letter);
        if let Some(cp) = cp {
            match UsnWatcher::new_from(drive, dtx.clone(), Some(cp)) {
                Ok(mut w) => {
                    w.drain();
                    let new_cp = w.checkpoint();
                    let mut s = index.write();
                    s.checkpoints.retain(|c| c.drive_letter != drive.letter);
                    s.checkpoints.push(new_cp);
                }
                Err(_) => {
                    let _ = std::fs::remove_file(cache_path);
                    return false;
                }
            }
        } else {
            let _ = std::fs::remove_file(cache_path);
            return false;
        }
    }
    drop(dtx);

    let mut s = index.write();
    for event in drx {
        match event {
            IndexEvent::Created(r)       => s.insert(r),
            IndexEvent::Deleted(id)      => s.remove(id),
            IndexEvent::Renamed { old_ref, new_record }
                                         => s.rename(old_ref, new_record),
            IndexEvent::Moved { file_ref, new_parent_ref, name, kind }
                                         => s.apply_move(file_ref, new_parent_ref, name, kind),
        }
    }
    true
}

fn full_scan(
    index:      &Arc<RwLock<IndexStore>>,
    drives:     &[fastsearch::mft::types::NtfsDrive],
    cache_path: &std::path::Path,
) {
    {
        let mut s = index.write();
        for drive in drives {
            let (dtx, _) = unbounded::<IndexEvent>();
            if let Ok(w) = UsnWatcher::new(drive, dtx) {
                s.checkpoints.push(w.checkpoint());
            }
        }
    }
    for drive in drives {
        if let Ok(reader) = MftReader::open(drive) {
            let (scan, _) = match reader.scan_direct() {
                Some(s) => (s, "direct"),
                None    => (reader.scan(), "ioctl"),
            };
            index.write().populate_from_scan(scan, &drive.root);
        }
    }
    index.write().finalize();
    save_cache(&index.read(), cache_path);
}

fn save_cache(store: &IndexStore, path: &std::path::Path) {
    if let Ok(bytes) = bincode::serialize(&store.to_cache()) {
        let _ = std::fs::write(path, lz4_flex::compress_prepend_size(&bytes));
    }
}

fn config_path() -> std::path::PathBuf {
    let dir = std::env::var("APPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir())
        .join("fastsearch");
    let _ = std::fs::create_dir_all(&dir);
    dir.join("config.txt")
}

fn load_exclusions() -> Vec<String> {
    std::fs::read_to_string(config_path())
        .unwrap_or_default()
        .lines()
        .map(|l| l.trim().to_lowercase())
        .filter(|l| !l.is_empty())
        .collect()
}