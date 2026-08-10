// use std::mem;
// use std::time::Duration;
// use crossbeam_channel::Sender;
// use windows::{
//     core::PCWSTR,
//     Win32::Foundation::HANDLE,
//     Win32::Storage::FileSystem::{
//         CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_DELETE,
//         FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
//     },
//     Win32::System::Ioctl::{
//         FSCTL_QUERY_USN_JOURNAL, FSCTL_READ_USN_JOURNAL,
//         READ_USN_JOURNAL_DATA_V0, USN_JOURNAL_DATA_V0, USN_RECORD_V2,
//         USN_REASON_FILE_CREATE, USN_REASON_FILE_DELETE,
//         USN_REASON_RENAME_NEW_NAME, USN_REASON_RENAME_OLD_NAME,
//     },
//     Win32::System::IO::DeviceIoControl,
// };
// use crate::mft::types::{FileKind, FileRecord, IndexEvent, JournalCheckpoint, NtfsDrive};

// const BUFFER_SIZE: usize = 64 * 1024;

// pub struct UsnWatcher {
//     handle: HANDLE,
//     drive: NtfsDrive,
//     sender: Sender<IndexEvent>,
//     pub next_usn: i64,
//     pub journal_id: u64,
// }

// impl UsnWatcher {
//     pub fn new(
//         drive: &NtfsDrive,
//         sender: Sender<IndexEvent>,
//     ) -> windows::core::Result<Self> {
//         Self::new_from(drive, sender, None)
//     }

//     pub fn new_from(
//         drive: &NtfsDrive,
//         sender: Sender<IndexEvent>,
//         checkpoint: Option<&JournalCheckpoint>,
//     ) -> windows::core::Result<Self> {
//         let path: Vec<u16> = drive.device_path.encode_utf16().chain(Some(0)).collect();

//         let handle = unsafe {
//             CreateFileW(
//                 PCWSTR(path.as_ptr()),
//                 0x0,
//                 FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
//                 None,
//                 OPEN_EXISTING,
//                 FILE_FLAG_BACKUP_SEMANTICS,
//                 None,
//             )?
//         };

//         let mut journal_data: USN_JOURNAL_DATA_V0 = unsafe { mem::zeroed() };
//         let mut bytes_returned = 0u32;

//         unsafe {
//             DeviceIoControl(
//                 handle,
//                 FSCTL_QUERY_USN_JOURNAL,
//                 None, 0,
//                 Some(&mut journal_data as *mut _ as *mut _),
//                 mem::size_of::<USN_JOURNAL_DATA_V0>() as u32,
//                 Some(&mut bytes_returned),
//                 None,
//             )?;
//         }

//         // If checkpoint matches current journal, resume from saved USN
//         // Otherwise start from current position (don't replay old history)
//         let next_usn = if let Some(cp) = checkpoint {
//             if cp.journal_id == journal_data.UsnJournalID {
//                 cp.next_usn
//             } else {
//                 // Journal was reset (e.g. disk check ran) — need full rescan
//                 return Err(windows::core::Error::new(
//                     windows::Win32::Foundation::ERROR_JOURNAL_NOT_ACTIVE.into(),
//                     "Journal ID mismatch — rescan needed",
//                 ));
//             }
//         } else {
//             journal_data.NextUsn
//         };

//         Ok(Self {
//             handle,
//             drive: drive.clone(),
//             sender,
//             next_usn,
//             journal_id: journal_data.UsnJournalID,
//         })
//     }

//     pub fn checkpoint(&self) -> JournalCheckpoint {
//         JournalCheckpoint {
//             next_usn: self.next_usn,
//             journal_id: self.journal_id,
//             drive_letter: self.drive.letter,
//         }
//     }

//     pub fn run(&mut self) {
//         let mut buffer = vec![0u8; BUFFER_SIZE];
//         loop {
//             std::thread::sleep(Duration::from_millis(500));
//             self.poll(&mut buffer);
//         }
//     }

//     /// Drain all pending journal entries — used for delta catch-up on startup
//     pub fn drain(&mut self) -> usize {
//         let mut buffer = vec![0u8; BUFFER_SIZE];
//         let mut count = 0;
//         loop {
//             let before = self.next_usn;
//             self.poll(&mut buffer);
//             if self.next_usn == before {
//                 break;
//             }
//             count += 1;
//         }
//         count
//     }

//     fn poll(&mut self, buffer: &mut Vec<u8>) {
//         let read_data = READ_USN_JOURNAL_DATA_V0 {
//             StartUsn: self.next_usn,
//             ReasonMask: USN_REASON_FILE_CREATE
//                 | USN_REASON_FILE_DELETE
//                 | USN_REASON_RENAME_NEW_NAME
//                 | USN_REASON_RENAME_OLD_NAME,
//             ReturnOnlyOnClose: 0,
//             Timeout: 0,
//             BytesToWaitFor: 0,
//             UsnJournalID: self.journal_id,
//         };

//         let mut bytes_returned = 0u32;
//         let ok = unsafe {
//             DeviceIoControl(
//                 self.handle,
//                 FSCTL_READ_USN_JOURNAL,
//                 Some(&read_data as *const _ as *const _),
//                 mem::size_of::<READ_USN_JOURNAL_DATA_V0>() as u32,
//                 Some(buffer.as_mut_ptr() as *mut _),
//                 BUFFER_SIZE as u32,
//                 Some(&mut bytes_returned),
//                 None,
//             )
//         };

//         if ok.is_err() || bytes_returned <= 8 {
//             return;
//         }

//         self.next_usn = i64::from_ne_bytes(buffer[0..8].try_into().unwrap());

//         let mut offset = 8usize;
//         while offset + mem::size_of::<USN_RECORD_V2>() <= bytes_returned as usize {
//             let record = unsafe {
//                 &*(buffer.as_ptr().add(offset) as *const USN_RECORD_V2)
//             };
//             if record.RecordLength == 0 { break; }
//             self.process_record(record, buffer, offset);
//             offset += record.RecordLength as usize;
//         }
//     }

//     fn process_record(&self, record: &USN_RECORD_V2, buffer: &[u8], offset: usize) {
//         let name_offset = record.FileNameOffset as usize;
//         let name_len = record.FileNameLength as usize / 2;
//         let name_ptr = unsafe {
//             buffer.as_ptr().add(offset + name_offset) as *const u16
//         };
//         let name_slice = unsafe { std::slice::from_raw_parts(name_ptr, name_len) };
//         let name = String::from_utf16_lossy(name_slice);

//         let is_dir = (record.FileAttributes & 0x10) != 0;
//         let file_ref = record.FileReferenceNumber as u64;
//         let parent_ref = record.ParentFileReferenceNumber as u64;
//         let reason = record.Reason;

//         if reason & USN_REASON_FILE_DELETE != 0 {
//             let _ = self.sender.send(IndexEvent::Deleted(file_ref));
//             return;
//         }

//         let kind = if is_dir { FileKind::Directory } else { FileKind::File };

//         // Rename new name = could be a rename OR a move to different folder
//         if reason & USN_REASON_RENAME_NEW_NAME != 0 {
//             let _ = self.sender.send(IndexEvent::Moved {
//                 file_ref,
//                 new_parent_ref: parent_ref,
//                 name: name.clone(),
//                 kind: kind.clone(),
//             });
//             return;
//         }

//         if reason & USN_REASON_FILE_CREATE != 0 {
//             let new_record = FileRecord {
//                 file_ref,
//                 parent_ref,
//                 name,
//                 kind,
//                 size: 0,
//                 full_path: std::path::PathBuf::new(),
//             };
//             let _ = self.sender.send(IndexEvent::Created(new_record));
//         }
//     }
// }

// impl Drop for UsnWatcher {
//     fn drop(&mut self) {
//         unsafe { windows::Win32::Foundation::CloseHandle(self.handle).ok() };
//     }
// }


use std::mem;
use std::sync::{atomic::Ordering, Arc};
use std::time::Duration;
use crossbeam_channel::Sender;
use windows::{
    core::PCWSTR,
    Win32::Foundation::HANDLE,
    Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    },
    Win32::System::Ioctl::{
        FSCTL_QUERY_USN_JOURNAL, FSCTL_READ_USN_JOURNAL,
        READ_USN_JOURNAL_DATA_V0, USN_JOURNAL_DATA_V0, USN_RECORD_V2,
        USN_REASON_FILE_CREATE, USN_REASON_FILE_DELETE,
        USN_REASON_RENAME_NEW_NAME, USN_REASON_RENAME_OLD_NAME,
    },
    Win32::System::IO::DeviceIoControl,
};
use crate::mft::types::{FileKind, FileRecord, IndexEvent, JournalCheckpoint, NtfsDrive};

const BUFFER_SIZE: usize = 64 * 1024;

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub struct UsnWatcher {
    handle: HANDLE,
    drive: NtfsDrive,
    sender: Sender<IndexEvent>,
    pub next_usn: i64,
    pub journal_id: u64,
}

impl UsnWatcher {
    pub fn new(
        drive: &NtfsDrive,
        sender: Sender<IndexEvent>,
    ) -> windows::core::Result<Self> {
        Self::new_from(drive, sender, None)
    }

    pub fn new_from(
        drive: &NtfsDrive,
        sender: Sender<IndexEvent>,
        checkpoint: Option<&JournalCheckpoint>,
    ) -> windows::core::Result<Self> {
        let path: Vec<u16> = drive.device_path.encode_utf16().chain(Some(0)).collect();

        let handle = unsafe {
            CreateFileW(
                PCWSTR(path.as_ptr()),
                0x0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS,
                None,
            )?
        };

        let mut journal_data: USN_JOURNAL_DATA_V0 = unsafe { mem::zeroed() };
        let mut bytes_returned = 0u32;

        unsafe {
            DeviceIoControl(
                handle,
                FSCTL_QUERY_USN_JOURNAL,
                None, 0,
                Some(&mut journal_data as *mut _ as *mut _),
                mem::size_of::<USN_JOURNAL_DATA_V0>() as u32,
                Some(&mut bytes_returned),
                None,
            )?;
        }

        // If checkpoint matches current journal and USN is still in range, resume.
        // Otherwise start from current position (don't replay old history).
        let next_usn = if let Some(cp) = checkpoint {
            if cp.journal_id != journal_data.UsnJournalID {
                return Err(windows::core::Error::new(
                    windows::Win32::Foundation::ERROR_JOURNAL_NOT_ACTIVE.into(),
                    "Journal ID mismatch — rescan needed",
                ));
            }
            if cp.next_usn < journal_data.FirstUsn || cp.next_usn > journal_data.NextUsn {
                return Err(windows::core::Error::new(
                    windows::Win32::Foundation::ERROR_JOURNAL_NOT_ACTIVE.into(),
                    "Saved USN outside journal range — rescan needed",
                ));
            }
            cp.next_usn
        } else {
            journal_data.NextUsn
        };

        Ok(Self {
            handle,
            drive: drive.clone(),
            sender,
            next_usn,
            journal_id: journal_data.UsnJournalID,
        })
    }

    pub fn checkpoint(&self) -> JournalCheckpoint {
        JournalCheckpoint {
            next_usn: self.next_usn,
            journal_id: self.journal_id,
            drive_letter: self.drive.letter,
        }
    }

    pub fn run(&mut self) {
        let mut buffer = vec![0u8; BUFFER_SIZE];
        loop {
            std::thread::sleep(Duration::from_millis(500));
            let _ = self.poll(&mut buffer);
        }
    }

    pub fn run_shared(&mut self, heartbeat: Arc<std::sync::atomic::AtomicU64>) {
        let mut buffer = vec![0u8; BUFFER_SIZE];
        let mut last: Option<JournalCheckpoint> = None;
        let mut consecutive_fails = 0u32;
        loop {
            std::thread::sleep(Duration::from_millis(500));
            let ok = self.poll(&mut buffer);
            if ok {
                consecutive_fails = 0;
                heartbeat.store(now_millis(), Ordering::Relaxed);
            } else {
                consecutive_fails += 1;
                // ~4s of failing reads = the USN journal is gone or the
                // handle broke (chkdsk/defrag/full/no journal). Stop the
                // thread so the GUI watchdog flips the app back into the
                // scan state instead of silently serving stale results.
                if consecutive_fails >= 8 {
                    heartbeat.store(0, Ordering::Relaxed);
                    return;
                }
            }
            let cp = self.checkpoint();
            // Only push a checkpoint when it actually advanced, so idle runs
            // don't wake the consumer or mark the cache dirty on every tick.
            if last.as_ref() != Some(&cp) {
                let _ = self.sender.send(IndexEvent::Checkpoint(cp.clone()));
                last = Some(cp);
            }
        }
    }

    /// Drain all pending journal entries — used for delta catch-up on startup
    pub fn drain(&mut self) -> usize {
        let mut buffer = vec![0u8; BUFFER_SIZE];
        let mut count = 0;
        loop {
            let before = self.next_usn;
            let _ = self.poll(&mut buffer);
            if self.next_usn == before {
                break;
            }
            count += 1;
        }
        count
    }

    /// Returns false when the journal read itself failed (not when it is
    /// merely idle — an idle read returning no records is a success).
    fn poll(&mut self, buffer: &mut Vec<u8>) -> bool {
        let read_data = READ_USN_JOURNAL_DATA_V0 {
            StartUsn: self.next_usn,
            ReasonMask: USN_REASON_FILE_CREATE
                | USN_REASON_FILE_DELETE
                | USN_REASON_RENAME_NEW_NAME
                | USN_REASON_RENAME_OLD_NAME,
            ReturnOnlyOnClose: 0,
            Timeout: 0,
            BytesToWaitFor: 0,
            UsnJournalID: self.journal_id,
        };

        let mut bytes_returned = 0u32;
        let ok = unsafe {
            DeviceIoControl(
                self.handle,
                FSCTL_READ_USN_JOURNAL,
                Some(&read_data as *const _ as *const _),
                mem::size_of::<READ_USN_JOURNAL_DATA_V0>() as u32,
                Some(buffer.as_mut_ptr() as *mut _),
                BUFFER_SIZE as u32,
                Some(&mut bytes_returned),
                None,
            )
        };

        if ok.is_err() {
            return false;
        }
        if bytes_returned <= 8 {
            return true; // no new records — healthy, idle
        }

        self.next_usn = i64::from_ne_bytes(buffer[0..8].try_into().unwrap());

        let returned = bytes_returned as usize;
        let mut offset = 8usize;
        while offset + mem::size_of::<USN_RECORD_V2>() <= returned {
            let record = unsafe {
                &*(buffer.as_ptr().add(offset) as *const USN_RECORD_V2)
            };
            let rec_len = record.RecordLength as usize;
            // Malformed/partial tail: never walk past the returned buffer and
            // never trust a name pointer that extends past its own record.
            if rec_len < mem::size_of::<USN_RECORD_V2>() || offset + rec_len > returned {
                break;
            }
            self.process_record(record, buffer, offset, rec_len);
            offset += rec_len;
        }

        true
    }

    fn process_record(&self, record: &USN_RECORD_V2, buffer: &[u8], offset: usize, rec_len: usize) {
        let name_offset = record.FileNameOffset as usize;
        let name_bytes = record.FileNameLength as usize;
        if name_bytes == 0 || name_offset > rec_len || name_bytes > rec_len - name_offset {
            return; // out-of-bounds name field — ignore this record
        }
        let name_len = name_bytes / 2;
        let name_ptr = unsafe {
            buffer.as_ptr().add(offset + name_offset) as *const u16
        };
        let name_slice = unsafe { std::slice::from_raw_parts(name_ptr, name_len) };
        let name = String::from_utf16_lossy(name_slice);

        let is_dir = (record.FileAttributes & 0x10) != 0;
        let file_ref = record.FileReferenceNumber as u64;
        let parent_ref = record.ParentFileReferenceNumber as u64;
        let reason = record.Reason;

        if reason & USN_REASON_FILE_DELETE != 0 {
            let _ = self.sender.send(IndexEvent::Deleted {
                drive_letter: self.drive.letter,
                file_ref,
            });
            return;
        }

        let kind = if is_dir { FileKind::Directory } else { FileKind::File };

        // Rename new name = could be a rename OR a move to different folder
        if reason & USN_REASON_RENAME_NEW_NAME != 0 {
            let _ = self.sender.send(IndexEvent::Moved {
                drive_letter: self.drive.letter,
                file_ref,
                new_parent_ref: parent_ref,
                name: name.clone(),
                kind: kind.clone(),
            });
            return;
        }

        if reason & USN_REASON_FILE_CREATE != 0 {
            let new_record = FileRecord {
                file_ref,
                parent_ref,
                name,
                kind,
            };
            let _ = self.sender.send(IndexEvent::Created {
                drive_letter: self.drive.letter,
                record: new_record,
            });
        }
    }
}

impl Drop for UsnWatcher {
    fn drop(&mut self) {
        unsafe { windows::Win32::Foundation::CloseHandle(self.handle).ok() };
    }
}