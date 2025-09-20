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
    WindowsError(#[from] WindowsErrorWrapper),
    #[error("missing required MFT attribute: {0}")]
    MissingMftAttribute(String),
    #[error("unknown")]
    Unknown,
}

#[derive(Debug)]
pub struct WindowsErrorWrapper(windows::core::Error);
impl WindowsErrorWrapper {
    pub fn from_thread() -> WindowsErrorWrapper {
        WindowsErrorWrapper(windows::core::Error::from_thread())
    }
}

impl std::fmt::Display for WindowsErrorWrapper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Windows error: {}", self.0)
    }
}
impl std::error::Error for WindowsErrorWrapper {}

pub type NtfsReaderResult<T> = core::result::Result<T, NtfsReaderError>;
