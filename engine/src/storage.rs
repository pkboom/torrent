//! Maps pieces onto files on disk.

use crate::metainfo::{FileEntry, Metainfo};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

const MAX_OPEN_FILES: usize = 64;

pub struct Storage {
    root: PathBuf,
    files: Vec<FileEntry>,
    piece_length: u64,
    total_length: u64,
    handles: Mutex<HashMap<usize, File>>,
    /// Once disabled (torrent removed), writes are dropped instead of recreating files.
    disabled: AtomicBool,
}

/// A slice of a piece that lands in one file.
struct Segment {
    file: usize,
    file_offset: u64,
    buf_start: usize,
    len: usize,
}

impl Storage {
    pub fn new(download_dir: &Path, meta: &Metainfo) -> Self {
        Storage {
            root: download_dir.to_path_buf(),
            files: meta.files.clone(),
            piece_length: meta.piece_length,
            total_length: meta.total_length,
            handles: Mutex::new(HashMap::new()),
            disabled: AtomicBool::new(false),
        }
    }

    fn segments(&self, index: u32, len: usize) -> Vec<Segment> {
        let start = index as u64 * self.piece_length;
        let end = (start + len as u64).min(self.total_length);
        // Files are sorted by offset, so binary search for the first overlapping one.
        let first = self.files.partition_point(|f| f.offset + f.length <= start);
        self.files[first..]
            .iter()
            .enumerate()
            .take_while(|(_, f)| f.offset < end)
            .filter(|(_, f)| f.length > 0)
            .map(|(i, f)| {
                let s = start.max(f.offset);
                let e = end.min(f.offset + f.length);
                Segment {
                    file: first + i,
                    file_offset: s - f.offset,
                    buf_start: (s - start) as usize,
                    len: (e - s) as usize,
                }
            })
            .collect()
    }

    fn with_file<T>(&self, idx: usize, f: impl FnOnce(&mut File) -> io::Result<T>) -> io::Result<T> {
        let mut handles = self.handles.lock().unwrap();
        if !handles.contains_key(&idx) {
            if handles.len() >= MAX_OPEN_FILES {
                handles.clear();
            }
            let path = self.root.join(&self.files[idx].path);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            let file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&path)?;
            handles.insert(idx, file);
        }
        f(handles.get_mut(&idx).unwrap())
    }

    pub fn write_piece(&self, index: u32, data: &[u8]) -> io::Result<()> {
        if self.disabled.load(Ordering::Relaxed) {
            return Ok(());
        }
        for seg in self.segments(index, data.len()) {
            if self.files[seg.file].pad {
                continue;
            }
            self.with_file(seg.file, |f| {
                f.seek(SeekFrom::Start(seg.file_offset))?;
                f.write_all(&data[seg.buf_start..seg.buf_start + seg.len])
            })?;
        }
        Ok(())
    }

    /// Reads a piece back for verification. Returns `None` if any part is missing on disk.
    pub fn read_piece(&self, index: u32, len: usize) -> io::Result<Option<Vec<u8>>> {
        let mut buf = vec![0u8; len];
        for seg in self.segments(index, len) {
            let entry = &self.files[seg.file];
            if entry.pad {
                continue;
            }
            let path = self.root.join(&entry.path);
            let mut file = match File::open(&path) {
                Ok(f) => f,
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(e) => return Err(e),
            };
            if file.metadata()?.len() < seg.file_offset + seg.len as u64 {
                return Ok(None);
            }
            file.seek(SeekFrom::Start(seg.file_offset))?;
            file.read_exact(&mut buf[seg.buf_start..seg.buf_start + seg.len])?;
        }
        Ok(Some(buf))
    }

    /// Cheap check that the bytes for a piece exist on disk (used to trust resume data).
    pub fn piece_present(&self, index: u32, len: usize, sizes: &mut HashMap<usize, u64>) -> bool {
        self.segments(index, len).iter().all(|seg| {
            let entry = &self.files[seg.file];
            if entry.pad {
                return true;
            }
            let size = *sizes.entry(seg.file).or_insert_with(|| {
                fs::metadata(self.root.join(&entry.path)).map(|m| m.len()).unwrap_or(0)
            });
            size >= seg.file_offset + seg.len as u64
        })
    }

    /// Creates zero-length files, which never receive any piece data.
    pub fn finalize(&self) -> io::Result<()> {
        self.handles.lock().unwrap().clear();
        for f in self.files.iter().filter(|f| f.length == 0 && !f.pad) {
            let path = self.root.join(&f.path);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            OpenOptions::new().write(true).create(true).truncate(false).open(path)?;
        }
        Ok(())
    }

    pub fn close(&self) {
        self.handles.lock().unwrap().clear();
    }

    pub fn disable(&self) {
        self.disabled.store(true, Ordering::Relaxed);
        self.close();
    }

    pub fn sync(&self) -> io::Result<()> {
        for f in self.handles.lock().unwrap().values() {
            f.sync_data()?;
        }
        Ok(())
    }

    /// Absolute paths of the real (non-padding) files.
    pub fn file_paths(&self) -> Vec<PathBuf> {
        self.files.iter().filter(|f| !f.pad).map(|f| self.root.join(&f.path)).collect()
    }
}
