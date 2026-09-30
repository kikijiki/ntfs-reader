#![cfg(target_os = "windows")]

//! `MftScan` against a whole `Mft` of the same volume. Live and deleted files should carry the
//! same public-file snapshots either way.
//!
//! `NTFS_READER_TEST_VOLUME` is quiet while this runs (Defender excluded, indexing off, tests
//! run one at a time), so [`a_scan_equals_the_whole_mft_file_by_file`] requires an exact match:
//! `MftScan` reads `$MFT` twice, but nothing on that volume writes to it in between, so a
//! difference there is a real bug, not the volume changing. A volume the tests do not control
//! (live, or under other load) is a different case with its own test,
//! [`a_scan_equals_the_whole_mft_on_a_live_volume`], `#[ignore]`d like `win32_parity_tests`'s:
//! there the two reads genuinely can disagree, and the comparison tolerates it.

use ntfs_reader::{Mft, MftScan, NtfsReaderResult, Volume};

mod common;
use common::{
    assert_parity, compare_scan, flush_volume, parity_policy, parity_volume_letter,
    test_volume_letter, ComparisonPolicy,
};

/// `NTFS_READER_TEST_VOLUME` is quiet for the whole test run (see the file's docs), so the two
/// reads `MftScan` and `Mft` do of it cannot disagree: any difference here is a bug in `MftScan`,
/// not the volume changing between the reads.
#[test]
fn a_scan_equals_the_whole_mft_file_by_file() -> NtfsReaderResult<()> {
    let letter = test_volume_letter();
    flush_volume(&letter);
    let volume_path = format!("\\\\.\\{letter}:");

    let mft = Mft::new(Volume::new(&volume_path)?)?;
    assert!(
        mft.files().next().is_some(),
        "nothing to compare: {volume_path} looks empty of live files"
    );

    let mut scan = MftScan::new(Volume::new(&volume_path)?)?;
    assert_parity(
        ComparisonPolicy::Quiet,
        &format!("scan {volume_path}"),
        compare_scan(&mft, &mut scan),
    );
    assert_eq!(
        scan.corrupt_records(),
        mft.corrupt_records(),
        "quiet scan corrupt_records"
    );
    Ok(())
}

/// As above, but on `NTFS_READER_PARITY_VOLUME`, a volume this test suite does not control (live,
/// or under load): the two reads can genuinely disagree (a file created, deleted or renamed
/// between them). With `NTFS_READER_PARITY_POLICY=live`, this tolerates up to 5% differences
/// in live files. Deleted differences are reported without gating them because freed records
/// can be reused between reads. Quiet (the default) requires zero differences in both populations.
///
/// ```text
/// set NTFS_READER_PARITY_POLICY=live
/// set NTFS_READER_PARITY_VOLUME=C
/// set NTFS_READER_ALLOW_SYSTEM_DRIVE=1
/// cargo test --features internals --test scan_tests -- --ignored --nocapture
/// ```
#[test]
#[ignore = "reads a whole live volume; needs NTFS_READER_PARITY_VOLUME (see the file's docs)"]
fn a_scan_equals_the_whole_mft_on_a_live_volume() -> NtfsReaderResult<()> {
    let letter = parity_volume_letter();
    let volume_path = format!("\\\\.\\{letter}:");

    let mft = Mft::new(Volume::new(&volume_path)?)?;
    assert!(
        mft.files().next().is_some(),
        "{volume_path} has no live files"
    );
    let mut scan = MftScan::new(Volume::new(&volume_path)?)?;
    assert_parity(
        parity_policy(),
        &format!("scan {volume_path}"),
        compare_scan(&mft, &mut scan),
    );
    Ok(())
}
