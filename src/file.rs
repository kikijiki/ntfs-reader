// Copyright (c) 2022, Matteo Bernacchia <dev@kikijiki.com>. All rights reserved.
// This project is dual licensed under the Apache License 2.0 and the MIT license.
// See the LICENSE files in the project root for details.

//! [`NtfsFile`]: one MFT record and the accessors for the logical file it
//! belongs to.

use std::ffi::OsString;
use std::fmt;
use std::mem::size_of;

use crate::{api::*, attribute::NtfsAttribute, mft::Mft};

/// One `$DATA` stream of a file: the default (unnamed) stream holds the
/// file contents, named ones are alternate data streams.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct NtfsDataStream {
    /// `None` for the default stream. Lossless, so `path:stream` built from it
    /// opens the stream even when the name is not valid UTF-16.
    pub name: Option<OsString>,
    /// Logical size in bytes.
    pub size: u64,
}

impl NtfsDataStream {
    /// `None` unless `attribute` is a `$DATA` attribute carrying the stream
    /// size, i.e. resident or the first extent (lowest VCN 0) of a
    /// non-resident stream split across records.
    fn from_attribute(attribute: &NtfsAttribute) -> Option<Self> {
        if attribute.attribute_type() != Some(NtfsAttributeType::Data) {
            return None;
        }
        let name = attribute.name();
        // A name that is declared but cannot be read makes the attribute
        // corrupt; it must not pass for the default stream.
        if name.is_none() && attribute.header.name_length != 0 {
            return None;
        }
        Some(Self {
            size: attribute.value_size()?,
            name,
        })
    }
}

/// One validated MFT record's bytes, not tied to any [`Mft`]: what the loader
/// and `$MFT` bootstrap read before an `Mft` exists. [`NtfsFile`] is a
/// `Record` plus the `Mft` it came from.
#[derive(Clone, Copy)]
pub(crate) struct Record<'a> {
    number: u64,
    pub(crate) header: &'a NtfsFileRecordHeader,
    data: &'a [u8],
}

impl<'a> Record<'a> {
    /// The record `number` held in `data` (one whole record, update sequence
    /// fixups already applied), or `None` if `data` does not look like a
    /// file record. See [`Self::is_valid`].
    pub(crate) fn new(number: u64, data: &'a [u8]) -> Option<Self> {
        if !Self::is_valid(data) {
            return None;
        }
        // SAFETY: `is_valid` checked `data` holds a whole header, and the
        // header is a packed struct of plain integers (alignment 1).
        let header = unsafe { &*(data.as_ptr() as *const NtfsFileRecordHeader) };
        Some(Record {
            number,
            header,
            data,
        })
    }

    /// Whether `data` (one whole record) has a plausible header: the `FILE`
    /// signature, a complete update sequence array (the sequence number plus
    /// one saved value for each 512-byte sector, the way Windows requires),
    /// and a used size and attribute offset inside the record.
    pub(crate) fn is_valid(data: &[u8]) -> bool {
        if data.len() < size_of::<NtfsFileRecordHeader>() {
            return false;
        }
        // SAFETY: `data` holds a whole header (checked above), and the header is a packed
        // struct of plain integers (alignment 1).
        let header = unsafe { &*(data.as_ptr() as *const NtfsFileRecordHeader) };
        if &header.signature != FILE_RECORD_SIGNATURE {
            return false;
        }

        // A short array would leave the last sectors' ends unrestored.
        if header.update_sequence_length as usize != data.len() / SECTOR_SIZE + 1 {
            return false;
        }

        if header.used_size as usize > data.len() {
            return false;
        }

        let usa_end =
            header.update_sequence_offset as usize + header.update_sequence_length as usize * 2;
        if usa_end > data.len() {
            return false;
        }

        if header.attributes_offset as usize >= header.used_size as usize {
            return false;
        }

        true
    }

    pub(crate) fn number(&self) -> u64 {
        self.number
    }

    pub(crate) fn reference(&self) -> u64 {
        let seq = self.header.sequence_value as u64;
        (seq << 48) | (self.number & 0x0000_FFFF_FFFF_FFFF)
    }

    pub(crate) fn base_reference(&self) -> Option<u64> {
        let reference = self.header.base_reference;
        (reference != 0).then_some(reference)
    }

    pub(crate) fn base_number(&self) -> Option<u64> {
        self.base_reference()
            .map(|reference| reference & 0x0000_FFFF_FFFF_FFFF)
    }

    pub(crate) fn is_used(&self) -> bool {
        self.header.flags & NtfsFileFlags::InUse as u16 != 0
    }

    pub(crate) fn is_directory(&self) -> bool {
        self.header.flags & NtfsFileFlags::IsDirectory as u16 != 0
    }

    pub(crate) fn attributes(&self) -> impl Iterator<Item = NtfsAttribute<'a>> + use<'a> {
        let data = self.data;
        let used = usize::min(self.header.used_size as usize, data.len());
        let mut offset = self.header.attributes_offset as usize;

        std::iter::from_fn(move || {
            let attribute = NtfsAttribute::new(data.get(offset..used)?)?;
            if attribute.attribute_type() == Some(NtfsAttributeType::End) {
                return None;
            }
            // `NtfsAttribute::new` guarantees 0 < len <= used - offset.
            offset += attribute.len();
            Some(attribute)
        })
    }
}

/// One MFT record, borrowed from a loaded [`Mft`]. Despite the name it is a
/// record, not necessarily a whole file: a file whose attributes overflow its
/// base record has extension records too. [`Self::records`], [`Self::attributes`],
/// [`Self::names`], [`Self::data_streams`] and the other accessors that
/// return a file's contents cover the whole logical file; the ones that
/// describe the record itself ([`Self::number`], [`Self::is_used`],
/// [`Self::record_attributes`] and so on) look at this record only.
///
/// Get one from [`Mft::files`] or [`Mft::record`]. The values the accessors
/// return borrow from the [`Mft`], not from the `NtfsFile`, so they can
/// outlive it.
pub struct NtfsFile<'a> {
    pub(crate) record: Record<'a>,
    mft: &'a Mft,
}

impl fmt::Debug for NtfsFile<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NtfsFile")
            .field("number", &self.number())
            .field("reference", &format_args!("{:#x}", self.reference()))
            .field("used", &self.is_used())
            .field("directory", &self.is_directory())
            .field("extension", &self.is_extension())
            .finish()
    }
}

impl<'a> NtfsFile<'a> {
    /// The record `number` held in `data` (which must be that record's bytes
    /// in `mft`), or `None` if `data` is not a valid file record.
    pub(crate) fn new(mft: &'a Mft, number: u64, data: &'a [u8]) -> Option<Self> {
        Some(NtfsFile {
            record: Record::new(number, data)?,
            mft,
        })
    }

    /// The [`Mft`] this record belongs to.
    pub(crate) fn mft(&self) -> &'a Mft {
        self.mft
    }

    /// The record number: its index in the `$MFT`.
    pub fn number(&self) -> u64 {
        self.record.number()
    }

    /// The record number plus the record's sequence number in the top 16 bits.
    /// The sequence number changes every time the record is freed and reused,
    /// so a reference held elsewhere (a parent reference in a name, a journal
    /// file id) identifies the record only while it still matches.
    pub fn reference(&self) -> u64 {
        self.record.reference()
    }

    /// The reference of the base record, if this is an extension record.
    /// `None` for a base record.
    pub fn base_reference(&self) -> Option<u64> {
        self.record.base_reference()
    }

    /// The record number of the base record, if this is an extension record.
    /// `None` for a base record.
    pub fn base_number(&self) -> Option<u64> {
        self.record.base_number()
    }

    /// Whether this is an extension record: it holds attributes of another
    /// (base) record's file and is never a file by itself.
    pub fn is_extension(&self) -> bool {
        self.base_reference().is_some()
    }

    /// The file's id, the same value a USN journal record for this file carries.
    pub fn file_id(&self) -> FileId {
        FileId::from(self.reference())
    }

    /// Whether the record is in use (its `InUse` flag is set).
    pub fn is_used(&self) -> bool {
        self.record.is_used()
    }

    /// Whether the record is a directory. Extension records never say: ask
    /// the base record.
    pub fn is_directory(&self) -> bool {
        self.record.is_directory()
    }

    /// The base MFT record and all live extension records that belong to the
    /// same logical file as this one (which may be either), base record first.
    pub fn records(&self) -> impl Iterator<Item = NtfsFile<'a>> + use<'a> {
        let mft = self.mft;
        let base_number = self.base_number().unwrap_or(self.number());
        let base_reference = mft.record(base_number).map(|base| base.reference());

        std::iter::once(base_number)
            .chain(mft.extension_record_numbers(base_number))
            .filter(move |number| mft.is_allocated(*number))
            .filter_map(move |number| mft.record(number))
            .filter(move |record| {
                record.is_used()
                    && (record.number() == base_number || record.base_reference() == base_reference)
            })
    }

    /// Attributes stored in this record only. A file whose attributes
    /// overflow into extension records needs [`Self::attributes`] instead.
    pub fn record_attributes(&self) -> impl Iterator<Item = NtfsAttribute<'a>> + use<'a> {
        self.record.attributes()
    }

    /// Attributes of the whole file, across the base and all extension
    /// records (see [`Self::records`]).
    pub fn attributes(&self) -> impl Iterator<Item = NtfsAttribute<'a>> + use<'a> {
        self.records().flat_map(|record| record.record_attributes())
    }

    /// Every `$FILE_NAME` attribute, including DOS 8.3 aliases.
    /// See [`Self::hard_links`] for names that each count as a link.
    pub fn names(&self) -> impl Iterator<Item = NtfsFileName> + use<'a> {
        self.attributes()
            .filter_map(|attribute| attribute.file_name())
    }

    /// One name per hard link, as `FindFirstFileNameW` would list them.
    /// Don't use the record's `link_count` for this: it includes DOS aliases
    /// and can be higher than the number of names actually present.
    /// Use [`Mft::resolve_path`] to turn each one into a full path.
    pub fn hard_links(&self) -> impl Iterator<Item = NtfsFileName> + use<'a> {
        self.names().filter(|name| !name.is_dos_alias())
    }

    /// The name to display for the file: a Win32 name when there is one,
    /// otherwise the first name seen (which can be a reparse point, such as
    /// a junction or symlink; those still need a name and a path). Use
    /// [`Self::hard_links`] to see every name that counts as a hard link.
    pub fn best_name(&self) -> Option<NtfsFileName> {
        let mut fallback = None;
        for name in self.names() {
            if matches!(
                name.namespace(),
                Some(NtfsFileNamespace::Win32 | NtfsFileNamespace::Win32AndDos)
            ) {
                return Some(name);
            }
            fallback.get_or_insert(name);
        }
        fallback
    }

    /// The file's `$STANDARD_INFORMATION`: its timestamps and attribute flags.
    pub fn standard_information(&self) -> Option<NtfsStandardInformation> {
        self.attributes()
            .find_map(|attribute| attribute.standard_information())
    }

    /// Every `$DATA` stream: the default (unnamed) one and any alternate
    /// streams, in record order. That is not necessarily default-first (an
    /// alternate stream in the base record can precede the default stream's
    /// extent in an extension record); use `name.is_none()` to identify the
    /// default stream rather than list position.
    pub fn data_streams(&self) -> impl Iterator<Item = NtfsDataStream> + use<'a> {
        self.attributes()
            .filter_map(|attribute| NtfsDataStream::from_attribute(&attribute))
    }

    /// Contents of the default stream when small enough to be stored inside
    /// the MFT. `None` for a non-resident stream.
    pub fn resident_data(&self) -> Option<&'a [u8]> {
        self.attributes()
            .find(|attribute| {
                attribute.attribute_type() == Some(NtfsAttributeType::Data)
                    && attribute.header.name_length == 0
            })?
            .resident_data()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_info::FileInfo;
    use crate::mft::test_records::*;

    const REPARSE: u32 = NtfsFileNameFlags::ReparsePoint as u32;

    fn attribute_types(record: &[u8]) -> Vec<Option<NtfsAttributeType>> {
        Record::new(FIRST_NORMAL_RECORD, record)
            .expect("valid record")
            .attributes()
            .take(10)
            .map(|attribute| attribute.attribute_type())
            .collect()
    }

    fn best_name(mft: &Mft, number: u64) -> Option<String> {
        let file = mft.record(number).expect("record");
        file.best_name().map(|name| name.to_string())
    }

    // Card 013. A junction or symlink has only names flagged as reparse
    // points; it still needs a name.
    #[test]
    fn best_name_includes_reparse_point_names() {
        let mut junction = new_record(24, 1, 0);
        set_record_flags(&mut junction, directory_flags());
        let offset = add_file_name(&mut junction, ATTRIBUTES_OFFSET, "junction", REPARSE);
        finish_record(&mut junction, offset);

        let mut link = new_record(25, 1, 0);
        let mut offset = ATTRIBUTES_OFFSET;
        for (id, namespace, name) in [
            (1, NtfsFileNamespace::Posix, "posix-link.txt"),
            (2, NtfsFileNamespace::Win32, "file-link.txt"),
        ] {
            offset = add_file_name_ex(&mut link, offset, id, ROOT_RECORD, namespace, name, REPARSE);
        }
        finish_record(&mut link, offset);

        let mft = mft_with(vec![junction, link]);
        assert_eq!(best_name(&mft, 24).as_deref(), Some("junction"));
        assert_eq!(best_name(&mft, 25).as_deref(), Some("file-link.txt"));

        for file in mft.files() {
            let best = file.best_name().expect("best name").to_string();
            assert!(
                file.hard_links().any(|link| link.to_string() == best),
                "best_name {best} is not one of the hard links",
            );
            assert_eq!(FileInfo::new(&file).name, best);
        }
    }

    // Card 016: best_name falls back to the first Posix name, and a Win32
    // name wins over an earlier Posix one.
    #[test]
    fn best_name_falls_back_to_first_posix_name() {
        let mut posix_only = new_record(24, 1, 0);
        let mut offset = ATTRIBUTES_OFFSET;
        for (id, name) in [(1, "posix-a"), (2, "posix-b")] {
            offset = add_file_name_ex(
                &mut posix_only,
                offset,
                id,
                ROOT_RECORD,
                NtfsFileNamespace::Posix,
                name,
                0,
            );
        }
        finish_record(&mut posix_only, offset);

        let mut mixed = new_record(25, 1, 0);
        let mut offset = ATTRIBUTES_OFFSET;
        for (id, namespace, name) in [
            (1, NtfsFileNamespace::Posix, "posix"),
            (2, NtfsFileNamespace::Win32, "win32"),
        ] {
            offset = add_file_name_ex(&mut mixed, offset, id, ROOT_RECORD, namespace, name, 0);
        }
        finish_record(&mut mixed, offset);

        let mft = mft_with(vec![posix_only, mixed]);
        assert_eq!(best_name(&mft, 24).as_deref(), Some("posix-a"));
        assert_eq!(best_name(&mft, 25).as_deref(), Some("win32"));
    }

    // A record whose attribute claims an implausible length (zero, shorter than
    // the attribute header, or not a multiple of 8) ends the walk. Walked a byte
    // at a time, its remaining bytes would come out as phantom attributes, and a
    // zero length must not loop (card 016).
    #[test]
    fn record_attributes_stop_at_an_implausible_attribute_length() {
        for length in [0u32, 1, 4, 8, 12, 20, 33] {
            let mut record = new_record(24, 1, 0);
            let offset = add_file_name(&mut record, ATTRIBUTES_OFFSET, "a.txt", 0);
            // Something that would parse as more attributes if the length
            // were skipped a few bytes at a time.
            for step in 0..8 {
                write_u32(
                    &mut record,
                    offset + step * 4,
                    NtfsAttributeType::Data as u32,
                );
            }
            write_u32(&mut record, offset + 4, length);
            finish_record(&mut record, offset + 64);

            assert_eq!(
                attribute_types(&record),
                [Some(NtfsAttributeType::FileName)],
                "attribute length {length}"
            );
        }
    }

    // Card 016: nothing past used_size is read, even without an End marker.
    #[test]
    fn record_attributes_stop_at_used_size_without_end_marker() {
        let mut record = new_record(24, 1, 0);
        let used = add_file_name(&mut record, ATTRIBUTES_OFFSET, "a.txt", 0);
        add_resident_attribute(&mut record, used, NtfsAttributeType::Data, 2, "", b"x");
        finish_record(&mut record, used);

        assert_eq!(
            attribute_types(&record),
            [Some(NtfsAttributeType::FileName)]
        );
    }

    // Card 016: an attribute that crosses used_size is not returned.
    #[test]
    fn record_attributes_skip_attribute_crossing_used_size() {
        let mut record = new_record(24, 1, 0);
        let offset = add_file_name(&mut record, ATTRIBUTES_OFFSET, "a.txt", 0);
        let end = add_resident_attribute(
            &mut record,
            offset,
            NtfsAttributeType::Data,
            2,
            "",
            &[7; 80],
        );
        assert!(end - offset > 32);
        finish_record(&mut record, offset + 32);

        assert_eq!(
            attribute_types(&record),
            [Some(NtfsAttributeType::FileName)]
        );
    }

    // Card 016: the End marker stops the walk even when more bytes follow
    // inside used_size.
    #[test]
    fn record_attributes_stop_at_end_marker() {
        let mut record = new_record(24, 1, 0);
        let mut offset = add_file_name(&mut record, ATTRIBUTES_OFFSET, "a.txt", 0);
        offset = add_end_marker(&mut record, offset);
        offset = add_resident_attribute(&mut record, offset, NtfsAttributeType::Data, 2, "", b"x");
        finish_record(&mut record, offset);

        assert_eq!(
            attribute_types(&record),
            [Some(NtfsAttributeType::FileName)]
        );
    }

    // Card 016: split non-resident streams. Only the lowest-VCN-0 extent of
    // each stream carries the size; later extents in an extension record must
    // not add streams or override the size.
    #[test]
    fn data_streams_count_split_nonresident_streams_once() {
        let mut base = new_record(24, 1, 0);
        let mut offset = add_file_name(&mut base, ATTRIBUTES_OFFSET, "split.bin", 0);
        offset =
            add_nonresident_attribute(&mut base, offset, NtfsAttributeType::Data, 2, "", 0, 5000);
        offset =
            add_nonresident_attribute(&mut base, offset, NtfsAttributeType::Data, 3, "ads", 0, 700);
        finish_record(&mut base, offset);

        let mut extension = new_record(25, 1, reference(1, 24));
        let mut offset = ATTRIBUTES_OFFSET;
        offset = add_nonresident_attribute(
            &mut extension,
            offset,
            NtfsAttributeType::Data,
            4,
            "",
            8,
            123,
        );
        offset = add_nonresident_attribute(
            &mut extension,
            offset,
            NtfsAttributeType::Data,
            5,
            "ads",
            8,
            999,
        );
        finish_record(&mut extension, offset);

        let mft = mft_with(vec![base, extension]);
        let file = mft.files().next().expect("one file");
        assert_eq!(file.records().count(), 2);

        let streams: Vec<_> = file
            .data_streams()
            .map(|stream| (stream.name, stream.size))
            .collect();
        assert_eq!(streams, [(None, 5000), (Some("ads".into()), 700)]);
        assert_eq!(FileInfo::new(&file).size, 5000);
    }

    // Card 035 (found by the structure-aware mft_load target). A `$DATA`
    // attribute that has a name, but whose name lies outside the attribute, is
    // corrupt. It is not the default stream, and data_streams() must not
    // report it as one (`name: None`): FileInfo::size and resident_data() key
    // on the attribute's name length and already leave it out.
    #[test]
    fn data_streams_skip_a_named_stream_with_an_unreadable_name() {
        let mut record = new_record(24, 1, 0);
        let mut offset = add_file_name(&mut record, ATTRIBUTES_OFFSET, "f.txt", 0);
        let stream = offset;
        offset = add_resident_attribute(
            &mut record,
            offset,
            NtfsAttributeType::Data,
            2,
            "ads",
            &[7; 59],
        );
        write_u16(&mut record, stream + 10, 0xC3C3);
        finish_record(&mut record, offset);

        let mft = mft_with(vec![record]);
        let file = mft.files().next().expect("one file");

        let streams: Vec<_> = file.data_streams().collect();
        assert_eq!(streams, [], "a stream with an unreadable name was reported");
        assert_eq!(FileInfo::new(&file).size, 0);
        assert!(file.resident_data().is_none());
    }

    // Card 036. A stream name is a raw UTF-16 code unit sequence like a file
    // name, so an alternate data stream can be named with an unpaired
    // surrogate. data_streams() must report that name exactly, or a caller
    // cannot open `path:stream` with it.
    #[test]
    fn data_streams_report_a_stream_name_with_an_unpaired_surrogate_losslessly() {
        let name: Vec<u16> = "ads-"
            .encode_utf16()
            .chain([0xD800])
            .chain("-x".encode_utf16())
            .collect();
        let mut record = new_record(24, 1, 0);
        let mut offset = add_file_name(&mut record, ATTRIBUTES_OFFSET, "f.txt", 0);
        offset = add_resident_attribute_raw(
            &mut record,
            offset,
            NtfsAttributeType::Data,
            2,
            &name,
            &[7; 5],
        );
        finish_record(&mut record, offset);

        let mft = mft_with(vec![record]);
        let file = mft.files().next().expect("one file");

        let streams: Vec<_> = file.data_streams().collect();
        assert_eq!(streams.len(), 1, "{streams:?}");
        let expected_bytes: &[u8] = b"ads-\xED\xA0\x80-x";
        assert_os_str_is(
            streams[0].name.as_deref().expect("a named stream"),
            &name,
            expected_bytes,
        );
        assert_eq!(streams[0].size, 5);

        let attribute_name = file
            .attributes()
            .find(|attribute| attribute.attribute_type() == Some(NtfsAttributeType::Data))
            .and_then(|attribute| attribute.name());
        assert_os_str_is(
            attribute_name.as_deref().expect("a named attribute"),
            &name,
            expected_bytes,
        );
    }
}
