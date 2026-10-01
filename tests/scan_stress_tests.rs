#![cfg(target_os = "windows")]

//! Read-only scan parity on `NTFS_READER_PARITY_VOLUME`. Set
//! `NTFS_READER_PARITY_POLICY=quiet` for stable volumes and `live` for changing volumes.
//! Quiet is the default and requires zero differences for live and deleted files. Live permits
//! up to 5% differences in live files and reports deleted differences without gating them:
//! tiny chunks can take minutes while freed records are reused. Frozen shadows remain exact.
//! Rich snapshots are compared one file at a time so the million-file x86 comparison fits in
//! memory.

use ntfs_reader::{DefaultPathCache, DeletedPathCache, Mft, MftScan, Volume};

mod common;
use common::{
    assert_parity, compare_mfts, compare_scan, parity_policy, parity_shadow_device_path,
    parity_stride, parity_volume_letter, scan_chunk_sizes, skip, ComparisonPolicy, FileSnapshot,
};

fn load_mft(volume_path: &str) -> Mft {
    Mft::new(Volume::new(volume_path).unwrap_or_else(|e| panic!("open {volume_path}: {e}")))
        .unwrap_or_else(|e| panic!("Mft::new({volume_path}): {e}"))
}

fn assert_scan_matches(mft: &Mft, volume_path: &str, chunk_records: u64, policy: ComparisonPolicy) {
    let mut scan = MftScan::with_chunk_records(
        Volume::new(volume_path).unwrap_or_else(|e| panic!("open {volume_path}: {e}")),
        chunk_records,
    )
    .unwrap_or_else(|e| panic!("scan {volume_path}, chunks of {chunk_records}: {e}"));
    assert_parity(
        policy,
        &format!("scan {volume_path}, chunk_records={chunk_records}"),
        compare_scan(mft, &mut scan),
    );
    if policy == ComparisonPolicy::Quiet {
        assert_eq!(
            scan.corrupt_records(),
            mft.corrupt_records(),
            "quiet scan corrupt_records"
        );
    }
}

#[test]
#[ignore = "reads a whole volume; needs NTFS_READER_PARITY_VOLUME (see the file's docs)"]
fn scan_matches_mft_at_every_chunk_size_on_the_parity_volume() {
    let letter = parity_volume_letter();
    let volume_path = format!("\\\\.\\{letter}:");
    let mft = load_mft(&volume_path);
    assert!(mft.record_count() > 0, "{volume_path}'s $MFT looks empty");
    for chunk_records in scan_chunk_sizes(mft.record_count()) {
        assert_scan_matches(&mft, &volume_path, chunk_records, parity_policy());
    }
}

/// `DefaultPathCache::with_max_bytes` gives the same answer as an unbounded cache and as `()` on
/// the parity volume, as `scan_edge_tests.rs` checks on the disposable test volume. Two bounds
/// (0 and a small one) and `NTFS_READER_PARITY_STRIDE` sampling keep a million-file comparison
/// to minutes rather than hours.
#[test]
#[ignore = "reads a whole volume; needs NTFS_READER_PARITY_VOLUME (see the file's docs)"]
fn bounded_path_cache_matches_unbounded_and_none_on_the_parity_volume() {
    let letter = parity_volume_letter();
    let volume_path = format!("\\\\.\\{letter}:");
    let mft = load_mft(&volume_path);
    let stride = parity_stride();
    assert!(
        mft.files().next().is_some(),
        "nothing to compare: {volume_path} looks empty of live files"
    );

    for max_bytes in [0usize, 65536] {
        let mut bounded = DefaultPathCache::with_max_bytes(max_bytes);
        let mut unbounded = DefaultPathCache::new();
        let mut bounded_deleted = DeletedPathCache::new();
        let mut unbounded_deleted = DeletedPathCache::new();
        for file in mft
            .files()
            .step_by(stride as usize)
            .chain(mft.deleted_files().step_by(stride as usize))
        {
            assert_eq!(
                FileSnapshot::new(&file, &mft, &mut bounded, &mut bounded_deleted),
                FileSnapshot::new(&file, &mft, &mut unbounded, &mut unbounded_deleted),
                "max_bytes={max_bytes}: public-file snapshot changed for {}",
                file.number()
            );
            for name in file.names() {
                let via_bounded = mft.resolve_path(&name, &mut bounded);
                let via_unbounded = mft.resolve_path(&name, &mut unbounded);
                let via_none = mft.resolve_path(&name, &mut ());
                assert_eq!(
                    via_bounded,
                    via_unbounded,
                    "max_bytes={max_bytes}: a bounded cache disagreed with an unbounded one for \
                     file {} on {volume_path}",
                    file.number()
                );
                assert_eq!(
                    via_bounded,
                    via_none,
                    "max_bytes={max_bytes}: a bounded cache disagreed with no cache for file {} \
                     on {volume_path}",
                    file.number()
                );
            }
        }
        assert!(
            bounded.bytes() <= max_bytes || bounded.len() <= 1,
            "max_bytes={max_bytes}: bytes()={} must not exceed the limit while more than one \
             entry is cached (len()={}) on {volume_path}",
            bounded.bytes(),
            bounded.len()
        );
    }
}

/// A shadow named by `NTFS_READER_PARITY_SHADOW` must give exact scan/whole and compact/whole
/// parity. This test reports a skip when no shadow device path is supplied.
#[test]
#[ignore = "reads a whole volume; needs NTFS_READER_PARITY_SHADOW (see the file's docs)"]
fn mft_scan_equals_mft_on_a_shadow_of_the_parity_volume() {
    let Some(device) = parity_shadow_device_path() else {
        skip(
            "mft_scan_equals_mft_on_a_shadow_of_the_parity_volume",
            "set NTFS_READER_PARITY_SHADOW to the device path of a shadow copy of the parity \
             volume",
        );
        return;
    };
    let mft = load_mft(&device);
    assert!(mft.record_count() > 0, "{device}'s $MFT looks empty");
    for chunk_records in scan_chunk_sizes(mft.record_count()) {
        assert_scan_matches(&mft, &device, chunk_records, ComparisonPolicy::Quiet);
    }

    // A frozen shadow also requires exact compact/whole parity.
    let compact =
        Mft::new_compact(Volume::new(&device).unwrap_or_else(|e| panic!("open {device}: {e}")))
            .unwrap_or_else(|e| panic!("Mft::new_compact({device}): {e}"));
    assert_parity(
        ComparisonPolicy::Quiet,
        &format!("compact shadow {device}"),
        compare_mfts(&mft, &compact),
    );
}
