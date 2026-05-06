#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crossbeam_channel::unbounded;
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, State, WindowEvent};
use tauri_plugin_global_shortcut::GlobalShortcutExt;

use fastsearch::index::search::{search as do_search, SearchResult};
use fastsearch::index::store::IndexStore;
use fastsearch::mft::reader::MftReader;
use fastsearch::mft::types::{IndexEvent, JournalCheckpoint, NtfsDrive};
use fastsearch::mft::watcher::UsnWatcher;
use fastsearch::utils::drives::get_ntfs_drives;

const WINDOW_LABEL: &str = "main";
const GLOBAL_SHORTCUT: &str = "Alt+Space";
const TRAY_MENU_TOGGLE_ID: &str = "toggle";
const TRAY_MENU_QUIT_ID: &str = "quit";

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct UiResult {
    full_path: String,
    name: String,
    rank: u8,
    is_dir: bool,
    kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SettingsPayload {
    excluded_paths: Vec<String>,
    case_sensitive: bool,
}

struct AppState {
    index: Arc<RwLock<IndexStore>>,
    excluded: Mutex<Vec<String>>,
    case_sensitive: Mutex<bool>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    App,
    Document,
    Image,
    Video,
    Audio,
    Archive,
    Folder,
    Other,
}

impl Kind {
    fn sort_key(self) -> u8 {
        match self {
            Kind::App => 0,
            Kind::Document => 1,
            Kind::Image => 2,
            Kind::Video => 3,
            Kind::Audio => 4,
            Kind::Archive => 5,
            Kind::Folder => 6,
            Kind::Other => 7,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Kind::App => "APP",
            Kind::Document => "DOC",
            Kind::Image => "IMG",
            Kind::Video => "VID",
            Kind::Audio => "AUD",
            Kind::Archive => "ZIP",
            Kind::Folder => "DIR",
            Kind::Other => "FILE",
        }
    }
}

fn kind_for_result(r: &SearchResult) -> Kind {
    if r.is_dir {
        return Kind::Folder;
    }
    let ext = r
        .full_path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    match ext.as_deref() {
        Some("exe") | Some("lnk") | Some("appref-ms") | Some("msi") | Some("appx") | Some("msix") => {
            Kind::App
        }
        Some("doc")
        | Some("docx")
        | Some("pdf")
        | Some("txt")
        | Some("xlsx")
        | Some("xls")
        | Some("pptx")
        | Some("ppt")
        | Some("odt")
        | Some("ods")
        | Some("odp")
        | Some("rtf")
        | Some("md")
        | Some("csv")
        | Some("json")
        | Some("xml")
        | Some("yaml")
        | Some("toml")
        | Some("ini")
        | Some("log") => Kind::Document,
        Some("png")
        | Some("jpg")
        | Some("jpeg")
        | Some("gif")
        | Some("bmp")
        | Some("webp")
        | Some("svg")
        | Some("ico")
        | Some("tiff")
        | Some("heic")
        | Some("raw")
        | Some("psd") => Kind::Image,
        Some("mp4")
        | Some("mkv")
        | Some("avi")
        | Some("mov")
        | Some("wmv")
        | Some("flv")
        | Some("webm")
        | Some("m4v") => Kind::Video,
        Some("mp3")
        | Some("flac")
        | Some("wav")
        | Some("aac")
        | Some("ogg")
        | Some("m4a")
        | Some("wma")
        | Some("opus") => Kind::Audio,
        Some("zip") | Some("rar") | Some("7z") | Some("tar") | Some("gz") | Some("bz2") | Some("xz")
        | Some("zst") => Kind::Archive,
        _ => Kind::Other,
    }
}

#[tauri::command]
fn search(
    state: State<'_, AppState>,
    query: String,
    limit: Option<usize>,
    case_sensitive: Option<bool>,
) -> Vec<UiResult> {
    let search_limit = limit.unwrap_or(50).clamp(1, 200);
    let excluded = state.excluded.lock().clone();
    let case_flag = case_sensitive.unwrap_or(*state.case_sensitive.lock());
    if let Some(flag) = case_sensitive {
        *state.case_sensitive.lock() = flag;
    }

    let store = state.index.read();
    let mut results = do_search(&store, query.trim(), search_limit, case_flag, &excluded);
    results.sort_by_key(|r| (kind_for_result(r).sort_key(), r.rank));

    results
        .into_iter()
        .map(|r| {
            let kind = kind_for_result(&r).as_str().to_string();
            UiResult {
                full_path: r.full_path.to_string_lossy().to_string(),
                name: r.name,
                rank: r.rank,
                is_dir: r.is_dir,
                kind,
            }
        })
        .collect()
}

#[tauri::command]
fn open_result(path: String, folder_only: bool) -> Result<(), String> {
    let input = PathBuf::from(path);
    let target = if folder_only {
        if input.is_dir() {
            input
        } else {
            input.parent().map(Path::to_path_buf).unwrap_or(input)
        }
    } else {
        input
    };
    std::process::Command::new("explorer.exe")
        .arg(target)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("failed to open path: {e}"))
}

#[tauri::command]
fn get_settings(state: State<'_, AppState>) -> SettingsPayload {
    SettingsPayload {
        excluded_paths: state.excluded.lock().clone(),
        case_sensitive: *state.case_sensitive.lock(),
    }
}

#[tauri::command]
fn save_settings(state: State<'_, AppState>, payload: SettingsPayload) -> Result<(), String> {
    let mut normalized = Vec::with_capacity(payload.excluded_paths.len());
    for p in payload.excluded_paths {
        let trimmed = p.trim().to_lowercase();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.ends_with('\\') || trimmed.ends_with('/') {
            normalized.push(trimmed);
        } else {
            normalized.push(format!("{trimmed}\\"));
        }
    }
    *state.excluded.lock() = normalized.clone();
    *state.case_sensitive.lock() = payload.case_sensitive;
    let content = normalized.join("\n");
    std::fs::write(config_path(), content).map_err(|e| format!("save settings failed: {e}"))
}

fn config_path() -> PathBuf {
    let dir = std::env::var("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir())
        .join("fastsearch");
    let _ = std::fs::create_dir_all(&dir);
    dir.join("config.txt")
}

fn load_exclusions() -> Vec<String> {
    std::fs::read_to_string(config_path())
        .unwrap_or_default()
        .lines()
        .map(|line| line.trim().to_lowercase())
        .filter(|line| !line.is_empty())
        .collect()
}

fn save_cache(store: &IndexStore, path: &Path) {
    if let Ok(bytes) = bincode::serialize(&store.to_cache()) {
        let _ = std::fs::write(path, lz4_flex::compress_prepend_size(&bytes));
    }
}

fn load_or_scan(index: &Arc<RwLock<IndexStore>>, drives: &[NtfsDrive], cache_path: &Path) -> bool {
    if !cache_path.exists() {
        return false;
    }

    let compressed = match std::fs::read(cache_path) {
        Ok(b) => b,
        Err(_) => return false,
    };
    let bytes = match lz4_flex::decompress_size_prepended(&compressed) {
        Ok(b) => b,
        Err(_) => return false,
    };
    let cache = match bincode::deserialize::<fastsearch::index::store::CacheData>(&bytes) {
        Ok(c) => c,
        Err(_) => {
            let _ = std::fs::remove_file(cache_path);
            return false;
        }
    };

    let checkpoints = cache.checkpoints.clone();
    *index.write() = IndexStore::from_cache(cache);
    if checkpoints.is_empty() {
        return true;
    }

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
            IndexEvent::Created(r) => s.insert(r),
            IndexEvent::Deleted(id) => s.remove(id),
            IndexEvent::Renamed { old_ref, new_record } => s.rename(old_ref, new_record),
            IndexEvent::Moved {
                file_ref,
                new_parent_ref,
                name,
                kind,
            } => s.apply_move(file_ref, new_parent_ref, name, kind),
        }
    }
    true
}

fn full_scan(index: &Arc<RwLock<IndexStore>>, drives: &[NtfsDrive], cache_path: &Path) {
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
            let scan = match reader.scan_direct() {
                Some(s) => s,
                None => reader.scan(),
            };
            index.write().populate_from_scan(scan, &drive.root);
        }
    }

    index.write().finalize();
    save_cache(&index.read(), cache_path);
}

fn setup_index() -> (Arc<RwLock<IndexStore>>, Arc<Mutex<Vec<JournalCheckpoint>>>) {
    let drives = get_ntfs_drives();
    let index: Arc<RwLock<IndexStore>> = Arc::new(RwLock::new(IndexStore::new()));
    let (tx, rx) = unbounded();
    let cache_path = std::env::temp_dir().join("fastseek_cache.bin");

    if !load_or_scan(&index, &drives, &cache_path) {
        full_scan(&index, &drives, &cache_path);
    }

    let live_cps: Arc<Mutex<Vec<JournalCheckpoint>>> =
        Arc::new(Mutex::new(index.read().checkpoints.clone()));

    for drive in &drives {
        let tx2 = tx.clone();
        let d2 = drive.clone();
        let cps2 = Arc::clone(&live_cps);
        std::thread::spawn(move || {
            if let Ok(mut watcher) = UsnWatcher::new(&d2, tx2) {
                watcher.run_shared(cps2);
            }
        });
    }

    let idx2 = Arc::clone(&index);
    std::thread::spawn(move || {
        for event in rx {
            let mut s = idx2.write();
            match event {
                IndexEvent::Created(r) => s.insert(r),
                IndexEvent::Deleted(id) => s.remove(id),
                IndexEvent::Renamed { old_ref, new_record } => s.rename(old_ref, new_record),
                IndexEvent::Moved {
                    file_ref,
                    new_parent_ref,
                    name,
                    kind,
                } => s.apply_move(file_ref, new_parent_ref, name, kind),
            }
        }
    });

    (index, live_cps)
}

fn toggle_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(WINDOW_LABEL) {
        match window.is_visible() {
            Ok(true) => {
                let _ = window.hide();
            }
            _ => {
                let _ = window.show();
                let _ = window.set_focus();
                let _ = app.emit("focus-search", ());
            }
        }
    }
}

fn main() {
    let (index, live_cps) = setup_index();
    let idx_for_exit = Arc::clone(&index);
    let cps_for_exit = Arc::clone(&live_cps);

    tauri::Builder::default()
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .manage(AppState {
            index,
            excluded: Mutex::new(load_exclusions()),
            case_sensitive: Mutex::new(false),
        })
        .invoke_handler(tauri::generate_handler![
            search,
            open_result,
            get_settings,
            save_settings
        ])
        .setup(move |app| {
            let toggle_item = MenuItem::with_id(app, TRAY_MENU_TOGGLE_ID, "Show / Hide", true, None::<&str>)?;
            let quit_item = MenuItem::with_id(app, TRAY_MENU_QUIT_ID, "Quit", true, None::<&str>)?;
            let tray_menu = Menu::with_items(app, &[&toggle_item, &quit_item])?;

            let app_handle_for_menu = app.handle().clone();
            let app_handle_for_tray = app.handle().clone();
            let tray_icon = app
                .default_window_icon()
                .cloned()
                .ok_or("missing default window icon")?;

            let _ = TrayIconBuilder::new()
                .icon(tray_icon)
                .menu(&tray_menu)
                .tooltip("FastSeek")
                .on_menu_event(move |_tray, event| match event.id.as_ref() {
                    TRAY_MENU_TOGGLE_ID => toggle_main_window(&app_handle_for_menu),
                    TRAY_MENU_QUIT_ID => app_handle_for_menu.exit(0),
                    _ => {}
                })
                .on_tray_icon_event(move |_tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        toggle_main_window(&app_handle_for_tray);
                    }
                })
                .build(app)?;

            app.global_shortcut()
                .register(GLOBAL_SHORTCUT)
                .map_err(|e| -> Box<dyn std::error::Error> { Box::new(e) })?;
            let app_handle = app.handle().clone();
            let _ = app.global_shortcut().on_shortcut(GLOBAL_SHORTCUT, move |_app, _sc, event| {
                if event.state == tauri_plugin_global_shortcut::ShortcutState::Pressed {
                    toggle_main_window(&app_handle);
                }
            });
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");

    let mut s = idx_for_exit.write();
    s.checkpoints = cps_for_exit.lock().clone();
    save_cache(&s, &std::env::temp_dir().join("fastseek_cache.bin"));
}
