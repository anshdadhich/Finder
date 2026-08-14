
#![allow(dead_code)]

use std::mem;
use windows::{
    core::PCWSTR,
    Win32::Foundation::HANDLE,
    Win32::Storage::FileSystem::{
        CreateFileW, ReadFile, SetFilePointerEx,
        FILE_BEGIN, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_SEQUENTIAL_SCAN,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    },
    Win32::System::Ioctl::{
        FSCTL_ENUM_USN_DATA, MFT_ENUM_DATA_V0, USN_RECORD_V2,
    },
    Win32::System::IO::DeviceIoControl,
};

use crate::mft::types::NtfsDrive;

const FALLBACK_BUF: usize = 16 * 1024 * 1024;
const DIRECT_BUF: usize = 16 * 1024 * 1024;

/// Records parsed per parallel task; each core owns its slice of the buffer.
const PARSE_SEG_RECS: usize = 512;

/// One core's slice of a parsed MFT chunk. name offsets are part-relative
/// until the driver shifts them while merging parts in order.
struct ParsedPart {
    records: Vec<CompactRecord>,
    names: Vec<u16>,
}

/// Compact MFT record — no heap allocations per file.
#[derive(Clone, Copy)]
pub struct CompactRecord {
    pub file_ref: u64,
    pub parent_ref: u64,
    pub name_off: u32,
    pub name_len: u16,
    pub is_dir: bool,
}

/// Result of a full MFT scan.
pub struct ScanResult {
    pub records: Vec<CompactRecord>,
    pub name_data: Vec<u16>,
}

pub struct MftReader {
    handle: HANDLE,
    pub drive: NtfsDrive,
}

impl MftReader {
    pub fn open(drive: &NtfsDrive) -> windows::core::Result<Self> {
        let path: Vec<u16> = drive
            .device_path
            .encode_utf16()
            .chain(Some(0))
            .collect();

        let handle = unsafe {
            CreateFileW(
                PCWSTR(path.as_ptr()),
                0x80000000u32,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS,
                None,
            )?
        };

        Ok(Self {
            handle,
            drive: drive.clone(),
        })
    }

    // ---------------------------------------------------------------
    //  Primary: direct $MFT file read  (falls back to FSCTL if fails)
    // ---------------------------------------------------------------

    /// Read one disjoint, record-aligned range of $MFT from a dedicated
    /// handle and parse it into (records, names) with range-local offsets.
    /// Sequential within the stream; streams run in parallel, so the NVMe's
    /// queue depth is used instead of one ~120 MB/s serial stream.
    fn read_record_range(
        mft_wide: &[u16],
        start_rec: u64,
        end_rec: u64,
        record_size: usize,
    ) -> (Vec<CompactRecord>, Vec<u16>) {
        let mut records: Vec<CompactRecord> = Vec::with_capacity((end_rec - start_rec) as usize);
        let mut name_data: Vec<u16> = Vec::new();

        let handle = unsafe {
            CreateFileW(
                PCWSTR(mft_wide.as_ptr()),
                0x80000000u32,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                // NOTE: FILE_FLAG_NO_BUFFERING was tried here and the open is
                // rejected by NTFS on $MFT (CreateFileW fails on every
                // attempt) — the FSCTL enumeration below is the fast path
                // and this buffered direct read only remains as a fallback.
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_SEQUENTIAL_SCAN,
                None,
            )
        };
        let Ok(handle) = handle else {
            return (records, name_data);
        };

        // The handle starts at byte 0; this stream owns bytes
        // [start_rec*RS, end_rec*RS) of the MFT.
        let pos = start_rec as i64 * record_size as i64;
        let _ = unsafe { SetFilePointerEx(handle, pos, None, FILE_BEGIN) };

        let mut rec_index = start_rec;
        let mut buffer = vec![0u8; DIRECT_BUF];
        let mut leftover = 0usize;

        loop {
            let mut bytes_read = 0u32;
            let ok = unsafe {
                ReadFile(
                    handle,
                    Some(&mut buffer[leftover..]),
                    Some(&mut bytes_read),
                    None,
                )
            };
            if ok.is_err() || bytes_read == 0 {
                break;
            }

            let total = leftover + bytes_read as usize;
            let rem = ((end_rec - rec_index) as usize) * record_size;
            let usable = total.min(rem);
            let aligned = usable - (usable % record_size);

            let seg_bytes = PARSE_SEG_RECS * record_size;
            let n_segs = aligned.div_ceil(seg_bytes);
            let mut base = name_data.len() as u32;
            for s in 0..n_segs {
                let start = s * seg_bytes;
                let end = ((s + 1) * seg_bytes).min(aligned);
                let mut part =
                    Self::parse_segment(&buffer[start..end], record_size, rec_index + (start / record_size) as u64);
                for r in &mut part.records {
                    r.name_off += base;
                }
                if !part.names.is_empty() {
                    name_data.extend_from_slice(&part.names);
                    base += part.names.len() as u32;
                }
                for r in part.records.drain(..) {
                    records.push(r);
                }
            }

            rec_index += (aligned / record_size) as u64;

            if usable < total {
                // Read past this stream's range (next stream owns those
                // bytes) — discard and finish.
                break;
            }
            leftover = total - aligned;
            if leftover > 0 {
                unsafe {
                    std::ptr::copy(
                        buffer.as_ptr().add(aligned),
                        buffer.as_mut_ptr(),
                        leftover,
                    );
                }
            }
        }

        unsafe {
            windows::Win32::Foundation::CloseHandle(handle).ok();
        }
        (records, name_data)
    }

    /// Try direct sequential read of $MFT for maximum speed.
    /// Returns None if direct access is unavailable.
    pub fn scan_direct(&self) -> Option<ScanResult> {
        let record_size = self.read_mft_record_size()?;

        let mft_path = format!("{}$MFT", self.drive.root);
        let mft_size = std::fs::metadata(&mft_path).ok()?.len();
        if mft_size < record_size as u64 {
            return None;
        }
        let mft_wide: Vec<u16> = mft_path.encode_utf16().chain(Some(0)).collect();

        let total_recs = mft_size / record_size as u64;
        let streams = (total_recs / 400_000).clamp(1, 8) as usize;
        let chunk_recs = (total_recs as usize).div_ceil(streams);

        // Each stream reads + parses its own slice of the MFT on its own
        // handle; ranges are record-aligned so no record is split.
        let parts: Vec<(Vec<CompactRecord>, Vec<u16>)> = std::thread::scope(|scope| {
            let mut handles = Vec::with_capacity(streams);
            for s in 0..streams {
                let wide = mft_wide.clone();
                handles.push(scope.spawn(move || {
                    let start_rec = s as u64 * chunk_recs as u64;
                    let end_rec = ((s + 1) as u64 * chunk_recs as u64).min(total_recs);
                    Self::read_record_range(&wide, start_rec, end_rec, record_size)
                }));
            }
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });

        let mut records: Vec<CompactRecord> = Vec::with_capacity(total_recs as usize);
        let mut name_data: Vec<u16> = Vec::with_capacity(40_000_000);
        let mut base = 0u32;
        for (mut recs, names) in parts {
            for r in &mut recs {
                r.name_off += base;
            }
            name_data.extend_from_slice(&names);
            base += names.len() as u32;
            records.extend(recs);
        }

        Some(ScanResult {
            records,
            name_data,
        })
    }

    // ---------------------------------------------------------------
    //  Fallback: FSCTL_ENUM_USN_DATA  (16 MB buffer)
    // ---------------------------------------------------------------

    pub fn scan(&self) -> ScanResult {
        self.scan_with_progress(&mut |_, _| {})
    }

    pub fn scan_with_progress(&self, on_progress: &mut dyn FnMut(usize, usize)) -> ScanResult {
        let mut records: Vec<CompactRecord> = Vec::with_capacity(3_000_000);
        let mut name_data: Vec<u16> = Vec::with_capacity(40_000_000);
        let estimate = self.estimated_record_count().unwrap_or(0);

        let mut enum_data = MFT_ENUM_DATA_V0 {
            StartFileReferenceNumber: 0,
            LowUsn: 0,
            HighUsn: i64::MAX,
        };

        let mut buffer = vec![0u8; FALLBACK_BUF];

        loop {
            let mut bytes_returned: u32 = 0;

            let ok = unsafe {
                DeviceIoControl(
                    self.handle,
                    FSCTL_ENUM_USN_DATA,
                    Some(&enum_data as *const _ as *const _),
                    mem::size_of::<MFT_ENUM_DATA_V0>() as u32,
                    Some(buffer.as_mut_ptr() as *mut _),
                    FALLBACK_BUF as u32,
                    Some(&mut bytes_returned),
                    None,
                )
            };

            if let Err(e) = ok {
                let code = e.code().0 as u32;
                if code == 0x80070026 {
                    break;
                }
                eprintln!("MFT error on {}: {:?}", self.drive.letter, e);
                break;
            }

            if bytes_returned <= 8 {
                // Enumeration complete: close the bar at 100% (the estimate
                // is MFT capacity, which always overshoots the active count).
                on_progress(records.len(), records.len());
                break;
            }

            let next_ref = u64::from_ne_bytes(buffer[0..8].try_into().unwrap());
            enum_data.StartFileReferenceNumber = next_ref;

            let mut offset = 8usize;
            while offset + mem::size_of::<USN_RECORD_V2>() <= bytes_returned as usize {
                let record = unsafe {
                    &*(buffer.as_ptr().add(offset) as *const USN_RECORD_V2)
                };

                let rec_len = record.RecordLength as usize;
                if rec_len == 0 || offset + rec_len > bytes_returned as usize {
                    break;
                }

                let name_offset = record.FileNameOffset as usize;
                let name_len = record.FileNameLength as usize / 2;
                // Same guard the watcher applies: the name must live inside
                // the record — malformed input must never reach from_raw_parts.
                if name_offset + name_len * 2 > rec_len {
                    break;
                }
                let name_ptr = unsafe {
                    buffer.as_ptr().add(offset + name_offset) as *const u16
                };
                let name_slice = unsafe { std::slice::from_raw_parts(name_ptr, name_len) };

                let arena_off = name_data.len() as u32;
                name_data.extend_from_slice(name_slice);

                records.push(CompactRecord {
                    file_ref: record.FileReferenceNumber as u64,
                    parent_ref: record.ParentFileReferenceNumber as u64,
                    name_off: arena_off,
                    name_len: name_len as u16,
                    is_dir: (record.FileAttributes & 0x10) != 0,
                });

                offset += rec_len;
            }

            on_progress(records.len(), estimate);
        }

        ScanResult {
            records,
            name_data,
        }
    }

    // ---------------------------------------------------------------
    //  NTFS helpers
    // ---------------------------------------------------------------

    /// Read MFT record size from the NTFS boot sector.
    fn read_mft_record_size(&self) -> Option<usize> {
        unsafe {
            SetFilePointerEx(self.handle, 0, None, FILE_BEGIN).ok()?;
        }
        let mut boot = [0u8; 512];
        let mut br = 0u32;
        unsafe {
            ReadFile(self.handle, Some(&mut boot), Some(&mut br), None).ok()?;
        }
        if br < 512 || &boot[3..7] != b"NTFS" {
            return None;
        }

        let bytes_per_sector = u16::from_le_bytes([boot[0x0B], boot[0x0C]]) as usize;
        let sectors_per_cluster = boot[0x0D] as usize;
        let cluster_size = bytes_per_sector * sectors_per_cluster;

        let raw = boot[0x40] as i8;
        let record_size = if raw > 0 {
            raw as usize * cluster_size
        } else {
            1usize << (-(raw as i32) as usize)
        };

        Some(record_size)
    }

    /// Best-effort total MFT record *capacity* ($MFT size ÷ record size) for
    /// progress estimation. None when the file can't be read (e.g. sparse
    /// placeholder volumes) — callers then fall back to indeterminate bars.
    fn estimated_record_count(&self) -> Option<usize> {
        let mft_path = format!("{}$MFT", self.drive.root);
        let size = std::fs::metadata(&mft_path).ok()?.len();
        let rec = self.read_mft_record_size().unwrap_or(1024);
        Some((size / rec as u64) as usize)
    }

    /// Parse one record-aligned slice: each record is copied out of the
    /// shared read buffer (fixups mutate bytes), then parsed into the part's
    /// own arenas. Name offsets stay part-relative until the merge step.
    fn parse_segment(buf: &[u8], record_size: usize, first_index: u64) -> ParsedPart {
        let mut part = ParsedPart {
            records: Vec::with_capacity(PARSE_SEG_RECS),
            names: Vec::new(),
        };
        let mut scratch = vec![0u8; record_size];
        let count = buf.len() / record_size;
        for i in 0..count {
            let src = &buf[i * record_size..(i + 1) * record_size];
            scratch.copy_from_slice(src);
            if MftReader::apply_fixup(&mut scratch, record_size) {
                MftReader::parse_file_record(
                    &scratch,
                    first_index + i as u64,
                    &mut part.records,
                    &mut part.names,
                );
            }
        }
        part
    }

    /// Apply NTFS multi-sector fixup. Returns false if the record is invalid.
    fn apply_fixup(record: &mut [u8], record_size: usize) -> bool {
        if record.len() < 48 || &record[0..4] != b"FILE" {
            return false;
        }

        let fixup_off = u16::from_le_bytes([record[4], record[5]]) as usize;
        let fixup_cnt = u16::from_le_bytes([record[6], record[7]]) as usize;

        if fixup_cnt < 2 || fixup_off + fixup_cnt * 2 > record_size {
            return false;
        }

        let check = [record[fixup_off], record[fixup_off + 1]];

        for i in 1..fixup_cnt {
            let end = i * 512 - 2;
            if end + 1 >= record_size {
                break;
            }
            if record[end] != check[0] || record[end + 1] != check[1] {
                return false;
            }
            record[end] = record[fixup_off + i * 2];
            record[end + 1] = record[fixup_off + i * 2 + 1];
        }

        true
    }

    /// Parse one MFT FILE record, extracting name + parent into the arena.
    fn parse_file_record(
        record: &[u8],
        mft_index: u64,
        records: &mut Vec<CompactRecord>,
        name_data: &mut Vec<u16>,
    ) {
        let flags = u16::from_le_bytes([record[0x16], record[0x17]]);
        if flags & 0x01 == 0 {
            return;
        }
    
        let is_dir = flags & 0x02 != 0;
        let seq = u16::from_le_bytes([record[0x10], record[0x11]]) as u64;
        let file_ref = mft_index | (seq << 48);
    
        let first_attr = u16::from_le_bytes([record[0x14], record[0x15]]) as usize;
        let mut aoff = first_attr;
    
        let mut best_ns: u8 = 255;
        let mut best_name: Option<(usize, usize, u64)> = None;
    
        while aoff + 8 <= record.len() {
            let atype = u32::from_le_bytes(record[aoff..aoff + 4].try_into().unwrap());
    
            if atype == 0xFFFF_FFFF {
                break;
            }
    
            let alen =
                u32::from_le_bytes(record[aoff + 4..aoff + 8].try_into().unwrap()) as usize;
    
            if alen == 0 || aoff + alen > record.len() {
                break;
            }
    
            // Resident attribute header is 22 bytes — guard before indexing
            // [aoff+8 .. aoff+22] (crafted MFT media must not panic the scan).
            if atype == 0x30 && alen >= 22 && record[aoff + 8] == 0 {
                let vlen =
                    u32::from_le_bytes(record[aoff + 16..aoff + 20].try_into().unwrap()) as usize;
    
                let voff =
                    u16::from_le_bytes([record[aoff + 20], record[aoff + 21]]) as usize;
    
                let vs = aoff + voff;
    
                if vs + 66 <= record.len() && vlen >= 66 {
                    let parent =
                        u64::from_le_bytes(record[vs..vs + 8].try_into().unwrap());
    
                    let nlen = record[vs + 64] as usize;
                    let ns = record[vs + 65];
    
                    if vs + 66 + nlen * 2 <= record.len() {
                        if ns == 2 {
                            continue;
                        }
    
                        let priority = match ns {
                            1 => 0, // Win32
                            3 => 1, // Win32 + DOS
                            0 => 2, // POSIX
                            _ => 3,
                        };
    
                        if priority < best_ns {
                            best_ns = priority;
                            best_name = Some((vs + 66, nlen, parent));
                        
                            if priority == 0 {
                                break; // Win32 name → best possible
                            }
                        }
                    }
                }
            }
    
            aoff += alen;
        }
    
        if let Some((name_pos, nlen, parent)) = best_name {
            let arena_off = name_data.len() as u32;
    
            for i in 0..nlen {
                let p = name_pos + i * 2;
                name_data.push(u16::from_le_bytes([record[p], record[p + 1]]));
            }
    
            records.push(CompactRecord {
                file_ref,
                parent_ref: parent,
                name_off: arena_off,
                name_len: nlen as u16,
                is_dir,
            });
        }
    }

}


impl Drop for MftReader {
    fn drop(&mut self) {
        unsafe { windows::Win32::Foundation::CloseHandle(self.handle).ok() };
    }
}


