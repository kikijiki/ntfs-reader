// Copyright (c) 2022, Matteo Bernacchia <dev@kikijiki.com>. All rights reserved.
// This project is dual licensed under the Apache License 2.0 and the MIT license.
// See the LICENSE files in the project root for details.

// Unit tests of `Mft::resolve_deleted_path`: the reference rule for freed directories (measured
// on a real volume), the markers where a chain breaks, the limits shared with `resolve_path`,
// and its cache. A freed record keeps its bytes except the in-use flag and a sequence one higher
// than when live, so fixtures store the old sequence plus one and reference it with the old
// sequence.

use std::path::PathBuf;

use super::resolve::{chain_ending_at, first_name, native_units};
use crate::api::*;
use crate::file_info::FileInfo;
use crate::mft::test_records::*;
use crate::path::*;

/// A record of the fixtures. References to it carry `sequence`; the record is stored with
/// `sequence + stored_delta`, in use or not, allocated or not.
#[derive(Clone)]
struct Node {
    number: u64,
    sequence: u16,
    parent: u64,
    name: Vec<u16>,
    directory: bool,
    stored_delta: u16,
    in_use: bool,
    allocated: bool,
}

/// A reference to record `number` at sequence 1, what every node's own reference is.
fn at(number: u64) -> u64 {
    reference(1, number)
}

fn root() -> u64 {
    reference(ROOT_SEQUENCE, ROOT_RECORD)
}

fn utf16(text: &str) -> Vec<u16> {
    text.encode_utf16().collect()
}

impl Node {
    fn new(number: u64, parent: u64, name: &str, directory: bool) -> Self {
        Node {
            number,
            sequence: 1,
            parent,
            name: utf16(name),
            directory,
            stored_delta: 0,
            in_use: true,
            allocated: true,
        }
    }

    fn dir(number: u64, parent: u64, name: &str) -> Self {
        Self::new(number, parent, name, true)
    }

    fn file(number: u64, parent: u64, name: &str) -> Self {
        Self::new(number, parent, name, false)
    }

    /// Deleted: what NTFS leaves, the sequence one higher, not in use, not allocated.
    fn freed(self) -> Self {
        self.stored(1, false, false)
    }

    /// Freed and then taken by another file: in use with the sequence one higher.
    fn reused(self) -> Self {
        self.stored(1, true, true)
    }

    fn stored(mut self, delta: u16, in_use: bool, allocated: bool) -> Self {
        (self.stored_delta, self.in_use, self.allocated) = (delta, in_use, allocated);
        self
    }

    fn raw_name(mut self, name: Vec<u16>) -> Self {
        self.name = name;
        self
    }

    fn record(&self) -> Vec<u8> {
        let mut record = new_record(self.number, self.sequence + self.stored_delta, 0);
        if self.directory {
            set_record_flags(&mut record, directory_flags());
        }
        let offset = add_file_name_raw(
            &mut record,
            ATTRIBUTES_OFFSET,
            1,
            self.parent,
            NtfsFileNamespace::Win32,
            &self.name,
            0,
        );
        finish_record(&mut record, offset);
        if !self.in_use {
            mark_freed(&mut record);
        }
        record
    }
}

/// The volume: the root and `nodes`, each at its own record number.
fn volume_of(nodes: &[Node]) -> Mft {
    let mut records = vec![(ROOT_RECORD, root_record())];
    let mut freed = Vec::new();
    for node in nodes {
        records.push((node.number, node.record()));
        if !node.allocated {
            freed.push(node.number);
        }
    }
    mft_with_at_freed(records, &freed)
}

fn name_of_record(mft: &Mft, number: u64) -> NtfsFileName {
    first_name(mft, number)
}

/// The volume path and then `components`.
fn under_volume(components: &[&str]) -> PathBuf {
    components
        .iter()
        .fold(PathBuf::from(VOLUME_PATH), |path, component| {
            path.join(component)
        })
}

fn found(components: &[&str], complete: bool) -> DeletedPath {
    DeletedPath {
        path: under_volume(components),
        complete,
    }
}

fn resolve(mft: &Mft, number: u64) -> DeletedPath {
    mft.resolve_deleted_path(&name_of_record(mft, number), &mut DeletedPathCache::new())
}

/// Every name from `FIRST_NORMAL_RECORD` up gives the same answer with no cache, with a cache
/// filled in record order, and with that same cache queried in reverse.
fn assert_cache_is_transparent(mft: &Mft) {
    let names: Vec<(u64, NtfsFileName)> = (FIRST_NORMAL_RECORD..mft.record_count())
        .filter_map(|number| mft.record(number))
        .flat_map(|file| {
            let number = file.number();
            file.names().take(3).map(move |name| (number, name))
        })
        .collect();
    assert!(names.len() >= 2, "a fixture with nothing to compare");
    let plain = |name: &NtfsFileName| mft.resolve_deleted_path(name, &mut DeletedPathCache::new());

    let mut cache = DeletedPathCache::new();
    for (number, name) in &names {
        assert!(
            mft.resolve_deleted_path(name, &mut cache) == plain(name),
            "record {number}: a cache changed the deleted path"
        );
    }
    for (number, name) in names.iter().rev() {
        assert!(
            mft.resolve_deleted_path(name, &mut cache) == plain(name),
            "record {number}: a warm cache changed the deleted path"
        );
    }
}

// A deleted file in a live directory keeps its complete path, matching what the live walk gave
// it; a live file gets its live path too.
#[test]
fn a_deleted_file_in_a_live_directory_has_the_path_it_had() {
    let tree = |file: Node| {
        volume_of(&[
            Node::dir(24, root(), "docs"),
            Node::dir(25, at(24), "2026"),
            file,
        ])
    };
    let live = tree(Node::file(26, at(25), "report.txt"));
    let deleted = tree(Node::file(26, at(25), "report.txt").freed());

    let expected = found(&["docs", "2026", "report.txt"], true);
    assert_eq!(resolve(&deleted, 26), expected);
    assert_eq!(resolve(&live, 26), expected);
    assert_eq!(
        live.resolve_path(&name_of_record(&live, 26), &mut ()),
        Some(expected.path.clone())
    );
    assert_cache_is_transparent(&deleted);
}

// A file at the top of the volume: no directory at all.
#[test]
fn a_deleted_file_in_the_root_is_complete() {
    let mft = volume_of(&[Node::file(24, root(), "top.txt").freed()]);
    assert_eq!(resolve(&mft, 24), found(&["top.txt"], true));
}

// Directories freed one at a time keep their names, so the path through them to the live
// directory that held the tree stays complete, however many are freed.
#[test]
fn a_deleted_file_in_deleted_directories_is_complete() {
    let mft = volume_of(&[
        Node::dir(24, root(), "kept"),
        Node::dir(25, at(24), "l1").freed(),
        Node::dir(26, at(25), "l2").freed(),
        Node::file(27, at(26), "leaf.txt").freed(),
        Node::dir(28, root(), "gone").freed(),
        Node::file(30, at(28), "other.txt").freed(),
    ]);
    assert_eq!(
        resolve(&mft, 27),
        found(&["kept", "l1", "l2", "leaf.txt"], true)
    );
    assert_eq!(resolve(&mft, 30), found(&["gone", "other.txt"], true));
    // A freed directory's own path, and its own name as a "file": the same walk.
    assert_eq!(resolve(&mft, 26), found(&["kept", "l1", "l2"], true));
    assert_cache_is_transparent(&mft);
}

// The reference rule, parent's side: reference `(24, 1)` names the record when live at sequence 1
// or freed at sequence 2; anything else is another incarnation, breaking the chain at `<lost 24>`
// while keeping what is below. Seen failing: accepting a freed record by its own sequence
// (`Freed => reference` in `Liveness::of_reference`) fails the freed rows, accepting any sequence
// fails S and S + 2, and dropping the freed rule fails the first row.
#[test]
fn a_parent_is_named_by_its_sequence_when_live_and_by_its_sequence_plus_one_when_freed() {
    // (stored delta, in use, allocated, the parent is identified)
    #[rustfmt::skip]
    let rows = [
        (0, true, true, true),       // live, its own sequence
        (1, false, false, true),     // freed: S + 1
        (0, false, false, false),    // freed, still the sequence a reference names
        (2, false, false, false),    // freed, reused, freed again: S + 2
        (1, true, true, false),      // in use with S + 1: a reuse
        (2, true, true, false),      // in use with S + 2
        (1, false, true, false),     // the flag and the bitmap disagree
        (1, true, false, false),
        (0, true, false, false),
    ];
    for (delta, in_use, allocated, identified) in rows {
        let mft = volume_of(&[
            Node::dir(24, root(), "parent").stored(delta, in_use, allocated),
            Node::dir(25, at(24), "middle").freed(),
            Node::file(26, at(25), "leaf.txt").freed(),
        ]);
        let expected = if identified {
            found(&["parent", "middle", "leaf.txt"], true)
        } else {
            found(&["<lost 24>", "middle", "leaf.txt"], false)
        };
        let row = format!("stored +{delta}, in use {in_use}, allocated {allocated}");
        assert_eq!(resolve(&mft, 26), expected, "{row}");
        assert_eq!(resolve(&mft, 25).complete, identified, "{row}: middle");
        assert_cache_is_transparent(&mft);
    }
}

// The break keeps what resolved below it and puts the marker where the missing directory would
// be, under the volume path.
#[test]
fn a_reused_directory_breaks_the_chain_and_keeps_what_is_below() {
    let mft = volume_of(&[
        Node::dir(24, root(), "old-name").reused(),
        Node::dir(25, at(24), "a").freed(),
        Node::dir(26, at(25), "b").freed(),
        Node::file(27, at(26), "file.txt").freed(),
    ]);
    assert_eq!(
        resolve(&mft, 27),
        found(&["<lost 24>", "a", "b", "file.txt"], false)
    );
    // The reused directory's own file, if it were still listed, is not under the old directory.
    assert_eq!(resolve(&mft, 25), found(&["<lost 24>", "a"], false));
}

// A parent past the last record, one never valid, and a root not matching the reference's
// sequence.
#[test]
fn a_missing_parent_and_a_stale_root_are_lost() {
    let mft = volume_of(&[
        Node::file(24, at(500), "orphan.txt").freed(),
        Node::file(25, reference(9, ROOT_RECORD), "stale-root.txt").freed(),
        Node::file(26, at(60), "nothing.txt").freed(),
    ]);
    assert_eq!(
        resolve(&mft, 24),
        found(&["<lost 500>", "orphan.txt"], false)
    );
    assert_eq!(
        resolve(&mft, 25),
        found(&["<lost 5>", "stale-root.txt"], false)
    );
    assert_eq!(
        resolve(&mft, 26),
        found(&["<lost 60>", "nothing.txt"], false)
    );
}

// A directory that kept no name cannot be shown in a path.
#[test]
fn a_directory_without_a_name_is_lost() {
    let mut nameless = new_record(24, 2, 0);
    set_record_flags(&mut nameless, directory_flags());
    let offset = add_end_marker(&mut nameless, ATTRIBUTES_OFFSET);
    finish_record(&mut nameless, offset);
    mark_freed(&mut nameless);
    let leaf = Node::file(25, at(24), "leaf.txt").freed();
    let mft = mft_with_at_freed(
        vec![
            (ROOT_RECORD, root_record()),
            (24, nameless),
            (25, leaf.record()),
        ],
        &[24, 25],
    );
    assert_eq!(resolve(&mft, 25), found(&["<lost 24>", "leaf.txt"], false));
}

// `remove_dir_all` renames each directory under `$Extend\$Deleted` (record 29) before deleting
// it, so a freed directory keeps only that random name and its files keep theirs. The walk stops
// at `<deleted>` with the random name; without the `$Deleted` special case (a real live directory
// here) it would give the complete path `$Extend\$Deleted\...` instead. A freed delete-pending
// file keeps its own names under it.
#[test]
fn a_directory_renamed_by_remove_dir_all_ends_the_path_at_deleted() {
    let mft = volume_of(&[
        Node::dir(11, root(), "$Extend"),
        Node::dir(24, root(), "kept"),
        Node::dir(29, at(11), "$Deleted"),
        Node::dir(25, at(29), "u9kq2xw7pf3n5c8v1jd4h6ra").freed(),
        Node::dir(26, at(29), "b3m7t1z5e9y2g6k0s4w8n2ql").freed(),
        Node::file(27, at(25), "one.txt").freed(),
        Node::file(28, at(26), "two.txt").freed(),
        Node::file(30, at(29), "r8d2c6h0v4x1b5n9j3m7t2ya").freed(),
    ]);
    // What a walk that took record 29 for an ordinary directory would say.
    let live_29 = under_volume(&["$Extend", "$Deleted", "u9kq2xw7pf3n5c8v1jd4h6ra", "one.txt"]);
    assert_eq!(
        mft.resolve_path(&name_of_record(&mft, 27), &mut ()),
        None,
        "the live walk does not go through a freed directory"
    );
    let resolved = resolve(&mft, 27);
    assert_ne!(resolved.path, live_29);
    assert_eq!(
        resolved,
        found(&["<deleted>", "u9kq2xw7pf3n5c8v1jd4h6ra", "one.txt"], false)
    );
    assert_eq!(
        resolve(&mft, 28),
        found(&["<deleted>", "b3m7t1z5e9y2g6k0s4w8n2ql", "two.txt"], false)
    );
    assert_eq!(
        resolve(&mft, 30),
        found(&["<deleted>", "r8d2c6h0v4x1b5n9j3m7t2ya"], false)
    );
    assert_eq!(
        resolve(&mft, 25),
        found(&["<deleted>", "u9kq2xw7pf3n5c8v1jd4h6ra"], false)
    );
    assert_cache_is_transparent(&mft);
}

// `$Extend\$Deleted` is identified by what it is, not its record number: it is record 40 here,
// record 29 is an ordinary directory, and a `$Deleted` directly under the root (Win32 allows it)
// is not the one under `$Extend`.
#[test]
fn only_the_deleted_directory_under_extend_is_deleted() {
    let mft = volume_of(&[
        Node::dir(11, root(), "$Extend"),
        Node::dir(29, root(), "documents"),
        Node::dir(40, at(11), "$Deleted"),
        Node::dir(41, root(), "$Deleted"),
        Node::dir(42, at(40), "h4f8k2m6q0t3w7z1b5d9j2ne").freed(),
        Node::file(43, at(42), "a.txt").freed(),
        Node::file(44, at(29), "b.txt").freed(),
        Node::file(45, at(41), "c.txt").freed(),
    ]);
    assert_eq!(
        resolve(&mft, 43),
        found(&["<deleted>", "h4f8k2m6q0t3w7z1b5d9j2ne", "a.txt"], false)
    );
    assert_eq!(resolve(&mft, 44), found(&["documents", "b.txt"], true));
    assert_eq!(resolve(&mft, 45), found(&["$Deleted", "c.txt"], true));
}

// A directory loop collapses to one `<lost N>` component, named by the loop's lowest record
// regardless of entry point; what leads into the loop stays below the marker. Nothing on the loop
// or the way in is cached, so a warm cache cannot change the answer. Seen failing: naming the
// marker after the record where the walk noticed the loop gave different paths per entry point
// and disagreed between warm and cold.
#[test]
fn a_loop_of_directories_is_lost_at_its_lowest_record() {
    for (tail, cycle) in [(0usize, 2usize), (2, 3), (0, 1500), (5, 1200)] {
        let first = FIRST_NORMAL_RECORD;
        let lowest = first + tail as u64;
        let mut nodes = Vec::new();
        for index in 0..tail + cycle {
            let number = first + index as u64;
            let parent = if index + 1 == tail + cycle {
                lowest
            } else {
                number + 1
            };
            // Record `number` is the parent of record `number - 1`: the walk goes up the numbers.
            nodes.push(Node::dir(number, at(parent), &format!("d{index}")).freed());
        }
        let leaf = first + (tail + cycle) as u64;
        let other = leaf + 1;
        nodes.push(Node::file(leaf, at(first), "leaf").freed());
        // The second leaf enters the loop at its last member.
        nodes.push(Node::file(other, at(first + (tail + cycle) as u64 - 1), "other").freed());
        let mft = volume_of(&nodes);

        let marker = format!("<lost {lowest}>");
        let mut expected = vec![marker.as_str()];
        let tail_names: Vec<String> = (0..tail).rev().map(|index| format!("d{index}")).collect();
        expected.extend(tail_names.iter().map(String::as_str));
        expected.push("leaf");
        let row = format!("{tail}+{cycle}");
        assert_eq!(
            resolve(&mft, leaf),
            found(&expected, false),
            "{row}: the path into the loop"
        );
        assert_eq!(
            resolve(&mft, other),
            found(&[marker.as_str(), "other"], false),
            "{row}: entered at another member"
        );

        // Entered at the last member first, then at the first: one loop, one marker.
        let mut cache = DeletedPathCache::new();
        for number in [other, leaf, other] {
            let name = name_of_record(&mft, number);
            assert!(
                mft.resolve_deleted_path(&name, &mut cache) == resolve(&mft, number),
                "{row}: a warm cache changed the answer for {number}"
            );
        }
        // Every member of the loop is remembered as the marker, so no walk goes round it twice.
        assert!(
            cache.len() >= cycle,
            "{row}: {} of {cycle} members cached",
            cache.len()
        );
    }
}

// A loop of live directories gives the same answer: loop handling does not depend on freed
// records.
#[test]
fn a_loop_of_live_directories_is_lost_too() {
    let mft = volume_of(&[
        Node::dir(24, at(25), "a"),
        Node::dir(25, at(24), "b"),
        Node::file(26, at(25), "f").freed(),
    ]);
    assert_eq!(resolve(&mft, 26), found(&["<lost 24>", "f"], false));
}

// A path Win32 cannot address becomes `<too long>` plus the name, nothing above it. Exactly 32767
// UTF-16 units resolve, 32768 does not, whatever the units are made of, matching `resolve_path`'s
// limit and chain.
#[test]
fn a_path_is_complete_up_to_32767_utf16_units_and_too_long_after() {
    let rows: [(&str, &[u16]); 3] = [
        ("ascii", &[b'x' as u16]),
        ("surrogate pair", &[0xD83D, 0xDE00]),
        ("unpaired surrogate", &[0xD800]),
    ];
    for (what, piece) in rows {
        for (total, fits) in [(32767, true), (32768, false)] {
            let row = format!("{what}, {total} units");
            let (mft, end, file, shallow) = chain_ending_at(piece, total);
            let mut cache = DeletedPathCache::new();
            for _ in 0..2 {
                let at_end = mft.resolve_deleted_path(&first_name(&mft, end), &mut cache);
                assert_eq!(at_end.complete, fits, "{row}");
                if fits {
                    assert_eq!(native_units(&at_end.path), total, "{row}");
                    assert_eq!(
                        Some(at_end.path),
                        mft.resolve_path(&first_name(&mft, end), &mut ()),
                        "{row}: the deleted walk is the live one"
                    );
                }
                let under = mft.resolve_deleted_path(&first_name(&mft, file), &mut cache);
                assert!(
                    !under.complete && under.path.starts_with(under_volume(&["<too long>"])),
                    "{row}: a file under the directory is past the limit"
                );
                let short = mft.resolve_deleted_path(&first_name(&mft, shallow), &mut cache);
                assert!(short.complete, "{row}: the short path is not affected");
            }
            // A directory too long is remembered as such, and everything below it too; one that
            // fits is remembered with its path.
            match cache.0.get(&reference(1, end)) {
                Some(DeletedEntry::Dir { complete: true, .. }) if fits => {}
                Some(DeletedEntry::TooLong) if !fits => {}
                cached => panic!("{row}: the directory is cached as {cached:?}"),
            }
        }
    }
}

// A chain far past the limit gives `<too long>`, unchanged by a cache holding the directories.
#[test]
fn a_deep_chain_of_deleted_directories_is_too_long() {
    let long = "d".repeat(254);
    let depth = 140;
    let mut nodes = Vec::new();
    for level in 0..depth {
        let number = FIRST_NORMAL_RECORD + level;
        let parent = if level == 0 { root() } else { at(number - 1) };
        nodes.push(Node::dir(number, parent, &long).freed());
    }
    let last = FIRST_NORMAL_RECORD + depth - 1;
    let leaf = last + 1;
    let shallow = last + 2;
    nodes.push(Node::file(leaf, at(last), "deep.txt").freed());
    nodes.push(Node::file(shallow, at(FIRST_NORMAL_RECORD + 3), "shallow.txt").freed());
    let mft = volume_of(&nodes);

    let expected = found(&["<too long>", "deep.txt"], false);
    assert!(resolve(&mft, leaf) == expected);
    let mut cache = DeletedPathCache::new();
    for number in [leaf, shallow, leaf, shallow] {
        let name = name_of_record(&mft, number);
        assert!(
            mft.resolve_deleted_path(&name, &mut cache) == resolve(&mft, number),
            "a cache changed the answer for {number}"
        );
    }
    assert!(resolve(&mft, shallow).complete);
}

// A file with several hard links has one path per name, each walked on its own.
#[test]
fn every_hard_link_of_a_deleted_file_resolves_on_its_own() {
    let mut file = new_record(28, 4, 0);
    let mut offset = ATTRIBUTES_OFFSET;
    for (id, parent, name) in [
        (1, at(24), "live-link"),
        (2, at(25), "freed-link"),
        (3, at(26), "lost-link"),
        (4, at(27), "deleted-link"),
    ] {
        offset = add_file_name_ex(
            &mut file,
            offset,
            id,
            parent,
            NtfsFileNamespace::Win32,
            name,
            0,
        );
    }
    finish_record(&mut file, offset);
    mark_freed(&mut file);
    let mut records: Vec<(u64, Vec<u8>)> = [
        Node::dir(24, root(), "live-dir"),
        Node::dir(25, root(), "freed-dir").freed(),
        Node::dir(26, root(), "reused-dir").reused(),
        Node::dir(27, at(29), "random-name").freed(),
        Node::dir(29, at(11), "$Deleted"),
    ]
    .iter()
    .map(|node| (node.number, node.record()))
    .collect();
    records.push((ROOT_RECORD, root_record()));
    records.push((28, file));
    let mft = mft_with_at_freed(records, &[25, 28, 27]);

    let file = mft.record(28).expect("the file");
    assert!(!file.is_used());
    let paths: Vec<DeletedPath> = file
        .hard_links()
        .map(|name| mft.resolve_deleted_path(&name, &mut DeletedPathCache::new()))
        .collect();
    assert_eq!(
        paths,
        [
            found(&["live-dir", "live-link"], true),
            found(&["freed-dir", "freed-link"], true),
            found(&["<lost 26>", "lost-link"], false),
            found(&["<deleted>", "random-name", "deleted-link"], false),
        ]
    );
    assert_cache_is_transparent(&mft);
}

// The cache is keyed by the full reference, so references to one record the rule treats
// differently do not poison each other, in either order. Seen failing: keyed by record number
// alone, whichever reference was asked first decided the rest.
#[test]
fn a_cache_keeps_references_to_one_record_apart() {
    let mft = volume_of(&[
        Node::dir(24, root(), "dir").freed(),
        Node::file(25, at(24), "valid.txt").freed(),
        Node::file(26, reference(2, 24), "same-sequence.txt").freed(),
        Node::file(27, reference(3, 24), "one-above.txt").freed(),
    ]);
    let expected = [
        (25, found(&["dir", "valid.txt"], true)),
        (26, found(&["<lost 24>", "same-sequence.txt"], false)),
        (27, found(&["<lost 24>", "one-above.txt"], false)),
    ];
    for order in [[0usize, 1, 2], [2, 1, 0], [1, 0, 2]] {
        let mut cache = DeletedPathCache::new();
        for index in order {
            let (number, expected) = &expected[index];
            let name = name_of_record(&mft, *number);
            assert_eq!(
                &mft.resolve_deleted_path(&name, &mut cache),
                expected,
                "record {number}, order {order:?}"
            );
        }
    }
}

// Names are lossless: an unpaired surrogate in the freed directory and the file survives unit
// for unit.
#[test]
fn a_name_with_an_unpaired_surrogate_survives_in_a_deleted_path() {
    let bad_dir: Vec<u16> = "bad".encode_utf16().chain([0xD800]).collect();
    let bad_file: Vec<u16> = [0xDC00u16]
        .into_iter()
        .chain("file".encode_utf16())
        .collect();
    let mft = volume_of(&[
        Node::dir(24, root(), "").raw_name(bad_dir.clone()).freed(),
        Node::file(25, at(24), "")
            .raw_name(bad_file.clone())
            .freed(),
    ]);
    let resolved = resolve(&mft, 25);
    assert!(resolved.complete);

    let separator = MAIN_SEPARATOR_STR;
    let units: Vec<u16> = VOLUME_PATH
        .encode_utf16()
        .chain(separator.encode_utf16())
        .chain(bad_dir)
        .chain(separator.encode_utf16())
        .chain(bad_file)
        .collect();
    let mut wtf8 = [VOLUME_PATH.as_bytes(), separator.as_bytes(), b"bad"].concat();
    wtf8.extend([0xED, 0xA0, 0x80]);
    wtf8.extend(separator.as_bytes());
    wtf8.extend([0xED, 0xB0, 0x80]);
    wtf8.extend(b"file");
    assert_os_str_is(resolved.path.as_os_str(), &units, &wtf8);
}

// `FileInfo` gives a deleted file a path only when its deleted walk is complete (freed
// directories on the way included), computed without the caller's cache; `deleted` says which
// case applies. A deleted file never touches the live cache, which stays empty here.
#[test]
fn file_info_gives_a_deleted_file_a_path_only_when_it_is_complete() {
    let mft = volume_of(&[
        Node::dir(24, root(), "kept"),
        Node::dir(25, at(24), "freed-dir").freed(),
        Node::file(26, at(25), "complete.txt").freed(),
        Node::dir(27, root(), "reused").reused(),
        Node::file(28, at(27), "lost.txt").freed(),
        Node::dir(29, at(11), "$Deleted"),
        Node::file(30, at(29), "pending.bin").freed(),
        Node::file(31, at(24), "live.txt"),
    ]);
    let mut cache = DefaultPathCache::new();
    let mut info = |number| FileInfo::with_cache(&mft.record(number).expect("record"), &mut cache);

    let complete = info(26);
    assert!(complete.is_deleted);
    assert_eq!(
        complete.path,
        Some(under_volume(&["kept", "freed-dir", "complete.txt"]))
    );
    let directory = info(25);
    assert!(directory.is_deleted && directory.is_directory);
    assert_eq!(directory.path, Some(under_volume(&["kept", "freed-dir"])));
    for number in [28, 30] {
        let broken = info(number);
        assert!(broken.is_deleted, "record {number}");
        assert_eq!(broken.path, None, "record {number}");
        assert!(!broken.name.is_empty());
    }
    assert!(
        cache.is_empty(),
        "a deleted file put something in the live cache"
    );

    let live = FileInfo::with_cache(&mft.record(31).expect("record"), &mut cache);
    assert!(!live.is_deleted);
    assert_eq!(live.path, Some(under_volume(&["kept", "live.txt"])));
    // The live cache now holds a live directory; a deleted file under a freed one resolves the
    // same either way.
    assert!(!cache.is_empty());
    assert_eq!(
        FileInfo::with_cache(&mft.record(26).expect("record"), &mut cache).path,
        complete.path
    );
}

// NTFS names are case insensitive but stored as created: `$Extend\$Deleted` is found regardless
// of ASCII letter case.
#[test]
fn the_deleted_directory_is_found_whatever_the_case_of_its_name() {
    for name in ["$Deleted", "$DELETED", "$deleted", "$dElEtEd"] {
        let mft = volume_of(&[
            Node::dir(11, root(), "$Extend"),
            Node::dir(29, at(11), name),
            Node::dir(30, at(29), "h4f8k2m6q0t3w7z1b5d9j2ne").freed(),
            Node::file(31, at(30), "a.txt").freed(),
        ]);
        assert_eq!(
            resolve(&mft, 31),
            found(&["<deleted>", "h4f8k2m6q0t3w7z1b5d9j2ne", "a.txt"], false),
            "{name}"
        );
    }
    // Only ASCII letters fold, and the name must be all of it.
    for name in ["$Deleted ", "$Delete", "$Delete\u{0111}", "Deleted"] {
        let mft = volume_of(&[
            Node::dir(11, root(), "$Extend"),
            Node::dir(29, at(11), name),
            Node::file(31, at(29), "a.txt").freed(),
        ]);
        assert_eq!(
            resolve(&mft, 31),
            found(&["$Extend", name, "a.txt"], true),
            "{name}"
        );
    }
}

// Only the actual `$Extend` record makes a `$Deleted` under it special: the same name under
// another system record (10 here) is an ordinary directory, part of the path.
#[test]
fn a_deleted_directory_under_another_system_record_is_an_ordinary_directory() {
    let mft = volume_of(&[
        Node::dir(10, root(), "system"),
        Node::dir(29, at(10), "$Deleted"),
        Node::dir(30, at(29), "child").freed(),
        Node::file(31, at(30), "a.txt").freed(),
    ]);
    assert_eq!(
        resolve(&mft, 31),
        found(&["system", "$Deleted", "child", "a.txt"], true)
    );
}

// A parent reference into a zeroed slot (never formatted, or wiped) is lost, whatever sequence it
// claims. The sequence wrap accepts a freed record at 0 or 1 for a reference at 0xFFFF; a zeroed
// slot must not be mistaken for one.
#[test]
fn a_parent_slot_that_holds_zeroes_is_lost() {
    for claimed in [0u16, 1, 2, 0xFFFF] {
        let mft = volume_of(&[
            Node::file(24, reference(claimed, 25), "orphan.txt").freed(),
            Node::file(26, at(24), "after-the-hole.txt").freed(),
        ]);
        assert!(mft.record(25).is_none(), "record 25 is a zeroed slot");
        assert!(mft.record_count() > 26);
        assert_eq!(
            resolve(&mft, 24),
            found(&["<lost 25>", "orphan.txt"], false),
            "claimed sequence {claimed}"
        );
    }
}

/// `levels` directories with 254 unit names, the first under `top`, at records from 24: the
/// level `n` directory is record `24 + n`.
fn long_chain(top: u64, levels: u64) -> Vec<Node> {
    let name = "d".repeat(254);
    (0..levels)
        .map(|level| {
            let parent = if level == 0 { top } else { at(23 + level) };
            Node::dir(24 + level, parent, &name).freed()
        })
        .collect()
}

// A marker counts toward the 32767-unit limit like any other component: under `<lost 500>`, a
// directory at exactly 32767 units is remembered with its path, 32768 as too long. Counting only
// the volume path would keep the second.
#[test]
fn a_marker_counts_toward_the_32767_unit_limit() {
    // Volume path, separator and marker, then 128 levels of 1 + 254, then the directory under test.
    let fixed = VOLUME_PATH.len() + 1 + "<lost 500>".len() + 128 * 255 + 1;
    for (name_units, fits) in [(32767 - fixed, true), (32768 - fixed, false)] {
        let mut nodes = long_chain(at(500), 128);
        nodes.push(Node::dir(152, at(151), &"e".repeat(name_units)).freed());
        nodes.push(Node::file(153, at(152), "f").freed());
        let mft = volume_of(&nodes);

        let mut cache = DeletedPathCache::new();
        let resolved = mft.resolve_deleted_path(&name_of_record(&mft, 153), &mut cache);
        // A file under it is past the limit either way.
        assert_eq!(resolved, found(&["<too long>", "f"], false));
        match cache.0.get(&at(152)) {
            Some(DeletedEntry::Dir {
                units,
                complete: false,
                ..
            }) if fits => assert_eq!(*units, 32767),
            Some(DeletedEntry::TooLong) if !fits => {}
            cached => panic!("{name_units} units: the directory is cached as {cached:?}"),
        }
        assert_cache_is_transparent(&mft);
    }
}

// An empty directory name (a corrupt record) still costs its separator: joining "" adds the
// separator, leaving the path ending in one, so the next component adds no second. The cached
// length matches what `assemble` builds, so 32767 units under an empty-named directory is
// complete and one unit more is too long.
#[test]
fn an_empty_directory_name_costs_one_separator_in_the_length() {
    // The volume path and the separator the empty name adds, the first level (254 units, no
    // separator of its own), 127 more levels of 255, then a name of `n` units and its separator.
    let through_the_chain = VOLUME_PATH.len() + 1 + 254 + 127 * 255;
    for (name_units, fits) in [
        (32767 - through_the_chain - 1, true),
        (32768 - through_the_chain - 1, false),
    ] {
        let name = "e".repeat(name_units);
        let mut nodes = vec![Node::dir(24, root(), "").freed()];
        let level = "d".repeat(254);
        for index in 0..128 {
            let parent = if index == 0 { at(24) } else { at(24 + index) };
            nodes.push(Node::dir(25 + index, parent, &level).freed());
        }
        // 152 is the last level: a file and a directory of `name_units` under it, then a file.
        nodes.push(Node::file(153, at(152), &name).freed());
        nodes.push(Node::dir(154, at(152), &name).freed());
        nodes.push(Node::file(155, at(154), "f").freed());
        // A second branch under the empty-named directory, walked with the cache the first left:
        // it stops at the cached directory, so its length and separator must be right.
        for index in 0..128 {
            let parent = if index == 0 { at(24) } else { at(199 + index) };
            nodes.push(Node::dir(200 + index, parent, &level).freed());
        }
        nodes.push(Node::file(328, at(327), &name).freed());
        let mft = volume_of(&nodes);

        let mut cache = DeletedPathCache::new();
        for _ in 0..2 {
            let resolved = mft.resolve_deleted_path(&name_of_record(&mft, 153), &mut cache);
            assert_eq!(resolved.complete, fits, "{name_units} units, a file");
            if fits {
                assert_eq!(native_units(&resolved.path), 32767, "{name_units} units");
            }
        }
        let second = mft.resolve_deleted_path(&name_of_record(&mft, 328), &mut cache);
        assert_eq!(
            second.complete, fits,
            "{name_units} units, the second branch"
        );
        assert_eq!(
            second,
            resolve(&mft, 328),
            "a warm cache changed the answer"
        );
        let below = mft.resolve_deleted_path(&name_of_record(&mft, 155), &mut cache);
        assert_eq!(below.path, found(&["<too long>", "f"], false).path);
        match cache.0.get(&at(154)) {
            Some(DeletedEntry::Dir { units, .. }) if fits => assert_eq!(*units, 32767),
            Some(DeletedEntry::TooLong) if !fits => {}
            cached => panic!("{name_units} units: the directory is cached as {cached:?}"),
        }
    }
}

// A directory known to be too long (per the cache) makes everything below it too long at once;
// directories walked on the way learn it too, so no walk repeats it.
#[test]
fn a_cached_too_long_directory_makes_what_is_below_it_too_long() {
    // 129 levels: the names alone total 32766 units, under the limit, so the walk continues and
    // finds the full path (separators, volume) is 32900 units. Level 129 (record 152) is the
    // first that does not fit.
    let mut nodes = long_chain(root(), 129);
    nodes.push(Node::dir(153, at(152), "child").freed());
    nodes.push(Node::file(154, at(152), "a.txt").freed());
    nodes.push(Node::file(155, at(153), "b.txt").freed());
    let mft = volume_of(&nodes);
    let mut cache = DeletedPathCache::new();

    let a = mft.resolve_deleted_path(&name_of_record(&mft, 154), &mut cache);
    assert_eq!(a, found(&["<too long>", "a.txt"], false));
    assert!(
        matches!(cache.0.get(&at(152)), Some(DeletedEntry::TooLong)),
        "the directory that does not fit"
    );
    assert!(!cache.0.contains_key(&at(153)), "not walked yet");

    let b = mft.resolve_deleted_path(&name_of_record(&mft, 155), &mut cache);
    assert_eq!(b, found(&["<too long>", "b.txt"], false));
    assert!(
        matches!(cache.0.get(&at(153)), Some(DeletedEntry::TooLong)),
        "learnt from the cached directory"
    );
    assert_eq!(
        b,
        resolve(&mft, 155),
        "the cache does not change the answer"
    );
}

// A loop is always `<lost N>` at its lowest record, whatever its names: the walk finds the loop
// before checking length, so a loop of long names, or one over the limit, never ends in
// `<too long>` for some entry points and `<lost N>` for others.
#[test]
fn a_loop_of_long_names_is_always_lost() {
    // The number of members and the name of each by its index.
    type Case = (u64, fn(u64) -> String);
    let cases: [Case; 3] = [
        (65, |index| {
            if index < 43 {
                "d".repeat(254)
            } else {
                "s".into()
            }
        }),
        (40, |_| "d".repeat(254)),
        (130, |_| "d".repeat(254)),
    ];
    for (members, name) in cases {
        let mut nodes = Vec::new();
        for index in 0..members {
            let parent = at(24 + (index + 1) % members);
            nodes.push(Node::dir(24 + index, parent, &name(index)).freed());
        }
        // One file below every member: each enters the loop at another place.
        for index in 0..members {
            nodes.push(Node::file(24 + members + index, at(24 + index), "f").freed());
        }
        let mft = volume_of(&nodes);
        for index in 0..members {
            assert_eq!(
                resolve(&mft, 24 + members + index),
                found(&["<lost 24>", "f"], false),
                "{members} members, entry {index}"
            );
        }
    }
}

// ---- Bounded work: a small hostile volume must not make the walk quadratic ----
//
// Work is counted in records visited (`walk_budget`), not timed. Each test gives the whole scan
// a budget of a few steps per directory: a scan sharing one cache visits every directory about
// once, whatever the tree's shape.

use crate::path::walk_budget;

/// Resolves every file number from `files`, in order, with one cache, under a budget of `budget`
/// steps. Returns the answers.
fn scan_with_budget(mft: &Mft, files: impl Iterator<Item = u64>, budget: u64) -> Vec<DeletedPath> {
    let mut cache = DeletedPathCache::new();
    let (paths, steps) = walk_budget::run(budget, || {
        files
            .map(|number| mft.resolve_deleted_path(&name_of_record(mft, number), &mut cache))
            .collect::<Vec<_>>()
    });
    eprintln!("{} files, {steps} steps", paths.len());
    paths
}

// Unnamed directories add no units, so the length limit never stops a walk through them: a loop
// of 20k is found by the loop detector and remembered for every member, so the 20k files each
// entering at a different member cost one step apiece.
#[test]
fn a_loop_of_unnamed_directories_is_walked_once_for_a_whole_scan() {
    const LOOP: u64 = 20_000;
    let mut nodes = Vec::new();
    for index in 0..LOOP {
        nodes.push(Node::dir(24 + index, at(24 + (index + 1) % LOOP), "").freed());
    }
    for index in 0..LOOP {
        nodes.push(Node::file(24 + LOOP + index, at(24 + index), "f").freed());
    }
    let mft = volume_of(&nodes);

    let paths = scan_with_budget(&mft, 24 + LOOP..24 + 2 * LOOP, 8 * LOOP);
    assert!(paths
        .iter()
        .all(|path| *path == found(&["<lost 24>", "f"], false)));
}

// A chain deeper than the limit: the deepest files are asked first, so each finds its path too
// long. The answer is remembered for the directories, not rediscovered per file.
#[test]
fn a_chain_past_the_limit_is_walked_once_for_a_whole_scan() {
    // 200-unit names: each level costs 201 units, and the 32767 limit falls around level 163, so
    // 400 levels cover both fitting and too-long paths. One-unit names needed 16,000 levels, and
    // building their paths dominated `mise run test-linux`.
    const DEPTH: u64 = 400;
    const NAME_UNITS: usize = 200;
    let name = "d".repeat(NAME_UNITS);
    let mut nodes = Vec::new();
    for level in 0..DEPTH {
        let parent = if level == 0 { root() } else { at(23 + level) };
        nodes.push(Node::dir(24 + level, parent, &name).freed());
    }
    for level in 0..DEPTH {
        nodes.push(Node::file(24 + DEPTH + level, at(24 + level), "f").freed());
    }
    let mft = volume_of(&nodes);

    let paths = scan_with_budget(&mft, (24 + DEPTH..24 + 2 * DEPTH).rev(), 6 * DEPTH);
    // Path length: volume path, then a separator and name per directory (level + 1 of them), then
    // a separator and the file's one unit.
    let mut fitting = 0;
    for (level, path) in paths.iter().rev().enumerate() {
        let fits = VOLUME_PATH.len() + (NAME_UNITS + 1) * (level + 1) + 2 <= 32767;
        fitting += usize::from(fits);
        assert_eq!(path.complete, fits, "level {level}");
        assert_eq!(
            path.path.starts_with(under_volume(&["<too long>"])),
            !fits,
            "level {level}"
        );
    }
    // Both sides of the limit were reached.
    assert!(
        fitting > 100 && fitting < paths.len() - 100,
        "{fitting} of {}",
        paths.len()
    );
}

// Same through `FileInfo`: a scan of `deleted_files()` shares one cache for the live walk and one
// for the deleted walk, visiting each directory about once. A cache per file would revisit the
// whole chain each time.
#[test]
fn a_scan_of_file_infos_shares_the_deleted_path_cache() {
    const DEPTH: u64 = 3_000;
    let mut nodes = Vec::new();
    for level in 0..DEPTH {
        let parent = if level == 0 { root() } else { at(23 + level) };
        nodes.push(Node::dir(24 + level, parent, "d").freed());
    }
    for level in 0..DEPTH {
        nodes.push(Node::file(24 + DEPTH + level, at(24 + level), "f").freed());
    }
    let mft = volume_of(&nodes);

    let mut live = DefaultPathCache::new();
    let mut deleted = DeletedPathCache::new();
    let (paths, steps) = walk_budget::run(6 * DEPTH, || {
        mft.deleted_files()
            .filter(|file| !file.is_directory())
            .map(|file| FileInfo::with_caches(&file, &mut live, &mut deleted).path)
            .collect::<Vec<_>>()
    });
    eprintln!("{} files, {steps} steps", paths.len());
    assert_eq!(paths.len(), DEPTH as usize);
    assert!(paths.iter().all(Option::is_some));
    assert!(
        live.is_empty(),
        "a deleted file put something in the live cache"
    );
}

// A parent must be a directory: a file's record named as a parent (a stale or corrupt reference)
// breaks the chain like any other unidentifiable parent.
#[test]
fn a_parent_that_is_not_a_directory_is_lost() {
    let mft = volume_of(&[
        Node::file(24, root(), "not-a-directory").freed(),
        Node::file(25, at(24), "leaf.txt").freed(),
        Node::file(26, root(), "live-not-a-directory"),
        Node::file(27, at(26), "leaf2.txt").freed(),
    ]);
    assert_eq!(resolve(&mft, 25), found(&["<lost 24>", "leaf.txt"], false));
    assert_eq!(resolve(&mft, 27), found(&["<lost 26>", "leaf2.txt"], false));
}

// An unnamed directory adds no units to a path, but its separator does: a chain of 40k unnamed
// live directories ends the walk once separators alone exceed the limit, around 32.8k steps
// rather than 40k, with the same answer as without the shortcut.
#[test]
fn a_chain_of_unnamed_live_directories_is_cut_by_its_separators() {
    const DEPTH: u64 = 40_000;
    let mut nodes = Vec::new();
    for level in 0..DEPTH {
        let parent = if level == 0 { root() } else { at(23 + level) };
        nodes.push(Node::dir(24 + level, parent, ""));
    }
    nodes.push(Node::file(24 + DEPTH, at(23 + DEPTH), "f"));
    let mft = volume_of(&nodes);

    let (path, steps) = walk_budget::run(34_000, || {
        mft.resolve_path(&name_of_record(&mft, 24 + DEPTH), &mut ())
    });
    assert_eq!(path, None);
    assert!(steps > 32_000, "{steps} steps: it stopped too early");
}
