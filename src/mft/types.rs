// use serde::{Serialize, Deserialize};

// #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
// pub enum FileKind {
//     File,
//     Directory,
// }

// #[derive(Debug, Clone)]
// pub struct FileRecord {
//     pub file_ref: u64,
//     pub parent_ref: u64,
//     pub name: String,
//     pub kind: FileKind,
//     pub size: u64,
//     pub full_path: std::path::PathBuf,
// }

// #[derive(Debug, Clone)]
// pub struct NtfsDrive {
//     pub letter: char,
//     pub root: String,
//     pub device_path: String,
// }

// #[derive(Debug)]
// pub enum IndexEvent {
//     Created(FileRecord),
//     Deleted(u64),
//     Renamed { old_ref: u64, new_record: FileRecord },
//     Moved { file_ref: u64, new_parent_ref: u64, name: String, kind: FileKind },
// }

// #[derive(Debug, Clone, Serialize, Deserialize)]
// pub struct JournalCheckpoint {
//     pub next_usn: i64,
//     pub journal_id: u64,
//     pub drive_letter: char,
// }


use serde::{Serialize, Deserialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum FileKind {
    File,
    Directory,
}

#[derive(Debug, Clone)]
pub struct FileRecord {
    pub file_ref: u64,
    pub parent_ref: u64,
    pub name: String,
    pub kind: FileKind,
}

#[derive(Debug, Clone)]
pub struct NtfsDrive {
    pub letter: char,
    pub root: String,
    pub device_path: String,
}

#[derive(Debug)]
pub enum IndexEvent {
    Created(FileRecord),
    Deleted(u64),
    Renamed { old_ref: u64, new_record: FileRecord },
    Moved { file_ref: u64, new_parent_ref: u64, name: String, kind: FileKind },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalCheckpoint {
    pub next_usn: i64,
    pub journal_id: u64,
    pub drive_letter: char,
}