#![allow(dead_code)]

use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
use std::io::{self, Write};
use std::path::PathBuf;
use bincode::Options as BincodeOptions;
use parking_lot::RwLock;
use crossbeam_channel::unbounded;

use finder::index::store::IndexStore;
use finder::index::search::search;
use finder::mft::reader::MftReader;
use finder::mft::watcher::UsnWatcher;
use finder::mft::types::IndexEvent;
use finder::utils::drives::get_ntfs_drives;

fn main() {
    println!("╔══════════════════════════════════╗");
    println!("║       Finder - File Search      ║");
    println!("╚══════════════════════════════════╝");
    println!();

    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 && args[1] == "--apps" {
        let apps = unsafe { finder::index::apps::get_installed_apps() };
        for app in apps {
            println!("{}", app.name);
            if let Some(loc) = app.install_location {
                println!("  Location: {}", loc);
            }
        }
        return;
    }

    let drives = get_ntfs_drives();
    if drives.is_empty() {
        eprintln!("No NTFS drives found. Are you running as Administrator?");
        std::process::exit(1);
    }

    let index: Arc<RwLock<IndexStore>> = Arc::new(RwLock::new(IndexStore::new()));
    let (tx, rx) = unbounded();
    // The CLI shares the GUI's cache — one source of truth under LOCALAPPDATA.
    // The old %TEMP%\finder_cache.bin location belonged to a dev-time snapshot
    // that drifted out of sync and falsely reported "cache corrupt".
    let cache_path = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("Finder")
        .join("index")
        .join("finder_cache.bin");

    // --- Try loading from cache ---
    let cache_loaded = if cache_path.exists() {
        print!("Loading cached index... ");
        io::stdout().flush().unwrap();
        match decode_cache_file(&cache_path) {
            Some(cache) => {
                if cache.entries.is_empty() {
                    println!("empty cache, rescanning...");
                    false
                } else {
                    match IndexStore::from_cache(cache) {
                        None => {
                            println!("cache format not decodable, rescanning...");
                            false
                        }
                        Some(store) => {
                            let count = store.len();
                            let checkpoints = store.checkpoints.clone();
                            *index.write() = store;
                            println!("{} files", count);

                            // --- Delta catch-up ---
                            if !checkpoints.is_empty() {
                                print!("Catching up on changes since last run... ");
                                io::stdout().flush().unwrap();

                                let (delta_tx, delta_rx) = unbounded::<IndexEvent>();
                                let mut journal_ok = true;

                                for drive in &drives {
                                    let cp = checkpoints
                                        .iter()
                                        .find(|c| c.drive_letter == drive.letter);

                                    if let Some(cp) = cp {
                                        match UsnWatcher::new_from(
                                            drive,
                                            delta_tx.clone(),
                                            Some(cp),
                                        ) {
                                            Ok(mut watcher) => {
                                                if watcher.drain().is_err() {
                                                    println!(
                                                        "journal read failed, falling back to a full scan."
                                                    );
                                                    journal_ok = false;
                                                    break;
                                                }
                                                let new_cp = watcher.checkpoint();
                                                let mut store = index.write();
                                                store
                                                    .checkpoints
                                                    .retain(|c| c.drive_letter != drive.letter);
                                                store.checkpoints.push(new_cp);
                                            }
                                            Err(_) => {
                                                println!(
                                                    "journal reset, falling back to a full scan."
                                                );
                                                journal_ok = false;
                                                break;
                                            }
                                        }
                                    } else {
                                        println!(
                                            "missing checkpoint for {}:, falling back to a full scan.",
                                            drive.letter
                                        );
                                        journal_ok = false;
                                        break;
                                    }
                                }

                                drop(delta_tx);

                                if journal_ok {
                                    let events: Vec<IndexEvent> = delta_rx.into_iter().collect();
                                    let applied = events.len();
                                    if !events.is_empty() {
                                        index.write().apply_events(events);
                                    }
                                    println!("{} change(s) applied", applied);
                                    println!();
                                    true
                                } else {
                                    false
                                }
                            } else {
                                println!();
                                true
                            }
                        }
                    }
                }
            }
            None => {
                // Unreadable, or a valid-format cache these codecs can't decode.
                // Report it, but NEVER delete the file — destroying a healthy
                // cache over a codec misread is the bug we are fixing here.
                println!("cache unreadable or unrecognized format, rescanning...");
                false
            }
        }
    } else {
        false
    };

    // --- Full MFT scan if no cache ---
    if !cache_loaded {
        println!("Found drives: {}", drives.iter().map(|d| format!("{}:", d.letter)).collect::<Vec<_>>().join(", "));
        println!("Building index...");

        let total_start = std::time::Instant::now();

        // Capture checkpoints BEFORE scan so changes during scan aren't lost
        {
            let mut store = index.write();
            for drive in &drives {
                let (dummy_tx, _) = unbounded::<IndexEvent>();
                if let Ok(w) = UsnWatcher::new(drive, dummy_tx) {
                    store.checkpoints.push(w.checkpoint());
                }
            }
        }

        let index_clone: Arc<RwLock<IndexStore>> = Arc::clone(&index);
        let drives_clone = drives.clone();

        let scan_thread = std::thread::spawn(move || {
            let mut total = 0usize;
            let mut total_scan_time = std::time::Duration::ZERO;
            let mut total_index_time = std::time::Duration::ZERO;

            for drive in &drives_clone {
                print!("  Scanning {}:  ... ", drive.letter);
                io::stdout().flush().unwrap();

                let reader: MftReader = match MftReader::open(drive) {
                    Ok(r) => r,
                    Err(e) => { println!("FAILED ({:?})", e); continue; }
                };

                let t1 = std::time::Instant::now();
                let (scan, method) = match reader.scan_direct() {
                    Some(s) if !s.records.is_empty() => (s, "direct"),
                    None => (reader.scan(), "ioctl"),
                    Some(_) => (reader.scan(), "ioctl"),
                };
                let count = scan.records.len();
                let scan_time = t1.elapsed();

                let t2 = std::time::Instant::now();
                {
                    let mut store = index_clone.write();
                    store.populate_from_scan(scan, drive);
                }
                let index_time = t2.elapsed();

                println!("{} files  (scan {:.2}s {}, index {:.2}s)",
                    count, scan_time.as_secs_f64(), method, index_time.as_secs_f64());

                total += count;
                total_scan_time += scan_time;
                total_index_time += index_time;
            }

            {
                let mut store = index_clone.write();
                store.finalize();
            }

            println!();
            println!("Index ready — {} total files  (scan {:.2}s, index {:.2}s)",
                total, total_scan_time.as_secs_f64(), total_index_time.as_secs_f64());
            total
        });

        scan_thread.join().unwrap();

        // Save cache
        {
            let store = index.read();
            if store.entries.is_empty() {
                eprintln!("Not saving empty cache.");
            } else {
                let cache = store.to_cache();
                persist_cache(&cache, &cache_path, true);
            }
        }

        let total_elapsed = total_start.elapsed();
        println!("Total startup: {:.2}s", total_elapsed.as_secs_f64());
        println!();
    }

    // --- USN watchers for live updates while running ---
    for drive in &drives {
        let tx_clone = tx.clone();
        let drive_clone = drive.clone();
        std::thread::spawn(move || {
            if let Ok(mut watcher) = UsnWatcher::new(&drive_clone, tx_clone) {
                watcher.run_shared(Arc::new(std::sync::atomic::AtomicU64::new(0)));
            }
        });
    }

    // --- Live index updates + checkpoint tracking ---
    // The applier consumes both data events and Checkpoint markers off the
    // same ordered channel. A Checkpoint is only stored in `store.checkpoints`
    // after every event that precedes it has been applied, so persisting that
    // checkpoint is always a consistent snapshot of the index.
    let dirty: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    let index_live: Arc<RwLock<IndexStore>> = Arc::clone(&index);
    let dirty_live = Arc::clone(&dirty);
    std::thread::spawn(move || {
        let mut pending: Vec<IndexEvent> = Vec::with_capacity(64);
        for event in rx {
            match event {
                IndexEvent::Checkpoint(cp) => {
                    if !pending.is_empty() {
                        index_live.write().apply_events(std::mem::take(&mut pending));
                    }
                    let mut store = index_live.write();
                    store.checkpoints.retain(|c| c.drive_letter != cp.drive_letter);
                    store.checkpoints.push(cp.clone());
                    dirty_live.store(true, Ordering::Relaxed);
                }
                other => {
                    pending.push(other);
                    if pending.len() >= 64 {
                        index_live.write().apply_events(std::mem::take(&mut pending));
                        dirty_live.store(true, Ordering::Relaxed);
                    }
                }
            }
        }
    });

    // --- Periodic cache persistence ---
    // The cache is only written on a clean exit today; a hard kill (Task
    // Manager / crash) would lose the USN checkpoints and force a full rescan.
    // Persist it on an interval, but only when the index actually changed.
    {
        let index_saver = Arc::clone(&index);
        let dirty_saver = Arc::clone(&dirty);
        let cache_path_saver = cache_path.clone();
        std::thread::spawn(move || {
            let interval = std::time::Duration::from_secs(30);
            loop {
                std::thread::sleep(interval);
                if dirty_saver.swap(false, Ordering::Relaxed) {
                    let cache = {
                        let store = index_saver.read();
                        if store.entries.is_empty() {
                            continue;
                        }
                        store.to_cache()
                    };
                    persist_cache(&cache, &cache_path_saver, false);
                }
            }
        });
    }

    // Save updated cache on exit; `store.to_cache()` carries the latest
    // checkpoints maintained by the live applier above.
    let index_for_save = Arc::clone(&index);
    let cache_path_for_save = cache_path.clone();
    ctrlc::set_handler(move || {
        let cache = {
            let store = index_for_save.write();
            if store.entries.is_empty() {
                std::process::exit(0);
            }
            store.to_cache()
        };
        persist_cache(&cache, &cache_path_for_save, false);
        std::process::exit(0);
    }).ok();

    // Show apps on startup
    {
        let store = index.read();
        let apps = finder::index::search::apps(&store, 50);
        if !apps.is_empty() {
            println!("📱 Installed Apps ({} shown):", apps.len());
            println!();
            for (i, app) in apps.iter().enumerate() {
                println!("  [{:>3}] {}", i + 1, app.name);
            }
            println!();
        }
    }

    search_loop(index, &cache_path);
}

fn search_loop(index: Arc<RwLock<IndexStore>>, cache_path: &std::path::Path) {
    let config_path = config_dir().join("config.txt");
    let mut case_sensitive = false;
    let mut excluded_dirs: Vec<String> = load_exclusions(&config_path);

    println!("Commands:");
    println!("  <query>           search files");
    println!("  folder:<query>    directories only    (or :<query>)");
    println!("  file:<query>      files only          (or !<query>)");
    println!("  *.ext / ext:ext   by extension e.g. *.pdf, ext:docx");
    println!("  case              toggle case sensitivity [off]");
    println!("  exclude <path>    exclude a directory");
    println!("  unexclude <path>  remove exclusion");
    println!("  exclusions        list excluded dirs");
    println!("  count             total indexed files");
    println!("  rescan            clear cache and rescan");
    println!("  quit              exit");
    println!();

    
    loop {
        print!("search> ");
        io::stdout().flush().unwrap();

        let mut input = String::new();
        match io::stdin().read_line(&mut input) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }

        let input = input.trim();
        if input.is_empty() { continue; }

        match input {
            "quit" | "exit" | "q" => {
                println!("Bye.");
                break;
            }

            "count" => {
                let store = index.read();
                println!("  {} files in index\n", store.len());
            }

            "rescan" => {
                // Delete the REAL cache (LOCALAPPDATA), not the old dev-time
                // %TEMP% snapshot that drifted out of sync and caused the
                // false "cache corrupt" reports.
                let _ = std::fs::remove_file(cache_path);
                println!("Cache cleared. Restart Finder to rescan.\n");
            }

            "case" => {
                case_sensitive = !case_sensitive;
                println!("  case sensitivity: {}\n", if case_sensitive { "ON" } else { "OFF" });
            }

            "exclusions" => {
                if excluded_dirs.is_empty() {
                    println!("  no excluded directories\n");
                } else {
                    println!();
                    for d in &excluded_dirs {
                        println!("  - {}", d);
                    }
                    println!();
                }
            }

            _ if input.starts_with("exclude ") => {
                let path = input[8..].trim().to_lowercase();
                if !path.is_empty() {
                    let path = if path.ends_with('\\') || path.ends_with('/') {
                        path
                    } else {
                        format!("{}\\", path)
                    };
                    if !excluded_dirs.contains(&path) {
                        excluded_dirs.push(path.clone());
                        save_exclusions(&config_path, &excluded_dirs);
                    }
                    println!("  excluded: {}\n", path);
                }
            }

            _ if input.starts_with("unexclude ") => {
                let path = input[10..].trim().to_lowercase();
                let path = if path.ends_with('\\') || path.ends_with('/') {
                    path
                } else {
                    format!("{}\\", path)
                };
                let before = excluded_dirs.len();
                excluded_dirs.retain(|d| d != &path);
                save_exclusions(&config_path, &excluded_dirs);
                if excluded_dirs.len() < before {
                    println!("  removed: {}\n", path);
                } else {
                    println!("  not found in exclusions\n");
                }
            }

            _ => {
                let parsed = parse_query(input);

                let store = index.read();
                let start = std::time::Instant::now();

                let results: Vec<_> = if let Some(ref ext) = parsed.ext_filter {
                    use finder::index::search::SearchResult;
                    let dot_ext = format!(".{}", ext);
                    store.entries.iter().filter_map(|entry| {
                        let name = store.name_lower(entry);
                        if !name.ends_with(&dot_ext) {
                            return None;
                        }
                        let kind_ok = match parsed.filter {
                            Filter::All   => true,
                            Filter::Dirs  => matches!(entry.kind(), finder::mft::types::FileKind::Directory),
                            Filter::Files => !matches!(entry.kind(), finder::mft::types::FileKind::Directory),
                        };
                        if !kind_ok { return None; }

                        let full_path = finder::index::search::build_path(
                            entry, &store
                        );

                        // Check exclusions
                        if !excluded_dirs.is_empty() {
                            let path_lower = full_path.to_string_lossy().to_lowercase();
                            for ex in &excluded_dirs {
                                if path_lower.starts_with(ex.as_str()) {
                                    return None;
                                }
                            }
                        }

                        Some(SearchResult {
                            full_path,
                            name: store.name(entry).to_string(),
                            rank: 0,
                            is_dir: matches!(entry.kind(), finder::mft::types::FileKind::Directory),
                            modified_time: None,
                            file_type_priority: 0,
                        })
                    }).take(300).collect()
                } else {
                    let raw = search(
                        &store,
                        parsed.query,
                        300,
                        case_sensitive,
                        &excluded_dirs,
                    );
                    raw.into_iter().filter(|r| {
                        match parsed.filter {
                            Filter::All   => true,
                            Filter::Dirs  => r.is_dir,
                            Filter::Files => !r.is_dir,
                        }
                    }).take(300).collect()
                };
                let elapsed = start.elapsed();

                if results.is_empty() {
                    println!("  no results for \"{}\"\n", input);
                } else {
                    println!();
                    for (i, r) in results.iter().enumerate() {
                        let kind = if r.is_dir { "DIR " } else { "FILE" };
                        println!("  [{:>3}] [{}] {}", i + 1, kind, r.full_path.display());
                    }
                    println!();
                    println!("  {} result(s) in {:.2}ms\n",
                        results.len(), elapsed.as_secs_f64() * 1000.0);
                }
            }
        }
    }
}

enum Filter { All, Dirs, Files }

struct ParsedQuery<'a> {
    query: &'a str,
    filter: Filter,
    ext_filter: Option<String>,
}

fn parse_query(input: &str) -> ParsedQuery<'_> {
    // ext:pdf or *.pdf
    if let Some(ext) = input.strip_prefix("ext:") {
        return ParsedQuery { query: "", filter: Filter::Files, ext_filter: Some(ext.to_lowercase()) };
    }
    if input.starts_with("*.") {
        return ParsedQuery { query: "", filter: Filter::All, ext_filter: Some(input[2..].to_lowercase()) };
    }
    // folder:name / file:name
    if let Some(q) = input.strip_prefix("folder:") {
        return ParsedQuery { query: q.trim(), filter: Filter::Dirs, ext_filter: None };
    }
    if let Some(q) = input.strip_prefix("file:") {
        return ParsedQuery { query: q.trim(), filter: Filter::Files, ext_filter: None };
    }
    // existing shortcuts
    if let Some(q) = input.strip_prefix(':') {
        return ParsedQuery { query: q, filter: Filter::Dirs, ext_filter: None };
    }
    if let Some(q) = input.strip_prefix('!') {
        return ParsedQuery { query: q, filter: Filter::Files, ext_filter: None };
    }
    ParsedQuery { query: input, filter: Filter::All, ext_filter: None }
}

fn config_dir() -> std::path::PathBuf {
    let dir = std::env::var("APPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir())
        .join("finder");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn load_exclusions(path: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(|l| l.trim().to_lowercase())
        .filter(|l| !l.is_empty())
        .collect()
}

fn save_exclusions(path: &std::path::Path, dirs: &[String]) {
    let content: String = dirs.join("\n");
    let _ = std::fs::write(path, content);
}

/// Decode a cache file in any format the project has ever written:
///  - v4 zstd frame (the GUI's current format)
///  - legacy lz4 frame (v3)
///  - earliest CLI block-lz4 (compress_prepend_size, no magic)
/// Returns None when the file exists but cannot be decoded. Never mutates
/// or deletes the file — a misread must never destroy a healthy cache.
fn decode_cache_file(path: &std::path::Path) -> Option<finder::index::store::CacheData> {
    use std::io::{Read, Seek};

    let file = std::fs::File::open(path).ok()?;
    let mut r = std::io::BufReader::new(file);
    let mut magic = [0u8; 4];
    if r.read_exact(&mut magic).is_err() {
        return None;
    }

    if magic == [0x28, 0xB5, 0x2F, 0xFD] {
        // v4: zstd frame. Stream straight into bincode (no full-index staging
        // buffer) and cap the decompressed bytes — the cache sits in the
        // user's LOCALAPPDATA, so it must be treated as untrusted input.
        r.seek(std::io::SeekFrom::Start(0)).ok()?;
        let mut dec = zstd::stream::read::Decoder::new(r).ok()?;
        bincode::DefaultOptions::new()
            .with_limit(finder::index::store::MAX_CACHE_DECODED)
            .deserialize_from::<_, finder::index::store::CacheData>(
                std::io::Read::take(&mut dec, finder::index::store::MAX_CACHE_DECODED),
            )
            .ok()
    } else if magic == [0x04, 0x22, 0x4D, 0x18] {
        // v3: legacy lz4 frame.
        r.seek(std::io::SeekFrom::Start(0)).ok()?;
        let mut dec = lz4_flex::frame::FrameDecoder::new(r);
        bincode::DefaultOptions::new()
            .with_limit(finder::index::store::MAX_CACHE_DECODED)
            .deserialize_from::<_, finder::index::store::CacheData>(
                std::io::Read::take(&mut dec, finder::index::store::MAX_CACHE_DECODED),
            )
            .ok()
    } else {
        // Earliest CLI format: block-lz4 with a size prefix (no magic). Try it
        // last as a fallback — a real match decodes, an unrelated file does not.
        r.seek(std::io::SeekFrom::Start(0)).ok()?;
        // Bound the raw slurp too (compressed bytes can exceed the decompressed
        // size on incompressible data, so allow 2x the cap + the 4-byte prefix).
        let max_raw = finder::index::store::MAX_CACHE_DECODED
            .checked_mul(2)
            .and_then(|v| v.checked_add(4))
            .unwrap_or(finder::index::store::MAX_CACHE_DECODED);
        let mut raw = Vec::new();
        std::io::Read::read_to_end(
            &mut std::io::Read::take(&mut r, max_raw),
            &mut raw,
        )
        .ok()?;
        // The size prefix is attacker-supplied: refuse to let lz4_flex allocate
        // a buffer larger than the decode cap before we ever call it.
        if raw.len() < 4 {
            return None;
        }
        let declared = u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]) as usize;
        if declared == 0 || declared as u64 > finder::index::store::MAX_CACHE_DECODED {
            return None;
        }
        let bytes = lz4_flex::decompress_size_prepended(&raw).ok()?;
        bincode::DefaultOptions::new()
            .with_limit(finder::index::store::MAX_CACHE_DECODED)
            .deserialize::<finder::index::store::CacheData>(&bytes)
            .ok()
    }
}

fn persist_cache(
    cache: &finder::index::store::CacheData,
    cache_path: &std::path::Path,
    verbose: bool,
) {
    use std::io::Write;

    let encoded = match bincode::serialize(cache) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("Could not serialize cache: {}", e);
            return;
        }
    };
    let mut buf = Vec::new();
    {
        let mut enc = match zstd::stream::write::Encoder::new(&mut buf, 3) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("zstd init failed: {}", e);
                return;
            }
        };
        if let Err(e) = enc.write_all(&encoded) {
            eprintln!("zstd write failed: {}", e);
            return;
        }
        if let Err(e) = enc.finish() {
            eprintln!("zstd finish failed: {}", e);
            return;
        }
    }

    if let Some(dir) = cache_path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = cache_path.with_extension(format!("tmp.{}", std::process::id()));
    if let Err(e) = std::fs::write(&tmp, &buf) {
        eprintln!("Could not write cache tmp: {}", e);
        return;
    }
    // Atomic replace (MoveFileExW REPLACE_EXISTING) — never leaves the
    // destination missing, so a concurrent reader always sees old-or-new.
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_REPLACE_EXISTING};
    let s: Vec<u16> = tmp.as_os_str().encode_wide().chain(Some(0)).collect();
    let d: Vec<u16> = cache_path.as_os_str().encode_wide().chain(Some(0)).collect();
    let result = unsafe {
        MoveFileExW(
            windows::core::PCWSTR(s.as_ptr()),
            windows::core::PCWSTR(d.as_ptr()),
            MOVEFILE_REPLACE_EXISTING,
        )
    };
    if let Err(e) = result {
        eprintln!("Could not replace cache: {}", e);
        let _ = std::fs::remove_file(&tmp);
        return;
    }
    if verbose {
        let raw_mb = encoded.len() as f64 / 1_048_576.0;
        let comp_mb = buf.len() as f64 / 1_048_576.0;
        println!(
            "Cache saved — {:.1}MB compressed ({:.1}MB raw)",
            comp_mb, raw_mb
        );
    }
}


