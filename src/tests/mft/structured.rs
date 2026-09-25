// Copyright (c) 2022, Matteo Bernacchia <dev@kikijiki.com>. All rights reserved.
// This project is dual licensed under the Apache License 2.0 and the MIT license.
// See the LICENSE files in the project root for details.

// Property test of `Mft::from_parts` and `Mft::read_data_fs`: the input is an `arbitrary`-derived
// description of an MFT (records, names, parent references, streams, extension records, a few
// deliberate corruptions), built into bytes with the `test_records` helpers, then checked against
// invariants, not just "no panic". See `crate::property` for running it longer or replaying a
// failing seed.
//
// `read_data_fs` on record 0 is checked against the description wherever it settles the answer
// (`expected_mft_read`): the first extent resident, the value absent, or `$DATA` one
// identity-mapped run. Elsewhere (arbitrary `$DATA` runs, a list entry reaching an extension
// record, extents to join) it is checked only for not panicking; `mft.rs`'s unit tests cover
// those.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::io::Cursor;
use std::path::{PathBuf, MAIN_SEPARATOR_STR};
use std::sync::atomic::{AtomicUsize, Ordering};

use arbitrary::Arbitrary;

use crate::api::{
    FileId, NtfsAttributeType, NtfsFileName, NtfsFileNamespace, FIRST_NORMAL_RECORD, ROOT_RECORD,
};
use crate::file::NtfsFile;
use crate::file_info::FileInfo;
use crate::mft::test_records::*;
use crate::mft::Mft;
use crate::path::{DefaultPathCache, DeletedPathCache};
use crate::property::{coverage, heavy_property, list, show_on_replay};
use crate::volume::Volume;

/// Records after `$MFT` itself.
const MAX_RECORDS: usize = 12;
const MAX_ATTRIBUTES: usize = 6;
const MAX_CORRUPTIONS: usize = 4;
const MAX_NAME_UNITS: usize = 6;
const MAX_RUNS: usize = 3;
const MAX_LIST_ENTRIES: usize = 4;
/// Files whose paths are resolved several ways per input.
const MAX_CHECKED_FILES: usize = 12;
const RECORD_NUMBER_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;
/// `test_records::test_volume()` has `volume_size: 0` (unbounded); a real volume never does. The
/// bound only rejects a `$DATA` larger than the volume itself, so a small one keeps a
/// description that claims gigabytes from attempting the allocation.
const VOLUME_SIZE: u64 = 64 * 1024 * 1024;
/// The record of `$Extend`, whose child `$Deleted` holds what Windows deletes later.
const EXTEND_RECORD: u64 = 11;
/// The parent of a `$Deleted` name: record 11, or the root.
fn deleted_name_parent(extend: bool) -> u64 {
    if extend {
        reference(1, EXTEND_RECORD)
    } else {
        reference(ROOT_SEQUENCE, ROOT_RECORD)
    }
}

/// `$Deleted` as UTF-16, in upper case when `upper`.
fn deleted_name(upper: bool) -> [u16; 8] {
    DELETED_NAME.map(|unit| {
        if upper {
            (unit as u8).to_ascii_uppercase() as u16
        } else {
            unit
        }
    })
}

/// `$Deleted` as UTF-16.
const DELETED_NAME: [u16; 8] = [
    b'$' as u16,
    b'D' as u16,
    b'e' as u16,
    b'l' as u16,
    b'e' as u16,
    b't' as u16,
    b'e' as u16,
    b'd' as u16,
];
/// Stream names as raw UTF-16, the last with an unpaired surrogate.
const STREAM_NAMES: [&[u16]; 4] = [
    &[b'a' as u16],
    &[b'b' as u16, b'c' as u16],
    &[b'a' as u16, b'd' as u16, b's' as u16],
    &[b'x' as u16, 0xD800, b'y' as u16],
];

#[derive(Debug, Arbitrary)]
struct Image {
    /// Record 0 (`$MFT`), read by `read_data_fs`; its base link is ignored.
    mft: RecordSpec,
    /// Record `i` is MFT record `24 + i`.
    #[arbitrary(with = list::<RecordSpec, MAX_RECORDS>)]
    records: Vec<RecordSpec>,
    corruptions: Vec<Corruption>,
    /// Visiting order for the cache check.
    order: Vec<u8>,
    /// Which live files of the image are deleted in the partial delete (`delete_some`): file `i`
    /// (record `24 + i`) when the byte at `i` is odd.
    #[arbitrary(with = list::<u8, MAX_RECORDS>)]
    deleted: Vec<u8>,
    /// Whether a sequence number that wraps skips 0 (NTFS is documented to), so that a record
    /// freed from 0xFFFF carries 1 and not 0.
    wrap_skips_zero: bool,
}

#[derive(Debug, Arbitrary)]
struct RecordSpec {
    sequence: u8,
    /// Bits 0-2 clear: not in use. Bits 3-5 clear: bitmap bit not set. Bit 6:
    /// directory. Bit 7: protect with another update sequence number than the
    /// default. Skewed so the common case is a normal record.
    flags: u8,
    base: Option<Link>,
    #[arbitrary(with = list::<AttributeSpec, MAX_ATTRIBUTES>)]
    attributes: Vec<AttributeSpec>,
}

impl RecordSpec {
    /// The sequence number the record is stored with. Live: 1 to 4, or one of the two just below
    /// the wrap (so a delete can cross it) - NTFS never gives a live record sequence 0, and a
    /// freed record at 1 reads as coming from 0xFFFF, so live 0 would be ambiguous. Not in use:
    /// may also carry 0 or 0xFFFF, the successors of 0xFFFE/0xFFFF after a delete (NTFS skips 0 on
    /// wrap by documentation, but its behavior at the wrap is unmeasured, so freed-at-0 and
    /// freed-at-1 both name a file live at 0xFFFF).
    fn sequence(&self) -> u16 {
        const LIVE: [u16; 6] = [1, 2, 3, 4, 0xFFFE, 0xFFFF];
        const FREED: [u16; 6] = [1, 2, 3, 4, 0, 0xFFFF];
        let choices = if self.in_use() { LIVE } else { FREED };
        choices[(self.sequence % 6) as usize]
    }

    fn in_use(&self) -> bool {
        self.flags & 0b111 != 0
    }

    fn allocated(&self) -> bool {
        self.flags >> 3 & 0b111 != 0
    }

    fn directory(&self) -> bool {
        self.flags & 0x40 != 0
    }

    fn protected(&self) -> bool {
        self.flags & 0x80 != 0
    }
}

/// A reference to another record: index 0 is `$MFT`, index `k` is record `24 +
/// k - 1`, wrapped to the records there are.
#[derive(Debug, Arbitrary)]
struct Link {
    target: u8,
    claim: Claim,
}

/// The sequence number a reference claims, relative to the target's real one.
#[derive(Debug, Arbitrary)]
enum Claim {
    Right,
    Stale,
    Zero,
    Raw(u16),
}

impl Claim {
    fn resolve(&self, actual: u16) -> u16 {
        match self {
            Claim::Right => actual,
            Claim::Stale => actual.wrapping_add(1),
            Claim::Zero => 0,
            Claim::Raw(sequence) => *sequence,
        }
    }
}

#[derive(Debug, Arbitrary)]
enum ParentSpec {
    Root(Claim),
    Record(Link),
    /// A record number below the first normal one, so not one of the records.
    Reserved {
        number: u8,
        sequence: u8,
    },
}

#[derive(Debug, Arbitrary)]
enum NamespaceSpec {
    Posix,
    Win32,
    Dos,
    Win32AndDos,
}

impl NamespaceSpec {
    fn namespace(&self) -> NtfsFileNamespace {
        match self {
            NamespaceSpec::Posix => NtfsFileNamespace::Posix,
            NamespaceSpec::Win32 => NtfsFileNamespace::Win32,
            NamespaceSpec::Dos => NtfsFileNamespace::Dos,
            NamespaceSpec::Win32AndDos => NtfsFileNamespace::Win32AndDos,
        }
    }
}

#[derive(Debug, Arbitrary)]
enum StorageSpec {
    Resident(u8),
    NonResident {
        size: u32,
        #[arbitrary(with = list::<RunSpec, MAX_RUNS>)]
        runs: Vec<RunSpec>,
    },
    /// The identity mapping `read_data_fs` needs to locate extension records.
    MftIdentity(u8),
    /// What NTFS leaves of a non-resident attribute when it deletes a file that has an
    /// `$ATTRIBUTE_LIST`: highest VCN -1, sizes 0, a run list that is only the terminator.
    Truncated,
}

#[derive(Debug, Arbitrary)]
struct RunSpec {
    /// In clusters; large enough to claim more than the volume holds.
    length: u32,
    /// `None` is a sparse run.
    lcn_delta: Option<i8>,
}

#[derive(Debug, Arbitrary)]
struct ListEntrySpec {
    attribute: u8,
    named: bool,
    vcn: u8,
    target: Link,
}

impl ListEntrySpec {
    fn attribute_type(&self) -> NtfsAttributeType {
        match self.attribute % 3 {
            0 => NtfsAttributeType::Data,
            1 => NtfsAttributeType::Bitmap,
            _ => NtfsAttributeType::FileName,
        }
    }
}

#[derive(Debug, Arbitrary)]
enum AttributeSpec {
    StandardInformation,
    FileName {
        parent: ParentSpec,
        namespace: NamespaceSpec,
        #[arbitrary(with = list::<u8, MAX_NAME_UNITS>)]
        name: Vec<u8>,
        reparse: bool,
    },
    /// A Win32 name `$Deleted` (upper case when `upper`, which the deleted walk also knows), whose
    /// parent is record 11 (`$Extend`) when `extend`, else the root. On a directory record under
    /// `$Extend` this is what Windows leaves after renaming a directory to `$Extend\$Deleted`; the
    /// deleted walk ends there with `<deleted>`. A directory of that name anywhere else is
    /// ordinary.
    DeletedName {
        upper: bool,
        extend: bool,
    },
    Data {
        stream: Option<u8>,
        vcn: u8,
        storage: StorageSpec,
    },
    /// An attribute of another type (`kind` picks one), unnamed, at VCN 0.
    Other {
        kind: u8,
        storage: StorageSpec,
    },
    AttributeList {
        #[arbitrary(with = list::<ListEntrySpec, MAX_LIST_ENTRIES>)]
        entries: Vec<ListEntrySpec>,
        /// Stored in a cluster after the records (only record 0's list is
        /// ever read, by `read_data_fs`; elsewhere this stays resident).
        non_resident: bool,
    },
}

#[derive(Debug, Arbitrary)]
struct Corruption {
    record: u8,
    attribute: u8,
    fault: Fault,
}

#[derive(Debug, Arbitrary)]
enum Fault {
    Signature,
    UsedSize(u16),
    AttributesOffset(u16),
    BaseReference(u64),
    /// Makes the update sequence check fail (protects the record, then
    /// changes the sector end).
    BrokenFixup,
    AttributeLength(u16),
    NameLength(u8),
    Namespace(u8),
    ValueLength(u32),
    DataSize(u64),
    /// Where the attribute's own name starts.
    NameOffset(u16),
    /// The update sequence array offset and length, after protection.
    UpdateSequence {
        offset: u16,
        length: u16,
    },
    RunByte {
        at: u8,
        value: u8,
    },
    /// The root record's sequence number (record number ignored).
    RootSequence(u16),
}

/// A `$FILE_NAME` as described: raw UTF-16 name, namespace, parent reference.
type Name = (Vec<u16>, NtfsFileNamespace, u64);

/// What the description says one record holds, to compare the crate's answers
/// against.
#[derive(Default)]
struct Expected {
    names: Vec<Name>,
    streams: Vec<(Option<Vec<u16>>, u64)>,
    /// Whether the stream at the same position of `streams` is resident.
    resident: Vec<bool>,
    /// Whether the record holds an `$ATTRIBUTE_LIST`.
    has_list: bool,
    /// Every unnamed `$DATA` attribute in order: its length if resident.
    unnamed_data: Vec<Option<usize>>,
    standard_information: bool,
}

struct Built {
    number: u64,
    /// The sequence number the record is stored with.
    sequence: u16,
    is_directory: bool,
    bytes: Vec<u8>,
    offsets: Vec<usize>,
    expected: Expected,
    /// The positions in the spec's attribute list of those that fit in the record.
    written: Vec<usize>,
    own_reference: u64,
    base_reference: u64,
    in_use: bool,
    allocated: bool,
    protected: bool,
    break_fixup: bool,
    update_sequence: Option<(u16, u16)>,
    /// The bytes of a non-resident attribute list, for the cluster after the
    /// records.
    list_area: Option<Vec<u8>>,
}

/// Where a link points: `(record number, sequence the record really has)`.
fn link_target(link: &Link, specs: &[&RecordSpec]) -> (u64, u16) {
    let index = link.target as usize % specs.len();
    (record_number(index), specs[index].sequence())
}

fn record_number(index: usize) -> u64 {
    if index == 0 {
        0
    } else {
        FIRST_NORMAL_RECORD + index as u64 - 1
    }
}

fn link_reference(link: &Link, specs: &[&RecordSpec]) -> u64 {
    let (number, sequence) = link_target(link, specs);
    reference(link.claim.resolve(sequence), number)
}

fn parent_reference(parent: &ParentSpec, specs: &[&RecordSpec]) -> u64 {
    match parent {
        ParentSpec::Root(claim) => reference(claim.resolve(ROOT_SEQUENCE), ROOT_RECORD),
        ParentSpec::Record(link) => link_reference(link, specs),
        ParentSpec::Reserved { number, sequence } => {
            reference((sequence % 4) as u16, *number as u64 % FIRST_NORMAL_RECORD)
        }
    }
}

/// Code units from a small alphabet: a few letters, a space and a dot, an
/// unpaired surrogate of each kind, a surrogate pair, and a non-ASCII letter.
fn name_units(name: &[u8]) -> Vec<u16> {
    let mut units = Vec::new();
    for &choice in name {
        match choice % 11 {
            0..=3 => units.push(b'a' as u16 + (choice % 11) as u16),
            4 => units.push(b' ' as u16),
            5 => units.push(b'.' as u16),
            6 => units.push(0xD800),
            7 => units.push(0xDC00),
            8 => units.extend_from_slice(&[0xD83D, 0xDE00]),
            9 => units.push(0x00E9),
            _ => units.push(b'x' as u16),
        }
    }
    units
}

/// The clusters an identity-mapped `$DATA` covers: 1 to 16, so that it sometimes covers all of
/// the records and sometimes fewer than the reader holds.
fn identity_clusters(clusters: u8) -> u8 {
    clusters % 16 + 1
}

fn stream_name(stream: Option<u8>) -> Vec<u16> {
    stream.map_or(Vec::new(), |index| {
        STREAM_NAMES[index as usize % STREAM_NAMES.len()].to_vec()
    })
}

/// Writes a resident, non-resident or identity-mapped attribute of the given
/// `(type, id, name, lowest VCN)`.
fn write_storage(
    record: &mut [u8],
    offset: usize,
    (attribute_type, id, name, vcn): (NtfsAttributeType, u16, &[u16], u8),
    storage: &StorageSpec,
) -> usize {
    match storage {
        StorageSpec::Resident(length) => {
            let value = vec![0xAB; (*length % 64) as usize];
            add_resident_attribute_raw(record, offset, attribute_type, id, name, &value)
        }
        StorageSpec::NonResident { size, runs } => {
            let runs: Vec<(u64, Option<i64>)> = runs
                .iter()
                .map(|run| (run.length as u64, run.lcn_delta.map(|d| d as i64)))
                .collect();
            let clusters: u64 = runs.iter().map(|(length, _)| length).sum();
            let lowest = vcn as u64;
            add_nonresident_data_runs_raw(
                record,
                offset,
                attribute_type,
                name,
                lowest,
                (lowest + clusters).saturating_sub(1),
                *size as u64,
                &encode_runs(&runs),
            )
        }
        StorageSpec::MftIdentity(clusters) => {
            add_identity_mft_data(record, offset, identity_clusters(*clusters))
        }
        StorageSpec::Truncated => {
            add_nonresident_data_runs_raw(record, offset, attribute_type, name, 0, u64::MAX, 0, &[])
        }
    }
}

/// The `$ATTRIBUTE_LIST` value: one entry per spec, each pointing at a record.
fn list_value(entries: &[ListEntrySpec], specs: &[&RecordSpec]) -> Vec<u8> {
    let mut value = Vec::new();
    for entry in entries {
        let name = if entry.named { "x" } else { "" };
        let mut bytes = list_entry(entry.attribute_type(), name, entry.vcn as u64, 0);
        write_u64(&mut bytes, 16, link_reference(&entry.target, specs));
        value.extend(bytes);
    }
    value
}

/// The attribute types an `AttributeSpec::Other` can be; `kind` picks one.
const OTHER_TYPES: [NtfsAttributeType; 11] = [
    NtfsAttributeType::ObjectId,
    NtfsAttributeType::SecurityDescriptor,
    NtfsAttributeType::VolumeName,
    NtfsAttributeType::VolumeInformation,
    NtfsAttributeType::IndexRoot,
    NtfsAttributeType::IndexAllocation,
    NtfsAttributeType::Bitmap,
    NtfsAttributeType::ReparsePoint,
    NtfsAttributeType::EaInformation,
    NtfsAttributeType::Ea,
    NtfsAttributeType::LoggedUtilityStream,
];

fn other_type(kind: u8) -> NtfsAttributeType {
    OTHER_TYPES[kind as usize % OTHER_TYPES.len()]
}

impl AttributeSpec {
    /// Whether this is an unnamed `$DATA` attribute (the identity mapping always is).
    fn is_unnamed_data(&self) -> bool {
        matches!(
            self,
            AttributeSpec::Data { stream: None, .. }
                | AttributeSpec::Data {
                    storage: StorageSpec::MftIdentity(_),
                    ..
                }
        )
    }

    /// Writes the attribute at `offset` and returns the offset after it.
    fn write(
        &self,
        record: &mut [u8],
        offset: usize,
        id: u16,
        specs: &[&RecordSpec],
        list_cluster: i64,
    ) -> usize {
        match self {
            AttributeSpec::StandardInformation => add_standard_information(record, offset, 0x20),
            AttributeSpec::FileName {
                parent,
                namespace,
                name,
                reparse,
            } => add_file_name_raw(
                record,
                offset,
                id,
                parent_reference(parent, specs),
                namespace.namespace(),
                &name_units(name),
                if *reparse { 0x400 } else { 0 },
            ),
            AttributeSpec::DeletedName { upper, extend } => add_file_name_raw(
                record,
                offset,
                id,
                deleted_name_parent(*extend),
                NtfsFileNamespace::Win32,
                &deleted_name(*upper),
                0,
            ),
            AttributeSpec::Data {
                stream,
                vcn,
                storage,
            } => write_storage(
                record,
                offset,
                (NtfsAttributeType::Data, id, &stream_name(*stream), *vcn),
                storage,
            ),
            AttributeSpec::Other { kind, storage } => {
                let attribute_type = other_type(*kind);
                // The identity mapping is `$DATA`, which is not what this is.
                let resident;
                let storage = if let StorageSpec::MftIdentity(length) = storage {
                    resident = StorageSpec::Resident(*length);
                    &resident
                } else {
                    storage
                };
                write_storage(record, offset, (attribute_type, id, &[], 0), storage)
            }
            AttributeSpec::AttributeList {
                entries,
                non_resident,
            } => {
                let value = list_value(entries, specs);
                if *non_resident {
                    add_nonresident_data_runs(
                        record,
                        offset,
                        NtfsAttributeType::AttributeList,
                        "",
                        0,
                        0,
                        value.len() as u64,
                        &encode_runs(&[(1, Some(list_cluster))]),
                    )
                } else {
                    add_resident_attribute(
                        record,
                        offset,
                        NtfsAttributeType::AttributeList,
                        id,
                        "",
                        &value,
                    )
                }
            }
        }
    }

    /// What the crate should report for this attribute once written.
    fn expect(&self, expected: &mut Expected, specs: &[&RecordSpec]) {
        match self {
            AttributeSpec::FileName {
                parent,
                namespace,
                name,
                ..
            } => expected.names.push((
                name_units(name),
                namespace.namespace(),
                parent_reference(parent, specs),
            )),
            AttributeSpec::Data {
                stream,
                vcn,
                storage,
            } => {
                // The identity mapping is always unnamed, whatever `stream` says.
                if stream.is_none() || matches!(storage, StorageSpec::MftIdentity(_)) {
                    expected.unnamed_data.push(match storage {
                        StorageSpec::Resident(length) => Some((*length % 64) as usize),
                        _ => None,
                    });
                }
                let name = stream.map(|_| stream_name(*stream));
                let listed = expected.streams.len();
                match storage {
                    StorageSpec::Resident(length) => {
                        expected.streams.push((name, (*length % 64) as u64))
                    }
                    // Only the VCN-0 extent carries the size.
                    StorageSpec::NonResident { size, .. } if *vcn == 0 => {
                        expected.streams.push((name, *size as u64))
                    }
                    StorageSpec::NonResident { .. } => {}
                    StorageSpec::Truncated => expected.streams.push((name, 0)),
                    StorageSpec::MftIdentity(clusters) => expected.streams.push((
                        None,
                        identity_clusters(*clusters) as u64 * CLUSTER_SIZE as u64,
                    )),
                }
                if expected.streams.len() > listed {
                    expected
                        .resident
                        .push(matches!(storage, StorageSpec::Resident(_)));
                }
            }
            AttributeSpec::DeletedName { upper, extend } => expected.names.push((
                deleted_name(*upper).to_vec(),
                NtfsFileNamespace::Win32,
                deleted_name_parent(*extend),
            )),
            AttributeSpec::StandardInformation => expected.standard_information = true,
            AttributeSpec::AttributeList { .. } => expected.has_list = true,
            AttributeSpec::Other { .. } => {}
        }
    }
}

fn build_record(index: usize, specs: &[&RecordSpec], list_cluster: i64) -> Built {
    let spec = specs[index];
    let number = record_number(index);
    let base_reference = match (&spec.base, index) {
        (Some(link), 1..) => link_reference(link, specs),
        _ => 0,
    };
    let mut record = new_record(number, spec.sequence(), base_reference);
    // InUse is bit 0, IsDirectory bit 1 (`NtfsFileFlags` is crate-private).
    set_record_flags(
        &mut record,
        spec.in_use() as u16 | (spec.directory() as u16) << 1,
    );

    let mut offset = ATTRIBUTES_OFFSET;
    let mut offsets = Vec::new();
    let mut written = Vec::new();
    let mut expected = Expected::default();
    let mut list_area = None;
    for (position, attribute) in spec.attributes.iter().take(MAX_ATTRIBUTES).enumerate() {
        let id = position as u16 + 1;
        let mut scratch = vec![0u8; RECORD_SIZE];
        let length = attribute.write(&mut scratch, 0, id, specs, list_cluster);
        // Leave room for the end marker.
        if offset + length + 16 > RECORD_SIZE {
            continue;
        }
        attribute.write(&mut record, offset, id, specs, list_cluster);
        attribute.expect(&mut expected, specs);
        if let (
            0,
            AttributeSpec::AttributeList {
                entries,
                non_resident: true,
            },
        ) = (index, attribute)
        {
            list_area.get_or_insert_with(|| list_value(entries, specs));
        }
        offsets.push(offset);
        written.push(position);
        offset += length;
    }
    let used = add_end_marker(&mut record, offset);
    finish_record(&mut record, used);

    Built {
        number,
        sequence: spec.sequence(),
        is_directory: spec.directory(),
        bytes: record,
        offsets,
        expected,
        written,
        own_reference: reference(spec.sequence(), number),
        base_reference,
        in_use: spec.in_use(),
        allocated: spec.allocated(),
        protected: spec.protected(),
        break_fixup: false,
        update_sequence: None,
        list_area,
    }
}

fn read_u16(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
}

/// Writes `value` at `at` only if it fits: a corruption never grows the record.
fn patch(bytes: &mut [u8], at: usize, value: &[u8]) {
    if let Some(target) = bytes.get_mut(at..at + value.len()) {
        target.copy_from_slice(value);
    }
}

fn corrupt(built: &mut Built, corruption: &Corruption) {
    let record = &mut built.bytes;
    let attribute = (!built.offsets.is_empty())
        .then(|| built.offsets[corruption.attribute as usize % built.offsets.len()]);
    let attribute_type = attribute
        .and_then(|at| record.get(at..at + 4))
        .map(|t| NtfsAttributeType::try_from(u32::from_le_bytes(t.try_into().unwrap())).ok());
    let non_resident = attribute.is_some_and(|at| record[at + 8] == 1);

    match (&corruption.fault, attribute) {
        (Fault::Signature, _) => patch(record, 0, b"BAAD"),
        (Fault::UsedSize(size), _) => patch(record, 24, &(*size as u32).to_le_bytes()),
        (Fault::AttributesOffset(offset), _) => patch(record, 20, &offset.to_le_bytes()),
        (Fault::BaseReference(base), _) => patch(record, 32, &base.to_le_bytes()),
        (Fault::BrokenFixup, _) => built.break_fixup = true,
        (Fault::AttributeLength(length), Some(at)) => {
            patch(record, at + 4, &(*length as u32).to_le_bytes())
        }
        (Fault::NameLength(_) | Fault::Namespace(_), Some(at))
            if attribute_type == Some(Some(NtfsAttributeType::FileName)) && !non_resident =>
        {
            if let Some(value) = read_u16(record, at + 20) {
                let value = at + value as usize;
                match corruption.fault {
                    Fault::NameLength(length) => patch(record, value + 64, &[length]),
                    Fault::Namespace(namespace) => patch(record, value + 65, &[namespace]),
                    _ => {}
                }
            }
        }
        (Fault::ValueLength(length), Some(at)) if !non_resident => {
            patch(record, at + 16, &length.to_le_bytes())
        }
        (Fault::NameOffset(offset), Some(at)) => patch(record, at + 10, &offset.to_le_bytes()),
        (Fault::UpdateSequence { offset, length }, _) => {
            built.update_sequence = Some((*offset, *length))
        }
        (Fault::DataSize(size), Some(at)) if non_resident => {
            patch(record, at + 48, &size.to_le_bytes())
        }
        (Fault::RunByte { at: index, value }, Some(at)) if non_resident => {
            if let Some(runs) = read_u16(record, at + 32) {
                patch(
                    record,
                    at + runs as usize + (*index % 16) as usize,
                    &[*value],
                );
            }
        }
        _ => {}
    }
}

/// The UTF-16 code units of an `OsStr` the crate built: `encode_wide` on Windows.
#[cfg(windows)]
fn units(text: &OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    text.encode_wide().collect()
}

/// The UTF-16 code units of an `OsStr` the crate built, decoded here (WTF-8,
/// which is what the crate writes for a lone surrogate elsewhere) instead of
/// by the crate.
#[cfg(not(windows))]
fn units(text: &OsStr) -> Vec<u16> {
    fn continuation(bytes: &[u8], at: usize) -> u32 {
        let byte = *bytes.get(at).expect("truncated WTF-8");
        assert_eq!(byte & 0xC0, 0x80, "malformed WTF-8 continuation {byte:#x}");
        (byte & 0x3F) as u32
    }

    let bytes = text.as_encoded_bytes();
    let mut units = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let byte = bytes[at];
        let (code_point, length) = match byte {
            0x00..=0x7F => (byte as u32, 1),
            0xC2..=0xDF => ((byte as u32 & 0x1F) << 6 | continuation(bytes, at + 1), 2),
            0xE0..=0xEF => (
                (byte as u32 & 0x0F) << 12
                    | continuation(bytes, at + 1) << 6
                    | continuation(bytes, at + 2),
                3,
            ),
            0xF0..=0xF4 => (
                (byte as u32 & 0x07) << 18
                    | continuation(bytes, at + 1) << 12
                    | continuation(bytes, at + 2) << 6
                    | continuation(bytes, at + 3),
                4,
            ),
            _ => panic!("malformed WTF-8 lead byte {byte:#x}"),
        };
        if code_point >= 0x1_0000 {
            let offset = code_point - 0x1_0000;
            units.push(0xD800 | (offset >> 10) as u16);
            units.push(0xDC00 | (offset & 0x3FF) as u16);
        } else {
            units.push(code_point as u16);
        }
        at += length;
    }
    units
}

/// What the description says `resolve_path` gives for a name.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    /// This path, as UTF-16.
    Path(Vec<u16>),
    Unresolved,
    /// A record on the way was corrupted on purpose, so the description no
    /// longer says.
    Unknown,
}

/// The description of the whole image, to answer questions from without
/// asking the crate.
struct World<'a> {
    built: &'a [Built],
    tainted: &'a HashSet<u64>,
    root_reference: u64,
}

impl World<'_> {
    fn record(&self, number: u64) -> Option<&Built> {
        self.built.iter().find(|record| record.number == number)
    }

    /// Whether the record's bitmap bit is set (record 0's always is).
    fn allocated(record: &Built) -> bool {
        record.number == 0 || record.allocated
    }

    /// The `$FILE_NAME`s of the file that record `number` belongs to, in the order the crate
    /// walks them: the base record's, then every in-use extension record whose base reference
    /// names the base in the state the base is in (the exact reference for a live base, the freed
    /// rule for a freed one), provided the base is not itself an extension record. `None` if any
    /// record of the family was corrupted on purpose.
    fn names(&self, number: u64) -> Option<Vec<&Name>> {
        let record = self.record(number)?;
        let base_number = match record.base_reference {
            0 => number,
            reference => reference & RECORD_NUMBER_MASK,
        };
        let base = self.record(base_number)?;
        let family = self.built.iter().filter(|other| {
            other.number == base_number
                || (other.base_reference != 0
                    && other.base_reference & RECORD_NUMBER_MASK == base_number)
        });
        if family
            .chain([record])
            .any(|r| self.tainted.contains(&r.number))
        {
            return None;
        }

        if record.base_reference != 0 {
            let names_base = match (Self::allocated(base), base.in_use) {
                (true, true) => record.base_reference == base.own_reference,
                (false, false) => names_freed(record.base_reference, base),
                _ => true,
            };
            if base.base_reference != 0 || !names_base {
                return Some(Vec::new());
            }
        }

        let mut members = Vec::new();
        if Self::allocated(base) && base.in_use {
            members.push(base);
        }
        members.extend(self.built.iter().filter(|other| {
            other.number != base_number
                && Self::allocated(other)
                && other.in_use
                && other.base_reference != 0
                && other.base_reference == base.own_reference
        }));
        Some(
            members
                .into_iter()
                .flat_map(|member| member.expected.names.iter())
                .collect(),
        )
    }

    /// Walks the parent chain of a name by record number, from the description alone: every hop
    /// needs an in-use, allocated record with the same full reference (sequence included).
    /// Directory names use the file's `best_name` (first Win32, else first). The chain ends at
    /// the root only if the reference matches the root's; one that revisits a reference never
    /// ends.
    fn resolve(&self, parent_reference: u64, leaf: &[u16]) -> Outcome {
        let mut components: Vec<&[u16]> = Vec::new();
        let mut seen = Vec::new();
        let mut reference = parent_reference;
        loop {
            let number = reference & RECORD_NUMBER_MASK;
            if number == ROOT_RECORD {
                if reference != self.root_reference {
                    return Outcome::Unresolved;
                }
                break;
            }
            if seen.contains(&reference) {
                return Outcome::Unresolved;
            }
            seen.push(reference);
            // Not one of the records: reserved numbers, or past the end.
            let Some(record) = self.record(number) else {
                return Outcome::Unresolved;
            };
            if self.tainted.contains(&number) {
                return Outcome::Unknown;
            }
            if record.own_reference != reference || !record.in_use || !Self::allocated(record) {
                return Outcome::Unresolved;
            }
            let Some(names) = self.names(number) else {
                return Outcome::Unknown;
            };
            let is_win32 = |name: &&&Name| {
                matches!(
                    name.1,
                    NtfsFileNamespace::Win32 | NtfsFileNamespace::Win32AndDos
                )
            };
            let Some(best) = names.iter().find(is_win32).or(names.first()) else {
                return Outcome::Unresolved;
            };
            components.push(&best.0);
            reference = best.2;
        }

        let mut path: Vec<u16> = VOLUME_PATH.encode_utf16().collect();
        components.reverse();
        components.push(leaf);
        for component in components {
            let ends_with_separator = path
                .last()
                .is_some_and(|&unit| unit < 0x80 && std::path::is_separator(unit as u8 as char));
            if !ends_with_separator {
                path.extend(MAIN_SEPARATOR_STR.encode_utf16());
            }
            path.extend_from_slice(component);
        }
        Outcome::Path(path)
    }
}

fn describe(name: &NtfsFileName) -> (Vec<u16>, Option<NtfsFileNamespace>, u64) {
    (
        units(&name.to_os_string()),
        name.namespace(),
        name.parent_reference(),
    )
}

#[test]
fn a_described_mft_loads_to_what_the_description_says() {
    heavy_property(|u| {
        let image = Image::arbitrary(u)?;
        show_on_replay(&image);
        check_image(&image);
        Ok(())
    });
}

/// A base reference fault turns record 25 into an extension record of directory 24, so the crate
/// reports the directory with two names (preferring the Win32 one) where the description gives
/// it one. The directory itself is not corrupted, so only tainting the record the reference
/// names keeps the oracle from misanswering for it (it indexed past its names).
#[test]
fn a_base_reference_fault_taints_the_record_it_names() {
    let name = |namespace, name: [u8; 2]| AttributeSpec::FileName {
        parent: ParentSpec::Root(Claim::Right),
        namespace,
        name: name.to_vec(),
        reparse: false,
    };
    let record = |flags, attributes| RecordSpec {
        sequence: 1,
        flags,
        base: None,
        attributes,
    };
    let image = Image {
        mft: record(0, vec![]),
        records: vec![
            // Record 24: a directory named "ab" (Posix).
            record(0x7F, vec![name(NamespaceSpec::Posix, [0, 1])]),
            // Record 25: a file named "cd" (Win32), which the fault makes an extension of 24.
            record(0x3F, vec![name(NamespaceSpec::Win32, [2, 3])]),
            // Record 26: a file "a" in the directory.
            record(
                0x3F,
                vec![AttributeSpec::FileName {
                    parent: ParentSpec::Record(Link {
                        target: 1,
                        claim: Claim::Right,
                    }),
                    namespace: NamespaceSpec::Win32,
                    name: vec![0],
                    reparse: false,
                }],
            ),
        ],
        corruptions: vec![Corruption {
            record: 2,
            attribute: 0,
            fault: Fault::BaseReference(reference(1, FIRST_NORMAL_RECORD)),
        }],
        order: vec![],
        deleted: vec![],
        wrap_skips_zero: false,
    };

    check_image(&image);
}

/// An image built into bytes, before `Mft::from_parts` sees it.
struct Assembled<'a> {
    specs: Vec<&'a RecordSpec>,
    built: Vec<Built>,
    /// The records a corruption touched, or named as a base.
    tainted: HashSet<u64>,
    root_sequence: Option<u16>,
    volume: Volume,
    data: Vec<u8>,
    bitmap: Vec<u8>,
}

/// Builds the description into `$MFT` bytes and a bitmap, corruptions applied. `None` for an
/// image with no records after `$MFT`.
fn assemble(image: &Image) -> Option<Assembled<'_>> {
    if image.records.is_empty() {
        return None;
    }
    let specs: Vec<&RecordSpec> = std::iter::once(&image.mft).chain(&image.records).collect();

    // A non-resident attribute list of record 0 lives in the first cluster
    // after the records.
    let list_cluster = ((FIRST_NORMAL_RECORD as usize + specs.len() - 1) * RECORD_SIZE)
        .div_ceil(CLUSTER_SIZE) as i64;
    let mut built: Vec<Built> = (0..specs.len())
        .map(|index| build_record(index, &specs, list_cluster))
        .collect();
    let mut tainted = HashSet::new();
    let mut root_sequence = None;
    for corruption in image.corruptions.iter().take(MAX_CORRUPTIONS) {
        if let Fault::RootSequence(sequence) = corruption.fault {
            root_sequence = Some(sequence);
            continue;
        }
        let target = &mut built[corruption.record as usize % specs.len()];
        tainted.insert(target.number);
        corrupt(target, corruption);
        // The record the new base reference names now has an extension record it did not
        // describe, so the description no longer says what it holds either.
        if let Fault::BaseReference(base) = corruption.fault {
            tainted.insert(base & RECORD_NUMBER_MASK);
        }
    }
    for record in &mut built {
        if record.protected || record.break_fixup {
            protect_record(&mut record.bytes, 0x1234);
            if record.break_fixup {
                record.bytes[SECTOR_END] ^= 0xFF;
            }
        }
        if let Some((offset, length)) = record.update_sequence {
            patch(&mut record.bytes, 4, &offset.to_le_bytes());
            patch(&mut record.bytes, 6, &length.to_le_bytes());
        }
    }

    let (volume, mut data, mut bitmap) = raw_parts(
        built[1..]
            .iter()
            .map(|record| record.bytes.clone())
            .collect(),
    );
    let volume = volume.with_volume_size(VOLUME_SIZE);
    if let Some(list) = &built[0].list_area {
        data.resize(list_cluster as usize * CLUSTER_SIZE, 0);
        data.extend(list);
        data.resize((list_cluster as usize + 1) * CLUSTER_SIZE, 0);
    }
    data[..RECORD_SIZE].copy_from_slice(&built[0].bytes);
    bitmap[0] |= 1;
    for record in &built[1..] {
        if !record.allocated {
            bitmap[record.number as usize / 8] &= !(1 << (record.number % 8));
        }
    }
    if let Some(sequence) = root_sequence {
        write_u16(
            &mut data[ROOT_RECORD as usize * RECORD_SIZE..],
            16,
            sequence,
        );
    }

    Some(Assembled {
        specs,
        built,
        tainted,
        root_sequence,
        volume,
        data,
        bitmap,
    })
}

fn check_image(image: &Image) {
    let Some(assembled) = assemble(image) else {
        return;
    };
    let Assembled {
        specs,
        built,
        tainted,
        root_sequence,
        volume,
        data,
        bitmap,
    } = assembled;

    // The `$MFT` bootstrap path, over the same bytes, through a reader that maps a cluster number
    // to the same offset. Record 0 as it is in the image (update sequence protection included)
    // must not panic it; the record the loader hands over is checked against the description.
    let protected = data[..RECORD_SIZE].to_vec();
    for attribute_type in [NtfsAttributeType::Data, NtfsAttributeType::Bitmap] {
        let _ = Mft::read_data_fs(&volume, &mut Cursor::new(&data), &protected, attribute_type);
    }
    if !tainted.contains(&0) {
        let record0 = Mft::read_record_fs(&mut Cursor::new(&data), RECORD_SIZE as u64, 0)
            .expect("an untouched record 0 reads");
        for attribute_type in [NtfsAttributeType::Data, NtfsAttributeType::Bitmap] {
            let read =
                Mft::read_data_fs(&volume, &mut Cursor::new(&data), &record0, attribute_type);
            match (expected_mft_read(&specs, &built[0], attribute_type, &data), read) {
                (None, _) => {}
                (Some(MftRead::Absent), Ok(None)) => {}
                (Some(MftRead::Value(expected)), Ok(Some(value))) => assert_eq!(
                    value, expected,
                    "read_data_fs({attribute_type:?}) on record 0 returned other bytes"
                ),
                (Some(MftRead::Fails), Err(_)) => {}
                (Some(expected), read) => panic!(
                    "read_data_fs({attribute_type:?}) on record 0 should be {expected:?}, got {read:?}"
                ),
            }
        }
    }

    let mft = build_from_parts(volume, data, bitmap);
    let world = World {
        built: &built,
        tainted: &tainted,
        root_reference: reference(root_sequence.unwrap_or(ROOT_SEQUENCE), ROOT_RECORD),
    };
    check(&mft, &world, &image.order);
}

/// What `check_deleted_view` checked, so the test can tell it did not skip everything.
#[derive(Default)]
struct DeletedViewStats {
    files: usize,
    with_extensions: usize,
    paths: usize,
}

/// Whether `name`'s parents, in `mft`, are the ordinary kind the deleted walk can be compared
/// on: no system record (below `FIRST_NORMAL_RECORD`, root aside), no extension record of one,
/// and none whose base is neither live nor freed consistently. The image can build either: a
/// system record stays live while its extension records are freed with the rest; an
/// inconsistent base keeps its live extensions but gives a freed one nothing (see
/// `a_base_that_is_neither_live_nor_freed_keeps_its_live_extension_records`). Only asked of a
/// name that resolves, so the chain ends at the root.
fn has_ordinary_parents(mft: &Mft, name: &NtfsFileName) -> bool {
    let mut parent = name.parent_reference() & RECORD_NUMBER_MASK;
    while parent != ROOT_RECORD {
        let Some(record) = mft.record(parent) else {
            return false;
        };
        // The deleted walk only goes through directories; the live one does not check.
        if !record.is_directory() {
            return false;
        }
        // The record itself, or the base it belongs to.
        let base = record
            .base_reference()
            .map_or(parent, |base| base & RECORD_NUMBER_MASK);
        let base_is_live = mft.record(base).is_some_and(|base| {
            mft.is_allocated(base.number()) && base.is_used() && !base.is_extension()
        });
        if base < FIRST_NORMAL_RECORD || !base_is_live {
            return false;
        }
        let Some(next) = record.best_name() else {
            return false;
        };
        parent = next.parent_reference() & RECORD_NUMBER_MASK;
    }
    true
}

/// Whether every parent of `name`, up to the root, is a base record (not an extension record).
fn has_only_base_records_as_parents(mft: &Mft, name: &NtfsFileName) -> bool {
    let mut parent = name.parent_reference() & RECORD_NUMBER_MASK;
    while parent != ROOT_RECORD {
        let Some(record) = mft.record(parent) else {
            return false;
        };
        if record.is_extension() {
            return false;
        }
        let Some(next) = record.best_name() else {
            return false;
        };
        parent = next.parent_reference() & RECORD_NUMBER_MASK;
    }
    true
}

/// The oracle: delete every live file the way NTFS does (in-use flag off, bitmap bit off,
/// sequence plus one - the measured change) and require the deleted view to equal the live view.
/// All live records are deleted at once, base and extension alike, so extension records keep the
/// base reference the live base had.
///
/// One exception: an extension record already freed, naming a live base with exactly the base's
/// own reference, is rewritten as one sequence lower (an earlier incarnation). On a real volume
/// such a record is empty; a random image gives it attributes that the live view ignores and the
/// deleted view would attribute. The empty case is covered by
/// `an_empty_freed_extension_adds_nothing_to_a_live_or_deleted_file`.
fn check_deleted_view(image: &Image) -> DeletedViewStats {
    let mut stats = DeletedViewStats::default();
    let Some(assembled) = assemble(image) else {
        return stats;
    };
    let live = build_from_parts(
        assembled.volume.clone(),
        assembled.data.clone(),
        assembled.bitmap.clone(),
    );
    let (mut data, mut bitmap) = (assembled.data, assembled.bitmap);

    let is_live = |mft: &Mft, number: u64| {
        mft.is_allocated(number) && mft.record(number).is_some_and(|record| record.is_used())
    };
    for number in FIRST_NORMAL_RECORD..live.record_count() {
        let Some(record) = live.record(number) else {
            continue;
        };
        let bytes = &mut data[number as usize * RECORD_SIZE..][..RECORD_SIZE];
        if is_live(&live, number) {
            let sequence = u16::from_le_bytes([bytes[16], bytes[17]]).wrapping_add(1);
            write_u16(bytes, 16, sequence);
            let flags = u16::from_le_bytes([bytes[22], bytes[23]]);
            write_u16(bytes, 22, flags & !1);
            bitmap[number as usize / 8] &= !(1 << (number % 8));
        } else if let Some(base_reference) = record.base_reference() {
            let names_a_live_base = live
                .record(base_reference & RECORD_NUMBER_MASK)
                .is_some_and(|base| {
                    is_live(&live, base.number()) && base.reference() == base_reference
                });
            if names_a_live_base {
                write_u64(bytes, 32, base_reference.wrapping_sub(1 << 48));
            }
        }
    }
    let deleted = build_from_parts(assembled.volume, data, bitmap);

    let described = |file: &NtfsFile| {
        let mut info = FileInfo::new(file);
        // The path and the flags are what differ: the deleted path is checked on its own, and a
        // file with an attribute list is only "lost" once deleted (checked below).
        info.path = None;
        info.is_deleted = false;
        info.data_lost = false;
        info
    };
    let has_a_name_or_times = |file: &NtfsFile| {
        file.attributes().any(|attribute| {
            matches!(
                attribute.attribute_type(),
                Some(NtfsAttributeType::StandardInformation | NtfsAttributeType::FileName)
            )
        })
    };
    let mut live_numbers = HashSet::new();
    let mut expected_deleted = Vec::new();
    for file in live.files() {
        let number = file.number();
        live_numbers.insert(number);
        let gone = deleted
            .record(number)
            .unwrap_or_else(|| panic!("record {number} lost its header"));
        assert!(!gone.is_used(), "record {number} is still in use");
        assert!(!deleted.is_allocated(number));
        assert_eq!(gone.is_directory(), file.is_directory(), "file {number}");
        assert_eq!(gone.base_reference(), file.base_reference());
        stats.files += 1;

        let records: Vec<u64> = file.records().map(|record| record.number()).collect();
        assert_eq!(
            gone.records()
                .map(|record| record.number())
                .collect::<Vec<_>>(),
            records,
            "file {number}: the deleted view has other records"
        );
        stats.with_extensions += usize::from(records.len() > 1);

        assert_eq!(
            gone.names().map(|name| describe(&name)).collect::<Vec<_>>(),
            file.names().map(|name| describe(&name)).collect::<Vec<_>>(),
            "file {number}: names()"
        );
        assert_eq!(
            gone.hard_links()
                .map(|name| describe(&name))
                .collect::<Vec<_>>(),
            file.hard_links()
                .map(|name| describe(&name))
                .collect::<Vec<_>>(),
            "file {number}: hard_links()"
        );
        assert_eq!(
            gone.best_name().map(|name| describe(&name)),
            file.best_name().map(|name| describe(&name)),
            "file {number}: best_name()"
        );
        assert_eq!(
            gone.standard_information(),
            file.standard_information(),
            "file {number}: standard_information()"
        );
        // Whether a stream lost its data is the one thing a delete changes (checked below).
        let without_lost = |file: &NtfsFile| {
            file.data_streams()
                .map(|stream| crate::NtfsDataStream {
                    data_lost: false,
                    ..stream
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            without_lost(&gone),
            without_lost(&file),
            "file {number}: data_streams()"
        );
        assert!(file.data_streams().all(|stream| !stream.data_lost));
        assert_eq!(
            gone.resident_data(),
            file.resident_data(),
            "file {number}: resident_data()"
        );
        assert_eq!(
            described(&gone),
            described(&file),
            "file {number}: FileInfo"
        );
        assert!(!FileInfo::new(&file).data_lost, "file {number}: live");
        // What a delete does to the streams, from the description and not from the crate: a stream
        // lost its data if it is not resident and a record of the file holds an `$ATTRIBUTE_LIST`.
        // Left out when a corruption touched the file, which the description no longer settles.
        if !family_is_tainted(&assembled.built, &assembled.tainted, number) {
            let family = live_family(&assembled.built, number);
            let has_list = family.iter().any(|record| record.expected.has_list);
            let expected_streams: Vec<(Option<Vec<u16>>, u64, bool)> = family
                .iter()
                .flat_map(|record| {
                    record
                        .expected
                        .streams
                        .iter()
                        .zip(&record.expected.resident)
                        .map(|(stream, resident)| {
                            (stream.0.clone(), stream.1, has_list && !resident)
                        })
                })
                .collect();
            assert_eq!(
                gone.data_streams()
                    .map(|stream| (
                        stream.name.map(|name| units(&name)),
                        stream.size,
                        stream.data_lost
                    ))
                    .collect::<Vec<_>>(),
                expected_streams,
                "file {number}: data_streams() of the deleted file"
            );
            assert_eq!(
                gone.stream_data_lost(),
                has_list,
                "file {number}: stream_data_lost()"
            );
            // The default stream's, the last one listed like `FileInfo::size`. A file with a list
            // whose default stream is not there at all lost it too (a size of 0 must not read as
            // an empty file); a directory has no data to lose.
            let default_lost = has_list
                && !file.is_directory()
                && expected_streams
                    .iter()
                    .rev()
                    .find(|stream| stream.0.is_none())
                    .is_none_or(|stream| stream.2);
            assert_eq!(
                FileInfo::new(&gone).data_lost,
                default_lost,
                "file {number}: data_lost"
            );
        }

        // Every parent of the live file is freed too, and a freed parent is named by the sequence
        // it had: a path that resolved while live resolves to the same path, complete.
        for (live_name, gone_name) in file.names().zip(gone.names()).take(3) {
            if let Some(path) = live.resolve_path(&live_name, &mut ()) {
                if !has_ordinary_parents(&live, &live_name) {
                    continue;
                }
                let resolved =
                    deleted.resolve_deleted_path(&gone_name, &mut DeletedPathCache::new());
                assert!(
                    resolved.complete && resolved.path == path,
                    "file {number}: the deleted path is not the live path: {resolved:?} against {path:?}"
                );
                stats.paths += 1;
            }
        }

        // The id of the live file is the id of the deleted one, and finds the freed record in both
        // volumes. The raw reference of the freed record is one sequence up, which names another
        // incarnation, and is not the id.
        let id = file.file_id();
        assert_eq!(
            live.record_by_id(id).map(|found| found.number()),
            Some(number)
        );
        let found = deleted
            .record_by_id(id)
            .expect("the id of the deleted file");
        assert_eq!((found.number(), found.is_used()), (number, false));
        assert_eq!(
            gone.file_id(),
            id,
            "file {number}: file_id() after the delete"
        );
        assert!(deleted
            .record_by_id(FileId::from(gone.reference()))
            .is_none());
        assert!(live.record_by_id(FileId::from(gone.reference())).is_none());

        if has_a_name_or_times(&file) {
            expected_deleted.push(number);
        }
    }

    // The deleted walk gives the same answer with any cache: one filled in record order, then the
    // same one in the reverse order (loops, too long chains and lost parents included).
    let names: Vec<(u64, NtfsFileName)> = (FIRST_NORMAL_RECORD..deleted.record_count())
        .filter_map(|number| deleted.record(number))
        .take(MAX_CHECKED_FILES)
        .flat_map(|file| {
            let number = file.number();
            file.names().take(3).map(move |name| (number, name))
        })
        .collect();
    let mut cache = DeletedPathCache::new();
    for (number, name) in names.iter().chain(names.iter().rev()) {
        assert!(
            deleted.resolve_deleted_path(name, &mut cache)
                == deleted.resolve_deleted_path(name, &mut DeletedPathCache::new()),
            "record {number}: a cache changed the deleted path"
        );
    }

    // `deleted_files()` returns each freed base once, in order, and for the files that were live
    // exactly the ones that hold something.
    let listed: Vec<NtfsFile> = deleted.deleted_files().collect();
    let numbers: Vec<u64> = listed.iter().map(|file| file.number()).collect();
    assert!(
        numbers.windows(2).all(|pair| pair[0] < pair[1]),
        "deleted_files() is not in order, or repeats: {numbers:?}"
    );
    for file in &listed {
        assert!(file.number() >= FIRST_NORMAL_RECORD);
        assert!(!file.is_used() && !file.is_extension() && !deleted.is_allocated(file.number()));
    }
    assert_eq!(
        numbers
            .iter()
            .copied()
            .filter(|number| live_numbers.contains(number))
            .collect::<Vec<_>>(),
        expected_deleted,
        "deleted_files() differs from the files that were live"
    );
    assert!(
        live.deleted_files()
            .all(|file| !live_numbers.contains(&file.number())),
        "deleted_files() yielded a live file"
    );
    stats
}

#[test]
fn the_deleted_view_of_a_file_is_its_live_view() {
    let files = AtomicUsize::new(0);
    let with_extensions = AtomicUsize::new(0);
    let paths = AtomicUsize::new(0);
    heavy_property(|u| {
        let image = Image::arbitrary(u)?;
        show_on_replay(&image);
        let stats = check_deleted_view(&image);
        files.fetch_add(stats.files, Ordering::Relaxed);
        with_extensions.fetch_add(stats.with_extensions, Ordering::Relaxed);
        paths.fetch_add(stats.paths, Ordering::Relaxed);
        Ok(())
    });
    // The search must reach the interesting cases, or it proves nothing.
    coverage("files compared", files.into_inner() as u64, 100);
    coverage(
        "files with extension records compared",
        with_extensions.into_inner() as u64,
        10,
    );
    coverage("deleted paths compared", paths.into_inner() as u64, 10);
}

/// A file with two live extension records, one that names another incarnation, and one freed one
/// that names the live base exactly, next to a file that was already deleted.
#[test]
fn the_deleted_view_is_the_live_view_for_a_file_with_extension_records() {
    let record = |flags, base, attributes| RecordSpec {
        sequence: 1,
        flags,
        base,
        attributes,
    };
    let name = |text: u8| AttributeSpec::FileName {
        parent: ParentSpec::Root(Claim::Right),
        namespace: NamespaceSpec::Win32,
        name: vec![text],
        reparse: false,
    };
    let link = |target, claim| Some(Link { target, claim });
    let image = Image {
        mft: record(0, None, vec![]),
        records: vec![
            // 24: the file: a name, standard information, a resident and a non-resident stream.
            record(
                0x3F,
                None,
                vec![
                    name(0),
                    AttributeSpec::StandardInformation,
                    AttributeSpec::Data {
                        stream: None,
                        vcn: 0,
                        storage: StorageSpec::Resident(5),
                    },
                ],
            ),
            // 25 and 26: live extension records of 24.
            record(0x3F, link(1, Claim::Right), vec![name(1)]),
            record(
                0x3F,
                link(1, Claim::Right),
                vec![AttributeSpec::Data {
                    stream: Some(0),
                    vcn: 0,
                    storage: StorageSpec::NonResident {
                        size: 4096,
                        runs: vec![],
                    },
                }],
            ),
            // 27: a live extension record that names another incarnation of 24.
            record(0x3F, link(1, Claim::Stale), vec![name(2)]),
            // 28: freed and not allocated, naming 24 exactly, with a name (not empty, so the
            // oracle has to turn it into an earlier incarnation's).
            record(0x00, link(1, Claim::Right), vec![name(3)]),
            // 29: a file that was already deleted, with an extension record 30.
            record(
                0x00,
                None,
                vec![name(4), AttributeSpec::StandardInformation],
            ),
            record(0x00, link(6, Claim::Right), vec![name(5)]),
        ],
        corruptions: vec![],
        order: vec![],
        deleted: vec![],
        wrap_skips_zero: false,
    };
    let stats = check_deleted_view(&image);
    assert_eq!((stats.files, stats.with_extensions), (1, 1));
}

// ---- The deleted view against a model of the description -------------------------------------------
//
// `check_deleted_view` compares deleted vs. live views for files deleted all at once. Unchecked
// there: files the image already had freed, a file deleted while its directory lives, and
// deleted-only facts (lost streams, `$Extend\$Deleted` paths, sequence wraps). Here, some files
// are deleted instead (`delete_some`, NTFS's way: flag off, bitmap bit clear, sequence up by one,
// for the base and every extension record naming it), and the crate's answers for every deleted
// file - already-freed ones included - are compared against a model built from the description
// alone (`deleted_model`).

/// The sequence a freed record with this stored sequence had while live. Freed at 0 or 1 is read as
/// live at 0xFFFF, because NTFS is documented to skip 0 at the wrap; what it really does was not
/// measured.
fn live_sequence(freed: u16) -> u16 {
    match freed {
        0 | 1 => 0xFFFF,
        stored => stored - 1,
    }
}

/// Whether `reference` names the freed record `base`: its number, and a sequence one below the one
/// it is stored with (which is what a delete leaves). A record stored with 1 was live at 0xFFFF
/// when 0 is skipped at the wrap, or at 0 when it is not: both name it.
fn names_freed(reference: u64, base: &Built) -> bool {
    let sequence = (reference >> 48) as u16;
    reference & RECORD_NUMBER_MASK == base.number
        && (base.sequence == sequence.wrapping_add(1) || (sequence == 0xFFFF && base.sequence == 1))
}

fn is_freed(record: &Built) -> bool {
    !record.in_use && !record.allocated && record.number != 0
}

fn is_live(record: &Built) -> bool {
    record.in_use && record.allocated && record.number != 0
}

/// What the description says about one deleted file.
#[derive(Debug)]
struct DeletedFile {
    number: u64,
    /// The base record and the freed extension records that name it, by number.
    members: Vec<u64>,
    names: Vec<Name>,
    /// Name, size and whether the stream lost its data, for every stream the file has.
    streams: Vec<(Option<Vec<u16>>, u64, bool)>,
    has_standard_information: bool,
    has_list: bool,
    is_directory: bool,
    /// The id the file had while live.
    id: u64,
}

impl DeletedFile {
    /// The default stream as `FileInfo` reports it: the last unnamed one listed.
    fn default_stream(&self) -> Option<&(Option<Vec<u16>>, u64, bool)> {
        self.streams.iter().rev().find(|stream| stream.0.is_none())
    }
}

/// The records of the live file whose base record is `number`, in the order the crate walks them:
/// the base, then the live extension records that name it exactly, by number. Empty when the base
/// is not live.
fn live_family(built: &[Built], number: u64) -> Vec<&Built> {
    let Some(base) = built.iter().find(|record| record.number == number) else {
        return Vec::new();
    };
    if !is_live(base) {
        return Vec::new();
    }
    std::iter::once(base)
        .chain(built.iter().filter(|other| {
            other.number != number
                && is_live(other)
                && other.base_reference != 0
                && other.base_reference == base.own_reference
        }))
        .collect()
}

/// Whether a corruption touched the record `number` or a record that names it as its base.
fn family_is_tainted(built: &[Built], tainted: &HashSet<u64>, number: u64) -> bool {
    built.iter().any(|other| {
        tainted.contains(&other.number)
            && (other.number == number || other.base_reference & RECORD_NUMBER_MASK == number)
    })
}

/// Every deleted file of the description: a freed base record (not an extension record, not below
/// the first normal record) that still holds a name or times in some record of the file. Files with
/// a record a corruption touched, or that names the base, are left out: the description no longer
/// says what they hold.
fn deleted_model(built: &[Built], tainted: &HashSet<u64>) -> Vec<DeletedFile> {
    let mut files = Vec::new();
    for base in built {
        if base.number < FIRST_NORMAL_RECORD || !is_freed(base) || base.base_reference != 0 {
            continue;
        }
        if family_is_tainted(built, tainted, base.number) {
            continue;
        }
        let mut members = vec![base];
        members.extend(built.iter().filter(|other| {
            other.number != base.number
                && is_freed(other)
                && other.base_reference != 0
                && names_freed(other.base_reference, base)
        }));
        let has_list = members.iter().any(|member| member.expected.has_list);
        let holds_something = members.iter().any(|member| {
            member.expected.standard_information || !member.expected.names.is_empty()
        });
        if !holds_something {
            continue;
        }
        let mut streams = Vec::new();
        for member in &members {
            for (stream, resident) in member
                .expected
                .streams
                .iter()
                .zip(&member.expected.resident)
            {
                streams.push((stream.0.clone(), stream.1, has_list && !resident));
            }
        }
        files.push(DeletedFile {
            number: base.number,
            members: members.iter().map(|member| member.number).collect(),
            names: members
                .iter()
                .flat_map(|member| member.expected.names.iter().cloned())
                .collect(),
            streams,
            has_standard_information: members
                .iter()
                .any(|member| member.expected.standard_information),
            has_list,
            is_directory: base.is_directory,
            id: reference(live_sequence(base.sequence), base.number),
        });
    }
    files
}

/// The volume after `delete_some`: the deleted volume, and the live one before it.
struct History<'a> {
    built: Vec<Built>,
    tainted: HashSet<u64>,
    root_reference: u64,
    live: Mft,
    deleted: Mft,
    /// The live files that were deleted.
    chosen: Vec<u64>,
    _image: std::marker::PhantomData<&'a Image>,
}

/// Deletes some of the live files of the image the way NTFS does: for each chosen live base record
/// (by `image.deleted`) and every live extension record that names it exactly, the in-use flag off,
/// the bitmap bit clear, the sequence up by one (0xFFFF wraps to 0, or to 1 when
/// `image.wrap_skips_zero`) and the log sequence number moved, and nothing else. The description's
/// records are updated the same way, so the model sees the state the bytes are in.
fn delete_some(image: &Image) -> Option<History<'_>> {
    let assembled = assemble(image)?;
    let Assembled {
        mut built,
        tainted,
        root_sequence,
        volume,
        mut data,
        mut bitmap,
        ..
    } = assembled;
    let live = build_from_parts(volume.clone(), data.clone(), bitmap.clone());

    let is_chosen = |index: usize| image.deleted.get(index).is_some_and(|byte| byte & 1 != 0);
    let bases: Vec<(u64, u64)> = built
        .iter()
        .enumerate()
        .filter(|(index, record)| {
            *index >= 1
                && is_live(record)
                && record.base_reference == 0
                && !tainted.contains(&record.number)
                && is_chosen(*index - 1)
        })
        .map(|(_, record)| (record.number, record.own_reference))
        .collect();
    let mut chosen = Vec::new();
    for (number, own_reference) in &bases {
        chosen.push(*number);
        let victims: Vec<usize> = built
            .iter()
            .enumerate()
            .filter(|(_, record)| {
                record.number == *number
                    || (is_live(record) && record.base_reference == *own_reference)
            })
            .map(|(index, _)| index)
            .collect();
        for index in victims {
            let record = &mut built[index];
            let sequence = match record.sequence {
                0xFFFF if image.wrap_skips_zero => 1,
                sequence => sequence.wrapping_add(1),
            };
            let bytes = &mut data[record.number as usize * RECORD_SIZE..][..RECORD_SIZE];
            delete_record(bytes);
            write_u16(bytes, 16, sequence);
            bitmap[record.number as usize / 8] &= !(1 << (record.number % 8));
            record.sequence = sequence;
            record.own_reference = reference(sequence, record.number);
            record.in_use = false;
            record.allocated = false;
            record.bytes.copy_from_slice(bytes);
        }
    }
    let deleted = build_from_parts(volume, data, bitmap);
    Some(History {
        built,
        tainted,
        root_reference: reference(root_sequence.unwrap_or(ROOT_SEQUENCE), ROOT_RECORD),
        live,
        deleted,
        chosen,
        _image: std::marker::PhantomData,
    })
}

/// What the model says a path is.
#[derive(Debug, PartialEq, Eq)]
enum ModelPath {
    /// The path as UTF-16, and whether the walk reached the volume.
    Path(Vec<u16>, bool),
    /// The description does not say (a loop, an extension record as a parent, a corrupted record).
    Unknown,
}

/// The names of the file whose base record is `number`, in the state its records are in: the live
/// ones when the base is live, the freed ones when it is freed. `None` when a corruption touched a
/// record of the file or the record is neither.
fn names_of_file(deleted: &[DeletedFile], number: u64, world: &World) -> Option<Vec<Name>> {
    let record = world.record(number)?;
    if is_freed(record) {
        deleted
            .iter()
            .find(|file| file.number == number)
            .map(|file| file.names.clone())
    } else if is_live(record) {
        world
            .names(number)
            .map(|names| names.into_iter().cloned().collect())
    } else {
        None
    }
}

/// The path `resolve_deleted_path` should give for a name with this parent and this leaf, worked out
/// from the description: a hop names a directory that is live (the same reference) or freed (the
/// sequence one above the reference's, wrapping), and takes its best name; anything else ends the
/// walk at `<lost N>`; a directory called `$Deleted` whose parent is record 11 ends it at
/// `<deleted>`; the root ends it at the volume when the reference is the root's.
fn model_path(
    history: &History,
    deleted: &[DeletedFile],
    world: &World,
    parent_reference: u64,
    leaf: &[u16],
) -> ModelPath {
    let mut components: Vec<Vec<u16>> = Vec::new();
    let mut seen = Vec::new();
    let mut reference = parent_reference;
    let marker = loop {
        let number = reference & RECORD_NUMBER_MASK;
        if number == ROOT_RECORD {
            if reference == history.root_reference {
                break None;
            }
            break Some(format!("<lost {number}>"));
        }
        if seen.contains(&reference) {
            return ModelPath::Unknown;
        }
        seen.push(reference);
        // Record 0 is `$MFT` itself, which the description does not model as a directory.
        if number == 0 {
            return ModelPath::Unknown;
        }
        let Some(record) = world.record(number) else {
            // Not one of the records (a reserved number, or past the end).
            break Some(format!("<lost {number}>"));
        };
        if history.tainted.contains(&number) {
            return ModelPath::Unknown;
        }
        if record.base_reference != 0 {
            // An extension record answers for its base's names: not modelled here.
            return ModelPath::Unknown;
        }
        let named = if is_live(record) {
            record.own_reference == reference
        } else if is_freed(record) {
            names_freed(reference, record)
        } else {
            false
        };
        if !record.is_directory || !named {
            break Some(format!("<lost {number}>"));
        }
        let Some(names) = names_of_file(deleted, number, world) else {
            return ModelPath::Unknown;
        };
        let is_win32 = |name: &&Name| {
            matches!(
                name.1,
                NtfsFileNamespace::Win32 | NtfsFileNamespace::Win32AndDos
            )
        };
        let Some(best) = names.iter().find(is_win32).or(names.first()) else {
            break Some(format!("<lost {number}>"));
        };
        let is_deleted_directory = best.0.len() == DELETED_NAME.len()
            && best.0.iter().zip(DELETED_NAME).all(|(&unit, expected)| {
                unit < 0x80 && (unit as u8).eq_ignore_ascii_case(&(expected as u8))
            });
        if is_deleted_directory && best.2 & RECORD_NUMBER_MASK == EXTEND_RECORD {
            break Some("<deleted>".to_string());
        }
        components.push(best.0.clone());
        reference = best.2;
    };

    let mut path: Vec<u16> = VOLUME_PATH.encode_utf16().collect();
    let complete = marker.is_none();
    let below = marker
        .iter()
        .map(|text| text.encode_utf16().collect::<Vec<u16>>())
        .chain(components.into_iter().rev())
        .chain(std::iter::once(leaf.to_vec()));
    for component in below {
        let ends_with_separator = path
            .last()
            .is_some_and(|&unit| unit < 0x80 && std::path::is_separator(unit as u8 as char));
        if !ends_with_separator {
            path.extend(MAIN_SEPARATOR_STR.encode_utf16());
        }
        path.extend_from_slice(&component);
    }
    ModelPath::Path(path, complete)
}

/// What `check_deleted_model` counted, so the test can tell the search reached each shape.
#[derive(Default)]
struct ModelStats {
    deleted_files: usize,
    already_freed: usize,
    newly_deleted: usize,
    with_extensions: usize,
    with_lost_streams: usize,
    with_resident_kept: usize,
    wrapped: usize,
    paths_complete: usize,
    paths_lost: usize,
    paths_deleted: usize,
    live_paths_after_partial_delete: usize,
}

impl ModelStats {
    fn add(&mut self, other: ModelStats) {
        self.deleted_files += other.deleted_files;
        self.already_freed += other.already_freed;
        self.newly_deleted += other.newly_deleted;
        self.with_extensions += other.with_extensions;
        self.with_lost_streams += other.with_lost_streams;
        self.with_resident_kept += other.with_resident_kept;
        self.wrapped += other.wrapped;
        self.paths_complete += other.paths_complete;
        self.paths_lost += other.paths_lost;
        self.paths_deleted += other.paths_deleted;
        self.live_paths_after_partial_delete += other.live_paths_after_partial_delete;
    }
}

/// A name as the model has it, from what the crate reports.
fn model_name(name: &NtfsFileName) -> Name {
    let (units, namespace, parent) = describe(name);
    (units, namespace.expect("a namespace"), parent)
}

/// Deletes some files of the image and checks, for every deleted file (already freed or just
/// deleted), the crate's answers against the model; the live oracle (`check`) runs over the same
/// volume for files still live, and a file deleted while its directory lives keeps the path it
/// had.
fn check_deleted_model(image: &Image) -> ModelStats {
    let mut stats = ModelStats::default();
    let Some(history) = delete_some(image) else {
        return stats;
    };
    let world = World {
        built: &history.built,
        tainted: &history.tainted,
        root_reference: history.root_reference,
    };
    let mft = &history.deleted;

    if std::env::var_os("MODEL_DEBUG").is_some() {
        eprintln!("chosen {:?}", history.chosen);
        for b in &history.built {
            eprintln!(
                "rec {} seq {} inuse {} alloc {} base {:#x} dir {} own {:#x} names {:x?} tainted {}",
                b.number, b.sequence, b.in_use, b.allocated, b.base_reference, b.is_directory,
                b.own_reference, b.expected.names.iter().map(|n| (n.0.clone(), n.2)).collect::<Vec<_>>(),
                history.tainted.contains(&b.number)
            );
        }
    }
    // The files that are still live are what the live oracle says, in a volume that has deleted
    // files next to them.
    check(mft, &world, &image.order);

    let model = deleted_model(&history.built, &history.tainted);
    let untouched: Vec<u64> = mft
        .deleted_files()
        .map(|file| file.number())
        .filter(|&number| !family_is_tainted(&history.built, &history.tainted, number))
        .collect();
    let modelled: Vec<u64> = model.iter().map(|file| file.number).collect();
    assert!(
        untouched == modelled,
        "deleted_files() is {untouched:?}, the description says {modelled:?}; the records that differ: {:#?}",
        history
            .built
            .iter()
            .filter(|b| untouched.contains(&b.number) != modelled.contains(&b.number))
            .map(|b| (
                b.number,
                b.sequence,
                b.in_use,
                b.allocated,
                b.base_reference,
                b.expected.names.len(),
                b.expected.standard_information,
                b.expected.has_list,
                history.tainted.contains(&b.number)
            ))
            .collect::<Vec<_>>()
    );

    let mut cache = DeletedPathCache::new();
    for expected in &model {
        let number = expected.number;
        let file = mft.record(number).expect("a deleted file has a record");
        let what = format!("deleted file {number}");
        stats.deleted_files += 1;
        if history.chosen.contains(&number) {
            stats.newly_deleted += 1;
        } else {
            stats.already_freed += 1;
        }
        stats.with_extensions += usize::from(expected.members.len() > 1);
        stats.with_lost_streams += usize::from(expected.streams.iter().any(|stream| stream.2));
        stats.with_resident_kept +=
            usize::from(expected.has_list && expected.streams.iter().any(|stream| !stream.2));
        let base = history.built.iter().find(|b| b.number == number).unwrap();
        stats.wrapped += usize::from(base.sequence <= 1);

        assert!(file.is_deleted() && !file.is_used(), "{what}");
        assert_eq!(
            file.records()
                .map(|record| record.number())
                .collect::<Vec<_>>(),
            expected.members,
            "{what}: records()"
        );
        assert_eq!(
            file.names()
                .map(|name| model_name(&name))
                .collect::<Vec<_>>(),
            expected.names,
            "{what}: names()"
        );
        let links: Vec<Name> = expected
            .names
            .iter()
            .filter(|name| name.1 != NtfsFileNamespace::Dos)
            .cloned()
            .collect();
        assert_eq!(
            file.hard_links()
                .map(|name| model_name(&name))
                .collect::<Vec<_>>(),
            links,
            "{what}: hard_links()"
        );
        let best = expected
            .names
            .iter()
            .find(|name| {
                matches!(
                    name.1,
                    NtfsFileNamespace::Win32 | NtfsFileNamespace::Win32AndDos
                )
            })
            .or(expected.names.first());
        assert_eq!(
            file.best_name().map(|name| units(&name.to_os_string())),
            best.map(|name| name.0.clone()),
            "{what}: best_name()"
        );
        assert_eq!(
            file.standard_information().is_some(),
            expected.has_standard_information,
            "{what}: standard_information()"
        );
        let streams: Vec<_> = file
            .data_streams()
            .map(|stream| {
                (
                    stream.name.map(|name| units(&name)),
                    stream.size,
                    stream.data_lost,
                )
            })
            .collect();
        assert_eq!(streams, expected.streams, "{what}: data_streams()");
        assert_eq!(
            file.stream_data_lost(),
            expected.has_list,
            "{what}: stream_data_lost()"
        );

        let info = FileInfo::new(&file);
        assert!(info.is_deleted, "{what}");
        assert_eq!(
            info.size,
            expected.default_stream().map_or(0, |stream| stream.1),
            "{what}: FileInfo size"
        );
        assert_eq!(
            info.data_lost,
            expected.has_list
                && !expected.is_directory
                && expected.default_stream().is_none_or(|stream| stream.2),
            "{what}: FileInfo data_lost"
        );

        // The id the file had while live finds it, and is the same however the file is reached.
        let id = FileId::from(expected.id);
        assert_eq!(file.file_id(), id, "{what}: file_id()");
        assert_eq!(
            mft.record_by_id(id).map(|found| found.number()),
            Some(number),
            "{what}: record_by_id(file_id())"
        );

        // The path of each name: the model's, and the same with a warm cache.
        for name in file.names().take(3) {
            let (leaf, _, parent) = describe(&name);
            let resolved = mft.resolve_deleted_path(&name, &mut cache);
            assert!(
                resolved == mft.resolve_deleted_path(&name, &mut DeletedPathCache::new()),
                "{what}: a cache changed the deleted path"
            );
            if let ModelPath::Path(path, complete) =
                model_path(&history, &model, &world, parent, &leaf)
            {
                assert_eq!(
                    (units(resolved.path.as_os_str()), resolved.complete),
                    (path, complete),
                    "{what}: the deleted path of {:?}",
                    name.to_string()
                );
                let text = resolved.path.to_string_lossy();
                stats.paths_complete += usize::from(resolved.complete);
                stats.paths_lost += usize::from(text.contains("<lost "));
                stats.paths_deleted += usize::from(text.contains("<deleted>"));
            }
        }
    }

    // A file deleted while its directories live keeps the path it had, complete. The live volume
    // says what that was.
    for number in &history.chosen {
        let (Some(gone), Some(before)) = (mft.record(*number), history.live.record(*number)) else {
            continue;
        };
        // An extension record freed before its base (empty on a real volume; a random image gives
        // it attributes) names the live base exactly, so once the base is freed it joins the
        // deleted file though it never belonged to the live one: the two name lists cannot be
        // compared position by position then.
        if !before
            .records()
            .map(|record| record.number())
            .eq(gone.records().map(|record| record.number()))
        {
            continue;
        }
        for (live_name, gone_name) in before.names().zip(gone.names()).take(3) {
            let Some(path) = history.live.resolve_path(&live_name, &mut ()) else {
                continue;
            };
            // The image can put an extension record where a directory is expected, and one that
            // names a base it does not belong to is a file's record with somebody else's names:
            // only base records are parents here.
            if !has_ordinary_parents(&history.live, &live_name)
                || !has_only_base_records_as_parents(&history.live, &live_name)
            {
                continue;
            }
            let resolved = mft.resolve_deleted_path(&gone_name, &mut DeletedPathCache::new());
            assert!(
                resolved.complete && resolved.path == path,
                "file {number}: the path after a partial delete is not the live path: {resolved:?} against {path:?}"
            );
            stats.live_paths_after_partial_delete += 1;
        }
    }
    stats
}

#[test]
fn the_deleted_view_is_what_the_description_says_for_a_partly_deleted_volume() {
    let stats = std::sync::Mutex::new(ModelStats::default());
    heavy_property(|u| {
        let image = Image::arbitrary(u)?;
        show_on_replay(&image);
        let found = check_deleted_model(&image);
        stats.lock().unwrap().add(found);
        Ok(())
    });
    let stats = stats.into_inner().unwrap();
    eprintln!(
        "deleted files {} (already freed {}, deleted here {}), with extensions {}, with lost streams {}, resident kept {}, wrapped {}, paths: complete {}, lost {}, deleted {}, under live directories {}",
        stats.deleted_files, stats.already_freed, stats.newly_deleted, stats.with_extensions,
        stats.with_lost_streams, stats.with_resident_kept, stats.wrapped, stats.paths_complete,
        stats.paths_lost, stats.paths_deleted, stats.live_paths_after_partial_delete
    );
    // The search must reach every shape the model is about, or it proves nothing.
    coverage("deleted files", stats.deleted_files as u64, 100);
    coverage("files the image had freed", stats.already_freed as u64, 20);
    coverage("files deleted by the test", stats.newly_deleted as u64, 20);
    coverage(
        "files with extension records",
        stats.with_extensions as u64,
        10,
    );
    coverage(
        "files that lost a stream",
        stats.with_lost_streams as u64,
        10,
    );
    coverage(
        "lost files with a resident stream kept",
        stats.with_resident_kept as u64,
        3,
    );
    coverage("files freed at the sequence wrap", stats.wrapped as u64, 5);
    coverage("complete deleted paths", stats.paths_complete as u64, 20);
    coverage("<lost N> deleted paths", stats.paths_lost as u64, 5);
    coverage("<deleted> paths", stats.paths_deleted as u64, 2);
    coverage(
        "files deleted under live directories",
        stats.live_paths_after_partial_delete as u64,
        10,
    );
}

/// What `remove_dir_all` leaves: a directory renamed below `$Extend\$Deleted` (a directory called
/// `$Deleted`, whose parent is record 11) under a random name and then deleted, and a file in it
/// that keeps its own name and its parent reference. The path of the file ends at `<deleted>`,
/// followed by the random name and the file's own.
#[test]
fn a_file_below_a_directory_renamed_under_extend_deleted_ends_its_path_at_deleted() {
    let record = |flags, attributes| RecordSpec {
        sequence: 1,
        flags,
        base: None,
        attributes,
    };
    let name = |parent, text: u8| AttributeSpec::FileName {
        parent,
        namespace: NamespaceSpec::Win32,
        name: vec![text],
        reparse: false,
    };
    let below = |target, claim| ParentSpec::Record(Link { target, claim });
    let image = Image {
        mft: record(0, vec![]),
        records: vec![
            // 24: `$Extend\$Deleted`, a live directory.
            record(
                0x7F,
                vec![AttributeSpec::DeletedName {
                    upper: false,
                    extend: true,
                }],
            ),
            // 25: the directory `d` renamed below it and deleted: freed, so stored with 2, and
            // named by the reference with sequence 1.
            record(0x40, vec![name(below(1, Claim::Right), 3)]),
            // 26: a live file `a` in it, deleted with the volume's partial delete.
            record(
                0x3F,
                vec![
                    name(below(2, Claim::Raw(1)), 0),
                    AttributeSpec::StandardInformation,
                ],
            ),
        ],
        corruptions: vec![],
        order: vec![],
        deleted: vec![0, 0, 1],
        wrap_skips_zero: false,
    };

    let history = delete_some(&image).expect("an image");
    assert_eq!(history.chosen, [26]);
    let file = history.deleted.record(26).expect("the file");
    assert!(file.is_deleted());
    let resolved = history.deleted.resolve_deleted_path(
        &file.best_name().expect("a name"),
        &mut DeletedPathCache::new(),
    );
    let expected: std::path::PathBuf = [VOLUME_PATH, "<deleted>", "d", "a"].iter().collect();
    assert_eq!((resolved.path, resolved.complete), (expected, false));

    let stats = check_deleted_model(&image);
    // The file, and the directory `d` (already freed) whose own path ends at `<deleted>` too.
    assert_eq!((stats.newly_deleted, stats.paths_deleted), (1, 2));
}

/// Offset of the last two bytes of the first sector.
const SECTOR_END: usize = 510;

/// What `read_data_fs` should make of record 0.
#[derive(Debug)]
enum MftRead {
    /// No such attribute.
    Absent,
    Value(Vec<u8>),
    /// An error: the runs point past the end of the reader.
    Fails,
}

/// What `Mft::read_data_fs` returns for `attribute_type` on record 0, by the description, when the
/// description settles it; `None` when it does not (see the header of this file). `data` is what
/// the reader holds, cluster `n` at byte `n * CLUSTER_SIZE`.
///
/// The crate scans record 0's unnamed attributes of the type, in order, and a resident first one
/// is the answer whatever follows. `$DATA` is also the map used to find extension records, decoded
/// first: only its resident, foreign-VCN and identity forms are settled here, not arbitrary runs.
fn expected_mft_read(
    specs: &[&RecordSpec],
    record0: &Built,
    attribute_type: NtfsAttributeType,
    data: &[u8],
) -> Option<MftRead> {
    let attributes: Vec<&AttributeSpec> = record0
        .written
        .iter()
        .map(|&position| &specs[0].attributes[position])
        .collect();
    if let Some(AttributeSpec::Data {
        vcn: 0,
        storage: StorageSpec::NonResident { .. },
        ..
    }) = attributes
        .iter()
        .copied()
        .find(|attribute| attribute.is_unnamed_data())
    {
        return None;
    }

    // Only the first attribute list is followed. An entry of the type that names another record
    // may reach an extension record, which the description here does not follow.
    let list = attributes.iter().find_map(|attribute| match attribute {
        AttributeSpec::AttributeList { entries, .. } => Some(entries),
        _ => None,
    });
    let reaches_another_record = list.is_some_and(|entries| {
        entries.iter().any(|entry| {
            entry.attribute_type() == attribute_type
                && !entry.named
                && link_target(&entry.target, specs).0 != 0
        })
    });
    if reaches_another_record {
        return None;
    }

    let own: Vec<&AttributeSpec> = attributes
        .iter()
        .copied()
        .filter(|attribute| match attribute_type {
            NtfsAttributeType::Data => attribute.is_unnamed_data(),
            _ => matches!(
                attribute,
                AttributeSpec::Other { kind, .. } if other_type(*kind) == attribute_type
            ),
        })
        .collect();
    match own.as_slice() {
        [] => Some(MftRead::Absent),
        // A resident first extent is the value. An identity mapping in an attribute other than
        // `$DATA` is written as a resident value (see `AttributeSpec::write`).
        [AttributeSpec::Data {
            storage: StorageSpec::Resident(length),
            ..
        }
        | AttributeSpec::Other {
            storage: StorageSpec::Resident(length) | StorageSpec::MftIdentity(length),
            ..
        }, ..] => Some(MftRead::Value(vec![0xAB; (*length % 64) as usize])),
        [AttributeSpec::Data {
            storage: StorageSpec::MftIdentity(clusters),
            ..
        }] => {
            let size = identity_clusters(*clusters) as usize * CLUSTER_SIZE;
            Some(match data.get(..size) {
                Some(bytes) => MftRead::Value(bytes.to_vec()),
                None => MftRead::Fails,
            })
        }
        _ => None,
    }
}

fn check(mft: &Mft, world: &World, order: &[u8]) {
    let (built, tainted) = (world.built, world.tainted);
    let by_number: HashMap<u64, &Built> = built.iter().map(|b| (b.number, b)).collect();

    // The cluster with a non-resident attribute list is read as records too.
    if tainted.is_empty() && built[0].list_area.is_none() {
        assert_eq!(
            mft.corrupt_records(),
            0,
            "a clean image reported corrupt records"
        );
    }

    // The files the description says exist, among the untouched records.
    let expected_files: Vec<u64> = built[1..]
        .iter()
        .filter(|b| b.in_use && b.allocated && b.base_reference == 0)
        .map(|b| b.number)
        .filter(|number| !tainted.contains(number))
        .collect();
    let files: Vec<NtfsFile> = mft.files().collect();
    let mut seen = HashSet::new();
    for file in &files {
        assert!(
            seen.insert(file.number()),
            "files() yielded {} twice",
            file.number()
        );
        assert!(
            !file.is_extension(),
            "files() yielded extension record {}",
            file.number()
        );
    }
    let actual_files: Vec<u64> = files
        .iter()
        .map(|file| file.number())
        .filter(|number| !tainted.contains(number))
        .collect();
    assert_eq!(
        actual_files, expected_files,
        "files() differs from the description"
    );

    for file in &files {
        let number = file.number();
        let records: Vec<u64> = file.records().map(|r| r.number()).collect();
        assert_eq!(
            records.first(),
            Some(&number),
            "file {number}: base record not first"
        );
        assert_eq!(
            records.iter().collect::<HashSet<_>>().len(),
            records.len(),
            "file {number}: a record was yielded twice: {records:?}"
        );

        // Invariant 1: hard links are the names minus the DOS aliases.
        let names: Vec<NtfsFileName> = file.names().collect();
        let links: Vec<_> = file.hard_links().map(|name| describe(&name)).collect();
        let without_dos: Vec<_> = names
            .iter()
            .filter(|name| name.namespace() != Some(NtfsFileNamespace::Dos))
            .map(describe)
            .collect();
        assert_eq!(
            links, without_dos,
            "file {number}: hard_links() is not names() minus DOS"
        );

        // What the description says the file holds, unless a record of it was
        // corrupted on purpose.
        let base = by_number[&number];
        let members: Vec<&Built> = std::iter::once(base)
            .chain(
                built[1..]
                    .iter()
                    .filter(|b| b.number != number)
                    .filter(|b| b.in_use && b.allocated)
                    .filter(|b| b.base_reference == base.own_reference),
            )
            .collect();
        let expected_records: Vec<u64> = members.iter().map(|b| b.number).collect();
        if records
            .iter()
            .chain(&expected_records)
            .any(|number| tainted.contains(number))
        {
            continue;
        }
        assert_eq!(
            records, expected_records,
            "file {number}: records() differs"
        );

        let expected_names: Vec<_> = members
            .iter()
            .flat_map(|b| b.expected.names.iter().cloned())
            .collect();
        let actual_names: Vec<_> = names
            .iter()
            .map(|name| {
                (
                    units(&name.to_os_string()),
                    name.namespace().expect("namespace"),
                    name.parent_reference(),
                )
            })
            .collect();
        assert_eq!(
            actual_names, expected_names,
            "file {number}: names() differs"
        );

        assert_eq!(
            file.reference(),
            base.own_reference,
            "file {number}: reference()"
        );
        assert_eq!(
            file.file_id(),
            FileId::from(base.own_reference),
            "file {number}: file_id()"
        );

        // Invariant 4: sizes come from the VCN-0 extent only.
        let expected_streams: Vec<_> = members
            .iter()
            .flat_map(|b| b.expected.streams.iter().cloned())
            .collect();
        let streams: Vec<_> = file
            .data_streams()
            .map(|stream| (stream.name.map(|name| units(&name)), stream.size))
            .collect();
        assert_eq!(
            streams, expected_streams,
            "file {number}: data_streams() differs"
        );

        // The default stream's bytes when resident, and the standard
        // information, come from the first such attribute of the file.
        let expected_resident = members
            .iter()
            .flat_map(|b| b.expected.unnamed_data.iter().copied())
            .next()
            .flatten();
        let resident = file.resident_data();
        assert_eq!(
            resident.map(<[u8]>::len),
            expected_resident,
            "file {number}: resident_data() differs"
        );
        assert!(
            resident.is_none_or(|data| data.iter().all(|&byte| byte == 0xAB)),
            "file {number}: resident_data() holds other bytes"
        );
        let information = file.standard_information();
        assert_eq!(
            information.map(|info| info.file_attributes()),
            members
                .iter()
                .any(|b| b.expected.standard_information)
                .then_some(0x20),
            "file {number}: standard_information() differs"
        );
    }

    check_paths(mft, world, &files, order);
}

/// Invariants 2 and 3: `resolve_path` against the description (unless a record
/// on the way was corrupted on purpose), uncached, with a cache in a
/// description-chosen order, and again in reverse order on the warm one. With or
/// without a cache the answer is the same.
fn check_paths(mft: &Mft, world: &World, files: &[NtfsFile], order: &[u8]) {
    let mut sequence: Vec<usize> = (0..files.len()).collect();
    if !order.is_empty() {
        sequence.sort_by_key(|&index| (order[index % order.len()], index));
    }
    sequence.truncate(MAX_CHECKED_FILES);

    // What `name`, the `index`th of `file`'s, should resolve to.
    let expected = |file: &NtfsFile, index: usize| match world.names(file.number()) {
        Some(names) => {
            let (leaf, _, parent) = names[index];
            world.resolve(*parent, leaf)
        }
        None => Outcome::Unknown,
    };
    let check_one =
        |file: &NtfsFile, index: usize, path: Option<PathBuf>, what: &str| match expected(
            file, index,
        ) {
            Outcome::Path(units_expected) => assert_eq!(
                path.as_deref().map(|path| units(path.as_os_str())),
                Some(units_expected),
                "file {}: {what} resolve_path differs from the description",
                file.number()
            ),
            Outcome::Unresolved => assert_eq!(
                path,
                None,
                "file {}: {what} resolve_path resolved what the description says cannot",
                file.number()
            ),
            Outcome::Unknown => {}
        };

    let mut cache = DefaultPathCache::new();
    for &index in &sequence {
        let file = &files[index];
        let number = file.number();

        let cached = FileInfo::with_cache(file, &mut cache);
        let plain = FileInfo::new(file);
        assert_eq!(
            cached.path, plain.path,
            "file {number}: FileInfo path differs with a cache"
        );
        assert_eq!(
            (
                cached.name.as_str(),
                cached.is_directory,
                cached.size,
                cached.file_attributes
            ),
            (
                plain.name.as_str(),
                plain.is_directory,
                plain.size,
                plain.file_attributes
            ),
            "file {number}: FileInfo differs with a cache"
        );
        assert_eq!(
            (cached.created, cached.accessed, cached.modified),
            (plain.created, plain.accessed, plain.modified),
            "file {number}: FileInfo times differ with a cache"
        );
        let best = file.best_name();
        assert_eq!(
            plain.name,
            best.map(|name| name.to_string()).unwrap_or_default(),
            "file {number}: FileInfo name is not best_name"
        );
        // The size is the default stream's: the last unnamed one that has one.
        let default_size = file
            .data_streams()
            .filter(|stream| stream.name.is_none())
            .last()
            .map_or(0, |stream| stream.size);
        assert_eq!(
            plain.size, default_size,
            "file {number}: FileInfo size is not the default stream's"
        );

        for (position, name) in file.names().take(3).enumerate() {
            let uncached = mft.resolve_path(&name, &mut ());
            check_one(file, position, uncached.clone(), "uncached");
            let with_cache = mft.resolve_path(&name, &mut cache);
            check_one(file, position, with_cache.clone(), "cached");
            assert_eq!(
                uncached, with_cache,
                "file {number}: a cache changed what resolve_path returns"
            );
        }
    }
    for &index in sequence.iter().rev() {
        let file = &files[index];
        for (position, name) in file.names().take(3).enumerate() {
            let with_cache = mft.resolve_path(&name, &mut cache);
            check_one(file, position, with_cache.clone(), "warm-cache");
            assert_eq!(
                with_cache,
                mft.resolve_path(&name, &mut ()),
                "file {}: a warm cache changed what resolve_path returns",
                file.number()
            );
        }
    }
}
