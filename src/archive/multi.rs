use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;

use anyhow::{anyhow, Result};

/// One contiguous slice of a physical file that contributes bytes to the
/// logical stream. Used to build `StreamSource::MultiFileRange` for serving
/// stored archive entries that span multiple split parts.
#[derive(Clone, Debug)]
pub(crate) struct FileSegment {
    pub path: PathBuf,
    pub start_in_file: u64,
    pub len_in_file: u64,
}

/// Concatenating reader over an ordered list of physical files.
///
/// Presents N parts as a single contiguous, seekable byte stream. Used to feed
/// the `zip` and `sevenz_rust2` libraries when a split archive (`.zip.001`,
/// `.7z.001`, bare `.001`, ...) is opened.
pub(crate) struct MultiFileReader {
    /// Open file handles, one per part. Kept open for the life of the reader.
    files: Vec<File>,
    /// Original paths, parallel to `files`. Retained so segments can hand out
    /// real on-disk paths for streaming.
    paths: Vec<PathBuf>,
    /// `bounds[i]` = total bytes in parts `0..i` (cumulative). `bounds[0] == 0`.
    /// Length is `files.len() + 1`; the last entry is the logical total size.
    bounds: Vec<u64>,
    /// Current logical position (0 <= pos <= total).
    pos: u64,
    /// Logical length of the concatenated stream.
    total: u64,
}

impl MultiFileReader {
    /// Open all `parts` in order. Each part must exist and be readable.
    /// Returns an error if the part list is empty.
    pub(crate) fn open(parts: &[PathBuf]) -> Result<Self> {
        if parts.is_empty() {
            return Err(anyhow!("MultiFileReader: part list is empty"));
        }
        let mut files = Vec::with_capacity(parts.len());
        let mut bounds = Vec::with_capacity(parts.len() + 1);
        bounds.push(0);
        for p in parts {
            let f = File::open(p)?;
            let len = f.metadata().map(|m| m.len()).unwrap_or(0);
            let next = *bounds.last().unwrap_or(&0) + len;
            bounds.push(next);
            files.push(f);
        }
        let total = *bounds.last().unwrap_or(&0);
        Ok(Self {
            files,
            paths: parts.to_vec(),
            bounds,
            pos: 0,
            total,
        })
    }

    /// Total length of the concatenated stream.
    pub(crate) fn total_size(&self) -> u64 {
        self.total
    }

    /// Map a logical byte range `[start, start + len)` onto the underlying
    /// physical files, returning one `FileSegment` per part it touches.
    ///
    /// `start + len` is clamped to `total_size()`; callers may pass a length
    /// that extends beyond EOF (e.g. a stored-entry size that runs past the
    /// last byte, which can happen with padded ZIP headers).
    pub(crate) fn map_logical_range(&self, start: u64, len: u64) -> Vec<FileSegment> {
        let mut out = Vec::new();
        if self.files.is_empty() || len == 0 {
            return out;
        }
        let end = start.saturating_add(len).min(self.total);
        if start >= end {
            return out;
        }

        // Find the part containing `start` via the cumulative bounds array.
        // `bounds[i]` = logical offset where part i begins, so we want the
        // largest i with bounds[i] <= start.
        let mut part = match self.bounds.binary_search(&start) {
            Ok(i) => i,
            Err(i) => i.saturating_sub(1),
        };
        if part >= self.files.len() {
            part = self.files.len() - 1;
        }

        let mut remaining = end - start;
        let mut logical_cur = start;
        while part < self.files.len() && remaining > 0 {
            let part_start = self.bounds[part];
            let part_end = self.bounds[part + 1];
            let offset_in_file = logical_cur - part_start;
            let part_available = part_end - logical_cur;
            let take = part_available.min(remaining);

            out.push(FileSegment {
                path: self.paths[part].clone(),
                start_in_file: offset_in_file,
                len_in_file: take,
            });

            remaining -= take;
            logical_cur += take;
            part += 1;
        }
        out
    }
}

impl Read for MultiFileReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.pos >= self.total || buf.is_empty() {
            return Ok(0);
        }
        // Fill across as many parts as needed so callers don't see short reads
        // at part boundaries (which would otherwise break libraries that call
        // `read` once and assume they get the full buffer back).
        let mut filled = 0;
        while filled < buf.len() && self.pos < self.total {
            let part = match self.bounds.binary_search(&self.pos) {
                Ok(i) => i,
                Err(i) => i.saturating_sub(1),
            };
            if part >= self.files.len() {
                break;
            }
            let offset_in_file = self.pos - self.bounds[part];
            let f = &mut self.files[part];
            f.seek(SeekFrom::Start(offset_in_file))?;
            let n = f.read(&mut buf[filled..])?;
            if n == 0 {
                break;
            }
            filled += n;
            self.pos += n as u64;
        }
        Ok(filled)
    }
}

impl Seek for MultiFileReader {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        let new_pos: u64 = match pos {
            SeekFrom::Start(p) => p,
            SeekFrom::End(off) => {
                if off < 0 {
                    self.total
                        .checked_sub(off.unsigned_abs())
                        .ok_or_else(|| {
                            std::io::Error::new(
                                std::io::ErrorKind::InvalidInput,
                                "seek before start",
                            )
                        })?
                } else {
                    self.total
                        .checked_add(off as u64)
                        .ok_or_else(|| {
                            std::io::Error::new(
                                std::io::ErrorKind::InvalidInput,
                                "seek overflow",
                            )
                        })?
                }
            }
            SeekFrom::Current(off) => {
                if off < 0 {
                    self.pos
                        .checked_sub(off.unsigned_abs())
                        .ok_or_else(|| {
                            std::io::Error::new(
                                std::io::ErrorKind::InvalidInput,
                                "seek before start",
                            )
                        })?
                } else {
                    self.pos
                        .checked_add(off as u64)
                        .ok_or_else(|| {
                            std::io::Error::new(
                                std::io::ErrorKind::InvalidInput,
                                "seek overflow",
                            )
                        })?
                }
            }
        };
        // Clamp to total; seeking past EOF is treated as EOF so subsequent
        // reads return 0 bytes, matching single-file `File` semantics.
        self.pos = new_pos.min(self.total);
        Ok(self.pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Seek, SeekFrom, Write};

    fn build_parts(dir: &std::path::Path, contents: &[&[u8]]) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        for (i, c) in contents.iter().enumerate() {
            let p = dir.join(format!("part.{}", i + 1));
            let mut f = File::create(&p).unwrap();
            f.write_all(c).unwrap();
            paths.push(p);
        }
        paths
    }

    fn unique_dir(label: &str) -> std::path::PathBuf {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!("pixview-{label}-{nanos:x}"));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn concatenates_parts_sequentially() {
        let tmp = unique_dir("concat");
        let parts = build_parts(&tmp, &[b"AAAA", b"BBBBBB", b"CC"]);
        let mut r = MultiFileReader::open(&parts).unwrap();
        let mut buf = String::new();
        r.read_to_string(&mut buf).unwrap();
        assert_eq!(buf, "AAAABBBBBBCC");
        assert_eq!(r.total_size(), 12);
    }

    #[test]
    fn seek_and_read_across_boundary() {
        let tmp = unique_dir("seek");
        let parts = build_parts(&tmp, &[b"AAAA", b"BBBBBB", b"CC"]);
        let mut r = MultiFileReader::open(&parts).unwrap();
        r.seek(SeekFrom::Start(5)).unwrap();
        let mut buf = [0u8; 4];
        let n = r.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"BBBB");
        r.seek(SeekFrom::Start(10)).unwrap();
        let n = r.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"CC");
    }

    #[test]
    fn seek_end_and_current() {
        let tmp = unique_dir("seekend");
        let parts = build_parts(&tmp, &[b"AAAA", b"BBBBBB", b"CC"]);
        let mut r = MultiFileReader::open(&parts).unwrap();
        r.seek(SeekFrom::End(-4)).unwrap();
        assert_eq!(r.pos, 8);
        let mut buf = [0u8; 4];
        let n = r.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"BBCC");
        r.seek(SeekFrom::Start(0)).unwrap();
        r.seek(SeekFrom::Current(3)).unwrap();
        assert_eq!(r.pos, 3);
    }

    #[test]
    fn map_logical_range_single_part() {
        let tmp = unique_dir("map1");
        let parts = build_parts(&tmp, &[b"AAAA", b"BBBBBB", b"CC"]);
        let r = MultiFileReader::open(&parts).unwrap();
        // Range [1,3) lies entirely within part 0 (which spans [0,4)).
        let segs = r.map_logical_range(1, 2);
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].start_in_file, 1);
        assert_eq!(segs[0].len_in_file, 2);
        assert_eq!(segs[0].path, parts[0]);
    }

    #[test]
    fn map_logical_range_spanning_parts() {
        let tmp = unique_dir("map2");
        let parts = build_parts(&tmp, &[b"AAAA", b"BBBBBB", b"CC"]);
        let r = MultiFileReader::open(&parts).unwrap();
        // 4-byte slice from logical offset 3 → 1 byte from part0, 3 from part1
        let segs = r.map_logical_range(3, 4);
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].start_in_file, 3);
        assert_eq!(segs[0].len_in_file, 1);
        assert_eq!(segs[1].start_in_file, 0);
        assert_eq!(segs[1].len_in_file, 3);
        assert_eq!(segs[0].path, parts[0]);
        assert_eq!(segs[1].path, parts[1]);
    }

    #[test]
    fn map_logical_range_spanning_all_parts() {
        let tmp = unique_dir("map3");
        let parts = build_parts(&tmp, &[b"AAAA", b"BBBBBB", b"CC"]);
        let r = MultiFileReader::open(&parts).unwrap();
        let segs = r.map_logical_range(1, 11);
        assert_eq!(segs.len(), 3);
        let total: u64 = segs.iter().map(|s| s.len_in_file).sum();
        assert_eq!(total, 11);
    }

    #[test]
    fn map_logical_range_clamps_past_eof() {
        let tmp = unique_dir("map4");
        let parts = build_parts(&tmp, &[b"AAAA", b"BBBBBB", b"CC"]);
        let r = MultiFileReader::open(&parts).unwrap();
        // Asking past EOF clamps to total bytes available.
        let segs = r.map_logical_range(8, 100);
        let total: u64 = segs.iter().map(|s| s.len_in_file).sum();
        assert_eq!(total, 4); // 4 bytes left from logical offset 8
    }

    #[test]
    fn rejects_empty_part_list() {
        assert!(MultiFileReader::open(&[]).is_err());
    }
}
