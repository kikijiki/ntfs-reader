// Copyright (c) 2022, Matteo Bernacchia <dev@kikijiki.com>. All rights reserved.
// This project is dual licensed under the Apache License 2.0 and the MIT license.
// See the LICENSE files in the project root for details.

use super::*;
use std::cell::Cell;
use std::io::Cursor;
use std::rc::Rc;

const ALIGNMENT: u64 = 4096;

#[derive(Default)]
struct Stats {
    reads: Cell<usize>,
    misaligned: Cell<usize>,
}

/// Counts inner reads and flags any read that is not block aligned, as a
/// raw volume handle would reject it.
struct Counting {
    inner: Cursor<Vec<u8>>,
    stats: Rc<Stats>,
}

impl Read for Counting {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let stats = &self.stats;
        stats.reads.set(stats.reads.get() + 1);
        if !self.inner.position().is_multiple_of(ALIGNMENT)
            || !(buf.len() as u64).is_multiple_of(ALIGNMENT)
        {
            stats.misaligned.set(stats.misaligned.get() + 1);
        }
        self.inner.read(buf)
    }
}

impl Seek for Counting {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.inner.seek(pos)
    }
}

fn data(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

fn reader(len: usize) -> (AlignedReader<Counting>, Rc<Stats>, Vec<u8>) {
    let bytes = data(len);
    let stats = Rc::new(Stats::default());
    let inner = Counting {
        inner: Cursor::new(bytes.clone()),
        stats: stats.clone(),
    };
    let reader = AlignedReader::new(inner, ALIGNMENT).expect("reader");
    (reader, stats, bytes)
}

#[test]
fn unaligned_reads_match_source() {
    let (mut reader, stats, bytes) = reader(8 * 4096);
    let cases = [
        (0usize, 10usize),
        (100, 4096),
        (4000, 200),
        (4095, 1),
        (4096, 8192),
        (12345, 6789),
        (1, 3 * 4096),
    ];
    for (position, len) in cases {
        reader.seek(SeekFrom::Start(position as u64)).unwrap();
        let mut buf = vec![0u8; len];
        reader.read_exact(&mut buf).unwrap();
        assert_eq!(buf, bytes[position..position + len], "at {position}+{len}");
    }

    reader.seek(SeekFrom::Current(-5000)).unwrap();
    let position = 1 + 3 * 4096 - 5000;
    let mut buf = vec![0u8; 300];
    reader.read_exact(&mut buf).unwrap();
    assert_eq!(buf, bytes[position..position + 300]);

    assert_eq!(stats.misaligned.get(), 0, "inner reads must stay aligned");
}

/// Records the largest read it was asked for, and gives back what it has.
struct Largest {
    inner: Cursor<Vec<u8>>,
    largest: usize,
}

impl Read for Largest {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.largest = self.largest.max(buf.len());
        self.inner.read(buf)
    }
}

impl Seek for Largest {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.inner.seek(pos)
    }
}

// std clamps a read's length to `u32::MAX` before handing it to Windows. 4 GiB is not a
// multiple of the sector size, so a raw-volume read that size or larger fails with
// ERROR_INVALID_PARAMETER. Each inner read is capped at `MAX_DIRECT_READ`; `read_exact`
// covers the rest with further reads, so a `$MFT` run of 4 GiB or more still loads.
#[test]
fn a_large_read_reaches_the_volume_in_pieces_of_at_most_64_mib() {
    let len = 3 * MAX_DIRECT_READ + 5 * 4096;
    let bytes = data(len);
    let mut reader = AlignedReader::new(
        Largest {
            inner: Cursor::new(bytes.clone()),
            largest: 0,
        },
        ALIGNMENT,
    )
    .unwrap();
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).unwrap();
    assert!(buf == bytes, "the bytes are not the source's");
    assert_eq!(reader.inner.largest, MAX_DIRECT_READ);
    assert_eq!(MAX_DIRECT_READ % ALIGNMENT as usize, 0);
}

#[test]
fn read_at_or_past_the_end_returns_zero() {
    for position in [2 * 4096, 100_000] {
        let (mut reader, _, _) = reader(2 * 4096);
        reader.seek(SeekFrom::Start(position)).unwrap();
        let mut buf = [0u8; 16];
        assert_eq!(reader.read(&mut buf).unwrap(), 0, "at {position}");
    }
}

#[test]
fn read_to_end_of_unaligned_source() {
    let (mut reader, _, bytes) = reader(4096 + 904);
    let mut out = Vec::new();
    reader.read_to_end(&mut out).unwrap();
    assert_eq!(out, bytes);
}

#[test]
fn empty_read_does_no_io() {
    let (mut reader, stats, _) = reader(2 * 4096);
    reader.seek(SeekFrom::Start(100)).unwrap();
    assert_eq!(reader.read(&mut []).unwrap(), 0);
    assert_eq!(stats.reads.get(), 0);
}

#[test]
fn large_aligned_read_is_one_inner_read() {
    let (mut reader, stats, bytes) = reader(2 << 20);
    reader.seek(SeekFrom::Start(4096)).unwrap();
    let mut buf = vec![0u8; 1 << 20];
    assert_eq!(reader.read(&mut buf).unwrap(), 1 << 20);
    assert_eq!(stats.reads.get(), 1);
    assert_eq!(buf, bytes[4096..4096 + (1 << 20)]);
    assert_eq!(stats.misaligned.get(), 0, "inner reads must stay aligned");
}

#[test]
fn large_unaligned_read_uses_few_inner_reads() {
    let (mut reader, stats, bytes) = reader(2 << 20);
    reader.seek(SeekFrom::Start(100)).unwrap();
    let mut buf = vec![0u8; 1 << 20];
    reader.read_exact(&mut buf).unwrap();
    assert_eq!(buf, bytes[100..100 + (1 << 20)]);
    // Head block, aligned middle span, tail block.
    assert!(stats.reads.get() <= 3, "{} inner reads", stats.reads.get());
    assert_eq!(stats.misaligned.get(), 0, "inner reads must stay aligned");
}

/// Returns at most `chunk` bytes per read, as `Read` allows before EOF
/// (issue #20). Optionally fails the read with index `fail_at`.
struct Chunked {
    inner: Cursor<Vec<u8>>,
    chunk: usize,
    reads: usize,
    fail_at: Option<usize>,
}

impl Read for Chunked {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let index = self.reads;
        self.reads += 1;
        if self.fail_at == Some(index) {
            return Err(io::Error::other("injected failure"));
        }
        let len = buf.len().min(self.chunk);
        self.inner.read(&mut buf[..len])
    }
}

impl Seek for Chunked {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.inner.seek(pos)
    }
}

fn chunked(len: usize, chunk: usize) -> (AlignedReader<Chunked>, Vec<u8>) {
    let bytes = data(len);
    let inner = Chunked {
        inner: Cursor::new(bytes.clone()),
        chunk,
        reads: 0,
        fail_at: None,
    };
    let reader = AlignedReader::new(inner, ALIGNMENT).expect("reader");
    (reader, bytes)
}

#[test]
fn short_inner_read_in_the_cache_is_not_eof() {
    // The scenario from issue #20.
    let (mut reader, bytes) = chunked(8192, 1024);
    reader.seek(SeekFrom::Start(100)).unwrap();
    let mut buf = vec![0u8; 2000];
    reader.read_exact(&mut buf).unwrap();
    assert_eq!(buf, bytes[100..2100]);
}

#[test]
fn short_direct_read_is_not_followed_by_eof() {
    // The direct read comes back short and leaves the position
    // unaligned; the next call goes through the cache.
    let (mut reader, bytes) = chunked(8192, 1024);
    let mut buf = vec![0u8; 8192];
    reader.read_exact(&mut buf).unwrap();
    assert_eq!(buf, bytes);
}

#[test]
fn short_inner_reads_match_source() {
    for chunk in [1, 1000, 1024, 4095, 5000] {
        let (mut reader, bytes) = chunked(4 * 4096 + 904, chunk);
        let mut out = Vec::new();
        reader.read_to_end(&mut out).unwrap();
        assert_eq!(out, bytes, "read_to_end, chunk {chunk}");

        for (position, len) in [(100usize, 2000usize), (4095, 5000), (12345, 4000)] {
            reader.seek(SeekFrom::Start(position as u64)).unwrap();
            let mut buf = vec![0u8; len];
            reader.read_exact(&mut buf).unwrap();
            assert_eq!(
                buf,
                bytes[position..position + len],
                "chunk {chunk} at {position}+{len}"
            );
        }
    }
}

#[test]
fn failed_cache_fill_does_not_leave_a_stale_block() {
    let (mut reader, bytes) = chunked(8192, 1024);
    let mut buf = [0u8; 10];
    reader.seek(SeekFrom::Start(100)).unwrap();
    reader.read_exact(&mut buf).unwrap();

    // Fill block 1: the first chunk lands in the cache, the second fails.
    reader.inner.fail_at = Some(reader.inner.reads + 1);
    reader.seek(SeekFrom::Start(4096 + 100)).unwrap();
    assert!(reader.read(&mut buf).is_err());

    // Block 0 must be read again, not served from the half-overwritten cache.
    reader.seek(SeekFrom::Start(100)).unwrap();
    reader.read_exact(&mut buf).unwrap();
    assert_eq!(buf, bytes[100..110]);
}
