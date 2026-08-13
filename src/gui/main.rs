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
use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError, HWND, RECT, SIZE};
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, GetDIBits,
    GetObjectW, ReleaseDC, SelectObject, StretchBlt, BITMAP, BITMAPINFO, BI_RGB, DIB_RGB_COLORS,
    HGDIOBJ, HBITMAP, RGBQUAD, SRCCOPY,
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
    DestroyIcon, FindWindowW, GetIconInfo, GetWindowRect, HICON, ICONINFO, SetForegroundWindow,
    ShowWindow, SW_RESTORE,
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
    /// Worker pool that performs the actual shell icon extraction. The
    /// command enqueues jobs here and awaits per-path reply channels.
    icon_tx: Arc<std::sync::mpsc::Sender<IconJob>>,
    freq: Arc<Mutex<std::collections::HashMap<String, u32>>>,
    first_scan: Arc<AtomicBool>,
}

/// One icon-extraction request with its own reply channel, so the async
/// command can await per-path results without any shared mutable state.
struct IconJob {
    path: String,
    reply: std::sync::mpsc::Sender<Option<String>>,
}

/// Shell icon extraction is STA+COM bound and must never run on the Tauri
/// main thread (a sync command there freezes the whole webview for every
/// batch). A small pool of dedicated threads — each with its own COM
/// apartment — parallelizes the ~5-30ms per-icon cost so a 12-row batch
/// lands in one round trip without ever blocking the UI.
fn spawn_icon_workers(n: usize) -> Arc<std::sync::mpsc::Sender<IconJob>> {
    let (tx, rx) = std::sync::mpsc::channel::<IconJob>();
    let rx = Arc::new(Mutex::new(rx));
    for _ in 0..n {
        let rx = Arc::clone(&rx);
        std::thread::spawn(move || {
            unsafe {
                let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            }
            loop {
                let job = rx.lock().recv();
                let Ok(job) = job else { break };
                let uri = get_icon_data_uri(&job.path);
                let _ = job.reply.send(uri);
            }
        });
    }
    Arc::new(tx)
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

#[derive(Serialize, Default)]
struct FileResults {
    items: Vec<UiResult>,
    /// Exact match count for extension-class queries (".py" lists ALL files
    /// with that extension); 0 = unknown for ordinary queries.
    total: usize,
}

#[tauri::command]
async fn search_files(
    query: String,
    offset: usize,
    app: tauri::AppHandle,
) -> Result<FileResults, String> {
    // The whole-index scan is rayon-parallel but still occupies the caller
    // thread while it joins; a sync command would freeze the webview on every
    // keystroke, so run it on the blocking pool instead. (AppHandle, not
    // State<'_, _>: tauri 1.8 cannot hold a borrowed state across an await.)
    let state = app.state::<AppState>().inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        // No ready gate: the launcher stays usable while the cache loads or
        // a rebuild runs, serving whatever the shared store holds so far.
        // The RwLock serializes against the builder thread; a search during
        // the very first empty instant simply returns nothing.
        if query.trim().is_empty() {
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
    })
    .await
    .map_err(|e| e.to_string())
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
    if let Some(rest) = path.strip_prefix("aumid:") {
        // Windows refuses elevated AppModel activation of packaged apps, so
        // resolve the package's real executable and elevate THAT instead.
        return launch_uwp_elevated(rest);
    }
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

/// Run a packaged (Store/UWP) app elevated. Windows blocks elevated
/// AppModel activation, but packaged apps are ordinary Win32 exes behind the
/// package — launch_uwp_elevated maps the AUMID to its package family, finds
/// the exe in the package's install directory and elevates that file.
/// Resolution runs as a one-shot PowerShell child process (it returns when
/// done), so it costs nothing in idle RAM.
fn launch_uwp_elevated(aumid_rest: &str) -> Result<(), String> {
    let exe = aumid_exe_token(aumid_rest).ok_or_else(|| {
        format!("'{}' has no executable to elevate", aumid_rest)
    })?;
    // PFN-style AUMID ("FamilyName!AppId") carries the package family
    // inline; legacy AUMIDs (Office: "Microsoft.Office.WINWORD.EXE.15") map
    // through the Windows.Launch contract key.
    let pfn = if let Some(idx) = aumid_rest.find('!') {
        aumid_rest[..idx].to_string()
    } else {
        use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_READ};
        use winreg::RegKey;
        let sub = format!(
            r"SOFTWARE\Classes\Extensions\ContractId\Windows.Launch\PackageId\{}",
            aumid_rest
        );
        let key = RegKey::predef(HKEY_LOCAL_MACHINE)
            .open_subkey_with_flags(sub, KEY_READ)
            .ok();
        key.and_then(|k| k.get_value::<String, _>("").ok())
            .unwrap_or_default()
    };
    if pfn.is_empty() {
        return Err(format!("no package mapping for '{}'", aumid_rest));
    }
    let script = format!(
        r#"$p = Get-AppxPackage | Where-Object {{ $_.PackageFamilyName -eq '{pfn}' }} | Select-Object -First 1;
if (-not $p -or -not $p.InstallLocation) {{ exit 1 }}
$exe = Get-ChildItem -Path $p.InstallLocation -Recurse -Filter '{exe}' -ErrorAction SilentlyContinue | Select-Object -First 1;
if (-not $exe) {{ exit 1 }}
Start-Process -FilePath $exe.FullName -Verb RunAs"#
    );
    let out = std::process::Command::new("powershell")
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW — no console flash
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!("could not resolve '{exe}' in package {pfn}"))
    }
}

use std::os::windows::process::CommandExt;

/// Windows accent color as "#rrggbb", in priority order:
/// 1. Live WinRT UISettings accent (correct even when the accent is set to
///    "automatically pick from my background" — the DWM AccentColor value
///    goes stale in that mode).
/// 2. HKCU ...\DWM\AccentColor (0xAABBGGRR).
#[tauri::command]
fn get_accent_color() -> Result<Option<String>, String> {
    use std::os::windows::process::CommandExt;
    let script = r#"[void][Windows.UI.ViewManagement.UISettings,Windows.UI,ContentType=WindowsRuntime]
$u = New-Object Windows.UI.ViewManagement.UISettings
$c = $u.GetColorValue([Windows.UI.ViewManagement.UIColorType]::Accent)
'{0:X2}{1:X2}{2:X2}' -f $c.R,$c.G,$c.B"#;
    let out = std::process::Command::new("powershell")
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW — no console flash
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        let hex = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !hex.is_empty() && hex.len() == 6 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return Ok(Some(format!("#{}", hex.to_lowercase())));
        }
    }
    // Fallback: DWM AccentColor (stale in "auto" mode, but better than none).
    use winreg::enums::{HKEY_CURRENT_USER, KEY_READ};
    use winreg::RegKey;
    let key = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(r"Software\Microsoft\Windows\DWM", KEY_READ)
        .map_err(|e| e.to_string())?;
    let color: u32 = key.get_value("AccentColor").unwrap_or(0);
    let (a, r, g, b) = (
        (color >> 24) & 0xFF,
        color & 0xFF,
        (color >> 8) & 0xFF,
        (color >> 16) & 0xFF,
    );
    if a == 0 {
        Ok(None)
    } else {
        Ok(Some(format!("#{:02x}{:02x}{:02x}", r, g, b)))
    }
}

/// Shell:startup shortcut path — the autostart switch creates/removes it.
fn autostart_lnk() -> std::path::PathBuf {
    std::env::var_os("APPDATA")
        .map(std::path::PathBuf::from)
        .map(|p| p.join(r"Microsoft\Windows\Start Menu\Programs\Startup\FastSeek.lnk"))
        .unwrap_or_default()
}

#[tauri::command]
fn autostart_enabled() -> bool {
    autostart_lnk().exists()
}

/// Turn "start with Windows" on/off (a shortcut in the user's Startup
/// folder — no admin, no UAC at login, runs unelevated with the session).
#[tauri::command]
fn set_autostart(enabled: bool) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    let lnk = autostart_lnk();
    if !enabled {
        let _ = std::fs::remove_file(&lnk);
        return Ok(());
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let target = exe.to_string_lossy().to_string();
    let dir = exe
        .parent()
        .map(|d| d.to_string_lossy().to_string())
        .unwrap_or_default();
    let lnk_str = lnk.to_string_lossy().to_string();
    let script = format!(
        r#"$w = New-Object -ComObject WScript.Shell; $s = $w.CreateShortcut('{lnk_str}'); $s.TargetPath = '{target}'; $s.WorkingDirectory = '{dir}'; $s.Save()"#
    );
    let out = std::process::Command::new("powershell")
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW — no console flash
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// Base64 data URI for an image file so the preview pane can show the
/// actual picture. Capped at 16 MB — anything bigger just falls back to
/// the regular icon row.
#[tauri::command]
fn image_data(path: String) -> Result<Option<String>, String> {
    use std::io::Read;
    let mut file = std::fs::File::open(&path).map_err(|e| e.to_string())?;
    let meta = file.metadata().map_err(|e| e.to_string())?;
    if meta.len() > 16 * 1024 * 1024 {
        return Ok(None);
    }
    let ext = path
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    let mime = match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        _ => return Ok(None),
    };
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    file.read_to_end(&mut bytes).map_err(|e| e.to_string())?;
    Ok(Some(format!(
        "data:{};base64,{}",
        mime,
        base64::engine::general_purpose::STANDARD.encode(&bytes)
    )))
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
async fn get_icons(
    paths: Vec<String>,
    app: tauri::AppHandle,
) -> Result<HashMap<String, String>, String> {
    let state = app.state::<AppState>().inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let mut out = HashMap::with_capacity(paths.len());
        let mut pending: Vec<String> = Vec::new();

        // 1) In-memory cache (apps pool + this session's extractions).
        {
            let cache = state.icon_cache.lock();
            for path in &paths {
                let key = path.to_lowercase();
                if let Some(v) = cache.get(&key) {
                    out.insert(path.clone(), v.clone());
                } else {
                    pending.push(path.clone());
                }
            }
        }

        // 2) Persistent disk cache: extraction hit once, reused forever.
        let mut to_extract: Vec<String> = Vec::new();
        {
            let mut cache = state.icon_cache.lock();
            for path in pending {
                let key = path.to_lowercase();
                let disk = icon_disk_path(&key);
                if let Ok(png) = std::fs::read(&disk) {
                    let uri = png_to_uri(&png);
                    cache.insert(key, uri.clone());
                    out.insert(path, uri);
                } else {
                    to_extract.push(path);
                }
            }
        }

        // 3) Real extraction on the STA worker pool, one job per path with an
        //    independent reply channel. The UI thread never blocks here.
        for path in to_extract {
            let (reply_tx, reply_rx) = std::sync::mpsc::channel::<Option<String>>();
            let job = IconJob { path: path.clone(), reply: reply_tx };
            if state.icon_tx.send(job).is_err() {
                break; // pool gone (shutdown) — serve what we have
            }
            let Ok(Some(uri)) = reply_rx.recv_timeout(Duration::from_secs(5)) else {
                continue;
            };
            let key = path.to_lowercase();
            state.icon_cache.lock().insert(key.clone(), uri.clone());
            if let Some(png) = uri_png_bytes(&uri) {
                let _ = std::fs::create_dir_all(icon_cache_dir());
                let _ = std::fs::write(icon_disk_path(&key), png);
            }
            out.insert(path, uri);
        }
        out
    })
    .await
    .map_err(|e| e.to_string())
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

/// Hide the launcher and tell the page it happened, so the page can reset
/// (clear the query, drop the selection) while the window is STILL visible.
/// The next show then paints a fresh DOM — no lingering text, no scroll
/// reset visible on screen. Must run before hide(): a hidden WebView2
/// throttles JS, so an event sent after hiding races the next show.
fn hide_spotlight(window: &tauri::Window) {
    let _ = window.emit("spotlight-hide", ());
    let _ = window.hide();
}

/// Frameless or not, Windows gives every window a system menu and pops it
/// on Alt+Space (and Alt alone can activate it too). That collides with the
/// Alt+Space summon hotkey and looks broken on a transparent sheet, so
/// strip WS_SYSMENU — AND push the style change to the non-client frame
/// with SWP_FRAMECHANGED, without which GetWindowLong/SetWindowLong alone
/// silently leave the old frame (and the menu) in place. Re-applied on
/// every show as belt-and-braces: some full-screen/DPI paths rebuild the
/// window frame and re-assert the style.
#[cfg(windows)]
fn strip_system_menu(window: &tauri::Window) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW, SetWindowLongPtrW, SetWindowPos, GWL_STYLE, HWND_TOP, SWP_FRAMECHANGED,
        SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, WS_SYSMENU,
    };
    unsafe {
        // window.hwnd() returns tauri's windows-crate HWND (a different
        // version of the type than our direct `windows` dependency) —
        // unwrap the raw pointer and re-wrap it in ours.
        let Ok(hwnd_raw) = window.hwnd() else { return };
        let hwnd = HWND(hwnd_raw.0 as *mut core::ffi::c_void);
        let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
        if style >= 0 && (style & (WS_SYSMENU.0 as isize)) != 0 {
            SetWindowLongPtrW(hwnd, GWL_STYLE, style & !(WS_SYSMENU.0 as isize));
            let _ = SetWindowPos(
                hwnd,
                Some(HWND_TOP),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_FRAMECHANGED,
            );
        }
    }
}

#[cfg(not(windows))]
fn strip_system_menu(_window: &tauri::Window) {}

#[tauri::command]
fn hide_window(window: tauri::Window) {
    hide_spotlight(&window);
}

#[derive(Clone, serde::Serialize)]
struct BackdropGrab {
    uri: String,
    w_css: f64,
    h_css: f64,
}

/// The most recent desktop grab taken while the window was still hidden.
/// A transparent WebView2 cannot sample the desktop with CSS backdrop-filter
/// (it only sees its own page pixels), so the frontend layers this JPEG
/// behind the card and blurs it with a plain CSS filter — that is what makes
/// the blur slider visibly real. The capture MUST happen before show(), or
/// the grab would contain the app's own panel.
static BACKDROP: Mutex<Option<BackdropGrab>> = Mutex::new(None);

/// The window rect the cached grab covers, plus when it was taken. A grab
/// is reused while it is recent AND covers the same rect: rapid hotkey
/// cycles must not push a fresh screen-sized JPEG through the IPC channel
/// on every single show — while the window is hidden the renderer is
/// throttled, so dozens of multi-MB events can pile up in the queue (and
/// WebView2 keeps their decoded textures around), which is what drove RAM
/// toward 1 GB under fast open/close mashing.
static BACKDROP_AT: Mutex<Option<Instant>> = Mutex::new(None);
static BACKDROP_RECT: Mutex<Option<(i32, i32, i32, i32)>> = Mutex::new(None);
const BACKDROP_TTL_MS: u128 = 800;

/// Desktop fingerprint: a tiny 64×36 downscaled grab of the window rect,
/// reduced to a dHash (bit = "pixel brighter than its left neighbor"). If
/// the desktop behind the launcher did not change, the webview's existing
/// backdrop is still accurate — the TTL alone would still push a fresh
/// screen-sized capture + ~5 MB decode/texture through the renderer every
/// 800 ms of hotkey mashing; the perceptual gate makes that zero. A dHash
/// (not a raw checksum) tolerates cursor blinks, clock digits and noise —
/// those flip a few bits, a real desktop change flips hundreds.
static BACKDROP_THUMB: Mutex<Option<Vec<u32>>> = Mutex::new(None);
const THUMB_W: i32 = 64;
const THUMB_H: i32 = 36;
/// Max differing dHash bits (~1% of 2268) that still counts as "same
/// desktop". Meaningful visual changes flip far more.
const THUMB_MAX_DIFF_BITS: u32 = 24;

fn hamming_bits(a: &[u32], b: &[u32]) -> u32 {
    a.iter().zip(b.iter()).map(|(x, y)| (x ^ y).count_ones()).sum()
}

/// Downscale the desktop behind the window rect into a tiny thumbnail and
/// return its dHash as u32 words (63×36 = 2268 bits). None on any failure —
/// callers then fall back to a full capture.
fn thumb_dhash(window: &tauri::Window, rect: Option<(i32, i32, i32, i32)>) -> Option<Vec<u32>> {
    let (left, top, right, bottom) = rect?;
    let w = (right - left).max(1);
    let h = (bottom - top).max(1);
    let hdc_screen = unsafe { GetDC(None) };
    let hdc_mem = unsafe { CreateCompatibleDC(Some(hdc_screen)) };
    if hdc_mem.0.is_null() {
        let _ = unsafe { ReleaseDC(None, hdc_screen) };
        return None;
    }
    let bmp = unsafe { CreateCompatibleBitmap(hdc_screen, THUMB_W, THUMB_H) };
    if bmp.0.is_null() {
        let _ = unsafe { DeleteDC(hdc_mem) };
        let _ = unsafe { ReleaseDC(None, hdc_screen) };
        return None;
    }
    let old = unsafe { SelectObject(hdc_mem, HGDIOBJ(bmp.0)) };
    let ok = unsafe {
        StretchBlt(
            hdc_mem, 0, 0, THUMB_W, THUMB_H,
            Some(hdc_screen), left, top, w, h, SRCCOPY,
        )
    };
    if !ok.as_bool() {
        unsafe {
            let _ = SelectObject(hdc_mem, old);
            let _ = DeleteObject(HGDIOBJ(bmp.0));
            let _ = DeleteDC(hdc_mem);
            let _ = ReleaseDC(None, hdc_screen);
        }
        return None;
    }
    let mut bmi = BITMAPINFO {
        bmiHeader: windows::Win32::Graphics::Gdi::BITMAPINFOHEADER {
            biSize: size_of::<windows::Win32::Graphics::Gdi::BITMAPINFOHEADER>() as u32,
            biWidth: THUMB_W,
            biHeight: -THUMB_H,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        bmiColors: [RGBQUAD::default()],
    };
    let mut pixels = vec![0u8; (THUMB_W * THUMB_H * 4) as usize];
    let n = unsafe {
        GetDIBits(
            hdc_mem,
            bmp,
            0,
            THUMB_H as u32,
            Some(pixels.as_mut_ptr() as *mut std::os::raw::c_void),
            &mut bmi,
            DIB_RGB_COLORS,
        )
    };
    unsafe {
        let _ = SelectObject(hdc_mem, old);
        let _ = DeleteObject(HGDIOBJ(bmp.0));
        let _ = DeleteDC(hdc_mem);
        let _ = ReleaseDC(None, hdc_screen);
    }
    if n < 1 {
        return None;
    }
    // dHash over the BGRA pixels using approximate luminance.
    let tw = THUMB_W as usize;
    let th = THUMB_H as usize;
    let bits = (tw - 1) * th;
    let mut out = vec![0u32; bits.div_ceil(32)];
    for y in 0..th {
        for x in 0..tw - 1 {
            let i0 = (y * tw + x) * 4;
            let i1 = i0 + 4;
            let l = (pixels[i0] as u32 + pixels[i0 + 1] as u32 + pixels[i0 + 2] as u32) / 3;
            let r = (pixels[i1] as u32 + pixels[i1 + 1] as u32 + pixels[i1 + 2] as u32) / 3;
            let bit = (l > r) as u32;
            let b = y * (tw - 1) + x;
            out[b / 32] |= bit << (b % 32);
        }
    }
    Some(out)
}

/// Capture the desktop behind the window and cache it. Returns the grab and
/// whether it is a FRESH capture (the webview already holds any reused one —
/// it is never cleared on hide — so only fresh grabs need emitting). The
/// caller can push a fresh grab to the webview BEFORE the window becomes
/// visible — the JPEG decode then overlaps the still-hidden period and the user never
/// sees the previous (stale) backdrop flash in.
fn capture_backdrop(window: &tauri::Window) -> Option<(BackdropGrab, bool)> {
    // The window rect this grab would cover — also the reuse validity check.
    let rect = (|| -> Option<(i32, i32, i32, i32)> {
        let hwnd_tauri = window.hwnd().ok()?;
        let hwnd = HWND(hwnd_tauri.0 as *mut std::os::raw::c_void);
        let mut r = RECT::default();
        unsafe { GetWindowRect(hwnd, &mut r).ok()?; }
        Some((r.left, r.top, r.right, r.bottom))
    })();

    // Reuse a recent grab that still covers the same rect. The webview keeps
    // the last applied backdrop across hides, so a reused grab needs NO emit:
    // rapid open/close then costs a single capture and no IPC traffic beyond
    // the first show.
    let rect_ok = match (*BACKDROP_RECT.lock(), rect) {
        (Some(c), Some(r)) => c == r,
        _ => false,
    };
    let age_ms = match *BACKDROP_AT.lock() {
        Some(t) => t.elapsed().as_millis(),
        None => u128::MAX,
    };

    // Fast path: the grab is recent AND covers the same rect — reuse.
    if rect_ok && age_ms < BACKDROP_TTL_MS {
        if let Some(g) = BACKDROP.lock().clone() {
            return Some((g, false));
        }
    }

    // Fingerprint path: the TTL expired, so check whether the desktop under
    // the window actually changed via a tiny dHash thumbnail. Unchanged →
    // the webview's backdrop is still accurate: reuse silently and refresh
    // the timestamp, so mashing the hotkey never produces a full capture.
    if rect_ok && age_ms >= BACKDROP_TTL_MS {
        let same = match (thumb_dhash(window, rect), BACKDROP_THUMB.lock().as_ref()) {
            (Some(a), Some(b)) => hamming_bits(&a, b) <= THUMB_MAX_DIFF_BITS,
            _ => false,
        };
        if same {
            *BACKDROP_AT.lock() = Some(Instant::now());
            if let Some(g) = BACKDROP.lock().clone() {
                return Some((g, false));
            }
        }
    }

    let grab = (|| -> Result<BackdropGrab, String> {
        // tauri's HWND comes from its own pinned `windows` crate version;
        // unwrap the raw pointer and rebuild it as our crate's handle type.
        let hwnd_tauri = window.hwnd().map_err(|e| e.to_string())?;
        let hwnd = HWND(hwnd_tauri.0 as *mut std::os::raw::c_void);
        let mut rect = RECT::default();
        unsafe {
            GetWindowRect(hwnd, &mut rect).map_err(|e| e.to_string())?;
        }
        let w = (rect.right - rect.left).max(1);
        let h = (rect.bottom - rect.top).max(1);
        let scale = window.scale_factor().unwrap_or(1.0).max(1.0);

        let hdc_screen = unsafe { GetDC(None) };
        let hdc_mem = unsafe { CreateCompatibleDC(Some(hdc_screen)) };
        if hdc_mem.0.is_null() {
            let _ = unsafe { ReleaseDC(None, hdc_screen) };
            return Err("CreateCompatibleDC failed".into());
        }
        let bmp = unsafe { CreateCompatibleBitmap(hdc_screen, w, h) };
        if bmp.0.is_null() {
            let _ = unsafe { DeleteDC(hdc_mem) };
            let _ = unsafe { ReleaseDC(None, hdc_screen) };
            return Err("CreateCompatibleBitmap failed".into());
        }
        let old_bmp = unsafe { SelectObject(hdc_mem, HGDIOBJ(bmp.0)) };
        let blt = unsafe { BitBlt(hdc_mem, 0, 0, w, h, Some(hdc_screen), rect.left, rect.top, SRCCOPY) };
        if blt.is_err() {
            unsafe {
                let _ = SelectObject(hdc_mem, old_bmp);
                let _ = DeleteObject(HGDIOBJ(bmp.0));
                let _ = DeleteDC(hdc_mem);
                let _ = ReleaseDC(None, hdc_screen);
            }
            return Err("BitBlt failed".into());
        }

        // Pull the pixels out as 32bpp BGRA with top-down rows (negative height).
        let mut bmi = BITMAPINFO {
            bmiHeader: windows::Win32::Graphics::Gdi::BITMAPINFOHEADER {
                biSize: std::mem::size_of::<windows::Win32::Graphics::Gdi::BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            bmiColors: [RGBQUAD::default()],
        };
        let mut pixels = vec![0u8; (w * h * 4) as usize];
        let dib = unsafe {
            GetDIBits(
                hdc_mem,
                bmp,
                0,
                h as u32,
                Some(pixels.as_mut_ptr() as *mut std::os::raw::c_void),
                &mut bmi,
                DIB_RGB_COLORS,
            )
        };
        unsafe {
            let _ = SelectObject(hdc_mem, old_bmp);
            let _ = DeleteObject(HGDIOBJ(bmp.0));
            let _ = DeleteDC(hdc_mem);
            let _ = ReleaseDC(None, hdc_screen);
        }
        // GetDIBits returns the number of scanlines retrieved (0 = failure).
        if dib < 1 {
            return Err(format!("GetDIBits failed: {}", dib));
        }

        // BGRA → RGBA for the image crate.
        for px in pixels.chunks_exact_mut(4) {
            px.swap(0, 2);
        }
        let img = image::RgbaImage::from_raw(w as u32, h as u32, pixels)
            .ok_or_else(|| "image buffer invalid".to_string())?;
        // The backdrop is heavily blurred and dimmed — full resolution buys
        // nothing. Downscale to half before encoding: ~4× smaller payload,
        // and the browser decodes/retains a ~4× smaller texture per capture.
        let small = image::imageops::thumbnail(&img, (w / 2).max(1) as u32, (h / 2).max(1) as u32);
        let mut out = std::io::Cursor::new(Vec::new());
        // Quality 55: barely perceptible under blur(20px)+dim, ~40% smaller
        // than the old default, and the retain-path memory shrinks with it.
        let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 55);
        enc.encode_image(&small).map_err(|e| e.to_string())?;
        Ok(BackdropGrab {
            uri: format!("data:image/jpeg;base64,{}", B64.encode(out.into_inner())),
            w_css: w as f64 / scale,
            h_css: h as f64 / scale,
        })
    })();

    match grab {
        Ok(g) => {
            *BACKDROP.lock() = Some(g.clone());
            *BACKDROP_AT.lock() = Some(Instant::now());
            *BACKDROP_RECT.lock() = rect;
            *BACKDROP_THUMB.lock() = thumb_dhash(window, rect);
            Some((g, true))
        }
        Err(e) => {
            log_line(&format!("backdrop capture failed: {}", e));
            None
        }
    }
}

#[tauri::command]
fn grab_backdrop() -> Option<BackdropGrab> {
    BACKDROP.lock().clone()
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
    if let Some(rest) = path.strip_prefix("aumid:") {
        // Packaged (Store) apps live in the ACL-protected WindowsApps folder,
        // which Explorer won't open for the user. The Apps folder is the
        // canonical "file location" Windows itself offers for these.
        return std::process::Command::new("explorer")
            .arg(format!("shell:AppsFolder\\{}", rest))
            .spawn()
            .map(|_| ())
            .map_err(|e| e.to_string());
    }
    let p = Path::new(&path);
    // A bare filename (or a root like "C:\") has an empty/None parent —
    // explorer can't open "", so fall back to the path itself.
    let parent = match p.parent() {
        Some(par) if !par.as_os_str().is_empty() => par.to_path_buf(),
        _ => p.to_path_buf(),
    };
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
    log_app_pool(&apps.read());
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
        icon_tx: spawn_icon_workers(4),
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
            strip_system_menu(&window);
            position_spotlight(&window);
            if let Some(grab) = capture_backdrop(&window) {
                // Harmless if the page hasn't registered listeners yet (the
                // initial load's refreshBackdrop() covers that first show).
                let _ = window.emit("backdrop", grab);
            }
            let _ = window.show();
            let _ = window.set_focus();

            // Primary summon hotkey, configurable in Settings (Ctrl+Space or
            // Alt+Space, persisted in HKCU\Software\FastSeek\Hotkey). NB:
            // Win+Space is RESERVED by Windows 11 (language switcher) and
            // RegisterHotKey refuses it, so Super+Space is never offered.
            let hotkey = hotkey_name();
            log_line(&format!("hotkey configured: {}", hotkey));
            register_shortcut(&app.handle(), &hotkey, window.clone());

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
                            hide_spotlight(window);
                        }
                        api.prevent_close();
                    }
                    tauri::WindowEvent::Focused(false) => {
                        if let Some(window) = event_close_window.lock().as_ref() {
                            if window.is_visible().unwrap_or(false) {
                                hide_spotlight(window);
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
            grab_backdrop,
            open_path,
            open_parent,
            open_web_search,
            rebuild_index,
            copy_path,
            quit_app,
            file_preview,
            app_info,
            uninstall_app,
            backdrop_ok,
            get_accent_color,
            autostart_enabled,
            set_autostart,
            get_hotkey,
            set_hotkey,
            image_data
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

// The OS backdrop (Mica/Acrylic) is retired: it paints the ENTIRE window
// rect as one dark sheet, which turns the transparent margins around the
// card into a big black rectangle. The window is now a fully transparent
// sheet — the card's own translucent background + full shadow is the design
// (the card is width-animated inside the sheet, so a window-filling backdrop
// is impossible anyway).
#[tauri::command]
fn backdrop_ok() -> bool {
    true
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
        // Spotlight-style top placement (12% of the screen), clamped so the
        // window always fits on the display (small screens, low resolutions):
        // the bottom must stay on-screen with a small margin.
        let top = size.height as f64 * 0.12;
        let max_top = (size.height as f64 - win.height * scale - 12.0).max(0.0);
        let y = top.min(max_top);
        let _ = window.set_position(tauri::LogicalPosition::new(x / scale, y / scale));
    }
}

fn show_spotlight(window: &tauri::Window) {
    strip_system_menu(window);
    position_spotlight(window);
    if let Some((grab, fresh)) = capture_backdrop(window) {
        // Reused grabs are already applied in the webview (it is never
        // cleared on hide) — only a genuinely fresh capture is emitted.
        if fresh {
            let _ = window.emit("backdrop", grab);
        }
    }
    let _ = window.show();
    let _ = window.set_focus();
}

/// Persisted summon-hotkey choice, HKCU\Software\FastSeek\Hotkey (REG_SZ).
fn hotkey_name() -> String {
    use winreg::enums::{HKEY_CURRENT_USER, KEY_READ};
    use winreg::RegKey;
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(r"Software\FastSeek", KEY_READ)
        .and_then(|k| k.get_value("Hotkey"))
        .unwrap_or_else(|_| "ctrl+space".to_string())
}

fn set_hotkey_name(name: &str) {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;
    if let Ok((key, _)) =
        RegKey::predef(HKEY_CURRENT_USER).create_subkey(r"Software\FastSeek")
    {
        let _ = key.set_value("Hotkey", &name.to_string());
    }
}

#[tauri::command]
fn get_hotkey() -> String {
    hotkey_name()
}

/// Switch the summon hotkey live: unregister the old combo, register the
/// new one, persist the choice so the next boot uses it too.
#[tauri::command]
fn set_hotkey(app: tauri::AppHandle, name: String) -> Result<(), String> {
    if name != "ctrl+space" && name != "alt+space" {
        return Err(format!("unsupported hotkey: {}", name));
    }
    let current = hotkey_name();
    if current == name {
        return Ok(());
    }
    let _ = app.global_shortcut_manager().unregister(&current);
    let Some(window) = app.get_window("main") else {
        return Err("main window not found".to_string());
    };
    register_shortcut(&app, &name, window);
    set_hotkey_name(&name);
    Ok(())
}

fn register_shortcut(app: &tauri::AppHandle, shortcut: &str, window: tauri::Window) {
    let label = shortcut.to_string();
    let handler = {
        let window = window.clone();
        move || {
            if window.is_visible().unwrap_or(false) {
                hide_spotlight(&window);
            } else {
                show_spotlight(&window);
            }
        }
    };
    match app.global_shortcut_manager().register(&label, handler) {
        Ok(()) => log_line(&format!("hotkey registered: {}", label)),
        Err(e) => {
            // The GUI has no console, so a silent eprintln would be invisible.
            // Log it, then retry on the main thread a few times — transient
            // conflicts (IME, another app briefly holding the combo) clear
            // within a couple of seconds.
            log_line(&format!("hotkey {} register failed: {}; retrying", label, e));
            let handle = app.clone();
            let label2 = label;
            std::thread::spawn(move || {
                for attempt in 1..=3u32 {
                    std::thread::sleep(Duration::from_secs(2));
                    let handle_inner = handle.clone();
                    let label3 = label2.clone();
                    let _ = handle_inner.run_on_main_thread({
                        let handle_inner = handle_inner.clone();
                        move || {
                            let Some(window) = handle_inner.get_window("main") else {
                                return;
                            };
                            let handler = {
                                let window = window.clone();
                                move || {
                                    if window.is_visible().unwrap_or(false) {
                                        hide_spotlight(&window);
                                    } else {
                                        show_spotlight(&window);
                                    }
                                }
                            };
                            match handle_inner.global_shortcut_manager().register(&label3, handler) {
                                Ok(()) => log_line(&format!("hotkey {} registered (retry {})", label3, attempt)),
                                Err(e) => log_line(&format!("hotkey {} retry {} failed: {}", label3, attempt, e)),
                            }
                        }
                    });
                }
            });
        }
    }
}

fn discover_apps() -> Vec<AppEntry> {
    let mut apps: Vec<AppEntry> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Store / UWP apps from the Shell AppsFolder (comes with real icons).
    for app in apps_from_shell() {
        if is_shell_junk(&app.name, &app.path) {
            continue;
        }
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
            if is_installer_junk(&path) {
                continue;
            }
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
        // Settings deep links. Static strings only — no scanning, no index
        // growth, a few KB of RAM for the whole list.
        ("Settings: Wi-Fi".to_string(), "ms-settings:wifi".to_string()),
        (
            "Settings: Bluetooth".to_string(),
            "ms-settings:bluetooth".to_string(),
        ),
        ("Settings: Display".to_string(), "ms-settings:display".to_string()),
        ("Settings: Sound".to_string(), "ms-settings:sound".to_string()),
        (
            "Settings: Network & Internet".to_string(),
            "ms-settings:network".to_string(),
        ),
        ("Settings: Ethernet".to_string(), "ms-settings:ethernet".to_string()),
        (
            "Settings: Mobile Hotspot".to_string(),
            "ms-settings:mobilehotspot".to_string(),
        ),
        ("Settings: VPN".to_string(), "ms-settings:vpndialup".to_string()),
        (
            "Settings: Windows Update".to_string(),
            "ms-settings:windowsupdate".to_string(),
        ),
        (
            "Settings: Installed Apps".to_string(),
            "ms-settings:appsfeatures".to_string(),
        ),
        (
            "Settings: Startup Apps".to_string(),
            "ms-settings:startupapps".to_string(),
        ),
        (
            "Settings: Default Apps".to_string(),
            "ms-settings:defaultapps".to_string(),
        ),
        (
            "Settings: Notifications".to_string(),
            "ms-settings:notifications".to_string(),
        ),
        ("Settings: Storage".to_string(), "ms-settings:storage".to_string()),
        (
            "Settings: Multitasking".to_string(),
            "ms-settings:multitasking".to_string(),
        ),
        ("Settings: About".to_string(), "ms-settings:about".to_string()),
        (
            "Settings: Keyboard".to_string(),
            "ms-settings:keyboard".to_string(),
        ),
        ("Settings: Mouse".to_string(), "ms-settings:mouse".to_string()),
        ("Settings: Typing".to_string(), "ms-settings:typing".to_string()),
        ("Settings: Touchpad".to_string(), "ms-settings:touchpad".to_string()),
        (
            "Settings: Personalization".to_string(),
            "ms-settings:personalization".to_string(),
        ),
        (
            "Settings: Lock Screen".to_string(),
            "ms-settings:lockscreen".to_string(),
        ),
        ("Settings: Taskbar".to_string(), "ms-settings:taskbar".to_string()),
        (
            "Settings: Sign-in Options".to_string(),
            "ms-settings:signinoptions".to_string(),
        ),
        (
            "Settings: Accounts".to_string(),
            "ms-settings:accounts".to_string(),
        ),
        (
            "Settings: Date & Time".to_string(),
            "ms-settings:dateandtime".to_string(),
        ),
        (
            "Settings: Region & Language".to_string(),
            "ms-settings:regionlanguage".to_string(),
        ),
        ("Settings: Gaming".to_string(), "ms-settings:gaming".to_string()),
        (
            "Settings: Game Bar".to_string(),
            "ms-settings:gaming-gamebar".to_string(),
        ),
        (
            "Settings: Accessibility".to_string(),
            "ms-settings:accessibility".to_string(),
        ),
        (
            "Settings: Camera Privacy".to_string(),
            "ms-settings:privacy-webcam".to_string(),
        ),
        (
            "Settings: Microphone Privacy".to_string(),
            "ms-settings:privacy-microphone".to_string(),
        ),
        (
            "Settings: Power & Battery".to_string(),
            "ms-settings:powersleep".to_string(),
        ),
        (
            "Settings: Battery Saver".to_string(),
            "ms-settings:batterysaver".to_string(),
        ),
        (
            "Settings: Night Light".to_string(),
            "ms-settings:nightlight".to_string(),
        ),
        (
            "Settings: Wallpaper".to_string(),
            "ms-settings:personalization-background".to_string(),
        ),
        ("Settings: Colors".to_string(), "ms-settings:colors".to_string()),
        ("Settings: Themes".to_string(), "ms-settings:themes".to_string()),
        ("Settings: Fonts".to_string(), "ms-settings:fonts".to_string()),
        (
            "Settings: Clipboard".to_string(),
            "ms-settings:clipboard".to_string(),
        ),
        (
            "Settings: Focus Assist".to_string(),
            "ms-settings:quiethours".to_string(),
        ),
        ("Settings: Phone Link".to_string(), "ms-settings:phone".to_string()),
        (
            "Settings: Printers & Scanners".to_string(),
            "ms-settings:printers".to_string(),
        ),
        (
            "Settings: Location Privacy".to_string(),
            "ms-settings:privacy-location".to_string(),
        ),
        ("Settings: Backup".to_string(), "ms-settings:backup".to_string()),
        (
            "Settings: Troubleshoot".to_string(),
            "ms-settings:troubleshoot".to_string(),
        ),
        ("Settings: Recovery".to_string(), "ms-settings:recovery".to_string()),
        (
            "Settings: Activation".to_string(),
            "ms-settings:activation".to_string(),
        ),
        (
            "Settings: Developer Mode".to_string(),
            "ms-settings:developers".to_string(),
        ),
        (
            "Settings: Windows Security".to_string(),
            "ms-settings:windowsdefender".to_string(),
        ),
        ("Settings: Search".to_string(), "ms-settings:search".to_string()),
        (
            "Settings: Optional Features".to_string(),
            "ms-settings:optionalfeatures".to_string(),
        ),
        (
            "Settings: Project to This PC".to_string(),
            "ms-settings:project".to_string(),
        ),
        (
            "Settings: Email & Accounts".to_string(),
            "ms-settings:emailandaccounts".to_string(),
        ),
        (
            "Settings: Time & Language".to_string(),
            "ms-settings:time-language".to_string(),
        ),
        (
            "Settings: Airplane Mode".to_string(),
            "ms-settings:network-airplanemode".to_string(),
        ),
        (
            "Settings: Data Usage".to_string(),
            "ms-settings:network-datausage".to_string(),
        ),
        (
            "Settings: Advanced Network".to_string(),
            "ms-settings:network-advancedsettings".to_string(),
        ),
        (
            "Settings: Network Status".to_string(),
            "ms-settings:network-status".to_string(),
        ),
        ("Settings: Proxy".to_string(), "ms-settings:network-proxy".to_string()),
        (
            "Settings: Bluetooth Devices".to_string(),
            "ms-settings:bluetoothdevices".to_string(),
        ),
        (
            "Settings: Diagnostics & Feedback".to_string(),
            "ms-settings:diagnostics".to_string(),
        ),
        (
            "Settings: Activity History".to_string(),
            "ms-settings:privacy-activityhistory".to_string(),
        ),
        (
            "Settings: Privacy".to_string(),
            "ms-settings:privacy".to_string(),
        ),
        ("Settings: Speech".to_string(), "ms-settings:speech".to_string()),
        (
            "Settings: Voice Typing".to_string(),
            "ms-settings:speech-typing".to_string(),
        ),
        (
            "Settings: Remote Desktop".to_string(),
            "ms-settings:remote-desktop".to_string(),
        ),
        (
            "Settings: Work or School".to_string(),
            "ms-settings:workplace".to_string(),
        ),
        (
            "Settings: Other Users".to_string(),
            "ms-settings:otherusers".to_string(),
        ),
        (
            "Settings: Captures".to_string(),
            "ms-settings:gaming-captures".to_string(),
        ),
        (
            "Settings: Broadcasting".to_string(),
            "ms-settings:gaming-broadcasting".to_string(),
        ),
        (
            "Settings: Advanced Display".to_string(),
            "ms-settings:display-advanced".to_string(),
        ),
        (
            "Settings: Graphics".to_string(),
            "ms-settings:display-graphics".to_string(),
        ),
        (
            "Settings: Windows Insider".to_string(),
            "ms-settings:windowsinsider".to_string(),
        ),
        (
            "Settings: Storage Sense".to_string(),
            "ms-settings:storagesense".to_string(),
        ),
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

    // Name-dedupe above is per-source; a final pass collapses entries that
    // point at the SAME executable but have different display names (e.g.
    // Start Menu "Word.lnk" + registry "WINWORD", or "Antigravity IDE" vs
    // "Antigravity IDE (User)"). The better-looking name wins.
    dedupe_exact_path(&mut apps);
    dedupe_by_target(&mut apps);

    apps.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    apps
}

/// Canonical target key for an app entry: resolve .lnk shortcuts to their
/// executable; keep .exe paths as-is; UWP (aumid:) / settings (ms-) / shell
/// folder links can't be resolved — they are never collapsed.
fn app_target_exe(app: &AppEntry) -> Option<String> {
    let p = app.path.trim();
    let lower = p.to_lowercase();
    // Curated system-tool entries carry a real exe path behind the aumid:
    // prefix ("aumid:C:\...\python.exe") — use the path itself as the key.
    let path: String = if let Some(rest) = lower.strip_prefix("aumid:") {
        if let Some(expanded) = expand_aumid_path(rest) {
            expanded
        } else if rest.ends_with(".exe") {
            rest.to_string()
        } else {
            return None;
        }
    } else {
        p.to_string()
    };
    let pl = path.to_lowercase();
    if pl.ends_with(".lnk") {
        resolve_lnk_target(&path)
    } else if pl.ends_with(".exe") {
        Some(path)
    } else {
        None
    }
}

/// Well-known folder SIDs that AppsFolder aumid paths are relative to
/// ("aumid:{1AC14E77-...}\mdsched.exe" = System32\mdsched.exe). Resolving
/// them gives real paths, so the same exe discovered via a Start Menu .lnk
/// collapses onto the aumid entry (Memory Diagnostics Tool == Windows Memory
/// Diagnostic). Verified against the live machine: System32/SysWOW64/
/// Program Files.
fn expand_aumid_path(rest: &str) -> Option<String> {
    let (sid, tail) = rest.split_once('\\')?;
    let windir = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into());
    let prog = std::env::var("ProgramFiles").unwrap_or_else(|_| r"C:\Program Files".into());
    let base = if sid.eq_ignore_ascii_case("{1ac14e77-02e7-4e5d-b744-2eb1ae5198b7}") {
        Some(format!(r"{}\System32", windir))
    } else if sid.eq_ignore_ascii_case("{d65231b0-b2f1-4857-a4ce-a8e7c6ea7d27}") {
        Some(format!(r"{}\SysWOW64", windir))
    } else if sid.eq_ignore_ascii_case("{6d809377-6af0-444b-8957-a3773f02200e}") {
        Some(prog)
    } else {
        None
    }?;
    Some(format!(r"{}\{}", base, tail))
}

/// Normalize a resolved target for dedupe keys: canonicalize (long names,
/// ~1 short names, dot-dot segments, case) with a raw-path fallback when the
/// file no longer exists. Registry paths and shortcut targets for the same
/// exe then collapse even if one of them was written with short names.
fn canonical_target(p: &str) -> String {
    let lower = p.to_lowercase();
    std::fs::canonicalize(p)
        .map(|c| c.to_string_lossy().to_lowercase())
        .unwrap_or(lower)
}

/// Which display name is worth keeping when two entries share a target:
/// prefer mixed-case over ALL-CAPS ("Word" > "WINWORD"), names without a
/// "(User)" profile suffix, shorter names, human-readable titles (spaces)
/// over exe-stem names, and Start Menu shortcuts over registry-sourced
/// paths.
fn app_name_score(app: &AppEntry) -> i32 {
    let name = &app.name;
    let mut score = 0;
    let upper = name.chars().filter(|c| c.is_ascii_uppercase()).count();
    let lower = name.chars().filter(|c| c.is_ascii_lowercase()).count();
    if upper > 0 && lower > 0 {
        score += 4; // mixed case: human-friendly title
    } else if upper > 0 && lower == 0 {
        score -= 4; // ALL-CAPS: exe-name style shortcut
    }
    let nlower = name.to_lowercase();
    if nlower.contains("(user)") || nlower.contains("- user") || nlower.ends_with(" user") {
        score -= 6;
    }
    score -= (name.chars().count() as i32) / 8;
    if app.path.to_lowercase().ends_with(".lnk") {
        score += 2; // Start Menu copy has the real identity
    }
    // A name that is literally the exe stem ("winword") is an internal name
    // — the human-readable twin ("Microsoft Word") should win the collapse.
    if let Some(stem) = app_target_exe(app).and_then(|t| {
        Path::new(&t)
            .file_stem()
            .map(|s| s.to_string_lossy().to_lowercase())
    }) {
        let compact: String = name
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
            .to_lowercase();
        if compact == stem {
            score -= 8;
        } else if name.contains(' ') {
            score += 3; // human-readable title, not a raw file name
        }
    }
    score
}

/// Some apps register the exact same entry twice (e.g. ZCode appears as two
/// identical aumid registrations) — keep the first copy per lowercase path.
fn dedupe_exact_path(apps: &mut Vec<AppEntry>) {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    apps.retain(|a| seen.insert(a.path.to_lowercase()));
}

/// Collapse entries that resolve to the same executable. Two layers:
/// 1. full canonical target path — Start Menu .lnk targets vs registry .exe
///    paths vs aumid:-entries that carry a real path behind the prefix;
/// 2. executable FILE NAME, bridged ONLY through Store-style aumids that
///    embed an exe name ("Microsoft.Office.WINWORD.EXE.15" → "winword.exe").
///    That collapses the classic Word/WINWORD twin while never merging two
///    real exe paths that merely share a file name (Chrome Stable vs Beta).
fn dedupe_by_target(apps: &mut Vec<AppEntry>) {
    let mut seen_path: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    // name key → (index, entry key came from an aumid exe token)
    let mut seen_name: std::collections::HashMap<String, (usize, bool)> =
        std::collections::HashMap::new();
    let mut keep = vec![true; apps.len()];
    for i in 0..apps.len() {
        let p = apps[i].path.trim();
        let pl = p.to_lowercase();
        let (path_key, name_key, name_is_aumid): (Option<String>, Option<String>, bool) =
            if let Some(rest) = pl.strip_prefix("aumid:") {
                if let Some(expanded) = expand_aumid_path(rest) {
                    let el = expanded.to_lowercase();
                    if el.ends_with(".exe") {
                        (
                            Some(canonical_target(&expanded)),
                            file_name_key(&expanded),
                            false,
                        )
                    } else if el.ends_with(".lnk") {
                        match resolve_lnk_target(&expanded) {
                            Some(t) => (Some(canonical_target(&t)), file_name_key(&t), false),
                            None => (None, None, false),
                        }
                    } else {
                        (None, None, false)
                    }
                } else if rest.ends_with(".exe") {
                    (Some(canonical_target(rest)), file_name_key(rest), false)
                } else {
                    (None, aumid_exe_token(rest), true)
                }
            } else if pl.ends_with(".lnk") {
                match resolve_lnk_target(p) {
                    Some(t) => (Some(canonical_target(&t)), file_name_key(&t), false),
                    None => (None, None, false),
                }
            } else if pl.ends_with(".exe") {
                (Some(canonical_target(p)), file_name_key(p), false)
            } else {
                (None, None, false)
            };

        let mut collided: Option<usize> = None;
        if let Some(k) = &path_key {
            if let Some(&j) = seen_path.get(k) {
                collided = Some(j);
            }
        }
        if collided.is_none() {
            if let Some(k) = &name_key {
                if let Some(&(j, aumid)) = seen_name.get(k) {
                    if aumid || name_is_aumid {
                        collided = Some(j);
                    }
                }
            }
        }
        match collided {
            None => {
                if let Some(k) = &path_key {
                    seen_path.insert(k.clone(), i);
                }
                if let Some(k) = &name_key {
                    seen_name.insert(k.clone(), (i, name_is_aumid));
                }
            }
            Some(j) => {
                let winner =
                    if app_name_score(&apps[i]) > app_name_score(&apps[j]) { i } else { j };
                keep[if winner == i { j } else { i }] = false;
                if let Some(k) = &path_key {
                    seen_path.insert(k.clone(), winner);
                }
                if let Some(k) = &name_key {
                    seen_name.insert(k.clone(), (winner, name_is_aumid));
                }
            }
        }
    }
    let mut idx = 0usize;
    apps.retain(|_| {
        let k = keep[idx];
        idx += 1;
        k
    });
}

/// Lowercased exe file name of a path ("C:\...\winword.exe" → "winword.exe").
fn file_name_key(p: &str) -> Option<String> {
    Path::new(p)
        .file_name()
        .map(|s| s.to_string_lossy().to_lowercase())
}

/// Store-style aumids embed the exe name ("Microsoft.Office.WINWORD.EXE.15",
/// "Microsoft.Office.POWERPNT.EXE.15") — extract it so the classic desktop
/// twin (registry WINWORD.EXE) can collapse onto the aumid entry.
fn aumid_exe_token(aumid: &str) -> Option<String> {
    let low = aumid.to_lowercase();
    let mut start = 0usize;
    while let Some(pos) = low[start..].find(".exe") {
        let abs = start + pos;
        let after = low[abs + 4..].chars().next();
        if after.is_none() || after == Some('.') {
            let stem_start = low[..abs].rfind('.').map(|d| d + 1).unwrap_or(0);
            let stem = &low[stem_start..abs];
            if !stem.is_empty()
                && stem
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            {
                return Some(format!("{}.exe", stem));
            }
        }
        start = abs + 4;
    }
    None
}

/// Installer/shell noise Windows lists as apps but a launcher should not:
/// URL shortcuts (nodejs.org docs/website, Windows Kits .url samples),
/// web-page links (Git Release Notes, Python docs) and installer entry
/// points ("Install Additional Tools for Node.js", "Uninstall Node.js",
/// "* command prompt" shells).
fn is_shell_junk(name: &str, path: &str) -> bool {
    let pl = path.to_lowercase();
    if let Some(rest) = pl.strip_prefix("aumid:") {
        if rest.starts_with("http")
            || rest.ends_with(".url")
            || rest.ends_with(".html")
            || rest.ends_with(".htm")
        {
            return true;
        }
    }
    let lower = name.to_lowercase();
    lower.starts_with("uninstall ")
        || lower.starts_with("install ")
        || lower.ends_with(" command prompt")
}

/// Shortcut-target noise: a .lnk resolving to a shell interpreter, an
/// uninstaller stub, or a web page (.url/.html/URL links like "Git Release
/// Notes", "Samples for Desktop Apps", "Python Manuals") is not a launchable
/// app.
fn is_shortcut_target_junk(target: &str) -> bool {
    let tl = target.to_lowercase();
    if tl.starts_with("http")
        || tl.ends_with(".url")
        || tl.ends_with(".html")
        || tl.ends_with(".htm")
    {
        return true;
    }
    let Some(stem) = Path::new(target)
        .file_stem()
        .map(|s| s.to_string_lossy().to_lowercase())
    else {
        return true;
    };
    matches!(stem.as_str(), "cmd" | "powershell" | "pwsh" | "wscript" | "cscript")
        || stem.contains("unins")
        || stem.contains("uninst")
        || stem.contains("uninstall")
}

/// One-time debug dump of the discovered pool — name | path | resolved target
/// | dedupe score — so duplicate pairs can be diagnosed straight from log.txt
/// instead of guessing. Removed once the remaining dupes are resolved.
fn log_app_pool(apps: &[AppEntry]) {
    log_line(&format!("app pool: {} entries", apps.len()));
    for a in apps {
        log_line(&format!(
            "app-pool | {} | {} | target={} | score={}",
            a.name,
            a.path,
            app_target_exe(a).unwrap_or_default(),
            app_name_score(a)
        ));
    }
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
                    if is_shell_junk(&name, "") {
                        continue;
                    }
                    let path_str = path.to_string_lossy().to_string();
                    if let Some(target) = resolve_lnk_target(&path_str) {
                        if is_shortcut_target_junk(&target) {
                            continue;
                        }
                    }
                    let key = norm_app_name(&name);
                    // Never replace a packaged AUMID entry with a shortcut —
                    // the AUMID copy has the real app identity/icon.
                    if !out.iter().any(|a| norm_app_name(&a.name) == key) {
                        if seen.insert(key) {
                            out.push(AppEntry {
                                name,
                                path: path_str,
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
/// shell:AppsFolder runs first). Version/arch/channel noise is folded so
/// twins share a key ("Antigravity" vs "Antigravity 2.1.4", "Opera Browser"
/// vs "Opera Stable 133.0.5932.85", "Python 3.13 (64-bit)" vs
/// "Python 3.13.7 (64-bit)") — while real distinguishers (x86 vs x64
/// PowerShell, "Outlook (classic)", versioned SDKs) are kept.
fn norm_app_name(name: &str) -> String {
    let mut n = name.trim().to_string();
    let lower = n.to_lowercase();
    if lower.ends_with(" - shortcut") {
        n.truncate(n.len() - " - shortcut".len());
    }
    for suffix in ["(user)", "(machine)", " - user", " - machine"] {
        if n.to_lowercase().ends_with(suffix) {
            n.truncate(n.len() - suffix.len());
            break;
        }
    }
    let toks: Vec<&str> = n.split_whitespace().collect();
    let mut end = toks.len();
    loop {
        if end == 0 {
            break;
        }
        let tl = toks[end - 1].to_lowercase();
        if tl == "-" || tl == "stable" || tl == "browser" {
            end -= 1;
        } else if is_version_token(toks[end - 1]) {
            end -= 1;
        } else if is_arch_token(&tl) && end >= 2 && is_version_token(toks[end - 2]) {
            end -= 2;
        } else {
            break;
        }
    }
    let mut out = String::new();
    for tok in toks.iter().take(end) {
        for ch in tok.chars() {
            if ch.is_ascii_alphanumeric() {
                out.push(ch.to_ascii_lowercase());
            }
        }
    }
    out
}

/// "3.13.7", "11.0.61030" — a dotted numeric version. "2012" (a year) and
/// plain numbers like "11" are NOT versions: they distinguish real products.
fn is_version_token(tok: &str) -> bool {
    tok.split('.').count() >= 2 && tok.chars().all(|c| c.is_ascii_digit() || c == '.')
}

fn is_arch_token(tok: &str) -> bool {
    matches!(tok, "(x86)" | "(x64)" | "(arm64)" | "(32-bit)" | "(64-bit)")
}

/// Registry entries that resolve to installer machinery (ProgramData\Package
/// Cache bootstrappers, uninstaller stubs) are not launchable apps — keep the
/// pool clean of them.
fn is_installer_junk(path: &str) -> bool {
    let low = path.to_lowercase();
    if low.contains("\\package cache\\") || low.ends_with("\\package cache") {
        return true;
    }
    if let Some(stem) = Path::new(path)
        .file_stem()
        .map(|s| s.to_string_lossy().to_lowercase())
    {
        if stem.contains("unins") || stem.contains("uninst") || stem.contains("uninstall") {
            return true;
        }
    }
    false
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
            let Some(store) = IndexStore::from_cache(cache) else {
                // Old cache format (pre-v2) or structurally corrupt — a
                // format bump intentionally invalidates previous caches.
                let _ = std::fs::remove_file(cache_path);
                *index.write() = IndexStore::new();
                return false;
            };
            *index.write() = store;

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
        index.write().populate_from_scan(scan, drive);
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
/// Double-guarded: a `Global\`-namespace mutex plus a live-window check, so two
/// instances can never both claim ownership (which would make them fight over
/// the same global hotkeys).
fn ensure_single_instance() -> Option<HWND> {
    let existing = find_fastseek_window();
    let mut name: Vec<u16> = "Global\\FastSeek_SingleInstance"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let name_ptr = PCWSTR(name.as_mut_ptr());
    // requireAdministrator grants SeCreateGlobalPrivilege, so the Global\
    // namespace is safe here and holds even across sessions.
    let Ok(mutex) = (unsafe { CreateMutexW(None, true.into(), name_ptr) }) else {
        return None; // could not even create the mutex; proceed anyway
    };
    let already_running = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    std::mem::forget(mutex); // leak the handle so the mutex lives for the process
    if already_running {
        return existing;
    }
    // Mutex says we're first — but if a FastSeek window already exists (mutex
    // namespace quirk), defer to it instead of starting a second index.
    existing
}

fn find_fastseek_window() -> Option<HWND> {
    let mut title: Vec<u16> = "FastSeek".encode_utf16().chain(std::iter::once(0)).collect();
    let title_ptr = PCWSTR(title.as_mut_ptr());
    unsafe { FindWindowW(PCWSTR::null(), title_ptr).ok().filter(|h| !h.0.is_null()) }
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
    Some(PreviewInfo {
        size: md.len(),
        modified_secs: modified_secs_of(&md),
        is_dir: md.is_dir(),
    })
}

fn modified_secs_of(md: &std::fs::Metadata) -> u64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[derive(Serialize, Default)]
struct AppInfo {
    /// Resolved executable for .lnk apps (None for UWP / unresolvable links).
    target: Option<String>,
    size: u64,
    modified_secs: u64,
    is_uwp: bool,
    publisher: Option<String>,
    version: Option<String>,
    uninstall_string: Option<String>,
}

/// App-targeted metadata for the preview pane: resolve the .lnk to its real
/// executable so Size/Modified/Where show the app itself (not the shortcut),
/// and pull publisher/version/uninstall from the registry.
#[tauri::command]
fn app_info(name: String, path: String) -> AppInfo {
    let is_uwp = path.starts_with("aumid:");
    let mut target = None;
    if !is_uwp {
        target = Some(if path.to_lowercase().ends_with(".lnk") {
            resolve_lnk_target(&path).unwrap_or_else(|| path.clone())
        } else {
            path.clone()
        });
    }
    let (size, modified_secs) = target
        .as_deref()
        .and_then(|p| std::fs::metadata(p).ok())
        .map(|md| (md.len(), modified_secs_of(&md)))
        .unwrap_or((0, 0));
    let reg = find_uninstall_entry(&name);
    AppInfo {
        target,
        size,
        modified_secs,
        is_uwp,
        publisher: reg.as_ref().and_then(|r| r.publisher.clone()),
        version: reg.as_ref().and_then(|r| r.version.clone()),
        uninstall_string: reg
            .and_then(|r| r.uninstall_string.or(r.quiet_uninstall_string)),
    }
}

/// Run the app's uninstaller (registry UninstallString), falling back to the
/// Apps & features settings page when no registry entry exists. UWP apps have
/// no uninstaller string — open their settings page instead.
#[tauri::command]
fn uninstall_app(name: String, path: String) -> Result<(), String> {
    let mut target = None;
    if !path.starts_with("aumid:") && path.to_lowercase().ends_with(".lnk") {
        target = resolve_lnk_target(&path);
    }
    let _ = target; // (kept for potential future UWP package mapping)
    if let Some(entry) = find_uninstall_entry(&name) {
        if let Some(cmd) = entry.uninstall_string.or(entry.quiet_uninstall_string) {
            // cmd is a console app — hide its window (same flash issue as
            // the PowerShell helpers).
            use std::os::windows::process::CommandExt;
            std::process::Command::new("cmd")
                .creation_flags(0x0800_0000)
                .args(["/C", &cmd])
                .spawn()
                .map_err(|e| e.to_string())?;
            return Ok(());
        }
    }
    // No registry entry (or UWP): the Apps & features page is the safe path.
    let _ = std::process::Command::new("explorer")
        .arg("ms-settings:appsfeatures")
        .spawn();
    Ok(())
}

struct UninstallEntry {
    publisher: Option<String>,
    version: Option<String>,
    uninstall_string: Option<String>,
    quiet_uninstall_string: Option<String>,
}

/// Match an app name against the standard Uninstall registry keys
/// (HKCU + HKLM + WOW6432Node) and return the entry that names it.
fn find_uninstall_entry(app_name: &str) -> Option<UninstallEntry> {
    use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ};
    use winreg::RegKey;
    let want = app_name.trim().trim_end_matches(".lnk").trim();
    if want.is_empty() {
        return None;
    }
    const SUB: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall";
    let bases = [
        (HKEY_CURRENT_USER, SUB.to_string()),
        (HKEY_LOCAL_MACHINE, SUB.to_string()),
        (HKEY_LOCAL_MACHINE, r"SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall".to_string()),
    ];
    for (hive, base) in bases {
        let Ok(root) = RegKey::predef(hive).open_subkey_with_flags(&base, KEY_READ) else {
            continue;
        };
        for sub in root.enum_keys().flatten() {
            let Ok(key) = root.open_subkey_with_flags(&sub, KEY_READ) else {
                continue;
            };
            let Ok(display) = key.get_value::<String, _>("DisplayName") else {
                continue;
            };
            if !display.trim().eq_ignore_ascii_case(want) {
                continue;
            }
            let uninstall = key.get_value::<String, _>("UninstallString").ok();
            let quiet = key.get_value::<String, _>("QuietUninstallString").ok();
            if uninstall.is_none() && quiet.is_none() {
                continue;
            }
            return Some(UninstallEntry {
                publisher: key.get_value::<String, _>("Publisher").ok(),
                version: key.get_value::<String, _>("DisplayVersion").ok(),
                uninstall_string: uninstall,
                quiet_uninstall_string: quiet,
            });
        }
    }
    None
}

/// Resolve a .lnk shortcut to its target path via IShellLinkW.
fn resolve_lnk_target(path: &str) -> Option<String> {
    use windows::core::{Interface, PCWSTR};
    use windows::Win32::Storage::FileSystem::WIN32_FIND_DATAW;
    use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, IPersistFile, STGM};
    use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};

    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let link: IShellLinkW = unsafe { CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER) }.ok()?;
    let persist = link.cast::<IPersistFile>().ok()?;
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        persist.Load(PCWSTR(wide.as_ptr()), STGM(0) /* STGM_READ */).ok()?;
    }
    let mut buf = [0u16; 1024];
    let mut find = WIN32_FIND_DATAW::default();
    let mut resolved = String::new();
    unsafe {
        if link.GetPath(&mut buf, &mut find, 0).is_ok() {
            let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            if end > 0 {
                resolved = String::from_utf16_lossy(&buf[..end]);
            }
        }
    }
    (!resolved.is_empty()).then_some(resolved)
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
