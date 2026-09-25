#![cfg(target_os = "windows")]

//! The path from the USN journal to a deleted file: a watcher sees `FILE_DELETE` records with a
//! file id, parent id, and name, and wants the deleted file behind them. The first test covers an
//! immediate delete: the record's ids must name the freed records in the `Mft`, the names must
//! match the journal, and the deleted path must be the one the file had.
//!
//! A delete is not always immediate. A file another handle holds open is moved below
//! `$Extend\$Deleted` under a random name and freed only when the handle closes; `remove_dir_all`
//! renames every directory the same way before deleting it. What the journal reports for those
//! cases was unmeasured, so the other two tests measure it: they print `MEASURED:` lines (ids and
//! names of the records, whether the parent id names a record, the chain of parents) and assert
//! only what must hold regardless of the answer.
//!
//! Tests take a lock and run one at a time; nothing else may create files on the volume while
//! they run. A delete NTFS deferred because something held the file open (a virus scanner) is
//! reported as a skip: the test cannot check what it is for.

use std::fs::{self, OpenOptions};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use ntfs_reader::{
    DeletedPathCache, FileId, Journal, JournalOptions, Mft, NtfsFile, Reason, UsnRecord, Volume,
};

mod common;
use common::{
    deleted_directory, describe_record, drain, flush_volume, reference_of, skip,
    test_volume_letter, TempDirGuard, RECORD_NUMBER_MASK,
};

/// The tests share the volume's list of free records, so they cannot overlap.
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// How long the journal is read for a record that must appear.
const JOURNAL_WAIT: Duration = Duration::from_secs(20);

fn fixture_dir(name: &str) -> TempDirGuard {
    TempDirGuard::new(format!("{}:\\deleted-journal-{name}", test_volume_letter()))
        .expect("create the fixture directory")
}

/// Flushes the volume and loads its `$MFT`.
fn load() -> Mft {
    let letter = test_volume_letter();
    flush_volume(&letter);
    let volume = Volume::new(format!("\\\\.\\{letter}:")).expect("open the volume");
    Mft::new(volume).expect("load the MFT")
}

/// A journal (default options) that starts at the current end.
fn open_journal() -> Journal {
    let volume = Volume::new(format!("\\\\?\\{}:", test_volume_letter())).expect("open volume");
    let mut journal = Journal::new(volume, JournalOptions::default()).expect("open the journal");
    drain(&mut journal).expect("read to the end of the journal");
    journal
}

fn number_of_id(id: FileId) -> u64 {
    id.as_reference().expect("an NTFS file id fits in 64 bits") & RECORD_NUMBER_MASK
}

/// Reads the journal until a `FILE_DELETE` record exists for each of the record numbers in
/// `numbers`, or `JOURNAL_WAIT` has passed. Returns every `FILE_DELETE` record seen for them.
fn delete_records(journal: &mut Journal, numbers: &[u64]) -> Vec<UsnRecord> {
    let started = Instant::now();
    let mut found: Vec<UsnRecord> = Vec::new();
    loop {
        let result = journal.read().expect("read the journal");
        found.extend(result.records.into_iter().filter(|record| {
            record.reason.contains(Reason::FILE_DELETE)
                && numbers.contains(&number_of_id(record.file_id))
        }));
        let all = numbers
            .iter()
            .all(|&number| found.iter().any(|r| number_of_id(r.file_id) == number));
        if all || started.elapsed() > JOURNAL_WAIT {
            return found;
        }
        if result.caught_up {
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

/// Loads until record `number` is freed. `Err` with what the record is like when it never is: a
/// delete that NTFS deferred leaves it in use, below `$Extend\$Deleted`.
fn load_when_freed(number: u64) -> Result<Mft, String> {
    let mut last = String::new();
    for _ in 0..8 {
        let mft = load();
        if mft
            .record(number)
            .is_some_and(|file| !file.is_used() && !mft.is_allocated(number))
        {
            return Ok(mft);
        }
        last = describe_record(&mft, number);
        std::thread::sleep(Duration::from_millis(1500));
    }
    Err(last)
}

/// `path` on the test volume as `resolve_deleted_path` gives it (`\\.\T:\dir\file`).
fn volume_path(path: &Path) -> PathBuf {
    let text = path.to_str().expect("a UTF-8 path");
    PathBuf::from(format!("\\\\.\\{}:{}", test_volume_letter(), &text[2..]))
}

fn has_name(file: &NtfsFile, name: &std::ffi::OsStr) -> bool {
    file.names()
        .any(|candidate| candidate.to_os_string() == name)
}

/// A file and its directory are deleted while the journal is open; the journal's `FILE_DELETE`
/// records are read, and the `Mft` loaded afterwards must answer for every id and name in them.
#[test]
fn a_journal_delete_record_leads_to_the_deleted_file() {
    const NAME: &str = "a_journal_delete_record_leads_to_the_deleted_file";
    let _serial = serial();
    let dir = fixture_dir("immediate");
    let sub = dir.path().join("d");
    fs::create_dir(&sub).unwrap();
    let file = sub.join("f.txt");
    fs::write(&file, b"a file that is about to be deleted").unwrap();
    let file_number = reference_of(&file) & RECORD_NUMBER_MASK;
    let sub_number = reference_of(&sub) & RECORD_NUMBER_MASK;

    let mut journal = open_journal();
    fs::remove_file(&file).unwrap();
    fs::remove_dir(&sub).unwrap();
    let records = delete_records(&mut journal, &[file_number, sub_number]);
    let of = |number: u64| {
        records
            .iter()
            .find(|record| number_of_id(record.file_id) == number)
            .unwrap_or_else(|| panic!("no FILE_DELETE record for record {number}: {records:?}"))
    };
    let (file_record, dir_record) = (of(file_number), of(sub_number));
    assert_eq!(file_record.name, "f.txt");
    assert_eq!(dir_record.name, "d");
    assert_eq!(number_of_id(file_record.parent_id), sub_number);

    let mft = match load_when_freed(file_number).and_then(|_| load_when_freed(sub_number)) {
        Ok(mft) => mft,
        Err(why) => {
            skip(NAME, &format!("the delete was deferred: {why}"));
            return;
        }
    };

    // The file: found by the id of its record, deleted, called what the journal called it.
    let deleted = mft
        .record_by_id(file_record.file_id)
        .expect("the file id of the FILE_DELETE record names no record");
    assert!(
        deleted.is_deleted(),
        "{}",
        describe_record(&mft, file_number)
    );
    assert_eq!(deleted.file_id(), file_record.file_id);
    assert!(
        has_name(&deleted, &file_record.name),
        "the journal says {:?}, the record names {:?}",
        file_record.name,
        deleted.names().map(|n| n.to_string()).collect::<Vec<_>>()
    );
    assert!(mft.deleted_files().any(|f| f.number() == file_number));

    // Its parent: the directory, also deleted, found by the parent id of the same record.
    let parent = mft
        .record_by_id(file_record.parent_id)
        .expect("the parent id of the FILE_DELETE record names no record");
    assert!(parent.is_deleted(), "{}", describe_record(&mft, sub_number));
    assert!(parent.is_directory());
    assert_eq!(parent.file_id(), file_record.parent_id);
    assert_eq!(parent.file_id(), dir_record.file_id);
    assert!(has_name(&parent, &dir_record.name));

    // The directory's own parent, the fixture directory, is live.
    let grandparent = mft
        .record_by_id(dir_record.parent_id)
        .expect("the parent id of the directory's record names no record");
    assert!(!grandparent.is_deleted());
    assert_eq!(grandparent.file_id(), dir_record.parent_id);

    // And the path is the one the file had, complete.
    let name = deleted
        .names()
        .find(|name| name.to_os_string() == file_record.name)
        .expect("the name");
    let resolved = mft.resolve_deleted_path(&name, &mut DeletedPathCache::new());
    assert!(resolved.complete, "{resolved:?}");
    assert_eq!(resolved.path, volume_path(&file));
}

/// What the journal and the `Mft` say about `record`, printed as one line for the record: its ids and
/// name, whether its parent id names a record, and the chain of parents up from there.
fn print_measured(what: &str, mft: &Mft, record: &UsnRecord) {
    let deleted_dir = deleted_directory(mft);
    let mut chain = Vec::new();
    let mut id = record.parent_id;
    for _ in 0..8 {
        let Some(parent) = mft.record_by_id(id) else {
            chain.push(format!("{:#x}: no record", id.as_u128()));
            break;
        };
        chain.push(format!(
            "record {} {:?} used {} deleted {}{}",
            parent.number(),
            parent.best_name().map(|n| n.to_string()),
            parent.is_used(),
            parent.is_deleted(),
            if Some(parent.number()) == deleted_dir {
                " ($Extend\\$Deleted)"
            } else {
                ""
            }
        ));
        match parent.best_name().and_then(|n| n.parent_reference().into()) {
            Some(next) if parent.number() != 5 => id = FileId::from(next),
            _ => break,
        }
    }
    let own = mft.record_by_id(record.file_id);
    println!(
        "MEASURED: {what}: FILE_DELETE {:?} attributes {:#x} reason {:?}, file_id {:#x} \
         (record {}), parent_id {:#x} (record {}), record_by_id(file_id) {}, \
         record_by_id(parent_id) {}, $Extend\\$Deleted is record {deleted_dir:?}; parent chain: {chain:?}",
        record.name,
        record.file_attributes,
        record.reason,
        record.file_id.as_u128(),
        number_of_id(record.file_id),
        record.parent_id.as_u128(),
        number_of_id(record.parent_id),
        match &own {
            Some(file) => format!(
                "Some(record {}, used {}, deleted {}, name {:?})",
                file.number(),
                file.is_used(),
                file.is_deleted(),
                file.best_name().map(|n| n.to_string())
            ),
            None => "None".to_string(),
        },
        if mft.record_by_id(record.parent_id).is_some() {
            "Some"
        } else {
            "None"
        },
    );
}

/// A file deleted while another handle holds it open: the name goes at once, the record stays
/// until the handle closes. The journal's `FILE_DELETE` record arrives when the record is freed
/// and is printed with what the ids and parent chain lead to. Asserts a delete record appears
/// once the handle closes, and its file id names a record.
#[test]
fn a_journal_delete_record_for_a_file_deleted_while_open_is_measured() {
    let _serial = serial();
    let dir = fixture_dir("open");
    let file = dir.path().join("pending.txt");
    fs::write(&file, b"held open while it is deleted").unwrap();
    let number = reference_of(&file) & RECORD_NUMBER_MASK;

    let mut journal = open_journal();
    let handle = OpenOptions::new()
        .read(true)
        .share_mode(7)
        .open(&file)
        .expect("open the file with FILE_SHARE_DELETE");
    fs::remove_file(&file).unwrap();
    // Check whether a delete record already exists while the handle is still open.
    let early = {
        let started = Instant::now();
        let mut seen = Vec::new();
        while started.elapsed() < Duration::from_secs(3) {
            let result = journal.read().expect("read the journal");
            seen.extend(result.records.into_iter().filter(|record| {
                record.reason.contains(Reason::FILE_DELETE)
                    && number_of_id(record.file_id) == number
            }));
            std::thread::sleep(Duration::from_millis(200));
        }
        seen
    };
    println!(
        "MEASURED: file deleted while open: {} FILE_DELETE record(s) for record {number} while the handle is open",
        early.len()
    );
    drop(handle);

    let mut records = early;
    records.extend(delete_records(&mut journal, &[number]));
    assert!(
        !records.is_empty(),
        "no FILE_DELETE record for record {number} after the handle was closed"
    );
    let mft = match load_when_freed(number) {
        Ok(mft) => mft,
        Err(why) => {
            println!("MEASURED: file deleted while open: the record was not freed: {why}");
            load()
        }
    };
    for record in &records {
        print_measured("file deleted while open", &mft, record);
    }
    let last = records.last().unwrap();
    assert!(
        mft.record_by_id(last.file_id).is_some(),
        "the file id of the FILE_DELETE record names no record"
    );
}

/// A tree removed with `remove_dir_all`, which renames each directory below `$Extend\$Deleted`
/// before deleting it: prints the same measurements for the files' and directories' delete
/// records. Asserts every entry gets a delete record, and each file id names a record.
#[test]
fn journal_delete_records_for_a_tree_removed_with_remove_dir_all_are_measured() {
    let _serial = serial();
    let dir = fixture_dir("tree");
    let top = dir.path().join("top");
    let l1 = top.join("l1");
    fs::create_dir_all(&l1).unwrap();
    let files = [top.join("top.txt"), l1.join("one.txt")];
    for file in &files {
        fs::write(file, b"a file in a tree").unwrap();
    }
    let numbers: Vec<u64> = files
        .iter()
        .chain([&l1, &top])
        .map(|path| reference_of(path) & RECORD_NUMBER_MASK)
        .collect();

    let mut journal = open_journal();
    fs::remove_dir_all(&top).unwrap();
    let records = delete_records(&mut journal, &numbers);
    for &number in &numbers {
        assert!(
            records.iter().any(|r| number_of_id(r.file_id) == number),
            "no FILE_DELETE record for record {number}"
        );
    }
    let last = *numbers.iter().max().unwrap();
    let mft = load_when_freed(last).unwrap_or_else(|why| {
        println!("MEASURED: remove_dir_all: a record was not freed: {why}");
        load()
    });
    for record in &records {
        print_measured("remove_dir_all", &mft, record);
        assert!(
            mft.record_by_id(record.file_id).is_some(),
            "the file id of the FILE_DELETE record for {:?} names no record",
            record.name
        );
    }
}
