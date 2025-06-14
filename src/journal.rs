// Copyright (c) 2022, Matteo Bernacchia <dev@kikijiki.com>. All rights reserved.
// This project is dual licensed under the Apache License 2.0 and the MIT license.
// See the LICENSE files in the project root for details.

use std::collections::VecDeque;
use std::ffi::{CString, OsString};
use std::mem::size_of;
use std::os::raw::c_void;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};

use windows::core::PCSTR;
use windows::Win32::Foundation::{self, ERROR_MORE_DATA};
use windows::Win32::Storage::FileSystem::{self, FILE_FLAG_BACKUP_SEMANTICS};
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

    String::new()
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
                    let name_u16 = std::slice::from_raw_parts(info.FileName.as_ptr(), name_len);
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

pub fn get_usn_reason_str(reason: u32) -> String {
    let mut reason_str = Vec::<&str>::new();

    if reason & Ioctl::USN_REASON_BASIC_INFO_CHANGE != 0 {
        reason_str.push("USN_REASON_BASIC_INFO_CHANGE");
    }
    if reason & Ioctl::USN_REASON_CLOSE != 0 {
        reason_str.push("USN_REASON_CLOSE");
    }
    if reason & Ioctl::USN_REASON_COMPRESSION_CHANGE != 0 {
        reason_str.push("USN_REASON_COMPRESSION_CHANGE");
    }
    if reason & Ioctl::USN_REASON_DATA_EXTEND != 0 {
        reason_str.push("USN_REASON_DATA_EXTEND");
    }
    if reason & Ioctl::USN_REASON_DATA_OVERWRITE != 0 {
        reason_str.push("USN_REASON_DATA_OVERWRITE");
    }
    if reason & Ioctl::USN_REASON_DATA_TRUNCATION != 0 {
        reason_str.push("USN_REASON_DATA_TRUNCATION");
    }
    if reason & Ioctl::USN_REASON_DESIRED_STORAGE_CLASS_CHANGE != 0 {
        reason_str.push("USN_REASON_DESIRED_STORAGE_CLASS_CHANGE");
    }
    if reason & Ioctl::USN_REASON_EA_CHANGE != 0 {
        reason_str.push("USN_REASON_EA_CHANGE");
    }
    if reason & Ioctl::USN_REASON_ENCRYPTION_CHANGE != 0 {
        reason_str.push("USN_REASON_ENCRYPTION_CHANGE");
    }
    if reason & Ioctl::USN_REASON_FILE_CREATE != 0 {
        reason_str.push("USN_REASON_FILE_CREATE");
    }
    if reason & Ioctl::USN_REASON_FILE_DELETE != 0 {
        reason_str.push("USN_REASON_FILE_DELETE");
    }
    if reason & Ioctl::USN_REASON_HARD_LINK_CHANGE != 0 {
        reason_str.push("USN_REASON_HARD_LINK_CHANGE");
    }
    if reason & Ioctl::USN_REASON_INDEXABLE_CHANGE != 0 {
        reason_str.push("USN_REASON_INDEXABLE_CHANGE");
    }
    if reason & Ioctl::USN_REASON_INTEGRITY_CHANGE != 0 {
        reason_str.push("USN_REASON_INTEGRITY_CHANGE");
    }
    if reason & Ioctl::USN_REASON_NAMED_DATA_EXTEND != 0 {
        reason_str.push("USN_REASON_NAMED_DATA_EXTEND");
    }
    if reason & Ioctl::USN_REASON_NAMED_DATA_OVERWRITE != 0 {
        reason_str.push("USN_REASON_NAMED_DATA_OVERWRITE");
    }
    if reason & Ioctl::USN_REASON_NAMED_DATA_TRUNCATION != 0 {
        reason_str.push("USN_REASON_NAMED_DATA_TRUNCATION");
    }
    if reason & Ioctl::USN_REASON_OBJECT_ID_CHANGE != 0 {
        reason_str.push("USN_REASON_OBJECT_ID_CHANGE");
    }
    if reason & Ioctl::USN_REASON_RENAME_NEW_NAME != 0 {
        reason_str.push("USN_REASON_RENAME_NEW_NAME");
    }
    if reason & Ioctl::USN_REASON_RENAME_OLD_NAME != 0 {
        reason_str.push("USN_REASON_RENAME_OLD_NAME");
    }
    if reason & Ioctl::USN_REASON_REPARSE_POINT_CHANGE != 0 {
        reason_str.push("USN_REASON_REPARSE_POINT_CHANGE");
    }
    if reason & Ioctl::USN_REASON_SECURITY_CHANGE != 0 {
        reason_str.push("USN_REASON_SECURITY_CHANGE");
    }
    if reason & Ioctl::USN_REASON_STREAM_CHANGE != 0 {
        reason_str.push("USN_REASON_STREAM_CHANGE");
    }
    if reason & Ioctl::USN_REASON_TRANSACTED_CHANGE != 0 {
        reason_str.push("USN_REASON_TRANSACTED_CHANGE");
    }

    reason_str.join(" | ")
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct UsnRecordExtent {
    pub offset: i64,
    pub length: i64,
}

#[derive(Debug, Clone)]
pub struct UsnRecord {
    pub usn: i64,
    pub timestamp: std::time::Duration,
    pub file_id: FileId,
    pub parent_id: FileId,
    pub reason: u32,
    pub path: PathBuf,
    pub extents: Option<Vec<UsnRecordExtent>>,
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
            extents: None,
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
            extents: None,
        }
    }

    fn from_v4_chain(
        journal: &Journal,
        v4_records: &[&Ioctl::USN_RECORD_V4],
        v3_record: &Ioctl::USN_RECORD_V3,
    ) -> Self {
        let mut record = Self::from_v3(journal, v3_record);

        if record.reason & Ioctl::USN_REASON_CLOSE == 0 {
            // The last record in the chain must have the close reason.
            // If it doesn't, something went wrong, so we just return it as is.
            return record;
        }

        let total_extents: usize = v4_records.iter().map(|r| r.NumberOfExtents as usize).sum();
        let mut extents = Vec::with_capacity(total_extents);

        for v4_rec in v4_records {
            let extent_base = &v4_rec.Extents[0] as *const Ioctl::USN_RECORD_EXTENT;

            unsafe {
                for i in 0..v4_rec.NumberOfExtents as isize {
                    let windows_extent = *extent_base.offset(i);
                    extents.push(UsnRecordExtent {
                        offset: windows_extent.Offset,
                        length: windows_extent.Length,
                    });
                }
            }
        }

        record.extents = Some(extents);
        record
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
    port: Foundation::HANDLE,
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
        })
    }

    pub fn is_range_tracking_enabled(&self) -> bool {
        self.journal.Flags & Ioctl::FLAG_USN_TRACK_MODIFIED_RANGES_ENABLE != 0
    }

    pub fn get_range_tracking_chunk_size(&self) -> u64 {
        self.journal.RangeTrackChunkSize
    }

    pub fn get_range_tracking_file_size_threshold(&self) -> i64 {
        self.journal.RangeTrackFileSizeThreshold
    }

    /// Enable range tracking for the journal.
    /// WARNINGS:
    /// - This is a persistent setting.
    /// - If it was already set, this will overwrite the settings.
    /// - There is no way to disable range tracking once set, you need to delete and recreate the journal.
    pub fn enable_range_tracking(
        &self,
        chunk_size: u64,
        file_size_threshold: i64,
    ) -> Result<(), std::io::Error> {
        let mut param = Ioctl::USN_TRACK_MODIFIED_RANGES {
            Flags: Ioctl::FLAG_USN_TRACK_MODIFIED_RANGES_ENABLE,
            Unused: 0,
            ChunkSize: chunk_size,
            FileSizeThreshold: file_size_threshold,
        };

        unsafe {
            IO::DeviceIoControl(
                self.volume_handle,
                Ioctl::FSCTL_USN_TRACK_MODIFIED_RANGES,
                Some(&mut param as *mut _ as *mut c_void),
                size_of::<Ioctl::USN_TRACK_MODIFIED_RANGES>() as u32,
                None,
                0,
                None,
                None,
            )?;
        }

        Ok(())
    }

    pub fn read(&mut self) -> Result<Vec<UsnRecord>, std::io::Error> {
        self.read_sized::<4096>()
    }

    pub fn read_sized<const BUFFER_SIZE: usize>(
        &mut self,
    ) -> Result<Vec<UsnRecord>, std::io::Error> {
        let mut results = Vec::<UsnRecord>::new();
        let mut v4_records = Vec::new();

        loop {
            let mut read = Ioctl::READ_USN_JOURNAL_DATA_V1 {
                StartUsn: self.next_usn,
                ReasonMask: self.reason_mask,
                ReturnOnlyOnClose: 0,
                Timeout: 0,
                BytesToWaitFor: 0,
                UsnJournalID: self.journal.UsnJournalID,
                MinMajorVersion: self.journal.MinSupportedMajorVersion,
                MaxMajorVersion: self.journal.MaxSupportedMajorVersion,
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

                let mut key = 0usize;
                let mut overlapped = std::ptr::null_mut();
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

            // We loop only if we are in the middle of a V4 record chain,
            // and the buffer ends before a closing V3 record.
            let mut need_loop = false;
            let mut offset = 8; // sizeof(USN)

            while offset < bytes_returned {
                let (record_len, version, record_ptr) = unsafe {
                    let record_ptr = std::mem::transmute::<*const u8, *const Ioctl::USN_RECORD_UNION>(
                        buffer.0[offset as usize..].as_ptr(),
                    );

                    let record_len = (*record_ptr).Header.RecordLength;
                    if record_len == 0 {
                        break;
                    }

                    (record_len, (*record_ptr).Header.MajorVersion, record_ptr)
                };

                match version {
                    2 => unsafe {
                        v4_records.clear();
                        if let Some(record) = Some(UsnRecord::from_v2(self, &(*record_ptr).V2)) {
                            Self::handle_history_record(self, &record);
                            results.push(record);
                        }
                    },
                    3 => unsafe {
                        if v4_records.is_empty() {
                            if let Some(record) = Some(UsnRecord::from_v3(self, &(*record_ptr).V3))
                            {
                                Self::handle_history_record(self, &record);
                                results.push(record);
                            }
                        } else {
                            let record =
                                UsnRecord::from_v4_chain(self, &v4_records, &(*record_ptr).V3);
                            Self::handle_history_record(self, &record);
                            results.push(record);
                            v4_records.clear();
                        }
                        need_loop = false;
                    },
                    4 => unsafe {
                        v4_records.push(&(*record_ptr).V4);
                        need_loop = true; // Pessimistically setting to true.
                    },
                    _ => {}
                }

                offset += record_len;
            }

            if !need_loop {
                break;
            }
        }

        Ok(results)
    }

    fn handle_history_record(&mut self, record: &UsnRecord) {
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
    }

    // Add the match_rename method needed by tests
    pub fn match_rename(&self, record: &UsnRecord) -> Option<PathBuf> {
        if record.reason & Ioctl::USN_REASON_RENAME_NEW_NAME != 0 {
            for old_record in self.history.iter().rev() {
                if old_record.file_id == record.file_id
                    && old_record.reason & Ioctl::USN_REASON_RENAME_OLD_NAME != 0
                {
                    return Some(old_record.path.clone());
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod test {
    use core::panic;
    use std::fs::File;
    use std::io::{Seek, SeekFrom, Write};
    use tracing::warn;
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

    fn make_journal(reason_mask: u32) -> NtfsReaderResult<Journal> {
        let volume = Volume::new("\\\\?\\C:")?;
        let options = JournalOptions {
            reason_mask,
            ..JournalOptions::default()
        };
        Ok(Journal::new(volume, options)?)
    }

    fn make_test_dir(name: &str) -> NtfsReaderResult<PathBuf> {
        let dir = std::env::temp_dir().canonicalize()?.join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    #[test]
    fn file_create() -> NtfsReaderResult<()> {
        init_tracing();

        let mut journal = make_journal(Ioctl::USN_REASON_FILE_CREATE)?;
        while !journal.read()?.is_empty() {}

        /////////////////////////////////////////////////////////////////
        // PREPARE DATA

        let mut files = Vec::new();
        let mut found = Vec::new();

        let dir = make_test_dir("usn-journal-test-create")?;

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

    #[test]
    fn file_move() -> NtfsReaderResult<()> {
        init_tracing();

        /////////////////////////////////////////////////////////////////
        // PREPARE DATA

        let dir = make_test_dir("usn-journal-test-move")?;

        let path_old = dir.join("usn-journal-test-move.old");
        let path_new = path_old.with_extension("new");

        let _ = std::fs::remove_file(path_new.as_path());
        let _ = std::fs::remove_file(path_old.as_path());

        File::create(path_old.as_path())?.write_all(b"test")?;

        /////////////////////////////////////////////////////////////////
        // TEST JOURNAL

        let mut journal =
            make_journal(Ioctl::USN_REASON_RENAME_OLD_NAME | Ioctl::USN_REASON_RENAME_NEW_NAME)?;
        while !journal.read()?.is_empty() {}

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

    #[test]
    fn file_delete() -> NtfsReaderResult<()> {
        init_tracing();

        /////////////////////////////////////////////////////////////////
        // PREPARE DATA

        let dir = make_test_dir("usn-journal-test-delete")?;
        let file_path = dir.join("usn-journal-test-delete.txt");
        File::create(&file_path)?.write_all(b"test")?;

        /////////////////////////////////////////////////////////////////
        // TEST JOURNAL

        let mut journal = make_journal(Ioctl::USN_REASON_FILE_DELETE)?;
        while !journal.read()?.is_empty() {}

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

    // To manually enable/monitor.
    // fsutil usn enablerangetracking c=65536 s=10485760 C:
    // fsutil usn readjournal C: tail wait | Select-String "usn-journal-test-ranges" -Context 1,12

    #[test]
    fn file_modify_ranges() -> NtfsReaderResult<()> {
        init_tracing();

        let mut journal = make_journal(
            Ioctl::USN_REASON_DATA_EXTEND
                | Ioctl::USN_REASON_DATA_OVERWRITE
                | Ioctl::USN_REASON_DATA_TRUNCATION,
        )?;

        if !journal.is_range_tracking_enabled() {
            // Let's not touch the existing journal by default.
            //journal.enable_range_tracking(16384, 10 * 1024 * 1024)?;
            warn!("Range tracking is not enabled. Please enable it to test range tracking.",);
            return Ok(());
        }

        /////////////////////////////////////////////////////////////////
        // PREPARE DATA

        let dir = make_test_dir("usn-journal-test-ranges")?;
        let file_path = dir.join("usn-journal-test-ranges.txt");

        let threshold = journal.get_range_tracking_file_size_threshold() as usize;
        let chunk_size = journal.get_range_tracking_chunk_size() as usize;

        // Create initial large file
        let initial_content = vec![b'a'; threshold * 2];
        File::create(&file_path)?.write_all(&initial_content)?;

        /////////////////////////////////////////////////////////////////
        // TEST JOURNAL

        // Clear existing records
        while !journal.read()?.is_empty() {}

        // Modify the file in different ranges with large chunks
        let mut file = std::fs::OpenOptions::new().write(true).open(&file_path)?;

        // Write at the beginning
        let start_content = vec![b'b'; chunk_size * 2];
        file.write_all(&start_content)?;

        // Write at the end
        file.seek(SeekFrom::End(0))?;
        let end_content = vec![b'c'; chunk_size * 2];
        file.write_all(&end_content)?;

        // Cleanup
        std::fs::remove_dir_all(&dir)?;

        // Retry a few times in case there is a lot of unrelated activity
        for _ in 0..10 {
            for result in journal.read()? {
                if result.path == file_path {
                    // Should have extents information
                    if let Some(extents) = result.extents {
                        if !extents.is_empty() {
                            return Ok(());
                        }
                    }
                }
            }
        }

        panic!("No modified ranges were detected");
    }
}
