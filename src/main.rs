#![allow(dead_code)]

use std::sync::Arc;
use std::io::{self, Write};
use parking_lot::RwLock;
use crossbeam_channel::unbounded;

use fastsearch::index::store::IndexStore;
use fastsearch::index::search::search;
use fastsearch::mft::reader::MftReader;
use fastsearch::mft::watcher::UsnWatcher;
use fastsearch::mft::types::IndexEvent;
use fastsearch::utils::drives::get_ntfs_drives;

fn main() {
    println!("╔══════════════════════════════════╗");
    println!("║       FastSeek - File Search      ║");
    println!("╚══════════════════════════════════╝");
    println!();

    let drives = get_ntfs_drives();
    if drives.is_empty() {
        eprintln!("No NTFS drives found. Are you running as Administrator?");
        std::process::exit(1);
    }

    let index: Arc<RwLock<IndexStore>> = Arc::new(RwLock::new(IndexStore::new()));
    let (tx, rx) = unbounded();
    let cache_path = std::env::temp_dir().join("fastseek_cache.bin");

    // --- Try loading from cache ---
    let cache_loaded = if cache_path.exists() {
        print!("Loading cached index... ");
        io::stdout().flush().unwrap();
        match std::fs::read(&cache_path) {
            Ok(compressed) => {
                match lz4_flex::decompress_size_prepended(&compressed) {
                    Ok(bytes) => {
                        match bincode::deserialize::<fastsearch::index::store::CacheData>(&bytes) {
                            Ok(cache) => {
                                if cache.entries.is_empty() {
                                    println!("empty cache, rescanning...");
                                    let _ = std::fs::remove_file(&cache_path);
                                    false
                                } else {
                                let count = cache.entries.len();
                                let checkpoints = cache.checkpoints.clone();
                                *index.write() = IndexStore::from_cache(cache);
                                println!("{} files", count);

                                // --- Delta catch-up ---
                                if !checkpoints.is_empty() {
                                    print!("Catching up on changes since last run... ");
                                    io::stdout().flush().unwrap();

                                    let (delta_tx, delta_rx) = unbounded::<IndexEvent>();
                                    let mut journal_ok = true;

                                    for drive in &drives {
                                        let cp = checkpoints.iter()
                                            .find(|c| c.drive_letter == drive.letter);

                                        if let Some(cp) = cp {
                                            match UsnWatcher::new_from(drive, delta_tx.clone(), Some(cp)) {
                                                Ok(mut watcher) => {
                                                    watcher.drain();
                                                    let new_cp = watcher.checkpoint();
                                                    let mut store = index.write();
                                                    store.checkpoints.retain(|c| c.drive_letter != drive.letter);
                                                    store.checkpoints.push(new_cp);
                                                }
                                                Err(_) => {
                                                    println!("journal reset, full rescan needed.");
                                                    let _ = std::fs::remove_file(&cache_path);
                                                    journal_ok = false;
                                                    break;
                                                }
                                            }
                                        } else {
                                            // No checkpoint for this drive — cache is incomplete
                                            println!("missing checkpoint for {}:, full rescan needed.", drive.letter);
                                            let _ = std::fs::remove_file(&cache_path);
                                            journal_ok = false;
                                            break;
                                        }
                                    }

                                    drop(delta_tx);

                                    if journal_ok {
                                        let mut applied = 0usize;
                                        let mut store = index.write();
                                        for event in delta_rx {
                                            match event {
                                                IndexEvent::Created(r) => store.insert(r),
                                                IndexEvent::Deleted(id) => store.remove(id),
                                                IndexEvent::Renamed { old_ref, new_record } => {
                                                    store.rename(old_ref, new_record)
                                                }
                                                IndexEvent::Moved { file_ref, new_parent_ref, name, kind } => {
                                                    store.apply_move(file_ref, new_parent_ref, name, kind);
                                                }
                                            }
                                            applied += 1;
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
                            Err(_) => { println!("cache corrupt, rescanning..."); false }
                        }
                    }
                    Err(_) => { println!("cache corrupt, rescanning..."); false }
                }
            }
            Err(_) => { println!("cache unreadable, rescanning..."); false }
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
                    store.populate_from_scan(scan, &drive.root);
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
            match bincode::serialize(&cache) {
                Ok(bytes) => {
                    let raw_mb = bytes.len() as f64 / 1_048_576.0;
                    let compressed = lz4_flex::compress_prepend_size(&bytes);
                    let comp_mb = compressed.len() as f64 / 1_048_576.0;
                    match std::fs::write(&cache_path, &compressed) {
                        Ok(_) => println!("Cache saved — {:.1}MB compressed ({:.1}MB raw)", comp_mb, raw_mb),
                        Err(e) => eprintln!("Could not save cache: {}", e),
                    }
                }
                Err(e) => eprintln!("Could not serialize: {}", e),
            }
            }
        }

        let total_elapsed = total_start.elapsed();
        println!("Total startup: {:.2}s", total_elapsed.as_secs_f64());
        println!();
    }

    // --- USN watchers for live updates while running ---
    let live_checkpoints: Arc<parking_lot::Mutex<Vec<fastsearch::mft::types::JournalCheckpoint>>> =
        Arc::new(parking_lot::Mutex::new(index.read().checkpoints.clone()));

    for drive in &drives {
        let tx_clone = tx.clone();
        let drive_clone = drive.clone();
        let cps = Arc::clone(&live_checkpoints);
        std::thread::spawn(move || {
            if let Ok(mut watcher) = UsnWatcher::new(&drive_clone, tx_clone) {
                watcher.run_shared(cps);
            }
        });
    }

    // --- Live index updates ---
    let index_live: Arc<RwLock<IndexStore>> = Arc::clone(&index);
    std::thread::spawn(move || {
        for event in rx {
            let mut store = index_live.write();
            match event {
                IndexEvent::Created(r) => store.insert(r),
                IndexEvent::Deleted(id) => store.remove(id),
                IndexEvent::Renamed { old_ref, new_record } => store.rename(old_ref, new_record),
                IndexEvent::Moved { file_ref, new_parent_ref, name, kind } => {
                    store.apply_move(file_ref, new_parent_ref, name, kind);
                }
            }
        }
    });

    // Save updated cache on exit with latest checkpoints from live watchers
    let index_for_save = Arc::clone(&index);
    let cps_for_save = Arc::clone(&live_checkpoints);
    ctrlc::set_handler(move || {
        let mut store = index_for_save.write();
        if store.entries.is_empty() {
            std::process::exit(0);
        }
        store.checkpoints = cps_for_save.lock().clone();
        let cache = store.to_cache();
        if let Ok(bytes) = bincode::serialize(&cache) {
            let compressed = lz4_flex::compress_prepend_size(&bytes);
            let _ = std::fs::write(
                std::env::temp_dir().join("fastseek_cache.bin"),
                &compressed,
            );
        }
        std::process::exit(0);
    }).ok();

    search_loop(index);
}

fn search_loop(index: Arc<RwLock<IndexStore>>) {
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
                let cache_path = std::env::temp_dir().join("fastseek_cache.bin");
                let _ = std::fs::remove_file(&cache_path);
                println!("Cache cleared. Restart fastseek to rescan.\n");
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
                    use fastsearch::index::search::SearchResult;
                    let dot_ext = format!(".{}", ext);
                    store.entries.iter().filter_map(|entry| {
                        let name = store.name_lower(entry);
                        if !name.ends_with(&dot_ext) {
                            return None;
                        }
                        let kind_ok = match parsed.filter {
                            Filter::All   => true,
                            Filter::Dirs  => matches!(entry.kind(), fastsearch::mft::types::FileKind::Directory),
                            Filter::Files => !matches!(entry.kind(), fastsearch::mft::types::FileKind::Directory),
                        };
                        if !kind_ok { return None; }

                        let full_path = fastsearch::index::search::build_path(
                            entry.file_ref, &store
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
                            is_dir: matches!(entry.kind(), fastsearch::mft::types::FileKind::Directory),
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
        .join("fastsearch");
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


