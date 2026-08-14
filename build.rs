fn main() {
    // Elevation is required to read the NTFS MFT (SeBackupPrivilege); without
    // it the scan finds zero records and the app would present an empty,
    // useless index. Finder ALWAYS runs elevated, so the requireAdministrator
    // manifest is embedded unconditionally on Windows. The `embed-resources`
    // cargo feature is retained (older builds/scripts still pass it) but is no
    // longer required for elevation.
    let mut windows = tauri_build::WindowsAttributes::new();
    windows = windows.app_manifest(include_str!("manifest.xml"));
    let attributes = tauri_build::Attributes::new().windows_attributes(windows);
    tauri_build::try_build(attributes).expect("failed to run tauri-build");
}