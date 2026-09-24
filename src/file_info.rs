// Copyright (c) 2022, Matteo Bernacchia <dev@kikijiki.com>. All rights reserved.
// This project is dual licensed under the Apache License 2.0 and the MIT license.
// See the LICENSE files in the project root for details.

//! [`FileInfo`]: a summary of one file.

use std::path::PathBuf;

use time::OffsetDateTime;

use crate::{
    api::{NtfsAttributeType, NtfsFileNamespace},
    file::NtfsFile,
    path::PathCache,
};

/// Summary of a file, built from [`NtfsFile`]'s attribute accessors.
///
/// Build one with [`FileInfo::new`] for a single lookup, or
/// [`FileInfo::with_cache`] and a [`DefaultPathCache`](crate::DefaultPathCache)
/// when summarising many files, which is about twice as fast on a full scan.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct FileInfo {
    /// The file's name (best effort, see [`NtfsFile::best_name`]): a Win32
    /// name when there is one, otherwise the first name. Empty if the file
    /// has no name. A name with an unpaired UTF-16 surrogate is lossy here;
    /// use [`Self::path`] when the exact name matters.
    pub name: String,
    /// The full path, starting with the volume path the [`Mft`](crate::Mft) was loaded
    /// from (`\\.\C:\dir\file`). `None` when it cannot be resolved: the file
    /// has no name, or a directory on the way is missing, freed, reused by
    /// another file, or part of a loop. See [`Mft::resolve_path`](crate::Mft::resolve_path).
    pub path: Option<PathBuf>,
    /// Whether the file is a directory.
    pub is_directory: bool,
    /// Size of the default data stream in bytes. Alternate data streams are
    /// not included; see [`NtfsFile::data_streams`].
    pub size: u64,
    /// Raw Windows `FILE_ATTRIBUTE_*` flags from `$STANDARD_INFORMATION`, or 0
    /// if the file has none. These are the flags on disk, which can differ from what Win32
    /// reports for a file behind a filter such as WOF (Windows Overlay Filter): Win32 hides
    /// `SPARSE_FILE` and `REPARSE_POINT` there.
    pub file_attributes: u32,
    /// Creation time, or `None` if the file has no `$STANDARD_INFORMATION`.
    pub created: Option<OffsetDateTime>,
    /// Last access time, or `None` if the file has no `$STANDARD_INFORMATION`.
    pub accessed: Option<OffsetDateTime>,
    /// Last data modification time, or `None` if the file has no
    /// `$STANDARD_INFORMATION`.
    pub modified: Option<OffsetDateTime>,
}

impl FileInfo {
    /// Summarises `file` without a path cache: fine for a single lookup.
    pub fn new(file: &NtfsFile) -> Self {
        Self::with_cache(file, &mut ())
    }

    /// Summarises `file`, remembering directory paths in `cache` so that
    /// later files under the same directories resolve faster.
    pub fn with_cache(file: &NtfsFile, cache: &mut impl PathCache) -> Self {
        // One pass over the file's attributes (best_name's own walk of
        // file.names() is folded in), no allocation for the walk itself.
        // resolve_path below still allocates a PathBuf.
        let mut standard_information = None;
        let mut size = 0;
        let mut best_name = None;
        let mut best_name_is_win32 = false;
        for attribute in file.attributes() {
            match attribute.attribute_type() {
                Some(NtfsAttributeType::StandardInformation) => {
                    standard_information = attribute.standard_information();
                }
                Some(NtfsAttributeType::Data) if attribute.header.name_length == 0 => {
                    size = attribute.value_size().unwrap_or(size);
                }
                // Same rule as NtfsFile::best_name: a Win32 name wins over
                // an earlier Posix one and, once found, nothing after it can
                // change the answer; otherwise keep the first name seen.
                Some(NtfsAttributeType::FileName) if !best_name_is_win32 => {
                    if let Some(candidate) = attribute.file_name() {
                        let is_win32 = matches!(
                            candidate.namespace(),
                            Some(NtfsFileNamespace::Win32 | NtfsFileNamespace::Win32AndDos)
                        );
                        if is_win32 || best_name.is_none() {
                            best_name = Some(candidate);
                            best_name_is_win32 = is_win32;
                        }
                    }
                }
                _ => {}
            }
        }

        let name = best_name;
        if name.is_none() {
            tracing::debug!("No name for file {}", file.number());
        }
        let path = name
            .as_ref()
            .and_then(|name| file.mft().resolve_path(name, cache));

        let base_number = file.base_number().unwrap_or(file.number());
        let base_record = file.mft().record(base_number);
        let is_directory = base_record
            .as_ref()
            .map_or(file.is_directory(), |base| base.is_directory());
        if let (true, Some(path), Some(base)) = (is_directory, &path, &base_record) {
            cache.insert(base.reference(), path.clone());
        }

        FileInfo {
            name: name.map(|name| name.to_string()).unwrap_or_default(),
            path,
            is_directory,
            size,
            file_attributes: standard_information.map_or(0, |info| info.file_attributes()),
            created: standard_information.map(|info| info.created()),
            accessed: standard_information.map(|info| info.accessed()),
            modified: standard_information.map(|info| info.modified()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{EPOCH_DIFFERENCE, FIRST_NORMAL_RECORD};
    use crate::mft::test_records::*;

    // Card 016. FileInfo::new must aggregate across the whole logical file
    // even when handed an extension record directly, not just the base.
    #[test]
    fn file_info_on_an_extension_record_matches_the_base() {
        let base_number = FIRST_NORMAL_RECORD;
        let extension_number = FIRST_NORMAL_RECORD + 1;

        let mut base = new_record(base_number, 1, 0);
        let mut offset = add_file_name(&mut base, ATTRIBUTES_OFFSET, "split.bin", 0);
        offset = add_standard_information(&mut base, offset, 0x20);
        finish_record(&mut base, offset);

        let mut extension = new_record(extension_number, 1, reference(1, base_number));
        let offset = add_nonresident_data(&mut extension, ATTRIBUTES_OFFSET, 4096);
        finish_record(&mut extension, offset);

        let mft = mft_with(vec![base, extension]);
        let base_file = mft.record(base_number).expect("base record");
        let extension_file = mft.record(extension_number).expect("extension record");
        assert!(extension_file.is_extension());

        let from_base = FileInfo::new(&base_file);
        let from_extension = FileInfo::new(&extension_file);

        assert_eq!(from_extension.name, from_base.name);
        assert_eq!(from_extension.size, from_base.size);
        assert_eq!(from_extension.size, 4096);
        assert_eq!(from_extension.file_attributes, from_base.file_attributes);
    }

    // The directory flag lives in the base record only: an extension record never carries it, so
    // FileInfo on the extension record of a directory must report the base's flag.
    #[test]
    fn file_info_on_an_extension_record_of_a_directory_says_directory() {
        let base_number = FIRST_NORMAL_RECORD;
        let extension_number = FIRST_NORMAL_RECORD + 1;

        let mut base = new_record(base_number, 1, 0);
        set_record_flags(&mut base, directory_flags());
        let offset = add_standard_information(&mut base, ATTRIBUTES_OFFSET, 0x10);
        finish_record(&mut base, offset);

        // The directory's only name spilled into an extension record.
        let mut extension = new_record(extension_number, 1, reference(1, base_number));
        let offset = add_file_name(&mut extension, ATTRIBUTES_OFFSET, "spilled-dir", 0x10);
        finish_record(&mut extension, offset);

        let mft = mft_with(vec![base, extension]);
        let base_file = mft.record(base_number).expect("base record");
        let extension_file = mft.record(extension_number).expect("extension record");
        assert!(extension_file.is_extension());
        assert!(
            !extension_file.is_directory(),
            "the extension record carries no directory flag"
        );

        assert!(FileInfo::new(&base_file).is_directory);
        let from_extension = FileInfo::new(&extension_file);
        assert_eq!(from_extension.name, "spilled-dir");
        assert!(from_extension.is_directory);
    }

    /// FILETIME ticks (100 ns) `secs` seconds and `ticks` ticks after the Unix epoch.
    fn filetime(secs: i64, ticks: u64) -> u64 {
        ((EPOCH_DIFFERENCE as i64 + secs * 10_000_000) as u64) + ticks
    }

    // T2: nothing used to pin which stored time is which. Four distinct times
    // per file, each with a sub-second part, expected as Unix nanoseconds
    // worked out by hand, through FileInfo and through the accessors of the
    // standard information. One file has times before 1970.
    #[test]
    fn each_standard_information_time_reaches_its_own_field() {
        let modern = [
            filetime(1_000_000_000, 1_234_567),
            filetime(1_100_000_000, 2_345_678),
            filetime(1_300_000_000, 4_567_890),
            filetime(1_200_000_000, 3_456_789),
        ];
        // created, modified, MFT modified, accessed
        let modern_nanos: [i128; 4] = [
            1_000_000_000_123_456_700,
            1_100_000_000_234_567_800,
            1_300_000_000_456_789_000,
            1_200_000_000_345_678_900,
        ];
        // 1960-01-01T00:00:00Z is 315_619_200 s before the epoch; each time a
        // second and a tick earlier than the one before.
        let old = [
            filetime(-315_619_200, 0),
            filetime(-315_619_201, 5),
            filetime(-315_619_203, 7),
            filetime(-315_619_202, 9),
        ];
        let old_nanos: [i128; 4] = [
            -315_619_200_000_000_000,
            -315_619_200_999_999_500,
            -315_619_202_999_999_300,
            -315_619_201_999_999_100,
        ];

        let mut records = Vec::new();
        for (index, times) in [modern, old].into_iter().enumerate() {
            let mut record = new_record(FIRST_NORMAL_RECORD + index as u64, 1, 0);
            let mut offset = add_file_name(&mut record, ATTRIBUTES_OFFSET, "t.txt", 0);
            offset = add_standard_information_at(&mut record, offset, 0x20, times);
            finish_record(&mut record, offset);
            records.push(record);
        }
        let mut without = new_record(FIRST_NORMAL_RECORD + 2, 1, 0);
        let offset = add_file_name(&mut without, ATTRIBUTES_OFFSET, "none.txt", 0);
        finish_record(&mut without, offset);
        records.push(without);
        let mft = mft_with(records);

        for (index, expected) in [modern_nanos, old_nanos].into_iter().enumerate() {
            let file = mft
                .record(FIRST_NORMAL_RECORD + index as u64)
                .expect("record");
            let info = FileInfo::new(&file);
            let nanos =
                |time: Option<OffsetDateTime>| time.map(OffsetDateTime::unix_timestamp_nanos);
            assert_eq!(nanos(info.created), Some(expected[0]), "created");
            assert_eq!(nanos(info.modified), Some(expected[1]), "modified");
            assert_eq!(nanos(info.accessed), Some(expected[3]), "accessed");
            assert_eq!(info.file_attributes, 0x20);

            let standard = file.standard_information().expect("standard information");
            assert_eq!(standard.created().unix_timestamp_nanos(), expected[0]);
            assert_eq!(standard.modified().unix_timestamp_nanos(), expected[1]);
            assert_eq!(standard.mft_modified().unix_timestamp_nanos(), expected[2]);
            assert_eq!(standard.accessed().unix_timestamp_nanos(), expected[3]);
            assert_eq!(standard.file_attributes(), 0x20);
        }

        let file = mft.record(FIRST_NORMAL_RECORD + 2).expect("record");
        let info = FileInfo::new(&file);
        assert_eq!(
            (info.created, info.modified, info.accessed),
            (None, None, None)
        );
        assert_eq!(info.file_attributes, 0);
        assert!(file.standard_information().is_none());
    }
}
