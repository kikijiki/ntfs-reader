// Copyright (c) 2022, Matteo Bernacchia <dev@kikijiki.com>. All rights reserved.
// This project is dual licensed under the Apache License 2.0 and the MIT license.
// See the LICENSE files in the project root for details.

//! The USN change journal of a volume.

use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::mem::size_of;
use std::os::raw::c_void;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    self, ERROR_JOURNAL_DELETE_IN_PROGRESS, ERROR_JOURNAL_ENTRY_DELETED, ERROR_JOURNAL_NOT_ACTIVE,
    ERROR_MORE_DATA,
};
use windows::Win32::Storage::FileSystem::{
    self, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
};
use windows::Win32::System::Ioctl;
use windows::Win32::System::IO;

use crate::{
    api::FileId,
    errors::{NtfsReaderError, NtfsReaderResult},
    usn::{parse_usn_records, Reason, UsnRecord},
    volume::Volume,
};

/// The Win32 error code inside a `windows` error: the low 16 bits of an `HRESULT_FROM_WIN32`
/// value, or the whole `HRESULT` for any other kind.
fn win32_code(err: &windows::core::Error) -> u32 {
    let hresult = err.code().0 as u32;
    if hresult & 0xFFFF_0000 == 0x8007_0000 {
        hresult & 0xFFFF
    } else {
        hresult
    }
}

/// Map a failed Win32 call to this crate's error: the journal-specific cases get their own
/// variant, access denied becomes `AccessDenied`, anything else is an `Io` error carrying the OS
/// error code.
fn map_windows_error(err: windows::core::Error) -> NtfsReaderError {
    let code = win32_code(&err);
    if code == ERROR_JOURNAL_NOT_ACTIVE.0 {
        NtfsReaderError::JournalNotActive
    } else if code == ERROR_JOURNAL_ENTRY_DELETED.0 {
        NtfsReaderError::JournalEntryDeleted
    } else if code == ERROR_JOURNAL_DELETE_IN_PROGRESS.0 {
        NtfsReaderError::JournalDeleteInProgress
    } else {
        NtfsReaderError::from(std::io::Error::from_raw_os_error(code as i32))
    }
}

/// Decode a `FILE_NAME_INFO` buffer (as filled in by `GetFileInformationByHandleEx(...,
/// FileNameInfo, ...)`) into a `PathBuf`, or `None` if it is malformed. Bounds-checks the declared
/// `FileNameLength` against the buffer before slicing the name out of it.
fn parse_file_name_info(buffer: &[u8]) -> Option<PathBuf> {
    let (length, rest) = buffer.split_first_chunk::<4>()?;
    let file_name_length = u32::from_le_bytes(*length) as usize;
    let name = rest.get(..file_name_length)?;

    let name_u16: Vec<u16> = name
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .collect();

    Some(PathBuf::from(OsString::from_wide(&name_u16)))
}

#[cfg(feature = "internals")]
thread_local! {
    static PATH_LOOKUPS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// How many times the calling thread has looked a path up through a file handle (the two
/// `OpenFileById` attempts of a [`Journal::resolve_path`] call). A test reads it before and after
/// a `read` to check that reading opens no handles.
#[cfg(feature = "internals")]
pub fn path_lookups_on_this_thread() -> u64 {
    PATH_LOOKUPS.with(|count| count.get())
}

/// The volume-relative path of the file `file_id` names (for example `\dir\file.txt`), or `None`
/// if it cannot be opened: it no longer exists, or the id is not one this volume understands.
fn get_file_path(volume_handle: Foundation::HANDLE, file_id: FileId) -> Option<PathBuf> {
    #[cfg(feature = "internals")]
    PATH_LOOKUPS.with(|count| count.set(count.get() + 1));

    // NTFS ids fit in 64 bits (the file reference); anything wider is opened as an extended id.
    let (id, id_type) = match file_id.as_reference() {
        Some(reference) => (
            FileSystem::FILE_ID_DESCRIPTOR_0 {
                FileId: reference as i64,
            },
            FileSystem::FileIdType,
        ),
        None => (
            FileSystem::FILE_ID_DESCRIPTOR_0 {
                ExtendedFileId: FileSystem::FILE_ID_128 {
                    Identifier: file_id.as_u128().to_le_bytes(),
                },
            },
            FileSystem::ExtendedFileIdType,
        ),
    };

    let file_id_desc = FileSystem::FILE_ID_DESCRIPTOR {
        Type: id_type,
        dwSize: size_of::<FileSystem::FILE_ID_DESCRIPTOR>() as u32,
        Anonymous: id,
    };

    // SAFETY: `file_id_desc` is a fully initialised descriptor that outlives the call, and
    // `volume_handle` is the open volume handle of the `Journal` calling this. The handle the call
    // returns is closed below, on every path that reaches it.
    unsafe {
        let file_handle = FileSystem::OpenFileById(
            volume_handle,
            &file_id_desc,
            0,
            FileSystem::FILE_SHARE_READ
                | FileSystem::FILE_SHARE_WRITE
                | FileSystem::FILE_SHARE_DELETE,
            None,
            // The reparse point itself, not what it points to: a journal record names the
            // link, and following it would resolve to the target's path.
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
        )
        .unwrap_or(Foundation::INVALID_HANDLE_VALUE);

        if file_handle.is_invalid() {
            return None;
        }

        // A `Vec<u32>` so the buffer is aligned for `FILE_NAME_INFO`, whose first field is a
        // `u32`. It is viewed as bytes for parsing.
        let mut info_words = (size_of::<FileSystem::FILE_NAME_INFO>()
            + (Foundation::MAX_PATH as usize) * size_of::<u16>())
        .div_ceil(size_of::<u32>());
        let mut info_buffer = vec![0u32; info_words];

        // GetFileInformationByHandleEx reports the exact size needed on ERROR_MORE_DATA, so this
        // should never take more than one retry in practice; the cap is defense in depth against
        // a driver that somehow keeps reporting a size that still doesn't fit.
        const MAX_RETRIES: u32 = 8;
        let mut retries = 0u32;

        let result = loop {
            let info_size = info_words * size_of::<u32>();
            let info_result = FileSystem::GetFileInformationByHandleEx(
                file_handle,
                FileSystem::FileNameInfo,
                info_buffer.as_mut_ptr() as *mut _,
                info_size as u32,
            );

            match info_result {
                Ok(_) => {
                    let info_bytes =
                        std::slice::from_raw_parts(info_buffer.as_ptr() as *const u8, info_size);
                    break parse_file_name_info(info_bytes);
                }
                Err(err) => {
                    if err.code() == ERROR_MORE_DATA.to_hresult() && retries < MAX_RETRIES {
                        retries += 1;
                        // The buffer was too small: the driver stored the length it needs.
                        let required = info_buffer[0] as usize;
                        info_words = (size_of::<FileSystem::FILE_NAME_INFO>() + required)
                            .div_ceil(size_of::<u32>());
                        info_buffer.resize(info_words, 0);
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

/// The full path of a record's file: from its parent directory's path when the parent still
/// exists, otherwise from the file's own id, otherwise `None`.
fn get_usn_record_path(
    volume_path: &Path,
    volume_handle: Foundation::HANDLE,
    file_name: &OsStr,
    file_id: FileId,
    parent_id: FileId,
) -> Option<PathBuf> {
    // First try to get the full path from the parent.
    // We do this because if the file was moved, computing the path from the file id
    // could return the wrong path.
    if let Some(parent_path) = get_file_path(volume_handle, parent_id) {
        return Some(volume_path.join(parent_path.join(file_name)));
    }

    // If we can't get the parent path, try to get the path from the file id.
    // This can happen if the parent was deleted.
    if let Some(path) = get_file_path(volume_handle, file_id) {
        return Some(volume_path.join(path));
    }

    tracing::debug!("Could not get path: {}", file_name.display());
    None
}

/// A response that carries nothing past the leading USN value (`bytes_returned <=
/// size_of::<i64>()`) means the driver scanned all the way to the journal's current end within
/// this call and found nothing more; the caller is caught up, even though a non-empty response
/// can still contain zero *matching* records (reason_mask filtered them, but the driver had to
/// scan past them to get there).
fn is_caught_up(bytes_returned: u32) -> bool {
    bytes_returned as usize <= size_of::<i64>()
}

/// Add `record` to the rename history if it is a `RENAME_OLD_NAME` record, the only kind
/// [`Journal::match_rename`] looks at. `max_history_size` follows `Journal.max_history_size`:
/// `None` is unlimited, `Some(0)` keeps nothing, `Some(n)` keeps the `n` most recent entries.
fn push_history(
    history: &mut VecDeque<UsnRecord>,
    max_history_size: Option<usize>,
    record: &UsnRecord,
) {
    if !record.reason.contains(Reason::RENAME_OLD_NAME) {
        return;
    }

    match max_history_size {
        Some(0) => {}
        Some(limit) => {
            if history.len() >= limit {
                history.pop_front();
            }
            history.push_back(record.clone());
        }
        None => history.push_back(record.clone()),
    }
}

/// A journal position: the journal id it was read from plus a USN within it. Used to validate
/// that a saved `NextUsn::Custom` position still refers to the same journal generation (a journal
/// can be deleted and recreated, which resets its id and its USN numbering).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JournalPosition {
    /// The id of the journal the position was taken from.
    pub journal_id: u64,
    /// The USN within that journal.
    pub usn: i64,
}

/// Where a [`Journal`] starts reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NextUsn {
    /// The oldest record still in the journal.
    First,
    /// The journal's current end: only changes made from now on.
    Next,
    /// A saved position. [`Journal::new`] fails with `JournalIdMismatch` if the journal was
    /// recreated since.
    Custom(JournalPosition),
}

/// How many `RENAME_OLD_NAME` records a [`Journal`] remembers for
/// [`Journal::match_rename`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistorySize {
    /// Keep every one. Grows with the number of renames read.
    Unlimited,
    /// Keep the most recent `n`; older ones are dropped. `Limited(0)` keeps none.
    Limited(usize),
}

/// How a [`Journal`] is opened.
#[derive(Debug, Clone)]
pub struct JournalOptions {
    /// Which reasons to read; records with none of them are skipped by the driver. Include
    /// [`Reason::RENAME_OLD_NAME`] if you call [`Journal::match_rename`], which needs the
    /// old-name records in its history. Defaults to [`Reason::ALL`].
    pub reason_mask: Reason,
    /// Where to start. Defaults to [`NextUsn::Next`].
    pub next_usn: NextUsn,
    /// How much rename history to keep. Defaults to `Limited(4096)`.
    pub max_history_size: HistorySize,
}

impl Default for JournalOptions {
    fn default() -> Self {
        JournalOptions {
            reason_mask: Reason::ALL,
            next_usn: NextUsn::Next,
            max_history_size: HistorySize::Limited(4096),
        }
    }
}

/// The result of a `Journal::read`/`read_sized` call.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct UsnReadResult {
    /// The records this call returned, oldest first.
    pub records: Vec<UsnRecord>,
    /// `true` when this read reached the journal's current end: there is nothing more to read
    /// right now, even though `records` can still be empty on a call that did have more to scan
    /// (for example everything in this window was filtered out by `reason_mask`). Use it, not
    /// "empty result", to decide when to stop reading.
    pub caught_up: bool,
}

/// A reader over a volume's USN change journal.
pub struct Journal {
    volume: Volume,
    volume_handle: windows::core::Owned<Foundation::HANDLE>,
    journal: Ioctl::USN_JOURNAL_DATA_V2,
    next_usn: i64,
    reason_mask: Reason,
    history: VecDeque<UsnRecord>,
    max_history_size: Option<usize>,
}

impl fmt::Debug for Journal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Journal")
            .field("volume", &self.volume.path())
            .field("journal_id", &self.journal.UsnJournalID)
            .field("next_usn", &self.next_usn)
            .field("reason_mask", &self.reason_mask)
            .field("history_len", &self.history.len())
            .finish_non_exhaustive()
    }
}

impl Journal {
    /// The smallest buffer [`Journal::read_sized`] accepts: room for the leading USN (8 bytes)
    /// and the largest record a file name can produce (about 600 bytes), rounded up. A smaller
    /// buffer could hold a record the driver then cannot return, and the read would stall.
    pub const MIN_READ_BUFFER_SIZE: usize = 1024;

    /// Open the journal of `volume`.
    ///
    /// Fails with `AccessDenied` without the privileges to open the raw volume,
    /// `JournalNotActive` if the volume has no journal, and `JournalIdMismatch` if
    /// `options.next_usn` is a saved position from an older journal.
    pub fn new(volume: Volume, options: JournalOptions) -> NtfsReaderResult<Journal> {
        // Wide, not ANSI: encode_wide() round-trips any OsString Windows can produce (including
        // ill-formed UTF-16) with no panic risk. `Volume::new` rejected a path with an embedded
        // NUL, which would end the string early, so this is the whole path.
        let mut wide_path: Vec<u16> = volume.path().as_os_str().encode_wide().collect();
        wide_path.push(0);

        // Owned from the moment CreateFileW succeeds, so every `?` below closes it automatically
        // instead of leaking it.
        let volume_handle: windows::core::Owned<Foundation::HANDLE> = unsafe {
            windows::core::Owned::new(
                FileSystem::CreateFileW(
                    PCWSTR::from_raw(wide_path.as_ptr()),
                    FileSystem::FILE_GENERIC_READ.0,
                    FileSystem::FILE_SHARE_READ
                        | FileSystem::FILE_SHARE_WRITE
                        | FileSystem::FILE_SHARE_DELETE,
                    None,
                    FileSystem::OPEN_EXISTING,
                    // Timeout = 0 / BytesToWaitFor = 0 on every read below means the FSCTL never
                    // actually waits, so overlapped I/O buys nothing here - open the handle for
                    // plain synchronous I/O instead.
                    FileSystem::FILE_FLAGS_AND_ATTRIBUTES(0),
                    None,
                )
                .map_err(map_windows_error)?,
            )
        };

        let mut journal = Ioctl::USN_JOURNAL_DATA_V2::default();

        // SAFETY: `volume_handle` is open, and the output buffer is a `USN_JOURNAL_DATA_V2` of
        // exactly the size passed.
        unsafe {
            let mut ioctl_bytes_returned = 0;
            IO::DeviceIoControl(
                *volume_handle,
                Ioctl::FSCTL_QUERY_USN_JOURNAL,
                None,
                0,
                Some(&mut journal as *mut _ as *mut c_void),
                size_of::<Ioctl::USN_JOURNAL_DATA_V2>() as u32,
                Some(&mut ioctl_bytes_returned),
                None,
            )
            .map_err(map_windows_error)?;
        }

        let next_usn = match options.next_usn {
            NextUsn::First => 0,
            NextUsn::Next => journal.NextUsn,
            NextUsn::Custom(position) => {
                if position.journal_id != journal.UsnJournalID {
                    return Err(NtfsReaderError::JournalIdMismatch);
                }
                position.usn
            }
        };

        let max_history_size = match options.max_history_size {
            HistorySize::Unlimited => None,
            HistorySize::Limited(size) => Some(size),
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

    /// Read one page of records (a 4096-byte buffer's worth) from the current position and move
    /// the position past them. Call it in a loop until `caught_up`.
    pub fn read(&mut self) -> NtfsReaderResult<UsnReadResult> {
        self.read_sized(4096)
    }

    /// Like [`Journal::read`] with a buffer of `buffer_size` bytes. A bigger buffer returns more
    /// records per call. A `buffer_size` under [`Journal::MIN_READ_BUFFER_SIZE`] returns
    /// `ReadBufferTooSmall` without reading.
    pub fn read_sized(&mut self, buffer_size: usize) -> NtfsReaderResult<UsnReadResult> {
        if buffer_size < Self::MIN_READ_BUFFER_SIZE {
            return Err(NtfsReaderError::ReadBufferTooSmall {
                size: buffer_size,
                min: Self::MIN_READ_BUFFER_SIZE,
            });
        }

        let mut read = Ioctl::READ_USN_JOURNAL_DATA_V1 {
            StartUsn: self.next_usn,
            ReasonMask: self.reason_mask.bits(),
            ReturnOnlyOnClose: 0,
            Timeout: 0,
            BytesToWaitFor: 0,
            UsnJournalID: self.journal.UsnJournalID,
            MinMajorVersion: 2,
            MaxMajorVersion: u16::min(3, self.journal.MaxSupportedMajorVersion),
        };

        // The FSCTL takes the size as a `u32`.
        let buffer_size = buffer_size.min(u32::MAX as usize);
        let mut buffer = vec![0u8; buffer_size];
        let mut bytes_returned = 0u32;

        // SAFETY: `read` is a `READ_USN_JOURNAL_DATA_V1` of the size passed, `buffer` is
        // `buffer_size` bytes long, and the handle is open.
        unsafe {
            // The handle is opened for synchronous I/O (see the comment in `new`), so this
            // blocks until the FSCTL completes and never returns ERROR_IO_PENDING.
            IO::DeviceIoControl(
                *self.volume_handle,
                Ioctl::FSCTL_READ_USN_JOURNAL,
                Some(&mut read as *mut _ as *mut c_void),
                size_of::<Ioctl::READ_USN_JOURNAL_DATA_V1>() as u32,
                Some(buffer.as_mut_ptr() as *mut c_void),
                buffer_size as u32,
                Some(&mut bytes_returned),
                None,
            )
            .map_err(map_windows_error)?;
        }

        // Defense in depth: DeviceIoControl should never report more bytes written than the
        // buffer it was given, but the parser trusts the slice it is handed as the extent of
        // valid data, so never hand it more than the buffer holds.
        let bytes_returned = bytes_returned.min(buffer.len() as u32);

        let caught_up = is_caught_up(bytes_returned);

        let next_usn = i64::from_le_bytes(buffer[0..8].try_into().unwrap());
        if next_usn > self.next_usn {
            self.next_usn = next_usn;
        }

        let records = parse_usn_records(&buffer[..bytes_returned as usize])?;
        for record in &records {
            push_history(&mut self.history, self.max_history_size, record);
        }

        Ok(UsnReadResult { records, caught_up })
    }

    /// Resolve a record's full path, or `None` if it cannot be resolved (for example the file and
    /// its parent directory are both gone). A `Some` path is absolute: it starts with the volume
    /// path the `Volume` was opened with, so it is never relative to the current directory.
    ///
    /// Separate from `read`/`read_sized`: it costs one or two `OpenFileById` handle opens, so a
    /// caller reading a large journal and only occasionally needing a path is not charged for it
    /// on every record.
    ///
    /// The path is the record's parent directory as it is *now* (looked up by the parent's id)
    /// joined with the name the record carries, so a moved parent gives its new location. For a
    /// `RENAME_OLD_NAME` record that name is the old one, which no longer exists under that
    /// directory: the result is where the file used to be, not where it is. If the parent is
    /// gone, the file's own id is looked up instead, which gives the file's current path (and
    /// name) if it still exists.
    pub fn resolve_path(&self, record: &UsnRecord) -> Option<PathBuf> {
        get_usn_record_path(
            self.volume.path(),
            *self.volume_handle,
            &record.name,
            record.file_id,
            record.parent_id,
        )
    }

    /// The old name of the rename that produced `record`'s `RENAME_NEW_NAME`, if one is in
    /// history. Returns `None` if `record` is not itself a `RENAME_NEW_NAME` record, or if no
    /// matching `RENAME_OLD_NAME` history entry is found (for example it aged out of a bounded
    /// history).
    ///
    /// The history holds only `RENAME_OLD_NAME` records, so the journal must have been opened
    /// with [`Reason::RENAME_OLD_NAME`] in `reason_mask` (and `RENAME_NEW_NAME` for the records
    /// to pass in), or nothing is ever found.
    pub fn match_rename(&self, record: &UsnRecord) -> Option<OsString> {
        if !record.reason.contains(Reason::RENAME_NEW_NAME) {
            return None;
        }

        // Search from the most recent entry backward: the first (oldest) match by file_id alone
        // is not necessarily the immediately preceding rename for a file renamed more than once.
        self.history
            .iter()
            .rev()
            .find(|r| r.file_id == record.file_id && r.usn < record.usn)
            .map(|r| r.name.clone())
    }

    /// Forget rename history. `Some(usn)` keeps the entries at `usn` or later and drops the older
    /// ones; `None` clears all of it.
    pub fn trim_history(&mut self, min_usn: Option<i64>) {
        match min_usn {
            Some(usn) => self.history.retain(|r| r.usn >= usn),
            None => self.history.clear(),
        }
    }

    /// The id of the journal this `Journal` was opened against. A journal deletion and
    /// recreation changes this; a saved `JournalPosition` from a previous id is stale.
    pub fn journal_id(&self) -> u64 {
        self.journal.UsnJournalID
    }

    /// The oldest USN still valid in this journal, as of when it was opened.
    pub fn first_usn(&self) -> i64 {
        self.journal.FirstUsn
    }

    /// The USN the next `read`/`read_sized` call will start from.
    pub fn next_usn(&self) -> i64 {
        self.next_usn
    }

    /// `(journal_id(), next_usn())`, saveable and later passed back as `NextUsn::Custom` to
    /// resume from here (rejected with `JournalIdMismatch` if the journal was recreated meanwhile).
    pub fn position(&self) -> JournalPosition {
        JournalPosition {
            journal_id: self.journal_id(),
            usn: self.next_usn(),
        }
    }
}

// SAFETY: the only field that is not `Send` on its own is `volume_handle`'s raw `HANDLE`. A
// Windows handle can be used from a different thread than the one that created it, as long as it
// is not used from two threads at once. `Journal` is `Send` but not `Sync` (the raw handle keeps
// it from being `Sync`), so a `&Journal` cannot be shared across threads either: at most one
// thread uses a `Journal` at a time, whether through `&mut self` (`read`, `read_sized`,
// `trim_history`) or `&self` (`resolve_path`, which opens files by id through the handle, and
// the plain accessors). Moving a `Journal` to another thread and using it only there is sound.
unsafe impl Send for Journal {}

// No manual Drop impl: `volume_handle` is `windows::core::Owned<HANDLE>`, which closes itself.

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Foundation::ERROR_ACCESS_DENIED;

    /// A `Volume` good enough for tests that only look at `Journal::new`'s
    /// path handling or at pure `Journal` methods that never touch the
    /// volume handle.
    fn fake_volume() -> Volume {
        Volume::synthetic(PathBuf::from("\\\\?\\T:"), 4096, 0, 1024, 0)
    }

    /// Build a `Journal` for testing pure, in-memory methods (`match_rename`) without opening a
    /// real volume or journal. The null placeholder handle is safe to drop for real: `Owned`
    /// only calls `CloseHandle` when the handle is not invalid, and a null `HANDLE` is invalid.
    fn fake_journal(history: VecDeque<UsnRecord>) -> Journal {
        Journal {
            volume: fake_volume(),
            volume_handle: unsafe {
                windows::core::Owned::new(Foundation::HANDLE(std::ptr::null_mut()))
            },
            journal: Ioctl::USN_JOURNAL_DATA_V2::default(),
            next_usn: 0,
            reason_mask: Reason::EMPTY,
            history,
            max_history_size: None,
        }
    }

    fn usn_record(usn: i64, file_id: FileId, reason: Reason, name: &str) -> UsnRecord {
        UsnRecord {
            usn,
            timestamp: time::OffsetDateTime::UNIX_EPOCH,
            file_id,
            parent_id: FileId::from(0u64),
            reason,
            file_attributes: 0,
            name: name.into(),
        }
    }

    // --- match_rename returns the most recent old name, not the oldest ---

    #[test]
    fn match_rename_returns_the_most_recent_old_name_not_the_oldest() {
        let file_id = FileId::from(42u64);
        let mut history = VecDeque::new();
        // A -> B -> C: both renames' old names are in history by the time we
        // see C's RENAME_NEW_NAME record.
        history.push_back(usn_record(1, file_id, Reason::RENAME_OLD_NAME, "A"));
        history.push_back(usn_record(5, file_id, Reason::RENAME_OLD_NAME, "B"));

        let journal = fake_journal(history);
        let new_name_record = usn_record(10, file_id, Reason::RENAME_NEW_NAME, "C");

        assert_eq!(
            journal.match_rename(&new_name_record),
            Some(OsString::from("B")),
            "match_rename returned the oldest matching history entry (A) instead of the \
             most recent rename-old-name entry (B) before the second rename"
        );
    }

    #[test]
    fn match_rename_ignores_a_record_that_is_not_a_new_name() {
        let file_id = FileId::from(7u64);
        let mut history = VecDeque::new();
        history.push_back(usn_record(1, file_id, Reason::RENAME_OLD_NAME, "old"));
        let journal = fake_journal(history);

        let create = usn_record(10, file_id, Reason::FILE_CREATE, "new");

        assert_eq!(journal.match_rename(&create), None);
    }

    // --- history keeps only what match_rename reads ---

    #[test]
    fn history_keeps_only_rename_old_name_records() {
        let mut history = VecDeque::new();
        let file_id = FileId::from(1u64);
        let old = usn_record(1, file_id, Reason::RENAME_OLD_NAME, "old");
        let link = usn_record(2, file_id, Reason::HARD_LINK_CHANGE, "link");
        let reparse = usn_record(3, file_id, Reason::REPARSE_POINT_CHANGE, "reparse");
        let new = usn_record(4, file_id, Reason::RENAME_NEW_NAME, "new");

        for record in [&old, &link, &reparse, &new] {
            push_history(&mut history, None, record);
        }

        let names: Vec<_> = history.iter().map(|r| r.name.clone()).collect();
        assert_eq!(names, vec![OsString::from("old")]);
    }

    #[test]
    fn a_reparse_point_burst_does_not_push_a_rename_out_of_a_bounded_history() {
        let mut history = VecDeque::new();
        let file_id = FileId::from(1u64);
        push_history(
            &mut history,
            Some(4),
            &usn_record(1, file_id, Reason::RENAME_OLD_NAME, "old"),
        );
        for usn in 2..50 {
            let noise = usn_record(usn, FileId::from(9u64), Reason::REPARSE_POINT_CHANGE, "x");
            push_history(&mut history, Some(4), &noise);
        }
        let journal = fake_journal(history);

        let new = usn_record(100, file_id, Reason::RENAME_NEW_NAME, "new");

        assert_eq!(journal.match_rename(&new), Some(OsString::from("old")));
    }

    #[test]
    fn trim_history_some_keeps_the_entry_at_the_usn_and_drops_older_ones() {
        let file_id = FileId::from(1u64);
        let mut history = VecDeque::new();
        for usn in [10, 20, 30] {
            history.push_back(usn_record(usn, file_id, Reason::RENAME_OLD_NAME, "n"));
        }
        let mut journal = fake_journal(history);

        journal.trim_history(Some(20));

        let usns: Vec<_> = journal.history.iter().map(|r| r.usn).collect();
        assert_eq!(usns, vec![20, 30]);
    }

    #[test]
    fn trim_history_none_clears_everything() {
        let mut history = VecDeque::new();
        history.push_back(usn_record(
            10,
            FileId::from(1u64),
            Reason::RENAME_OLD_NAME,
            "n",
        ));
        let mut journal = fake_journal(history);

        journal.trim_history(None);

        assert!(journal.history.is_empty());
    }

    // `Journal::resolve_path` and `get_file_path` copy a `FILE_NAME_INFO` out of a buffer; the
    // decode is a pure function over bytes (no `align_to`, bounds-checked), testable here
    // without a real file handle.
    #[test]
    fn parse_file_name_info_rejects_a_file_name_length_that_exceeds_the_buffer() {
        // FileNameLength claims 1000 bytes; the buffer only has 4 bytes of name data after the
        // 4-byte header. It must be rejected, not read out of bounds.
        let mut buffer = vec![0u8; 8];
        buffer[0..4].copy_from_slice(&1000u32.to_le_bytes());

        assert_eq!(parse_file_name_info(&buffer), None);
    }

    #[test]
    fn parse_file_name_info_rejects_a_buffer_shorter_than_its_length_field() {
        assert_eq!(parse_file_name_info(&[1, 0]), None);
    }

    #[test]
    fn parse_file_name_info_decodes_a_well_formed_buffer() {
        let name: Vec<u16> = "child.txt".encode_utf16().collect();
        let mut buffer = ((name.len() as u32) * 2).to_le_bytes().to_vec();
        for unit in &name {
            buffer.extend_from_slice(&unit.to_le_bytes());
        }

        assert_eq!(
            parse_file_name_info(&buffer),
            Some(PathBuf::from("child.txt"))
        );
    }

    // --- errors ---

    #[test]
    fn access_denied_from_a_windows_call_is_access_denied() {
        let err = windows::core::Error::from(ERROR_ACCESS_DENIED.to_hresult());

        assert!(matches!(
            map_windows_error(err),
            NtfsReaderError::AccessDenied
        ));
    }

    #[test]
    fn a_journal_specific_windows_error_gets_its_own_variant() {
        let not_active = windows::core::Error::from(ERROR_JOURNAL_NOT_ACTIVE.to_hresult());
        let deleted = windows::core::Error::from(ERROR_JOURNAL_ENTRY_DELETED.to_hresult());
        let being_deleted =
            windows::core::Error::from(ERROR_JOURNAL_DELETE_IN_PROGRESS.to_hresult());

        assert!(matches!(
            map_windows_error(not_active),
            NtfsReaderError::JournalNotActive
        ));
        assert!(matches!(
            map_windows_error(deleted),
            NtfsReaderError::JournalEntryDeleted
        ));
        assert!(matches!(
            map_windows_error(being_deleted),
            NtfsReaderError::JournalDeleteInProgress
        ));
    }

    #[test]
    fn any_other_windows_error_is_io_with_the_os_error_code() {
        let err = windows::core::Error::from(Foundation::ERROR_FILE_NOT_FOUND.to_hresult());

        match map_windows_error(err) {
            NtfsReaderError::Io(io) => {
                assert_eq!(
                    io.raw_os_error(),
                    Some(Foundation::ERROR_FILE_NOT_FOUND.0 as i32)
                )
            }
            other => panic!("expected Io, got {other:?}"),
        }
    }

    // --- Journal options and history ---

    #[test]
    fn journal_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<Journal>();
    }

    #[test]
    fn default_journal_options_bound_history() {
        assert!(
            matches!(
                JournalOptions::default().max_history_size,
                HistorySize::Limited(_)
            ),
            "JournalOptions::default() should bound history, not be Unlimited"
        );
    }

    #[test]
    fn push_history_limited_zero_keeps_nothing() {
        let mut history = VecDeque::new();
        let record = usn_record(1, FileId::from(1u64), Reason::RENAME_OLD_NAME, "a");

        push_history(&mut history, Some(0), &record);

        assert!(
            history.is_empty(),
            "HistorySize::Limited(0) should keep no history entries, not be treated as unlimited"
        );
    }

    #[test]
    fn push_history_limited_n_keeps_the_n_most_recent() {
        let mut history = VecDeque::new();
        for i in 0..5i64 {
            let record = usn_record(
                i,
                FileId::from(1u64),
                Reason::RENAME_OLD_NAME,
                &i.to_string(),
            );
            push_history(&mut history, Some(3), &record);
        }

        let names: Vec<_> = history.iter().map(|r| r.name.clone()).collect();
        assert_eq!(
            names,
            vec![
                OsString::from("2"),
                OsString::from("3"),
                OsString::from("4")
            ]
        );
    }

    #[test]
    fn push_history_unlimited_keeps_everything() {
        let mut history = VecDeque::new();
        for i in 0..50i64 {
            let record = usn_record(
                i,
                FileId::from(1u64),
                Reason::RENAME_OLD_NAME,
                &i.to_string(),
            );
            push_history(&mut history, None, &record);
        }

        assert_eq!(history.len(), 50);
    }

    #[test]
    fn read_sized_rejects_a_buffer_smaller_than_the_minimum() {
        // The size check runs before any I/O, so this is safe to call on a fake journal with a
        // null handle: it never touches volume_handle.
        let mut journal = fake_journal(VecDeque::new());

        for size in [4, 8, Journal::MIN_READ_BUFFER_SIZE - 1] {
            let result = journal.read_sized(size);

            assert!(
                matches!(
                    result,
                    Err(NtfsReaderError::ReadBufferTooSmall { size: got, min })
                        if got == size && min == Journal::MIN_READ_BUFFER_SIZE
                ),
                "expected ReadBufferTooSmall for a {size}-byte buffer, got {:?}",
                result.map(|_| ())
            );
        }
    }

    // The boundary is the leading USN value: 8 bytes or fewer carry no record.
    #[test]
    fn caught_up_exactly_when_the_response_carries_no_record_bytes() {
        assert!(is_caught_up(0), "the driver returned nothing at all");
        assert!(is_caught_up(8), "exactly the leading USN and nothing else");
        assert!(!is_caught_up(9), "one byte of an actual record");
    }
}
