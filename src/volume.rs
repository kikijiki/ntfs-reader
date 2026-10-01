// Copyright (c) 2022, Matteo Bernacchia <dev@kikijiki.com>. All rights reserved.
// This project is dual licensed under the Apache License 2.0 and the MIT license.
// See the LICENSE files in the project root for details.

//! [`Volume`]: an NTFS volume opened by path, with its geometry.

use std::fs::File;
use std::io::{self, Read};
use std::mem::size_of;
use std::path::{Path, PathBuf};

use crate::{
    aligned_reader::AlignedReader,
    api::*,
    errors::{NtfsReaderError, NtfsReaderResult},
};

/// Volume layout derived from the boot sector.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct VolumeGeometry {
    pub cluster_size: u64,
    pub volume_size: u64,
    pub file_record_size: u64,
    pub mft_position: u64,
}

impl VolumeGeometry {
    /// Parses a raw boot sector (the first 512 bytes of the volume). Only
    /// used by tests below; `Volume::new` already has a parsed `BootSector`
    /// and calls `from_boot_sector` directly.
    #[cfg(test)]
    pub fn from_bytes(bytes: &[u8]) -> NtfsReaderResult<Self> {
        Self::from_boot_sector(&decode_boot_sector(bytes)?)
    }

    /// Computes the volume layout from a parsed boot sector.
    pub fn from_boot_sector(boot_sector: &BootSector) -> NtfsReaderResult<Self> {
        fn invalid(field: &'static str) -> NtfsReaderError {
            NtfsReaderError::InvalidBootSector { field }
        }

        // The OEM id lives inside `crap_0` (offset 3..11); `api::BootSector`
        // keeps the raw layout, so read it from there instead of
        // adding a field.
        let oem_id: [u8; 8] = boot_sector.crap_0[3..11].try_into().unwrap();
        if oem_id != *b"NTFS    " {
            return Err(invalid("oem_id"));
        }

        let sector_size = boot_sector.sector_size as u64;
        if !(256..=4096).contains(&sector_size) || !sector_size.is_power_of_two() {
            return Err(invalid("sector_size"));
        }

        // `sectors_per_cluster` above 0x80 encodes a negative power-of-two
        // exponent: 2^(256 - v) sectors per cluster, instead of a plain count.
        let sectors_per_cluster = boot_sector.sectors_per_cluster;
        let cluster_size = if sectors_per_cluster > 0x80 {
            let exponent = 256u32 - sectors_per_cluster as u32;
            1u64.checked_shl(exponent)
                .and_then(|sectors| sectors.checked_mul(sector_size))
        } else {
            (sectors_per_cluster as u64).checked_mul(sector_size)
        }
        .filter(|size| size.is_power_of_two())
        .ok_or_else(|| invalid("sectors_per_cluster"))?;

        let volume_size = boot_sector
            .total_sectors
            .checked_mul(sector_size)
            .filter(|&size| size != 0)
            .ok_or_else(|| invalid("total_sectors"))?;

        // Positive `file_record_size_info` counts clusters; zero or negative
        // encodes 2^(-info) bytes. Either way the result must land on a
        // power of two in 512..=65536; that single check is the gate (no
        // separate bound on the exponent itself).
        let file_record_size_info = boot_sector.file_record_size_info;
        let file_record_size = if file_record_size_info > 0 {
            (file_record_size_info as u64).checked_mul(cluster_size)
        } else {
            file_record_size_info
                .checked_neg()
                .and_then(|exponent| 1u64.checked_shl(exponent as u32))
        }
        .filter(|size| (512..=65536).contains(size) && size.is_power_of_two())
        .ok_or_else(|| invalid("file_record_size_info"))?;

        let mft_position = boot_sector
            .mft_lcn
            .checked_mul(cluster_size)
            .ok_or_else(|| invalid("mft_lcn"))?;

        Ok(Self {
            cluster_size,
            volume_size,
            file_record_size,
            mft_position,
        })
    }
}

/// Copies a [`BootSector`] out of a raw byte buffer. No alignment
/// requirement: the struct is `repr(C, packed)`, so any byte pointer is
/// valid for it.
fn decode_boot_sector(bytes: &[u8]) -> NtfsReaderResult<BootSector> {
    if bytes.len() < size_of::<BootSector>() {
        return Err(NtfsReaderError::InvalidBootSector { field: "length" });
    }
    // SAFETY: `bytes` holds a whole boot sector (checked above), and the struct is a packed
    // `Copy` one of plain integers and byte arrays (alignment 1).
    Ok(unsafe { *(bytes.as_ptr() as *const BootSector) })
}

fn backing_length(file: &File) -> io::Result<u64> {
    // Query the opened handle, not the path: an image's logical file length is the bound,
    // never the capacity of the disk containing it. Raw volumes do not provide ordinary
    // file metadata and still require the device-length query to succeed.
    match file.metadata() {
        Ok(metadata) if metadata.is_file() => Ok(metadata.len()),
        _ => device_length(file),
    }
}

// GET_LENGTH_INFORMATION is one signed 64-bit length. An incomplete or nonpositive
// reply cannot bound a boot sector, so never treat it as an unknown/unbounded size.
#[cfg(any(windows, test))]
fn checked_device_length(length: i64, returned: u32) -> io::Result<u64> {
    if returned as usize != size_of::<i64>() || length <= 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid IOCTL_DISK_GET_LENGTH_INFO response",
        ));
    }
    Ok(length as u64)
}

#[cfg(windows)]
fn device_length(file: &File) -> io::Result<u64> {
    use std::ffi::c_void;
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Ioctl::{GET_LENGTH_INFORMATION, IOCTL_DISK_GET_LENGTH_INFO};
    use windows::Win32::System::IO::DeviceIoControl;

    let mut info = GET_LENGTH_INFORMATION::default();
    let mut returned = 0;
    // SAFETY: `file` owns the live handle throughout the synchronous call; the output
    // buffer and byte count are writable, and this query takes no input buffer.
    unsafe {
        DeviceIoControl(
            HANDLE(file.as_raw_handle()),
            IOCTL_DISK_GET_LENGTH_INFO,
            None,
            0,
            Some(&mut info as *mut GET_LENGTH_INFORMATION as *mut c_void),
            size_of::<GET_LENGTH_INFORMATION>() as u32,
            Some(&mut returned),
            None,
        )
    }
    .map_err(|error| {
        // `From<windows::core::Error>` keeps the HRESULT instead of the Win32 code.
        // Unwrap HRESULT_FROM_WIN32 so raw_os_error and AccessDenied stay correct.
        let hresult = error.code().0 as u32;
        let code = if hresult & 0xFFFF_0000 == 0x8007_0000 {
            hresult & 0xFFFF
        } else {
            hresult
        };
        io::Error::from_raw_os_error(code as i32)
    })?;
    checked_device_length(info.Length, returned)
}

#[cfg(not(windows))]
fn device_length(_file: &File) -> io::Result<u64> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "raw volume length queries require Windows",
    ))
}

/// An NTFS volume or flat volume image: its path plus the geometry read from its boot sector.
/// [`Mft::new`](crate::Mft::new) and `Journal::new` both take ownership
/// of one, and reopen the volume by its path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Volume {
    path: PathBuf,
    cluster_size: u64,
    volume_size: u64,
    file_record_size: u64,
    mft_position: u64,
}

impl Volume {
    /// Opens the volume at `path` and reads its boot sector. `path` is a Win32 device path such
    /// as `\\.\C:` or `\\?\C:`, or an ordinary file containing a flat NTFS volume image with
    /// its boot sector at byte zero. Partition tables and image-container metadata are not parsed.
    ///
    /// Opening a raw volume needs an elevated (administrator) process; without one this fails
    /// with [`NtfsReaderError::AccessDenied`]. So does a path naming a directory (`C:\`) instead
    /// of the volume (`\\.\C:`), elevated or not. A path with a NUL character fails with
    /// [`NtfsReaderError::InvalidVolumePath`], before anything is opened.
    ///
    /// The boot sector's file-system size must fit within the same handle's device length
    /// (`IOCTL_DISK_GET_LENGTH_INFO`), or its logical file length for an ordinary image file.
    /// Otherwise this returns
    /// [`NtfsReaderError::InvalidBootSector`] for `total_sectors`. Unused space after the file
    /// system is allowed. If the length cannot be queried, construction fails with an I/O error.
    pub fn new<P: AsRef<Path>>(path: P) -> NtfsReaderResult<Self> {
        // A NUL ends a Windows path string early, so a path holding one would name a shorter
        // path than the caller wrote (and `Journal::new` reopens the volume by this path).
        if path.as_ref().as_os_str().as_encoded_bytes().contains(&0) {
            return Err(NtfsReaderError::InvalidVolumePath);
        }
        // `NtfsReaderError::from(io::Error)` maps a refused open to `AccessDenied`.
        let file = File::open(path.as_ref())?;
        let backing_size = backing_length(&file)?;
        Self::from_reader(path.as_ref(), AlignedReader::new(file, 4096)?, backing_size)
    }

    // The construction path is shared with synthetic tests; only opening the handle and
    // obtaining a raw device's length need Windows.
    fn from_reader(
        path: &Path,
        mut reader: impl Read,
        backing_size: u64,
    ) -> NtfsReaderResult<Self> {
        let mut boot_sector_bytes = [0u8; size_of::<BootSector>()];
        reader.read_exact(&mut boot_sector_bytes)?;
        let boot_sector = decode_boot_sector(&boot_sector_bytes)?;
        let VolumeGeometry {
            cluster_size,
            volume_size,
            file_record_size,
            mft_position,
        } = VolumeGeometry::from_boot_sector(&boot_sector)?;

        // The device includes trailing space outside the file system (one sector on the
        // measured live volumes and shadows). Keep the file-system boundary, but never
        // allow an untrusted boot sector to inflate it past the backing device or image file.
        if volume_size > backing_size {
            return Err(NtfsReaderError::InvalidBootSector {
                field: "total_sectors",
            });
        }

        Ok(Volume {
            path: path.into(),
            cluster_size,
            volume_size,
            file_record_size,
            mft_position,
        })
    }

    /// The path the volume was opened with. It holds no NUL character.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Size of a cluster in bytes.
    pub fn cluster_size(&self) -> u64 {
        self.cluster_size
    }

    /// File-system size in bytes from the boot sector, checked against the device or image length by
    /// [`Self::new`]. Excludes any unused space after the file system.
    pub fn volume_size(&self) -> u64 {
        self.volume_size
    }

    /// Size of one MFT record in bytes (usually 1024).
    pub fn file_record_size(&self) -> u64 {
        self.file_record_size
    }

    /// Byte offset of the start of the `$MFT` from the start of the volume.
    pub fn mft_position(&self) -> u64 {
        self.mft_position
    }

    /// A `Volume` not read from a boot sector, for unit tests and benches that never touch a
    /// real volume. A `volume_size` of 0 means "unknown, do not bound sizes by it".
    #[cfg(any(test, feature = "internals"))]
    #[doc(hidden)]
    pub fn synthetic(
        path: impl Into<PathBuf>,
        cluster_size: u64,
        volume_size: u64,
        file_record_size: u64,
        mft_position: u64,
    ) -> Self {
        Volume {
            path: path.into(),
            cluster_size,
            volume_size,
            file_record_size,
            mft_position,
        }
    }

    /// Replaces the volume size of a synthetic volume.
    #[cfg(any(test, feature = "internals"))]
    #[doc(hidden)]
    pub fn with_volume_size(mut self, volume_size: u64) -> Self {
        self.volume_size = volume_size;
        self
    }
}

#[cfg(test)]
#[path = "tests/volume.rs"]
mod tests;
