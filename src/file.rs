// Copyright (c) 2022, Matteo Bernacchia <dev@kikijiki.com>. All rights reserved.
// This project is dual licensed under the Apache License 2.0 and the MIT license.
// See the LICENSE files in the project root for details.

//! [`NtfsFile`]: one MFT record and the accessors for the logical file it
//! belongs to.

use std::ffi::OsString;
use std::fmt;
use std::mem::size_of;

use crate::{
    api::*,
    attribute::NtfsAttribute,
    mft::{Liveness, Mft},
};

/// One `$DATA` stream of a file: the default (unnamed) stream holds the file contents, named
/// ones are alternate data streams.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct NtfsDataStream {
    /// `None` for the default stream. Lossless, so `path:stream` built from it opens the stream
    /// even when the name is not valid UTF-16.
    pub name: Option<OsString>,
    /// Logical size in bytes.
    pub size: u64,
    /// Whether this stream's data was lost: the file is deleted, had an `$ATTRIBUTE_LIST`, and
    /// this stream is not resident. NTFS was seen to zero the size and data runs of every such
    /// stream on delete, so [`Self::size`] is 0 and the stream opens as an empty one, not what it
    /// was. A resident stream of the same file is intact. See [`NtfsFile::stream_data_lost`] for
    /// the whole file.
    pub data_lost: bool,
}

impl NtfsDataStream {
    /// `None` unless `attribute` is a `$DATA` attribute carrying the stream size: resident, or
    /// the first extent (lowest VCN 0) of a non-resident stream split across records.
    fn from_attribute(attribute: &NtfsAttribute, file_lost_data: bool) -> Option<Self> {
        if attribute.attribute_type() != Some(NtfsAttributeType::Data) {
            return None;
        }
        let name = attribute.name();
        // A name that is declared but unreadable makes the attribute corrupt; it must not pass
        // for the default stream.
        if name.is_none() && attribute.header.name_length != 0 {
            return None;
        }
        Some(Self {
            size: attribute.value_size()?,
            name,
            data_lost: file_lost_data && !attribute.is_resident(),
        })
    }
}

/// One validated MFT record's bytes, not tied to any [`Mft`]: what the loader and `$MFT`
/// bootstrap read before an `Mft` exists. [`NtfsFile`] is a `Record` plus the `Mft` it came from.
#[derive(Clone, Copy)]
pub(crate) struct Record<'a> {
    number: u64,
    pub(crate) header: &'a NtfsFileRecordHeader,
    data: &'a [u8],
}

impl<'a> Record<'a> {
    /// The record `number` held in `data` (one whole record, update sequence fixups already
    /// applied), or `None` if `data` does not look like a file record. See [`Self::is_valid`].
    pub(crate) fn new(number: u64, data: &'a [u8]) -> Option<Self> {
        if !Self::is_valid(data) {
            return None;
        }
        // SAFETY: `is_valid` checked `data` holds a whole header, and the header is a packed
        // struct of plain integers (alignment 1).
        let header = unsafe { &*(data.as_ptr() as *const NtfsFileRecordHeader) };
        Some(Record {
            number,
            header,
            data,
        })
    }

    /// Whether `data` (one whole record) has a plausible header: the `FILE` signature, a
    /// complete update sequence array (the sequence number plus one saved value per 512-byte
    /// sector, as Windows requires), and a used size and attribute offset inside the record.
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

        // A short array leaves the last sectors' ends unrestored.
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

    pub(crate) fn sequence(&self) -> u16 {
        self.header.sequence_value
    }

    pub(crate) fn reference(&self) -> u64 {
        (u64::from(self.sequence()) << 48) | (self.number & 0x0000_FFFF_FFFF_FFFF)
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

/// One MFT record, borrowed from a loaded [`Mft`]. Despite the name it is a record, not
/// necessarily a whole file: a file whose attributes overflow its base record has extension
/// records too. [`Self::records`], [`Self::attributes`], [`Self::names`], [`Self::data_streams`]
/// and similar accessors cover the whole logical file; [`Self::number`], [`Self::is_used`],
/// [`Self::record_attributes`] and similar describe this record only.
///
/// Get one from [`Mft::files`], [`Mft::deleted_files`], [`Mft::record`] or
/// [`Mft::record_by_id`]. Accessor return values borrow from the [`Mft`], not the `NtfsFile`, so
/// they can outlive it.
///
/// A deleted file is one whose base record is freed ([`Self::is_deleted`], [`Self::is_used`]
/// `false`); accessors read what it still holds, as for a live file. NTFS keeps everything but
/// three header fields on delete, so names, times and sizes usually survive. Best effort: a
/// record can be reused, and some deleted files lose their data run information (see
/// [`Self::data_streams`]).
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
    /// The record `number` held in `data` (which must be that record's bytes in `mft`), or `None`
    /// if `data` is not a valid file record.
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

    /// The record number plus the record's sequence number in the top 16 bits, as the header
    /// holds them. The sequence number changes each time the record is freed and reused, so a
    /// reference held elsewhere (a parent reference in a name, a journal file id) identifies the
    /// record only while it still matches.
    ///
    /// Freeing a record adds one to its sequence number, so for a deleted file this is **not**
    /// the reference the file had while live, and not what a journal record or a name's parent
    /// reference holds. Use [`Self::file_id`] for the id the file is known by, live or deleted.
    pub fn reference(&self) -> u64 {
        self.record.reference()
    }

    /// The reference of the base record, if this is an extension record. `None` for a base
    /// record.
    pub fn base_reference(&self) -> Option<u64> {
        self.record.base_reference()
    }

    /// The record number of the base record, if this is an extension record. `None` for a base
    /// record.
    pub fn base_number(&self) -> Option<u64> {
        self.record.base_number()
    }

    /// Whether this is an extension record: it holds attributes of another (base) record's file
    /// and is never a file by itself.
    pub fn is_extension(&self) -> bool {
        self.base_reference().is_some()
    }

    /// The file's id: the same value a USN journal record for this file carries, and the one
    /// [`Mft::record_by_id`] finds the record with, live or deleted.
    ///
    /// For a freed record (a deleted file's base or extension record, see [`Mft::deleted_files`])
    /// this is the id the file had while live: freeing adds one to the sequence number, and the
    /// id is the reference one sequence below that, matching a pre-delete journal record, so
    /// `mft.record_by_id(file.file_id())` finds `file` before and after deletion. At the
    /// sequence wrap (live 0xFFFF, freed to 0 as measured) the id has sequence 0xFFFF. A record neither live nor freed (in-use flag and `$BITMAP` bit disagree)
    /// has no id of its own: falls back to [`Self::reference`], the raw header value.
    pub fn file_id(&self) -> FileId {
        match self.mft.liveness(&self.record) {
            Some(Liveness::Freed) => FileId::from(Liveness::live_reference(&self.record)),
            _ => FileId::from(self.reference()),
        }
    }

    /// Whether the record is in use (its `InUse` flag is set). `false` for a deleted file (see
    /// [`Mft::deleted_files`]) and for a free extension record, but this is not the test for a
    /// deleted file: a freed extension record of a live file is also not in use, and a record
    /// whose flag and `$BITMAP` bit disagree is neither live nor deleted. Use
    /// [`Self::is_deleted`]. Every other accessor works on both.
    pub fn is_used(&self) -> bool {
        self.record.is_used()
    }

    /// Whether this record belongs to a deleted file: the file's base record is freed (in-use
    /// flag and `$BITMAP` bit both clear). The base decides: a freed extension record of a live
    /// file is not deleted; an extension record of a deleted file is, provided it names that
    /// incarnation of the base. A freed extension record left by an earlier file with the same
    /// record number, or one whose own base is itself an extension record, belongs to nobody. A
    /// record whose in-use flag and `$BITMAP` bit disagree (caught between the two writes, or
    /// corrupt) is neither live nor deleted: `false`.
    ///
    /// Not quite what [`Mft::deleted_files`] lists: that also requires the record be at or above
    /// 24 and hold `$STANDARD_INFORMATION` or `$FILE_NAME`, so a freed record below 24, or one
    /// holding neither, is deleted here but not listed there.
    ///
    /// A delete-pending file, deleted while something held it open and renamed under
    /// `$Extend\$Deleted`, is still in use and so is **not** deleted here: it is one of
    /// [`Mft::files`], not [`Mft::deleted_files`], until the last handle closes.
    pub fn is_deleted(&self) -> bool {
        let base_number = self.base_number().unwrap_or(self.number());
        let Some(base) = self.mft.record(base_number) else {
            return false;
        };
        // A record that names an extension record as its base belongs to no file.
        if base.is_extension() || self.mft.liveness(&base.record) != Some(Liveness::Freed) {
            return false;
        }
        match self.base_reference() {
            None => true,
            Some(reference) => {
                self.mft.reference_liveness(reference, &base.record) == Some(Liveness::Freed)
                    && self.mft.liveness(&self.record) == Some(Liveness::Freed)
            }
        }
    }

    /// Whether the record is a directory. Extension records never say: ask the base record.
    pub fn is_directory(&self) -> bool {
        self.record.is_directory()
    }

    /// The base MFT record and the extension records belonging to the same logical file as this
    /// one (which may be either), base record first.
    ///
    /// The base decides which extension records count. For a freed record (not in use, not
    /// allocated) with a freed base too, i.e. a deleted file, this returns the base plus the
    /// freed extension records naming it the way a freed record is named: base reference
    /// sequence one below its own, since freeing adds one and leaves other references unchanged.
    /// A freed extension record naming an older incarnation of the same record number (base
    /// reused since) has a different sequence and is left out.
    ///
    /// For an extension record, this answers only when its base reference names the base in its
    /// current state (exact reference if live, the freed rule above if freed); a stale freed
    /// extension record of an earlier file with the same number, or one whose base is itself an
    /// extension record, gets nothing.
    ///
    /// Otherwise this is a live file: the base if in use and allocated, plus live extension
    /// records whose base reference is the base's own, sequence included. A live record never
    /// sees a freed one, whatever it claims.
    ///
    /// Every whole-file accessor builds on this, so they work on a deleted file the same as a
    /// live one: see [`Mft::deleted_files`].
    pub fn records(&self) -> impl Iterator<Item = NtfsFile<'a>> + use<'a> {
        let mft = self.mft;
        let base_number = self.base_number().unwrap_or(self.number());
        let base = mft.record(base_number);
        let liveness = base.as_ref().and_then(|base| mft.liveness(&base.record));
        // An extension record belongs to a file only if its base reference names this incarnation
        // of the base, in the state the base is in, and the base is not itself an extension
        // record. A stale freed extension record of an earlier file with the same record number
        // names a different sequence and sees nothing, whatever state the base is in now.
        let belongs = match self.base_reference() {
            None => true,
            Some(reference) => base.as_ref().is_some_and(|base| {
                !base.is_extension() && mft.reference_liveness(reference, &base.record) == liveness
            }),
        };
        // A deleted file's view is only for a freed record that asks: a live extension record of
        // a freed base is still a live record, and what it sees must not come from freed ones.
        let freed = belongs
            && liveness == Some(Liveness::Freed)
            && mft.liveness(&self.record) == Some(Liveness::Freed);
        // The extension index is split by state, so a live file only looks at live extension
        // records and a deleted one at freed ones; the base reference then picks the ones naming
        // this incarnation of the base.
        let index = if freed {
            Liveness::Freed
        } else {
            Liveness::Live
        };
        let extensions = if belongs && base.is_some() {
            mft.extension_records(base_number, index)
        } else {
            &[]
        };
        let base_record = base.as_ref().map(|base| base.record);

        base.filter(|_| belongs && (liveness == Some(Liveness::Live) || freed))
            .into_iter()
            .chain(
                extensions
                    .iter()
                    .filter_map(move |&(_, number)| mft.record(number))
                    .filter(move |extension| {
                        let (Some(base), Some(reference)) =
                            (&base_record, extension.base_reference())
                        else {
                            return false;
                        };
                        if freed {
                            mft.reference_liveness(reference, base) == Some(Liveness::Freed)
                        } else {
                            reference == base.reference()
                        }
                    }),
            )
    }

    /// Attributes stored in this record only. A file whose attributes overflow into extension
    /// records needs [`Self::attributes`] instead.
    pub fn record_attributes(&self) -> impl Iterator<Item = NtfsAttribute<'a>> + use<'a> {
        self.record.attributes()
    }

    /// Attributes of the whole file, across the base and all extension records (see
    /// [`Self::records`]). For a deleted file, what its records still hold.
    pub fn attributes(&self) -> impl Iterator<Item = NtfsAttribute<'a>> + use<'a> {
        self.records().flat_map(|record| record.record_attributes())
    }

    /// Every `$FILE_NAME` attribute, including DOS 8.3 aliases. See [`Self::hard_links`] for
    /// names that each count as a link. A deleted file keeps the names its records held, but a
    /// name is lost if it was unlinked while the file lived: only the last name of a file with
    /// several hard links survives deletion, and the base record of a file with an
    /// `$ATTRIBUTE_LIST` has no name of its own.
    pub fn names(&self) -> impl Iterator<Item = NtfsFileName> + use<'a> {
        self.attributes()
            .filter_map(|attribute| attribute.file_name())
    }

    /// One name per hard link, as `FindFirstFileNameW` would list them. The record's `link_count`
    /// is not a substitute: it includes DOS aliases and can exceed the number of names present. Use [`Mft::resolve_path`] to turn each one into a full path, or
    /// [`Mft::resolve_deleted_path`] for a deleted file.
    pub fn hard_links(&self) -> impl Iterator<Item = NtfsFileName> + use<'a> {
        self.names().filter(|name| !name.is_dos_alias())
    }

    /// The name to display for the file: a Win32 name when there is one, otherwise the first
    /// name seen (which can be a reparse point, such as a junction or symlink; those still need a
    /// name and a path). Use [`Self::hard_links`] to see every name that counts as a hard link.
    /// `None` for a deleted file that kept no name (see [`Self::names`]). A directory deleted
    /// with `remove_dir_all` was renamed to a random name first and keeps that name, as does a
    /// file deleted while something held it open. For the path of a deleted file use
    /// [`Mft::resolve_deleted_path`] with this name.
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

    /// The file's `$STANDARD_INFORMATION`: its timestamps and attribute flags. A deleted file has
    /// the values its record still holds: the created, modified and accessed times it had while
    /// live. None of the times is the deletion time; NTFS records no such thing.
    pub fn standard_information(&self) -> Option<NtfsStandardInformation> {
        self.attributes()
            .find_map(|attribute| attribute.standard_information())
    }

    /// Every `$DATA` stream: the default (unnamed) one and any alternate streams, in record
    /// order. Not necessarily default-first (an alternate stream in the base record can precede
    /// the default stream's extent in an extension record); use `name.is_none()` to identify the
    /// default stream, not list position.
    ///
    /// A deleted file's size is what its record still says, usually the size it had. But a
    /// deleted file with an `$ATTRIBUTE_LIST` (many streams or hard links) was seen with every
    /// non-resident stream at size 0 and no data runs, reported as-is rather than "fixed".
    ///
    /// [`NtfsDataStream::data_lost`] says which streams lost their data.
    pub fn data_streams(&self) -> impl Iterator<Item = NtfsDataStream> + use<'a> {
        let file_lost_data = self.stream_data_lost();
        self.attributes()
            .filter_map(move |attribute| NtfsDataStream::from_attribute(&attribute, file_lost_data))
    }

    /// Whether this is a deleted file that lost the data of **some** stream: it is deleted and
    /// has an `$ATTRIBUTE_LIST` among its records, so every non-resident stream is unlocatable.
    /// `false` for every live file. Per file, not per stream: a file with an intact resident
    /// default stream and a lost alternate one is still `true`. Ask the stream instead:
    /// [`NtfsDataStream::data_lost`], [`StreamReader::data_lost`](crate::StreamReader::data_lost)
    /// and [`FileInfo::data_lost`](crate::FileInfo::data_lost) (the default stream).
    ///
    /// Measured on Windows 11 (build 26200), NTFS 3.1, on a virtual disk and a physical SSD: a file
    /// with an `$ATTRIBUTE_LIST` comes back from delete
    /// with the size and data runs of **every non-resident stream** zeroed, default and alternate
    /// alike. Such a stream opens empty ([`Self::open_stream`] gives size 0, no extents), so
    /// check this before trusting an empty stream: its bytes are no longer recorded and cannot
    /// be found. A **resident** stream of the same file is intact and reads as usual.
    ///
    /// The signal is the attribute list alone, wherever it and the other attributes sit. Seen for
    /// lists from many hard links, many streams, or data runs alone, resident or not, for sparse
    /// and compressed files, and for `remove_file`, `del`, `rd /s`, `Remove-Item` and a Shell
    /// permanent delete alike. The clusters themselves are freed and keep their bytes until
    /// reused or trimmed; only where they are is lost. A file without a list is unaffected.
    pub fn stream_data_lost(&self) -> bool {
        // The base record decides whether the file is deleted: a freed extension record of a
        // live file sees the live file, and must not call it deleted.
        self.is_deleted()
            && self.records().any(|record| {
                record.record_attributes().any(|attribute| {
                    attribute.attribute_type() == Some(NtfsAttributeType::AttributeList)
                })
            })
    }

    /// Contents of the default stream when small enough to be stored inside the MFT. `None` for
    /// a non-resident stream. A deleted file keeps its resident data in the record until reused.
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
#[path = "tests/file.rs"]
mod tests;
