// Copyright (c) 2022, Matteo Bernacchia <dev@kikijiki.com>. All rights reserved.
// This project is dual licensed under the Apache License 2.0 and the MIT license.
// See the LICENSE files in the project root for details.

//! [`FileInfo`]: a summary of one file.

use std::path::PathBuf;

use time::OffsetDateTime;

use crate::{
    api::{NtfsAttributeType, NtfsFileNamespace},
    file::NtfsFile,
    mft::Liveness,
    path::{DeletedPathCache, PathCache},
};

/// Summary of a file, built from [`NtfsFile`]'s attribute accessors.
///
/// Works on a deleted file (see [`Mft::deleted_files`](crate::Mft::deleted_files)) the same as
/// a live one: the name, size, times and attributes are what the file's records still hold, and
/// [`Self::is_deleted`] says which it is. The path is the one thing that differs, see
/// [`Self::path`].
///
/// Build one with [`FileInfo::new`] for a single lookup, or [`FileInfo::with_cache`] and a
/// [`DefaultPathCache`](crate::DefaultPathCache) when summarising many files: faster on a full
/// scan (see the numbers in the paths and caches guide).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct FileInfo {
    /// The file's name (best effort, see [`NtfsFile::best_name`]): a Win32 name when there is
    /// one, otherwise the first name; empty if the file has none. Lossy for an unpaired UTF-16
    /// surrogate; use [`Self::path`] when the exact name matters. A deleted file may have only a
    /// random name: one deleted while held open, or a directory removed with `remove_dir_all`,
    /// was renamed under `$Extend\$Deleted` first and keeps that name (see
    /// [`Mft::resolve_deleted_path`](crate::Mft::resolve_deleted_path)).
    pub name: String,
    /// The full path, starting with the volume path the [`Mft`](crate::Mft) was loaded from
    /// (`\\.\C:\dir\file`). `None` when unresolvable: the file has no name, or a directory on
    /// the way is missing, freed (a live file with a non-live parent chain), reused, or part of
    /// a loop. See [`Mft::resolve_path`](crate::Mft::resolve_path).
    ///
    /// For a deleted file this is the path of its [`Self::name`] with every directory on the way
    /// identified, freed ones included; `None` when one was not (`remove_dir_all` renames a
    /// directory before deleting it, losing the real path). A directory on the way shows the
    /// name its record holds: at deletion, or current if live (NTFS keeps no rename history).
    /// Computed without `cache`, which only holds live directories, so [`Self::new`] and
    /// [`Self::with_cache`] walk the whole parent chain per deleted file. Use
    /// [`Self::with_caches`] for a scan, and
    /// [`Mft::resolve_deleted_path`](crate::Mft::resolve_deleted_path) with a
    /// [`DeletedPathCache`] for the path up to where it stops.
    pub path: Option<PathBuf>,
    /// Whether the file is a directory.
    pub is_directory: bool,
    /// Whether the file is deleted: [`NtfsFile::is_deleted`], its base record freed (in-use flag
    /// and `$BITMAP` bit both clear). Every file [`Mft::deleted_files`](crate::Mft::deleted_files)
    /// yields is deleted here, but not vice versa: that listing also needs a record at or above
    /// 24 holding `$STANDARD_INFORMATION` or `$FILE_NAME`. A record whose flag and bit disagree
    /// is neither live nor deleted, and a delete-pending file (still open, renamed under
    /// `$Extend\$Deleted`) is still in use: both `false`.
    pub is_deleted: bool,
    /// Whether the default stream's data was lost
    /// ([`NtfsDataStream::data_lost`](crate::NtfsDataStream::data_lost) of the default stream):
    /// the file is deleted, had an `$ATTRIBUTE_LIST`, and the stream is non-resident or not
    /// found at all (its extension record freed and reused). Always `false` for a live file, a
    /// directory, or a deleted file whose default stream is resident and intact, even if an
    /// alternate stream was lost (see [`NtfsFile::stream_data_lost`]). When `true`,
    /// [`Self::size`] is not the file's real size, and the data cannot be found.
    pub data_lost: bool,
    /// Size of the default data stream in bytes. Alternate data streams are not included; see
    /// [`NtfsFile::data_streams`]. A deleted file's size is what its record still says: 0 for a
    /// file that was bigger when [`Self::data_lost`] is `true`.
    pub size: u64,
    /// Raw Windows `FILE_ATTRIBUTE_*` flags from `$STANDARD_INFORMATION`, or 0 if the file has
    /// none. These are the on-disk flags, which can differ from what Win32 reports for a file
    /// behind a filter such as WOF (Windows Overlay Filter): Win32 hides `SPARSE_FILE` and
    /// `REPARSE_POINT` there.
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

    /// Summarises `file`, remembering directory paths in `cache` so later files under the same
    /// directories resolve faster. Helps live files only: a deleted file's path is walked
    /// without `cache`, using a cache of its own dropped afterwards, so summarising many deleted
    /// files this way costs each one's depth. Use [`Self::with_caches`] instead.
    pub fn with_cache(file: &NtfsFile, cache: &mut impl PathCache) -> Self {
        Self::with_caches(file, cache, &mut DeletedPathCache::new())
    }

    /// Summarises `file` like [`Self::with_cache`], also remembering the directories a deleted
    /// file's path goes through, in `deleted_cache`. Give a scan of
    /// [`Mft::files`](crate::Mft::files) and [`Mft::deleted_files`](crate::Mft::deleted_files)
    /// one of each, and every directory is walked about once, whatever the depth.
    pub fn with_caches(
        file: &NtfsFile,
        cache: &mut impl PathCache,
        deleted_cache: &mut DeletedPathCache,
    ) -> Self {
        // One pass over the file's attributes (best_name's own walk of file.names() is folded
        // in), no allocation for the walk itself. resolve_path below still allocates a PathBuf.
        let mut standard_information = None;
        let mut size = 0;
        let mut default_stream = None;
        let mut best_name = None;
        let mut best_name_is_win32 = false;
        for attribute in file.attributes() {
            match attribute.attribute_type() {
                Some(NtfsAttributeType::StandardInformation) => {
                    standard_information = attribute.standard_information();
                }
                Some(NtfsAttributeType::Data) if attribute.header.name_length == 0 => {
                    if let Some(value_size) = attribute.value_size() {
                        (size, default_stream) = (value_size, Some(attribute.is_resident()));
                    }
                }
                // Same rule as NtfsFile::best_name: a Win32 name wins over an earlier Posix one,
                // and once found nothing after it can change the answer; otherwise keep the
                // first name seen.
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

        let base_number = file.base_number().unwrap_or(file.number());
        let base_record = file.mft().record(base_number);
        let is_directory = base_record
            .as_ref()
            .map_or(file.is_directory(), |base| base.is_directory());
        let deleted = file.is_deleted();

        let path = name.as_ref().and_then(|name| {
            if deleted {
                // The live cache must not learn what a freed directory leads to, nor be asked
                // about it.
                let resolved = file.mft().resolve_deleted_path(name, deleted_cache);
                resolved.complete.then_some(resolved.path)
            } else {
                file.mft().resolve_path(name, cache)
            }
        });
        // Only a live directory's path is cached. The cache must never hand out a deleted
        // directory's path for a reference to the freed record, nor the path of one whose in-use
        // flag and `$BITMAP` bit disagree, which a live walk refuses as a parent.
        if let (true, Some(path), Some(base)) = (is_directory, &path, &base_record) {
            if file.mft().liveness(&base.record) == Some(Liveness::Live) {
                cache.insert(base.reference(), path.clone());
            }
        }

        FileInfo {
            name: name.map(|name| name.to_string()).unwrap_or_default(),
            path,
            is_directory,
            is_deleted: deleted,
            // The default stream's, so a file that lost an alternate stream, or whose default
            // stream is resident, does not call its size wrong. A default stream not seen at all
            // (its extension record was freed and reused) is lost too: size 0 would read as an
            // empty file. A directory has no data to lose.
            data_lost: deleted
                && !is_directory
                && file.stream_data_lost()
                && default_stream != Some(true),
            size,
            file_attributes: standard_information.map_or(0, |info| info.file_attributes()),
            created: standard_information.map(|info| info.created()),
            accessed: standard_information.map(|info| info.accessed()),
            modified: standard_information.map(|info| info.modified()),
        }
    }
}

#[cfg(test)]
#[path = "tests/file_info.rs"]
mod tests;
