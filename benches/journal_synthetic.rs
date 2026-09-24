//! Timing of the USN record parser on synthetic `FSCTL_READ_USN_JOURNAL` buffers. No real journal
//! or volume needed: `Journal::read_sized` reaches the parser only after a real `DeviceIoControl`
//! call, so `parse_usn_records_bench` is the entry point that runs it on its own. The records are
//! laid out by hand in the `USN_RECORD_V2` format, so this runs on any platform.

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use ntfs_reader::internals::parse_usn_records_bench;
use ntfs_reader::Reason;
use std::hint::black_box;

const RECORD_COUNTS: [usize; 3] = [16, 256, 4_096];

// Byte offsets in a `USN_RECORD_V2`; the name follows the 60-byte header.
const V2_FILE_REFERENCE: usize = 8;
const V2_PARENT_REFERENCE: usize = 16;
const V2_USN: usize = 24;
const V2_REASON: usize = 40;
const V2_FILE_ATTRIBUTES: usize = 52;
const V2_NAME_LENGTH: usize = 56;
const V2_NAME_OFFSET: usize = 58;
const V2_HEADER: usize = 60;
const FILE_ATTRIBUTE_ARCHIVE: u32 = 0x20;

/// One `FSCTL_READ_USN_JOURNAL` reply: an 8-byte "next USN" prefix (which the parser skips)
/// followed by `record_count` V2 records, each named `change-<i>.dat` and 8-byte aligned as the
/// kernel always emits them.
fn build_usn_buffer(record_count: usize) -> (Vec<u8>, u32) {
    let mut buffer = vec![0u8; 8];

    for index in 0..record_count {
        let name: Vec<u16> = format!("change-{index}.dat").encode_utf16().collect();
        let name_len = name.len() * 2;
        let record_len = (V2_HEADER + name_len).next_multiple_of(8);

        let mut entry = vec![0u8; record_len];
        let mut put = |at: usize, value: &[u8]| entry[at..at + value.len()].copy_from_slice(value);
        put(0, &(record_len as u32).to_le_bytes());
        put(4, &2u16.to_le_bytes());
        put(
            V2_FILE_REFERENCE,
            &((1u64 << 48) | (1_000 + index as u64)).to_le_bytes(),
        );
        put(V2_PARENT_REFERENCE, &((1u64 << 48) | 5).to_le_bytes());
        put(V2_USN, &(index as i64 * 64).to_le_bytes());
        put(V2_REASON, &Reason::FILE_CREATE.bits().to_le_bytes());
        put(V2_FILE_ATTRIBUTES, &FILE_ATTRIBUTE_ARCHIVE.to_le_bytes());
        put(V2_NAME_LENGTH, &(name_len as u16).to_le_bytes());
        put(V2_NAME_OFFSET, &(V2_HEADER as u16).to_le_bytes());
        for (slot, unit) in name.iter().enumerate() {
            put(V2_HEADER + slot * 2, &unit.to_le_bytes());
        }
        buffer.extend_from_slice(&entry);
    }

    let bytes_returned = buffer.len() as u32;
    (buffer, bytes_returned)
}

fn bench_parse_usn_records(c: &mut Criterion) {
    let mut group = c.benchmark_group("parse_usn_records");
    for &count in &RECORD_COUNTS {
        let (buffer, bytes_returned) = build_usn_buffer(count);
        group.bench_with_input(BenchmarkId::new("records", count), &buffer, |b, buffer| {
            b.iter(|| black_box(parse_usn_records_bench(buffer, bytes_returned).expect("parse")))
        });
    }
    group.finish();
}

criterion_group!(benches, bench_parse_usn_records);
criterion_main!(benches);
