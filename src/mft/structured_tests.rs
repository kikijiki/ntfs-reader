// Property test of `Mft::from_parts` and `Mft::read_data_fs` (card 035, card 041): the input is an
// `arbitrary`-derived description of an MFT (records, names, parent references, streams, extension
// records, a few deliberate corruptions), built into bytes with the `test_records` helpers, then
// checked against invariants instead of only "no panic". See `crate::property` for how to run it
// for longer and how to replay a failing seed.
//
// `read_data_fs` on record 0 is checked against the description where the description settles the
// answer (`expected_mft_read`): the first extent is resident, or the value is absent, or `$DATA`
// is one identity-mapped run. Where it does not (a `$DATA` map with arbitrary runs, a list entry
// that reaches an extension record, extents to join), it is checked for not panicking only; the
// unit tests in `mft.rs` cover those.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::io::Cursor;
use std::path::{PathBuf, MAIN_SEPARATOR_STR};

use arbitrary::Arbitrary;

use super::test_records::*;
use super::Mft;
use crate::api::{
    FileId, NtfsAttributeType, NtfsFileName, NtfsFileNamespace, FIRST_NORMAL_RECORD, ROOT_RECORD,
};
use crate::file::NtfsFile;
use crate::file_info::FileInfo;
use crate::path::DefaultPathCache;
use crate::property::{list, property, show_on_replay};

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
/// bound only rejects a `$DATA` larger than the volume itself (card 028), so a small one keeps a
/// description that claims gigabytes from attempting the allocation.
const VOLUME_SIZE: u64 = 64 * 1024 * 1024;
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
    fn sequence(&self) -> u16 {
        (self.sequence % 4) as u16
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
    /// Every unnamed `$DATA` attribute in order: its length if resident.
    unnamed_data: Vec<Option<usize>>,
    standard_information: bool,
}

struct Built {
    number: u64,
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
                match storage {
                    StorageSpec::Resident(length) => {
                        expected.streams.push((name, (*length % 64) as u64))
                    }
                    // Only the VCN-0 extent carries the size.
                    StorageSpec::NonResident { size, .. } if *vcn == 0 => {
                        expected.streams.push((name, *size as u64))
                    }
                    StorageSpec::NonResident { .. } => {}
                    StorageSpec::MftIdentity(clusters) => expected.streams.push((
                        None,
                        identity_clusters(*clusters) as u64 * CLUSTER_SIZE as u64,
                    )),
                }
            }
            AttributeSpec::StandardInformation => expected.standard_information = true,
            AttributeSpec::Other { .. } | AttributeSpec::AttributeList { .. } => {}
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

    /// The `$FILE_NAME`s of the file that record `number` belongs to, in the
    /// order the crate walks them: the base record's, then those of every
    /// in-use extension record whose base reference names the base exactly.
    /// `None` if any record of the family was corrupted on purpose.
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

    /// Walks the parent chain of a name with these parents by record number,
    /// from the description alone: every hop needs an in-use, allocated
    /// record with the same full reference (sequence included), directory
    /// names are the file's `best_name` (the first Win32 name, else the first),
    /// the chain ends at the root only if the reference matches the root's,
    /// and a chain that revisits a reference never ends.
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
    property(|u| {
        let image = Image::arbitrary(u)?;
        show_on_replay(&image);
        check_image(&image);
        Ok(())
    });
}

/// A base reference fault turns record 25 into an extension record of the directory at 24, so the
/// crate reports the directory with two names (and prefers the Win32 one) where the description
/// gives it one. The directory is not corrupted itself, so only tainting the record the reference
/// names keeps the oracle from answering for it (it indexed past its names).
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
    };

    check_image(&image);
}

fn check_image(image: &Image) {
    if image.records.is_empty() {
        return;
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

    // The `$MFT` bootstrap path, over the same bytes, through a reader that maps a cluster number
    // to the same offset. Record 0 as it is in the image (update sequence protection and all) must
    // not panic it; the record as the loader hands it over is checked against the description.
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
