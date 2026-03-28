use thiserror::Error;

#[derive(Error, Debug)]
pub enum NtfsReaderError {
    #[error("elevation error")]
    ElevationError,
    #[error("io error")]
    IOError(#[from] std::io::Error),
    #[error("binread error")]
    BinReadError(#[from] binread::error::Error),
    #[error("windows error")]
    WindowsError(#[from] windows::core::Error),
    #[error("missing required MFT attribute: {0}")]
    MissingMftAttribute(String),
    #[error("corrupt MFT record {number}")]
    CorruptMftRecord { number: u64 },
    #[error("invalid MFT record at byte position {position}")]
    InvalidMftRecord { position: u64 },
    #[error("corrupt MFT record at byte position {position}")]
    CorruptMft { position: u64 },
    #[error("invalid NTFS data run: {details}")]
    InvalidDataRun { details: &'static str },
    #[error("allocation of {size} bytes exceeds platform address space")]
    AllocationTooLarge { size: u64 },
    #[error("unknown")]
    Unknown,
}

pub type NtfsReaderResult<T> = core::result::Result<T, NtfsReaderError>;
