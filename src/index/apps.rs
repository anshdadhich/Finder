use std::ffi::OsString;
use std::os::windows::prelude::*;
use windows::core::*;
use windows::Win32::System::Registry::*;

#[derive(Debug, Clone)]
pub struct InstalledApp {
    pub name: String,
    pub install_location: Option<String>,
    pub publisher: Option<String>,
    pub uninstall_string: Option<String>,
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
                apps.push(InstalledApp {
                    name,
                    install_location,
                    publisher,
                    uninstall_string,
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
    let mut seen = std::collections::HashSet::new();
    apps.retain(|app| seen.insert(app.name.clone()));
    apps.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    apps
}