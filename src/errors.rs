// Copyright (c) 2022, Matteo Bernacchia <dev@kikijiki.com>. All rights reserved.
// This project is dual licensed under the Apache License 2.0 and the MIT license.
// See the LICENSE files in the project root for details.

//! [`NtfsReaderError`]: the error type every fallible entry point returns.

use std::ffi::OsString;

use thiserror::Error;

/// Everything this crate can fail with.
///
/// Failures from Windows APIs arrive as [`NtfsReaderError::Io`] carrying the raw OS error code,
/// except access denied, which is always [`NtfsReaderError::AccessDenied`].
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum NtfsReaderError {
    /// Opening the volume or the journal was refused: the process lacks the privileges to read
    /// the raw volume (run elevated). Also returned for a directory path such as `C:\` instead
    /// of the volume path (`\\.\C:`), even when elevated.
    #[error("access denied")]
    AccessDenied,
    /// An I/O or Windows API failure. A Windows failure carries its OS error code
    /// ([`std::io::Error::raw_os_error`]).
    #[error("io error: {0}")]
    Io(std::io::Error),
    /// The `$MFT` record has no attribute the loader requires.
    #[error("missing required MFT attribute: {attribute}")]
    MissingMftAttribute {
        /// The missing attribute.
        attribute: &'static str,
    },
    /// A record's update sequence check failed, so its bytes cannot be trusted.
    #[error("MFT record {number} failed fixup verification")]
    MftRecordFixupFailed {
        /// The record number.
        number: u64,
    },
    /// A record is not a valid `FILE` record.
    #[error("invalid MFT record at byte position {position}")]
    InvalidMftRecord {
        /// Byte offset of the record on the volume.
        position: u64,
    },
    /// A data run list is malformed.
    #[error("invalid NTFS data run: {details}")]
    InvalidDataRun {
        /// What is wrong with it.
        details: &'static str,
    },
    /// A size read from the volume cannot be held: it does not fit in this platform's address
    /// space (a 32-bit build reading a huge `$MFT`, or a corrupt size), it exceeds what the crate
    /// reads (a `$Bitmap` over 4 GiB, which no volume NTFS can format has), or the memory for it
    /// could not be reserved.
    #[error("allocation of {size} bytes is too large: over the platform's address space, the crate's limit, or the memory available")]
    AllocationTooLarge {
        /// The requested size in bytes.
        size: u64,
    },
    /// The volume's `$Bitmap` cannot be used as a [`ClusterBitmap`](crate::ClusterBitmap):
    /// missing, not in use, sparse or with missing parts (read as free clusters), shorter than
    /// the volume needs (only initialized bytes; the rest reads as free too), or the volume's
    /// size is unknown (no cluster count to work out). Also returned by
    /// [`StreamReader::allocation`](crate::StreamReader::allocation) for a bitmap read from
    /// another volume or with a different cluster size than the stream.
    #[error("invalid cluster bitmap: {details}")]
    InvalidClusterBitmap {
        /// What is wrong with it.
        details: &'static str,
    },
    /// A boot sector field is out of range.
    #[error("invalid boot sector field: {field}")]
    InvalidBootSector {
        /// The offending field.
        field: &'static str,
    },
    /// The volume path holds a NUL character, which would silently truncate it in a Windows call.
    #[error("volume path contains an embedded NUL character")]
    InvalidVolumePath,
    /// A record in a `FSCTL_READ_USN_JOURNAL` buffer is malformed. The whole read is rejected.
    #[error("invalid USN journal record: {details}")]
    InvalidUsnRecord {
        /// What is wrong with it.
        details: &'static str,
    },
    /// The volume has no active USN journal.
    #[error("USN journal is not active on this volume")]
    JournalNotActive,
    /// The requested USN is older than the journal's first entry; the journal wrapped around it.
    #[error("the requested USN journal entry has been deleted")]
    JournalEntryDeleted,
    /// The journal is being deleted.
    #[error("the USN journal is being deleted")]
    JournalDeleteInProgress,
    /// A saved position belongs to an older journal (the journal was deleted and recreated).
    #[error("saved journal position's journal id does not match the volume's current journal")]
    JournalIdMismatch,
    /// The read buffer cannot hold one maximum-size record.
    #[error("read buffer of {size} bytes is smaller than the minimum of {min} bytes")]
    ReadBufferTooSmall {
        /// The size asked for.
        size: usize,
        /// The smallest size accepted.
        min: usize,
    },
    /// The file has no data stream of that name. A directory has no default stream either.
    #[error("no such data stream: {}", stream_name(.name))]
    StreamNotFound {
        /// The stream asked for: `None` is the default stream.
        name: Option<OsString>,
    },
    /// The stream is stored compressed (NTFS LZNT1). This crate does not decompress.
    #[error("the data stream is compressed")]
    CompressedStream,
    /// The file is compressed by Windows Overlay Filter (WOF, CompactOS): its default stream is a
    /// sparse placeholder reading as zeroes; the real contents live compressed in the named
    /// stream `WofCompressedData`. This crate does not decompress them.
    #[error("the file is WOF compressed: its default stream holds no data")]
    WofCompressedStream,
    /// The stream is encrypted (EFS). Its bytes on the volume are ciphertext.
    #[error("the data stream is encrypted")]
    EncryptedStream,
    /// A read reached a stream part whose extent record cannot be found: a deleted file's
    /// extension record was freed and reused. Distinct from
    /// [`NtfsFile::stream_data_lost`](crate::NtfsFile::stream_data_lost), which is about runs
    /// NTFS wiped outright.
    ///
    /// Travels only inside the [`io::Error`](std::io::Error) that
    /// [`StreamReader`](crate::StreamReader)'s `Read` returns, never as a crate function's own
    /// result: extract it with [`io::Error::get_ref`](std::io::Error::get_ref) and a downcast.
    /// See [`StreamReader`](crate::StreamReader) for an example.
    #[error(
        "stream data at offset {offset} is missing: the record holding its extent was not found"
    )]
    StreamExtentMissing {
        /// Offset in the stream of the first byte that cannot be read.
        offset: u64,
    },
}

/// How a stream is named in a message: the default stream has no name.
fn stream_name(name: &Option<OsString>) -> String {
    match name {
        Some(name) => format!("{name:?}"),
        None => "the default stream".to_string(),
    }
}

/// Every `?` on an [`std::io::Error`] goes through here, so a refused volume open is
/// [`NtfsReaderError::AccessDenied`] no matter which entry point hit it.
impl From<std::io::Error> for NtfsReaderError {
    fn from(err: std::io::Error) -> Self {
        if err.kind() == std::io::ErrorKind::PermissionDenied {
            NtfsReaderError::AccessDenied
        } else {
            NtfsReaderError::Io(err)
        }
    }
}

/// `Result` with [`NtfsReaderError`].
pub type NtfsReaderResult<T> = core::result::Result<T, NtfsReaderError>;

#[cfg(test)]
#[path = "tests/errors.rs"]
mod tests;
