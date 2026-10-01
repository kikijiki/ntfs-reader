#![cfg(target_os = "windows")]

//! A flat NTFS image copied from a quiet, small real volume must behave like its source.
//! Run elevated with `NTFS_READER_IMAGE_SOURCE_VOLUME` naming the source drive letter and
//! `NTFS_READER_TEST_VOLUME` naming a disposable destination volume. The source is read-only,
//! must not be the system or destination volume, and must be at most 4 GiB. The temporary image
//! is deleted even if an assertion fails.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::mem::size_of;
use std::os::windows::io::AsRawHandle;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use ntfs_reader::{DefaultPathCache, DeletedPathCache, Mft, NtfsFile, NtfsReaderError, Volume};
use windows::core::HSTRING;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::GetVolumeNameForVolumeMountPointW;
use windows::Win32::System::Ioctl::{
    FSCTL_ALLOW_EXTENDED_DASD_IO, GET_LENGTH_INFORMATION, IOCTL_DISK_GET_LENGTH_INFO,
};
use windows::Win32::System::IO::DeviceIoControl;

mod common;
use common::{skip, test_volume_letter, FileSnapshot, TempDirGuard};

fn drive_letter(value: &str) -> String {
    let value = value.trim().trim_end_matches(':');
    assert!(
        value.len() == 1 && value.as_bytes()[0].is_ascii_alphabetic(),
        "expected a drive letter, got {value:?}"
    );
    value.to_ascii_uppercase()
}

fn volume_name(letter: &str) -> String {
    let root = HSTRING::from(format!("{letter}:\\"));
    let mut name = [0u16; 64];
    // SAFETY: the root is NUL terminated and the output buffer lives throughout the call.
    unsafe { GetVolumeNameForVolumeMountPointW(&root, &mut name) }
        .unwrap_or_else(|error| panic!("identify volume {letter}: {error}"));
    let end = name
        .iter()
        .position(|&unit| unit == 0)
        .expect("volume name terminator");
    String::from_utf16(&name[..end])
        .expect("a volume GUID is valid UTF-16")
        .to_ascii_lowercase()
}

fn device_length(source: &File) -> u64 {
    let mut info = GET_LENGTH_INFORMATION::default();
    let mut returned = 0;
    // SAFETY: the source handle and writable output buffers live throughout this call.
    unsafe {
        DeviceIoControl(
            HANDLE(source.as_raw_handle()),
            IOCTL_DISK_GET_LENGTH_INFO,
            None,
            0,
            Some((&mut info as *mut GET_LENGTH_INFORMATION).cast()),
            size_of::<GET_LENGTH_INFORMATION>() as u32,
            Some(&mut returned),
            None,
        )
    }
    .expect("query the source device length");
    assert_eq!(returned as usize, size_of::<GET_LENGTH_INFORMATION>());
    assert!(info.Length > 0, "source length must be positive");
    let length = info.Length as u64;
    assert!(
        length <= 4 << 30,
        "refusing to copy a source above 4 GiB: {length} bytes"
    );
    length
}

fn copy_volume(source: &mut File, image: &mut File, length: u64) {
    let mut returned = 0;
    // Normal raw reads stop at the last whole cluster. This handle-only setting lets this
    // read-only source reach the trailing sectors too; every request stays within `length`.
    // SAFETY: the handle is live, there are no input/output buffers, and the count is writable.
    unsafe {
        DeviceIoControl(
            HANDLE(source.as_raw_handle()),
            FSCTL_ALLOW_EXTENDED_DASD_IO,
            None,
            0,
            None,
            0,
            Some(&mut returned),
            None,
        )
    }
    .expect("allow reading the source volume's trailing sectors");

    const ALIGNMENT: usize = 4096;
    const CHUNK: usize = 64 * 1024;
    let mut storage = vec![0u8; CHUNK + ALIGNMENT];
    let start = storage.as_ptr().align_offset(ALIGNMENT);
    let buffer = &mut storage[start..start + CHUNK];
    source
        .read_exact(&mut buffer[..ALIGNMENT])
        .expect("read the source boot sector");
    let sector_size = usize::from(u16::from_le_bytes([buffer[0x0b], buffer[0x0c]]));
    assert!(
        (256..=ALIGNMENT).contains(&sector_size) && sector_size.is_power_of_two(),
        "unsupported source sector size: {sector_size}"
    );
    assert!(length.is_multiple_of(sector_size as u64));
    source.seek(SeekFrom::Start(0)).expect("rewind the source");
    let mut copied = 0;
    while copied < length {
        let count = (length - copied).min(CHUNK as u64) as usize;
        // A short raw read fails here instead of retrying from an unaligned offset/address.
        let read = source
            .read(&mut buffer[..count])
            .expect("read source image bytes");
        assert_eq!(
            read, count,
            "short source read at {copied} of {length} bytes"
        );
        image.write_all(&buffer[..read]).expect("write image bytes");
        copied += read as u64;
    }
    image.sync_all().expect("flush the image");
    assert_eq!(image.metadata().expect("image metadata").len(), length);
}

fn relative_path(path: &mut PathBuf, volume: &Path) {
    let relative = path
        .strip_prefix(volume)
        .unwrap_or_else(|error| panic!("{path:?} is not below {volume:?}: {error}"));
    // A device prefix can leave a root separator where an ordinary image filename does not.
    // Keep every actual component losslessly, including non-Unicode NTFS names.
    *path = relative
        .components()
        .filter(|component| !matches!(component, Component::RootDir))
        .collect();
}

fn snapshot(
    file: &NtfsFile<'_>,
    mft: &Mft,
    live: &mut DefaultPathCache,
    deleted: &mut DeletedPathCache,
) -> FileSnapshot {
    let mut snapshot = FileSnapshot::new(file, mft, live, deleted);
    let volume = mft.volume().path();
    if let Some(path) = &mut snapshot.info.path {
        relative_path(path, volume);
    }
    for (live, deleted) in &mut snapshot.paths {
        if let Some(path) = live {
            relative_path(path, volume);
        }
        if deleted.is_complete() {
            relative_path(&mut deleted.path, volume);
        }
    }
    snapshot
}

fn assert_same_files(device: &Mft, image: &Mft) {
    assert_eq!(image.record_count(), device.record_count());
    assert_eq!(image.corrupt_records(), device.corrupt_records());
    let mut expected_live = DefaultPathCache::new();
    let mut expected_deleted = DeletedPathCache::new();
    let mut actual_live = DefaultPathCache::new();
    let mut actual_deleted = DeletedPathCache::new();
    let mut found = image.files().chain(image.deleted_files());
    let mut count = 0;
    for expected in device.files().chain(device.deleted_files()) {
        let actual = found.next().expect("image is missing a listed file");
        assert_eq!(
            actual.file_id(),
            expected.file_id(),
            "listed file IDs differ"
        );
        assert_eq!(
            snapshot(&actual, image, &mut actual_live, &mut actual_deleted),
            snapshot(&expected, device, &mut expected_live, &mut expected_deleted),
            "image differs from quiet source for file {}",
            expected.number()
        );
        count += 1;
    }
    assert!(found.next().is_none(), "image lists extra files");
    assert!(count > 0, "the source must contain files to compare");
    eprintln!("MEASURED: flat image matches {count} live and deleted source files");
}

#[test]
#[ignore = "copies a small quiet volume; requires NTFS_READER_IMAGE_SOURCE_VOLUME and a different test volume"]
fn a_flat_ntfs_image_matches_its_device_and_rejects_truncation() {
    const NAME: &str = "a_flat_ntfs_image_matches_its_device_and_rejects_truncation";
    let source_letter = match std::env::var("NTFS_READER_IMAGE_SOURCE_VOLUME") {
        Ok(value) => drive_letter(&value),
        Err(std::env::VarError::NotPresent) => {
            skip(
                NAME,
                "set NTFS_READER_IMAGE_SOURCE_VOLUME to the drive letter of a quiet NTFS volume \
                 no larger than 4 GiB",
            );
            return;
        }
        Err(error) => panic!("read NTFS_READER_IMAGE_SOURCE_VOLUME: {error}"),
    };
    let destination_letter = test_volume_letter();
    assert_ne!(
        source_letter, destination_letter,
        "source and destination must differ"
    );
    let source_name = volume_name(&source_letter);
    assert_ne!(
        source_name,
        volume_name(&destination_letter),
        "drive letters alias the same volume"
    );
    let system_letter =
        drive_letter(&std::env::var("SystemDrive").expect("SystemDrive must be set"));
    assert_ne!(
        source_name,
        volume_name(&system_letter),
        "the source must not be the system volume"
    );

    let device_path = format!("\\\\.\\{source_letter}:");
    let mut source = File::open(&device_path).expect("open the source volume read-only");
    let length = device_length(&source);
    let device_volume = Volume::new(&device_path).expect("open source geometry");
    let boot_size = device_volume.volume_size();
    assert!(
        boot_size > 4096,
        "source must hold more than its boot sector"
    );
    let device = Mft::new(device_volume).expect("load the quiet source MFT");

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time")
        .as_nanos();
    let directory = PathBuf::from(format!(
        "\\\\?\\{destination_letter}:\\ntfs-reader-volume-image-{}-{nonce}",
        std::process::id()
    ));
    // Create atomically instead of TempDirGuard::new, which removes a pre-existing directory.
    fs::create_dir(&directory).expect("create a unique image directory");
    let directory = TempDirGuard(directory);
    let image_path = directory.path().join("volume.img");
    let mut image_file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&image_path)
        .expect("create a new ordinary image file");
    copy_volume(&mut source, &mut image_file, length);

    let image_volume =
        Volume::new(&image_path).expect("ordinary image must use its file length when opened");
    assert_eq!(image_volume.volume_size(), boot_size);
    let image = Mft::new(image_volume).expect("load the image MFT");
    assert_same_files(&device, &image);

    image_file
        .set_len(boot_size - 1)
        .expect("truncate only the image below its claimed size");
    image_file.sync_all().expect("flush the truncated image");
    let truncated = Volume::new(&image_path);
    assert!(
        matches!(
            truncated,
            Err(NtfsReaderError::InvalidBootSector {
                field: "total_sectors"
            })
        ),
        "ordinary image must use its file length when truncated: {truncated:?}"
    );
    eprintln!(
        "MEASURED: copied {length} bytes; rejected image truncated to {} bytes",
        boot_size - 1
    );
}
