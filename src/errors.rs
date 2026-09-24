//! [`NtfsReaderError`]: the error type every fallible entry point returns.

use thiserror::Error;

/// Everything this crate can fail with.
///
/// Failures from Windows APIs arrive as [`NtfsReaderError::Io`] carrying the raw OS error code,
/// except access denied, which is always [`NtfsReaderError::AccessDenied`].
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum NtfsReaderError {
    /// Opening the volume or the journal was refused: the process lacks the privileges to read the
    /// raw volume (run elevated). Every entry point that opens a volume reports this the same way.
    /// Opening a directory path such as `C:\` instead of the volume (`\\.\C:`) is refused too,
    /// even when elevated.
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
    /// A size read from the volume does not fit in this platform's address space (a 32-bit
    /// build reading a huge `$MFT`, or a corrupt size).
    #[error("allocation of {size} bytes exceeds platform address space")]
    AllocationTooLarge {
        /// The requested size in bytes.
        size: u64,
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
mod tests {
    use super::*;

    // A variant that wraps another error must show that error's message, and one with fields
    // must show each of them; the fixed messages of the other variants are not worth a row.
    #[test]
    fn display_includes_the_wrapped_error_and_every_field() {
        let cases: Vec<(NtfsReaderError, &[&str])> = vec![
            (
                NtfsReaderError::Io(std::io::Error::other("disk exploded")),
                &["disk exploded"],
            ),
            (
                NtfsReaderError::MissingMftAttribute { attribute: "Data" },
                &["Data"],
            ),
            (
                NtfsReaderError::MftRecordFixupFailed { number: 42 },
                &["42"],
            ),
            (
                NtfsReaderError::InvalidMftRecord { position: 4096 },
                &["4096"],
            ),
            (
                NtfsReaderError::InvalidDataRun {
                    details: "bad header",
                },
                &["bad header"],
            ),
            (
                NtfsReaderError::AllocationTooLarge { size: 9001 },
                &["9001"],
            ),
            (
                NtfsReaderError::InvalidBootSector { field: "oem_id" },
                &["oem_id"],
            ),
            (
                NtfsReaderError::InvalidUsnRecord {
                    details: "too short",
                },
                &["too short"],
            ),
            (
                NtfsReaderError::ReadBufferTooSmall {
                    size: 3071,
                    min: 4096,
                },
                &["3071", "4096"],
            ),
        ];

        for (error, expected) in cases {
            let message = error.to_string();
            for part in expected {
                assert!(
                    message.contains(part),
                    "Display for {error:?} was {message:?}, expected it to contain {part:?}",
                );
            }
        }
    }

    #[test]
    fn a_permission_denied_io_error_becomes_access_denied() {
        let denied = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        assert!(matches!(
            NtfsReaderError::from(denied),
            NtfsReaderError::AccessDenied
        ));

        let other = std::io::Error::from(std::io::ErrorKind::NotFound);
        assert!(matches!(
            NtfsReaderError::from(other),
            NtfsReaderError::Io(_)
        ));
    }
}
