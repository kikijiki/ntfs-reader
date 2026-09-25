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

/// The Win32 error code inside a `windows` error: the low 16 bits of an
/// `HRESULT_FROM_WIN32` value, or the whole `HRESULT` otherwise.
fn win32_code(err: &windows::core::Error) -> u32 {
    let hresult = err.code().0 as u32;
    if hresult & 0xFFFF_0000 == 0x8007_0000 {
        hresult & 0xFFFF
    } else {
        hresult
    }
}

/// Maps a failed Win32 call to this crate's error: journal-specific cases
/// get their own variant, access denied becomes `AccessDenied`, anything
/// else is an `Io` error carrying the OS error code.
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

/// Decodes a `FILE_NAME_INFO` buffer (as `GetFileInformationByHandleEx(...,
/// FileNameInfo, ...)` fills it) into a `PathBuf`, or `None` if malformed.
/// Bounds-checks the declared `FileNameLength` before slicing the name out.
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

/// How many times the calling thread has looked a path up through a file
/// handle (the two `OpenFileById` attempts in [`Journal::resolve_path`]). A
/// test reads it around a `read` to check that reading opens no handles.
#[cfg(feature = "internals")]
pub fn path_lookups_on_this_thread() -> u64 {
    PATH_LOOKUPS.with(|count| count.get())
}

/// The volume-relative path of the file `file_id` names (e.g. `\dir\file.txt`),
/// or `None` if it cannot be opened: it no longer exists, or the id is not one
/// this volume understands.
fn get_file_path(volume_handle: Foundation::HANDLE, file_id: FileId) -> Option<PathBuf> {
    #[cfg(feature = "internals")]
    PATH_LOOKUPS.with(|count| count.set(count.get() + 1));

    // NTFS ids fit in 64 bits (the file reference); wider ones open as extended ids.
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

    // SAFETY: `file_id_desc` is fully initialized and outlives the call;
    // `volume_handle` is the calling `Journal`'s open handle. The returned
    // handle is closed below on every path that reaches it.
    unsafe {
        let file_handle = FileSystem::OpenFileById(
            volume_handle,
            &file_id_desc,
            0,
            FileSystem::FILE_SHARE_READ
                | FileSystem::FILE_SHARE_WRITE
                | FileSystem::FILE_SHARE_DELETE,
            None,
            // The reparse point itself, not its target: a journal record
            // names the link, and following it would resolve the target's
            // path instead.
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
        )
        .unwrap_or(Foundation::INVALID_HANDLE_VALUE);

        if file_handle.is_invalid() {
            return None;
        }

        // A `Vec<u32>` so the buffer is aligned for `FILE_NAME_INFO`, whose
        // first field is a `u32`. Viewed as bytes for parsing.
        let mut info_words = (size_of::<FileSystem::FILE_NAME_INFO>()
            + (Foundation::MAX_PATH as usize) * size_of::<u16>())
        .div_ceil(size_of::<u32>());
        let mut info_buffer = vec![0u32; info_words];

        // GetFileInformationByHandleEx reports the exact size needed on
        // ERROR_MORE_DATA, so one retry should suffice; the cap defends
        // against a driver that keeps reporting a size that still does not fit.
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
                        // The buffer was too small: the driver stored the needed length.
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

/// The full path of a record's file: from its parent directory's path when
/// the parent still exists, otherwise from the file's own id, otherwise
/// `None`.
fn get_usn_record_path(
    volume_path: &Path,
    volume_handle: Foundation::HANDLE,
    file_name: &OsStr,
    file_id: FileId,
    parent_id: FileId,
) -> Option<PathBuf> {
    // Prefer the parent's path: computing it from the file id instead could
    // return a stale path if the file was moved.
    if let Some(parent_path) = get_file_path(volume_handle, parent_id) {
        return Some(volume_path.join(parent_path.join(file_name)));
    }

    // The parent may be deleted; fall back to the file's own id.
    if let Some(path) = get_file_path(volume_handle, file_id) {
        return Some(volume_path.join(path));
    }

    tracing::debug!("Could not get path: {}", file_name.display());
    None
}

/// Nothing past the leading USN value (`bytes_returned <= size_of::<i64>()`)
/// means the driver scanned to the journal's current end and found nothing
/// more: the caller is caught up. A non-empty response can still hold zero
/// *matching* records: `reason_mask` filtered them out after the driver had
/// already scanned past them.
fn is_caught_up(bytes_returned: u32) -> bool {
    bytes_returned as usize <= size_of::<i64>()
}

/// Adds `record` to the rename history if it is a `RENAME_OLD_NAME` record,
/// the only kind [`Journal::match_rename`] looks at. `max_history_size`
/// mirrors `Journal.max_history_size`: `None` unlimited, `Some(0)` keeps
/// nothing, `Some(n)` keeps the `n` most recent.
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

/// A journal position: the journal id it was read from plus a USN within it.
/// Validates that a saved `NextUsn::Custom` still refers to the same
/// journal generation (deleting and recreating a journal resets its id and
/// USN numbering).
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
    /// A saved position. [`Journal::new`] fails with `JournalIdMismatch` if
    /// the journal was recreated since.
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
    /// Which reasons to read; records matching none are skipped by the
    /// driver. Include [`Reason::RENAME_OLD_NAME`] to use
    /// [`Journal::match_rename`]. Defaults to [`Reason::ALL`].
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
    /// `true` when this read reached the journal's current end: nothing more
    /// to read right now, even though `records` can be empty on a call that
    /// still had more to scan (`reason_mask` filtered it all out). Use this,
    /// not "empty result", to decide when to stop.
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
    /// The smallest buffer [`Journal::read_sized`] accepts: room for the
    /// leading USN (8 bytes) plus the largest record a file name can
    /// produce (about 600 bytes), rounded up. A smaller buffer could stall
    /// the read on a record the driver then cannot return.
    pub const MIN_READ_BUFFER_SIZE: usize = 1024;

    /// Opens the journal of `volume`.
    ///
    /// Fails with `AccessDenied` without privileges to open the raw volume,
    /// `JournalNotActive` if the volume has no journal, and
    /// `JournalIdMismatch` if `options.next_usn` is from an older journal.
    pub fn new(volume: Volume, options: JournalOptions) -> NtfsReaderResult<Journal> {
        // Wide, not ANSI: encode_wide() round-trips any OsString Windows can
        // produce, ill-formed UTF-16 included. `Volume::new` already
        // rejected an embedded NUL, so this is the whole path.
        let mut wide_path: Vec<u16> = volume.path().as_os_str().encode_wide().collect();
        wide_path.push(0);

        // Owned from the moment CreateFileW succeeds, so every `?` below
        // closes it instead of leaking it.
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
                    // Timeout = BytesToWaitFor = 0 on every read below means
                    // the FSCTL never waits, so open for synchronous I/O.
                    FileSystem::FILE_FLAGS_AND_ATTRIBUTES(0),
                    None,
                )
                .map_err(map_windows_error)?,
            )
        };

        let mut journal = Ioctl::USN_JOURNAL_DATA_V2::default();

        // SAFETY: `volume_handle` is open, and the output buffer is a
        // `USN_JOURNAL_DATA_V2` of exactly the size passed.
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

    /// Reads one page of records (a 4096-byte buffer's worth) from the
    /// current position and moves past them. Call it in a loop until
    /// `caught_up`.
    pub fn read(&mut self) -> NtfsReaderResult<UsnReadResult> {
        self.read_sized(4096)
    }

    /// Like [`Journal::read`] with a buffer of `buffer_size` bytes: bigger
    /// returns more records per call. Under
    /// [`Journal::MIN_READ_BUFFER_SIZE`] returns `ReadBufferTooSmall`
    /// without reading.
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

        // SAFETY: `read` is a `READ_USN_JOURNAL_DATA_V1` of the size passed,
        // `buffer` is `buffer_size` bytes long, and the handle is open.
        unsafe {
            // Opened for synchronous I/O (see `new`), so this blocks until
            // the FSCTL completes and never returns ERROR_IO_PENDING.
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

        // DeviceIoControl should never report more bytes than the buffer it
        // was given, but the parser trusts the slice length, so clamp
        // defensively.
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

    /// Resolves a record's full path, or `None` if it cannot be resolved
    /// (e.g. the file and its parent directory are both gone). A `Some`
    /// path is absolute, starting from the volume path the `Volume` was
    /// opened with, never relative to the current directory.
    ///
    /// Separate from `read`/`read_sized` since it costs one or two
    /// `OpenFileById` handle opens: a caller only occasionally needing a
    /// path is not charged for it on every record.
    ///
    /// The path is the record's parent directory as it is *now* (looked up
    /// by the parent's id) joined with the name the record carries, so a
    /// moved parent gives its new location. For a `RENAME_OLD_NAME` record
    /// that name is the old one, no longer existing under that directory:
    /// the result is where the file used to be, not where it is. If the
    /// parent is gone, the file's own id is looked up instead, giving its
    /// current path and name if it still exists.
    pub fn resolve_path(&self, record: &UsnRecord) -> Option<PathBuf> {
        get_usn_record_path(
            self.volume.path(),
            *self.volume_handle,
            &record.name,
            record.file_id,
            record.parent_id,
        )
    }

    /// The old name of the rename that produced `record`'s `RENAME_NEW_NAME`,
    /// if one is in history. `None` if `record` is not itself a
    /// `RENAME_NEW_NAME` record, or no matching `RENAME_OLD_NAME` entry is
    /// found (e.g. it aged out of a bounded history).
    ///
    /// The history holds only `RENAME_OLD_NAME` records, so the journal must
    /// be opened with [`Reason::RENAME_OLD_NAME`] in `reason_mask` (and
    /// `RENAME_NEW_NAME` for the records passed in), or nothing is found.
    pub fn match_rename(&self, record: &UsnRecord) -> Option<OsString> {
        if !record.reason.contains(Reason::RENAME_NEW_NAME) {
            return None;
        }

        // Search backward from the most recent entry: the first match by
        // file_id alone need not be the immediately preceding rename for a
        // file renamed more than once.
        self.history
            .iter()
            .rev()
            .find(|r| r.file_id == record.file_id && r.usn < record.usn)
            .map(|r| r.name.clone())
    }

    /// Forgets rename history. `Some(usn)` keeps entries at `usn` or later
    /// and drops the rest; `None` clears everything.
    pub fn trim_history(&mut self, min_usn: Option<i64>) {
        match min_usn {
            Some(usn) => self.history.retain(|r| r.usn >= usn),
            None => self.history.clear(),
        }
    }

    /// The id of the journal this `Journal` was opened against. Deleting and
    /// recreating it changes this; a saved `JournalPosition` from a
    /// previous id is stale.
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

    /// `(journal_id(), next_usn())`, saveable and passed back later as
    /// `NextUsn::Custom` to resume (rejected with `JournalIdMismatch` if
    /// the journal was recreated meanwhile).
    pub fn position(&self) -> JournalPosition {
        JournalPosition {
            journal_id: self.journal_id(),
            usn: self.next_usn(),
        }
    }
}

// SAFETY: the only field not `Send` on its own is `volume_handle`'s raw
// `HANDLE`. A Windows handle can be used from a different thread than the
// one that created it, as long as it is never used from two threads at
// once. `Journal` is `Send` but not `Sync` (the raw handle keeps it from
// being `Sync`), so a `&Journal` cannot be shared across threads either: at
// most one thread uses a `Journal` at a time, through `&mut self` (`read`,
// `read_sized`, `trim_history`) or `&self` (`resolve_path`, which opens
// files by id through the handle, and the plain accessors). Moving a
// `Journal` to another thread and using it only there is sound.
unsafe impl Send for Journal {}

// No manual Drop impl: `volume_handle` is `windows::core::Owned<HANDLE>`, which closes itself.

#[cfg(test)]
#[path = "tests/journal.rs"]
mod tests;
