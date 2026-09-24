// Copyright (c) 2022, Matteo Bernacchia <dev@kikijiki.com>. All rights reserved.
// This project is dual licensed under the Apache License 2.0 and the MIT license.
// See the LICENSE files in the project root for details.

use std::fs::File;
use std::io::{self, BufReader};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

pub struct AlignedReader<R>
where
    R: Read + Seek,
{
    inner: R,
    alignment: u64,
    position: u64,

    // The one-block cache used only for an unaligned head/tail, or any
    // request smaller than a full block. `buffer_valid` distinguishes "no
    // block fetched yet" from "fetched a block whose size happens to be 0"
    // (a fresh reader and a cached block at true EOF both have
    // `buffer_size == 0`, but only the latter should skip a refetch).
    buffer_pos: u64,
    buffer_size: usize,
    buffer_valid: bool,
    buffer: Vec<u8>,
}

impl<R> AlignedReader<R>
where
    R: Read + Seek,
{
    pub fn new(inner: R, alignment: u64) -> io::Result<Self> {
        if !alignment.is_power_of_two() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "alignment must be a power of two",
            ));
        }

        Ok(Self {
            inner,
            alignment,
            position: 0,
            buffer_pos: 0,
            buffer_size: 0,
            buffer_valid: false,
            buffer: Vec::with_capacity(alignment as usize),
        })
    }

    fn round_down(&self, n: u64) -> u64 {
        n / self.alignment * self.alignment
    }
}

impl<R> Read for AlignedReader<R>
where
    R: Read + Seek,
{
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }

        let aligned_position = self.round_down(self.position);
        let start = (self.position - aligned_position) as usize;

        // A request that starts on a block boundary and covers at least one
        // full block reads the aligned span directly into the caller's
        // buffer, bypassing the one-block cache entirely. Any unaligned
        // remainder (before this boundary or after the last full block) is
        // left for a later call to pick up through the cache below.
        if start == 0 && buf.len() >= self.alignment as usize {
            let direct_len = self.round_down(buf.len() as u64) as usize;
            self.inner.seek(SeekFrom::Start(aligned_position))?;
            let got = self.inner.read(&mut buf[..direct_len])?;
            self.position += got as u64;
            return Ok(got);
        }

        if !self.buffer_valid || aligned_position != self.buffer_pos {
            self.inner.seek(SeekFrom::Start(aligned_position))?;
            self.buffer.resize(self.alignment as usize, 0u8);
            let filled = self.inner.read(&mut self.buffer)?;
            self.buffer_pos = aligned_position;
            self.buffer_size = filled;
            self.buffer_valid = true;
        }

        // `buffer_size` may be short of a full block at true EOF; a request
        // past the end of what was actually filled returns 0, not an error.
        let available = self.buffer_size.saturating_sub(start);
        let to_read = buf
            .len()
            .min(self.alignment as usize - start)
            .min(available);
        buf[..to_read].copy_from_slice(&self.buffer[start..start + to_read]);

        self.position += to_read as u64;
        Ok(to_read)
    }
}

impl<R> Seek for AlignedReader<R>
where
    R: Read + Seek,
{
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let raw_position = match pos {
            SeekFrom::Start(n) => Some(n),
            SeekFrom::End(_) => {
                return Err(io::Error::other("unsupported"));
            }
            SeekFrom::Current(n) => {
                if n >= 0 {
                    self.position.checked_add(n as u64)
                } else {
                    self.position.checked_sub(n.wrapping_neg() as u64)
                }
            }
        };

        match raw_position {
            Some(n) => {
                let aligned_position = self.round_down(n);
                self.inner.seek(SeekFrom::Start(aligned_position))?;
                self.position = n;
                Ok(n)
            }
            None => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid position",
            )),
        }
    }
}

pub fn open_volume(path: &Path) -> std::io::Result<BufReader<AlignedReader<File>>> {
    let file = File::open(path)?;
    let sr = AlignedReader::new(file, 4096u64)?;
    let mut reader = BufReader::new(sr);

    reader.seek(SeekFrom::Start(0))?;
    Ok(reader)
}

#[cfg(test)]
mod tests {
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
}
