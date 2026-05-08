fn main() {
    tauri_build::build();

    // Only embed resources when the feature is enabled
    if std::env::var("CARGO_FEATURE_EMBED_RESOURCES").is_ok() {
        #[cfg(target_os = "windows")]
        {
            let mut res = winres::WindowsResource::new();
            res.set_manifest_file("manifest.xml");
            if let Err(e) = res.compile() {
                eprintln!("Warning: Failed to embed manifest: {}", e);
            }
        }
    }
}
