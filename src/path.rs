// Copyright (c) 2022, Matteo Bernacchia <dev@kikijiki.com>. All rights reserved.
// This project is dual licensed under the Apache License 2.0 and the MIT license.
// See the LICENSE files in the project root for details.

//! Full paths: [`Mft::resolve_path`] and the [`PathCache`] it can use.

use std::{
    collections::HashMap,
    ffi::{OsStr, OsString},
    fmt,
    path::{Path, PathBuf, MAIN_SEPARATOR_STR},
};

use crate::{
    api::{NtfsFileName, ROOT_RECORD},
    mft::{Mft, RECORD_NUMBER_MASK},
};

/// The longest path [`Mft::resolve_path`] returns, in UTF-16 units: the Win32
/// maximum. The limit is on path length, not depth, so a cache hit enforces
/// it the same way a full walk does.
const MAX_PATH_UNITS: usize = 32767;

/// The record of `$Extend`, a system record with a fixed number.
const EXTEND_RECORD: u64 = 11;

/// The name of `$Extend\$Deleted`, where NTFS moves a directory
/// `remove_dir_all` is about to delete, or a file deleted while open, under a
/// random name. Its record number is 29 on the volume this was measured on,
/// but not fixed, so it is recognized by name under [`EXTEND_RECORD`] (ASCII
/// case insensitive, as NTFS names are): no user can create one there.
const DELETED_DIRECTORY: &str = "$Deleted";

/// Counts the records a deleted walk visits, so tests can assert its work
/// without timing it. [`walk_budget::run`] sets a budget; going over it
/// panics, stopping a walk that would take minutes.
#[cfg(test)]
pub(crate) mod walk_budget {
    use std::cell::Cell;

    thread_local! {
        static STEPS: Cell<u64> = const { Cell::new(0) };
        static BUDGET: Cell<u64> = const { Cell::new(u64::MAX) };
    }

    pub(crate) fn step() {
        let steps = STEPS.get() + 1;
        STEPS.set(steps);
        assert!(
            steps <= BUDGET.get(),
            "the walk went over its budget of {} steps",
            BUDGET.get()
        );
    }

    /// Runs `f` under a budget of `budget` steps and returns its result and the steps it took.
    pub(crate) fn run<R>(budget: u64, f: impl FnOnce() -> R) -> (R, u64) {
        STEPS.set(0);
        BUDGET.set(budget);
        let result = f();
        BUDGET.set(u64::MAX);
        (result, STEPS.get())
    }
}

/// What a [`PathCache`] knows about a file reference (record number plus
/// sequence, as [`NtfsFile::reference`](crate::NtfsFile::reference) and
/// [`NtfsFileName::parent_reference`] return it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CachedPath<'a> {
    /// Not looked up yet.
    Unknown,
    /// Resolved to this path by an earlier [`Mft::resolve_path`] call.
    Resolved(&'a Path),
    /// An earlier [`Mft::resolve_path`] call could not resolve this
    /// reference (a missing, freed or stale record, a parent loop, or a path
    /// too long). Do not walk the same failing chain again.
    Failed,
}

/// Directory paths keyed by full file reference (record number plus sequence
/// number, not just the record number), reused across [`Mft::resolve_path`]
/// calls. [`DefaultPathCache`] is the implementation to use; `()` caches
/// nothing. Keying by the full reference lets a stale reference and a valid
/// one sharing a record number coexist without either poisoning the other.
/// Do not reuse an instance across two [`Mft`]s: a reference is only
/// meaningful for the `Mft` it came from.
pub trait PathCache {
    /// What is known about `reference`.
    fn get(&self, reference: u64) -> CachedPath<'_>;
    /// Remember that `reference` (a directory) has this full path.
    fn insert(&mut self, reference: u64, path: PathBuf);
    /// Remember that `reference` could not be resolved, so a later lookup
    /// can return [`CachedPath::Failed`] instead of repeating the failing
    /// walk.
    fn insert_failed(&mut self, reference: u64);
}

impl PathCache for () {
    fn get(&self, _reference: u64) -> CachedPath<'_> {
        CachedPath::Unknown
    }

    fn insert(&mut self, _reference: u64, _path: PathBuf) {}

    fn insert_failed(&mut self, _reference: u64) {}
}

/// The full path of every directory resolved so far, and every reference
/// known not to resolve. Grows with directories visited, not the volume, so
/// it suits a few lookups as well as a full scan. Do not reuse across two
/// [`Mft`]s (see [`PathCache`]).
#[derive(Default)]
pub struct DefaultPathCache(HashMap<u64, Option<PathBuf>>);

impl fmt::Debug for DefaultPathCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DefaultPathCache")
            .field("len", &self.len())
            .finish()
    }
}

impl DefaultPathCache {
    /// An empty cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of references cached, resolved or failed.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether nothing is cached.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl PathCache for DefaultPathCache {
    fn get(&self, reference: u64) -> CachedPath<'_> {
        match self.0.get(&reference) {
            None => CachedPath::Unknown,
            Some(None) => CachedPath::Failed,
            Some(Some(path)) => CachedPath::Resolved(path.as_path()),
        }
    }

    fn insert(&mut self, reference: u64, path: PathBuf) {
        self.0.insert(reference, Some(path));
    }

    fn insert_failed(&mut self, reference: u64) {
        self.0.insert(reference, None);
    }
}

/// The result of [`Mft::resolve_deleted_path`]: the path, and the marker
/// where the walk stopped short of the volume, if it did.
///
/// A complete path (`marker` is `None`) starts with the volume path like
/// [`Mft::resolve_path`]'s: `\\.\C:\dir\file.txt`. An incomplete one is
/// relative: the names that resolved below the marker, ending with the
/// file's, `dir\file.txt` under [`DeletedPathMarker::Lost`]. Either way
/// `path` holds only names read from records, never marker text, so a
/// recovery tool can recreate it under a folder of its own, with a name of
/// its choosing for the marker. [`to_marked_path`](Self::to_marked_path)
/// gives the one-path form for display, `\\.\C:\<lost 1234>\dir\file.txt`.
///
/// NTFS keeps no rename history: a directory shows the name in its record,
/// current if live, or as of its deletion. The path is where the file was
/// when its directories were last renamed, not necessarily when deleted.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct DeletedPath {
    /// Why the path stops short of the volume, or `None` if every directory
    /// up to the volume was identified.
    pub marker: Option<DeletedPathMarker>,
    /// The whole path from the volume path if complete, else the names below
    /// the marker (just the file's name under [`DeletedPathMarker::TooLong`]).
    /// It names where the file was, not something openable: the file is
    /// deleted.
    pub path: PathBuf,
}

impl DeletedPath {
    /// Whether every directory up to the volume was identified: no marker.
    pub fn is_complete(&self) -> bool {
        self.marker.is_none()
    }

    /// The path with the marker as a component under `volume`, the volume
    /// path the [`Mft`] was loaded from (`mft.volume().path()`):
    /// `\\.\C:\<lost 1234>\dir\file.txt`. A complete path is returned as is.
    /// For display: a marker's text holds `<` and `>`, illegal in a Win32
    /// name, so this is not a path to create; match on
    /// [`marker`](Self::marker) for that.
    pub fn to_marked_path(&self, volume: &Path) -> PathBuf {
        match self.marker {
            None => self.path.clone(),
            Some(marker) => join_one(
                &join_one(volume, OsStr::new(&marker.to_string())),
                self.path.as_os_str(),
            ),
        }
    }
}

/// Why [`Mft::resolve_deleted_path`] could not resolve a path up to the
/// volume. [`Display`](fmt::Display) gives the text
/// [`DeletedPath::to_marked_path`] uses as a component: `<lost 1234>`,
/// `<deleted>`, `<too long>`. No ordinary Win32 name can be one (`<` and `>`
/// are illegal; a POSIX namespace name can hold them, hence convention, not
/// proof), so it is no name to create a file or folder with: match on the
/// variant and pick a name of your own for that.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum DeletedPathMarker {
    /// A directory that could not be identified: missing, unnamed, not a
    /// directory, reused by another file, another incarnation of the record,
    /// or on a parent loop. The record number the walk could not use; for a
    /// loop, its lowest record, whichever member the walk entered at. Not
    /// related to [`FileInfo::data_lost`](crate::FileInfo::data_lost).
    Lost(u64),
    /// `$Extend\$Deleted`, where NTFS moves a directory `remove_dir_all` is
    /// about to delete, or a file deleted while open, under a random name.
    /// What is below keeps that random name; the real one is gone for good.
    Deleted,
    /// The path would exceed 32767 UTF-16 units, the Win32 limit, marker
    /// text included. Only the file's name is kept below it.
    TooLong,
}

impl fmt::Display for DeletedPathMarker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DeletedPathMarker::Lost(record_number) => write!(f, "<lost {record_number}>"),
            DeletedPathMarker::Deleted => f.write_str("<deleted>"),
            DeletedPathMarker::TooLong => f.write_str("<too long>"),
        }
    }
}

/// What a directory's path is built on: the volume, a marker, or another
/// directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Parent {
    Volume,
    /// [`DeletedPathMarker::Lost`] or [`DeletedPathMarker::Deleted`]; a path
    /// too long is [`DeletedEntry::TooLong`] instead.
    Marker(DeletedPathMarker),
    /// A directory with a [`DeletedEntry::Dir`] in the cache, by reference.
    Dir(u64),
}

/// What a [`DeletedPathCache`] knows about a directory, by reference. Kept as
/// the link to its parent plus its own name, not a whole path, so the cache
/// holds one name per directory however deep the tree is; a path is built
/// only for the file that asks.
#[derive(Debug)]
enum DeletedEntry {
    /// The directory's path is a marker: `$Extend\$Deleted`, or a loop member.
    Marker(DeletedPathMarker),
    /// The directory's path is its parent's plus its name.
    Dir {
        parent: Parent,
        name: OsString,
        /// Length in UTF-16 units of the whole path, at most [`MAX_PATH_UNITS`].
        units: usize,
        /// The marker the path is built on, `None` if it reaches the volume.
        marker: Option<DeletedPathMarker>,
    },
    /// This directory's path, or one above it, exceeds [`MAX_PATH_UNITS`].
    TooLong,
}

/// What [`Mft::resolve_deleted_path`] found out about directories, keyed by
/// full file reference like [`DefaultPathCache`]: its counterpart for the
/// deleted walk. Share one across a scan of many deleted files and every
/// directory is walked once, whatever the tree's shape, loops included.
/// A separate type on purpose: a deleted walk goes through freed
/// directories and ends at markers, and a live lookup must never see what
/// it left behind, or vice versa. Do not reuse across two [`Mft`]s (see
/// [`PathCache`]).
#[derive(Default)]
pub struct DeletedPathCache(HashMap<u64, DeletedEntry>);

impl fmt::Debug for DeletedPathCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeletedPathCache")
            .field("len", &self.len())
            .finish()
    }
}

impl DeletedPathCache {
    /// An empty cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of references cached, resolved or too long.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether nothing is cached.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Detects that a walk along parent references revisits a reference
/// (Brent's algorithm): exact, since only an equal reference is reported,
/// and it needs no allocation.
struct CycleDetector {
    checkpoint: Option<u64>,
    power: usize,
    steps: usize,
}

impl CycleDetector {
    fn new() -> Self {
        Self {
            checkpoint: None,
            power: 1,
            steps: 0,
        }
    }

    /// Records that the walk reached `reference`; `true` if it is in a loop.
    /// A loop is found within about twice its length.
    fn revisits(&mut self, reference: u64) -> bool {
        if self.checkpoint == Some(reference) {
            return true;
        }
        self.steps += 1;
        if self.steps == self.power {
            self.checkpoint = Some(reference);
            self.power *= 2;
            self.steps = 0;
        }
        false
    }
}

impl Mft {
    /// Full path of `name`, starting with the volume path the `Mft` was
    /// loaded from, e.g. `\\.\C:\Users\me\file.txt` for `\\.\C:`. Built
    /// from [`NtfsFileName::to_os_string`], so invalid UTF-16 still gives an
    /// openable path.
    ///
    /// Follows each parent's [`best_name`](crate::NtfsFile::best_name),
    /// live directories only; use
    /// [`resolve_deleted_path`](Self::resolve_deleted_path) for a deleted
    /// file or a path through deleted directories.
    ///
    /// Pass a [`DefaultPathCache`] to reuse directory paths between calls
    /// (`()` caches nothing); a warm cache answers the same as none.
    ///
    /// Returns `None` if a parent is missing, not in use, unnamed, stale
    /// (freed and reused, caught by comparing the full reference including
    /// sequence number), or on a loop; or if the path would exceed 32767
    /// UTF-16 units, the Win32 limit (volume path and separators counted; a
    /// name outside the Basic Multilingual Plane costs 2 units per
    /// character).
    pub fn resolve_path(&self, name: &NtfsFileName, cache: &mut impl PathCache) -> Option<PathBuf> {
        let mut components: Vec<(u64, OsString)> = Vec::new();
        // UTF-16 units walked so far, components and separators, base path
        // excluded: exceeding the limit here only ends a walk that could
        // not succeed anyway. Count the separator even for an empty name.
        let mut walked = 0usize;
        let mut cycle = CycleDetector::new();
        let mut reference = name.parent_reference();

        let mut path = loop {
            #[cfg(test)]
            walk_budget::step();
            let record_number = reference & RECORD_NUMBER_MASK;
            if record_number == ROOT_RECORD {
                let is_root = self
                    .record(ROOT_RECORD)
                    .is_some_and(|root| root.reference() == reference);
                if !is_root {
                    Self::cache_chain_as_failed(cache, &components, reference);
                    return None;
                }
                let root_path = self.volume().path();
                if components.is_empty() {
                    return join_within_limit(root_path, &name.to_os_string());
                }
                break root_path.to_path_buf();
            }
            match cache.get(reference) {
                CachedPath::Resolved(cached) => {
                    if components.is_empty() {
                        return join_within_limit(cached, &name.to_os_string());
                    }
                    break cached.to_path_buf();
                }
                CachedPath::Failed => {
                    // Everything walked so far leads to it.
                    Self::cache_chain_as_failed(cache, &components, reference);
                    return None;
                }
                CachedPath::Unknown => {}
            }
            if cycle.revisits(reference) {
                Self::cache_chain_as_failed(cache, &components, reference);
                return None;
            }

            // The record must be live: a freed record still holds its names,
            // and `best_name` returns them to whoever asks.
            let directory = self
                .record(record_number)
                .filter(|record| {
                    record.reference() == reference
                        && record.is_used()
                        && self.is_allocated(record_number)
                })
                .and_then(|record| record.best_name());
            let Some(directory) = directory else {
                Self::cache_chain_as_failed(cache, &components, reference);
                return None;
            };

            let component = directory.to_os_string();
            walked += utf16_len(&component) + 1;
            if walked > MAX_PATH_UNITS {
                // The path is too long. Nothing is cached: from where each of
                // these directories stands, the path may well be short enough.
                return None;
            }
            components.push((reference, component));
            reference = directory.parent_reference();
        };

        // Top down. A too-long directory can never resolve, so it is cached
        // as failed, and so is everything below it.
        //
        // `path_units` is `path`'s running UTF-16 length, counted once here
        // then updated per level. Recounting with `join_within_limit` each
        // time would make this loop quadratic in chain depth.
        let mut path_units = utf16_len(path.as_os_str());
        let mut resolvable = true;
        for (reference, component) in components.into_iter().rev() {
            match resolvable
                .then(|| join_component_within_limit(&path, path_units, &component))
                .flatten()
            {
                Some((joined, units)) => {
                    path = joined;
                    path_units = units;
                    cache.insert(reference, path.clone());
                }
                None => {
                    resolvable = false;
                    cache.insert_failed(reference);
                }
            }
        }
        if !resolvable {
            return None;
        }
        join_within_limit(&path, &name.to_os_string())
    }

    /// Marks every reference on a failing chain, plus the reference whose
    /// lookup failed, as unresolvable, so a later resolution through any of
    /// them short-circuits instead of repeating the walk.
    fn cache_chain_as_failed(
        cache: &mut impl PathCache,
        components: &[(u64, OsString)],
        failed_at: u64,
    ) {
        for (reference, _) in components {
            cache.insert_failed(*reference);
        }
        cache.insert_failed(failed_at);
    }
}

impl Mft {
    /// Full path of `name`, a name of a possibly deleted file. Where a
    /// directory cannot be identified the walk ends at a
    /// [`DeletedPathMarker`] and keeps the names below it, instead of giving
    /// nothing (see [`DeletedPath`]). `name` is one of the file's
    /// [`hard_links`](crate::NtfsFile::hard_links), like
    /// [`resolve_path`](Self::resolve_path)'s.
    ///
    /// A parent reference is followed for a live directory, as in
    /// [`resolve_path`](Self::resolve_path), or a freed one: not in use,
    /// not allocated, sequence one above the reference's (see
    /// [`Mft::record_by_id`]). The name is whatever the record still holds.
    /// A record reused by another file (in use, higher sequence), any other
    /// incarnation, a nonexistent or unnamed record ends the walk at
    /// [`Lost`](DeletedPathMarker::Lost); `$Extend\$Deleted` ends it at
    /// [`Deleted`](DeletedPathMarker::Deleted).
    ///
    /// With all directories live, the result matches
    /// [`resolve_path`](Self::resolve_path) and is complete, except for a
    /// delete-pending file (still open, renamed into live
    /// `$Extend\$Deleted`): `resolve_path` gives
    /// `\\.\C:\$Extend\$Deleted\<random>`, this walk the random name under
    /// [`Deleted`](DeletedPathMarker::Deleted), since it always stops at
    /// `$Deleted`. Deleting with `remove_file`/`remove_dir` one entry at a
    /// time keeps a complete path regardless of how many directories are
    /// gone; `remove_dir_all` ends at `Deleted`, since it renames the
    /// directories away first and the real names are gone for good.
    ///
    /// A parent must be a directory, live or freed, or the walk ends at
    /// `Lost`. A loop collapses to one `Lost` (its lowest record) regardless
    /// of entry point. A path over 32767 UTF-16 units, marker text (its
    /// [`Display`](fmt::Display) form) included, becomes
    /// [`TooLong`](DeletedPathMarker::TooLong) with the name alone below it.
    /// A warm cache answers the same as none.
    ///
    /// Work is bounded by directories, not files: the walk climbs to the
    /// volume, a marker, a loop, or a cached directory, remembering every
    /// directory passed, loops and too-long ones included. Share one
    /// [`DeletedPathCache`] across a scan of many files and each directory
    /// is walked once regardless of tree shape; a fresh cache per call
    /// re-walks the whole chain. Never shared with a [`PathCache`]: a live
    /// lookup never sees what this walk left behind.
    ///
    /// A directory shows the name in its record, current if live or as of
    /// deletion. NTFS keeps no rename history, so the path reflects where
    /// the file was when its directories were last renamed, not necessarily
    /// when deleted.
    ///
    /// ```no_run
    /// # use ntfs_reader::{DeletedPathCache, DeletedPathMarker, Mft, Volume};
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let mft = Mft::new(Volume::new(r"\\.\C:")?)?;
    /// let mut cache = DeletedPathCache::new();
    /// for file in mft.deleted_files() {
    ///     for name in file.hard_links() {
    ///         let found = mft.resolve_deleted_path(&name, &mut cache);
    ///         match found.marker {
    ///             None => println!("{}", found.path.display()),
    ///             Some(DeletedPathMarker::Lost(record)) => {
    ///                 println!("under lost directory {record}: {}", found.path.display())
    ///             }
    ///             // `\\.\C:\<deleted>\...` or `\\.\C:\<too long>\...`.
    ///             Some(_) => println!("{}", found.to_marked_path(mft.volume().path()).display()),
    ///         }
    ///     }
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn resolve_deleted_path(
        &self,
        name: &NtfsFileName,
        cache: &mut DeletedPathCache,
    ) -> DeletedPath {
        let leaf = name.to_os_string();
        // Directories walked so far, leaf's parent first, with no cache entry.
        let mut pending: Vec<(u64, OsString)> = Vec::new();
        let mut cycle = CycleDetector::new();
        let mut reference = name.parent_reference();

        // What the walk ended at: the volume, a marker, a cached directory to
        // build on, or a directory already known to be too long.
        let end = loop {
            #[cfg(test)]
            walk_budget::step();
            let record_number = reference & RECORD_NUMBER_MASK;
            if record_number == ROOT_RECORD {
                let is_root = self
                    .record(ROOT_RECORD)
                    .is_some_and(|root| root.reference() == reference);
                break Some(if is_root {
                    Parent::Volume
                } else {
                    Parent::Marker(DeletedPathMarker::Lost(record_number))
                });
            }
            match cache.0.get(&reference) {
                Some(DeletedEntry::Marker(marker)) => break Some(Parent::Marker(*marker)),
                Some(DeletedEntry::Dir { .. }) => break Some(Parent::Dir(reference)),
                Some(DeletedEntry::TooLong) => break None,
                None => {}
            }
            if cycle.revisits(reference) {
                // Everything walked since the earlier visit to `reference` is
                // one turn of a loop with no path of its own: one marker,
                // named by its lowest record whatever member was entered at,
                // with what leads into the loop kept below it. The walk may
                // have entered before that first visit, so members are found
                // by stepping back while it repeats itself. Every member is
                // cached as the marker: a loop is always `Lost`,
                // regardless of entry point or names.
                let visit = pending
                    .iter()
                    .position(|(visited, _)| *visited == reference)
                    .unwrap_or(pending.len());
                let turn = pending.len() - visit;
                let mut entry = visit;
                while entry > 0 && pending[entry - 1].0 == pending[entry - 1 + turn].0 {
                    entry -= 1;
                }
                let members = pending.split_off(entry);
                let lowest = members
                    .iter()
                    .map(|(member, _)| member & RECORD_NUMBER_MASK)
                    .min()
                    .unwrap_or(record_number);
                for (member, _) in members {
                    cache.0.insert(
                        member,
                        DeletedEntry::Marker(DeletedPathMarker::Lost(lowest)),
                    );
                }
                break Some(Parent::Marker(DeletedPathMarker::Lost(lowest)));
            }

            // A parent is a directory the reference names: live, or freed with
            // the sequence a delete leaves. A file's record is never one.
            let directory = self
                .record(record_number)
                .filter(|record| {
                    record.is_directory()
                        && self.reference_liveness(reference, &record.record).is_some()
                })
                .and_then(|record| record.best_name());
            let Some(directory) = directory else {
                break Some(Parent::Marker(DeletedPathMarker::Lost(record_number)));
            };

            let component = directory.to_os_string();
            if component.eq_ignore_ascii_case(DELETED_DIRECTORY)
                && directory.parent_reference() & RECORD_NUMBER_MASK == EXTEND_RECORD
            {
                cache
                    .0
                    .insert(reference, DeletedEntry::Marker(DeletedPathMarker::Deleted));
                break Some(Parent::Marker(DeletedPathMarker::Deleted));
            }
            pending.push((reference, component));
            reference = directory.parent_reference();
        };

        let Some(mut parent) = end else {
            // Everything below a too-long directory is too long.
            for (reference, _) in pending {
                cache.0.insert(reference, DeletedEntry::TooLong);
            }
            return Self::too_long_path(&leaf);
        };

        // Top down: each directory's path length is its parent's plus its own
        // name, so a level costs only its name. A directory whose path is
        // too long is cached as such, and so is everything below it.
        // The length of a path on a marker counts the marker's text, as
        // `DeletedPath::to_marked_path` writes it.
        let (mut units, mut ends_with_separator_now, marker) = match parent {
            Parent::Volume => {
                let path = self.volume().path().as_os_str();
                (utf16_len(path), ends_with_separator(path), None)
            }
            Parent::Marker(marker) => {
                let path = self.marker_path(marker);
                (
                    utf16_len(path.as_os_str()),
                    ends_with_separator(path.as_os_str()),
                    Some(marker),
                )
            }
            Parent::Dir(reference) => match cache.0.get(&reference) {
                Some(DeletedEntry::Dir {
                    units,
                    name,
                    marker,
                    ..
                }) => (*units, ends_with_separator_after(name), *marker),
                _ => unreachable!("a cached directory was just found"),
            },
        };
        let mut resolvable = true;
        for (reference, component) in pending.into_iter().rev() {
            let next = units + usize::from(!ends_with_separator_now) + utf16_len(&component);
            if resolvable && next <= MAX_PATH_UNITS {
                (units, ends_with_separator_now) = (next, ends_with_separator_after(&component));
                cache.0.insert(
                    reference,
                    DeletedEntry::Dir {
                        parent,
                        name: component,
                        units,
                        marker,
                    },
                );
                parent = Parent::Dir(reference);
            } else {
                resolvable = false;
                cache.0.insert(reference, DeletedEntry::TooLong);
            }
        }
        let leaf_units = units + usize::from(!ends_with_separator_now) + utf16_len(&leaf);
        if !resolvable || leaf_units > MAX_PATH_UNITS {
            return Self::too_long_path(&leaf);
        }
        DeletedPath {
            marker,
            path: self.assemble(parent, cache, &leaf),
        }
    }

    /// The path of the directory `parent`, followed by `leaf`, built once:
    /// from the volume path, or relative if it is built on a marker.
    fn assemble(&self, parent: Parent, cache: &DeletedPathCache, leaf: &OsStr) -> PathBuf {
        let mut names = Vec::new();
        let mut current = parent;
        let base = loop {
            match current {
                Parent::Volume => break self.volume().path(),
                Parent::Marker(_) => break Path::new(""),
                Parent::Dir(reference) => match cache.0.get(&reference) {
                    Some(DeletedEntry::Dir { parent, name, .. }) => {
                        names.push(name.as_os_str());
                        current = *parent;
                    }
                    _ => unreachable!("a directory of a path is in the cache"),
                },
            }
        };
        let size = base.as_os_str().len()
            + names.iter().map(|name| name.len() + 1).sum::<usize>()
            + leaf.len()
            + 1;
        let mut path = OsString::with_capacity(size);
        path.push(base.as_os_str());
        for component in names.into_iter().rev().chain(std::iter::once(leaf)) {
            // Relative on a marker: no leading separator, `to_marked_path`
            // adds the one after the marker. An empty first name (a corrupt
            // record) then adds nothing, and the marked path is what a walk
            // from the marker's own path builds.
            if !path.is_empty() && !ends_with_separator(&path) {
                path.push(MAIN_SEPARATOR_STR);
            }
            path.push(component);
        }
        PathBuf::from(path)
    }

    /// The volume path plus the marker's text as one component: what the
    /// length of a path on the marker is counted from.
    fn marker_path(&self, marker: DeletedPathMarker) -> PathBuf {
        join_one(self.volume().path(), OsStr::new(&marker.to_string()))
    }

    /// What a path too long to address becomes: the marker and the name.
    fn too_long_path(leaf: &OsStr) -> DeletedPath {
        DeletedPath {
            marker: Some(DeletedPathMarker::TooLong),
            path: PathBuf::from(leaf),
        }
    }
}

/// Length of `text` in UTF-16 units, how Win32 counts a path: a character
/// outside the Basic Multilingual Plane is 2 units, an unpaired surrogate (a
/// name NTFS accepts) is 1.
#[cfg(windows)]
fn utf16_len(text: &OsStr) -> usize {
    use std::os::windows::ffi::OsStrExt;
    text.encode_wide().count()
}

/// Only reached off Windows, by unit tests: there the `OsStr` holds the
/// WTF-8 `utf16_to_os_string` writes. Every character starts with a byte
/// that is not a continuation byte (`10xxxxxx`); a 4-byte start
/// (`11110xxx`) means a surrogate pair.
#[cfg(not(windows))]
fn utf16_len(text: &OsStr) -> usize {
    text.as_encoded_bytes()
        .iter()
        .map(|&byte| match byte {
            0x80..=0xBF => 0,
            0xF0.. => 2,
            _ => 1,
        })
        .sum()
}

/// [`join_one`], or `None` if the result is longer than [`MAX_PATH_UNITS`].
fn join_within_limit(base: &Path, component: &OsStr) -> Option<PathBuf> {
    let separator = usize::from(!ends_with_separator(base.as_os_str()));
    let units = utf16_len(base.as_os_str()) + separator + utf16_len(component);
    (units <= MAX_PATH_UNITS).then(|| join_one(base, component))
}

/// [`join_within_limit`], but takes `base`'s already-known UTF-16 length
/// (`base_units`) instead of recounting it, and returns the joined length
/// along with the path. Used by the top-down loop in [`Mft::resolve_path`],
/// so joining one more level costs that level's component, not the whole
/// path built so far.
fn join_component_within_limit(
    base: &Path,
    base_units: usize,
    component: &OsStr,
) -> Option<(PathBuf, usize)> {
    let separator = usize::from(!ends_with_separator(base.as_os_str()));
    let units = base_units + separator + utf16_len(component);
    (units <= MAX_PATH_UNITS).then(|| (join_one(base, component), units))
}

/// `base` plus one more path component, in a single allocation sized to the
/// exact result, unlike `PathBuf::push` on a freshly built buffer (no spare
/// capacity), which forces a reallocation and a full copy on every push.
/// `component` is `&OsStr`, not `&str`, so a lossless file name
/// (`NtfsFileName::to_os_string`) passes straight through.
fn join_one(base: &Path, component: &OsStr) -> PathBuf {
    let base_os = base.as_os_str();
    let needs_separator = !ends_with_separator(base_os);
    let separator_len = if needs_separator {
        MAIN_SEPARATOR_STR.len()
    } else {
        0
    };

    let mut buf = OsString::with_capacity(base_os.len() + separator_len + component.len());
    buf.push(base_os);
    if needs_separator {
        buf.push(MAIN_SEPARATOR_STR);
    }
    buf.push(component);
    PathBuf::from(buf)
}

/// Whether `s` already ends with a path separator, the check
/// `PathBuf::push`/`Path::join` use to decide whether to add one. Checking
/// the raw encoded bytes (not chars) is enough: separators are ASCII, and in
/// UTF-8 (or the WTF-8 `OsStr` uses on Windows) an ASCII byte never appears
/// inside a multi-byte sequence, so a trailing separator byte is unambiguous.
fn ends_with_separator(s: &OsStr) -> bool {
    s.as_encoded_bytes()
        .last()
        .is_some_and(|&b| b < 0x80 && std::path::is_separator(b as char))
}

/// Whether a path ends with a separator once `component` is joined to it. A
/// separator goes before the component unless the path already ends with
/// one, so an empty component (a corrupt name) leaves it ending with one,
/// and any other component only when it ends with one itself.
fn ends_with_separator_after(component: &OsStr) -> bool {
    component.is_empty() || ends_with_separator(component)
}

#[cfg(test)]
#[path = "tests/path/mod.rs"]
mod tests;
