// Copyright (c) 2022, Matteo Bernacchia <dev@kikijiki.com>. All rights reserved.
// This project is dual licensed under the Apache License 2.0 and the MIT license.
// See the LICENSE files in the project root for details.

//! Reads an NTFS volume's `$MFT` into memory and the USN change journal.
//! Windows only: [`Volume::new`] opens a volume path such as `\\.\C:`,
//! [`Mft::new`] loads and parses its whole `$MFT`, and [`Journal::new`] reads
//! the volume's USN journal. Every public item is at the crate root.
//!
//! The examples below are compile-checked but not run (`no_run`): they need
//! a real NTFS volume and elevated privileges, for the MFT and the journal
//! alike.
#![doc = include_str!("../README.md")]
#![warn(missing_docs)]
#![warn(missing_debug_implementations)]

#[cfg(not(any(windows, feature = "internals")))]
compile_error!("ntfs-reader only supports Windows targets");

// The `internals` feature builds the pure, byte-slice parsing modules (no
// `windows` crate, no real volume or journal I/O) on any target, so the
// unit and property tests and the synthetic criterion benches run on Linux
// (see docs/operating-notes.md's "Linux-side testing"). `journal` stays
// Windows-only regardless: it talks to `DeviceIoControl`. The USN record
// parser it runs lives in `usn`, which is portable, so the tests
// exercise the code production runs. `aligned_reader` builds under
// `internals` too (it has no `windows` dependency of its own) purely because
// `mft`/`volume` need it in scope; `aligned_reader::open_volume` itself stays
// useless off Windows.
#[cfg(any(windows, feature = "internals"))]
mod aligned_reader;
#[cfg(any(windows, feature = "internals"))]
mod api;
#[cfg(any(windows, feature = "internals"))]
mod attribute;
#[cfg(any(windows, feature = "internals"))]
mod data_run;
#[cfg(any(windows, feature = "internals"))]
mod errors;
#[cfg(any(windows, feature = "internals"))]
mod file;
#[cfg(any(windows, feature = "internals"))]
mod file_info;
#[cfg(windows)]
mod journal;
#[cfg(any(windows, feature = "internals"))]
mod mft;
#[cfg(any(windows, feature = "internals"))]
mod path;
#[cfg(any(windows, feature = "internals"))]
mod volume;

// Off Windows only the tests read USN records: `journal`, which parses them, is Windows-only.
#[cfg(any(windows, feature = "internals"))]
#[cfg_attr(not(windows), allow(dead_code))]
mod usn;

#[cfg(test)]
mod property;

// Crate-private entry points that the synthetic benches and integration tests need.
#[cfg(feature = "internals")]
#[doc(hidden)]
pub mod internals;

#[cfg(any(windows, feature = "internals"))]
pub use api::{
    FileId, NtfsAttributeType, NtfsFileName, NtfsFileNamespace, NtfsStandardInformation,
};
#[cfg(any(windows, feature = "internals"))]
pub use attribute::NtfsAttribute;
#[cfg(any(windows, feature = "internals"))]
pub use errors::{NtfsReaderError, NtfsReaderResult};
#[cfg(any(windows, feature = "internals"))]
pub use file::{NtfsDataStream, NtfsFile};
#[cfg(any(windows, feature = "internals"))]
pub use file_info::FileInfo;
#[cfg(windows)]
pub use journal::{HistorySize, Journal, JournalOptions, JournalPosition, NextUsn, UsnReadResult};
#[cfg(any(windows, feature = "internals"))]
pub use mft::Mft;
#[cfg(any(windows, feature = "internals"))]
pub use path::{CachedPath, DefaultPathCache, PathCache};
#[cfg(any(windows, feature = "internals"))]
pub use usn::{Reason, UsnRecord};
#[cfg(any(windows, feature = "internals"))]
pub use volume::Volume;
