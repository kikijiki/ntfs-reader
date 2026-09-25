#![cfg(target_os = "windows")]

//! `NtfsFile::open_stream` against a real volume: every stream is created through Win32, read
//! back through the crate from the raw volume, and compared with what Win32 reads.
//!
//! A test that cannot check what it is for (missing fixture, no such file) prints `SKIPPED:
//! <test>: <reason>` and, under `NTFS_READER_REQUIRE_ALL=1` (set by the maintainer's VM run),
//! fails instead: a skip must not look like a pass.

use std::ffi::{c_void, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};

use ntfs_reader::{
    ClusterBitmap, DefaultPathCache, ExtentLocation, FileInfo, Mft, NtfsFile, NtfsReaderError,
    StreamReader, Volume,
};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Ioctl::{FSCTL_SET_COMPRESSION, FSCTL_SET_SPARSE};
use windows::Win32::System::IO::DeviceIoControl;

mod common;
use common::{
    allow_fsutil, flush_volume, parity_volume_letter, skip, test_volume_letter, TempDirGuard,
};

const MIB: u64 = 1 << 20;

/// A fixture directory `\\?\<volume>:\<name>`, removed when dropped.
fn fixture_dir(name: &str) -> (TempDirGuard, PathBuf) {
    let dir = PathBuf::from(format!("\\\\?\\{}:\\{name}", test_volume_letter()));
    (TempDirGuard::new(&dir).expect("create fixture dir"), dir)
}

fn load() -> Mft {
    flush_volume(&test_volume_letter());
    let volume = Volume::new(format!("\\\\.\\{}:", test_volume_letter())).expect("open volume");
    Mft::new(volume).expect("load the MFT")
}

/// The file `name` in the fixture directory `dir_name`.
fn find<'m>(mft: &'m Mft, dir_name: &str, name: &str) -> NtfsFile<'m> {
    let mut cache = DefaultPathCache::new();
    let tail = Path::new(dir_name).join(name);
    mft.files()
        .find(|file| {
            file.hard_links().any(|link| {
                link.to_string() == name
                    && mft
                        .resolve_path(&link, &mut cache)
                        .is_some_and(|path| path.ends_with(&tail))
            })
        })
        .unwrap_or_else(|| panic!("{dir_name}\\{name} is not in the MFT"))
}

/// Deterministic, incompressible bytes: a different chunk for every `seed`.
fn pattern(seed: u64, length: usize) -> Vec<u8> {
    let mut state = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    let mut bytes = Vec::with_capacity(length);
    while bytes.len() < length {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        bytes.extend_from_slice(&state.to_le_bytes());
    }
    bytes.truncate(length);
    bytes
}

fn ioctl(file: &File, code: u32, input: Option<&u16>) -> std::io::Result<()> {
    let mut returned = 0u32;
    unsafe {
        DeviceIoControl(
            HANDLE(file.as_raw_handle()),
            code,
            input.map(|value| value as *const u16 as *const c_void),
            if input.is_some() { 2 } else { 0 },
            None,
            0,
            Some(&mut returned),
            None,
        )
    }
    .map_err(|err| std::io::Error::other(format!("ioctl {code:#x} failed: {err}")))
}

fn mark_sparse(file: &File) -> std::io::Result<()> {
    ioctl(file, FSCTL_SET_SPARSE, None)
}

/// The extents tile the stream from 0 to its size, in order, and none is missing.
fn assert_extents_are_sound(reader: &StreamReader, what: &str) {
    let mut end = 0;
    for extent in reader.extents() {
        assert_eq!(
            extent.stream_offset, end,
            "{what}: extents are not contiguous"
        );
        assert!(extent.length > 0, "{what}: empty extent");
        assert_ne!(
            extent.location,
            ExtentLocation::Missing,
            "{what}: a live file has no missing extent"
        );
        end += extent.length;
    }
    assert_eq!(end, reader.size(), "{what}: extents do not end at the size");
    assert!(reader.initialized_size() <= reader.size(), "{what}");
}

/// Reads `len` bytes at `offset` from both and compares them, 4 MiB at a time. Says where the
/// first difference is.
fn assert_window_equal(
    ours: &mut StreamReader,
    truth: &mut File,
    offset: u64,
    len: u64,
    what: &str,
) {
    ours.seek(SeekFrom::Start(offset)).unwrap();
    truth.seek(SeekFrom::Start(offset)).unwrap();
    let mut done = 0u64;
    let mut a = vec![0u8; 4 << 20];
    let mut b = vec![0u8; 4 << 20];
    while done < len {
        let chunk = (len - done).min(a.len() as u64) as usize;
        ours.read_exact(&mut a[..chunk])
            .unwrap_or_else(|err| panic!("{what}: reading at {}: {err}", offset + done));
        truth.read_exact(&mut b[..chunk]).unwrap();
        if a[..chunk] != b[..chunk] {
            let at = a[..chunk]
                .iter()
                .zip(&b[..chunk])
                .position(|(x, y)| x != y)
                .unwrap();
            panic!(
                "{what}: first difference at stream offset {}",
                offset + done + at as u64
            );
        }
        done += chunk as u64;
    }
}

/// `stream` of `file` reads exactly like the same stream opened through Win32 at `path`: same
/// size, all of it, and windows after seeking. Returns the reader.
fn assert_reads_like_win32(file: &NtfsFile, stream: Option<&str>, path: &Path) -> StreamReader {
    let what = format!("{} stream {stream:?}", path.display());
    let name = stream.map(OsString::from);
    let mut ours = file
        .open_stream(name.as_deref())
        .unwrap_or_else(|err| panic!("{what}: open_stream: {err}"));
    let win32_path = match stream {
        Some(stream) => PathBuf::from(format!("{}:{stream}", path.display())),
        None => path.to_path_buf(),
    };
    let mut truth = File::open(&win32_path).unwrap_or_else(|err| panic!("{what}: {err}"));
    let size = truth.metadata().unwrap().len();

    assert_eq!(ours.size(), size, "{what}: size");
    assert_extents_are_sound(&ours, &what);
    assert_window_equal(&mut ours, &mut truth, 0, size, &what);
    assert_eq!(
        ours.read(&mut [0u8; 64]).unwrap(),
        0,
        "{what}: reads nothing at the end"
    );

    for offset in [size / 3, size.saturating_sub(100), size / 2 + 1, 0] {
        let len = (size - offset).min(10_000);
        assert_window_equal(&mut ours, &mut truth, offset, len, &what);
    }
    // Into a caller's buffer at every odd address: the volume must never be read into one.
    let len = size.min(10_000) as usize;
    let mut expected = vec![0u8; len];
    for offset in [size / 3, 0] {
        truth.seek(SeekFrom::Start(offset)).unwrap();
        let len = (size - offset).min(10_000) as usize;
        truth.read_exact(&mut expected[..len]).unwrap();
        for skew in [1usize, 2, 3, 5, 7, 8, 13] {
            let mut buf = vec![0u8; len + 16];
            ours.seek(SeekFrom::Start(offset)).unwrap();
            ours.read_exact(&mut buf[skew..skew + len])
                .unwrap_or_else(|err| panic!("{what}: at {offset} into buffer +{skew}: {err}"));
            assert!(
                buf[skew..skew + len] == expected[..len],
                "{what}: at {offset}, buffer +{skew}"
            );
        }
    }
    ours
}

// A small file lives in its MFT record: the stream is served from the record.
#[test]
fn a_resident_file_reads_from_the_record() {
    let (_guard, dir) = fixture_dir("stream-resident");
    let path = dir.join("small.txt");
    fs::write(&path, pattern(1, 100)).unwrap();
    fs::create_dir(dir.join("sub")).unwrap();

    let mft = load();
    let file = find(&mft, "stream-resident", "small.txt");
    let reader = assert_reads_like_win32(&file, None, &path);
    assert_eq!(reader.size(), 100);
    assert_eq!(reader.initialized_size(), 100);
    assert_eq!(reader.extents().len(), 1);
    assert_eq!(reader.extents()[0].location, ExtentLocation::Resident);
    assert_eq!(reader.cluster_size(), mft.volume().cluster_size());

    // No stream of that name, and a directory has no default stream.
    assert!(matches!(
        file.open_stream(Some("nope".as_ref())),
        Err(NtfsReaderError::StreamNotFound { .. })
    ));
    let directory = find(&mft, "stream-resident", "sub");
    assert!(directory.is_directory());
    assert!(matches!(
        directory.open_stream(None),
        Err(NtfsReaderError::StreamNotFound { name: None })
    ));
}

// Two files growing in turn, flushed each time, end up interleaved on the volume: the runs are
// several and their clusters are out of order.
#[test]
fn a_fragmented_file_reads_across_its_runs() {
    let (_guard, dir) = fixture_dir("stream-fragmented");
    let (path_a, path_b) = (dir.join("frag-a.bin"), dir.join("frag-b.bin"));
    let mut a = File::create(&path_a).unwrap();
    let mut b = File::create(&path_b).unwrap();
    for i in 0..24u64 {
        a.write_all(&pattern(i, 192 * 1024)).unwrap();
        a.sync_all().unwrap();
        b.write_all(&pattern(1000 + i, 192 * 1024)).unwrap();
        b.sync_all().unwrap();
    }
    drop((a, b));

    let mft = load();
    let mut counts = Vec::new();
    for (name, path) in [("frag-a.bin", &path_a), ("frag-b.bin", &path_b)] {
        let file = find(&mft, "stream-fragmented", name);
        let reader = assert_reads_like_win32(&file, None, path);
        assert_eq!(reader.size(), 24 * 192 * 1024);
        counts.push(reader.extents().len());
    }
    eprintln!("fragmented: extents per file {counts:?}");
    assert!(
        counts.iter().any(|&count| count > 1),
        "two files grown in turn were not fragmented: {counts:?}"
    );
}

// A hole in the middle and one at the end. Sparse parts are runs without a location.
#[test]
fn a_sparse_file_reads_its_holes_as_zeroes() {
    let (_guard, dir) = fixture_dir("stream-sparse");
    let path = dir.join("sparse.bin");
    let mut file = File::create(&path).unwrap();
    mark_sparse(&file).unwrap();
    file.write_all(&pattern(1, MIB as usize)).unwrap();
    file.seek(SeekFrom::Start(32 * MIB)).unwrap();
    file.write_all(&pattern(2, MIB as usize)).unwrap();
    file.set_len(96 * MIB).unwrap();
    file.sync_all().unwrap();
    drop(file);

    let mft = load();
    let file = find(&mft, "stream-sparse", "sparse.bin");
    let reader = assert_reads_like_win32(&file, None, &path);
    let sparse: u64 = reader
        .extents()
        .iter()
        .filter(|extent| extent.location == ExtentLocation::Sparse)
        .map(|extent| extent.length)
        .sum();
    assert!(
        sparse >= 90 * MIB,
        "only {sparse} sparse bytes in {:?}",
        reader.extents()
    );
    assert_eq!(
        reader.extents().last().unwrap().location,
        ExtentLocation::Sparse
    );
    assert!(reader
        .extents()
        .iter()
        .any(|extent| matches!(extent.location, ExtentLocation::Volume { .. })));
}

// Extending a file with SetEndOfFile allocates clusters that hold whatever was on the volume, and
// leaves the written length where it was: those bytes must read as zeroes.
#[test]
fn bytes_past_the_initialized_size_read_as_zeroes() {
    let (_guard, dir) = fixture_dir("stream-initialized");
    let path = dir.join("extended.bin");
    let mut file = File::create(&path).unwrap();
    file.write_all(&pattern(1, MIB as usize)).unwrap();
    file.set_len(10 * MIB).unwrap();
    file.sync_all().unwrap();
    drop(file);

    let mft = load();
    let file = find(&mft, "stream-initialized", "extended.bin");
    let reader = assert_reads_like_win32(&file, None, &path);
    assert_eq!(reader.size(), 10 * MIB);
    assert!(
        reader.initialized_size() < reader.size(),
        "initialized size {} is not below the size",
        reader.initialized_size()
    );
}

#[test]
fn an_alternate_stream_reads_by_name() {
    let (_guard, dir) = fixture_dir("stream-ads");
    let path = dir.join("host.txt");
    fs::write(&path, pattern(1, 300)).unwrap();
    fs::write(dir.join("host.txt:note"), b"a short note").unwrap();
    fs::write(dir.join("host.txt:big"), pattern(2, 300 * 1024)).unwrap();

    let mft = load();
    let file = find(&mft, "stream-ads", "host.txt");
    assert_reads_like_win32(&file, None, &path);
    let note = assert_reads_like_win32(&file, Some("note"), &path);
    assert_eq!(note.extents()[0].location, ExtentLocation::Resident);
    let big = assert_reads_like_win32(&file, Some("big"), &path);
    assert!(matches!(
        big.extents()[0].location,
        ExtentLocation::Volume { .. }
    ));
    assert_eq!(big.size(), 300 * 1024);

    assert!(matches!(
        file.open_stream(Some("Nope".as_ref())),
        Err(NtfsReaderError::StreamNotFound { .. })
    ));
}

// So many streams that the base record overflows and NTFS adds an `$ATTRIBUTE_LIST` and extension
// records: the streams are spread over several records.
#[test]
fn a_file_with_many_streams_reads_them_all() {
    let (_guard, dir) = fixture_dir("stream-many");
    let path = dir.join("many.bin");
    fs::write(&path, pattern(0, 5000)).unwrap();
    let names: Vec<String> = (0..150).map(|i| format!("stream-{i:03}")).collect();
    for (i, name) in names.iter().enumerate() {
        // Mostly resident, every twenty-fifth one not.
        let length = if i % 25 == 0 { 20_000 } else { 10 + i };
        fs::write(
            dir.join(format!("many.bin:{name}")),
            pattern(i as u64 + 1, length),
        )
        .unwrap();
    }

    let mft = load();
    let file = find(&mft, "stream-many", "many.bin");
    assert!(
        file.records().count() > 1,
        "no extension record: the file is not spread over records"
    );
    assert_reads_like_win32(&file, None, &path);
    for name in &names {
        assert_reads_like_win32(&file, Some(name), &path);
    }
    // Every stream the file reports opens.
    let reported: Vec<_> = file.data_streams().collect();
    assert_eq!(reported.len(), names.len() + 1);
    for stream in reported {
        let reader = file.open_stream(stream.name.as_deref()).unwrap();
        assert_eq!(reader.size(), stream.size);
    }
}

// One default stream whose runs do not fit in the base record: its extents are in extension
// records, each with its own VCN range.
#[test]
fn a_stream_with_thousands_of_runs_joins_its_extents_across_records() {
    let (_guard, dir) = fixture_dir("stream-runs");
    let path = dir.join("runs.bin");
    let mut file = File::create(&path).unwrap();
    mark_sparse(&file).unwrap();
    let step = 128 * 1024u64;
    for k in 0..1500u64 {
        file.seek(SeekFrom::Start(k * step)).unwrap();
        file.write_all(&pattern(k, 4096)).unwrap();
    }
    file.set_len(1500 * step).unwrap();
    file.sync_all().unwrap();
    drop(file);

    let mft = load();
    let file = find(&mft, "stream-runs", "runs.bin");
    let reader = assert_reads_like_win32(&file, None, &path);
    eprintln!(
        "runs: {} extents over {} records",
        reader.extents().len(),
        file.records().count()
    );
    assert!(
        file.records().count() > 1,
        "the runs fit in the base record"
    );
    assert!(reader.extents().len() > 1000);
}

#[test]
fn a_100_mib_file_reads_like_win32() {
    let (_guard, dir) = fixture_dir("stream-100m");
    let path = dir.join("big.bin");
    let mut file = File::create(&path).unwrap();
    for i in 0..100 {
        file.write_all(&pattern(i, MIB as usize)).unwrap();
    }
    file.sync_all().unwrap();
    drop(file);

    let mft = load();
    let file = find(&mft, "stream-100m", "big.bin");
    let reader = assert_reads_like_win32(&file, None, &path);
    assert_eq!(reader.size(), 100 * MIB);
    assert_eq!(FileInfo::new(&file).size, 100 * MIB);
}

// A file opened with no sharing cannot be opened through Win32 by anyone else, but the raw volume
// does not care.
#[test]
fn a_locked_file_reads_from_the_raw_volume() {
    let (_guard, dir) = fixture_dir("stream-locked");
    let path = dir.join("locked.bin");
    let contents = pattern(7, 3 * MIB as usize + 123);
    let mut locked = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .share_mode(0)
        .open(&path)
        .unwrap();
    locked.write_all(&contents).unwrap();
    locked.sync_all().unwrap();
    assert!(
        File::open(&path).is_err(),
        "the file is not locked, the test proves nothing"
    );

    let mft = load();
    let file = find(&mft, "stream-locked", "locked.bin");
    let mut reader = file.open_stream(None).unwrap();
    assert_extents_are_sound(&reader, "locked");
    let mut read = Vec::new();
    reader.read_to_end(&mut read).unwrap();
    assert!(read == contents, "the locked file's bytes differ");
    drop(locked);
}

/// A new compressed file at `path` with `bytes` written to it. `Err` says why not: NTFS refuses
/// compression on a volume with clusters over 4 KiB.
fn create_compressed(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|err| format!("create {}: {err}", path.display()))?;
    ioctl(&file, FSCTL_SET_COMPRESSION, Some(&1u16))
        .map_err(|err| format!("compression is not available on this volume: {err}"))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|err| format!("write {}: {err}", path.display()))
}

#[test]
fn compressed_and_encrypted_streams_are_refused() {
    const NAME: &str = "compressed_and_encrypted_streams_are_refused";
    let (_guard, dir) = fixture_dir("stream-refused");
    // What could not be checked and why; reported at the end, after everything that could be.
    let mut skipped: Vec<String> = Vec::new();

    // Compressed, non-resident: the bytes on the volume are LZNT1.
    let compressed = dir.join("compressed.bin");
    let compressed_ok = create_compressed(
        &compressed,
        &pattern(1, 200 * 1024)
            .iter()
            .map(|byte| byte % 4)
            .collect::<Vec<u8>>(),
    );

    // Compressed but small: NTFS keeps it in the record, uncompressed, with the flag set.
    let small = dir.join("compressed-small.txt");
    let small_ok = create_compressed(&small, b"small and compressed");

    // Encrypted. `EncryptFileW` creates an EFS certificate and key in the user's profile if none
    // exists, outliving the test, so this runs only with `NTFS_READER_ALLOW_FSUTIL=1`.
    let encrypted = dir.join("encrypted.bin");
    let encrypted_ok = if !allow_fsutil() {
        skipped.push(
            "the encrypted case: EncryptFileW creates an EFS certificate in the user's profile \
             and NTFS_READER_ALLOW_FSUTIL=1 is not set"
                .to_string(),
        );
        false
    } else {
        use windows::core::HSTRING;
        use windows::Win32::Storage::FileSystem::EncryptFileW;
        fs::write(&encrypted, pattern(2, 64 * 1024)).unwrap();
        let name = HSTRING::from(encrypted.to_string_lossy().trim_start_matches(r"\\?\"));
        match unsafe { EncryptFileW(&name) } {
            Ok(()) => true,
            Err(err) => {
                skipped.push(format!(
                    "the encrypted case: EncryptFileW does not work on this volume or edition: {err}"
                ));
                false
            }
        }
    };

    let mft = load();
    match &compressed_ok {
        Ok(()) => {
            let file = find(&mft, "stream-refused", "compressed.bin");
            assert!(matches!(
                file.open_stream(None),
                Err(NtfsReaderError::CompressedStream)
            ));
        }
        Err(why) => skipped.push(format!("the compressed case: {why}")),
    }
    match &small_ok {
        Ok(()) => {
            let file = find(&mft, "stream-refused", "compressed-small.txt");
            let mut reader = file
                .open_stream(None)
                .expect("a small compressed file is resident");
            let mut bytes = Vec::new();
            reader.read_to_end(&mut bytes).unwrap();
            assert_eq!(bytes, b"small and compressed");
        }
        Err(why) => skipped.push(format!("the small compressed case: {why}")),
    }
    if encrypted_ok {
        let file = find(&mft, "stream-refused", "encrypted.bin");
        assert!(matches!(
            file.open_stream(None),
            Err(NtfsReaderError::EncryptedStream)
        ));
    }
    if !skipped.is_empty() {
        skip(NAME, &skipped.join("; "));
    }
}

// The fixture of the VM (`T:\large-fragmented.rar`, a 32 GiB sparse file in thousands of extents)
// is compared in windows, never whole. Reports a skip where the fixture does not exist.
#[test]
fn a_huge_fragmented_sparse_file_reads_like_win32_in_windows() {
    let path = PathBuf::from(format!("{}:\\large-fragmented.rar", test_volume_letter()));
    if !path.exists() {
        skip(
            "a_huge_fragmented_sparse_file_reads_like_win32_in_windows",
            &format!("{} does not exist", path.display()),
        );
        return;
    }

    let mft = load();
    let file = mft
        .files()
        .find(|file| {
            file.hard_links()
                .any(|link| link.to_string() == "large-fragmented.rar")
        })
        .expect("large-fragmented.rar is not in the MFT");
    let mut ours = file.open_stream(None).unwrap();
    let mut truth = File::open(&path).unwrap();

    let size = truth.metadata().unwrap().len();
    assert_eq!(ours.size(), size);
    assert_eq!(FileInfo::new(&file).size, size);
    assert_eq!(
        file.data_streams()
            .find(|stream| stream.name.is_none())
            .map(|stream| stream.size),
        Some(size)
    );
    assert_extents_are_sound(&ours, "large-fragmented.rar");

    let stored: Vec<u64> = ours
        .extents()
        .iter()
        .filter(|extent| matches!(extent.location, ExtentLocation::Volume { .. }))
        .map(|extent| extent.stream_offset)
        .collect();
    eprintln!(
        "large-fragmented.rar: {} extents, {} stored, size {size}, {} records",
        ours.extents().len(),
        stored.len(),
        file.records().count()
    );
    assert!(
        stored.len() > 1000,
        "expected thousands of extents, found {}",
        stored.len()
    );
    assert!(file.records().count() > 1);

    let window = 256 * MIB;
    // From the start, from a stored extent in the middle, and up to the end.
    let middle = stored[stored.len() / 2] & !4095;
    for offset in [0, middle.min(size - window), size - window] {
        assert_window_equal(
            &mut ours,
            &mut truth,
            offset,
            window,
            "large-fragmented.rar",
        );
    }
}

// Read only. A CompactOS-compressed file has a sparse default stream that reads as zeroes, with
// its real contents in `WofCompressedData`: opening the default stream is refused, the WOF stream
// reads raw. Runs on the volume named by `NTFS_READER_PARITY_VOLUME` (same variable and
// system-drive rule as the Win32 parity test, so `C:` needs `NTFS_READER_ALLOW_SYSTEM_DRIVE=1`),
// and skips if that is unset or the volume has no such file.
#[test]
fn a_wof_compressed_file_refuses_its_default_stream() {
    const NAME: &str = "a_wof_compressed_file_refuses_its_default_stream";
    if std::env::var_os("NTFS_READER_PARITY_VOLUME").is_none() {
        skip(NAME, "NTFS_READER_PARITY_VOLUME is not set");
        return;
    }
    let letter = parity_volume_letter();
    let volume = Volume::new(format!("\\\\.\\{letter}:")).expect("open volume");
    let mft = Mft::new(volume).expect("load the MFT");

    let wof = std::ffi::OsStr::new("WofCompressedData");
    let Some(file) = mft.files().find(|file| {
        !file.is_directory()
            && file
                .data_streams()
                .any(|stream| stream.name.as_deref() == Some(wof))
    }) else {
        skip(NAME, &format!("no WOF compressed file on {letter}:"));
        return;
    };

    assert!(matches!(
        file.open_stream(None),
        Err(NtfsReaderError::WofCompressedStream)
    ));
    let size = file
        .data_streams()
        .find(|stream| stream.name.as_deref() == Some(wof))
        .map(|stream| stream.size);
    let mut raw = file.open_stream(Some(wof)).expect("the WOF stream opens");
    assert_eq!(Some(raw.size()), size);
    assert_extents_are_sound(&raw, "WofCompressedData");
    // The raw compressed bytes are readable, and are not the zeroes of the placeholder.
    let mut head = vec![0u8; raw.size().min(64 * 1024) as usize];
    raw.read_exact(&mut head).expect("read the raw bytes");
    assert!(head.iter().any(|&byte| byte != 0));
}

// The stream reader clamps every volume read to the end of the volume, so it never depends on
// what Windows does past it. Measured on the test VM: a read starting inside the readable part
// and crossing its end returns the bytes up to it (a short read); one wholly past it returns 0
// bytes. A file cannot be placed at the end of `T:`, so this pins the raw volume's own behavior,
// read the way the crate reads: through a plain `File`, into a buffer aligned like the crate's
// scratch buffer.
//
// Three sizes differ on a volume. Measured on the VM: partition 644227268608 bytes, file system
// 644227268096 (`NumberSectors * BytesPerSector`; its last sector is the backup boot sector), and
// the volume handle readable only up to the last whole CLUSTER, `TotalClusters * BytesPerCluster`
// = 644227264512 (a read of exactly the 3584 bytes after it, 7 sectors, returns 0 bytes; one
// starting in the last cluster and going past the end returns just that cluster). The readable
// end is therefore the cluster area, used by this test. The crate never needs the trailing
// sectors: every stream extent is whole clusters, and `ClusterBitmap` counts whole clusters only.
#[test]
fn a_volume_read_that_crosses_the_end_is_short() {
    use windows::Win32::System::Ioctl::{
        FSCTL_GET_NTFS_VOLUME_DATA, GET_LENGTH_INFORMATION, IOCTL_DISK_GET_LENGTH_INFO,
        NTFS_VOLUME_DATA_BUFFER,
    };

    const BLOCK: usize = 4096;
    let path = format!("\\\\.\\{}:", test_volume_letter());
    let mut volume = File::open(&path).expect("open volume");
    let handle = HANDLE(volume.as_raw_handle());

    let mut info = GET_LENGTH_INFORMATION::default();
    let mut returned = 0u32;
    unsafe {
        DeviceIoControl(
            handle,
            IOCTL_DISK_GET_LENGTH_INFO,
            None,
            0,
            Some(&mut info as *mut GET_LENGTH_INFORMATION as *mut c_void),
            std::mem::size_of::<GET_LENGTH_INFORMATION>() as u32,
            Some(&mut returned),
            None,
        )
    }
    .expect("IOCTL_DISK_GET_LENGTH_INFO");
    let partition_length = info.Length as u64;

    let mut data = NTFS_VOLUME_DATA_BUFFER::default();
    unsafe {
        DeviceIoControl(
            handle,
            FSCTL_GET_NTFS_VOLUME_DATA,
            None,
            0,
            Some(&mut data as *mut NTFS_VOLUME_DATA_BUFFER as *mut c_void),
            std::mem::size_of::<NTFS_VOLUME_DATA_BUFFER>() as u32,
            Some(&mut returned),
            None,
        )
    }
    .expect("FSCTL_GET_NTFS_VOLUME_DATA");
    let file_system = data.NumberSectors as u64 * u64::from(data.BytesPerSector);
    let cluster_size = u64::from(data.BytesPerCluster);
    let end = data.TotalClusters as u64 * cluster_size;
    eprintln!(
        "MEASURED: partition {partition_length} bytes, file system {file_system} bytes ({} sectors of \
         {}), cluster area {end} bytes ({} clusters of {cluster_size}): the file system is {} bytes \
         longer than the cluster area, the partition {} bytes longer than the file system",
        data.NumberSectors,
        data.BytesPerSector,
        data.TotalClusters,
        file_system.saturating_sub(end),
        partition_length.saturating_sub(file_system)
    );

    // What the crate believes: its bitmap covers exactly the cluster area, and its volume size
    // (what it clamps reads to) is the file system, which contains the cluster area.
    let mft = load();
    let bitmap = ClusterBitmap::new(&mft).expect("read the cluster bitmap");
    assert_eq!(
        bitmap.cluster_count() * bitmap.cluster_size(),
        end,
        "the crate's clusters cover the cluster area of the volume"
    );
    assert_eq!(
        mft.volume().volume_size(),
        file_system,
        "Volume::volume_size() is the size of the file system"
    );
    assert!(
        end >= cluster_size && end.is_multiple_of(cluster_size) && cluster_size >= BLOCK as u64,
        "the cluster area is {end} bytes of {cluster_size} byte clusters"
    );

    // Room for a read of two clusters past a 4096-aligned start.
    let span = 2 * cluster_size as usize;
    let mut backing = vec![0u8; span + BLOCK];
    let start = backing.as_ptr().align_offset(BLOCK);
    let buf = &mut backing[start..start + span];
    let last = end - cluster_size;

    // The last cluster reads, whole, and a read that ends exactly at the end returns its bytes.
    volume.seek(SeekFrom::Start(last)).unwrap();
    let read = volume
        .read(&mut buf[..cluster_size as usize])
        .unwrap_or_else(|err| panic!("a read of the last cluster failed: {err}"));
    assert_eq!(
        read as u64, cluster_size,
        "a read of the last cluster ({cluster_size} bytes at {last})"
    );
    volume.seek(SeekFrom::Start(last - cluster_size)).unwrap();
    let read = volume
        .read(buf)
        .unwrap_or_else(|err| panic!("a read that ends at the end of the volume failed: {err}"));
    assert_eq!(
        read, span,
        "a read of the last two clusters, ending exactly at the end"
    );

    // Starting in the last cluster and going past the end: measured to return the bytes up to the
    // end, a short read (an error would be just as unusable to the crate, so accepted too).
    volume.seek(SeekFrom::Start(last)).unwrap();
    let crossing = volume.read(buf);
    eprintln!("a read of {span} bytes at {last}, {cluster_size} past the end: {crossing:?}");
    if let Ok(read) = crossing {
        assert_eq!(
            read as u64, cluster_size,
            "a read that crosses the end returns the bytes up to it"
        );
    }

    // From the end on there is nothing either.
    volume.seek(SeekFrom::Start(end)).unwrap();
    let past = volume.read(&mut buf[..cluster_size as usize]);
    eprintln!("a read at the end of the volume: {past:?}");
    if let Ok(read) = past {
        assert_eq!(read, 0, "a read at the end of the volume returns no bytes");
    }
}
