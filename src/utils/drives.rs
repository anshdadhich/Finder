use crate::mft::types::NtfsDrive;
use windows::{
    Win32::Storage::FileSystem::{
        GetLogicalDriveStringsW, GetVolumeInformationW,
    },
};

/// Returns all NTFS drives on the system
pub fn get_ntfs_drives() -> Vec<NtfsDrive> {
    let mut drives = Vec::new();

    // Get all logical drive strings (e.g. "C:\\\0D:\\\0\0")
    let mut buf = vec![0u16; 256];
    let len = unsafe { GetLogicalDriveStringsW(Some(&mut buf)) } as usize;
    if len == 0 {
        return drives;
    }

    // Parse null-separated drive strings
    let drive_strings: Vec<String> = buf[..len]
        .split(|&c| c == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf16_lossy(s))
        .collect();

    for root in drive_strings {
        if is_ntfs(&root) {
            let letter = root.chars().next().unwrap();
            drives.push(NtfsDrive {
                letter,
                root: root.clone(),
                device_path: format!("\\\\.\\{}:", letter),
            });
        }
    }

    drives
}

fn is_ntfs(root: &str) -> bool {
    let root_wide: Vec<u16> = root.encode_utf16().chain(Some(0)).collect();
    let mut fs_name = vec![0u16; 32];

    let ok = unsafe {
        GetVolumeInformationW(
            windows::core::PCWSTR(root_wide.as_ptr()),
            None,
            None,
            None,
            None,
            Some(&mut fs_name),
        )
    };

    if ok.is_err() {
        return false;
    }

    let fs = String::from_utf16_lossy(&fs_name);
    fs.starts_with("NTFS")
}