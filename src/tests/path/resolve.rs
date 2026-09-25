// Copyright (c) 2022, Matteo Bernacchia <dev@kikijiki.com>. All rights reserved.
// This project is dual licensed under the Apache License 2.0 and the MIT license.
// See the LICENSE files in the project root for details.

use std::cell::Cell;

use crate::api::*;
use crate::file_info::FileInfo;
use crate::mft::test_records::*;
use crate::path::*;

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

pub(super) fn first_name(mft: &Mft, number: u64) -> NtfsFileName {
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

// A parent cycle is found by repeated reference, not a depth limit, and remembered: every
// directory the walk passed is cached as failed, so a second resolution costs one lookup.
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

// A loop entered through a tail, or far longer than any depth limit would allow, is found and
// cached like a short one; the tail is on the failing chain too.
#[test]
fn resolve_path_finds_a_loop_of_any_length() {
    for (tail, cycle) in [(2usize, 3usize), (0, 1500), (5, 1200)] {
        let first = FIRST_NORMAL_RECORD;
        let mut records = Vec::new();
        // `first..first+tail` leads into the cycle; the next `cycle` records form it and close
        // on its first.
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

/// FIRST_NORMAL_RECORD holds directory "dir" (sequence 1). `stale` references it with sequence
/// 9, as if left over from whatever occupied the record before; `valid` uses the current
/// sequence 1.
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

// Found by review after the first fix: the cache must key on the full reference (record +
// sequence), not the bare record number, or a stale reference resolved first poisons the cache
// for a later, valid sibling with the same record number.
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

// Found by review after the first fix, reverse ordering: a valid reference resolved first must
// not let a later, stale reference to the same record number reuse its cached path.
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

// Found by review after the first fix: the root special case must also check the sequence, not
// just the record number, or a parent reference masking to ROOT_RECORD with the wrong sequence
// counts as the root.
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

// A Windows filename need not be valid UTF-16: NTFS allows an unpaired surrogate
// (0xD800..=0xDFFF), which `str::encode_utf16` can never produce but a real file can have.
// `resolve_path` must use the lossless conversion, not `Display`/`to_string`, which substitutes
// U+FFFD and would point at a path that does not exist.
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

    // Checked unit by unit against volume path + separator + name, surrogate included, not by
    // joining with the crate's own conversion of the name.
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

/// A chain of `depth` directories with the given name, each under the previous (first under
/// root), a file under the deepest, and one under the directory at `shallow_level`. Returns the
/// mft and the two file names.
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

// A chain deeper than the old 1024-level limit is not special: it resolves to the same path
// with no cache, a cache filled by a scan (parents first, so the walk is short), or a cache
// holding only some of the directories.
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
pub(super) fn native_units(path: &Path) -> usize {
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

/// A chain of directories with 254-unit names under the root, ending in directory `end` whose
/// path is exactly `total` UTF-16 units (its own name, built from `piece`, makes up the
/// difference). File `f` under `end` is always past the limit; file `s` under the first
/// directory is always well inside it. Returns the mft and the three record numbers.
pub(super) fn chain_ending_at(piece: &[u16], total: usize) -> (Mft, u64, u64, u64) {
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

// Win32 addresses paths of up to 32767 UTF-16 units: 32767 resolves, 32768 does not, regardless
// of unit makeup (a surrogate pair is 2 units, an unpaired surrogate 1 despite its 3 bytes) or
// cache state. Giving up on an overlong path caches nothing wrong for the directories above it;
// a directory whose own path is too long never resolves.
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

// A freed directory record still holds its name, sequence and parent, but a name under it must
// not resolve. Same for a record whose bitmap bit is clear, a freed extension record of a live
// directory (which would otherwise reach the directory's names), and a record past the last one.
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

// A directory whose in-use flag and `$BITMAP` bit disagree is neither live nor deleted: a live
// walk refuses it as a parent, and a scan must not cache its path either, or a child resolves
// warm but not cold. Its name survives in a live extension record (see
// `a_base_that_is_neither_live_nor_freed_keeps_its_live_extension_records`), so `FileInfo` still
// finds and resolves it. Seen failing when `FileInfo` cached every non-deleted directory.
#[test]
fn a_scan_does_not_cache_a_directory_whose_flag_and_bitmap_disagree() {
    let (dir, extension, child) = (
        FIRST_NORMAL_RECORD,
        FIRST_NORMAL_RECORD + 1,
        FIRST_NORMAL_RECORD + 2,
    );
    for in_use in [true, false] {
        let mut directory = new_record(dir, 1, 0);
        set_record_flags(&mut directory, directory_flags());
        let offset = add_end_marker(&mut directory, ATTRIBUTES_OFFSET);
        finish_record(&mut directory, offset);
        if !in_use {
            mark_freed(&mut directory);
        }
        let mut name_record = new_record(extension, 1, reference(1, dir));
        let offset = add_file_name_ex(
            &mut name_record,
            ATTRIBUTES_OFFSET,
            1,
            reference(ROOT_SEQUENCE, ROOT_RECORD),
            NtfsFileNamespace::Win32,
            "dir",
            0,
        );
        finish_record(&mut name_record, offset);
        let file = file_under(child, reference(1, dir), "child.txt");
        let unallocated: &[u64] = if in_use { &[FIRST_NORMAL_RECORD] } else { &[] };
        let mft = mft_with_freed(vec![directory, name_record, file], unallocated);
        let name = first_name(&mft, child);
        let cold = mft.resolve_path(&name, &mut ());
        assert_eq!(cold, None, "in use {in_use}");

        let mut cache = DefaultPathCache::new();
        let directory = mft.record(dir).expect("record");
        let info = FileInfo::with_cache(&directory, &mut cache);
        assert_eq!(info.name, "dir", "in use {in_use}");
        assert!(info.is_directory && !info.is_deleted, "in use {in_use}");
        assert_eq!(
            mft.resolve_path(&name, &mut cache),
            cold,
            "a warm cache and no cache disagree, in use {in_use}"
        );
    }
}
