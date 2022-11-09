use thiserror::Error;

#[derive(Error, Debug)]
pub enum NtfsReaderError {
    #[error("io error")]
    IOError(#[from] std::io::Error),
    #[error("binread error")]
    BinReadError(#[from] binread::error::Error),
    #[error("unknown")]
    Unknown,
}

pub type NtfsReaderResult<T> = core::result::Result<T, NtfsReaderError>;
