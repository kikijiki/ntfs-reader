// Copyright (c) 2022, Matteo Bernacchia <dev@kikijiki.com>. All rights reserved.
// This project is dual licensed under the Apache License 2.0 and the MIT license.
// See the LICENSE files in the project root for details.

//! Decoded data runs of a non-resident attribute.
//!
//! The module is private: `DataRun` is `pub` only so that the `internals`
//! module can hand it to benches and integration tests.

/// One run of a non-resident attribute's value: a stretch of the value that
/// is either stored at a place on the volume or is sparse. Both lengths and
/// offsets are in bytes (the on-disk cluster counts, multiplied by the
/// volume's cluster size).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataRun {
    /// Stored on the volume.
    Data {
        /// Byte offset of the run from the start of the volume (its first
        /// cluster number times the cluster size).
        offset: u64,
        /// Length of the run in bytes.
        length: u64,
    },
    /// Not stored: reads as zeroes.
    Sparse {
        /// Length of the run in bytes.
        length: u64,
    },
}
