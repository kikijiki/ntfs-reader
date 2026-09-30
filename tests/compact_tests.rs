#![cfg(target_os = "windows")]

//! Compact/whole public-file parity on the quiet test volume and the parity volume.
//! `NTFS_READER_PARITY_POLICY=quiet` (the default) requires exact parity on a stable volume;
//! `live` permits 5% differences in live files on a changing volume and reports deleted
//! differences without gating them. Exact deleted parity requires a quiet volume or shadow.
//! Compare rich snapshots one file at a time so the million-file x86 stress test fits in memory.

use ntfs_reader::{Mft, NtfsReaderResult, Volume};

mod common;
use common::{
    assert_parity, compare_mfts, flush_volume, parity_policy, parity_volume_letter,
    test_volume_letter, ComparisonPolicy,
};

#[test]
fn a_compact_mft_equals_the_whole_mft_file_by_file() -> NtfsReaderResult<()> {
    let letter = test_volume_letter();
    flush_volume(&letter);
    let volume_path = format!("\\\\.\\{letter}:");

    let whole = Mft::new(Volume::new(&volume_path)?)?;
    assert!(
        whole.files().next().is_some(),
        "nothing to compare: {volume_path} looks empty of live files"
    );

    let compact = Mft::new_compact(Volume::new(&volume_path)?)?;
    assert_parity(
        ComparisonPolicy::Quiet,
        &format!("compact {volume_path}"),
        compare_mfts(&whole, &compact),
    );
    assert!(
        compact.size_in_memory() <= whole.size_in_memory(),
        "a compact store ({} bytes) must not be bigger than the whole one ({} bytes) on \
         {volume_path}",
        compact.size_in_memory(),
        whole.size_in_memory()
    );
    assert_eq!(
        compact.corrupt_records(),
        whole.corrupt_records(),
        "corrupt_records() diverged between Mft::new_compact and Mft::new on {volume_path}"
    );
    Ok(())
}

/// The parity-volume comparison uses `NTFS_READER_PARITY_POLICY` to select quiet/live policy.
///
/// ```text
/// set NTFS_READER_PARITY_POLICY=live
/// set NTFS_READER_PARITY_VOLUME=C
/// set NTFS_READER_ALLOW_SYSTEM_DRIVE=1
/// cargo test --features internals --test compact_tests -- --ignored --nocapture
/// ```
#[test]
#[ignore = "reads a whole live volume; needs NTFS_READER_PARITY_VOLUME (see the file's docs)"]
fn a_compact_mft_equals_the_whole_mft_on_a_live_volume() -> NtfsReaderResult<()> {
    let letter = parity_volume_letter();
    let volume_path = format!("\\\\.\\{letter}:");

    let whole = Mft::new(Volume::new(&volume_path)?)?;
    assert!(
        whole.files().next().is_some(),
        "nothing to compare: {volume_path} looks empty of live files"
    );

    let compact = Mft::new_compact(Volume::new(&volume_path)?)?;
    let policy = parity_policy();
    assert_parity(
        policy,
        &format!("compact {volume_path}"),
        compare_mfts(&whole, &compact),
    );
    if policy == ComparisonPolicy::Quiet {
        assert_eq!(
            compact.corrupt_records(),
            whole.corrupt_records(),
            "quiet compact corrupt_records"
        );
    }
    assert!(
        compact.size_in_memory() <= whole.size_in_memory(),
        "a compact store ({} bytes) must not be bigger than the whole one ({} bytes) on \
         {volume_path}",
        compact.size_in_memory(),
        whole.size_in_memory()
    );
    Ok(())
}
