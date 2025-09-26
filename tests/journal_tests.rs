#![cfg(target_os = "windows")]

use std::fs::File;
use std::io::Write;
use std::path::PathBuf;

use ntfs_reader::errors::NtfsReaderResult;
use ntfs_reader::journal::{Journal, JournalOptions};
use ntfs_reader::test_utils::{test_volume_letter, TempDirGuard};
use ntfs_reader::volume::Volume;
use tracing_subscriber::FmtSubscriber;
use windows::Win32::System::Ioctl;

#[test]
fn file_create() -> NtfsReaderResult<()> {
    let subscriber = FmtSubscriber::builder()
        .with_max_level(tracing::Level::TRACE)
        .without_time()
        .finish();
    let _ = tracing::subscriber::set_global_default(subscriber);

    let options = JournalOptions {
        reason_mask: Ioctl::USN_REASON_FILE_CREATE,
        ..JournalOptions::default()
    };
    let volume = Volume::new(format!("\\\\?\\{}:", test_volume_letter()))?;
    let mut journal = Journal::new(volume, options)?;
    while !journal.read()?.is_empty() {}

    let mut files = Vec::new();
    let mut found = Vec::new();

    let dir = PathBuf::from(format!(
        "\\\\?\\{}:\\{}",
        test_volume_letter(),
        "usn-journal-test-create"
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    let _cleanup = TempDirGuard::new(&dir)?;

    for x in 0..10 {
        let path = dir.join(format!("usn-journal-test-create-{}.txt", x));
        File::create(&path)?.write_all(b"test")?;
        files.push(path);
    }

    for _ in 0..10 {
        for result in journal.read()? {
            found.push(result.path);
        }

        if files.iter().all(|f| found.contains(f)) {
            return Ok(());
        }
    }

    panic!("The file creation was not detected");
}

#[test]
fn file_move() -> NtfsReaderResult<()> {
    let subscriber = FmtSubscriber::builder()
        .with_max_level(tracing::Level::TRACE)
        .without_time()
        .finish();
    let _ = tracing::subscriber::set_global_default(subscriber);

    let dir = PathBuf::from(format!(
        "\\\\?\\{}:\\{}",
        test_volume_letter(),
        "usn-journal-test-move"
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    let _cleanup = TempDirGuard::new(&dir)?;

    let path_old = dir.join("usn-journal-test-move.old");
    let path_new = path_old.with_extension("new");

    let _ = std::fs::remove_file(&path_new);
    let _ = std::fs::remove_file(&path_old);
    File::create(&path_old)?.write_all(b"test")?;

    let options = JournalOptions {
        reason_mask: Ioctl::USN_REASON_RENAME_OLD_NAME | Ioctl::USN_REASON_RENAME_NEW_NAME,
        ..JournalOptions::default()
    };
    let volume = Volume::new(format!("\\\\?\\{}:", test_volume_letter()))?;
    let mut journal = Journal::new(volume, options)?;
    while !journal.read()?.is_empty() {}

    std::fs::rename(&path_old, &path_new)?;

    for _ in 0..10 {
        for result in journal.read()? {
            if result.path == path_new && (result.reason & Ioctl::USN_REASON_RENAME_NEW_NAME != 0) {
                if let Some(path) = journal.match_rename(&result) {
                    assert_eq!(path, path_old);
                    return Ok(());
                } else {
                    panic!("No old path found for {}", result.path.display());
                }
            }
        }
    }

    panic!("The file move was not detected");
}

#[test]
fn file_delete() -> NtfsReaderResult<()> {
    let subscriber = FmtSubscriber::builder()
        .with_max_level(tracing::Level::TRACE)
        .without_time()
        .finish();
    let _ = tracing::subscriber::set_global_default(subscriber);

    let dir = PathBuf::from(format!(
        "\\\\?\\{}:\\{}",
        test_volume_letter(),
        "usn-journal-test-delete"
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    let _cleanup = TempDirGuard::new(&dir)?;
    let file_path = dir.join("usn-journal-test-delete.txt");
    File::create(&file_path)?.write_all(b"test")?;

    let options = JournalOptions {
        reason_mask: Ioctl::USN_REASON_FILE_DELETE,
        ..JournalOptions::default()
    };
    let volume = Volume::new(format!("\\\\?\\{}:", test_volume_letter()))?;
    let mut journal = Journal::new(volume, options)?;
    while !journal.read()?.is_empty() {}

    std::fs::remove_file(&file_path)?;

    for _ in 0..10 {
        for result in journal.read()? {
            if result.path == file_path {
                return Ok(());
            }
        }
    }

    panic!("The file deletion was not detected");
}
