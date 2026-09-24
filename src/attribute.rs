// Copyright (c) 2022, Matteo Bernacchia <dev@kikijiki.com>. All rights reserved.
// This project is dual licensed under the Apache License 2.0 and the MIT license.
// See the LICENSE files in the project root for details.

//! [`NtfsAttribute`]: a typed view over one attribute of an MFT record.

use std::ffi::OsString;
use std::fmt;
use std::mem::size_of;

use crate::{
    api::*,
    data_run::DataRun,
    errors::{NtfsReaderError, NtfsReaderResult},
    volume::Volume,
};

/// One attribute of an MFT record, borrowed from the record's bytes. Get them
/// from [`NtfsFile::attributes`](crate::NtfsFile::attributes) (every
/// attribute of the file) or
/// [`NtfsFile::record_attributes`](crate::NtfsFile::record_attributes)
/// (one record only). Most callers want the file-level accessors
/// (`names`, `data_streams`, `standard_information`) instead.
pub struct NtfsAttribute<'a> {
    data: &'a [u8],
    pub(crate) header: &'a NtfsAttributeHeader,
    length: usize,
}

impl fmt::Debug for NtfsAttribute<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NtfsAttribute")
            .field("type", &self.attribute_type())
            .field("resident", &self.is_resident())
            .field("name", &self.name())
            .field("length", &self.length)
            .finish()
    }
}

impl<'a> NtfsAttribute<'a> {
    /// The attribute at the start of `data`, or `None` if `data` is too short
    /// for a header or the declared length is not a plausible one: shorter
    /// than the common header, not a multiple of 8 (NTFS pads every attribute
    /// to 8 bytes), or longer than `data`.
    pub(crate) fn new(data: &'a [u8]) -> Option<Self> {
        if data.len() < size_of::<NtfsAttributeHeader>() {
            return None;
        }

        // SAFETY: `data` holds a whole header (checked above), and the header is a packed
        // struct of plain integers (alignment 1).
        let header = unsafe { &*(data.as_ptr() as *const NtfsAttributeHeader) };
        let length = header.length as usize;
        if length < size_of::<NtfsAttributeHeader>() || length & 7 != 0 || length > data.len() {
            return None;
        }

        Some(Self {
            data,
            header,
            length,
        })
    }

    /// The attribute's declared length in bytes (header and value).
    pub(crate) fn len(&self) -> usize {
        self.length
    }

    /// The attribute's bytes, header included.
    pub(crate) fn data(&self) -> &'a [u8] {
        &self.data[..self.length]
    }

    /// `None` for a type this crate doesn't know.
    pub fn attribute_type(&self) -> Option<NtfsAttributeType> {
        self.header.type_id.try_into().ok()
    }

    /// Whether the value is stored in the record itself (`true`) or in
    /// clusters elsewhere on the volume, described by data runs (`false`).
    pub fn is_resident(&self) -> bool {
        self.header.is_non_resident == 0
    }

    /// The attribute's own name, e.g. the stream name of a named `$DATA`
    /// attribute. `None` when unnamed or when the name lies outside the attribute.
    ///
    /// Lossless, like [`NtfsFileName::to_os_string`]: the name is UTF-16 with
    /// no guarantee of being valid, so it is an `OsString` and can be used to
    /// open `path:stream` again.
    pub fn name(&self) -> Option<OsString> {
        let length = self.header.name_length as usize;
        if length == 0 {
            return None;
        }
        let start = self.header.name_offset as usize;
        let bytes = self.data().get(start..start + length * 2)?;
        let units: Vec<u16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .copied()
            .map(u16::from_le_bytes)
            .collect();
        Some(utf16_to_os_string(&units))
    }

    /// Logical size of the attribute's value: the resident length, or the
    /// `data_size` of a non-resident one. `None` for the later extents of a
    /// value split across records, which don't carry the size.
    pub fn value_size(&self) -> Option<u64> {
        match self.nonresident_header() {
            Some(header) => (header.lowest_vcn == 0).then_some(header.data_size),
            None => Some(self.resident_header()?.value_length as u64),
        }
    }

    pub(crate) fn resident_header(&self) -> Option<&'a NtfsResidentAttributeHeader> {
        if !self.is_resident() {
            return None;
        }
        if self.length < size_of::<NtfsResidentAttributeHeader>() {
            return None;
        }
        // SAFETY: `length` is at most `data.len()` (see `new`) and holds a whole header (checked
        // above); the header is a packed struct of plain integers (alignment 1).
        Some(unsafe { &*(self.data.as_ptr() as *const NtfsResidentAttributeHeader) })
    }

    pub(crate) fn nonresident_header(&self) -> Option<&'a NtfsNonResidentAttributeHeader> {
        if self.is_resident() {
            return None;
        }
        if self.length < size_of::<NtfsNonResidentAttributeHeader>() {
            return None;
        }
        // SAFETY: `length` is at most `data.len()` (see `new`) and holds a whole header (checked
        // above); the header is a packed struct of plain integers (alignment 1).
        Some(unsafe { &*(self.data.as_ptr() as *const NtfsNonResidentAttributeHeader) })
    }

    /// The value of a resident attribute, whatever its type. `None` for a
    /// non-resident attribute, or if the value lies outside the attribute.
    pub fn resident(&self) -> Option<&'a [u8]> {
        let header = self.resident_header()?;
        let start = header.value_offset as usize;
        let value_length = header.value_length as usize;
        let end = start.checked_add(value_length)?;
        if end > self.data().len() {
            return None;
        }
        Some(&self.data()[start..end])
    }

    /// The `$STANDARD_INFORMATION` value, if this is one (and complete).
    pub fn standard_information(&self) -> Option<NtfsStandardInformation> {
        if self.attribute_type() != Some(NtfsAttributeType::StandardInformation) {
            return None;
        }
        let slice = self.resident()?;
        if slice.len() < size_of::<NtfsStandardInformation>() {
            return None;
        }
        // SAFETY: `slice` holds at least `size_of::<NtfsStandardInformation>()`
        // bytes, and every bit pattern is a valid value of that plain-integer
        // packed struct.
        Some(unsafe { std::ptr::read_unaligned(slice.as_ptr() as *const NtfsStandardInformation) })
    }

    /// The `$FILE_NAME` value, if this is one (and complete). `None` also for
    /// a name that runs past the attribute.
    pub fn file_name(&self) -> Option<NtfsFileName> {
        if self.attribute_type() != Some(NtfsAttributeType::FileName) {
            return None;
        }
        let slice = self.resident()?;
        if slice.len() < size_of::<NtfsFileNameHeader>() {
            return None;
        }

        // SAFETY: `slice` holds a whole header (checked above), and the header is a packed
        // `Copy` struct of plain integers (alignment 1), so it is copied out as is.
        let header = unsafe { *(slice.as_ptr() as *const NtfsFileNameHeader) };
        let name_bytes = (header.name_length as usize).checked_mul(2)?;
        let header_size = size_of::<NtfsFileNameHeader>();
        let end = header_size.checked_add(name_bytes)?;
        if end > slice.len() {
            return None;
        }

        let char_count = header.name_length as usize;
        if char_count > 255 {
            return None;
        }

        let mut data = [0u16; 255];
        if char_count > 0 {
            let bytes = &slice[header_size..end];
            for (i, slot) in data.iter_mut().take(char_count).enumerate() {
                let byte_index = i * 2;
                *slot = u16::from_le_bytes([bytes[byte_index], bytes[byte_index + 1]]);
            }
        }

        Some(NtfsFileName { header, data })
    }

    /// The value of a resident `$DATA` attribute (of any stream). `None` if
    /// this is not a `$DATA` attribute or it is non-resident.
    pub fn resident_data(&self) -> Option<&'a [u8]> {
        if self.attribute_type() != Some(NtfsAttributeType::Data) {
            return None;
        }
        self.resident()
    }

    /// Decodes one non-resident extent's data runs, without checking them
    /// against `data_size`: a later extent of a value split across
    /// extension records carries `data_size == 0` on disk, so only the
    /// VCN-0 extent's `data_size` means anything. Callers reading a single,
    /// self-contained attribute should use [`Self::nonresident_data_runs`]
    /// instead, which does that check.
    pub(crate) fn nonresident_extent_runs(
        &self,
        volume: &Volume,
    ) -> NtfsReaderResult<Vec<DataRun>> {
        let header_nonres = self
            .nonresident_header()
            .ok_or(NtfsReaderError::InvalidDataRun {
                details: "attribute is resident",
            })?;

        let mut out = Vec::new();

        let start = header_nonres.data_runs_offset as usize;
        if start > self.length {
            return Err(NtfsReaderError::InvalidDataRun {
                details: "data runs offset outside attribute",
            });
        }
        let runs_data = &self.data()[start..];

        let cluster_size = volume.cluster_size();
        const BUF_SIZE: usize = 8;

        let mut cursor = 0usize;
        let mut prev_offset = 0i128;
        // Only guards against a corrupt/overflowing run stream; the caller
        // checks the concatenated total against the declared size.
        let mut total_run_length = 0u64;
        loop {
            if cursor >= runs_data.len() {
                return Err(NtfsReaderError::InvalidDataRun {
                    details: "unterminated data run sequence",
                });
            }
            if runs_data[cursor] == 0 {
                break;
            }

            let descriptor = runs_data[cursor];
            let cluster_count_b = (descriptor & 0x0f) as usize;
            let cluster_offset_b = ((descriptor & 0xf0) >> 4) as usize;

            if cluster_count_b == 0 || cluster_count_b > BUF_SIZE {
                return Err(NtfsReaderError::InvalidDataRun {
                    details: "invalid cluster count field",
                });
            }
            if cluster_offset_b > BUF_SIZE {
                return Err(NtfsReaderError::InvalidDataRun {
                    details: "invalid cluster offset field",
                });
            }

            cursor += 1;

            if cursor + cluster_count_b > runs_data.len() {
                return Err(NtfsReaderError::InvalidDataRun {
                    details: "unexpected end of run-length data",
                });
            }
            let mut count_buf = [0u8; BUF_SIZE];
            count_buf[..cluster_count_b]
                .copy_from_slice(&runs_data[cursor..cursor + cluster_count_b]);
            let cluster_count = u64::from_le_bytes(count_buf);
            if cluster_count == 0 {
                return Err(NtfsReaderError::InvalidDataRun {
                    details: "cluster count is zero",
                });
            }
            cursor += cluster_count_b;

            let run_length_bytes =
                cluster_count
                    .checked_mul(cluster_size)
                    .ok_or(NtfsReaderError::InvalidDataRun {
                        details: "run length overflow",
                    })?;
            total_run_length = total_run_length.checked_add(run_length_bytes).ok_or(
                NtfsReaderError::InvalidDataRun {
                    details: "total run length overflow",
                },
            )?;

            let run_offset = if cluster_offset_b == 0 {
                None
            } else {
                if cursor + cluster_offset_b > runs_data.len() {
                    return Err(NtfsReaderError::InvalidDataRun {
                        details: "unexpected end of run-offset data",
                    });
                }
                let mut offset_buf = [0u8; BUF_SIZE];
                offset_buf[..cluster_offset_b]
                    .copy_from_slice(&runs_data[cursor..cursor + cluster_offset_b]);
                let raw = i64::from_le_bytes(offset_buf);
                let empty_bits = (BUF_SIZE - cluster_offset_b) * 8;
                let cluster_offset = (raw << empty_bits) >> empty_bits;
                cursor += cluster_offset_b;

                let delta = (cluster_offset as i128)
                    .checked_mul(cluster_size as i128)
                    .ok_or(NtfsReaderError::InvalidDataRun {
                        details: "relative offset overflow",
                    })?;
                let start =
                    prev_offset
                        .checked_add(delta)
                        .ok_or(NtfsReaderError::InvalidDataRun {
                            details: "relative offset overflow",
                        })?;
                if start < 0 {
                    return Err(NtfsReaderError::InvalidDataRun {
                        details: "relative offset underflow",
                    });
                }
                prev_offset = start;
                Some(
                    u64::try_from(start).map_err(|_| NtfsReaderError::InvalidDataRun {
                        details: "run offset out of range",
                    })?,
                )
            };

            match run_offset {
                Some(start) => out.push(DataRun::Data {
                    offset: start,
                    length: run_length_bytes,
                }),
                None => out.push(DataRun::Sparse {
                    length: run_length_bytes,
                }),
            }
        }

        Ok(out)
    }

    /// Decodes a single, self-contained non-resident attribute's data runs
    /// and checks them against its own `data_size`. A value split across
    /// extension records needs the crate's own internal extension-record
    /// walk instead, which concatenates every extent before checking the
    /// total.
    pub(crate) fn nonresident_data_runs(
        &self,
        volume: &Volume,
    ) -> NtfsReaderResult<(u64, Vec<DataRun>)> {
        let header_nonres = self
            .nonresident_header()
            .ok_or(NtfsReaderError::InvalidDataRun {
                details: "attribute is resident",
            })?;

        let total_size = header_nonres.data_size;
        if total_size == 0 {
            return Ok((total_size, Vec::new()));
        }

        let out = self.nonresident_extent_runs(volume)?;

        if out.is_empty() {
            return Err(NtfsReaderError::InvalidDataRun {
                details: "attribute has size but no runs",
            });
        }

        let total_run_length: u64 = out
            .iter()
            .map(|run| match run {
                DataRun::Data { length, .. } | DataRun::Sparse { length } => *length,
            })
            .try_fold(0u64, |acc, length| acc.checked_add(length))
            .ok_or(NtfsReaderError::InvalidDataRun {
                details: "total run length overflow",
            })?;

        if total_run_length < total_size {
            return Err(NtfsReaderError::InvalidDataRun {
                details: "data runs shorter than declared size",
            });
        }

        Ok((total_size, out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 64 bytes holding a `$DATA` attribute header that claims `length`.
    fn attribute_claiming(length: u32) -> Vec<u8> {
        let mut bytes = vec![0u8; 64];
        bytes[0..4].copy_from_slice(&(NtfsAttributeType::Data as u32).to_le_bytes());
        bytes[4..8].copy_from_slice(&length.to_le_bytes());
        bytes
    }

    // NTFS pads every attribute to 8 bytes and the common header alone is 16.
    // A length outside that (or past the bytes given) is corrupt, not an
    // attribute to step over a few bytes at a time.
    #[test]
    fn an_attribute_needs_a_plausible_length() {
        let cases = [
            (0, false),
            (1, false),
            (4, false),
            (8, false),
            (12, false),
            (16, true),
            (20, false),
            (24, true),
            (64, true),
            (72, false),
            (u32::MAX, false),
        ];
        for (length, plausible) in cases {
            let bytes = attribute_claiming(length);
            assert_eq!(
                NtfsAttribute::new(&bytes).is_some(),
                plausible,
                "length {length}"
            );
        }
        assert!(NtfsAttribute::new(&attribute_claiming(16)[..15]).is_none());
    }
}
