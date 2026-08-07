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
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, SIZE};
use windows::Win32::Graphics::Gdi::{
    DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO, BI_RGB, DIB_RGB_COLORS,
    HGDIOBJ, HBITMAP,
};
use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL;
use windows::Win32::System::Com::{
    CoInitializeEx, CoTaskMemFree, IBindCtx, COINIT_APARTMENTTHREADED,
};
use windows::Win32::UI::Shell::{
    Common::ITEMIDLIST, BHID_SFObject, BHID_SFUIObject, ILCombine, ILFree, IEnumIDList,
    IShellFolder, IShellItem, IShellItemImageFactory, SHCreateItemFromParsingName, SHGetFileInfoW,
    SHGetNameFromIDList, SHParseDisplayName, SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON,
    SHGFI_USEFILEATTRIBUTES, SHCONTF_FOLDERS, SHCONTF_INCLUDEHIDDEN, SHCONTF_NONFOLDERS,
    SIGDN_DESKTOPABSOLUTEPARSING, SIGDN_NORMALDISPLAY, SIIGBF_BIGGERSIZEOK, SIIGBF_ICONONLY,
};
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

    let mut scored: Vec<(u8, &AppEntry)> = if q.is_empty() {
        state
            .apps
            .iter()
            .map(|app| (1u8, app))
            .collect()
    } else {
        state
            .apps
            .iter()
            .filter_map(|app| app_rank(&app.name.to_lowercase(), &q).map(|rank| (rank, app)))
            .collect()
    };
    scored.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.name.to_lowercase().cmp(&b.1.name.to_lowercase())));
    scored
        .into_iter()
        .take(if q.is_empty() { 16 } else { 10 })
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
fn launch_app(path: String) -> Result<(), String> {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    use windows::core::PCWSTR;

    if let Some(rest) = path.strip_prefix("aumid:") {
        let target = format!("shell:appsFolder\\{}", rest);
        let _ = std::process::Command::new("explorer")
            .arg(&target)
            .spawn();
        return Ok(());
    }

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

#[tauri::command]
fn get_icons(paths: Vec<String>, state: tauri::State<AppState>) -> HashMap<String, String> {
    let mut out = HashMap::with_capacity(paths.len());
    let mut cache = state.icon_cache.lock();
    for path in paths {
        let key = path.to_lowercase();
        let uri = if let Some(v) = cache.get(&key) {
            v.clone()
        } else if let Some(v) = get_icon_data_uri(&path) {
            cache.insert(key, v.clone());
            v
        } else {
            continue;
        };
        out.insert(path, uri);
    }
    out
}

fn get_icon_data_uri(path: &str) -> Option<String> {
    if let Some(rest) = path.strip_prefix("aumid:") {
        return aumid_icon_data_uri(rest);
    }
    extract_icon_data_uri(path)
}

fn extract_icon_data_uri(path: &str) -> Option<String> {
    // Prefer the shell item image factory: cleaner, higher-res, no shortcut
    // arrow overlay for .lnk, real app icon for .exe.
    if let Some(uri) = shellitem_icon_data_uri(path, 64) {
        return Some(uri);
    }
    if let Some(uri) = shellitem_icon_data_uri(path, 32) {
        return Some(uri);
    }

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

fn shellitem_icon_data_uri(path: &str, size: i32) -> Option<String> {
    let wide = utf16_null(path);
    unsafe {
        let item: IShellItem = SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None).ok()?;
        let factory: IShellItemImageFactory = item
            .BindToHandler(None, &BHID_SFUIObject)
            .ok()?;
        let sb = SIZE { cx: size, cy: size };
        let hbmp = factory
            .GetImage(sb, SIIGBF_ICONONLY | SIIGBF_BIGGERSIZEOK)
            .ok()?;
        let png = hbitmap_to_png(hbmp)?;
        let _ = DeleteObject(HGDIOBJ(hbmp.0));
        Some(format!("data:image/png;base64,{}", B64.encode(&png)))
    }
}

fn icon_to_png(icon: HICON) -> Option<Vec<u8>> {
    unsafe {
        let mut info: ICONINFO = std::mem::zeroed();
        if GetIconInfo(icon, &mut info).is_err() {
            return None;
        }
        let out = hbitmap_to_png(info.hbmColor);
        let _ = DeleteObject(HGDIOBJ(info.hbmColor.0));
        let _ = DeleteObject(HGDIOBJ(info.hbmMask.0));
        out
    }
}

fn aumid_icon_data_uri(aumid: &str) -> Option<String> {
    let mut wide = "shell:appsFolder\\".encode_utf16().collect::<Vec<u16>>();
    wide.extend(aumid.encode_utf16());
    wide.push(0);
    unsafe {
        let item: IShellItem = SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None).ok()?;
        let factory: IShellItemImageFactory = item
            .BindToHandler(None, &BHID_SFUIObject)
            .ok()?;
        for size in [64, 32] {
            let sb = SIZE { cx: size, cy: size };
            if let Ok(hbmp) = factory.GetImage(sb, SIIGBF_ICONONLY | SIIGBF_BIGGERSIZEOK) {
                if let Some(png) = hbitmap_to_png(hbmp) {
                    let _ = DeleteObject(HGDIOBJ(hbmp.0));
                    return Some(format!(
                        "data:image/png;base64,{}",
                        B64.encode(&png)
                    ));
                }
                let _ = DeleteObject(HGDIOBJ(hbmp.0));
            }
        }
    }
    None
}

fn hbitmap_to_png(hbm: HBITMAP) -> Option<Vec<u8>> {
    unsafe {
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
        bmi.bmiHeader.biSize = size_of::<windows::Win32::Graphics::Gdi::BITMAPINFOHEADER>() as u32;
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

fn utf16_null(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn apps_from_shell() -> Vec<AppEntry> {
    let mut out = Vec::new();
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let w = utf16_null("shell:AppsFolder");
        let Ok(item) = SHCreateItemFromParsingName::<_, _, IShellItem>(PCWSTR(w.as_ptr()), None) else {
            return out;
        };
        let Ok(folder) = item.BindToHandler::<_, IShellFolder>(None, &BHID_SFObject) else {
            return out;
        };

        // Absolute pidl of shell:AppsFolder — child pidls from EnumObjects are
        // relative, but SHGetNameFromIDList requires an absolute one.
        let mut abs_pidl: *mut ITEMIDLIST = std::ptr::null_mut();
        if SHParseDisplayName::<_, Option<&IBindCtx>>(
            PCWSTR(w.as_ptr()),
            None,
            &mut abs_pidl,
            0,
            None,
        )
        .is_err()
            || abs_pidl.is_null()
        {
            return out;
        }

        let mut enums: Option<IEnumIDList> = None;
        let flags = (SHCONTF_FOLDERS.0 | SHCONTF_NONFOLDERS.0 | SHCONTF_INCLUDEHIDDEN.0) as u32;
        if folder.EnumObjects(HWND(std::ptr::null_mut()), flags, &mut enums).is_err() {
            let _ = ILFree(Some(abs_pidl));
            return out;
        }
        let Some(ids) = enums else {
            let _ = ILFree(Some(abs_pidl));
            return out;
        };
        loop {
            let mut pidls = [std::ptr::null_mut::<ITEMIDLIST>()];
            let mut fetched = 0u32;
            let hr = ids.Next(&mut pidls, Some(&mut fetched));
            if hr.is_err() || fetched == 0 {
                break;
            }
            let pidl = pidls[0];
            let full = ILCombine(Some(abs_pidl), Some(pidl));
            let mut name = String::new();
            let mut parsing = String::new();
            if !full.is_null() {
                if let Ok(pw) = SHGetNameFromIDList(full, SIGDN_NORMALDISPLAY) {
                    name = pw.to_string().unwrap_or_default();
                    CoTaskMemFree(Some(pw.0 as *const c_void));
                }
                if let Ok(pw) = SHGetNameFromIDList(full, SIGDN_DESKTOPABSOLUTEPARSING) {
                    parsing = pw.to_string().unwrap_or_default();
                    CoTaskMemFree(Some(pw.0 as *const c_void));
                }
                let _ = ILFree(Some(full));
            }
            let _ = ILFree(Some(pidl));
            if !name.is_empty() && !parsing.is_empty() {
                let aumid = parsing
                    .strip_prefix("shell:appsFolder\\")
                    .or_else(|| parsing.strip_prefix("shell:APPSFOLDER\\"))
                    .or_else(|| parsing.strip_prefix("shell:AppsFolder\\"))
                    .unwrap_or(&parsing)
                    .to_string();
                out.push(AppEntry {
                    name,
                    path: format!("aumid:{}", aumid),
                });
            }
        }
        let _ = ILFree(Some(abs_pidl));
    }
    out
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
    if std::env::args().any(|a| a == "--dump-apps") {
        let apps = discover_apps();
        println!("total={}", apps.len());
        for app in &apps {
            println!("{}\t{}", app.name, app.path);
        }
        std::process::exit(0);
    }

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
            get_icons,
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

    // Store / UWP apps from the Shell AppsFolder (comes with real icons).
    for app in apps_from_shell() {
        let key = norm_app_name(&app.name);
        if seen.insert(key) {
            apps.push(app);
        }
    }

    // Start Menu shortcuts: prefer these over their AppsFolder twin (icon extraction
    // is cheaper on a real .lnk path) — but keep the UWP one if it's the only copy.
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
        collect_shortcuts(&root, &mut apps, &mut seen);
    }

    // Registry Uninstall entries: any app with a DisplayName is listed; the
    // launch/icon path comes from DisplayIcon (e.g. "C:\app\app.exe,0") or, as a
    // fallback, the first .exe under InstallLocation. This surfaces installed
    // apps that have no Start Menu shortcut or Store entry.
    for app in unsafe { fastsearch::index::apps::get_installed_apps() } {
        let key = norm_app_name(&app.name);
        if seen.contains(&key) {
            continue;
        }
        if let Some(path) = registry_app_path(&app) {
            if seen.insert(key) {
                apps.push(AppEntry {
                    name: app.name,
                    path,
                });
            }
        }
    }

    // Curated system tools (Settings, Control Panel, admin consoles, ...).
    // Added last with the same name-dedupe, so they only fill names no other
    // source already provided.
    let app_root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
    let sys32 = format!(r"{}\System32", app_root);
    let sys_tools: Vec<(String, String)> = vec![
        ("Settings".to_string(), "ms-settings:".to_string()),
        (
            "Control Panel".to_string(),
            "shell:::{21EC2020-3AEA-1069-A2DD-08002B30309D}".to_string(),
        ),
        (
            "Run".to_string(),
            "shell:::{2559a1f3-21d7-11d4-bdaf-00c04f60b9f0}".to_string(),
        ),
        ("Task Manager".to_string(), format!(r"{}\Taskmgr.exe", sys32)),
        ("Device Manager".to_string(), format!(r"{}\devmgmt.msc", sys32)),
        ("Event Viewer".to_string(), format!(r"{}\eventvwr.msc", sys32)),
        ("Disk Management".to_string(), format!(r"{}\diskmgmt.msc", sys32)),
        (
            "Computer Management".to_string(),
            format!(r"{}\compmgmt.msc", sys32),
        ),
        (
            "Programs and Features".to_string(),
            format!(r"{}\appwiz.cpl", sys32),
        ),
        (
            "Network Connections".to_string(),
            format!(r"{}\ncpa.cpl", sys32),
        ),
        (
            "System Properties".to_string(),
            format!(r"{}\sysdm.cpl", sys32),
        ),
        ("Registry Editor".to_string(), format!(r"{}\regedit.exe", sys32)),
        ("Windows Explorer".to_string(), format!(r"{}\explorer.exe", sys32)),
    ];
    for (name, target) in sys_tools {
        let key = norm_app_name(&name);
        if seen.contains(&key) {
            continue;
        }
        let usable = target.starts_with("ms-settings:")
            || target.starts_with("shell:::{")
            || Path::new(&target).exists();
        if usable && seen.insert(key) {
            apps.push(AppEntry {
                name,
                path: target,
            });
        }
    }

    apps.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    apps
}
/// (strip a trailing ",icon-index" and expand env vars; only .exe/.lnk count —
/// for .ico/.dll icons we hunt for an .exe in the same folder instead), else
/// InstallLocation.
fn registry_app_path(app: &fastsearch::index::apps::InstalledApp) -> Option<String> {
    if let Some(icon) = &app.icon {
        let raw = icon.split(',').next().unwrap_or(icon).trim().trim_matches('"');
        if !raw.is_empty() {
            let expanded = expand_env(raw);
            if !expanded.is_empty() {
                let p = Path::new(&expanded);
                if p.is_file() {
                    let is_exe = p
                        .extension()
                        .map(|e| e.eq_ignore_ascii_case("exe") || e.eq_ignore_ascii_case("lnk"))
                        .unwrap_or(false);
                    if is_exe {
                        return Some(expanded);
                    }
                    if let Some(dir) = p.parent() {
                        if let Some(exe) = find_exe(dir, &app.name) {
                            return Some(exe.to_string_lossy().to_string());
                        }
                    }
                }
            }
        }
    }
    if let Some(loc) = &app.install_location {
        let exe = find_exe(Path::new(loc), &app.name)?;
        return Some(exe.to_string_lossy().to_string());
    }
    // Last resort: an .exe path found in the uninstall command lines of the
    // executable (InnoSetup/NSIS installers record their uninstaller there).
    for uninstall in [&app.uninstall_string, &app.quiet_uninstall_string] {
        if let Some(u) = uninstall {
            if let Some(exe) = exe_from_uninstall_string(u, &app.name) {
                return Some(exe);
            }
        }
    }
    None
}

/// Pull an existing `.exe` out of an UninstallString/QuietUninstallString.
/// Skips msiexec and obvious uninstaller modules (hunts for the app exe beside
/// those instead).
fn exe_from_uninstall_string(uninstall: &str, app_name: &str) -> Option<String> {
    let s = uninstall.trim();
    if s.is_empty() {
        return None;
    }
    let low = s.to_lowercase();
    if low.starts_with("msiexec") || low.starts_with("wmic") {
        return None;
    }
    let token = if let Some(rest) = s.strip_prefix('"') {
        rest.split('"').next().unwrap_or_default()
    } else {
        s.split_whitespace().next().unwrap_or_default()
    };
    if token.is_empty() {
        return None;
    }
    let expanded = expand_env(token);
    let p = Path::new(&expanded);
    if !p.is_file()
        || !p
            .extension()
            .map(|e| e.eq_ignore_ascii_case("exe"))
            .unwrap_or(false)
    {
        return None;
    }
    let stem_low = p
        .file_stem()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if stem_low.contains("unins")
        || stem_low.contains("uninst")
        || stem_low.contains("uninstall")
        || stem_low == "setup"
        || stem_low == "install"
    {
        return p
            .parent()
            .and_then(|dir| find_exe(dir, app_name))
            .map(|e| e.to_string_lossy().to_string());
    }
    Some(expanded)
}

/// Expand %VAR% tokens (e.g. %ProgramFiles%, %SystemRoot%) in a registry path.
fn expand_env(s: &str) -> String {
    let mut out = s.to_string();
    loop {
        let start = match out.find('%') {
            Some(i) => i,
            None => break,
        };
        let after = &out[start + 1..];
        let end = match after.find('%') {
            Some(i) => start + 1 + i,
            None => break,
        };
        let var = &out[start + 1..end];
        if var.is_empty() {
            out.remove(start);
            continue;
        }
        match std::env::var_os(var) {
            Some(val) => out.replace_range(start..=end, &val.to_string_lossy()),
            None => {
                out.remove(start);
            }
        }
    }
    out
}

fn collect_shortcuts(dir: &str, out: &mut Vec<AppEntry>, seen: &mut std::collections::HashSet<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_shortcuts(&path.to_string_lossy(), out, seen);
        } else if path
            .extension()
            .map(|e| e.eq_ignore_ascii_case("lnk"))
            .unwrap_or(false)
        {
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                let name = stem.trim().to_string();
                if !name.is_empty() {
                    let key = norm_app_name(&name);
                    // Never replace a packaged AUMID entry with a shortcut —
                    // the AUMID copy has the real app identity/icon.
                    if !out.iter().any(|a| norm_app_name(&a.name) == key) {
                        if seen.insert(key) {
                            out.push(AppEntry {
                                name,
                                path: path.to_string_lossy().to_string(),
                            });
                        }
                    }
                }
            }
        }
    }
}

/// Normalized key for dedupe: lowercase alphanumerics only, so "SnippingTool"
/// and "Snipping Tool" collapse to the same app (packaged twin wins, since
/// shell:AppsFolder runs first).
fn norm_app_name(name: &str) -> String {
    let mut out = String::new();
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        }
    }
    out
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
