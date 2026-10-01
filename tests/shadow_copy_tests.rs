#![cfg(target_os = "windows")]

//! Reading an NTFS volume through a Volume Shadow Copy, a read-only, point-in-time device
//! (see `docs/shadow-copies.md`). Create the fixtures described below on a disposable test
//! volume, then take a shadow copy and set `NTFS_READER_TEST_SHADOW` to its device path, for
//! example `\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopy4`.
//!
//! The crate reads existing shadow copies; it does not create them. These tests report a skip
//! when `NTFS_READER_TEST_SHADOW` is unset.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::{Read, Seek, SeekFrom};

use ntfs_reader::{
    AllocationState, ClusterBitmap, DefaultPathCache, ExtentLocation, FileInfo, Mft, MftScan,
    NtfsFile, Volume,
};

mod common;
use common::{
    assert_parity, compare_mfts, compare_scan, scan_chunk_sizes, shadow_device_path,
    ComparisonPolicy,
};

const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
const FILE_ATTRIBUTE_SYSTEM: u32 = 0x4;
const FILE_ATTRIBUTE_ARCHIVE: u32 = 0x20;

/// Opens the shadow named by `NTFS_READER_TEST_SHADOW` and loads its `$MFT`, or logs a skip and
/// returns `None`.
fn load_shadow(test: &str) -> Option<Mft> {
    let Some(device) = shadow_device_path() else {
        common::skip(
            test,
            "set NTFS_READER_TEST_SHADOW to the device path of a shadow copy of the test \
             volume taken after creating the required fixtures",
        );
        return None;
    };
    let volume =
        Volume::new(&device).unwrap_or_else(|e| panic!("Volume::new({device}) (shadow): {e}"));
    Some(Mft::new(volume).unwrap_or_else(|e| panic!("Mft::new({device}) (shadow): {e}")))
}

/// Every name of every live file, resolved to its path relative to the shadow's device path
/// (`fixtures\dos\A long file name.txt`, not `fixtures`'s own record but every name it has),
/// mapped to the record number the name belongs to. A hard link's other names, and any name
/// `resolve_path` cannot place, are simply absent.
fn scan_names(mft: &Mft) -> HashMap<String, u64> {
    let mut cache = DefaultPathCache::new();
    let mut names = HashMap::new();
    let device = mft.volume().path();
    for file in mft.files() {
        for name in file.names() {
            let Some(path) = mft.resolve_path(&name, &mut cache) else {
                continue;
            };
            if let Ok(relative) = path.strip_prefix(device) {
                names.insert(relative.to_string_lossy().into_owned(), file.number());
            }
        }
    }
    names
}

/// The record `key` (a path relative to the shadow's device path, as [`scan_names`] keys it)
/// resolves to, or a panic naming what was and was not found.
fn find<'m>(mft: &'m Mft, names: &HashMap<String, u64>, key: &str) -> NtfsFile<'m> {
    let &number = names.get(key).unwrap_or_else(|| {
        panic!(
            "{key:?} was not found scanning the shadow ({} names resolved)",
            names.len()
        )
    });
    mft.record(number)
        .unwrap_or_else(|| panic!("record {number} ({key:?}) is missing from the shadow's $MFT"))
}

/// Reads a stream of `file` fully, through the shadow.
fn read_stream(file: &NtfsFile, name: Option<&str>) -> Vec<u8> {
    let os_name = name.map(OsStr::new);
    let mut stream = file
        .open_stream(os_name)
        .unwrap_or_else(|e| panic!("open_stream({name:?}) on the shadow: {e}"));
    let mut buf = Vec::new();
    stream
        .read_to_end(&mut buf)
        .unwrap_or_else(|e| panic!("reading stream {name:?} through the shadow: {e}"));
    buf
}

// The shadow must contain the fixture names, attributes and stream contents checked below.
// Take the shadow copy only after creating all fixtures on the test volume.
#[test]
fn mft_reads_a_shadow_copy_and_matches_the_known_fixtures() {
    let Some(mft) = load_shadow("mft_reads_a_shadow_copy_and_matches_the_known_fixtures") else {
        return;
    };

    assert!(mft.record_count() > 0, "shadow $MFT has no records");
    assert!(
        mft.is_allocated(0),
        "record 0 is not allocated on the shadow"
    );
    assert!(mft.record(0).is_some());

    let names = scan_names(&mft);
    let device = mft.volume().path().to_path_buf();

    // A resolved path on the shadow starts with the shadow's own device path, not the live
    // volume's: the documented caveat (docs/shadow-copies.md).
    let small = find(&mft, &names, "fixtures\\small.txt");
    let mut cache = DefaultPathCache::new();
    let small_name = small.best_name().expect("fixtures\\small.txt has no name");
    let resolved = mft
        .resolve_path(&small_name, &mut cache)
        .expect("fixtures\\small.txt did not resolve");
    assert!(
        resolved.starts_with(&device),
        "{resolved:?} does not start with the shadow's device path {device:?}"
    );
    assert_eq!(FileInfo::new(&small).size, 4);
    assert_eq!(read_stream(&small, None), b"tiny");

    // A plain hard link pair: two names, one record. `hard_links()` excludes DOS aliases.
    let dos = find(&mft, &names, "fixtures\\dos\\A long file name.txt");
    assert!(names.contains_key("fixtures\\dos\\Another long link name.txt"));
    assert_eq!(dos.hard_links().count(), 2);
    assert_eq!(read_stream(&dos, None), b"dos");

    // 300 names on one file (target.txt plus 299 links spread across links\a, links\b, links\c),
    // enough to spill into an extension record.
    let target = find(&mft, &names, "fixtures\\links\\target.txt");
    assert_eq!(target.hard_links().count(), 300);
    assert_eq!(read_stream(&target, None), b"links");

    // many-streams.txt: default stream "main" plus 60 named streams of 200 bytes each.
    let many = find(&mft, &names, "fixtures\\many-streams.txt");
    assert_eq!(read_stream(&many, None), b"main");
    let named_streams = many.data_streams().filter(|s| s.name.is_some()).count();
    assert_eq!(named_streams, 60);
    let expected_stream_value = "x".repeat(200);
    for i in 0..60 {
        let name = format!("stream{i:02}");
        assert_eq!(
            read_stream(&many, Some(&name)),
            expected_stream_value.as_bytes(),
            "stream {name} on the shadow"
        );
    }

    // A directory can carry a named stream of its own. This fixture's stream includes a CRLF
    // line ending, unlike the other small fixture streams.
    let fixtures_dir = find(&mft, &names, "fixtures");
    assert!(fixtures_dir.is_directory());
    assert_eq!(
        read_stream(&fixtures_dir, Some("dirstream")),
        b"directory stream\r\n"
    );

    // The fragmented fixture: 32 GiB, 201 names, hidden+system+archive.
    let big = find(&mft, &names, "large-fragmented.rar");
    let big_info = FileInfo::new(&big);
    assert_eq!(big_info.size, 32u64 * 1024 * 1024 * 1024);
    assert_eq!(big.hard_links().count(), 201);
    let expected_attributes =
        FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM | FILE_ATTRIBUTE_ARCHIVE;
    assert_eq!(
        big_info.file_attributes & expected_attributes,
        expected_attributes,
        "large-fragmented.rar attributes on the shadow: {:#x}",
        big_info.file_attributes
    );
}

#[test]
fn open_stream_reads_a_fragmented_file_through_a_shadow_copy() {
    let Some(mft) = load_shadow("open_stream_reads_a_fragmented_file_through_a_shadow_copy") else {
        return;
    };
    let names = scan_names(&mft);
    let big = find(&mft, &names, "large-fragmented.rar");

    let stream = big
        .open_stream(None)
        .unwrap_or_else(|e| panic!("open_stream on large-fragmented.rar (shadow): {e}"));
    assert_eq!(stream.size(), 32u64 * 1024 * 1024 * 1024);

    // Written 4 KiB at a time every 8 MiB, so NTFS's 64 KiB sparse allocation unit gives about
    // 4096 small, scattered extents and about 256 MiB actually stored.
    let stored_extents: Vec<_> = stream
        .extents()
        .iter()
        .filter(|extent| matches!(extent.location, ExtentLocation::Volume { .. }))
        .collect();
    assert!(
        stored_extents.len() > 1000,
        "expected a heavily fragmented stream, got {} stored extents",
        stored_extents.len()
    );
    let stored_bytes: u64 = stored_extents.iter().map(|extent| extent.length).sum();
    let mib: u64 = 1024 * 1024;
    assert!(
        (200 * mib..300 * mib).contains(&stored_bytes),
        "stored bytes {stored_bytes} outside the ~256 MiB the fixture writes"
    );

    // Reading through the middle of the file must reach the volume's clusters via the shadow
    // device, not just the resident/sparse parts near the start.
    let mut reader = big
        .open_stream(None)
        .unwrap_or_else(|e| panic!("open_stream on large-fragmented.rar (shadow): {e}"));
    reader
        .seek(SeekFrom::Start(16 * 1024 * mib))
        .unwrap_or_else(|e| panic!("seek through the shadow: {e}"));
    let mut buf = [0u8; 4096];
    reader
        .read_exact(&mut buf)
        .unwrap_or_else(|e| panic!("read at 16 GiB through the shadow: {e}"));
}

/// `MftScan` equals `Mft` on the shadow at every chunk size, and compact loading agrees too.
/// A shadow is a read-only, point-in-time snapshot, so all passes read the same bytes.
#[test]
fn mft_scan_equals_mft_on_a_shadow_copy() {
    let Some(mft) = load_shadow("mft_scan_equals_mft_on_a_shadow_copy") else {
        return;
    };
    assert!(mft.record_count() > 0, "shadow $MFT has no records");
    let device = mft.volume().path().to_string_lossy().into_owned();

    for chunk_records in scan_chunk_sizes(mft.record_count()) {
        let mut scan = MftScan::with_chunk_records(
            Volume::new(&device).unwrap_or_else(|e| panic!("Volume::new({device}) (shadow): {e}")),
            chunk_records,
        )
        .unwrap_or_else(|e| panic!("MftScan::with_chunk_records({chunk_records}) (shadow): {e}"));
        assert_parity(
            ComparisonPolicy::Quiet,
            &format!("scan shadow, chunk_records={chunk_records}"),
            compare_scan(&mft, &mut scan),
        );
        assert_eq!(
            scan.corrupt_records(),
            mft.corrupt_records(),
            "shadow scan corrupt_records"
        );
    }
    let compact = Mft::new_compact(Volume::new(&device).expect("open shadow"))
        .expect("compact Mft on shadow");
    assert_parity(
        ComparisonPolicy::Quiet,
        "compact shadow",
        compare_mfts(&mft, &compact),
    );
    assert_eq!(
        compact.corrupt_records(),
        mft.corrupt_records(),
        "shadow corrupt_records"
    );
}

#[test]
fn cluster_bitmap_reads_through_a_shadow_copy() {
    let Some(mft) = load_shadow("cluster_bitmap_reads_through_a_shadow_copy") else {
        return;
    };

    let bitmap =
        ClusterBitmap::new(&mft).unwrap_or_else(|e| panic!("ClusterBitmap::new on shadow: {e}"));
    assert_eq!(bitmap.cluster_size(), mft.volume().cluster_size());
    assert!(bitmap.cluster_count() > 0);

    let names = scan_names(&mft);
    let big = find(&mft, &names, "large-fragmented.rar");
    let stream = big
        .open_stream(None)
        .unwrap_or_else(|e| panic!("open_stream on large-fragmented.rar (shadow): {e}"));
    let allocation = stream
        .allocation(&bitmap)
        .unwrap_or_else(|e| panic!("allocation() through the shadow: {e}"));
    assert_ne!(
        allocation.state(),
        AllocationState::Incomplete,
        "the shadow's $Bitmap does not cover every cluster the stream's extents point to"
    );
}
