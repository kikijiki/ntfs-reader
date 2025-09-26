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

    buffer_pos: u64,
    buffer_size: usize,
    buffer: Vec<u8>,
}

impl<R> AlignedReader<R>
where
    R: Read + Seek,
{
    pub fn new(inner: R, alignment: u64) -> io::Result<Self> {
        assert!(alignment.is_power_of_two());

        Ok(Self {
            inner,
            alignment,
            position: 0,
            buffer_pos: 0,
            buffer_size: 0,
            buffer: Vec::with_capacity(alignment as usize),
        })
    }

    fn round_down(&self, n: u64) -> u64 {
        n / self.alignment * self.alignment
    }

    fn round_up(&self, n: u64) -> u64 {
        if n.is_multiple_of(self.alignment) {
            n
        } else {
            self.round_down(n) + self.alignment
        }
    }
}

impl<R> Read for AlignedReader<R>
where
    R: Read + Seek,
{
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let aligned_position = self.round_down(self.position);

        let start = self.position as usize - aligned_position as usize;
        let end = start + buf.len();
        let size = self.round_up(end as u64) as usize;

        if aligned_position != self.buffer_pos || size > self.buffer_size {
            self.inner.seek(SeekFrom::Start(aligned_position))?;
            self.buffer.resize(size, 0u8);
            self.inner.read_exact(&mut self.buffer)?;
            self.buffer_pos = aligned_position;
            self.buffer_size = size;
        }

        buf.copy_from_slice(&self.buffer[start..end]);

        self.position += buf.len() as u64;
        Ok(buf.len())
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
