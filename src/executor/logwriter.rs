//! Size-capped output log (design 7.5): keeps the head of the output in the
//! file, keeps the most recent tail in memory once the head is full, and
//! appends the tail when the stream ends. Reading never stops, so the
//! writing process never sees a closed pipe because of the cap.

use std::collections::VecDeque;
use std::fs::File;
use std::io::Write;
use std::path::Path;

pub struct CappedLog {
    file: File,
    head_left: usize,
    tail: VecDeque<u8>,
    tail_cap: usize,
    dropped: u64,
}

impl CappedLog {
    pub fn create(path: &Path, head: usize, tail: usize) -> std::io::Result<Self> {
        Ok(Self {
            file: File::create(path)?,
            head_left: head,
            tail: VecDeque::new(),
            tail_cap: tail,
            dropped: 0,
        })
    }

    pub fn write(&mut self, mut data: &[u8]) -> std::io::Result<()> {
        if self.head_left > 0 {
            let n = data.len().min(self.head_left);
            self.file.write_all(&data[..n])?;
            self.head_left -= n;
            data = &data[n..];
        }
        for &b in data {
            if self.tail.len() == self.tail_cap {
                self.tail.pop_front();
                self.dropped += 1;
            }
            self.tail.push_back(b);
        }
        Ok(())
    }

    /// Writes the kept tail, with a marker for the omitted middle.
    pub fn finish(mut self) -> std::io::Result<()> {
        if self.dropped > 0 {
            writeln!(self.file, "\n[ats: {} bytes omitted]", self.dropped)?;
        }
        let (a, b) = self.tail.as_slices();
        self.file.write_all(a)?;
        self.file.write_all(b)?;
        self.file.sync_all()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_head_and_tail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.log");
        let mut log = CappedLog::create(&path, 4, 3).unwrap();
        log.write(b"abcdefgh").unwrap();
        log.write(b"ij").unwrap();
        log.finish().unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "abcd\n[ats: 3 bytes omitted]\nhij"
        );
    }

    #[test]
    fn short_output_is_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.log");
        let mut log = CappedLog::create(&path, 100, 10).unwrap();
        log.write(b"hello\n").unwrap();
        log.finish().unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello\n");
    }
}
