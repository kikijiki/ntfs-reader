// Copyright (c) 2022, Matteo Bernacchia <dev@kikijiki.com>. All rights reserved.
// This project is dual licensed under the Apache License 2.0 and the MIT license.
// See the LICENSE files in the project root for details.

use std::fs::File;
use std::io::{self, BufReader};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// The most bytes a single read of the inner reader is asked for: 64 MiB, a multiple of every
/// alignment used. Windows fails a read of 4 GiB or more on a raw volume (see the tests).
const MAX_DIRECT_READ: usize = 64 << 20;

/// Reads from `inner` only at block-aligned offsets, in whole blocks, as a raw volume handle
/// requires.
///
/// That holds as long as `inner` returns whole blocks before EOF, as a volume handle does. A
/// short read while filling the cache continues from where it stopped: correct for any `Read`,
/// but no longer aligned.
pub struct AlignedReader<R>
where
    R: Read + Seek,
{
    inner: R,
    alignment: u64,
    position: u64,

    // The one-block cache, used only for an unaligned head/tail or a request smaller than a full
    // block. `buffer_valid` distinguishes "no block fetched yet" from "fetched a block whose size
    // happens to be 0" (a fresh reader and a cached block at true EOF both have `buffer_size ==
    // 0`, but only the latter should skip a refetch).
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

        // A request starting on a block boundary and covering at least one full block reads the
        // aligned span directly into the caller's buffer, bypassing the one-block cache, in one
        // read of at most `MAX_DIRECT_READ` bytes (the caller's `read_exact` continues). Any
        // unaligned remainder (before this boundary or after the last full block) is left for a
        // later call to pick up through the cache below.
        if start == 0 && buf.len() >= self.alignment as usize {
            let direct_len = (self.round_down(buf.len() as u64) as usize).min(MAX_DIRECT_READ);
            self.inner.seek(SeekFrom::Start(aligned_position))?;
            let got = self.inner.read(&mut buf[..direct_len])?;
            self.position += got as u64;
            return Ok(got);
        }

        if !self.buffer_valid || aligned_position != self.buffer_pos {
            // Invalidate first: an error part way through the fill leaves the
            // buffer holding pieces of two blocks.
            self.buffer_valid = false;
            self.inner.seek(SeekFrom::Start(aligned_position))?;
            self.buffer.resize(self.alignment as usize, 0u8);
            // A short read is not EOF (issue #20): keep reading until the
            // block is full or the inner reader returns 0.
            let mut filled = 0;
            while filled < self.buffer.len() {
                match self.inner.read(&mut self.buffer[filled..]) {
                    Ok(0) => break,
                    Ok(n) => filled += n,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(e),
                }
            }
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
#[path = "tests/aligned_reader.rs"]
mod tests;
