
#![allow(dead_code)]

use std::mem;
use windows::{
    core::PCWSTR,
    Win32::Foundation::HANDLE,
    Win32::Storage::FileSystem::{
        CreateFileW, ReadFile, SetFilePointerEx,
        FILE_BEGIN, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_SEQUENTIAL_SCAN,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    },
    Win32::System::Ioctl::{
        FSCTL_ENUM_USN_DATA, MFT_ENUM_DATA_V0, USN_RECORD_V2,
    },
    Win32::System::IO::DeviceIoControl,
};

use crate::mft::types::NtfsDrive;

const FALLBACK_BUF: usize = 4 * 1024 * 1024;
const DIRECT_BUF: usize = 4 * 1024 * 1024;

/// Compact MFT record — no heap allocations per file.
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

    /// Try direct sequential read of $MFT for maximum speed.
    /// Returns None if direct access is unavailable.
    pub fn scan_direct(&self) -> Option<ScanResult> {
        let record_size = self.read_mft_record_size()?;

        let mft_path = format!("{}$MFT", self.drive.root);
        let mft_wide: Vec<u16> = mft_path.encode_utf16().chain(Some(0)).collect();

        let mft_handle = unsafe {
            CreateFileW(
                PCWSTR(mft_wide.as_ptr()),
                0x80000000u32,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_SEQUENTIAL_SCAN,
                None,
            )
            .ok()?
        };

        let mut records: Vec<CompactRecord> = Vec::with_capacity(3_000_000);
        let mut name_data: Vec<u16> = Vec::with_capacity(40_000_000);
        let mut buffer = vec![0u8; DIRECT_BUF];
        let mut mft_index: u64 = 0;
        let mut leftover = 0usize;

        loop {
            let mut bytes_read = 0u32;
            let ok = unsafe {
                ReadFile(
                    mft_handle,
                    Some(&mut buffer[leftover..]),
                    Some(&mut bytes_read),
                    None,
                )
            };
            if ok.is_err() || bytes_read == 0 {
                break;
            }

            let total = leftover + bytes_read as usize;
            let mut offset = 0usize;

            while offset + record_size <= total {
                let applied =
                    Self::apply_fixup(&mut buffer[offset..offset + record_size], record_size);
            
                if applied {
                    Self::parse_file_record(
                        &buffer[offset..offset + record_size],
                        mft_index,
                        &mut records,
                        &mut name_data,
                    );
                }
            
                mft_index += 1;
                offset += record_size;
            }

            offset = total - (total % record_size);

            leftover = total - offset;
            if leftover > 0 {
                unsafe {
                    std::ptr::copy(
                        buffer.as_ptr().add(offset),
                        buffer.as_mut_ptr(),
                        leftover,
                    );
                }
            }

            // for (mut recs, mut names) in results {
            //     records.append(&mut recs);
            //     name_data.append(&mut names);
            // }
        }

        unsafe {
            windows::Win32::Foundation::CloseHandle(mft_handle).ok();
        }

        Some(ScanResult { records, name_data })
    }

    // ---------------------------------------------------------------
    //  Fallback: FSCTL_ENUM_USN_DATA  (4 MB buffer)
    // ---------------------------------------------------------------

    pub fn scan(&self) -> ScanResult {
        let mut records: Vec<CompactRecord> = Vec::with_capacity(3_000_000);
        let mut name_data: Vec<u16> = Vec::with_capacity(40_000_000);

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
        }

        ScanResult { records, name_data }
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
    
            if atype == 0x30 && record[aoff + 8] == 0 {
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


