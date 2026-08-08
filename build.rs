fn main() {
    // Elevation is required to read the NTFS MFT (SeBackupPrivilege);
    // without it the scan finds zero records and the app would present an
    // empty, useless index. Embedding the requireAdministrator manifest is
    // opt-in (`--features embed-resources`) — every shipped build MUST use
    // it — while normal `cargo test`/dev builds stay un-elevated.
    let mut windows = tauri_build::WindowsAttributes::new();
    if std::env::var("CARGO_FEATURE_EMBED_RESOURCES").is_ok() {
        windows = windows.app_manifest(include_str!("manifest.xml"));
    }
    let attributes = tauri_build::Attributes::new().windows_attributes(windows);
    tauri_build::try_build(attributes).expect("failed to run tauri-build");
}