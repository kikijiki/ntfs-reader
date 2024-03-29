// Copyright (c) 2022, Matteo Bernacchia <dev@kikijiki.com>. All rights reserved.
// This project is dual licensed under the Apache License 2.0 and the MIT license.
// See the LICENSE files in the project root for details.

use std::collections::VecDeque;
use std::ffi::CString;
use std::mem::size_of;
use std::os::raw::c_void;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};

use windows::core::PCSTR;
use windows::Win32::Foundation;
use windows::Win32::Storage::FileSystem::{self, FILE_FLAGS_AND_ATTRIBUTES};
use windows::Win32::System::Ioctl;
use windows::Win32::System::IO;

use crate::volume::Volume;

#[repr(align(64))]
#[derive(Debug, Clone, Copy)]
struct AlignedBuffer<const N: usize>([u8; N]);

fn get_usn_record_time(record: &Ioctl::USN_RECORD_V3) -> std::time::Duration {
    std::time::Duration::from_nanos((record.TimeStamp as u64) * 100u64)
}

fn get_usn_record_name(record: &Ioctl::USN_RECORD_V3) -> String {
    let size = (record.FileNameLength / 2) as usize;

    if size > 0 {
        unsafe {
            let name_u16 = std::slice::from_raw_parts(record.FileName.as_ptr() as *const u16, size);
            let name = std::ffi::OsString::from_wide(name_u16)
                .to_string_lossy()
                .into_owned();
            return name;
        }
    }

    return String::new();
}

fn get_file_path(
    volume_handle: Foundation::HANDLE,
    file_id: &FileSystem::FILE_ID_128,
) -> Option<PathBuf> {
    let file_id_desc = FileSystem::FILE_ID_DESCRIPTOR {
        Type: FileSystem::ExtendedFileIdType,
        dwSize: size_of::<FileSystem::FILE_ID_DESCRIPTOR>() as u32,
        Anonymous: FileSystem::FILE_ID_DESCRIPTOR_0 {
            ExtendedFileId: *file_id,
        },
    };

    unsafe {
        let file_handle = FileSystem::OpenFileById(
            volume_handle,
            &file_id_desc,
            FileSystem::FILE_GENERIC_READ.0,
            FileSystem::FILE_SHARE_READ
                | FileSystem::FILE_SHARE_WRITE
                | FileSystem::FILE_SHARE_DELETE,
            None,
            FILE_FLAGS_AND_ATTRIBUTES::default(),
        )
        .unwrap_or(Foundation::INVALID_HANDLE_VALUE);

        if file_handle.is_invalid() {
            return None;
        }

        let info_buffer_size = size_of::<FileSystem::FILE_NAME_INFO>()
            + (Foundation::MAX_PATH as usize) * size_of::<u16>();
        let mut info_buffer = vec![0u8; info_buffer_size];
        let info_result = FileSystem::GetFileInformationByHandleEx(
            file_handle,
            FileSystem::FileNameInfo,
            &mut *info_buffer as *mut _ as *mut c_void,
            info_buffer_size as u32,
        );

        let _ = Foundation::CloseHandle(file_handle);

        match info_result {
            Ok(_) => {
                let (_, body, _) = info_buffer.align_to::<FileSystem::FILE_NAME_INFO>();
                let info = &body[0];
                let name_len = info.FileNameLength as usize / size_of::<u16>();
                let name_u16 =
                    std::slice::from_raw_parts(info.FileName.as_ptr() as *const u16, name_len);
                let path = PathBuf::from(std::ffi::OsString::from_wide(name_u16));
                return Some(path);
            }
            Err(_) => {
                return None;
            }
        }
    }
}

fn get_usn_record_path(
    volume_path: &Path,
    volume_handle: Foundation::HANDLE,
    record: &Ioctl::USN_RECORD_V3,
) -> PathBuf {
    let parent_path = get_file_path(volume_handle, &record.ParentFileReferenceNumber);
    let file_name = get_usn_record_name(&record);
    match parent_path {
        Some(path) => volume_path.join(path).join(&file_name),
        None => PathBuf::from(&file_name),
    }
}

#[derive(Debug, Clone)]
pub struct UsnRecord {
    pub usn: i64,
    pub timestamp: std::time::Duration,
    pub file_id: u128,
    pub parent_id: u128,
    pub reason: u32,
    pub path: PathBuf,
}

#[derive(Debug, Clone)]
pub enum NextUsn {
    First,
    Next,
    Custom(i64),
}

#[derive(Debug, Clone)]
pub enum HistorySize {
    Unlimited,
    Limited(usize),
}

#[derive(Debug, Clone)]
pub struct JournalOptions {
    pub reason_mask: u32,
    pub next_usn: NextUsn,
    pub max_history_size: HistorySize,
}

impl Default for JournalOptions {
    fn default() -> Self {
        JournalOptions {
            reason_mask: 0xFFFFFFFF,
            next_usn: NextUsn::Next,
            max_history_size: HistorySize::Unlimited,
        }
    }
}

pub struct Journal {
    volume: Volume,
    volume_handle: Foundation::HANDLE,
    journal: Ioctl::USN_JOURNAL_DATA_V2,
    next_usn: i64,
    reason_mask: u32, // Ioctl::USN_REASON_FILE_CREATE
    history: VecDeque<UsnRecord>,
    max_history_size: usize,
}

impl Journal {
    pub fn new(volume: Volume, options: JournalOptions) -> Result<Journal, std::io::Error> {
        let volume_handle: Foundation::HANDLE;

        unsafe {
            // Needs to be null terminated.
            let path = CString::new(volume.path.to_str().unwrap()).unwrap();

            volume_handle = FileSystem::CreateFileA(
                PCSTR::from_raw(path.as_bytes_with_nul().as_ptr()),
                (FileSystem::FILE_GENERIC_READ | FileSystem::FILE_GENERIC_WRITE).0,
                FileSystem::FILE_SHARE_READ
                    | FileSystem::FILE_SHARE_WRITE
                    | FileSystem::FILE_SHARE_DELETE,
                None,
                FileSystem::OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES::default(),
                None,
            )?;
        }

        let mut journal = Ioctl::USN_JOURNAL_DATA_V2::default();

        unsafe {
            let mut ioctl_bytes_returned = 0;
            IO::DeviceIoControl(
                volume_handle,
                Ioctl::FSCTL_QUERY_USN_JOURNAL,
                None,
                0,
                Some(&mut journal as *mut _ as *mut c_void),
                size_of::<Ioctl::USN_JOURNAL_DATA_V2>() as u32,
                Some(&mut ioctl_bytes_returned),
                None,
            )?;
        }

        let next_usn = match options.next_usn {
            NextUsn::First => 0,
            NextUsn::Next => journal.NextUsn,
            NextUsn::Custom(usn) => usn,
        };

        let max_history_size = match options.max_history_size {
            HistorySize::Unlimited => 0,
            HistorySize::Limited(size) => size,
        };

        Ok(Journal {
            volume,
            volume_handle,
            journal,
            next_usn,
            reason_mask: options.reason_mask,
            history: VecDeque::new(),
            max_history_size,
        })
    }

    pub fn read(&mut self) -> Result<Vec<UsnRecord>, std::io::Error> {
        self.read_sized::<4096>()
    }

    pub fn read_sized<const BUFFER_SIZE: usize>(
        &mut self,
    ) -> Result<Vec<UsnRecord>, std::io::Error> {
        let mut results = Vec::<UsnRecord>::new();

        let mut read = Ioctl::READ_USN_JOURNAL_DATA_V1 {
            StartUsn: self.next_usn,
            ReasonMask: self.reason_mask,
            ReturnOnlyOnClose: 0,
            Timeout: 0,
            BytesToWaitFor: 0,
            UsnJournalID: self.journal.UsnJournalID,
            MinMajorVersion: 3,
            MaxMajorVersion: 3,
        };

        let mut buffer = AlignedBuffer::<BUFFER_SIZE>([0u8; BUFFER_SIZE]);

        let mut ioctl_bytes_returned = 0;

        unsafe {
            let _ioctl_result = IO::DeviceIoControl(
                self.volume_handle,
                Ioctl::FSCTL_READ_USN_JOURNAL,
                Some(&mut read as *mut _ as *mut c_void),
                size_of::<Ioctl::READ_USN_JOURNAL_DATA_V1>() as u32,
                Some(&mut buffer as *mut _ as *mut c_void),
                BUFFER_SIZE as u32,
                Some(&mut ioctl_bytes_returned),
                None,
            )?;
        }

        let next_usn = i64::from_le_bytes(buffer.0[0..8].try_into().unwrap());
        if next_usn == 0 || next_usn < self.next_usn {
            return Ok(results);
        } else {
            self.next_usn = next_usn;
        }

        let mut offset = 8; // sizeof(USN)
        while offset < ioctl_bytes_returned {
            let record;
            let record_length;

            unsafe {
                let record_raw = std::mem::transmute::<*const u8, *const Ioctl::USN_RECORD_UNION>(
                    buffer.0[offset as usize..].as_ptr(),
                );
                let header = &(*record_raw).Header;

                if header.RecordLength == 0 || header.MajorVersion != 3 {
                    break;
                }

                record_length = header.RecordLength;
                record = &(*record_raw).V3;
            }

            let record = UsnRecord {
                usn: record.Usn,
                timestamp: get_usn_record_time(&record),
                file_id: u128::from_le_bytes(record.FileReferenceNumber.Identifier),
                parent_id: u128::from_le_bytes(record.ParentFileReferenceNumber.Identifier),
                reason: record.Reason,
                path: get_usn_record_path(&self.volume.path, self.volume_handle, &record),
            };

            if record.reason
                & (Ioctl::USN_REASON_RENAME_OLD_NAME
                    | Ioctl::USN_REASON_HARD_LINK_CHANGE
                    | Ioctl::USN_REASON_REPARSE_POINT_CHANGE)
                != 0
            {
                if self.max_history_size > 0 && self.history.len() >= self.max_history_size {
                    self.history.pop_front();
                }
                self.history.push_back(record.clone());
            }

            results.push(record);
            offset += record_length;
        }

        Ok(results)
    }

    pub fn match_rename(&self, record: &UsnRecord) -> Option<PathBuf> {
        if record.reason & Ioctl::USN_REASON_RENAME_NEW_NAME == 0 {
            return None;
        }

        match self
            .history
            .iter()
            .find(|r| r.file_id == record.file_id && r.usn < record.usn)
        {
            Some(r) => Some(r.path.clone()),
            None => None,
        }
    }

    pub fn trim_history(&mut self, min_usn: Option<i64>) {
        match min_usn {
            Some(usn) => self.history.retain(|r| r.usn > usn),
            None => self.history.clear(),
        }
    }

    pub fn get_next_usn(&self) -> i64 {
        self.next_usn
    }

    pub fn get_reason_str(reason: u32) -> String {
        let mut reason_str = String::new();

        if reason & Ioctl::USN_REASON_BASIC_INFO_CHANGE != 0 {
            reason_str.push_str("USN_REASON_BASIC_INFO_CHANGE ");
        }
        if reason & Ioctl::USN_REASON_CLOSE != 0 {
            reason_str.push_str("USN_REASON_CLOSE ");
        }
        if reason & Ioctl::USN_REASON_COMPRESSION_CHANGE != 0 {
            reason_str.push_str("USN_REASON_COMPRESSION_CHANGE ");
        }
        if reason & Ioctl::USN_REASON_DATA_EXTEND != 0 {
            reason_str.push_str("USN_REASON_DATA_EXTEND ");
        }
        if reason & Ioctl::USN_REASON_DATA_OVERWRITE != 0 {
            reason_str.push_str("USN_REASON_DATA_OVERWRITE ");
        }
        if reason & Ioctl::USN_REASON_DATA_TRUNCATION != 0 {
            reason_str.push_str("USN_REASON_DATA_TRUNCATION ");
        }
        if reason & Ioctl::USN_REASON_DESIRED_STORAGE_CLASS_CHANGE != 0 {
            reason_str.push_str("USN_REASON_DESIRED_STORAGE_CLASS_CHANGE ");
        }
        if reason & Ioctl::USN_REASON_EA_CHANGE != 0 {
            reason_str.push_str("USN_REASON_EA_CHANGE ");
        }
        if reason & Ioctl::USN_REASON_ENCRYPTION_CHANGE != 0 {
            reason_str.push_str("USN_REASON_ENCRYPTION_CHANGE ");
        }
        if reason & Ioctl::USN_REASON_FILE_CREATE != 0 {
            reason_str.push_str("USN_REASON_FILE_CREATE ");
        }
        if reason & Ioctl::USN_REASON_FILE_DELETE != 0 {
            reason_str.push_str("USN_REASON_FILE_DELETE ");
        }
        if reason & Ioctl::USN_REASON_HARD_LINK_CHANGE != 0 {
            reason_str.push_str("USN_REASON_HARD_LINK_CHANGE ");
        }
        if reason & Ioctl::USN_REASON_INDEXABLE_CHANGE != 0 {
            reason_str.push_str("USN_REASON_INDEXABLE_CHANGE ");
        }
        if reason & Ioctl::USN_REASON_INTEGRITY_CHANGE != 0 {
            reason_str.push_str("USN_REASON_INTEGRITY_CHANGE ");
        }
        if reason & Ioctl::USN_REASON_NAMED_DATA_EXTEND != 0 {
            reason_str.push_str("USN_REASON_NAMED_DATA_EXTEND ");
        }
        if reason & Ioctl::USN_REASON_NAMED_DATA_OVERWRITE != 0 {
            reason_str.push_str("USN_REASON_NAMED_DATA_OVERWRITE ");
        }
        if reason & Ioctl::USN_REASON_NAMED_DATA_TRUNCATION != 0 {
            reason_str.push_str("USN_REASON_NAMED_DATA_TRUNCATION ");
        }
        if reason & Ioctl::USN_REASON_OBJECT_ID_CHANGE != 0 {
            reason_str.push_str("USN_REASON_OBJECT_ID_CHANGE ");
        }
        if reason & Ioctl::USN_REASON_RENAME_NEW_NAME != 0 {
            reason_str.push_str("USN_REASON_RENAME_NEW_NAME ");
        }
        if reason & Ioctl::USN_REASON_RENAME_OLD_NAME != 0 {
            reason_str.push_str("USN_REASON_RENAME_OLD_NAME ");
        }
        if reason & Ioctl::USN_REASON_REPARSE_POINT_CHANGE != 0 {
            reason_str.push_str("USN_REASON_REPARSE_POINT_CHANGE ");
        }
        if reason & Ioctl::USN_REASON_SECURITY_CHANGE != 0 {
            reason_str.push_str("USN_REASON_SECURITY_CHANGE ");
        }
        if reason & Ioctl::USN_REASON_STREAM_CHANGE != 0 {
            reason_str.push_str("USN_REASON_STREAM_CHANGE ");
        }
        if reason & Ioctl::USN_REASON_TRANSACTED_CHANGE != 0 {
            reason_str.push_str("USN_REASON_TRANSACTED_CHANGE ");
        }

        reason_str
    }
}

impl Drop for Journal {
    fn drop(&mut self) {
        unsafe {
            let _ = Foundation::CloseHandle(self.volume_handle);
        }
    }
}

#[cfg(test)]
mod test {
    use std::fs::File;
    use std::io::Write;

    use crate::errors::NtfsReaderResult;

    use super::*;

    #[test]
    fn file_create() -> NtfsReaderResult<()> {
        let volume = Volume::new("\\\\?\\C:")?;
        let mut journal = Journal::new(volume, JournalOptions::default())?;
        let _ = journal.read()?;

        // Make sure there is something to read.
        for x in 0..10 {
            let path = std::env::temp_dir().join(format!("usn-journal-test-{}.txt", x));
            File::create(path)?.write_all(b"test")?;
        }

        let results = journal.read()?;
        assert!(results.len() >= 10);

        Ok(())
    }

    #[test]
    fn file_move() -> NtfsReaderResult<()> {
        let volume = Volume::new("\\\\?\\C:")?;
        let mut journal = Journal::new(volume, JournalOptions::default())?;

        let path_old = std::env::temp_dir()
            .canonicalize()?
            .join("usn-journal-test-move.txt");
        let path_new = path_old.with_extension("moved");

        let _ = std::fs::remove_file(path_new.as_path());
        let _ = std::fs::remove_file(path_old.as_path());

        File::create(path_old.as_path())?.write_all(b"test")?;

        let _ = journal.read()?;
        std::fs::rename(path_old.as_path(), path_new.as_path())?;

        // Retry a few times in case there is a lot of unrelated activity.
        for _ in 0..10 {
            for result in journal.read()? {
                if (result.reason & Ioctl::USN_REASON_RENAME_NEW_NAME != 0)
                    && result.path == path_new
                {
                    if let Some(path) = journal.match_rename(&result) {
                        assert_eq!(path, path_old);
                        return Ok(());
                    } else {
                        panic!("No old path found for {}", result.path.to_str().unwrap());
                    }
                }
            }
        }

        panic!("The file move was not detected");
    }
}
