// Copyright (c) 2022, Matteo Bernacchia <dev@kikijiki.com>. All rights reserved.
// This project is dual licensed under the Apache License 2.0 and the MIT license.
// See the LICENSE files in the project root for details.

use std::collections::VecDeque;
use std::ffi::{CString, OsString};
use std::mem::size_of;
use std::os::raw::c_void;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};

use tracing::warn;
use windows::core::PCSTR;
use windows::Win32::Foundation::{self, ERROR_MORE_DATA, INVALID_HANDLE_VALUE};
use windows::Win32::Storage::FileSystem::{
    self, FILE_FLAGS_AND_ATTRIBUTES, FILE_FLAG_BACKUP_SEMANTICS,
};
use windows::Win32::System::Ioctl;
use windows::Win32::System::Threading::INFINITE;
use windows::Win32::System::IO::{self, GetQueuedCompletionStatus};

use crate::volume::Volume;

#[derive(Debug, Copy, Clone, PartialEq)]
pub enum FileId {
    Normal(u64),
    Extended(FileSystem::FILE_ID_128),
}

#[repr(align(64))]
#[derive(Debug, Clone, Copy)]
struct AlignedBuffer<const N: usize>([u8; N]);

fn get_usn_record_time(timestamp: i64) -> std::time::Duration {
    std::time::Duration::from_nanos(timestamp as u64 * 100u64)
}

fn get_usn_record_name(file_name_length: u16, file_name: *const u16) -> String {
    let size = (file_name_length / 2) as usize;

    if size > 0 {
        unsafe {
            let name_u16 = std::slice::from_raw_parts(file_name, size);
            let name = std::ffi::OsString::from_wide(name_u16)
                .to_string_lossy()
                .into_owned();
            return name;
        }
    }

    return String::new();
}

fn get_file_path(volume_handle: Foundation::HANDLE, file_id: FileId) -> Option<PathBuf> {
    let (id, id_type) = match file_id {
        FileId::Normal(id) => (
            FileSystem::FILE_ID_DESCRIPTOR_0 { FileId: id as i64 },
            FileSystem::FileIdType,
        ),
        FileId::Extended(id) => (
            FileSystem::FILE_ID_DESCRIPTOR_0 { ExtendedFileId: id },
            FileSystem::ExtendedFileIdType,
        ),
    };

    let file_id_desc = FileSystem::FILE_ID_DESCRIPTOR {
        Type: id_type,
        dwSize: size_of::<FileSystem::FILE_ID_DESCRIPTOR>() as u32,
        Anonymous: id,
    };

    unsafe {
        let file_handle = FileSystem::OpenFileById(
            volume_handle,
            &file_id_desc,
            0,
            FileSystem::FILE_SHARE_READ
                | FileSystem::FILE_SHARE_WRITE
                | FileSystem::FILE_SHARE_DELETE,
            None,
            FILE_FLAG_BACKUP_SEMANTICS,
        )
        .unwrap_or(Foundation::INVALID_HANDLE_VALUE);

        if file_handle.is_invalid() {
            return None;
        }

        let mut info_buffer_size = size_of::<FileSystem::FILE_NAME_INFO>()
            + (Foundation::MAX_PATH as usize) * size_of::<u16>();
        let mut info_buffer = vec![0u8; info_buffer_size];

        let result = loop {
            let info_result = FileSystem::GetFileInformationByHandleEx(
                file_handle,
                FileSystem::FileNameInfo,
                info_buffer.as_mut_ptr() as *mut _,
                info_buffer_size as u32,
            );

            match info_result {
                Ok(_) => {
                    let (_, body, _) = info_buffer.align_to::<FileSystem::FILE_NAME_INFO>();
                    let info = &body[0];
                    let name_len = info.FileNameLength as usize / size_of::<u16>();
                    let name_u16 =
                        std::slice::from_raw_parts(info.FileName.as_ptr() as *const u16, name_len);
                    break Some(PathBuf::from(OsString::from_wide(name_u16)));
                }
                Err(err) => {
                    if err.code() == ERROR_MORE_DATA.to_hresult() {
                        // The buffer was too small, resize it and try again.
                        let required_size = info_buffer.align_to::<FileSystem::FILE_NAME_INFO>().1
                            [0]
                        .FileNameLength as usize;

                        info_buffer_size = size_of::<FileSystem::FILE_NAME_INFO>() + required_size;
                        info_buffer.resize(info_buffer_size, 0);
                    } else {
                        break None;
                    }
                }
            }
        };

        let _ = Foundation::CloseHandle(file_handle);
        result
    }
}

fn get_usn_record_path(
    volume_path: &Path,
    volume_handle: Foundation::HANDLE,
    file_name: String,
    file_id: FileId,
    parent_id: FileId,
) -> PathBuf {
    // First try to get the full path from the parent.
    // We do this because if the file was moved, computing the path from the file id
    // could return the wrong path.
    if let Some(parent_path) = get_file_path(volume_handle, parent_id) {
        return volume_path.join(parent_path.join(&file_name));
    } else {
        // If we can't get the parent path, try to get the path from the file id.
        // This can happen if the parent was deleted.
        if let Some(path) = get_file_path(volume_handle, file_id) {
            return volume_path.join(path);
        }
    }

    //warn!("Could not get path: {}", file_name);
    PathBuf::from(&file_name)
}

#[derive(Debug, Clone)]
pub struct UsnRecord {
    pub usn: i64,
    pub timestamp: std::time::Duration,
    pub file_id: FileId,
    pub parent_id: FileId,
    pub reason: u32,
    pub path: PathBuf,
}

impl UsnRecord {
    fn from_v2(journal: &Journal, rec: &Ioctl::USN_RECORD_V2) -> Self {
        let usn = rec.Usn;
        let timestamp = get_usn_record_time(rec.TimeStamp);
        let file_id = FileId::Normal(rec.FileReferenceNumber);
        let parent_id = FileId::Normal(rec.ParentFileReferenceNumber);
        let reason = rec.Reason;
        let name = get_usn_record_name(rec.FileNameLength, rec.FileName.as_ptr());
        let path = get_usn_record_path(
            &journal.volume.path,
            journal.volume_handle,
            name,
            file_id,
            parent_id,
        );

        UsnRecord {
            usn,
            timestamp,
            file_id,
            parent_id,
            reason,
            path,
        }
    }

    fn from_v3(journal: &Journal, rec: &Ioctl::USN_RECORD_V3) -> Self {
        let usn = rec.Usn;
        let timestamp = get_usn_record_time(rec.TimeStamp);
        let file_id = FileId::Extended(rec.FileReferenceNumber);
        let parent_id = FileId::Extended(rec.ParentFileReferenceNumber);
        let reason = rec.Reason;

        let name = get_usn_record_name(rec.FileNameLength, rec.FileName.as_ptr());
        let path = get_usn_record_path(
            &journal.volume.path,
            journal.volume_handle,
            name,
            file_id,
            parent_id,
        );

        UsnRecord {
            usn,
            timestamp,
            file_id,
            parent_id,
            reason,
            path,
        }
    }
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
    pub version_range: (u16, u16),
}

impl Default for JournalOptions {
    fn default() -> Self {
        JournalOptions {
            reason_mask: 0xFFFFFFFF,
            next_usn: NextUsn::Next,
            max_history_size: HistorySize::Unlimited,
            version_range: (2, 3),
        }
    }
}

pub struct Journal {
    volume: Volume,
    volume_handle: Foundation::HANDLE,
    port: Foundation::HANDLE,
    journal: Ioctl::USN_JOURNAL_DATA_V2,
    next_usn: i64,
    reason_mask: u32, // Ioctl::USN_REASON_FILE_CREATE
    history: VecDeque<UsnRecord>,
    max_history_size: usize,
    version_range: (u16, u16),
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
                FileSystem::FILE_FLAG_OVERLAPPED,
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

        let port = unsafe { IO::CreateIoCompletionPort(volume_handle, None, 0, 1)? };

        Ok(Journal {
            volume,
            volume_handle,
            port,
            journal,
            next_usn,
            reason_mask: options.reason_mask,
            history: VecDeque::new(),
            max_history_size,
            version_range: options.version_range,
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
            MinMajorVersion: u16::max(self.version_range.0, self.journal.MinSupportedMajorVersion),
            MaxMajorVersion: u16::min(self.version_range.1, self.journal.MaxSupportedMajorVersion),
        };

        let mut buffer = AlignedBuffer::<BUFFER_SIZE>([0u8; BUFFER_SIZE]);

        let mut bytes_returned = 0;
        let mut overlapped = IO::OVERLAPPED {
            ..Default::default()
        };

        unsafe {
            IO::DeviceIoControl(
                self.volume_handle,
                Ioctl::FSCTL_READ_USN_JOURNAL,
                Some(&mut read as *mut _ as *mut c_void),
                size_of::<Ioctl::READ_USN_JOURNAL_DATA_V1>() as u32,
                Some(&mut buffer as *mut _ as *mut c_void),
                BUFFER_SIZE as u32,
                Some(&mut bytes_returned),
                Some(&mut overlapped),
            )?;

            // NOTE: Switched to overlapped IO while investigating a bug,
            // but it's not needed (we just wait immediately anyway).

            // Wait for the operation to complete.
            let mut key = 0usize;
            let mut overlapped = 0 as *mut _;
            GetQueuedCompletionStatus(
                self.port,
                &mut bytes_returned,
                &mut key,
                &mut overlapped,
                INFINITE,
            )?;
        }

        let next_usn = i64::from_le_bytes(buffer.0[0..8].try_into().unwrap());
        if next_usn == 0 || next_usn < self.next_usn {
            return Ok(results);
        } else {
            self.next_usn = next_usn;
        }

        let mut offset = 8; // sizeof(USN)
        while offset < bytes_returned {
            let (record_len, record) = unsafe {
                let record_ptr = std::mem::transmute::<*const u8, *const Ioctl::USN_RECORD_UNION>(
                    buffer.0[offset as usize..].as_ptr(),
                );

                let record_len = (*record_ptr).Header.RecordLength;
                if record_len == 0 {
                    break;
                }

                let record = match (*record_ptr).Header.MajorVersion {
                    2 => Some(UsnRecord::from_v2(&self, &(*record_ptr).V2)),
                    3 => Some(UsnRecord::from_v3(&self, &(*record_ptr).V3)),
                    _ => None,
                };

                (record_len, record)
            };

            if let Some(record) = record {
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
            }

            offset += record_len;
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
            let _ = Foundation::CloseHandle(self.port);
        }
    }
}

#[cfg(test)]
mod test {
    use core::panic;
    use std::fs::File;
    use std::io::Write;

    use tracing_subscriber::FmtSubscriber;

    use crate::errors::NtfsReaderResult;

    use super::*;

    fn init_tracing() {
        let subscriber = FmtSubscriber::builder()
            .with_max_level(tracing::Level::TRACE)
            .without_time()
            .finish();
        let _ = tracing::subscriber::set_global_default(subscriber);
    }

    fn make_journal(version: u16, reason_mask: u32) -> NtfsReaderResult<Journal> {
        let volume = Volume::new("\\\\?\\C:")?;
        let options = JournalOptions {
            version_range: (version, version),
            reason_mask,
            ..JournalOptions::default()
        };
        Ok(Journal::new(volume, options)?)
    }

    fn make_test_dir(name: &str, version: u16) -> NtfsReaderResult<PathBuf> {
        let name = format!("{}-v{}", name, version);
        let dir = std::env::temp_dir().canonicalize()?.join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    fn test_file_create(journal_version: u16) -> NtfsReaderResult<()> {
        init_tracing();

        let mut journal = make_journal(journal_version, Ioctl::USN_REASON_FILE_CREATE)?;
        while journal.read()?.len() > 0 {}

        /////////////////////////////////////////////////////////////////
        // PREPARE DATA

        let mut files = Vec::new();
        let mut found = Vec::new();

        let dir = make_test_dir("usn-journal-test-create", journal_version)?;

        for x in 0..10 {
            let path = dir.join(format!("usn-journal-test-create-{}.txt", x));
            File::create(&path)?.write_all(b"test")?;
            files.push(path);
        }

        /////////////////////////////////////////////////////////////////
        // TEST JOURNAL

        // Retry a few times in case there is a lot of unrelated activity.
        for _ in 0..10 {
            for result in journal.read()? {
                found.push(result.path);
            }

            if files.iter().all(|f| found.contains(f)) {
                return Ok(());
            }
        }

        panic!("The file creation was not detected");
    }

    fn test_file_move(journal_version: u16) -> NtfsReaderResult<()> {
        init_tracing();

        /////////////////////////////////////////////////////////////////
        // PREPARE DATA

        let dir = make_test_dir("usn-journal-test-move", journal_version)?;

        let path_old = dir.join("usn-journal-test-move.old");
        let path_new = path_old.with_extension("new");

        let _ = std::fs::remove_file(path_new.as_path());
        let _ = std::fs::remove_file(path_old.as_path());

        File::create(path_old.as_path())?.write_all(b"test")?;

        /////////////////////////////////////////////////////////////////
        // TEST JOURNAL

        let mut journal = make_journal(
            journal_version,
            Ioctl::USN_REASON_RENAME_OLD_NAME | Ioctl::USN_REASON_RENAME_NEW_NAME,
        )?;
        while journal.read()?.len() > 0 {}

        std::fs::rename(path_old.as_path(), path_new.as_path())?;

        // Retry a few times in case there is a lot of unrelated activity.
        for _ in 0..10 {
            for result in journal.read()? {
                if (result.path == path_new)
                    && (result.reason & Ioctl::USN_REASON_RENAME_NEW_NAME != 0)
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

    fn test_file_delete(journal_version: u16) -> NtfsReaderResult<()> {
        init_tracing();

        /////////////////////////////////////////////////////////////////
        // PREPARE DATA

        let dir = make_test_dir("usn-journal-test-delete", journal_version)?;
        let file_path = dir.join("usn-journal-test-delete.txt");
        File::create(&file_path)?.write_all(b"test")?;

        /////////////////////////////////////////////////////////////////
        // TEST JOURNAL

        let mut journal = make_journal(journal_version, Ioctl::USN_REASON_FILE_DELETE)?;
        while journal.read()?.len() > 0 {}

        // This will not work well for the files inside because the directory
        // will be gone by the time the journal is processed.
        //std::fs::remove_dir_all(&dir)?;

        std::fs::remove_file(&file_path)?;

        // Retry a few times in case there is a lot of unrelated activity.
        for _ in 0..10 {
            for result in journal.read()? {
                if result.path == file_path {
                    return Ok(());
                }
            }
        }

        panic!("The file deletion was not detected");
    }

    #[test]
    fn file_create_v2() -> NtfsReaderResult<()> {
        test_file_create(2)
    }

    #[test]
    fn file_create_v3() -> NtfsReaderResult<()> {
        test_file_create(3)
    }

    #[test]
    fn file_move_v2() -> NtfsReaderResult<()> {
        test_file_move(2)
    }

    #[test]
    fn file_move_v3() -> NtfsReaderResult<()> {
        test_file_move(3)
    }

    #[test]
    fn file_delete_v2() -> NtfsReaderResult<()> {
        test_file_delete(2)
    }

    #[test]
    fn file_delete_v3() -> NtfsReaderResult<()> {
        test_file_delete(3)
    }
}
