#![cfg(target_os = "windows")]

//! Compares the crate with Win32 on a whole volume. `#[ignore]`d: it reads every file of the
//! volume named by `NTFS_READER_PARITY_VOLUME` (a drive letter; the system drive needs
//! `NTFS_READER_ALLOW_SYSTEM_DRIVE=1`), so it runs only on request:
//!
//! ```text
//! set NTFS_READER_PARITY_VOLUME=S
//! cargo test --features internals --test win32_parity_tests -- --ignored --nocapture
//! ```
//!
//! For every in-use file record (base records 24 and up) it checks, against what Win32 says about
//! the same file:
//!
//! - `links`: `hard_links()` resolved to paths, as a set, against `FindFirstFileNameW` (a directory
//!   against its own path: it has one name);
//! - `streams`: `data_streams()` names and sizes against `FindFirstStreamW` (the WOF stream
//!   `WofCompressedData` is hidden from Win32, so it is left out of ours);
//! - `path`: `FileInfo.path` reopens a file with the same file id as `NtfsFile::file_id()`;
//! - `is_directory`, `size`, `attributes`, `created`, `accessed`, `modified`, `mft_modified`, each its own
//!   line: the crate against `GetFileInformationByHandleEx`. A directory's size is compared with 0
//!   (the crate reports the unnamed `$DATA` size, which a directory does not have; Win32's
//!   `EndOfFile` is its index size). Bits 0x200 and 0x400 of a WOF-compressed file (one with a
//!   `WofCompressedData` stream) are not compared: the filter hides them from Win32.
//!   Files under `\$Extend` are left out of these (NTFS changes them live) and counted as not compared;
//! - `children`: for every directory, the names ours attributes to it against a Win32 listing.
//!
//! A file whose change time falls between the MFT load and its own check is skipped (it may have
//! changed after the snapshot). Win32 refusing to open or list something is counted, not a
//! mismatch, unless it happens for more than 5% of the files. Everything else that differs is a
//! mismatch and fails the test; the first ones are printed per check.
//!
//! `NTFS_READER_PARITY_STRIDE=n` checks every n-th file only (default 1, all of them).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use ntfs_reader::{DefaultPathCache, FileInfo, Mft, NtfsFile, Volume};
use time::{OffsetDateTime, PrimitiveDateTime};

mod common;
use common::{flush_volume, parity_volume_letter};

type Wide = Vec<u16>;

/// Attribute bits that are not compared: `DIRECTORY` and `NORMAL` are synthesised by Win32 (the
/// directory flag is checked on its own), and NTFS keeps the two index flags in the record but
/// Win32 does not return them.
const IGNORED_ATTRIBUTES: u32 = 0x10 | 0x80 | 0x1000_0000 | 0x2000_0000;
/// FILETIME ticks (100 ns) between 1601-01-01 and 1970-01-01.
const EPOCH_DIFFERENCE: i128 = 116_444_736_000_000_000;
/// `SPARSE_FILE` and `REPARSE_POINT`: what the WOF filter hides from Win32 on a file it compressed.
const WOF_HIDDEN_ATTRIBUTES: u32 = 0x200 | 0x400;
const FIRST_NORMAL_RECORD: u64 = 24;
const ROOT_RECORD: u64 = 5;
const REPARSE_POINT: u32 = 0x400;
/// Findings kept and printed per check.
const PRINTED_PER_CHECK: usize = 15;
/// Records a thread claims at a time.
const CHUNK: u64 = 2048;

// ---- Win32 -----------------------------------------------------------------------------------

mod win32 {
    use super::Wide;
    use std::ffi::c_void;
    use windows::core::{Error, PCWSTR, PWSTR};
    use windows::Win32::Foundation::{
        CloseHandle, ERROR_HANDLE_EOF, ERROR_MORE_DATA, ERROR_NO_MORE_FILES, HANDLE,
    };
    use windows::Win32::Storage::FileSystem::FILE_STANDARD_INFO;
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FileBasicInfo, FileIdInfo, FileStandardInfo, FindClose, FindExInfoBasic,
        FindExSearchNameMatch, FindFirstFileExW, FindFirstFileNameW, FindFirstStreamW,
        FindNextFileNameW, FindNextFileW, FindNextStreamW, FindStreamInfoStandard,
        GetFileInformationByHandleEx, FILE_BASIC_INFO, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_INFO, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, FIND_FIRST_EX_LARGE_FETCH, OPEN_EXISTING,
        WIN32_FIND_DATAW, WIN32_FIND_STREAM_DATA,
    };

    /// A Win32 error code, or what `windows` reports for an HRESULT that is not one.
    pub type Code = u32;

    fn code(error: &Error) -> Code {
        (error.code().0 as u32) & 0xFFFF
    }

    fn nul(path: &[u16]) -> Wide {
        let mut path = path.to_vec();
        path.push(0);
        path
    }

    struct Find(HANDLE);

    impl Drop for Find {
        fn drop(&mut self) {
            unsafe {
                let _ = FindClose(self.0);
            }
        }
    }

    struct Handle(HANDLE);

    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }

    /// What one open handle says about a file.
    pub struct Opened {
        pub basic: FILE_BASIC_INFO,
        pub size: u64,
        pub directory: bool,
        pub id: u128,
    }

    /// Opens `path` (`\\?\` form) the way the parity test needs: attributes only, the reparse
    /// point itself, directories too. Nothing is recalled and nothing is read.
    pub fn open(path: &[u16]) -> Result<Opened, Code> {
        let path = nul(path);
        unsafe {
            let handle = CreateFileW(
                PCWSTR(path.as_ptr()),
                FILE_READ_ATTRIBUTES.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                None,
            )
            .map_err(|e| code(&e))?;
            let handle = Handle(handle);

            let mut basic = FILE_BASIC_INFO::default();
            GetFileInformationByHandleEx(
                handle.0,
                FileBasicInfo,
                &mut basic as *mut _ as *mut c_void,
                size_of::<FILE_BASIC_INFO>() as u32,
            )
            .map_err(|e| code(&e))?;
            let mut standard = FILE_STANDARD_INFO::default();
            GetFileInformationByHandleEx(
                handle.0,
                FileStandardInfo,
                &mut standard as *mut _ as *mut c_void,
                size_of::<FILE_STANDARD_INFO>() as u32,
            )
            .map_err(|e| code(&e))?;
            let mut id = FILE_ID_INFO::default();
            GetFileInformationByHandleEx(
                handle.0,
                FileIdInfo,
                &mut id as *mut _ as *mut c_void,
                size_of::<FILE_ID_INFO>() as u32,
            )
            .map_err(|e| code(&e))?;

            Ok(Opened {
                basic,
                size: standard.EndOfFile as u64,
                directory: standard.Directory,
                id: u128::from_le_bytes(id.FileId.Identifier),
            })
        }
    }

    /// Every hard link of `path`, as paths relative to the volume (`\dir\file`).
    pub fn hard_links(path: &[u16]) -> Result<Vec<Wide>, Code> {
        let path = nul(path);
        let mut buffer = vec![0u16; 1024];
        let mut links = Vec::new();
        unsafe {
            let mut length = buffer.len() as u32;
            let find = loop {
                match FindFirstFileNameW(
                    PCWSTR(path.as_ptr()),
                    0,
                    &mut length,
                    PWSTR(buffer.as_mut_ptr()),
                ) {
                    Ok(handle) => break Find(handle),
                    Err(e) if code(&e) == ERROR_MORE_DATA.0 => {
                        buffer.resize(length as usize, 0);
                        length = buffer.len() as u32;
                    }
                    Err(e) => return Err(code(&e)),
                }
            };
            loop {
                links.push(buffer[..length.saturating_sub(1) as usize].to_vec());
                length = buffer.len() as u32;
                loop {
                    match FindNextFileNameW(find.0, &mut length, PWSTR(buffer.as_mut_ptr())) {
                        Ok(()) => break,
                        Err(e) if code(&e) == ERROR_MORE_DATA.0 => {
                            buffer.resize(length as usize, 0);
                            length = buffer.len() as u32;
                        }
                        Err(e) if code(&e) == ERROR_HANDLE_EOF.0 => return Ok(links),
                        Err(e) => return Err(code(&e)),
                    }
                }
            }
        }
    }

    /// The `$DATA` streams Win32 lists: `None` is the default stream.
    pub fn streams(path: &[u16]) -> Result<Vec<(Option<Wide>, u64)>, Code> {
        let path = nul(path);
        let mut streams = Vec::new();
        unsafe {
            let mut data = WIN32_FIND_STREAM_DATA::default();
            let find = match FindFirstStreamW(
                PCWSTR(path.as_ptr()),
                FindStreamInfoStandard,
                &mut data as *mut _ as *mut c_void,
                None,
            ) {
                Ok(handle) => Find(handle),
                // A directory without streams: nothing to list.
                Err(e) if code(&e) == ERROR_HANDLE_EOF.0 => return Ok(streams),
                Err(e) => return Err(code(&e)),
            };
            loop {
                streams.push((stream_name(&data.cStreamName), data.StreamSize as u64));
                match FindNextStreamW(find.0, &mut data as *mut _ as *mut c_void) {
                    Ok(()) => {}
                    Err(e) if code(&e) == ERROR_HANDLE_EOF.0 => return Ok(streams),
                    Err(e) => return Err(code(&e)),
                }
            }
        }
    }

    /// `::$DATA` is the default stream, `:name:$DATA` the stream `name`.
    fn stream_name(raw: &[u16]) -> Option<Wide> {
        let end = raw.iter().position(|&unit| unit == 0).unwrap_or(raw.len());
        let raw = &raw[..end];
        let suffix: Wide = ":$DATA".encode_utf16().collect();
        let name = raw.strip_prefix(&[b':' as u16][..]).unwrap_or(raw);
        let name = name.strip_suffix(&suffix[..]).unwrap_or(name);
        (!name.is_empty()).then(|| name.to_vec())
    }

    /// The names in the directory `path` (`\\?\` form), without `.` and `..`.
    pub fn children(path: &[u16]) -> Result<Vec<Wide>, Code> {
        let mut pattern = path.to_vec();
        if pattern.last() != Some(&(b'\\' as u16)) {
            pattern.push(b'\\' as u16);
        }
        pattern.push(b'*' as u16);
        let pattern = nul(&pattern);
        let mut names = Vec::new();
        unsafe {
            let mut data = WIN32_FIND_DATAW::default();
            let find = FindFirstFileExW(
                PCWSTR(pattern.as_ptr()),
                FindExInfoBasic,
                &mut data as *mut _ as *mut c_void,
                FindExSearchNameMatch,
                None,
                FIND_FIRST_EX_LARGE_FETCH,
            )
            .map_err(|e| code(&e))?;
            let find = Find(find);
            loop {
                let end = data
                    .cFileName
                    .iter()
                    .position(|&unit| unit == 0)
                    .unwrap_or(data.cFileName.len());
                let name = &data.cFileName[..end];
                let dot = b'.' as u16;
                if name != [dot] && name != [dot, dot] {
                    names.push(name.to_vec());
                }
                match FindNextFileW(find.0, &mut data) {
                    Ok(()) => {}
                    Err(e) if code(&e) == ERROR_NO_MORE_FILES.0 => return Ok(names),
                    Err(e) => return Err(code(&e)),
                }
            }
        }
    }
}

// ---- Results ---------------------------------------------------------------------------------

/// The checks, in the order they are reported.
const CHECKS: [&str; 11] = [
    "path",
    "links",
    "streams",
    "is_directory",
    "size",
    "attributes",
    "created",
    "accessed",
    "modified",
    "mft_modified",
    "children",
];
/// The checks that need the file opened; a file that cannot be compared is skipped in all of them.
const PER_FILE_CHECKS: [&str; 10] = [
    "path",
    "links",
    "streams",
    "is_directory",
    "size",
    "attributes",
    "created",
    "accessed",
    "modified",
    "mft_modified",
];
/// The metadata checks (a subset of `PER_FILE_CHECKS`), each reported and printed on its own.
const METADATA_CHECKS: [&str; 7] = [
    "is_directory",
    "size",
    "attributes",
    "created",
    "accessed",
    "modified",
    "mft_modified",
];

#[derive(Default)]
struct Counts {
    ok: u64,
    mismatch: u64,
    /// Not compared, and why (changed since the load, Win32 error code, ...).
    skipped: BTreeMap<String, u64>,
    /// The first mismatches.
    printed: Vec<String>,
}

#[derive(Default)]
struct Tally {
    checks: BTreeMap<&'static str, Counts>,
    /// Attribute bits that differ, and how often.
    attribute_bits: BTreeMap<u32, u64>,
    /// Informational counters about the volume.
    facts: BTreeMap<&'static str, u64>,
}

impl Tally {
    fn ok(&mut self, check: &'static str) {
        self.checks.entry(check).or_default().ok += 1;
    }

    fn mismatch(&mut self, check: &'static str, detail: impl FnOnce() -> String) {
        let counts = self.checks.entry(check).or_default();
        counts.mismatch += 1;
        if counts.printed.len() < PRINTED_PER_CHECK {
            counts.printed.push(detail());
        }
    }

    fn skipped(&mut self, check: &'static str, reason: String) {
        *self
            .checks
            .entry(check)
            .or_default()
            .skipped
            .entry(reason)
            .or_default() += 1;
    }

    fn fact(&mut self, name: &'static str, value: u64) {
        *self.facts.entry(name).or_default() += value;
    }

    fn merge(&mut self, other: Tally) {
        for (check, counts) in other.checks {
            let mine = self.checks.entry(check).or_default();
            mine.ok += counts.ok;
            mine.mismatch += counts.mismatch;
            for (reason, count) in counts.skipped {
                *mine.skipped.entry(reason).or_default() += count;
            }
            for line in counts.printed {
                if mine.printed.len() < PRINTED_PER_CHECK {
                    mine.printed.push(line);
                }
            }
        }
        for (bit, count) in other.attribute_bits {
            *self.attribute_bits.entry(bit).or_default() += count;
        }
        for (name, value) in other.facts {
            *self.facts.entry(name).or_default() += value;
        }
    }
}

// ---- Helpers ---------------------------------------------------------------------------------

fn wide(text: &OsStr) -> Wide {
    text.encode_wide().collect()
}

fn show(text: &[u16]) -> String {
    String::from_utf16_lossy(text)
}

/// What the crate documents for a FILETIME: the exact date, clamped to the range
/// `OffsetDateTime` holds. Worked out here from the definition, not by calling the crate.
fn expected_time(filetime: i64) -> OffsetDateTime {
    let nanos = (filetime as u64 as i128 - EPOCH_DIFFERENCE) * 100;
    OffsetDateTime::from_unix_timestamp_nanos(nanos).unwrap_or_else(|_| {
        if nanos < 0 {
            PrimitiveDateTime::MIN.assume_utc()
        } else {
            PrimitiveDateTime::MAX.assume_utc()
        }
    })
}

fn now_filetime() -> i64 {
    let since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("the clock is after 1970");
    (since_epoch.as_nanos() as i128 / 100 + EPOCH_DIFFERENCE) as i64
}

/// `\\.\S:\dir\file` as the volume-relative path `\dir\file` (`\` for the root) and the `\\?\`
/// form Win32 opens.
fn win32_forms(path: &Path, volume_path: &[u16]) -> Option<(Wide, Wide)> {
    let path = wide(path.as_os_str());
    let rest = path.strip_prefix(volume_path)?;
    let relative = if rest.is_empty() {
        vec![b'\\' as u16]
    } else {
        rest.to_vec()
    };
    let mut long: Wide = r"\\?\".encode_utf16().collect();
    long.extend_from_slice(&volume_path[4..]);
    long.extend_from_slice(rest);
    if rest.is_empty() {
        long.push(b'\\' as u16);
    }
    Some((relative, long))
}

/// Shared read-only state of one run.
struct Context<'a> {
    mft: &'a Mft,
    /// `\\.\S:` as UTF-16.
    volume_path: Wide,
    /// FILETIME the load started at: a file changed after it is not compared.
    loaded_at: i64,
    stride: u64,
    /// Non-DOS names by parent directory reference, from the MFT.
    children: HashMap<u64, Vec<Wide>>,
}

impl Context<'_> {
    /// Whether `change` (a change time from Win32) falls after the load: the file may have
    /// changed since the snapshot. Times in the future (the fixture has files dated far ahead)
    /// are not changes.
    fn changed_since_load(&self, change: i64) -> bool {
        change >= self.loaded_at && change <= now_filetime()
    }
}

// ---- The checks ------------------------------------------------------------------------------

fn check_file(context: &Context, file: &NtfsFile, cache: &mut DefaultPathCache, tally: &mut Tally) {
    let mft = context.mft;
    let number = file.number();
    let info = FileInfo::with_cache(file, cache);

    tally.fact("files", 1);
    if file.records().count() > 1 {
        tally.fact("files spanning extension records", 1);
    }
    let links: Vec<_> = file.hard_links().collect();
    if links.len() > 1 {
        tally.fact("files with several hard links", 1);
    }
    if file.data_streams().any(|stream| stream.name.is_some()) {
        tally.fact("files with alternate data streams", 1);
    }

    let forms = info
        .path
        .as_deref()
        .and_then(|path| win32_forms(path, &context.volume_path));
    let Some((relative, long)) = forms else {
        tally.mismatch("path", || {
            format!(
                "record {number} ({:?}): FileInfo.path is {:?}, not a path on the volume",
                info.name, info.path
            )
        });
        return;
    };

    // One handle answers the path, metadata and change questions.
    let opened = match win32::open(&long) {
        Ok(opened) => opened,
        Err(code) => {
            for check in PER_FILE_CHECKS {
                tally.skipped(check, format!("open failed, Win32 error {code}"));
            }
            return;
        }
    };
    if context.changed_since_load(opened.basic.ChangeTime) {
        for check in PER_FILE_CHECKS {
            tally.skipped(check, "changed after the MFT was read".into());
        }
        return;
    }

    // path: reopening FileInfo.path gives the same file.
    if opened.id == file.file_id().as_u128() {
        tally.ok("path");
    } else {
        tally.mismatch("path", || {
            format!(
                "record {number} {}: file id from Win32 {:#x}, from the MFT {:#x}",
                show(&long),
                opened.id,
                file.file_id().as_u128()
            )
        });
    }

    // metadata
    if is_live_metadata_file(&relative) {
        // NTFS keeps changing these while the volume is mounted (transaction log, deleted-file
        // tracking), so the on-disk record and what Win32 reports differ in times and in
        // attribute bits Win32 hides. Their names, links and streams are still compared.
        for check in METADATA_CHECKS {
            tally.skipped(check, "live NTFS metadata file under $Extend".into());
        }
    } else {
        // The same detection the streams check uses: a file with a WofCompressedData stream is
        // compressed by the WOF filter.
        let wof = file
            .data_streams()
            .any(|stream| stream.name.as_deref() == Some(OsStr::new("WofCompressedData")));
        check_metadata(&info, file, &opened, &long, wof, tally);
    }

    // links
    let reparse = opened.basic.FileAttributes & REPARSE_POINT != 0;
    let ours: BTreeSet<Wide> = links
        .iter()
        .filter_map(|link| mft.resolve_path(link, cache))
        .filter_map(|path| win32_forms(&path, &context.volume_path))
        .map(|(relative, _)| relative)
        .collect();
    // A directory has exactly one name (`FindFirstFileNameW` is documented for files): its own path.
    let theirs = if info.is_directory {
        Ok(vec![relative])
    } else {
        win32::hard_links(&long)
    };
    match theirs {
        Ok(theirs) => {
            let theirs: BTreeSet<Wide> = theirs.into_iter().collect();
            if ours == theirs {
                tally.ok("links");
            } else if reparse {
                // Win32 may follow a symbolic link or junction and list the target's links.
                tally.skipped("links", "reparse point, Win32 may follow it".into());
            } else {
                tally.mismatch("links", || {
                    format!(
                        "record {number} {}: only ours {:?}, only Win32 {:?} ({} vs {} links)",
                        show(&long),
                        ours.difference(&theirs)
                            .map(|p| show(p))
                            .collect::<Vec<_>>(),
                        theirs
                            .difference(&ours)
                            .map(|p| show(p))
                            .collect::<Vec<_>>(),
                        ours.len(),
                        theirs.len()
                    )
                });
            }
        }
        Err(code) => tally.skipped("links", format!("Win32 error {code}")),
    }

    // streams
    let ours: BTreeSet<(Option<Wide>, u64)> = file
        .data_streams()
        .filter(|stream| stream.name.as_deref() != Some(OsStr::new("WofCompressedData")))
        .map(|stream| (stream.name.as_deref().map(wide), stream.size))
        .collect();
    match win32::streams(&long) {
        Ok(theirs) => {
            let theirs: BTreeSet<_> = theirs.into_iter().collect();
            if ours == theirs {
                tally.ok("streams");
            } else if reparse {
                tally.skipped("streams", "reparse point, Win32 may follow it".into());
            } else {
                let describe = |set: &BTreeSet<(Option<Wide>, u64)>| -> Vec<String> {
                    set.iter()
                        .map(|(name, size)| format!("{:?}={size}", name.as_deref().map(show)))
                        .collect()
                };
                tally.mismatch("streams", || {
                    format!(
                        "record {number} {}: ours {:?}, Win32 {:?}",
                        show(&long),
                        describe(&ours),
                        describe(&theirs)
                    )
                });
            }
        }
        Err(code) => tally.skipped("streams", format!("Win32 error {code}")),
    }

    // children
    if info.is_directory && number >= FIRST_NORMAL_RECORD {
        if reparse {
            // Listing a junction, mount point or directory symlink lists its target.
            tally.skipped("children", "reparse point, Win32 lists the target".into());
        } else {
            check_children(context, file.reference(), &long, tally);
        }
    }
}

/// Whether `relative` (`\dir\file`, volume relative) is a file inside `\$Extend`.
fn is_live_metadata_file(relative: &[u16]) -> bool {
    let prefix: Wide = r"\$extend\".encode_utf16().collect();
    relative.len() > prefix.len()
        && relative[..prefix.len()]
            .iter()
            .zip(&prefix)
            .all(|(&unit, &wanted)| {
                char::from_u32(unit as u32).map(|c| c.to_ascii_lowercase() as u32)
                    == Some(wanted as u32)
            })
}

/// One line per kind of difference: the point is to see each kind on its own.
fn check_metadata(
    info: &FileInfo,
    file: &NtfsFile,
    opened: &win32::Opened,
    long: &[u16],
    wof: bool,
    tally: &mut Tally,
) {
    let number = file.number();
    let report =
        |tally: &mut Tally, kind: &'static str, same: bool, detail: &dyn Fn() -> String| {
            if same {
                tally.ok(kind);
            } else {
                tally.mismatch(kind, || {
                    format!("record {number} {}: {}", show(long), detail())
                });
            }
        };

    report(
        tally,
        "is_directory",
        info.is_directory == opened.directory,
        &|| format!("crate {} vs Win32 {}", info.is_directory, opened.directory),
    );

    // The crate's size is the unnamed $DATA stream's size, like `nFileSize` of `FindFirstFile`, so a
    // directory (no such stream) is 0. `FileStandardInfo.EndOfFile` of a directory is the size of its
    // index allocation instead, which is a different thing: compare a directory against 0.
    let expected_size = if opened.directory { 0 } else { opened.size };
    report(tally, "size", info.size == expected_size, &|| {
        format!("crate {} vs Win32 {}", info.size, expected_size)
    });

    let attributes = opened.basic.FileAttributes;
    let mut differing = (info.file_attributes ^ attributes) & !IGNORED_ATTRIBUTES;
    if wof && differing & WOF_HIDDEN_ATTRIBUTES != 0 {
        // The record says sparse and reparse point (true on disk); the WOF filter hides both from
        // Win32, as it hides the WofCompressedData stream. Count it, compare the other bits.
        differing &= !WOF_HIDDEN_ATTRIBUTES;
        tally.skipped("attributes", "WOF filter hides sparse/reparse bits".into());
    }
    if differing != 0 {
        for bit in (0..32)
            .map(|shift| 1u32 << shift)
            .filter(|b| differing & b != 0)
        {
            *tally.attribute_bits.entry(bit).or_default() += 1;
        }
    }
    report(tally, "attributes", differing == 0, &|| {
        format!(
            "crate {:#x} (from $STANDARD_INFORMATION) vs Win32 {:#x}, differing bits {:#x}",
            info.file_attributes, attributes, differing
        )
    });

    let standard = file.standard_information();
    let times = [
        ("created", info.created, opened.basic.CreationTime),
        ("accessed", info.accessed, opened.basic.LastAccessTime),
        ("modified", info.modified, opened.basic.LastWriteTime),
        (
            "mft_modified",
            standard.map(|standard| standard.mft_modified()),
            opened.basic.ChangeTime,
        ),
    ];
    for (kind, ours, theirs) in times {
        let expected = expected_time(theirs);
        report(tally, kind, ours == Some(expected), &|| {
            format!("crate {ours:?} vs Win32 {expected:?}")
        });
    }
}

fn check_children(context: &Context, reference: u64, long: &[u16], tally: &mut Tally) {
    let mut ours = context
        .children
        .get(&reference)
        .cloned()
        .unwrap_or_default();
    match win32::children(long) {
        Ok(mut theirs) => {
            ours.sort();
            theirs.sort();
            if ours == theirs {
                tally.ok("children");
            } else {
                let only = |a: &[Wide], b: &[Wide]| -> Vec<String> {
                    a.iter()
                        .filter(|n| !b.contains(n))
                        .take(5)
                        .map(|n| show(n))
                        .collect()
                };
                tally.mismatch("children", || {
                    format!(
                        "{}: {} vs {} entries, only ours {:?}, only Win32 {:?}",
                        show(long),
                        ours.len(),
                        theirs.len(),
                        only(&ours, &theirs),
                        only(&theirs, &ours)
                    )
                });
            }
        }
        Err(code) => tally.skipped("children", format!("Win32 error {code}")),
    }
}

fn check_range(context: &Context, next: &AtomicU64) -> Tally {
    let mft = context.mft;
    let mut tally = Tally::default();
    let mut cache = DefaultPathCache::new();
    loop {
        let start = FIRST_NORMAL_RECORD + next.fetch_add(CHUNK, Ordering::Relaxed);
        if start >= mft.record_count() {
            return tally;
        }
        let end = (start + CHUNK).min(mft.record_count());
        for number in (start..end).filter(|n| n % context.stride == 0) {
            if !mft.is_allocated(number) {
                continue;
            }
            let Some(file) = mft.record(number) else {
                continue;
            };
            if file.is_used() && !file.is_extension() {
                check_file(context, &file, &mut cache, &mut tally);
            }
        }
    }
}

/// Non-DOS names by the reference of the directory they are in.
fn index_children(mft: &Mft) -> HashMap<u64, Vec<Wide>> {
    let mut children: HashMap<u64, Vec<Wide>> = HashMap::new();
    for file in mft.files() {
        for link in file.hard_links() {
            children
                .entry(link.parent_reference())
                .or_default()
                .push(wide(&link.to_os_string()));
        }
    }
    children
}

// ---- The test --------------------------------------------------------------------------------

#[test]
#[ignore = "reads a whole volume; needs NTFS_READER_PARITY_VOLUME (see the file's docs)"]
fn the_crate_agrees_with_win32_on_the_whole_volume() {
    let letter = parity_volume_letter();
    let stride: u64 = std::env::var("NTFS_READER_PARITY_STRIDE")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|&stride| stride > 0)
        .unwrap_or(1);
    let volume_path = format!(r"\\.\{letter}:");

    // Records the system has not written back yet are not on the disk the MFT is read from.
    flush_volume(&letter);
    let loaded_at = now_filetime();
    let start = Instant::now();
    let mft = Mft::new(Volume::new(&volume_path).expect("open the volume")).expect("load the MFT");
    println!(
        "volume {letter}: Mft::new {:?}, {} records, {:.1} MiB in memory, {} corrupt, stride {stride}",
        start.elapsed(),
        mft.record_count(),
        mft.size_in_memory() as f64 / (1024.0 * 1024.0),
        mft.corrupt_records()
    );

    let start = Instant::now();
    let children = index_children(&mft);
    println!(
        "indexed the names of {} directories in {:?}",
        children.len(),
        start.elapsed()
    );

    let context = Context {
        mft: &mft,
        volume_path: wide(OsStr::new(&volume_path)),
        loaded_at,
        stride,
        children,
    };

    let threads = std::thread::available_parallelism()
        .map_or(4, |threads| threads.get())
        .min(16);
    let start = Instant::now();
    let next = AtomicU64::new(0);
    let mut total = Tally::default();
    std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|_| scope.spawn(|| check_range(&context, &next)))
            .collect();
        for worker in workers {
            total.merge(worker.join().expect("a checking thread panicked"));
        }
    });

    // The root directory is not a file the crate yields, but its children count too.
    if let Some(root) = mft.record(ROOT_RECORD) {
        let mut root_tally = Tally::default();
        let long: Wide = format!(r"\\?\{letter}:\").encode_utf16().collect();
        check_children(&context, root.reference(), &long, &mut root_tally);
        total.merge(root_tally);
    }
    let elapsed = start.elapsed();

    // ---- Report ----
    let files = total.facts.get("files").copied().unwrap_or(0);
    println!(
        "checked {files} files on {threads} threads in {elapsed:?} ({:.0} us per file)",
        elapsed.as_micros() as f64 * threads as f64 / files.max(1) as f64
    );
    for (name, value) in &total.facts {
        println!("  {name}: {value}");
    }
    let mut failed = false;
    for check in CHECKS {
        let counts = total.checks.remove(check).unwrap_or_default();
        println!(
            "{check}: {} ok, {} mismatched, {} not compared",
            counts.ok,
            counts.mismatch,
            counts.skipped.values().sum::<u64>()
        );
        for (reason, count) in &counts.skipped {
            println!("    not compared: {reason}: {count}");
        }
        for line in &counts.printed {
            println!("    MISMATCH {line}");
        }
        if counts.mismatch > counts.printed.len() as u64 {
            println!(
                "    ... and {} more mismatches",
                counts.mismatch - counts.printed.len() as u64
            );
        }
        failed |= counts.mismatch > 0;
        // A Win32 that refuses more than 5% of what it is asked proves little.
        let unavailable: u64 = counts
            .skipped
            .iter()
            .filter(|(reason, _)| {
                reason.starts_with("open failed") || reason.starts_with("Win32 error")
            })
            .map(|(_, count)| count)
            .sum();
        let asked = counts.ok + counts.mismatch + counts.skipped.values().sum::<u64>();
        if unavailable * 20 > asked {
            println!("    TOO MANY Win32 failures for {check}: {unavailable} of {asked}");
            failed = true;
        }
    }
    for (bit, count) in &total.attribute_bits {
        println!("attribute bit {bit:#x} differs: {count} files");
    }

    assert!(files > 0, "no files were checked");
    assert!(
        !failed,
        "the crate and Win32 disagree, see the MISMATCH lines above"
    );
}

// The helpers below decide what the parity test compares; they need no volume, so they run in the
// normal test pass too.
#[test]
fn only_files_inside_extend_are_live_metadata() {
    let is = |path: &str| is_live_metadata_file(&path.encode_utf16().collect::<Vec<_>>());
    assert!(is(r"\$Extend\$Deleted"));
    assert!(is(r"\$extend\$RmMetadata\$Txf"));
    assert!(!is(r"\$Extend"), "the directory itself is not inside it");
    assert!(!is(r"\dir\$Extend\file"));
    assert!(!is(r"\$ExtendedName\file"));
    assert!(!is(r"\bulk\b000"));
}

#[test]
fn expected_time_is_exact_and_clamped_like_the_crate_documents() {
    // One tick after 1601, the Unix epoch, and a FILETIME far past year 9999 (year 30000).
    let first = expected_time(1);
    assert_eq!(
        first.unix_timestamp_nanos(),
        -11_644_473_600_000_000_000 + 100
    );
    assert_eq!(expected_time(EPOCH_DIFFERENCE as i64).unix_timestamp(), 0);
    let year_30000 = 8_961_858_144_000_000_000i64;
    assert_eq!(
        expected_time(year_30000),
        PrimitiveDateTime::MAX.assume_utc()
    );
}
