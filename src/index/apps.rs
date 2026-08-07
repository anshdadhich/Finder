use std::ffi::OsString;
use std::os::windows::prelude::*;
use std::path::Path;
use windows::core::*;
use windows::Win32::System::Registry::*;

#[derive(Debug, Clone)]
pub struct InstalledApp {
    pub name: String,
    pub install_location: Option<String>,
    pub publisher: Option<String>,
    pub uninstall_string: Option<String>,
    pub quiet_uninstall_string: Option<String>,
    pub icon: Option<String>,
}

unsafe fn read_reg_string(hkey: HKEY, subkey: &str, value_name: &str) -> Option<String> {
    let mut buffer = [0u16; 1024];
    let mut len = buffer.len() as u32 * 2;
    let value_name_wide: Vec<u16> = value_name.encode_utf16().chain(std::iter::once(0)).collect();
    let subkey_wide: Vec<u16> = subkey.encode_utf16().chain(std::iter::once(0)).collect();
    let mut h_subkey = HKEY::default();
    unsafe {
        if RegOpenKeyExW(hkey, PCWSTR(subkey_wide.as_ptr()), Some(0), KEY_READ, &mut h_subkey).is_err() {
            return None;
        }
        let result = RegQueryValueExW(
            h_subkey,
            PCWSTR(value_name_wide.as_ptr()),
            None,
            None,
            Some(&mut buffer as *mut _ as *mut u8),
            Some(&mut len as *mut u32),
        );
        let _ = RegCloseKey(h_subkey);

        if result.is_err() {
            return None;
        }
        let len_chars = len as usize / 2;
        let s = OsString::from_wide(&buffer[..len_chars.saturating_sub(1)]);
        s.into_string().ok()
    }
}

unsafe fn enum_uninstall_key(hkey: HKEY) -> Vec<InstalledApp> {
    let mut apps = Vec::new();
    let key_paths = [
        r"Software\Microsoft\Windows\CurrentVersion\Uninstall",
        r"Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall",
    ];
    for &key_path in &key_paths {
        let mut h_key = HKEY::default();
        let key_wide: Vec<u16> = key_path.encode_utf16().chain(std::iter::once(0)).collect();
        unsafe {
            if RegOpenKeyExW(
                hkey,
                PCWSTR(key_wide.as_ptr()),
                Some(0),
                KEY_READ | KEY_ENUMERATE_SUB_KEYS,
                &mut h_key,
            ).is_err() {
                continue;
            }
            let mut index = 0u32;
            loop {
                let mut subkey_name = [0u16; 256];
                let mut subkey_len = subkey_name.len() as u32;
                let result = RegEnumKeyExW(
                    h_key,
                    index,
                    Some(PWSTR(subkey_name.as_mut_ptr())),
                    &mut subkey_len,
                    None,
                    None,
                    None,
                    None,
                );
                if result.is_err() {
                    break;
                }
                index += 1;
                let subkey_str = OsString::from_wide(&subkey_name[..subkey_len as usize])
                    .into_string()
                    .unwrap_or_default();
                let name = read_reg_string(h_key, &subkey_str, "DisplayName");
                let name = match name {
                    Some(n) if !n.trim().is_empty() => n,
                    _ => continue,
                };
                let install_location = read_reg_string(h_key, &subkey_str, "InstallLocation");
                let publisher = read_reg_string(h_key, &subkey_str, "Publisher");
                let uninstall_string = read_reg_string(h_key, &subkey_str, "UninstallString");
                let quiet_uninstall_string = read_reg_string(h_key, &subkey_str, "QuietUninstallString");
                let icon = read_reg_string(h_key, &subkey_str, "DisplayIcon");
                apps.push(InstalledApp {
                    name,
                    install_location,
                    publisher,
                    uninstall_string,
                    quiet_uninstall_string,
                    icon,
                });
            }
           let _ = RegCloseKey(h_key);
        }
    }
    apps
}

pub unsafe fn get_installed_apps() -> Vec<InstalledApp> {
    let mut apps = Vec::new();
    apps.extend(enum_uninstall_key(HKEY_LOCAL_MACHINE));
    apps.extend(enum_uninstall_key(HKEY_CURRENT_USER));
    apps.extend(unsafe { msi_installed_apps() });
    apps.extend(unsafe { apps_from_app_paths() });

    // Merge by lowercase name, preferring the variant that has a runnable path.
    let mut by_name: std::collections::HashMap<String, InstalledApp> = std::collections::HashMap::new();
    for app in apps {
        let key = app.name.to_lowercase();
        match by_name.get_mut(&key) {
            Some(existing) => {
                if existing.install_location.is_none() && app.install_location.is_some() {
                    existing.install_location = app.install_location.clone();
                }
                if existing.icon.is_none() && app.icon.is_some() {
                    existing.icon = app.icon.clone();
                }
                if existing.uninstall_string.is_none() && app.uninstall_string.is_some() {
                    existing.uninstall_string = app.uninstall_string.clone();
                }
                if existing.quiet_uninstall_string.is_none() && app.quiet_uninstall_string.is_some() {
                    existing.quiet_uninstall_string = app.quiet_uninstall_string.clone();
                }
            }
            None => {
                by_name.insert(key, app);
            }
        }
    }
    let mut apps: Vec<InstalledApp> = by_name.into_values().collect();
    apps.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    apps
}

/// Registry "App Paths" provider: HKLM/HKCU `...\App Paths` keys are keyed by
/// executable name ("chrome.exe", "notepad.exe") whose (Default) value is the
/// full path. Covers Win32 tools that never get a Start Menu shortcut, and
/// works on every machine without any setup.
unsafe fn apps_from_app_paths() -> Vec<InstalledApp> {
    let mut apps = Vec::new();
    let key_paths = [
        r"Software\Microsoft\Windows\CurrentVersion\App Paths",
        r"Software\WOW6432Node\Microsoft\Windows\CurrentVersion\App Paths",
    ];
    for hkey in [HKEY_LOCAL_MACHINE, HKEY_CURRENT_USER] {
        for &key_path in &key_paths {
            let mut h_key = HKEY::default();
            let key_wide: Vec<u16> = key_path.encode_utf16().chain(std::iter::once(0)).collect();
            unsafe {
                if RegOpenKeyExW(
                    hkey,
                    PCWSTR(key_wide.as_ptr()),
                    Some(0),
                    KEY_READ | KEY_ENUMERATE_SUB_KEYS,
                    &mut h_key,
                )
                .is_err()
                {
                    continue;
                }
                let mut index = 0u32;
                loop {
                    let mut subkey_name = [0u16; 256];
                    let mut subkey_len = subkey_name.len() as u32;
                    let result = RegEnumKeyExW(
                        h_key,
                        index,
                        Some(PWSTR(subkey_name.as_mut_ptr())),
                        &mut subkey_len,
                        None,
                        None,
                        None,
                        None,
                    );
                    if result.is_err() {
                        break;
                    }
                    index += 1;
                    let subkey_str = OsString::from_wide(&subkey_name[..subkey_len as usize])
                        .into_string()
                        .unwrap_or_default();
                    if !subkey_str.to_lowercase().ends_with(".exe") {
                        continue;
                    }
                    let default_path = read_reg_string(h_key, &subkey_str, "");
                    let dir_path = read_reg_string(h_key, &subkey_str, "Path");
                    let mut candidate = default_path.or_else(|| {
                        dir_path.map(|dir| {
                            Path::new(&dir).join(&subkey_str).to_string_lossy().to_string()
                        })
                    });
                    if let Some(c) = candidate {
                        candidate = Some(expand_env_vars(&c));
                    }
                    let Some(candidate) = candidate else { continue };
                    let p = Path::new(&candidate);
                    if !p.is_file() || !p
                        .extension()
                        .map(|e| e.eq_ignore_ascii_case("exe"))
                        .unwrap_or(false)
                    {
                        continue;
                    }
                    let stem = p
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    if stem.is_empty() {
                        continue;
                    }
                    apps.push(InstalledApp {
                        name: prettify_exe_name(&stem),
                        install_location: p.parent().map(|d| d.to_string_lossy().into_owned()),
                        publisher: None,
                        uninstall_string: None,
                        quiet_uninstall_string: None,
                        icon: None,
                    });
                }
                let _ = RegCloseKey(h_key);
            }
        }
    }
    apps
}

fn prettify_exe_name(stem: &str) -> String {
    let mut out = String::new();
    let mut cap_next = true;
    for ch in stem.chars() {
        if ch.is_ascii_alphanumeric() {
            if cap_next {
                out.extend(ch.to_uppercase());
                cap_next = false;
            } else {
                out.push(ch);
            }
        } else if ch == '\'' {
            out.push(ch);
        } else {
            cap_next = true;
        }
    }
    if out.is_empty() { stem.to_string() } else { out }
}

/// Expand %VAR% tokens (e.g. %ProgramFiles%, %SystemRoot%) in a path string.
pub fn expand_env_vars(s: &str) -> String {
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

unsafe fn msi_product_prop(
    info_fn: unsafe extern "system" fn(PCWSTR, PCWSTR, PWSTR, *mut u32) -> u32,
    product: &str,
    prop: &str,
) -> Option<String> {
    let product_wide: Vec<u16> = product.encode_utf16().chain(std::iter::once(0)).collect();
    let prop_wide: Vec<u16> = prop.encode_utf16().chain(std::iter::once(0)).collect();
    let mut buf = [0u16; 1024];
    let mut len = 1024u32;
    let res = info_fn(
        PCWSTR(product_wide.as_ptr()),
        PCWSTR(prop_wide.as_ptr()),
        PWSTR(buf.as_mut_ptr()),
        &mut len,
    );
    if res != 0 {
        return None;
    }
    let s = OsString::from_wide(&buf[..len as usize])
        .into_string()
        .unwrap_or_default();
    let s = s.trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

/// Enumerate MSI products via msi.dll, loaded at run time, so apps installed by
/// an MSI package (Node.js, MongoDB, ...) surface even when the Uninstall
/// registry key carries no DisplayIcon or InstallLocation.
unsafe fn msi_installed_apps() -> Vec<InstalledApp> {
    use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

    let mut apps = Vec::new();
    let msi_wide: Vec<u16> = "msi.dll".encode_utf16().chain(std::iter::once(0)).collect();
    let Ok(hmod) = LoadLibraryW(PCWSTR(msi_wide.as_ptr())) else {
        return apps;
    };
    let Some(enum_fn) = GetProcAddress(hmod, windows::core::s!("MsiEnumProductsW")) else {
        return apps;
    };
    let Some(info_fn) = GetProcAddress(hmod, windows::core::s!("MsiGetProductInfoW")) else {
        return apps;
    };
    let enum_fn = std::mem::transmute::<_, unsafe extern "system" fn(u32, PWSTR) -> u32>(enum_fn);
    let info_fn = std::mem::transmute::<_, unsafe extern "system" fn(PCWSTR, PCWSTR, PWSTR, *mut u32) -> u32>(info_fn);

    let mut index = 0u32;
    loop {
        let mut product_buf = [0u16; 39];
        if enum_fn(index, PWSTR(product_buf.as_mut_ptr())) != 0 {
            break;
        }
        index += 1;
        let product = OsString::from_wide(&product_buf).into_string().unwrap_or_default();
        if product.is_empty() {
            continue;
        }
        let name = msi_product_prop(info_fn, &product, "InstalledProductName");
        let Some(name) = name else { continue };
        let location = msi_product_prop(info_fn, &product, "INSTALLLOCATION");
        apps.push(InstalledApp {
            name,
            install_location: location,
            publisher: None,
            uninstall_string: None,
            quiet_uninstall_string: None,
            icon: None,
        });
    }
    apps
}