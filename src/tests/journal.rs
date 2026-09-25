// Copyright (c) 2022, Matteo Bernacchia <dev@kikijiki.com>. All rights reserved.
// This project is dual licensed under the Apache License 2.0 and the MIT license.
// See the LICENSE files in the project root for details.

use super::*;
use windows::Win32::Foundation::ERROR_ACCESS_DENIED;

/// A `Volume` good enough for tests that only look at `Journal::new`'s
/// path handling or at pure `Journal` methods that never touch the
/// volume handle.
fn fake_volume() -> Volume {
    Volume::synthetic(PathBuf::from("\\\\?\\T:"), 4096, 0, 1024, 0)
}

/// Build a `Journal` for testing pure, in-memory methods (`match_rename`) without opening a
/// real volume or journal. The null placeholder handle is safe to drop for real: `Owned`
/// only calls `CloseHandle` when the handle is not invalid, and a null `HANDLE` is invalid.
fn fake_journal(history: VecDeque<UsnRecord>) -> Journal {
    Journal {
        volume: fake_volume(),
        volume_handle: unsafe {
            windows::core::Owned::new(Foundation::HANDLE(std::ptr::null_mut()))
        },
        journal: Ioctl::USN_JOURNAL_DATA_V2::default(),
        next_usn: 0,
        reason_mask: Reason::EMPTY,
        history,
        max_history_size: None,
    }
}

fn usn_record(usn: i64, file_id: FileId, reason: Reason, name: &str) -> UsnRecord {
    UsnRecord {
        usn,
        timestamp: time::OffsetDateTime::UNIX_EPOCH,
        file_id,
        parent_id: FileId::from(0u64),
        reason,
        file_attributes: 0,
        name: name.into(),
    }
}

#[test]
fn match_rename_returns_the_most_recent_old_name_not_the_oldest() {
    let file_id = FileId::from(42u64);
    let mut history = VecDeque::new();
    // A -> B -> C: both renames' old names are in history by the time we
    // see C's RENAME_NEW_NAME record.
    history.push_back(usn_record(1, file_id, Reason::RENAME_OLD_NAME, "A"));
    history.push_back(usn_record(5, file_id, Reason::RENAME_OLD_NAME, "B"));

    let journal = fake_journal(history);
    let new_name_record = usn_record(10, file_id, Reason::RENAME_NEW_NAME, "C");

    assert_eq!(
        journal.match_rename(&new_name_record),
        Some(OsString::from("B")),
        "match_rename returned the oldest matching history entry (A) instead of the \
             most recent rename-old-name entry (B) before the second rename"
    );
}

#[test]
fn match_rename_ignores_a_record_that_is_not_a_new_name() {
    let file_id = FileId::from(7u64);
    let mut history = VecDeque::new();
    history.push_back(usn_record(1, file_id, Reason::RENAME_OLD_NAME, "old"));
    let journal = fake_journal(history);

    let create = usn_record(10, file_id, Reason::FILE_CREATE, "new");

    assert_eq!(journal.match_rename(&create), None);
}

// --- history keeps only what match_rename reads ---

#[test]
fn history_keeps_only_rename_old_name_records() {
    let mut history = VecDeque::new();
    let file_id = FileId::from(1u64);
    let old = usn_record(1, file_id, Reason::RENAME_OLD_NAME, "old");
    let link = usn_record(2, file_id, Reason::HARD_LINK_CHANGE, "link");
    let reparse = usn_record(3, file_id, Reason::REPARSE_POINT_CHANGE, "reparse");
    let new = usn_record(4, file_id, Reason::RENAME_NEW_NAME, "new");

    for record in [&old, &link, &reparse, &new] {
        push_history(&mut history, None, record);
    }

    let names: Vec<_> = history.iter().map(|r| r.name.clone()).collect();
    assert_eq!(names, vec![OsString::from("old")]);
}

#[test]
fn a_reparse_point_burst_does_not_push_a_rename_out_of_a_bounded_history() {
    let mut history = VecDeque::new();
    let file_id = FileId::from(1u64);
    push_history(
        &mut history,
        Some(4),
        &usn_record(1, file_id, Reason::RENAME_OLD_NAME, "old"),
    );
    for usn in 2..50 {
        let noise = usn_record(usn, FileId::from(9u64), Reason::REPARSE_POINT_CHANGE, "x");
        push_history(&mut history, Some(4), &noise);
    }
    let journal = fake_journal(history);

    let new = usn_record(100, file_id, Reason::RENAME_NEW_NAME, "new");

    assert_eq!(journal.match_rename(&new), Some(OsString::from("old")));
}

#[test]
fn trim_history_some_keeps_the_entry_at_the_usn_and_drops_older_ones() {
    let file_id = FileId::from(1u64);
    let mut history = VecDeque::new();
    for usn in [10, 20, 30] {
        history.push_back(usn_record(usn, file_id, Reason::RENAME_OLD_NAME, "n"));
    }
    let mut journal = fake_journal(history);

    journal.trim_history(Some(20));

    let usns: Vec<_> = journal.history.iter().map(|r| r.usn).collect();
    assert_eq!(usns, vec![20, 30]);
}

#[test]
fn trim_history_none_clears_everything() {
    let mut history = VecDeque::new();
    history.push_back(usn_record(
        10,
        FileId::from(1u64),
        Reason::RENAME_OLD_NAME,
        "n",
    ));
    let mut journal = fake_journal(history);

    journal.trim_history(None);

    assert!(journal.history.is_empty());
}

// `Journal::resolve_path` and `get_file_path` copy a `FILE_NAME_INFO` out of a buffer; the
// decode is a pure function over bytes (no `align_to`, bounds-checked), testable here
// without a real file handle.
#[test]
fn parse_file_name_info_rejects_a_file_name_length_that_exceeds_the_buffer() {
    // FileNameLength claims 1000 bytes; the buffer only has 4 bytes of name data after the
    // 4-byte header. It must be rejected, not read out of bounds.
    let mut buffer = vec![0u8; 8];
    buffer[0..4].copy_from_slice(&1000u32.to_le_bytes());

    assert_eq!(parse_file_name_info(&buffer), None);
}

#[test]
fn parse_file_name_info_rejects_a_buffer_shorter_than_its_length_field() {
    assert_eq!(parse_file_name_info(&[1, 0]), None);
}

#[test]
fn parse_file_name_info_decodes_a_well_formed_buffer() {
    let name: Vec<u16> = "child.txt".encode_utf16().collect();
    let mut buffer = ((name.len() as u32) * 2).to_le_bytes().to_vec();
    for unit in &name {
        buffer.extend_from_slice(&unit.to_le_bytes());
    }

    assert_eq!(
        parse_file_name_info(&buffer),
        Some(PathBuf::from("child.txt"))
    );
}

// --- errors ---

#[test]
fn access_denied_from_a_windows_call_is_access_denied() {
    let err = windows::core::Error::from(ERROR_ACCESS_DENIED.to_hresult());

    assert!(matches!(
        map_windows_error(err),
        NtfsReaderError::AccessDenied
    ));
}

#[test]
fn a_journal_specific_windows_error_gets_its_own_variant() {
    let not_active = windows::core::Error::from(ERROR_JOURNAL_NOT_ACTIVE.to_hresult());
    let deleted = windows::core::Error::from(ERROR_JOURNAL_ENTRY_DELETED.to_hresult());
    let being_deleted = windows::core::Error::from(ERROR_JOURNAL_DELETE_IN_PROGRESS.to_hresult());

    assert!(matches!(
        map_windows_error(not_active),
        NtfsReaderError::JournalNotActive
    ));
    assert!(matches!(
        map_windows_error(deleted),
        NtfsReaderError::JournalEntryDeleted
    ));
    assert!(matches!(
        map_windows_error(being_deleted),
        NtfsReaderError::JournalDeleteInProgress
    ));
}

#[test]
fn any_other_windows_error_is_io_with_the_os_error_code() {
    let err = windows::core::Error::from(Foundation::ERROR_FILE_NOT_FOUND.to_hresult());

    match map_windows_error(err) {
        NtfsReaderError::Io(io) => {
            assert_eq!(
                io.raw_os_error(),
                Some(Foundation::ERROR_FILE_NOT_FOUND.0 as i32)
            )
        }
        other => panic!("expected Io, got {other:?}"),
    }
}

// --- Journal options and history ---

#[test]
fn journal_is_send() {
    fn assert_send<T: Send>() {}
    assert_send::<Journal>();
}

#[test]
fn default_journal_options_bound_history() {
    assert!(
        matches!(
            JournalOptions::default().max_history_size,
            HistorySize::Limited(_)
        ),
        "JournalOptions::default() should bound history, not be Unlimited"
    );
}

#[test]
fn push_history_limited_zero_keeps_nothing() {
    let mut history = VecDeque::new();
    let record = usn_record(1, FileId::from(1u64), Reason::RENAME_OLD_NAME, "a");

    push_history(&mut history, Some(0), &record);

    assert!(
        history.is_empty(),
        "HistorySize::Limited(0) should keep no history entries, not be treated as unlimited"
    );
}

#[test]
fn push_history_limited_n_keeps_the_n_most_recent() {
    let mut history = VecDeque::new();
    for i in 0..5i64 {
        let record = usn_record(
            i,
            FileId::from(1u64),
            Reason::RENAME_OLD_NAME,
            &i.to_string(),
        );
        push_history(&mut history, Some(3), &record);
    }

    let names: Vec<_> = history.iter().map(|r| r.name.clone()).collect();
    assert_eq!(
        names,
        vec![
            OsString::from("2"),
            OsString::from("3"),
            OsString::from("4")
        ]
    );
}

#[test]
fn push_history_unlimited_keeps_everything() {
    let mut history = VecDeque::new();
    for i in 0..50i64 {
        let record = usn_record(
            i,
            FileId::from(1u64),
            Reason::RENAME_OLD_NAME,
            &i.to_string(),
        );
        push_history(&mut history, None, &record);
    }

    assert_eq!(history.len(), 50);
}

#[test]
fn read_sized_rejects_a_buffer_smaller_than_the_minimum() {
    // The size check runs before any I/O, so this is safe to call on a fake journal with a
    // null handle: it never touches volume_handle.
    let mut journal = fake_journal(VecDeque::new());

    for size in [4, 8, Journal::MIN_READ_BUFFER_SIZE - 1] {
        let result = journal.read_sized(size);

        assert!(
            matches!(
                result,
                Err(NtfsReaderError::ReadBufferTooSmall { size: got, min })
                    if got == size && min == Journal::MIN_READ_BUFFER_SIZE
            ),
            "expected ReadBufferTooSmall for a {size}-byte buffer, got {:?}",
            result.map(|_| ())
        );
    }
}

// The boundary is the leading USN value: 8 bytes or fewer carry no record.
#[test]
fn caught_up_exactly_when_the_response_carries_no_record_bytes() {
    assert!(is_caught_up(0), "the driver returned nothing at all");
    assert!(is_caught_up(8), "exactly the leading USN and nothing else");
    assert!(!is_caught_up(9), "one byte of an actual record");
}
