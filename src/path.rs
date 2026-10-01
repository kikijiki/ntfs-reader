// Copyright (c) 2022, Matteo Bernacchia <dev@kikijiki.com>. All rights reserved.
// This project is dual licensed under the Apache License 2.0 and the MIT license.
// See the LICENSE files in the project root for details.

//! Full paths: [`Mft::resolve_path`] and the [`PathCache`] it can use.

use std::{
    collections::HashMap,
    ffi::{OsStr, OsString},
    fmt, mem,
    path::{Path, PathBuf, MAIN_SEPARATOR_STR},
};

use crate::{
    api::{NtfsFileName, ROOT_RECORD},
    file::NtfsFile,
    mft::{Liveness, Mft, MftId, RECORD_NUMBER_MASK},
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
///
/// A reference is only meaningful for the `Mft` it came from: reusing an instance across two
/// `Mft`s (a rescan that keeps the cache "for efficiency", a live `Mft` then an `MftScan`) can
/// resolve a reference to the wrong `Mft`'s path if the same record number and sequence occur in
/// both, coincidentally or after a wrap. [`Self::check_owner`] is how a cache protects
/// against that; [`DefaultPathCache`] and [`DeletedPathCache`] use it to start empty on a new
/// `Mft` instead of returning a stale path. A custom cache that does not override it (the
/// default does nothing) must not be reused across two `Mft`s.
pub trait PathCache {
    /// What is known about `reference`. On a bounded cache this counts as a
    /// use of `reference`, keeping it from eviction a little longer: `&mut
    /// self` records that.
    fn get(&mut self, reference: u64) -> CachedPath<'_>;
    /// Remember that `reference` (a directory) has this full path.
    fn insert(&mut self, reference: u64, path: PathBuf);
    /// Remember that `reference` could not be resolved, so a later lookup
    /// can return [`CachedPath::Failed`] instead of repeating the failing
    /// walk.
    fn insert_failed(&mut self, reference: u64);
    /// Called once by [`Mft::resolve_path`] before any lookup of the walk, with the identity of
    /// the `Mft` doing the walking. The default does nothing, so a custom cache must override it
    /// to clear on a new owner (as [`DefaultPathCache`] does) before it can safely be reused
    /// across two `Mft`s.
    fn check_owner(&mut self, owner: MftId) {
        let _ = owner;
    }
}

impl PathCache for () {
    fn get(&mut self, _reference: u64) -> CachedPath<'_> {
        CachedPath::Unknown
    }

    fn insert(&mut self, _reference: u64, _path: PathBuf) {}

    fn insert_failed(&mut self, _reference: u64) {}
}

/// One entry of [`DefaultPathCache`]'s slab, and its place in the recency
/// list (most recently used at `head`, least at `tail`). Slots freed by
/// eviction are reused, so the slab never grows past the largest number of
/// entries alive at once.
struct Slot {
    reference: u64,
    value: Option<PathBuf>,
    prev: Option<usize>,
    next: Option<usize>,
}

/// Approximate cost of one more bucket in the `HashMap<u64, usize>` index: the key, the value,
/// and one byte for hashbrown's per-bucket control tag. `HashMap` itself is what decides how
/// many buckets a given `capacity()` needs (rounded up for its load factor), so this only
/// multiplies buckets `capacity()` already reports growing by, not a guess at that rounding.
const INDEX_BUCKET_BYTES: usize = mem::size_of::<u64>() + mem::size_of::<usize>() + 1;

/// The full path of every directory resolved so far, and every reference
/// known not to resolve. Grows with directories visited, not the volume, so
/// it suits a few lookups as well as a full scan. Unbounded by default
/// ([`new`](Self::new)); [`with_max_bytes`](Self::with_max_bytes) evicts the
/// least recently used entry (by [`get`](PathCache::get),
/// [`insert`](PathCache::insert) or [`insert_failed`](PathCache::insert_failed))
/// once the cache's cost would exceed the limit. A bound never changes what
/// [`Mft::resolve_path`] returns, only how much of the walk a later lookup
/// redoes: a warm, bounded or evicting cache answers the same as none.
/// Reusing an instance across two [`Mft`]s (see [`PathCache`]) starts it
/// empty again instead of returning a path from the previous `Mft`: safe,
/// but throws away whatever it held.
///
/// The cost tracked is bytes over entries because what makes this cache big in practice is the
/// length of the paths it holds, not how many of them there are (a scan of a deeply nested
/// volume was measured at 27 of 44 MiB, one full path per directory). It is the sum of two
/// things, so the *real* heap held tracks the limit instead of a multiple of it: a resolved
/// path's `PathBuf` capacity (not its length: a clone can round up), which shrinks back on
/// eviction; and the index's and slab's own growth, charged
/// once, when it happens, and never given back (`remove` and dropping a freed slot do not shrink
/// either structure). `Vec::capacity` only grows, so the slab's growth is exactly the difference
/// between two readings of it; `HashMap::capacity` is current length plus remaining headroom, so
/// it *drops* by one on every `remove` even though nothing shrank, and recovers on the next
/// `insert` without a new allocation - `index_capacity_seen`, a high-water mark, is what tells an
/// insert that recovers old headroom (not a charge) apart from one that needs a real, bigger
/// table (a charge). A cache that grew its structures once and then shrank to a few entries
/// still carries that growth as a floor on `bytes`; that floor is itself bounded by the same
/// `max_bytes` that made it grow in the first place, so the cache still self-limits, it just
/// cannot un-grow its bookkeeping.
#[derive(Default)]
pub struct DefaultPathCache {
    index: HashMap<u64, usize>,
    /// High-water mark of `index.capacity()`, so a dip from `remove` (see the type's doc
    /// comment) recovering on a later `insert` is not mistaken for a second real allocation.
    index_capacity_seen: usize,
    slots: Vec<Slot>,
    free: Vec<usize>,
    head: Option<usize>,
    tail: Option<usize>,
    bytes: usize,
    max_bytes: Option<usize>,
    /// The `Mft` this cache's contents belong to, `None` before the first lookup. Checked by
    /// [`PathCache::check_owner`], set on every call: a mismatch means the cache was reused
    /// across two `Mft`s, and resets everything but `max_bytes`.
    owner: Option<MftId>,
}

impl fmt::Debug for DefaultPathCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DefaultPathCache")
            .field("len", &self.len())
            .field("bytes", &self.bytes)
            .field("max_bytes", &self.max_bytes)
            .finish()
    }
}

impl DefaultPathCache {
    /// An empty, unbounded cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// An empty cache that evicts its least recently used entry whenever its
    /// total cost (see the type's doc comment) would otherwise exceed
    /// `max_bytes`. The entry just inserted is never evicted to make room
    /// for itself, so a single path longer than `max_bytes` is still cached,
    /// alone.
    pub fn with_max_bytes(max_bytes: usize) -> Self {
        Self {
            max_bytes: Some(max_bytes),
            ..Self::default()
        }
    }

    /// Number of references cached, resolved or failed.
    pub fn len(&self) -> usize {
        self.index.len()
    }

    /// Whether nothing is cached.
    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    /// Current total cost of what is cached, in the same unit as
    /// [`with_max_bytes`](Self::with_max_bytes)'s argument (see the type's
    /// doc comment for what counts).
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// A value's own heap cost: a resolved path's real `PathBuf` capacity (not its length),
    /// 0 for a failed entry. Unlike the index/slab growth charged in [`Self::put`] and
    /// [`Self::evict_tail`], this is given back: it is subtracted when the entry's value is
    /// replaced or the entry is evicted.
    fn content_cost(value: &Option<PathBuf>) -> usize {
        value.as_ref().map_or(0, PathBuf::capacity)
    }

    /// Removes `slot` from the recency list, wherever it sits.
    fn unlink(&mut self, slot: usize) {
        let (prev, next) = (self.slots[slot].prev, self.slots[slot].next);
        match prev {
            Some(prev) => self.slots[prev].next = next,
            None => self.head = next,
        }
        match next {
            Some(next) => self.slots[next].prev = prev,
            None => self.tail = prev,
        }
    }

    /// Makes `slot` the most recently used.
    fn push_front(&mut self, slot: usize) {
        self.slots[slot].prev = None;
        self.slots[slot].next = self.head;
        if let Some(head) = self.head {
            self.slots[head].prev = Some(slot);
        }
        self.head = Some(slot);
        self.tail.get_or_insert(slot);
    }

    /// Marks `slot` as just used, without changing its value.
    fn touch(&mut self, slot: usize) {
        if self.head != Some(slot) {
            self.unlink(slot);
            self.push_front(slot);
        }
    }

    /// Drops the least recently used entry. `free`'s own growth is not charged: unlike growing
    /// the index or the slab to fit a new entry (charged once, in [`Self::put`]), growing `free`
    /// is a side effect of eviction itself, and charging it here would fight the eviction it is
    /// trying to do: a growth charge bigger than the content it just freed would make the cache
    /// worse off for having evicted, never converging back under budget except by evicting down
    /// to the floor of one entry. `free` is one `usize` per slot ever evicted, a small fraction
    /// of the slab it parallels, so leaving it out of the budget costs little accuracy.
    fn evict_tail(&mut self) {
        let Some(slot) = self.tail else { return };
        self.unlink(slot);
        let entry = &mut self.slots[slot];
        self.bytes -= Self::content_cost(&entry.value);
        self.index.remove(&entry.reference);
        entry.value = None;
        self.free.push(slot);
    }

    fn put(&mut self, reference: u64, value: Option<PathBuf>) {
        let content = Self::content_cost(&value);
        if let Some(&slot) = self.index.get(&reference) {
            self.bytes -= Self::content_cost(&self.slots[slot].value);
            self.slots[slot].value = value;
            self.bytes += content;
            self.touch(slot);
        } else {
            let slot = match self.free.pop() {
                Some(slot) => slot,
                None => {
                    let before = self.slots.capacity();
                    self.slots.push(Slot {
                        reference,
                        value: None,
                        prev: None,
                        next: None,
                    });
                    self.bytes += (self.slots.capacity() - before) * mem::size_of::<Slot>();
                    self.slots.len() - 1
                }
            };
            self.slots[slot] = Slot {
                reference,
                value,
                prev: None,
                next: None,
            };
            self.index.insert(reference, slot);
            let capacity = self.index.capacity();
            if capacity > self.index_capacity_seen {
                self.bytes += (capacity - self.index_capacity_seen) * INDEX_BUCKET_BYTES;
                self.index_capacity_seen = capacity;
            }
            self.push_front(slot);
            self.bytes += content;
        }
        // Never evict down to nothing: the entry just written stays even if
        // it alone is over budget.
        while self.index.len() > 1 && self.max_bytes.is_some_and(|max| self.bytes > max) {
            self.evict_tail();
        }
    }
}

impl PathCache for DefaultPathCache {
    fn get(&mut self, reference: u64) -> CachedPath<'_> {
        let Some(&slot) = self.index.get(&reference) else {
            return CachedPath::Unknown;
        };
        self.touch(slot);
        match &self.slots[slot].value {
            None => CachedPath::Failed,
            Some(path) => CachedPath::Resolved(path.as_path()),
        }
    }

    fn insert(&mut self, reference: u64, path: PathBuf) {
        self.put(reference, Some(path));
    }

    fn insert_failed(&mut self, reference: u64) {
        self.put(reference, None);
    }

    fn check_owner(&mut self, owner: MftId) {
        if self.owner != Some(owner) {
            *self = Self {
                max_bytes: self.max_bytes,
                owner: Some(owner),
                ..Self::default()
            };
        }
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
/// it left behind, or vice versa.
///
/// Reusing an instance across two [`Mft`]s (see [`PathCache`]) starts it empty again instead of
/// walking through the wrong `Mft`'s directories: [`Mft::resolve_deleted_path`] checks
/// the owner on every call, the same protection [`PathCache::check_owner`] gives
/// [`DefaultPathCache`].
///
/// Unbounded, unlike [`DefaultPathCache`]: a directory entry here names its
/// parent by reference, so evicting one that another live entry still
/// points at would leave that pointer dangling. Bounding it soundly would need either pinning
/// every ancestor of what is kept (the working set is then whatever chain is deepest, not a
/// fixed limit) or a scheme that tears down or rebuilds a dangling chain on the next lookup.
/// This cache keeps its entries until dropped or used with another `Mft`.
#[derive(Default)]
pub struct DeletedPathCache {
    entries: HashMap<u64, DeletedEntry>,
    /// The `Mft` this cache's contents belong to; see `DefaultPathCache::owner`.
    owner: Option<MftId>,
}

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
        self.entries.len()
    }

    /// Whether nothing is cached.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Same protection as [`PathCache::check_owner`], called by
    /// [`Mft::resolve_deleted_path`] before any lookup of the walk: notices a different `Mft`
    /// than last time and starts empty instead of walking through its directories.
    fn check_owner(&mut self, owner: MftId) {
        if self.owner != Some(owner) {
            self.entries.clear();
            self.owner = Some(owner);
        }
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
        cache.check_owner(self.id());
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
                if !self.is_root_reference(reference) {
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

            let Some((component, parent)) = self.directory_name(reference) else {
                Self::cache_chain_as_failed(cache, &components, reference);
                return None;
            };

            walked += utf16_len(&component) + 1;
            if walked > MAX_PATH_UNITS {
                // The path is too long. Nothing is cached: from where each of
                // these directories stands, the path may well be short enough.
                return None;
            }
            components.push((reference, component));
            reference = parent;
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

    /// The best name of the live record `reference` names, and the parent reference that name
    /// holds: one step of [`Self::resolve_path`]'s walk. A scan's directory name index
    /// (see [`MftScan`](crate::MftScan)) is authoritative, including a missing entry: a directory
    /// readable only in pass 2 must not change the answer with the current window or cache.
    pub(crate) fn directory_name(&self, reference: u64) -> Option<(OsString, u64)> {
        let record_number = reference & RECORD_NUMBER_MASK;
        if let Some(names) = self.side().and_then(|side| side.names()) {
            let entry = names.get(record_number)?;
            return (entry.liveness == Liveness::Live && entry.reference == reference)
                .then(|| (entry.name.to_os_string(), entry.parent));
        }
        // The record must be a live base directory: a freed record still holds its names, and
        // `best_name` returns them to whoever asks; an extension record's own directory flag is
        // meaningless (see `is_directory_parent`).
        self.record(record_number)
            .filter(|record| {
                record.reference() == reference
                    && record.is_used()
                    && self.is_allocated(record_number)
                    && Self::is_directory_parent(record)
            })
            .and_then(|record| record.best_name())
            .map(|name| (name.to_os_string(), name.parent_reference()))
    }

    /// The best name of the live or freed record `reference` names, and the parent reference
    /// that name holds: one step of [`Self::resolve_deleted_path`]'s walk, live or freed
    /// directories alike. A scan's directory name index (see [`MftScan`](crate::MftScan)) also
    /// keeps freed directories, with the liveness they were indexed at. As for the live walk,
    /// a missing entry stays missing in every chunk.
    pub(crate) fn deleted_directory_name(&self, reference: u64) -> Option<(OsString, u64)> {
        let record_number = reference & RECORD_NUMBER_MASK;
        if let Some(names) = self.side().and_then(|side| side.names()) {
            let entry = names.get(record_number)?;
            return entry
                .liveness
                .names(reference, entry.reference)
                .then(|| (entry.name.to_os_string(), entry.parent));
        }
        // A parent is a directory the reference names: live, or freed with the sequence a
        // delete leaves. A file's record is never one, and neither is an extension record (see
        // `is_directory_parent`).
        self.record(record_number)
            .filter(|record| {
                Self::is_directory_parent(record)
                    && self.reference_liveness(reference, &record.record).is_some()
            })
            .and_then(|record| record.best_name())
            .map(|name| (name.to_os_string(), name.parent_reference()))
    }

    /// Root has no directory-name entry. During a scan its pass-1 bytes are authoritative too:
    /// if pass 1 rejected it, a readable root in the current window cannot revive the path.
    fn is_root_reference(&self, reference: u64) -> bool {
        let root = if let Some(side) = self.side() {
            side.record(ROOT_RECORD)
                .and_then(|data| NtfsFile::new(self, ROOT_RECORD, data))
        } else {
            self.record(ROOT_RECORD)
        };
        root.is_some_and(|root| root.reference() == reference)
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

    /// Whether `record` can stand as a directory in a parent chain: shared by
    /// [`resolve_path`](Self::resolve_path) and
    /// [`resolve_deleted_path`](Self::resolve_deleted_path), the one place
    /// that decides it. Only a base record answers
    /// [`NtfsFile::is_directory`] meaningfully ("Extension records never
    /// say: ask the base record."), so an extension record is refused
    /// whatever its own directory flag says; a parent reference naming one
    /// (corrupt: a real parent reference never does) is refused like any
    /// other unresolvable one instead of resolving to the base's name.
    fn is_directory_parent(record: &NtfsFile) -> bool {
        !record.is_extension() && record.is_directory()
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
        cache.check_owner(self.id());
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
                break Some(if self.is_root_reference(reference) {
                    Parent::Volume
                } else {
                    Parent::Marker(DeletedPathMarker::Lost(record_number))
                });
            }
            match cache.entries.get(&reference) {
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
                    cache.entries.insert(
                        member,
                        DeletedEntry::Marker(DeletedPathMarker::Lost(lowest)),
                    );
                }
                break Some(Parent::Marker(DeletedPathMarker::Lost(lowest)));
            }

            let Some((component, parent)) = self.deleted_directory_name(reference) else {
                break Some(Parent::Marker(DeletedPathMarker::Lost(record_number)));
            };

            if component.eq_ignore_ascii_case(DELETED_DIRECTORY)
                && parent & RECORD_NUMBER_MASK == EXTEND_RECORD
            {
                cache
                    .entries
                    .insert(reference, DeletedEntry::Marker(DeletedPathMarker::Deleted));
                break Some(Parent::Marker(DeletedPathMarker::Deleted));
            }
            pending.push((reference, component));
            reference = parent;
        };

        let Some(mut parent) = end else {
            // Everything below a too-long directory is too long.
            for (reference, _) in pending {
                cache.entries.insert(reference, DeletedEntry::TooLong);
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
            Parent::Dir(reference) => match cache.entries.get(&reference) {
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
                cache.entries.insert(
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
                cache.entries.insert(reference, DeletedEntry::TooLong);
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
                Parent::Dir(reference) => match cache.entries.get(&reference) {
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
