// Copyright (c) 2022, Matteo Bernacchia <dev@kikijiki.com>. All rights reserved.
// This project is dual licensed under the Apache License 2.0 and the MIT license.
// See the LICENSE files in the project root for details.

//! Low-level entry points for the synthetic benches (`benches/*_synthetic.rs`)
//! and the integration tests, which drive the byte-slice parsing. Only built
//! with the `internals` feature and hidden from the documentation: none of
//! this is part of the crate's API, and it can change or disappear in any
//! release.

use crate::{
    attribute::NtfsAttribute, errors::NtfsReaderResult, file::Record, usn::UsnRecord,
    volume::Volume,
};

pub use crate::data_run::DataRun;
#[cfg(windows)]
pub use crate::journal::path_lookups_on_this_thread;
pub use crate::mft::test_records;

/// The parsing entry point for `benches/journal_synthetic.rs`: times the parse alone on a
/// synthetic buffer. It is the same function `Journal::read_sized` runs on real data.
pub fn parse_usn_records_bench(
    buffer: &[u8],
    bytes_returned: u32,
) -> NtfsReaderResult<Vec<UsnRecord>> {
    let end = (bytes_returned as usize).min(buffer.len());
    crate::usn::parse_usn_records(&buffer[..end])
}

/// The attributes stored in `data`, one whole MFT record, or `None` if `data`
/// is not a valid record (see `Record::new`). `NtfsFile` needs an `Mft`, so this
/// is the record-only walk (`NtfsFile::record_attributes`) without one.
pub fn record_attributes(data: &[u8]) -> Option<impl Iterator<Item = NtfsAttribute<'_>>> {
    Some(Record::new(0, data)?.attributes())
}

/// Decodes a self-contained non-resident attribute's data runs and checks
/// them against its size (see `NtfsAttribute::nonresident_data_runs`).
pub fn nonresident_data_runs(
    attribute: &NtfsAttribute<'_>,
    volume: &Volume,
) -> NtfsReaderResult<(u64, Vec<DataRun>)> {
    attribute.nonresident_data_runs(volume)
}
