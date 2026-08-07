#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::{
    collections::HashMap,
    ffi::c_void,
    io::{self, Write},
    mem::size_of,
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

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use windows::Win32::Graphics::Gdi::{
    GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO, BI_RGB, DIB_RGB_COLORS,
    HGDIOBJ,
};
use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL;
use windows::Win32::UI::Shell::{SHGetFileInfoW, SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON, SHGFI_USEFILEATTRIBUTES};
use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, HICON, ICONINFO};

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
    apps: Arc<Vec<AppEntry>>,
    icon_cache: Arc<Mutex<HashMap<String, String>>>,
}

#[derive(Clone, Serialize)]
struct AppEntry {
    name: String,
    path: String,
}

#[derive(Serialize)]
struct UiResult {
    name: String,
    path: String,
    is_dir: bool,
    kind: String,
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
            kind: if r.is_dir { "dir".to_string() } else { "file".to_string() },
            rank: r.rank,
        })
        .collect()
}

#[tauri::command]
fn search_apps(query: String, state: tauri::State<AppState>) -> Vec<UiResult> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return Vec::new();
    }

    let mut scored: Vec<(u8, &AppEntry)> = state
        .apps
        .iter()
        .filter_map(|app| app_rank(&app.name.to_lowercase(), &q).map(|rank| (rank, app)))
        .collect();
    scored.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.name.to_lowercase().cmp(&b.1.name.to_lowercase())));
    scored
        .into_iter()
        .take(10)
        .map(|(rank, app)| UiResult {
            name: app.name.clone(),
            path: app.path.clone(),
            is_dir: false,
            kind: "app".to_string(),
            rank,
        })
        .collect()
}

fn app_rank(name_lower: &str, q: &str) -> Option<u8> {
    if name_lower == q {
        Some(0)
    } else if name_lower.starts_with(q) {
        Some(1)
    } else if name_lower
        .split([' ', '-', '_', '.', '(', '['])
        .any(|w| w.starts_with(q))
    {
        Some(2)
    } else if name_lower.contains(q) {
        Some(3)
    } else {
        None
    }
}

#[tauri::command]
fn get_icon(path: String, state: tauri::State<AppState>) -> String {
    let key = path.to_lowercase();
    if let Some(icon) = state.icon_cache.lock().get(&key) {
        return icon.clone();
    }

    let icon = extract_icon_data_uri(&path).unwrap_or_default();
    if !icon.is_empty() {
        state.icon_cache.lock().insert(key, icon.clone());
    }
    icon
}

#[tauri::command]
fn launch_app(path: String) -> Result<(), String> {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    use windows::core::PCWSTR;

    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    let operation: Vec<u16> = "open".encode_utf16().chain(std::iter::once(0)).collect();
    let result = unsafe {
        ShellExecuteW(
            None,
            PCWSTR(operation.as_ptr()),
            PCWSTR(wide.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    if result.0 as isize <= 32 {
        Err(format!("failed to launch: {}", path))
    } else {
        Ok(())
    }
}

fn extract_icon_data_uri(path: &str) -> Option<String> {
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    let mut sfi: SHFILEINFOW = unsafe { std::mem::zeroed() };

    let flags = if Path::new(path).exists() {
        SHGFI_ICON | SHGFI_LARGEICON
    } else {
        SHGFI_ICON | SHGFI_LARGEICON | SHGFI_USEFILEATTRIBUTES
    };

    unsafe {
        SHGetFileInfoW(
            windows::core::PCWSTR(wide.as_ptr()),
            FILE_ATTRIBUTE_NORMAL,
            Some(&mut sfi),
            size_of::<SHFILEINFOW>() as u32,
            flags,
        );
        if sfi.hIcon.is_invalid() {
            return None;
        }
        let png = icon_to_png(sfi.hIcon)?;
        let _ = DestroyIcon(sfi.hIcon);
        Some(format!(
            "data:image/png;base64,{}",
            B64.encode(&png)
        ))
    }
}

fn icon_to_png(icon: HICON) -> Option<Vec<u8>> {
    unsafe {
        let mut info: ICONINFO = std::mem::zeroed();
        if GetIconInfo(icon, &mut info).is_err() {
            return None;
        }
        let hbm = info.hbmColor;

        let mut bmp: BITMAP = std::mem::zeroed();
        GetObjectW(
            HGDIOBJ(hbm.0),
            size_of::<BITMAP>() as i32,
            Some(&mut bmp as *mut _ as *mut c_void),
        );

        let w = bmp.bmWidth;
        let h = bmp.bmHeight;
        if w <= 0 || h <= 0 {
            return None;
        }

        let mut bmi: BITMAPINFO = std::mem::zeroed();
        bmi.bmiHeader.biSize = size_of::<BITMAPINFO>() as u32;
        bmi.bmiHeader.biWidth = w;
        bmi.bmiHeader.biHeight = -h;
        bmi.bmiHeader.biPlanes = 1;
        bmi.bmiHeader.biBitCount = 32;
        bmi.bmiHeader.biCompression = BI_RGB.0;

        let mut pixels = vec![0u8; (w * h * 4) as usize];
        let hdc = GetDC(None);
        if hdc.is_invalid() {
            return None;
        }
        let got = GetDIBits(
            hdc,
            hbm,
            0,
            h as u32,
            Some(pixels.as_mut_ptr() as *mut c_void),
            &mut bmi,
            DIB_RGB_COLORS,
        );
        let _ = ReleaseDC(None, hdc);
        if got == 0 {
            return None;
        }

        // BGRA -> RGBA
        let mut rgba = Vec::with_capacity(pixels.len());
        for px in pixels.chunks_exact(4) {
            rgba.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
        }

        let img = image::RgbaImage::from_raw(w as u32, h as u32, rgba)?;
        let mut png_bytes: Vec<u8> = Vec::new();
        let mut cursor = std::io::Cursor::new(&mut png_bytes);
        img.write_to(&mut cursor, image::ImageFormat::Png).ok()?;
        Some(png_bytes)
    }
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
    let apps = Arc::new(discover_apps());
    let icon_cache = Arc::new(Mutex::new(HashMap::new()));

    let state = AppState {
        index: Arc::clone(&index),
        ready: Arc::clone(&ready),
        status: Arc::clone(&status),
        apps: Arc::clone(&apps),
        icon_cache: Arc::clone(&icon_cache),
    };
    let setup_index = Arc::clone(&index);
    let setup_ready = Arc::clone(&ready);
    let setup_status = Arc::clone(&status);
    let setup_live_checkpoints = Arc::clone(&live_checkpoints);
    let close_index = Arc::clone(&index);
    let close_live_checkpoints = Arc::clone(&live_checkpoints);
    let close_window: Arc<Mutex<Option<tauri::Window>>> = Arc::new(Mutex::new(None));
    let setup_close_window = Arc::clone(&close_window);
    let event_close_window = Arc::clone(&close_window);

    tauri::Builder::default()
        .system_tray(SystemTray::new().with_menu(
            SystemTrayMenu::new()
                .add_item(CustomMenuItem::new("show".to_string(), "Show Search"))
                .add_item(CustomMenuItem::new("quit".to_string(), "Quit")),
        ))
        .manage(state)
        .setup(move |app| {
            let window = app.get_window("main").expect("main window");
            *setup_close_window.lock() = Some(window.clone());
            position_spotlight(&window);
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
                match event.event() {
                    tauri::WindowEvent::CloseRequested { .. } => {
                        save_cache_with_checkpoints(
                            &close_index,
                            &close_live_checkpoints,
                            &std::env::temp_dir().join("fastseek_cache.bin"),
                        );
                    }
                    tauri::WindowEvent::Focused(false) => {
                        if let Some(window) = event_close_window.lock().as_ref() {
                            if window.is_visible().unwrap_or(false) {
                                let _ = window.hide();
                            }
                        }
                    }
                    _ => {}
                }
            }
        })
        .on_system_tray_event(|app, event| match event {
            SystemTrayEvent::LeftClick { .. } => {
                if let Some(window) = app.get_window("main") {
                    show_spotlight(&window);
                }
            }
            SystemTrayEvent::MenuItemClick { id, .. } if id.as_str() == "show" => {
                if let Some(window) = app.get_window("main") {
                    show_spotlight(&window);
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
            search_apps,
            get_icon,
            launch_app,
            hide_window,
            open_path,
            open_parent,
            open_web_search
        ])
        .run(tauri::generate_context!())
        .expect("error while running FastSeek");
}

fn position_spotlight(window: &tauri::Window) {
    if let Some(monitor) = window.current_monitor().ok().flatten() {
        let size = monitor.size();
        let scale = monitor.scale_factor();
        let win = window.inner_size().map(|s| tauri::LogicalSize::new(
            s.width as f64 / scale,
            s.height as f64 / scale,
        )).unwrap_or(tauri::LogicalSize::new(700.0, 460.0));
        let x = (size.width as f64 - win.width * scale) / 2.0;
        let y = size.height as f64 * 0.12;
        let _ = window.set_position(tauri::LogicalPosition::new(x / scale, y / scale));
    }
}

fn show_spotlight(window: &tauri::Window) {
    position_spotlight(window);
    let _ = window.show();
    let _ = window.set_focus();
}

fn register_shortcut(app: &tauri::App, shortcut: &str, window: tauri::Window) {
    let label = shortcut.to_string();
    if let Err(e) = app.global_shortcut_manager().register(shortcut, move || {
        if window.is_visible().unwrap_or(false) {
            let _ = window.hide();
        } else {
            show_spotlight(&window);
        }
    }) {
        eprintln!("Could not register {}: {}", label, e);
    }
}

fn discover_apps() -> Vec<AppEntry> {
    let mut apps: Vec<AppEntry> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    let program_data = std::env::var("ProgramData").unwrap_or_else(|_| "C:\\ProgramData".into());
    let appdata = std::env::var("APPDATA").unwrap_or_default();
    let mut roots: Vec<String> = Vec::new();
    if !program_data.is_empty() {
        roots.push(format!(
            r"{}\Microsoft\Windows\Start Menu\Programs",
            program_data
        ));
    }
    if !appdata.is_empty() {
        roots.push(format!(
            r"{}\Microsoft\Windows\Start Menu\Programs",
            appdata
        ));
    }

    for root in roots {
        collect_shortcuts(&root, &mut apps);
    }

    // Registry install locations: find the first .exe and add as fallback
    for app in unsafe { fastsearch::index::apps::get_installed_apps() } {
        if let Some(loc) = app.install_location {
            let loc_path = Path::new(&loc);
            let exe = find_exe(loc_path, &app.name);
            if let Some(exe) = exe {
                let key = exe.to_string_lossy().to_lowercase();
                if seen.insert(key) {
                    apps.push(AppEntry {
                        name: app.name,
                        path: exe.to_string_lossy().to_string(),
                    });
                }
            }
        }
    }

    apps.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    apps
}

fn collect_shortcuts(dir: &str, out: &mut Vec<AppEntry>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_shortcuts(&path.to_string_lossy(), out);
        } else if path
            .extension()
            .map(|e| e.eq_ignore_ascii_case("lnk"))
            .unwrap_or(false)
        {
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                let name = stem.trim().to_string();
                if !name.is_empty() {
                    out.push(AppEntry {
                        name,
                        path: path.to_string_lossy().to_string(),
                    });
                }
            }
        }
    }
}

fn find_exe(dir: &Path, app_name: &str) -> Option<std::path::PathBuf> {
    if !dir.is_dir() {
        return None;
    }
    let name_lower = app_name.to_lowercase();
    let mut best: Option<std::path::PathBuf> = None;
    let mut best_score = 0u32;

    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if !path
            .extension()
            .map(|e| e.eq_ignore_ascii_case("exe"))
            .unwrap_or(false)
        {
            continue;
        }
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        let score = if stem == name_lower {
            100
        } else if stem.starts_with(&name_lower) || name_lower.starts_with(&stem) {
            50
        } else {
            0
        };
        if score > best_score {
            best_score = score;
            best = Some(path);
        }
    }
    best
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
