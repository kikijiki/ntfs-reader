//! Public logical-file observations, shared by synthetic and real-volume parity tests.

use std::ffi::OsString;
use std::path::PathBuf;

use super::reader::{
    DefaultPathCache, DeletedPath, DeletedPathCache, FileId, FileInfo, Mft, MftChunk,
    NtfsDataStream, NtfsFile, NtfsFileName, NtfsFileNamespace, NtfsStandardInformation,
};

pub trait PathSource {
    fn live_path(&self, name: &NtfsFileName, cache: &mut DefaultPathCache) -> Option<PathBuf>;
    fn deleted_path(&self, name: &NtfsFileName, cache: &mut DeletedPathCache) -> DeletedPath;
}

macro_rules! path_source {
    ($source:ty) => {
        impl PathSource for $source {
            fn live_path(
                &self,
                name: &NtfsFileName,
                cache: &mut DefaultPathCache,
            ) -> Option<PathBuf> {
                self.resolve_path(name, cache)
            }
            fn deleted_path(
                &self,
                name: &NtfsFileName,
                cache: &mut DeletedPathCache,
            ) -> DeletedPath {
                self.resolve_deleted_path(name, cache)
            }
        }
    };
}
path_source!(Mft);
path_source!(MftChunk<'_>);

#[derive(Debug, PartialEq, Eq)]
pub struct NameSnapshot {
    name: OsString,
    namespace: Option<NtfsFileNamespace>,
    parent_reference: u64,
    parent_id: FileId,
    parent_number: u64,
    in_root: bool,
    dos_alias: bool,
    readonly: bool,
    hidden: bool,
    system: bool,
    reparse_point: bool,
}

impl From<NtfsFileName> for NameSnapshot {
    fn from(name: NtfsFileName) -> Self {
        Self {
            name: name.to_os_string(),
            namespace: name.namespace(),
            parent_reference: name.parent_reference(),
            parent_id: name.parent_id(),
            parent_number: name.parent_number(),
            in_root: name.is_in_root(),
            dos_alias: name.is_dos_alias(),
            readonly: name.is_readonly(),
            hidden: name.is_hidden(),
            system: name.is_system(),
            reparse_point: name.is_reparse_point(),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct FileSnapshot {
    pub info: FileInfo,
    reference: u64,
    file_id: FileId,
    base_reference: Option<u64>,
    base_number: Option<u64>,
    used: bool,
    deleted: bool,
    directory: bool,
    extension: bool,
    records: Vec<u64>,
    names: Vec<NameSnapshot>,
    hard_links: Vec<NameSnapshot>,
    best_name: Option<NameSnapshot>,
    // Keep the typed stream and path values: FileInfo alone loses ADS and marker differences.
    pub streams: Vec<NtfsDataStream>,
    pub paths: Vec<(Option<PathBuf>, DeletedPath)>,
    stream_data_lost: bool,
    standard_information: Option<NtfsStandardInformation>,
    resident_data: Option<Vec<u8>>,
}

impl FileSnapshot {
    pub fn new(
        file: &NtfsFile<'_>,
        source: &impl PathSource,
        cache: &mut DefaultPathCache,
        deleted_cache: &mut DeletedPathCache,
    ) -> Self {
        let paths = file
            .names()
            .map(|name| {
                (
                    source.live_path(&name, cache),
                    source.deleted_path(&name, deleted_cache),
                )
            })
            .collect();
        Self {
            info: FileInfo::with_caches(file, cache, deleted_cache),
            reference: file.reference(),
            file_id: file.file_id(),
            base_reference: file.base_reference(),
            base_number: file.base_number(),
            used: file.is_used(),
            deleted: file.is_deleted(),
            directory: file.is_directory(),
            extension: file.is_extension(),
            records: file.records().map(|record| record.reference()).collect(),
            names: file.names().map(NameSnapshot::from).collect(),
            hard_links: file.hard_links().map(NameSnapshot::from).collect(),
            best_name: file.best_name().map(NameSnapshot::from),
            streams: file.data_streams().collect(),
            paths,
            stream_data_lost: file.stream_data_lost(),
            standard_information: file.standard_information(),
            resident_data: file.resident_data().map(<[u8]>::to_vec),
        }
    }
}

/// Quiet comparisons are exact for live and deleted files. On a live volume, at most 5% of
/// live files may differ; deleted differences are reported only. Freed records can be reused
/// throughout a slow scan, so deleted correctness needs the quiet-volume or shadow comparison.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ComparisonPolicy {
    Quiet,
    Live,
}

impl ComparisonPolicy {
    /// Each population is (differences, expected files), live first and deleted second.
    pub fn accepts_populations(self, [live, deleted]: [(usize, usize); 2]) -> bool {
        match self {
            Self::Quiet => live.0 == 0 && deleted.0 == 0,
            Self::Live => live.0 <= live.1 / 20,
        }
    }
}
