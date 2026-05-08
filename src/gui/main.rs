#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::{
    io::{self, Write},
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::Instant,
};

use crossbeam_channel::unbounded;
use parking_lot::{Mutex, RwLock};
use serde::Serialize;
use tauri::{
    CustomMenuItem, GlobalShortcutManager, Manager, SystemTray, SystemTrayEvent, SystemTrayMenu,
};

use fastsearch::{
    index::{search, store::IndexStore},
    mft::{
        reader::MftReader,
        types::{IndexEvent, JournalCheckpoint},
        watcher::UsnWatcher,
    },
    utils::drives::get_ntfs_drives,
};

#[derive(Clone)]
struct AppState {
    index: Arc<RwLock<IndexStore>>,
    ready: Arc<AtomicBool>,
    status: Arc<RwLock<String>>,
}

#[derive(Serialize)]
struct UiResult {
    name: String,
    path: String,
    is_dir: bool,
    rank: u8,
}

#[derive(Serialize)]
struct IndexStatus {
    ready: bool,
    count: usize,
    message: String,
}

#[tauri::command]
fn get_index_status(state: tauri::State<AppState>) -> IndexStatus {
    IndexStatus {
        ready: state.ready.load(Ordering::Relaxed),
        count: state.index.read().len(),
        message: state.status.read().clone(),
    }
}

#[tauri::command]
fn search_files(query: String, state: tauri::State<AppState>) -> Vec<UiResult> {
    if !state.ready.load(Ordering::Relaxed) || query.trim().is_empty() {
        return Vec::new();
    }

    let store = state.index.read();
    search::search(&store, query.trim(), 300, false, &[])
        .into_iter()
        .take(300)
        .map(|r| UiResult {
            name: r.name,
            path: r.full_path.to_string_lossy().to_string(),
            is_dir: r.is_dir,
            rank: r.rank,
        })
        .collect()
}

#[tauri::command]
fn hide_window(window: tauri::Window) {
    let _ = window.hide();
}

#[tauri::command]
fn open_path(path: String) -> Result<(), String> {
    std::process::Command::new("explorer")
        .arg(path)
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn open_parent(path: String) -> Result<(), String> {
    let parent = Path::new(&path)
        .parent()
        .ok_or_else(|| "Path has no parent".to_string())?;
    std::process::Command::new("explorer")
        .arg(parent)
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn open_web_search(query: String) -> Result<(), String> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return Ok(());
    }

    let target = if looks_like_url(trimmed) {
        if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
            trimmed.to_string()
        } else {
            format!("https://{}", trimmed)
        }
    } else {
        format!(
            "https://www.google.com/search?q={}",
            trimmed.replace(' ', "+")
        )
    };

    std::process::Command::new("rundll32")
        .args(["url.dll,FileProtocolHandler", &target])
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

fn looks_like_url(value: &str) -> bool {
    let lower = value.to_lowercase();
    lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.contains(".com")
        || lower.contains(".net")
        || lower.contains(".org")
}

fn main() {
    let index = Arc::new(RwLock::new(IndexStore::new()));
    let ready = Arc::new(AtomicBool::new(false));
    let status = Arc::new(RwLock::new(String::from("Starting...")));
    let live_checkpoints: Arc<Mutex<Vec<JournalCheckpoint>>> = Arc::new(Mutex::new(Vec::new()));

    let state = AppState {
        index: Arc::clone(&index),
        ready: Arc::clone(&ready),
        status: Arc::clone(&status),
    };
    let setup_index = Arc::clone(&index);
    let setup_ready = Arc::clone(&ready);
    let setup_status = Arc::clone(&status);
    let setup_live_checkpoints = Arc::clone(&live_checkpoints);
    let close_index = Arc::clone(&index);
    let close_live_checkpoints = Arc::clone(&live_checkpoints);

    tauri::Builder::default()
        .system_tray(SystemTray::new().with_menu(
            SystemTrayMenu::new()
                .add_item(CustomMenuItem::new("show".to_string(), "Show Search"))
                .add_item(CustomMenuItem::new("quit".to_string(), "Quit")),
        ))
        .manage(state)
        .setup(move |app| {
            let window = app.get_window("main").expect("main window");
            let _ = window.center();
            let _ = window.show();
            let _ = window.set_focus();

            register_shortcut(app, "Super+Space", window.clone());
            register_shortcut(app, "Ctrl+Space", window.clone());

            start_backend(
                Arc::clone(&setup_index),
                Arc::clone(&setup_ready),
                Arc::clone(&setup_status),
                Arc::clone(&setup_live_checkpoints),
            );
            Ok(())
        })
        .on_window_event({
            move |event| {
                if matches!(event.event(), tauri::WindowEvent::CloseRequested { .. }) {
                    save_cache_with_checkpoints(
                        &close_index,
                        &close_live_checkpoints,
                        &std::env::temp_dir().join("fastseek_cache.bin"),
                    );
                }
            }
        })
        .on_system_tray_event(|app, event| match event {
            SystemTrayEvent::LeftClick { .. } => {
                if let Some(window) = app.get_window("main") {
                    let _ = window.center();
                    let _ = window.show();
                    let _ = window.set_focus();
                }
            }
            SystemTrayEvent::MenuItemClick { id, .. } if id.as_str() == "show" => {
                if let Some(window) = app.get_window("main") {
                    let _ = window.center();
                    let _ = window.show();
                    let _ = window.set_focus();
                }
            }
            SystemTrayEvent::MenuItemClick { id, .. } if id.as_str() == "quit" => {
                std::process::exit(0);
            }
            _ => {}
        })
        .invoke_handler(tauri::generate_handler![
            get_index_status,
            search_files,
            hide_window,
            open_path,
            open_parent,
            open_web_search
        ])
        .run(tauri::generate_context!())
        .expect("error while running FastSeek");
}

fn register_shortcut(app: &tauri::App, shortcut: &str, window: tauri::Window) {
    let label = shortcut.to_string();
    if let Err(e) = app.global_shortcut_manager().register(shortcut, move || {
        if window.is_visible().unwrap_or(false) {
            let _ = window.hide();
        } else {
            let _ = window.center();
            let _ = window.show();
            let _ = window.set_focus();
        }
    }) {
        eprintln!("Could not register {}: {}", label, e);
    }
}

fn start_backend(
    index: Arc<RwLock<IndexStore>>,
    ready: Arc<AtomicBool>,
    status: Arc<RwLock<String>>,
    live_checkpoints: Arc<Mutex<Vec<JournalCheckpoint>>>,
) {
    thread::spawn(move || {
        *status.write() = String::from("Finding NTFS drives...");
        let drives = get_ntfs_drives();
        if drives.is_empty() {
            *status.write() = String::from("No NTFS drives found. Run as Administrator.");
            eprintln!("No NTFS drives found. Are you running as Administrator?");
            return;
        }

        let (tx, rx) = unbounded();
        let cache_path = std::env::temp_dir().join("fastseek_cache.bin");
        *status.write() = String::from("Loading cached index...");
        let cache_loaded = load_cache_and_catch_up(&index, &drives, &cache_path);

        if !cache_loaded {
            build_full_index(&index, &drives, &cache_path, &status);
        }

        ready.store(true, Ordering::Relaxed);
        *status.write() = format!("{} files indexed", index.read().len());

        *live_checkpoints.lock() = index.read().checkpoints.clone();

        for drive in &drives {
            let tx_clone = tx.clone();
            let drive_clone = drive.clone();
            let cps = Arc::clone(&live_checkpoints);
            thread::spawn(move || {
                if let Ok(mut watcher) = UsnWatcher::new(&drive_clone, tx_clone) {
                    watcher.run_shared(cps);
                }
            });
        }

        for event in rx {
            let mut store = index.write();
            apply_event(&mut store, event);
        }
    });
}

fn load_cache_and_catch_up(
    index: &Arc<RwLock<IndexStore>>,
    drives: &[fastsearch::mft::types::NtfsDrive],
    cache_path: &Path,
) -> bool {
    if !cache_path.exists() {
        return false;
    }

    match std::fs::read(cache_path)
        .ok()
        .and_then(|compressed| lz4_flex::decompress_size_prepended(&compressed).ok())
        .and_then(|bytes| bincode::deserialize::<fastsearch::index::store::CacheData>(&bytes).ok())
    {
        Some(cache) => {
            if cache.entries.is_empty() {
                let _ = std::fs::remove_file(cache_path);
                *index.write() = IndexStore::new();
                return false;
            }

            let checkpoints = cache.checkpoints.clone();
            *index.write() = IndexStore::from_cache(cache);

            if checkpoints.is_empty() {
                return true;
            }

            let (delta_tx, delta_rx) = unbounded::<IndexEvent>();
            for drive in drives {
                let Some(cp) = checkpoints.iter().find(|c| c.drive_letter == drive.letter) else {
                    let _ = std::fs::remove_file(cache_path);
                    *index.write() = IndexStore::new();
                    return false;
                };

                let Ok(mut watcher) = UsnWatcher::new_from(drive, delta_tx.clone(), Some(cp)) else {
                    let _ = std::fs::remove_file(cache_path);
                    *index.write() = IndexStore::new();
                    return false;
                };

                watcher.drain();
                let new_cp = watcher.checkpoint();
                let mut store = index.write();
                store.checkpoints.retain(|c| c.drive_letter != drive.letter);
                store.checkpoints.push(new_cp);
            }

            drop(delta_tx);
            let mut store = index.write();
            for event in delta_rx {
                apply_event(&mut store, event);
            }
            true
        }
        None => {
            let _ = std::fs::remove_file(cache_path);
            *index.write() = IndexStore::new();
            false
        }
    }
}

fn build_full_index(
    index: &Arc<RwLock<IndexStore>>,
    drives: &[fastsearch::mft::types::NtfsDrive],
    cache_path: &Path,
    status: &Arc<RwLock<String>>,
) {
    *index.write() = IndexStore::new();
    {
        let mut store = index.write();
        for drive in drives {
            let (dummy_tx, _) = unbounded::<IndexEvent>();
            if let Ok(w) = UsnWatcher::new(drive, dummy_tx) {
                store.checkpoints.push(w.checkpoint());
            }
        }
    }

    let total_start = Instant::now();
    for drive in drives {
        *status.write() = format!("Scanning {}:...", drive.letter);
        let reader: MftReader = match MftReader::open(drive) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("Scan failed on {}: {:?}", drive.letter, e);
                continue;
            }
        };

        let scan = match reader.scan_direct() {
            Some(scan) if !scan.records.is_empty() => scan,
            _ => reader.scan(),
        };
        index.write().populate_from_scan(scan, &drive.root);
    }

    index.write().finalize();
    save_cache(index, cache_path);
    *status.write() = format!("{} files indexed", index.read().len());
    let _ = writeln!(
        io::stderr(),
        "FastSeek index ready in {:.2}s",
        total_start.elapsed().as_secs_f64()
    );
}

fn save_cache(index: &Arc<RwLock<IndexStore>>, cache_path: &Path) {
    let store = index.read();
    if store.entries.is_empty() {
        return;
    }

    let cache = store.to_cache();
    if let Ok(bytes) = bincode::serialize(&cache) {
        let compressed = lz4_flex::compress_prepend_size(&bytes);
        let _ = write_atomic(cache_path, &compressed);
    }
}

fn save_cache_with_checkpoints(
    index: &Arc<RwLock<IndexStore>>,
    live_checkpoints: &Arc<Mutex<Vec<JournalCheckpoint>>>,
    cache_path: &Path,
) {
    {
        let mut store = index.write();
        if store.entries.is_empty() {
            return;
        }

        let checkpoints = live_checkpoints.lock().clone();
        if !checkpoints.is_empty() {
            store.checkpoints = checkpoints;
        }
    }
    save_cache(index, cache_path);
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension(format!(
        "tmp.{}",
        std::process::id()
    ));
    std::fs::write(&tmp, bytes)?;
    match std::fs::rename(&tmp, path) {
        Ok(_) => Ok(()),
        Err(_) => {
            let _ = std::fs::remove_file(path);
            std::fs::rename(&tmp, path)
        }
    }
}

fn apply_event(store: &mut IndexStore, event: IndexEvent) {
    match event {
        IndexEvent::Created(r) => store.insert(r),
        IndexEvent::Deleted(id) => store.remove(id),
        IndexEvent::Renamed {
            old_ref,
            new_record,
        } => store.rename(old_ref, new_record),
        IndexEvent::Moved {
            file_ref,
            new_parent_ref,
            name,
            kind,
        } => store.apply_move(file_ref, new_parent_ref, name, kind),
    }
}
