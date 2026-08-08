#![windows_subsystem = "windows"]

use std::{
    collections::HashMap,
    ffi::c_void,
    io::{self, Write},
    mem::size_of,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

use crossbeam_channel::unbounded;
use parking_lot::{Mutex, RwLock};
use serde::Serialize;
use tauri::{
    ClipboardManager, CustomMenuItem, GlobalShortcutManager, Manager, SystemTray, SystemTrayEvent,
    SystemTrayMenu,
};

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError, HWND, SIZE};
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
    SHGetImageList, SHGetNameFromIDList, SHParseDisplayName, SHFILEINFOW, SHGFI_ICON,
    SHGFI_LARGEICON, SHGFI_PIDL, SHGFI_SYSICONINDEX, SHGFI_USEFILEATTRIBUTES, SHCONTF_FOLDERS,
    SHCONTF_INCLUDEHIDDEN, SHCONTF_NONFOLDERS, SIGDN_DESKTOPABSOLUTEPARSING, SIGDN_NORMALDISPLAY,
    SIIGBF_BIGGERSIZEOK, SIIGBF_ICONONLY, SHGSI_ICON, SHGSI_LARGEICON, SHGetStockIconInfo,
    SIID_APPLICATION, SHSTOCKICONINFO, SHIL_JUMBO,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DestroyIcon, FindWindowW, GetIconInfo, HICON, ICONINFO, SetForegroundWindow, ShowWindow, SW_RESTORE,
};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Controls::{IImageList, ILD_TRANSPARENT};

use fastsearch::{
    index::{search, store::IndexStore},
    mft::{
        reader::MftReader,
        types::IndexEvent,
        watcher::UsnWatcher,
    },
    utils::drives::get_ntfs_drives,
};

#[derive(Clone)]
struct AppState {
    index: Arc<RwLock<IndexStore>>,
    ready: Arc<AtomicBool>,
    status: Arc<RwLock<String>>,
    apps: Arc<RwLock<Vec<AppEntry>>>,
    /// Bumped whenever the app pool is swapped (install/uninstall detected),
    /// so the UI knows to re-fetch without polling the full list down.
    app_rev: Arc<AtomicU64>,
    icon_cache: Arc<Mutex<HashMap<String, String>>>,
    icon_gate: Arc<IconGate>,
    freq: Arc<Mutex<std::collections::HashMap<String, u32>>>,
    first_scan: Arc<AtomicBool>,
}

/// Simple counting semaphore so concurrent shell icon extractors never hammer
/// the COM/SHGetFileInfo plumbing from many render cycles at once.
struct IconGate {
    gate: std::sync::Mutex<u32>,
    cv: std::sync::Condvar,
}

impl IconGate {
    fn new(permits: u32) -> Self {
        Self {
            gate: std::sync::Mutex::new(permits),
            cv: std::sync::Condvar::new(),
        }
    }
    fn acquire(&self) {
        let mut g = self.gate.lock().unwrap();
        while *g == 0 {
            g = self.cv.wait(g).unwrap();
        }
        *g -= 1;
    }
    fn release(&self) {
        let mut g = self.gate.lock().unwrap();
        *g += 1;
        self.cv.notify_one();
    }
}

#[derive(Clone, Serialize)]
struct AppEntry {
    name: String,
    path: String,
    icon: Option<String>,
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
    /// True only while the very first (fresh-cache) scan is running, so the
    /// UI can show the one-time "first run" indexing screen.
    first_scan: bool,
    /// App pool version; the UI re-fetches get_all_apps when it moves.
    apps_rev: u64,
}

#[tauri::command]
fn get_index_status(state: tauri::State<AppState>) -> IndexStatus {
    IndexStatus {
        ready: state.ready.load(Ordering::Relaxed),
        count: state.index.read().len(),
        message: state.status.read().clone(),
        first_scan: state.first_scan.load(Ordering::Relaxed),
        apps_rev: state.app_rev.load(Ordering::Relaxed),
    }
}

#[tauri::command]
fn get_all_apps(state: tauri::State<AppState>) -> Vec<UiResult> {
    state
        .apps
        .read()
        .iter()
        .map(|app| UiResult {
            name: app.name.clone(),
            path: app.path.clone(),
            is_dir: false,
            kind: "app".to_string(),
            rank: 0,
        })
        .collect()
}

#[derive(Serialize)]
struct FileResults {
    items: Vec<UiResult>,
    /// Exact match count for extension-class queries (".py" lists ALL files
    /// with that extension); 0 = unknown for ordinary queries.
    total: usize,
}

#[tauri::command]
fn search_files(query: String, offset: usize, state: tauri::State<AppState>) -> FileResults {
    if !state.ready.load(Ordering::Relaxed) || query.trim().is_empty() {
        return FileResults { items: Vec::new(), total: 0 };
    }

    // Extension buckets can be stale after live journal mutations; refresh
    // once under the write lock before serving an extension search.
    if state.index.read().ext_dirty {
        state.index.write().rebuild_ext_index();
    }

    let store = state.index.read();
    let page = search::search_paged(&store, query.trim(), 100, offset, false, &[]);
    FileResults {
        total: page.total,
        items: page
            .results
            .into_iter()
            .map(|r| UiResult {
                name: r.name,
                path: r.full_path.to_string_lossy().to_string(),
                is_dir: r.is_dir,
                kind: if r.is_dir { "dir".to_string() } else { "file".to_string() },
                rank: r.rank,
            })
            .collect(),
    }
}

#[tauri::command]
fn search_apps(query: String, state: tauri::State<AppState>) -> Vec<UiResult> {
    let q = query.trim().to_lowercase();
    let freq = state.freq.lock();
    let apps = state.apps.read();

    let mut scored: Vec<(u8, u32, &AppEntry)> = if q.is_empty() {
        apps.iter()
            .map(|app| (1u8, freq.get(&app.path.to_lowercase()).copied().unwrap_or(0), app))
            .collect()
    } else {
        apps.iter()
            .filter_map(|app| {
                app_rank(&app.name.to_lowercase(), &q)
                    .map(|rank| (rank, freq.get(&app.path.to_lowercase()).copied().unwrap_or(0), app))
            })
            .collect()
    };
    drop(freq);
    // Frecency: equally-ranked apps sort by how often you actually launch
    // them this session, so "the app I open every day" floats to the top.
    scored.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| b.1.cmp(&a.1))
            .then_with(|| a.2.name.to_lowercase().cmp(&b.2.name.to_lowercase()))
    });
    scored
        .into_iter()
        .take(if q.is_empty() { 16 } else { 10 })
        .map(|(rank, _, app)| UiResult {
            name: app.name.clone(),
            path: app.path.clone(),
            is_dir: false,
            kind: "app".to_string(),
            rank,
        })
        .collect()
}

/// Periodic re-discovery of installed apps (Start Menu, AppsFolder, Uninstall
/// registry). Runs forever on a worker thread; swaps the pool in place when
/// the set of app paths changed and bumps the revision so the UI refreshes.
/// Never swaps on empty results — a transient enumeration failure must not
/// blank the launcher.
fn apps_refresh_loop(apps: Arc<RwLock<Vec<AppEntry>>>, rev: Arc<AtomicU64>) {
    loop {
        std::thread::sleep(std::time::Duration::from_secs(60));
        let fresh = discover_apps();
        if fresh.is_empty() {
            continue;
        }
        let same = {
            let cur = apps.read();
            if cur.len() != fresh.len() {
                false
            } else {
                let cur_keys: std::collections::HashSet<String> =
                    cur.iter().map(|a| a.path.to_lowercase()).collect();
                let new_keys: std::collections::HashSet<String> =
                    fresh.iter().map(|a| a.path.to_lowercase()).collect();
                cur_keys == new_keys
            }
        };
        if !same {
            *apps.write() = fresh;
            rev.fetch_add(1, Ordering::Relaxed);
            log_line("apps: pool refreshed — install/uninstall picked up");
        }
    }
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
fn launch_app(path: String, state: tauri::State<AppState>) -> Result<(), String> {
    let mut freq = state.freq.lock();
    let entry = freq.entry(path.to_lowercase()).or_insert(0);
    *entry = entry.saturating_add(1);
    drop(freq);
    launch_with_verb(&path, "open")
}

#[tauri::command]
fn launch_admin(path: String, state: tauri::State<AppState>) -> Result<(), String> {
    let mut freq = state.freq.lock();
    let entry = freq.entry(path.to_lowercase()).or_insert(0);
    *entry = entry.saturating_add(1);
    drop(freq);
    launch_with_verb(&path, "runas")
}

#[tauri::command]
fn open_properties(path: String) -> Result<(), String> {
    launch_with_verb(&path, "properties")
}

fn launch_with_verb(path: &str, verb: &str) -> Result<(), String> {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    use windows::core::PCWSTR;

    // Namespace / virtual paths (UWP, ms-settings, control-panel CLSIDs) can't
    // take elevated or properties verbs — fall back to a plain open.
    if verb != "open"
        && (path.starts_with("aumid:") || path.starts_with("ms-") || path.starts_with("shell:::"))
    {
        return launch_with_verb(path, "open");
    }

    if let Some(rest) = path.strip_prefix("aumid:") {
        let target = format!("shell:appsFolder\\{}", rest);
        let _ = std::process::Command::new("explorer")
            .arg(&target)
            .spawn();
        return Ok(());
    }

    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    let operation: Vec<u16> = verb.encode_utf16().chain(std::iter::once(0)).collect();
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

fn icon_cache_dir() -> std::path::PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("FastSeek").join("icons")
}

fn icon_disk_path(path: &str) -> std::path::PathBuf {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    path.to_lowercase().hash(&mut h);
    icon_cache_dir().join(format!("{:016x}.png", h.finish()))
}

fn png_to_uri(png: &[u8]) -> String {
    format!("data:image/png;base64,{}", B64.encode(png))
}

fn uri_png_bytes(uri: &str) -> Option<Vec<u8>> {
    let b64 = uri.strip_prefix("data:image/png;base64,")?;
    B64.decode(b64).ok()
}

#[tauri::command]
fn get_icons(paths: Vec<String>, state: tauri::State<AppState>) -> HashMap<String, String> {
    let mut out = HashMap::with_capacity(paths.len());
    let mut cache = state.icon_cache.lock();
    for path in paths {
        let key = path.to_lowercase();
        let disk = icon_disk_path(&key);
        let uri = if let Some(v) = cache.get(&key) {
            Some(v.clone())
        } else if let Ok(png) = std::fs::read(&disk) {
            // Persistent disk cache: extraction hit once, reused forever.
            let uri = png_to_uri(&png);
            cache.insert(key, uri.clone());
            Some(uri)
        } else {
            state.icon_gate.acquire();
            let v = get_icon_data_uri(&path);
            state.icon_gate.release();
            if let Some(uri) = v {
                cache.insert(key, uri.clone());
                if let Some(png) = uri_png_bytes(&uri) {
                    let _ = std::fs::create_dir_all(icon_cache_dir());
                    let _ = std::fs::write(&disk, png);
                }
                Some(uri)
            } else {
                None
            }
        };
        if let Some(uri) = uri {
            out.insert(path, uri);
        }
    }
    out
}

fn get_icon_data_uri(path: &str) -> Option<String> {
    // Shell icon handlers require an STA thread with COM initialized.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }

    let uri = if let Some(rest) = path.strip_prefix("aumid:") {
        aumid_icon_data_uri(rest).or_else(generic_icon_data_uri)
    } else if path.starts_with("ms-") {
        uri_icon_data_uri(path).or_else(generic_icon_data_uri)
    } else {
        extract_icon_data_uri(path).or_else(generic_icon_data_uri)
    };
    if uri.is_none() {
        eprintln!("[icon] no icon for {}", path);
    }
    uri
}

/// Map known URI schemes to a real app icon.
fn uri_icon_data_uri(uri: &str) -> Option<String> {
    if uri.starts_with("ms-settings") {
        return aumid_icon_data_uri(
            "windows.immersivecontrolpanel_cw5n1h2txyewy!microsoft.windows.immersivecontrolpanel",
        );
    }
    None
}

/// Fallback application icon so no row ever renders blank. Uses the shell's
/// stock APPLICATION icon — never a blank document glyph.
fn generic_icon_data_uri() -> Option<String> {
    use std::sync::OnceLock;
    static GENERIC: OnceLock<Option<String>> = OnceLock::new();
    GENERIC
        .get_or_init(|| {
            unsafe {
                let mut info: SHSTOCKICONINFO = std::mem::zeroed();
                info.cbSize = size_of::<SHSTOCKICONINFO>() as u32;
                if SHGetStockIconInfo(
                    SIID_APPLICATION,
                    SHGSI_ICON | SHGSI_LARGEICON,
                    &mut info,
                )
                .is_ok()
                    && !info.hIcon.is_invalid()
                {
                    if let Some(png) = icon_to_png(info.hIcon) {
                        let _ = DestroyIcon(info.hIcon);
                        return Some(format!(
                            "data:image/png;base64,{}",
                            B64.encode(&png)
                        ));
                    }
                    let _ = DestroyIcon(info.hIcon);
                }
            }
            for candidate in [
                "C:\\Windows\\System32\\shell32.dll,3",
                "C:\\Windows\\System32\\shell32.dll,220",
            ] {
                if let Some(uri) = extract_icon_data_uri(candidate) {
                    return Some(uri);
                }
            }
            None
        })
        .clone()
}

fn extract_icon_data_uri(path: &str) -> Option<String> {
    // Prefer the 256px system-image-list icon (crisp at any scale).
    let wide0: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let mut sfi0: SHFILEINFOW = std::mem::zeroed();
        let res0 = SHGetFileInfoW(
            PCWSTR(wide0.as_ptr()),
            FILE_ATTRIBUTE_NORMAL,
            Some(&mut sfi0),
            size_of::<SHFILEINFOW>() as u32,
            SHGFI_SYSICONINDEX,
        );
        if res0 != 0 {
            if let Some(uri) = imagelist_icon_data_uri(sfi0.iIcon) {
                return Some(uri);
            }
        }
    }

    // Fall back to the shell item image factory: cleaner, higher-res, no shortcut
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
    // SHGetFileInfoW resolves shell:AppsFolder\<AUMID> to the app's icon.
    // NOTE: SHGFI_USEFILEATTRIBUTES must NOT be used here — it skips namespace
    // resolution and returns a generic blank document icon for EVERY entry.
    let mut wide = "shell:appsFolder\\".encode_utf16().collect::<Vec<u16>>();
    wide.extend(aumid.encode_utf16());
    wide.push(0);
    unsafe {
        let mut sfi: SHFILEINFOW = std::mem::zeroed();
        SHGetFileInfoW(
            PCWSTR(wide.as_ptr()),
            FILE_ATTRIBUTE_NORMAL,
            Some(&mut sfi),
            size_of::<SHFILEINFOW>() as u32,
            SHGFI_ICON | SHGFI_LARGEICON,
        );
        if sfi.hIcon.is_invalid() {
            return None;
        }
        let png = icon_to_png(sfi.hIcon)?;
        let _ = DestroyIcon(sfi.hIcon);
        Some(format!("data:image/png;base64,{}", B64.encode(&png)))
    }
}

/// Extract the shell item's icon from the system image list at 256 px (JUMBO).
/// `index` comes from SHGetFileInfoW with SHGFI_SYSICONINDEX.
fn imagelist_icon_data_uri(index: i32) -> Option<String> {
    if index < 0 {
        return None;
    }
    unsafe {
        let list: IImageList = SHGetImageList(SHIL_JUMBO as i32).ok()?;
        let icon = list.GetIcon(index & 0xffff, ILD_TRANSPARENT.0).ok()?;
        let png = icon_to_png(icon)?;
        let _ = DestroyIcon(icon);
        Some(format!("data:image/png;base64,{}", B64.encode(&png)))
    }
}

/// Extract the real per-item icon using the item's absolute PIDL. This is the
/// reliable way for AppsFolder (Store/UWP) entries: SHGetFileInfoW with
/// SHGFI_PIDL resolves the actual shell item (not a file-type guess), and it
/// avoids both the BindToHandler E_NOINTERFACE and the USEFILEATTRIBUTES
/// "every icon looks like a document" problems. Prefers the 256px JUMBO
/// system-image-list icon, falling back to a plain 32px HICON.
fn pidl_icon_data_uri(pidl: *mut ITEMIDLIST) -> Option<String> {
    if pidl.is_null() {
        return None;
    }
    unsafe {
        let mut sfi: SHFILEINFOW = std::mem::zeroed();
        let res = SHGetFileInfoW(
            PCWSTR(pidl as *const u16),
            FILE_ATTRIBUTE_NORMAL,
            Some(&mut sfi),
            size_of::<SHFILEINFOW>() as u32,
            SHGFI_SYSICONINDEX | SHGFI_PIDL,
        );
        if res != 0 {
            if let Some(uri) = imagelist_icon_data_uri(sfi.iIcon) {
                return Some(uri);
            }
        }

        // Path 2: plain large icon from the PIDL.
        let mut sfi2: SHFILEINFOW = std::mem::zeroed();
        let res2 = SHGetFileInfoW(
            PCWSTR(pidl as *const u16),
            FILE_ATTRIBUTE_NORMAL,
            Some(&mut sfi2),
            size_of::<SHFILEINFOW>() as u32,
            SHGFI_ICON | SHGFI_LARGEICON | SHGFI_PIDL,
        );
        if res2 == 0 || sfi2.hIcon.is_invalid() {
            return None;
        }
        let png = icon_to_png(sfi2.hIcon)?;
        let _ = DestroyIcon(sfi2.hIcon);
        Some(format!("data:image/png;base64,{}", B64.encode(&png)))
    }
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
            let mut icon = None;
            if !full.is_null() {
                if let Ok(pw) = SHGetNameFromIDList(full, SIGDN_NORMALDISPLAY) {
                    name = pw.to_string().unwrap_or_default();
                    CoTaskMemFree(Some(pw.0 as *const c_void));
                }
                if let Ok(pw) = SHGetNameFromIDList(full, SIGDN_DESKTOPABSOLUTEPARSING) {
                    parsing = pw.to_string().unwrap_or_default();
                    CoTaskMemFree(Some(pw.0 as *const c_void));
                }
                icon = pidl_icon_data_uri(full);
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
                    icon,
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

/// Append a lifecycle line to %LOCALAPPDATA%\FastSeek\log.txt — the tray
/// app has no console, so this file is the only place panic/exit evidence
/// survives.
fn log_line(msg: &str) {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    if let Some(dir) = base.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(base.join("FastSeek").join("log.txt"))
        .and_then(|mut f| {
            use std::io::Write;
            writeln!(
                f,
                "[{}] pid={} {}",
                chrono_like_now(),
                std::process::id(),
                msg
            )
        });
}

/// Cheap RFC-3339-ish timestamp (std only; avoids a chrono dependency).
fn chrono_like_now() -> String {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("+{:.3}s", d.as_secs_f64())
}

fn main() {
    // Everything hits the log file, panics included — the window has no
    // console and silent exits are impossible to debug otherwise.
    std::panic::set_hook(Box::new(|info| {
        log_line(&format!("PANIC: {}", info));
    }));
    log_line("main: start");
    if std::env::args().any(|a| a == "--dump-apps") {
        let apps = discover_apps();
        let mut real = 0usize;
        let mut generic = 0usize;
        print!("total={}\n", apps.len());
        for app in &apps {
            let primary = app.icon.clone().or_else(|| {
                if app.path.starts_with("aumid:") {
                    None
                } else if app.path.starts_with("ms-") {
                    uri_icon_data_uri(&app.path)
                } else {
                    extract_icon_data_uri(&app.path)
                }
            });
            if primary.is_some() {
                real += 1;
            } else {
                generic += 1;
            }
            print!(
                "{}\t{}\t{}\n",
                app.name,
                app.path,
                if primary.is_some() { "REAL" } else { "GENERIC" }
            );
        }
        eprintln!("[icon] REAL={} GENERIC={}", real, generic);
        std::process::exit(0);
    }

    // Single instance: a second launch just wakes and focuses the running
    // window, so double-clicking the exe never spawns a second index or a
    // second hotkey registration.
    if let Some(hwnd) = ensure_single_instance() {
        unsafe {
            let _ = ShowWindow(hwnd, SW_RESTORE);
            let _ = SetForegroundWindow(hwnd);
        }
        std::process::exit(0);
    }
    log_line("main: single-instance owner");

    let index = Arc::new(RwLock::new(IndexStore::new()));
    let ready = Arc::new(AtomicBool::new(false));
    let status = Arc::new(RwLock::new(String::from("Starting...")));
    let first_scan = Arc::new(AtomicBool::new(false));
    let apps = Arc::new(RwLock::new(discover_apps()));
    let app_rev = Arc::new(AtomicU64::new(0));
    let icon_cache = Arc::new(Mutex::new(HashMap::new()));
    {
        let mut cache = icon_cache.lock();
        for app in apps.read().iter() {
            if let Some(ic) = &app.icon {
                cache.insert(app.path.to_lowercase(), ic.clone());
            }
        }
    }

    let state = AppState {
        index: Arc::clone(&index),
        ready: Arc::clone(&ready),
        status: Arc::clone(&status),
        apps: Arc::clone(&apps),
        app_rev: Arc::clone(&app_rev),
        icon_cache: Arc::clone(&icon_cache),
        icon_gate: Arc::new(IconGate::new(8)),
        freq: Arc::new(Mutex::new(std::collections::HashMap::new())),
        first_scan: Arc::clone(&first_scan),
    };

    // Live app pool: re-scan Start Menu / AppsFolder / Uninstall registry
    // every minute; swap + bump the rev only when the list actually changed
    // (an install or uninstall), so search_apps picks it up without a restart.
    {
        let apps_loop = Arc::clone(&apps);
        let rev_loop = Arc::clone(&app_rev);
        std::thread::spawn(move || apps_refresh_loop(apps_loop, rev_loop));
    }
    let setup_index = Arc::clone(&index);
    let setup_ready = Arc::clone(&ready);
    let setup_status = Arc::clone(&status);
    let close_index = Arc::clone(&index);
    let close_window: Arc<Mutex<Option<tauri::Window>>> = Arc::new(Mutex::new(None));
    let setup_close_window = Arc::clone(&close_window);
    let event_close_window = Arc::clone(&close_window);

    let app = tauri::Builder::default()
        .system_tray(SystemTray::new().with_menu(
            SystemTrayMenu::new()
                .add_item(CustomMenuItem::new("show".to_string(), "Show Search"))
                .add_item(CustomMenuItem::new("reindex".to_string(), "Re-Index Files"))
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
            // Fallback that never collides with Win+Space (language switch)
            // or Ctrl+Space (IME); registered last so the common ones win.
            register_shortcut(app, "Ctrl+Alt+Space", window.clone());

            start_backend(
                Arc::clone(&setup_index),
                Arc::clone(&setup_ready),
                Arc::clone(&setup_status),
                Arc::clone(&first_scan),
            );
            Ok(())
        })
        .on_window_event({
            move |event| {
                match event.event() {
                    tauri::WindowEvent::CloseRequested { api, .. } => {
                        // The window is a launcher — "close" means "hide".
                        // Unhandled closes destroy the window and (with the
                        // loop exiting on the last window) take the whole
                        // app down, tray and all.
                        log_line("window: CloseRequested → save+hide");
                        save_cache(&close_index, &index_cache_path());
                        if let Some(window) = event_close_window.lock().as_ref() {
                            let _ = window.hide();
                        }
                        api.prevent_close();
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
            SystemTrayEvent::MenuItemClick { id, .. } if id.as_str() == "reindex" => {
                let state = app.state::<AppState>();
                rebuild_index_impl(&state);
            }
            SystemTrayEvent::MenuItemClick { id, .. } if id.as_str() == "quit" => {
                // Save explicitly: plain exit() skips the window-close handler
                // and would lose the latest journal checkpoints.
                log_line("exit: tray quit");
                let state = app.state::<AppState>();
                save_cache(&state.index, &index_cache_path());
                app.exit(0);
            }
            _ => {}
        })
        .invoke_handler(tauri::generate_handler![
            get_index_status,
            search_files,
            search_apps,
            get_all_apps,
            get_icons,
            launch_app,
            launch_admin,
            open_properties,
            hide_window,
            open_path,
            open_parent,
            open_web_search,
            rebuild_index,
            copy_path,
            quit_app,
            file_preview,
            resize_palette
        ])
        .build(tauri::generate_context!())
        .expect("error while building FastSeek");

    log_line("run: event loop starting");
    app.run(|_handle, event| match event {
        tauri::RunEvent::ExitRequested { api, .. } => {
            // A closed window must never be able to take the tray app
            // down (the window itself now intercepts close; this catches
            // every other exit-request origin). The tray Quit and the
            // scan page's Quit button remain the only way out.
            log_line("run: ExitRequested prevented");
            api.prevent_exit();
        }
        tauri::RunEvent::Exit => log_line("run: loop exited"),
        _ => {}
    });
    log_line("run: finished");
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
                    icon: None,
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
                icon: None,
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
                                icon: None,
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
    first_scan: Arc<AtomicBool>,
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
        let cache_path = index_cache_path();
        if let Some(dir) = cache_path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        // Migrate the old %TEMP% cache once — TEMP is regularly wiped, which
        // used to silently force a full rescan on the next boot.
        if !cache_path.exists() {
            let old = std::env::temp_dir().join("fastseek_cache.bin");
            if old.exists() {
                let _ = std::fs::rename(&old, &cache_path);
            }
        }

        // One-time "install" marker: the full-screen welcome/scanning overlay
        // shows only on the very first launch. If the cache is later missing
        // or corrupt, the rescan still happens but through the status bar.
        let first_run = {
            let marker = first_run_marker();
            let fresh = !marker.exists();
            if fresh {
                if let Some(dir) = marker.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                let _ = std::fs::write(&marker, b"1");
            }
            fresh
        };

        *status.write() = String::from("Loading cached index...");
        let cache_loaded = load_cache_and_catch_up(&index, &drives, &cache_path);

        if !cache_loaded {
            if first_run {
                first_scan.store(true, Ordering::Relaxed);
            }
            build_full_index(&index, &drives, &cache_path, &status);
            first_scan.store(false, Ordering::Relaxed);
        }

        // Never present a working-looking palette over an empty index: if the
        // MFT couldn't be read (missing admin rights), stay on the scan page
        // with an actionable message instead of silently searching nothing.
        if index.read().len() == 0 {
            *status.write() = String::from(
                "No files were indexed — FastSeek needs administrator rights to read the NTFS journal. Relaunch as Administrator and press \"Try again\".",
            );
            eprintln!("Error: index has 0 files — running elevated? NTFS readable?");
            return;
        }

        ready.store(true, Ordering::Relaxed);
        log_line("index ready — watchers starting");
        *status.write() = format!("{} files indexed", index.read().len());

        // Watcher heartbeats: the watchdog below flips the app back to the
        // scan state if a journal dies mid-session (chkdsk, defrag, USN
        // journal reset) instead of silently serving stale results forever.
        let heartbeats: std::collections::HashMap<char, Arc<std::sync::atomic::AtomicU64>> =
            drives
                .iter()
                .map(|d| (d.letter, Arc::new(AtomicU64::new(now_millis() as u64))))
                .collect();

        for drive in &drives {
            let tx_clone = tx.clone();
            let drive_clone = drive.clone();
            let hb = heartbeats.get(&drive.letter).cloned().expect("heartbeat");
            thread::spawn(move || {
                let Ok(mut watcher) = UsnWatcher::new(&drive_clone, tx_clone) else {
                    hb.store(0, Ordering::Relaxed);
                    return;
                };
                watcher.run_shared(hb);
            });
        }

        // Live event applier: batches data events, and only stores a
        // Checkpoint in `store.checkpoints` once every prior event on the
        // ordered channel has been applied. Saving that checkpoint is always a
        // consistent snapshot, so nothing is lost or duplicated on restart.
        let dirty: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
        let applier_index = Arc::clone(&index);
        let applier_dirty = Arc::clone(&dirty);
        thread::spawn(move || {
            let mut pending: Vec<IndexEvent> = Vec::with_capacity(64);
            for event in &rx {
                match event {
                    IndexEvent::Checkpoint(cp) => {
                        if !pending.is_empty() {
                            applier_index.write().apply_events(std::mem::take(&mut pending));
                        }
                        let mut store = applier_index.write();
                        store.checkpoints.retain(|c| c.drive_letter != cp.drive_letter);
                        store.checkpoints.push(cp.clone());
                        applier_dirty.store(true, Ordering::Relaxed);
                    }
                    other => {
                        pending.push(other);
                        if pending.len() >= 64 {
                            applier_index.write().apply_events(std::mem::take(&mut pending));
                            applier_dirty.store(true, Ordering::Relaxed);
                        }
                    }
                }
            }
        });

        // Watchdog: if a drive's watcher heartbeat went dark (>60s), mark the
        // index stale and surface the scan page with a re-index hint.
        let wd_hb = heartbeats.clone();
        let wd_status = Arc::clone(&status);
        let wd_ready = Arc::clone(&ready);
        thread::spawn(move || {
            loop {
                thread::sleep(Duration::from_secs(15));
                let now = now_millis();
                for (letter, hb) in &wd_hb {
                    let last = hb.load(Ordering::Relaxed);
                    if last != 0 && now.saturating_sub(last) > 60_000 {
                        hb.store(0, Ordering::Relaxed);
                        wd_ready.store(false, Ordering::Relaxed);
                        *wd_status.write() = format!(
                            "File watcher on {} stopped — results may be stale. Use \"Try again\" to re-index.",
                            letter
                        );
                    }
                }
            }
        });

        // Periodic cache persistence so a hard kill never loses the USN
        // checkpoints. Only writes when the index changed since last save.
        let saver_index = Arc::clone(&index);
        let saver_dirty = Arc::clone(&dirty);
        let saver_path = cache_path.clone();
        thread::spawn(move || {
            let interval = Duration::from_secs(30);
            loop {
                thread::sleep(interval);
                if saver_dirty.swap(false, Ordering::Relaxed) {
                    save_cache(&saver_index, &saver_path);
                }
            }
        });
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
            let events: Vec<IndexEvent> = delta_rx.try_iter().collect();
            store.apply_events(events);
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
    log_line(&format!("scan begin: {} drive(s)", drives.len()));
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
    let total_drives = drives.len();
    let mut indexed = 0usize;
    for (i, drive) in drives.iter().enumerate() {
        *status.write() = format!(
            "Scanning {}: (drive {}/{})...",
            drive.letter,
            i + 1,
            total_drives
        );
        let reader: MftReader = match MftReader::open(drive) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("Scan failed on {}: {:?}", drive.letter, e);
                continue;
            }
        };

        let t_read = Instant::now();
        // FSCTL_ENUM_USN enumeration (active records only, 16 MB IOCTL
        // buffers) is the fast scan path (~6s for a 4 GB MFT). The raw
        // $MFT file read is slower and kept only as a fallback, so a
        // drive where the IOCTL misbehaves still indexes.
        let forced_direct = std::env::var_os("FASTSEEK_DIRECT").is_some();
        let (scan, method): (_, &str) = if forced_direct {
            match reader.scan_direct() {
                Some(scan) if !scan.records.is_empty() => (scan, "direct"),
                _ => (reader.scan(), "ioctl"),
            }
        } else {
            let scan = reader.scan();
            if scan.records.is_empty() {
                match reader.scan_direct() {
                    Some(direct) if !direct.records.is_empty() => (direct, "direct-fallback"),
                    _ => (scan, "ioctl"),
                }
            } else {
                (scan, "ioctl")
            }
        };
        let read_secs = t_read.elapsed().as_secs_f64();
        indexed += scan.records.len();
        *status.write() = format!(
            "Indexing {} records (drive {}/{})...",
            scan.records.len(),
            i + 1,
            total_drives
        );
        let t_index = Instant::now();
        index.write().populate_from_scan(scan, &drive.root);
        let index_secs = t_index.elapsed().as_secs_f64();
        log_line(&format!(
            "scan drive {}: {} records via {} (read+parse {:.2}s, index {:.2}s; workers {})",
            drive.letter,
            indexed,
            method,
            read_secs,
            index_secs,
            rayon::current_num_threads()
        ));
        *status.write() = format!("Indexed {indexed} files so far...");
    }

    *status.write() = "Optimizing index (sorting + buckets)...".to_string();
    let t_fin = Instant::now();
    index.write().finalize();
    let fin_secs = t_fin.elapsed().as_secs_f64();
    *status.write() = "Saving cache...".to_string();
    let t_save = Instant::now();
    save_cache(index, cache_path);
    let save_secs = t_save.elapsed().as_secs_f64();
    *status.write() = format!("{} files indexed", index.read().len());
    let total_secs = total_start.elapsed().as_secs_f64();
    log_line(&format!(
        "index ready in {:.2}s (finalize {:.2}s, save {:.2}s, drives {})",
        total_secs, fin_secs, save_secs, total_drives
    ));
    let _ = writeln!(io::stderr(), "FastSeek index ready in {:.2}s", total_secs);
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

/// Path of the one-time install marker that gates the first-run overlay.
/// Serializes every cache write (30s saver, window-close, explicit save):
/// two writers can never race over the temp file, and the rename only ever
/// publishes a fully-written cache.
static SAVE_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());
static SAVE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Path of the index cache. Lives in LOCALAPPDATA (survives Disk Cleanup),
/// not %TEMP% (which tools wipe and would silently force a full rescan).
fn index_cache_path() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("FastSeek").join("index").join("fastseek_cache.bin")
}

/// Millis since epoch — used for watcher heartbeats.
fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Returns the existing FastSeek window when an instance is already running.
fn ensure_single_instance() -> Option<HWND> {
    let mut name: Vec<u16> = "FastSeek_SingleInstance"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let name_ptr = PCWSTR(name.as_mut_ptr());
    let Ok(mutex) = (unsafe { CreateMutexW(None, true.into(), name_ptr) }) else {
        return None; // could not even create the mutex; proceed anyway
    };
    let already_running = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    std::mem::forget(mutex); // keep the name owned until process exit
    if !already_running {
        return None;
    }
    let mut title: Vec<u16> = "FastSeek".encode_utf16().chain(std::iter::once(0)).collect();
    let title_ptr = PCWSTR(title.as_mut_ptr());
    unsafe { FindWindowW(PCWSTR::null(), title_ptr).ok() }
}

#[tauri::command]
fn rebuild_index(state: tauri::State<AppState>) {
    rebuild_index_impl(&state);
}

fn rebuild_index_impl(state: &AppState) {
    state.ready.store(false, Ordering::Relaxed);
    state.first_scan.store(false, Ordering::Relaxed);
    *state.status.write() = String::from("Rebuilding index...");
    let index = Arc::clone(&state.index);
    let ready = Arc::clone(&state.ready);
    let status = Arc::clone(&state.status);
    thread::spawn(move || {
        let cache_path = index_cache_path();
        let _ = std::fs::remove_file(&cache_path);
        let drives = get_ntfs_drives();
        if drives.is_empty() {
            *status.write() = String::from("No NTFS drives found. Run as Administrator.");
            return;
        }
        build_full_index(&index, &drives, &cache_path, &status);
        if index.read().len() == 0 {
            *status.write() = String::from(
                "Rebuild finished but nothing could be read — run as Administrator and try again.",
            );
            return;
        }
        ready.store(true, Ordering::Relaxed);
        *status.write() = format!("{} files indexed", index.read().len());
    });
}

#[tauri::command]
fn copy_path(path: String, app: tauri::AppHandle) -> Result<(), String> {
    app.clipboard_manager()
        .write_text(path)
        .map_err(|e| e.to_string())
}

#[derive(Serialize)]
struct PreviewInfo {
    size: u64,
    modified_secs: u64,
    is_dir: bool,
}

/// Cheap metadata for the preview pane — one stat() per selection change,
/// called only while the pane is visible and debounced from the UI.
#[tauri::command]
fn file_preview(path: String) -> Option<PreviewInfo> {
    let md = std::fs::metadata(&path).ok()?;
    let modified_secs = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Some(PreviewInfo {
        size: md.len(),
        modified_secs,
        is_dir: md.is_dir(),
    })
}

/// Animates the palette between the wide (preview) and compact (results-only)
/// widths. JS steps this command frame-by-frame; the clamp keeps a glitchy
/// caller from blowing the window past its authored sizes. Resizing anchors
/// the LEFT edge, so every step re-centers on the monitor — this also fixes
/// the boot case where the hidden-by-default preview shrinks an 820px window.
#[tauri::command]
fn resize_palette(window: tauri::Window, width: u32) -> Result<(), String> {
    let width = width.clamp(560, 820);
    window
        .set_size(tauri::LogicalSize::new(width as f64, 520.0))
        .map_err(|e| e.to_string())?;
    position_spotlight(&window);
    Ok(())
}

#[tauri::command]
fn quit_app(app: tauri::AppHandle) {
    log_line("exit: quit_app (scan page or UI)");
    let state = app.state::<AppState>();
    save_cache(&state.index, &index_cache_path());
    app.exit(0);
}

fn first_run_marker() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("FastSeek").join("first-run-complete")
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let _guard = SAVE_LOCK.lock();
    let seq = SAVE_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_extension(format!("tmp.{}.{}", std::process::id(), seq));
    std::fs::write(&tmp, bytes)?;
    match std::fs::rename(&tmp, path) {
        Ok(_) => Ok(()),
        Err(_) => {
            let _ = std::fs::remove_file(path);
            std::fs::rename(&tmp, path)
        }
    }
}
