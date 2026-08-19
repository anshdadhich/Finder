use finder::index::store::CacheData;
use std::io::Read;

/// Standalone cache inspection: decodes the persistence file exactly as the
/// app's load path does and prints the decoded summary. Exits non-zero if the
/// stream cannot be decoded (corruption probe).
fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 && args[1] == "--test-rt" {
        roundtrip_test();
        return;
    }
    let path = std::env::args().nth(1).unwrap_or_else(|| {
        let base = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".into());
        format!("{}\\Finder\\index\\finder_cache.bin", base)
    });
    let mut file = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("open failed: {} ({})", path, e);
            std::process::exit(2);
        }
    };
    let mut magic = [0u8; 4];
    if file.read_exact(&mut magic).is_err() {
        eprintln!("read failed (too short)");
        std::process::exit(3);
    }
    let cache: CacheData = if magic == [0x28, 0xB5, 0x2F, 0xFD] {
        std::io::Seek::seek(&mut file, std::io::SeekFrom::Start(0)).unwrap();
        let mut dec = match zstd::stream::read::Decoder::new(&mut file) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("zstd header error: {}", e);
                std::process::exit(4);
            }
        };
        let mut buf = Vec::new();
        if dec.read_to_end(&mut buf).is_err() {
            eprintln!("zstd stream error");
            std::process::exit(5);
        }
        match bincode::deserialize(&buf) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("bincode deserialize error: {}", e);
                std::process::exit(6);
            }
        }
    } else if magic == [0x04, 0x22, 0x4D, 0x18] {
        std::io::Seek::seek(&mut file, std::io::SeekFrom::Start(0)).unwrap();
        let mut dec = lz4_flex::frame::FrameDecoder::new(&mut file);
        match bincode::deserialize_from(&mut dec) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("lz4/bincode error: {}", e);
                std::process::exit(7);
            }
        }
    } else {
        eprintln!("unknown magic: {:?}", magic);
        std::process::exit(8);
    };
    println!(
        "OK: entries={} roots={:?} checkpoints={}",
        cache.entries.len(),
        cache.drive_roots,
        cache.checkpoints.len()
    );
    for c in &cache.checkpoints {
        println!("  cp: drive='{}' next_usn={} journal_id={}", c.drive_letter, c.next_usn, c.journal_id);
    }
}

/// Reproduce the exact production save path on a synthetic 8MB blob and read
/// it back through the exact production load path. Failure here proves the
/// writer/reader bytes disagree independent of the real index data.
fn roundtrip_test() {
    let blob: Vec<u8> = (0..8_000_000u32).map(|i| (i % 251) as u8).collect();
    let path = std::env::temp_dir().join("finder_rt_test.bin");
    {
        let file = std::fs::File::create(&path).unwrap();
        let mut enc = zstd::stream::write::Encoder::new(std::io::BufWriter::new(file), 3).unwrap();
        bincode::serialize_into(&mut enc, &blob).unwrap();
        enc.finish().unwrap();
    }
    let size = std::fs::metadata(&path).unwrap().len();
    let mut file = std::fs::File::open(&path).unwrap();
    let mut magic = [0u8; 4];
    file.read_exact(&mut magic).unwrap();
    assert_eq!(magic, [0x28, 0xB5, 0x2F, 0xFD], "magic missing");
    std::io::Seek::seek(&mut file, std::io::SeekFrom::Start(0)).unwrap();
    let mut dec = zstd::stream::read::Decoder::new(&mut file).unwrap();
    let mut buf = Vec::new();
    match dec.read_to_end(&mut buf) {
        Ok(_) => {
            let same = buf.len() == blob.len() && buf[..] == blob[..];
            println!(
                "RT OK: file={}B in->{}B out {}",
                size,
                buf.len(),
                if same { "MATCH" } else { "MISMATCH" }
            );
        }
        Err(e) => println!(
            "RT FAIL: file={}B, stream error reading: {} (decoded {}B)",
            size,
            e,
            buf.len()
        ),
    }
}