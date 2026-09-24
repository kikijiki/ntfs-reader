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
    mft::Mft,
};

/// The longest path [`Mft::resolve_path`] returns, in UTF-16 units: the
/// Win32 maximum, so nothing Win32 can open is refused and nothing longer is
/// returned. A limit on the path itself, not on the number of levels, is what
/// a cache hit can apply exactly as a full walk does: the cached path has a
/// length.
const MAX_PATH_UNITS: usize = 32767;

/// What a [`PathCache`] knows about a file reference (a record number plus
/// its sequence number, as returned by
/// [`NtfsFile::reference`](crate::NtfsFile::reference) /
/// [`NtfsFileName::parent_reference`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CachedPath<'a> {
    /// Not looked up yet.
    Unknown,
    /// Resolved to this path by an earlier [`Mft::resolve_path`] call.
    Resolved(&'a Path),
    /// An earlier [`Mft::resolve_path`] call could not resolve this
    /// reference (a missing, freed or stale record, a parent loop, or a path
    /// too long). Don't walk the same failing chain again.
    Failed,
}

/// Directory paths keyed by full file reference (record number plus
/// sequence number, not just the bare record number), reused across
/// [`Mft::resolve_path`] calls. [`DefaultPathCache`] is the implementation to
/// use; `()` caches nothing. Keying by the full reference (rather than the
/// record number alone) is what lets a stale reference and a valid one that
/// happen to share a record number coexist in the same cache without either
/// poisoning the other. An instance must not be reused across two different
/// [`Mft`]s: a reference is only meaningful for the `Mft` it came from, so a
/// cache warmed on one `Mft` would return stale or wrong entries for
/// another.
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
/// known not to resolve. Its size grows with the directories visited, not
/// with the volume, so it suits a few lookups as well as a full scan. Must
/// not be reused across two different [`Mft`]s (see [`PathCache`]).
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

const RECORD_NUMBER_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;

/// Detects that a walk along parent references came back to a reference it
/// already visited (Brent's algorithm): exact, since only an equal reference
/// is reported, and it needs no allocation.
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
    /// Full path of `name`, starting with the path of the volume the `Mft`
    /// was loaded from, for example `\\.\C:\Users\me\file.txt` for a `Mft` of
    /// the volume `\\.\C:`. Every component is built from
    /// [`NtfsFileName::to_os_string`], so a name that is not valid UTF-16
    /// still gives a path that opens the file.
    ///
    /// The path follows each parent directory's
    /// [`best_name`](crate::NtfsFile::best_name). Pass a
    /// [`DefaultPathCache`] to reuse directory paths between calls (`()`
    /// caches nothing).
    ///
    /// Returns `None` if the chain of parents cannot be resolved: a parent is
    /// missing, not in use, unnamed, stale (its record number was freed and
    /// reused by another file, detected by comparing the full reference,
    /// sequence number included), or the chain loops; or if the path would
    /// be longer than Win32 can address: 32767 UTF-16 units, counting the
    /// volume path and the separators (a name outside the Basic Multilingual
    /// Plane takes 2 units per character). The result does not depend on the
    /// cache: a warm cache gives the same answer as none.
    pub fn resolve_path(&self, name: &NtfsFileName, cache: &mut impl PathCache) -> Option<PathBuf> {
        let mut components: Vec<(u64, OsString)> = Vec::new();
        // UTF-16 units the walked component names take: less than the path they
        // end up in (separators and the base path are not counted), so it only
        // ever ends a walk that cannot succeed.
        let mut walked = 0usize;
        let mut cycle = CycleDetector::new();
        let mut reference = name.parent_reference();

        let mut path = loop {
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

            // The record itself must be live: `best_name` skips free records
            // of the file's own, but a free extension record still leads to
            // its base record's names.
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
            walked += utf16_len(&component);
            if walked > MAX_PATH_UNITS {
                // Whatever is above this point, the path is too long. Nothing
                // is cached: from where each of these directories stands the
                // path may well be short enough.
                return None;
            }
            components.push((reference, component));
            reference = directory.parent_reference();
        };

        // Top down. A directory whose own path is too long can never resolve,
        // whoever asks, so (unlike a merely deep start) it is cached as failed,
        // and so is everything below it.
        //
        // `path_units` is the running UTF-16 length of `path`, counted once
        // here (the volume root, or a cached directory's path on a cache
        // hit) and then updated by one component's worth per level, instead
        // of recounting the whole path at every level: `join_within_limit`
        // would make this loop quadratic in the chain's depth.
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

    /// Marks every reference visited on a failing chain (plus the reference
    /// whose lookup actually failed) as unresolvable, so a later resolution
    /// through any of them short-circuits instead of repeating the walk.
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

/// The length of `text` in UTF-16 units, which is how Win32 counts a path: a
/// character outside the Basic Multilingual Plane is 2 units, an unpaired
/// surrogate (a name NTFS accepts) is 1.
#[cfg(windows)]
fn utf16_len(text: &OsStr) -> usize {
    use std::os::windows::ffi::OsStrExt;
    text.encode_wide().count()
}

/// Only reached off Windows, by the unit tests: there the `OsStr` holds the
/// WTF-8 that `utf16_to_os_string` writes. Every character starts with one
/// byte that is not a continuation byte (`10xxxxxx`), and a 4-byte one
/// (`11110xxx`) is a surrogate pair.
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

/// [`join_within_limit`], but taking `base`'s already-known UTF-16 length
/// (`base_units`) instead of recounting `base`, and returning the joined
/// path's length along with the path itself. What the top-down loop in
/// [`Mft::resolve_path`] uses, so joining one more level costs the size of
/// that level's component, not the whole path built so far.
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
/// capacity), which forces a reallocation and a full copy of `base` on every
/// push. `component` is `&OsStr` rather than `&str` so the lossless name of a
/// file (`NtfsFileName::to_os_string`) can be passed straight through.
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

/// Whether `s` already ends with a path separator, the same check
/// `PathBuf::push`/`Path::join` use to decide whether to add one. Checking
/// the raw encoded bytes (not chars) is enough: separators are ASCII, and in
/// UTF-8 (or the WTF-8 `OsStr` uses on Windows) an ASCII byte never appears
/// as part of a multi-byte sequence, so a trailing separator byte is
/// unambiguous.
fn ends_with_separator(s: &OsStr) -> bool {
    s.as_encoded_bytes()
        .last()
        .is_some_and(|&b| b < 0x80 && std::path::is_separator(b as char))
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;
    use crate::api::*;
    use crate::file_info::FileInfo;
    use crate::mft::test_records::*;

    /// Wraps [`DefaultPathCache`] and counts `get` calls, to show whether a
    /// failed chain gets cached.
    struct CountingCache {
        inner: DefaultPathCache,
        gets: Cell<usize>,
    }

    impl PathCache for CountingCache {
        fn get(&self, reference: u64) -> CachedPath<'_> {
            self.gets.set(self.gets.get() + 1);
            self.inner.get(reference)
        }

        fn insert(&mut self, reference: u64, path: PathBuf) {
            self.inner.insert(reference, path);
        }

        fn insert_failed(&mut self, reference: u64) {
            self.inner.insert_failed(reference);
        }
    }

    fn first_name(mft: &Mft, number: u64) -> NtfsFileName {
        mft.record(number)
            .expect("record")
            .names()
            .next()
            .expect("name")
    }

    /// A directory `number` named `name` under `parent`.
    fn directory_under(number: u64, parent: u64, name: &str) -> Vec<u8> {
        let mut record = new_record(number, 1, 0);
        set_record_flags(&mut record, directory_flags());
        let offset = add_file_name_ex(
            &mut record,
            ATTRIBUTES_OFFSET,
            1,
            parent,
            NtfsFileNamespace::Win32,
            name,
            0,
        );
        finish_record(&mut record, offset);
        record
    }

    fn file_under(number: u64, parent: u64, name: &str) -> Vec<u8> {
        let mut record = new_record(number, 1, 0);
        let offset = add_file_name_ex(
            &mut record,
            ATTRIBUTES_OFFSET,
            1,
            parent,
            NtfsFileNamespace::Win32,
            name,
            0,
        );
        finish_record(&mut record, offset);
        record
    }

    // Card 014. A parent cycle is found exactly (the same reference twice),
    // not by running into a depth limit, and remembered: every directory the
    // walk passed is cached as failed, so a second resolution under the same
    // cycle costs one lookup.
    #[test]
    fn resolve_path_caches_a_failed_chain() {
        let dir_a = FIRST_NORMAL_RECORD;
        let dir_b = FIRST_NORMAL_RECORD + 1;
        let leaf = FIRST_NORMAL_RECORD + 2;

        let mft = mft_with(vec![
            directory_under(dir_a, reference(1, dir_b), "a"),
            directory_under(dir_b, reference(1, dir_a), "b"),
            file_under(leaf, reference(1, dir_a), "leaf.txt"),
        ]);
        let name = first_name(&mft, leaf);

        let mut cache = CountingCache {
            inner: DefaultPathCache::new(),
            gets: Cell::new(0),
        };

        assert_eq!(mft.resolve_path(&name, &mut cache), None);
        let first_lookups = cache.gets.get();
        assert!(
            first_lookups < 10,
            "a loop of two directories is found within a few lookups, not by walking a depth \
             limit; got {first_lookups}",
        );
        for directory in [dir_a, dir_b] {
            assert_eq!(
                cache.inner.get(reference(1, directory)),
                CachedPath::Failed,
                "directory {directory} is on the loop and must be cached as failed",
            );
        }

        cache.gets.set(0);
        assert_eq!(mft.resolve_path(&name, &mut cache), None);
        assert_eq!(cache.gets.get(), 1, "the cached failure is one lookup");
    }

    // A loop entered through a tail, and one much longer than any depth a
    // limit would allow, are found and cached like a short one. The tail is
    // on the failing chain too.
    #[test]
    fn resolve_path_finds_a_loop_of_any_length() {
        for (tail, cycle) in [(2usize, 3usize), (0, 1500), (5, 1200)] {
            let first = FIRST_NORMAL_RECORD;
            let mut records = Vec::new();
            // Records `first..first + tail` lead into the cycle, which
            // occupies the next `cycle` records and closes on its first.
            for index in 0..tail + cycle {
                let number = first + index as u64;
                let parent = if index + 1 == tail + cycle {
                    first + tail as u64
                } else {
                    first + index as u64 + 1
                };
                records.push(directory_under(number, reference(1, parent), "d"));
            }
            let leaf = first + (tail + cycle) as u64;
            records.push(file_under(leaf, reference(1, first), "leaf"));
            let mft = mft_with(records);
            let name = first_name(&mft, leaf);

            let mut cache = DefaultPathCache::new();
            assert_eq!(mft.resolve_path(&name, &mut ()), None, "{tail}+{cycle}");
            assert_eq!(mft.resolve_path(&name, &mut cache), None, "{tail}+{cycle}");
            for index in 0..tail + cycle {
                assert_eq!(
                    cache.get(reference(1, first + index as u64)),
                    CachedPath::Failed,
                    "{tail}+{cycle}: directory {index} leads into the loop",
                );
            }
        }
    }

    /// Record FIRST_NORMAL_RECORD holds a real directory "dir" (sequence 1).
    /// `stale` references it with sequence 9 (a leftover reference to
    /// whatever occupied that record before "dir"); `valid` references it
    /// with the real, current sequence 1.
    fn stale_and_valid_siblings() -> (Mft, NtfsFileName, NtfsFileName) {
        let mut dir = new_record(FIRST_NORMAL_RECORD, 1, 0);
        set_record_flags(&mut dir, directory_flags());
        let offset = add_file_name(&mut dir, ATTRIBUTES_OFFSET, "dir", 0);
        finish_record(&mut dir, offset);

        let mut stale_file = new_record(FIRST_NORMAL_RECORD + 1, 1, 0);
        let offset = add_file_name_ex(
            &mut stale_file,
            ATTRIBUTES_OFFSET,
            1,
            reference(9, FIRST_NORMAL_RECORD),
            NtfsFileNamespace::Win32,
            "stale.txt",
            0,
        );
        finish_record(&mut stale_file, offset);

        let mut valid_file = new_record(FIRST_NORMAL_RECORD + 2, 1, 0);
        let offset = add_file_name_ex(
            &mut valid_file,
            ATTRIBUTES_OFFSET,
            1,
            reference(1, FIRST_NORMAL_RECORD),
            NtfsFileNamespace::Win32,
            "valid.txt",
            0,
        );
        finish_record(&mut valid_file, offset);

        let mft = mft_with(vec![dir, stale_file, valid_file]);
        let stale_name = first_name(&mft, FIRST_NORMAL_RECORD + 1);
        let valid_name = first_name(&mft, FIRST_NORMAL_RECORD + 2);
        (mft, stale_name, valid_name)
    }

    // Card 014 (found by review after the first fix). The cache must be
    // keyed by the full reference (record + sequence), not the bare record
    // number: a stale reference resolved first must not poison the cache for
    // a later, valid sibling that references the same record number with the
    // current sequence.
    #[test]
    fn resolve_path_does_not_cache_a_failure_across_a_valid_sibling() {
        let (mft, stale_name, valid_name) = stale_and_valid_siblings();
        let mut cache = DefaultPathCache::new();

        assert_eq!(
            mft.resolve_path(&stale_name, &mut cache),
            None,
            "the stale reference must not resolve",
        );
        assert_eq!(
            mft.resolve_path(&valid_name, &mut cache),
            Some(PathBuf::from(r"\\.\T:").join("dir").join("valid.txt")),
            "a later, valid sibling must still resolve even though the same record number was \
             cached as failed for the stale reference",
        );
    }

    // Card 014 (found by review after the first fix): the reverse ordering.
    // A valid reference resolved first must not let a later, stale reference
    // to the same record number reuse its cached path.
    #[test]
    fn resolve_path_does_not_reuse_a_cached_path_for_a_stale_reference() {
        let (mft, stale_name, valid_name) = stale_and_valid_siblings();
        let mut cache = DefaultPathCache::new();

        assert_eq!(
            mft.resolve_path(&valid_name, &mut cache),
            Some(PathBuf::from(r"\\.\T:").join("dir").join("valid.txt")),
        );
        assert_eq!(
            mft.resolve_path(&stale_name, &mut cache),
            None,
            "a stale reference must not reuse the path cached for the valid reference",
        );
    }

    // Card 014 (found by review after the first fix). The root special-case
    // must also check the reference, not just the record number: a parent
    // reference that masks to ROOT_RECORD but with the wrong sequence must
    // not be treated as the root.
    #[test]
    fn resolve_path_rejects_stale_root_reference() {
        let mut root = new_record(ROOT_RECORD, 3, 0);
        set_record_flags(&mut root, directory_flags());
        let offset = add_end_marker(&mut root, ATTRIBUTES_OFFSET);
        finish_record(&mut root, offset);

        let mut file = new_record(FIRST_NORMAL_RECORD, 1, 0);
        let offset = add_file_name_ex(
            &mut file,
            ATTRIBUTES_OFFSET,
            1,
            reference(9, ROOT_RECORD),
            NtfsFileNamespace::Win32,
            "file.txt",
            0,
        );
        finish_record(&mut file, offset);

        let mft = mft_with_at(vec![(ROOT_RECORD, root), (FIRST_NORMAL_RECORD, file)]);
        let name = first_name(&mft, FIRST_NORMAL_RECORD);

        let mut cache = DefaultPathCache::new();
        assert_eq!(mft.resolve_path(&name, &mut cache), None);
    }

    // Card 034. A Windows filename is a raw UTF-16 code unit sequence with
    // no requirement that it be *valid* UTF-16 - NTFS accepts an unpaired
    // surrogate (0xD800..=0xDFFF), which `str::encode_utf16` could never
    // produce but a real file can have. `resolve_path` must build its
    // result from the lossless conversion, not `Display`/`to_string`
    // (which substitutes U+FFFD and would point at a path that doesn't
    // exist).
    #[test]
    fn resolve_path_preserves_a_name_with_an_unpaired_surrogate() {
        let mut file = new_record(FIRST_NORMAL_RECORD, 1, 0);
        let raw_name: Vec<u16> = "bad"
            .encode_utf16()
            .chain([0xD800u16])
            .chain("name".encode_utf16())
            .collect();
        let offset = add_file_name_raw(
            &mut file,
            ATTRIBUTES_OFFSET,
            1,
            reference(ROOT_SEQUENCE, ROOT_RECORD),
            NtfsFileNamespace::Win32,
            &raw_name,
            0,
        );
        finish_record(&mut file, offset);

        let mft = mft_with(vec![file]);
        let name = first_name(&mft, FIRST_NORMAL_RECORD);

        let mut cache = DefaultPathCache::new();
        let resolved = mft.resolve_path(&name, &mut cache).expect("resolves");

        // The volume path, a separator, then the name: checked unit for unit
        // (the lone surrogate included), not by joining with the crate's own
        // conversion of the name.
        let separator = MAIN_SEPARATOR_STR;
        let units: Vec<u16> = VOLUME_PATH
            .encode_utf16()
            .chain(separator.encode_utf16())
            .chain(raw_name.iter().copied())
            .collect();
        let mut wtf8 = [VOLUME_PATH.as_bytes(), separator.as_bytes(), b"bad"].concat();
        wtf8.extend([0xED, 0xA0, 0x80]);
        wtf8.extend(b"name");
        assert_os_str_is(resolved.as_os_str(), &units, &wtf8);
    }

    /// A chain of `depth` directories with the given name, each under the
    /// previous one (the first under the root), then a file under the
    /// deepest and one under the directory at `shallow_level`. Returns the mft
    /// and the two names.
    fn deep_chain(
        depth: usize,
        shallow_level: usize,
        directory_name: &str,
    ) -> (Mft, NtfsFileName, NtfsFileName) {
        let mut records = Vec::new();
        for level in 0..depth {
            let number = FIRST_NORMAL_RECORD + level as u64;
            let parent = if level == 0 {
                reference(ROOT_SEQUENCE, ROOT_RECORD)
            } else {
                reference(1, number - 1)
            };
            records.push(directory_under(number, parent, directory_name));
        }
        for (index, level) in [depth - 1, shallow_level].into_iter().enumerate() {
            let number = FIRST_NORMAL_RECORD + (depth + index) as u64;
            let parent = reference(1, FIRST_NORMAL_RECORD + level as u64);
            records.push(file_under(number, parent, "f"));
        }
        let mft = mft_with(records);
        let deep = first_name(&mft, FIRST_NORMAL_RECORD + depth as u64);
        let shallow = first_name(&mft, FIRST_NORMAL_RECORD + depth as u64 + 1);
        (mft, deep, shallow)
    }

    /// What a full `FileInfo::with_cache` scan leaves in the cache: every
    /// directory in record order, so parents come first.
    fn scan(mft: &Mft) -> DefaultPathCache {
        let mut cache = DefaultPathCache::new();
        for file in mft.files() {
            FileInfo::with_cache(&file, &mut cache);
        }
        cache
    }

    // A chain deeper than the old 1024-level limit is not special. It
    // resolves, and resolves to the same path with no cache, with a cache
    // filled by a scan (which visits parents first, so the walk itself is
    // short), and with a cache holding some of the directories.
    #[test]
    fn resolve_path_gives_the_same_answer_for_a_deep_chain_cached_or_not() {
        let (mft, deep, shallow) = deep_chain(1500, 1200, "d");

        let plain = mft.resolve_path(&deep, &mut ());
        let plain_shallow = mft.resolve_path(&shallow, &mut ());

        let mut warm = scan(&mft);
        // Not assert_eq!/expect on the paths: they are thousands of characters long.
        assert!(
            mft.resolve_path(&deep, &mut warm) == plain,
            "a warm cache and no cache disagree"
        );
        assert!(
            mft.resolve_path(&shallow, &mut warm) == plain_shallow,
            "a warm cache and no cache disagree"
        );

        let path = plain
            .as_ref()
            .expect("a chain of 1500 directories resolves");
        assert_eq!(
            path.as_os_str().len(),
            mft.volume().path().as_os_str().len() + 2 * 1501
        );
        assert!(plain_shallow.is_some(), "a name 1200 levels down resolves");

        let mut cold = DefaultPathCache::new();
        assert!(
            mft.resolve_path(&shallow, &mut cold) == plain_shallow,
            "cold cache"
        );
        assert!(
            mft.resolve_path(&deep, &mut cold) == plain,
            "partly warm cache"
        );
    }

    /// UTF-16 units in `path`, counted the platform's own way instead of by the crate.
    fn native_units(path: &Path) -> usize {
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;
            path.as_os_str().encode_wide().count()
        }
        #[cfg(not(windows))]
        {
            utf16_len(path.as_os_str())
        }
    }

    /// A name of exactly `units` UTF-16 units: `piece` repeated as often as it
    /// fits whole, then `x` for the rest.
    fn name_of(piece: &[u16], units: usize) -> Vec<u16> {
        let mut name: Vec<u16> = piece
            .iter()
            .copied()
            .cycle()
            .take(units / piece.len() * piece.len())
            .collect();
        name.resize(units, u16::from(b'x'));
        name
    }

    fn record_named(number: u64, parent: u64, name: &[u16], directory: bool) -> Vec<u8> {
        let mut record = new_record(number, 1, 0);
        if directory {
            set_record_flags(&mut record, directory_flags());
        }
        let offset = add_file_name_raw(
            &mut record,
            ATTRIBUTES_OFFSET,
            1,
            parent,
            NtfsFileNamespace::Win32,
            name,
            0,
        );
        finish_record(&mut record, offset);
        record
    }

    /// A chain of directories with 254-unit names under the root, ending in a
    /// directory `end` whose path is exactly `total` UTF-16 units (its own name
    /// makes up the difference, built from `piece`). Under `end` is a file `f`,
    /// which is always past the limit, and under the first directory a file `s`,
    /// which is always well inside it. Returns the mft, the numbers of `end`,
    /// `f` and `s`.
    fn chain_ending_at(piece: &[u16], total: usize) -> (Mft, u64, u64, u64) {
        const LEVEL: usize = 254;
        // The volume path, then a separator and a name for each level.
        let name_units = total - VOLUME_PATH.len() - 1;
        let directories = (name_units - 1) / (LEVEL + 1);
        let end_units = name_units - directories * (LEVEL + 1);
        assert!((1..=LEVEL + 1).contains(&end_units) && end_units <= 255);

        let root = reference(ROOT_SEQUENCE, ROOT_RECORD);
        let mut records = Vec::new();
        for level in 0..directories {
            let number = FIRST_NORMAL_RECORD + level as u64;
            let parent = if level == 0 {
                root
            } else {
                reference(1, number - 1)
            };
            records.push(record_named(number, parent, &name_of(piece, LEVEL), true));
        }
        let end = FIRST_NORMAL_RECORD + directories as u64;
        let last = if directories == 0 {
            root
        } else {
            reference(1, end - 1)
        };
        records.push(record_named(end, last, &name_of(piece, end_units), true));
        records.push(record_named(
            end + 1,
            reference(1, end),
            &[u16::from(b'f')],
            false,
        ));
        records.push(record_named(
            end + 2,
            reference(1, FIRST_NORMAL_RECORD),
            &[u16::from(b's')],
            false,
        ));
        (mft_with(records), end, end + 1, end + 2)
    }

    fn resolve_each(
        mft: &Mft,
        names: [&NtfsFileName; 3],
        cache: &mut impl PathCache,
    ) -> [Option<PathBuf>; 3] {
        names.map(|name| mft.resolve_path(name, cache))
    }

    // Win32 addresses paths of up to 32767 UTF-16 units, and that is exactly
    // the limit: a path of 32767 units resolves and one of 32768 does not,
    // whatever the units are made of (a surrogate pair is 2 units, an
    // unpaired surrogate 1, though it takes 3 bytes) and whatever the cache
    // holds. Giving up on a path that is too long caches nothing that is wrong
    // for the directories above it, and a directory whose own path is too long
    // can never resolve.
    #[test]
    fn resolve_path_stops_at_exactly_32767_utf16_units() {
        let rows: [(&str, &[u16]); 3] = [
            ("ascii", &[b'x' as u16]),
            ("surrogate pair", &[0xD83D, 0xDE00]),
            ("unpaired surrogate", &[0xD800]),
        ];
        for (what, piece) in rows {
            for (total, fits) in [(32767, true), (32768, false)] {
                let row = format!("{what}, {total} units");
                let (mft, end, file, shallow) = chain_ending_at(piece, total);
                let end_name = first_name(&mft, end);
                let file_name = first_name(&mft, file);
                let shallow_name = first_name(&mft, shallow);

                let expected = mft.resolve_path(&shallow_name, &mut ());
                assert!(expected.is_some(), "{row}: the short path resolves");
                let names = [&end_name, &file_name, &shallow_name];
                let mut fresh = DefaultPathCache::new();
                let mut warm = scan(&mft);
                let runs = [
                    ("no cache", resolve_each(&mft, names, &mut ())),
                    ("fresh cache", resolve_each(&mft, names, &mut fresh)),
                    ("warm cache", resolve_each(&mft, names, &mut warm)),
                ];
                for (cache_name, [resolved, under, short]) in runs {
                    assert_eq!(resolved.is_some(), fits, "{row}, {cache_name}");
                    if let Some(path) = resolved {
                        assert_eq!(native_units(&path), total, "{row}, {cache_name}");
                    }
                    assert!(
                        under.is_none(),
                        "{row}, {cache_name}: a file under the directory is past the limit"
                    );
                    assert!(
                        short == expected,
                        "{row}, {cache_name}: giving up on the long path changed the short one"
                    );
                }

                let end_reference = reference(1, end);
                let directory = mft.resolve_path(&end_name, &mut ());
                match (fits, warm.get(end_reference)) {
                    (true, CachedPath::Resolved(path)) => {
                        assert_eq!(Some(path), directory.as_deref(), "{row}")
                    }
                    (false, CachedPath::Failed) => {}
                    (_, cached) => panic!("{row}: the directory is cached as {cached:?}"),
                }
            }
        }
    }

    // A freed directory record still holds its name, sequence number and
    // parent; a name under it must not resolve. Neither must one under a
    // record whose bitmap bit is clear, nor under a free extension record of
    // a live directory, which would otherwise lead to the directory's names,
    // nor under one that does not exist at all (past the last record, card 016).
    #[test]
    fn resolve_path_rejects_a_parent_that_is_not_in_use() {
        let root = reference(ROOT_SEQUENCE, ROOT_RECORD);
        let first = FIRST_NORMAL_RECORD;
        let (freed, unallocated, live, ext_freed, ext_unallocated) =
            (first, first + 1, first + 2, first + 3, first + 4);
        let missing = first + 50;

        let mut freed_record = directory_under(freed, root, "freed");
        set_record_flags(&mut freed_record, NtfsFileFlags::IsDirectory as u16);
        // In use by its own flag, but its bitmap bit is clear.
        let unallocated_record = directory_under(unallocated, root, "unallocated");
        let live_record = directory_under(live, root, "live");
        let extension = |number, flags| {
            let mut record = new_record(number, 1, reference(1, live));
            set_record_flags(&mut record, flags);
            let offset = add_end_marker(&mut record, ATTRIBUTES_OFFSET);
            finish_record(&mut record, offset);
            record
        };
        let cases = [
            (freed, "a freed directory"),
            (unallocated, "an unallocated directory"),
            (ext_freed, "a freed extension record"),
            (ext_unallocated, "an unallocated extension record"),
            (missing, "a record past the last one"),
        ];
        let mut records = vec![
            freed_record,
            unallocated_record,
            live_record,
            extension(ext_freed, 0),
            extension(ext_unallocated, directory_flags()),
        ];
        for (index, (parent, _)) in cases.iter().enumerate() {
            records.push(file_under(
                first + 5 + index as u64,
                reference(1, *parent),
                "f",
            ));
        }
        // A control: the same walk through a live directory does resolve.
        let control = first + 5 + cases.len() as u64;
        records.push(file_under(control, reference(1, live), "f"));

        let (volume, data, mut bitmap) = raw_parts(records);
        for number in [unallocated, ext_unallocated] {
            bitmap[number as usize / 8] &= !(1 << (number % 8));
        }
        let mft = build_from_parts(volume, data, bitmap);

        for (index, (_, why)) in cases.iter().enumerate() {
            let name = first_name(&mft, first + 5 + index as u64);
            assert_eq!(mft.resolve_path(&name, &mut ()), None, "{why}");
            assert_eq!(
                mft.resolve_path(&name, &mut DefaultPathCache::new()),
                None,
                "{why}, with a cache"
            );
        }
        assert_eq!(
            mft.resolve_path(&first_name(&mft, control), &mut ()),
            Some(PathBuf::from(VOLUME_PATH).join("live").join("f")),
            "the control resolves"
        );
    }
}
