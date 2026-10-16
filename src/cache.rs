//! Remembers file hashes between runs, so unchanged files are not read again.
//!
//! An entry is reused only while the file's size, inode, modified time and change time
//! (ctime, which normal tools cannot set back) are all the same as when it was hashed.
//! Even a wrong entry cannot lose data: `clean` compares every file byte by byte before moving it.
//!
//! Stored in ~/Library/Caches/dupfind/hashes.bin (macOS) or ~/.cache/dupfind/hashes.bin.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

const MAGIC: &[u8; 8] = b"DUPFINDC";
const VERSION: u32 = 1;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Stamp {
    pub size: u64,
    pub ino: u64,
    pub mtime: (i64, i64),
    pub ctime: (i64, i64),
}

#[derive(Clone, Copy)]
pub enum Kind {
    Partial,
    Full,
}

#[derive(Clone, Copy)]
struct Entry {
    stamp: Stamp,
    partial: Option<u128>,
    full: Option<u128>,
}

impl Entry {
    fn get(&self, kind: Kind) -> Option<u128> {
        match kind {
            Kind::Partial => self.partial,
            Kind::Full => self.full,
        }
    }
}

pub struct Cache {
    file: Option<PathBuf>,
    /// Entries loaded from disk that this run has not confirmed yet.
    old: HashMap<PathBuf, Entry>,
    /// Entries confirmed or computed this run.
    fresh: Mutex<HashMap<PathBuf, Entry>>,
    pub hits: AtomicUsize,
}

pub fn default_file() -> Option<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME")?);
    let dir = if cfg!(target_os = "macos") {
        home.join("Library/Caches")
    } else {
        std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".cache"))
    };
    Some(dir.join("dupfind/hashes.bin"))
}

impl Cache {
    /// A cache that remembers nothing and saves nothing.
    pub fn disabled() -> Self {
        Cache { file: None, old: HashMap::new(), fresh: Mutex::new(HashMap::new()), hits: AtomicUsize::new(0) }
    }

    /// Load the cache file. A missing, unreadable or outdated file means starting empty.
    pub fn open(file: PathBuf) -> Self {
        let old = fs::read(&file).ok().and_then(|b| parse(&b)).unwrap_or_default();
        Cache { file: Some(file), old, fresh: Mutex::new(HashMap::new()), hits: AtomicUsize::new(0) }
    }

    /// The walk saw `path` with `stamp`: keep its entry if the file has not changed.
    pub fn confirm(&mut self, path: &Path, stamp: Stamp) {
        if let Some(e) = self.old.remove(path).filter(|e| e.stamp == stamp) {
            self.fresh.get_mut().unwrap().insert(path.to_path_buf(), e);
        }
    }

    pub fn get(&self, path: &Path, kind: Kind) -> Option<u128> {
        let h = self.fresh.lock().unwrap().get(path)?.get(kind)?;
        self.hits.fetch_add(1, Ordering::Relaxed);
        Some(h)
    }

    pub fn put(&self, path: &Path, stamp: Stamp, kind: Kind, hash: u128) {
        let mut fresh = self.fresh.lock().unwrap();
        let e = fresh.entry(path.to_path_buf()).or_insert(Entry { stamp, partial: None, full: None });
        if e.stamp != stamp {
            *e = Entry { stamp, partial: None, full: None };
        }
        match kind {
            Kind::Partial => e.partial = Some(hash),
            Kind::Full => e.full = Some(hash),
        }
    }

    /// Write the cache back. Entries under `roots` that this run did not see are dropped
    /// (deleted, changed or excluded files); entries for other folders are kept.
    pub fn save(self, roots: &[PathBuf]) {
        let Some(file) = self.file else { return };
        let mut all = self.fresh.into_inner().unwrap();
        all.extend(self.old.into_iter().filter(|(p, _)| !roots.iter().any(|r| p.starts_with(r))));
        if let Err(e) = write(&file, &all) {
            eprintln!("  note: could not save hash cache {}: {e}", file.display());
        }
    }
}

fn write(file: &Path, entries: &HashMap<PathBuf, Entry>) -> io::Result<()> {
    if let Some(dir) = file.parent() {
        fs::create_dir_all(dir)?;
    }
    // Write a temporary file and rename it, so an interrupted save never leaves a broken cache.
    let tmp = file.with_extension(format!("tmp{}", std::process::id()));
    let result = (|| {
        let mut w = BufWriter::new(File::create(&tmp)?);
        w.write_all(MAGIC)?;
        w.write_all(&VERSION.to_le_bytes())?;
        w.write_all(&(entries.len() as u64).to_le_bytes())?;
        for (path, e) in entries {
            let p = path.as_os_str().as_bytes();
            w.write_all(&(p.len() as u32).to_le_bytes())?;
            w.write_all(p)?;
            let s = e.stamp;
            for n in [s.size, s.ino] {
                w.write_all(&n.to_le_bytes())?;
            }
            for n in [s.mtime.0, s.mtime.1, s.ctime.0, s.ctime.1] {
                w.write_all(&n.to_le_bytes())?;
            }
            w.write_all(&[e.partial.is_some() as u8 | (e.full.is_some() as u8) << 1])?;
            w.write_all(&e.partial.unwrap_or(0).to_le_bytes())?;
            w.write_all(&e.full.unwrap_or(0).to_le_bytes())?;
        }
        w.into_inner().map_err(|e| e.into_error())?.sync_all()?;
        fs::rename(&tmp, file)
    })();
    if result.is_err() {
        fs::remove_file(&tmp).ok();
    }
    result
}

fn parse(mut b: &[u8]) -> Option<HashMap<PathBuf, Entry>> {
    fn take<'a>(b: &mut &'a [u8], n: usize) -> Option<&'a [u8]> {
        if b.len() < n {
            return None;
        }
        let (head, rest) = b.split_at(n);
        *b = rest;
        Some(head)
    }
    fn u64_(b: &mut &[u8]) -> Option<u64> {
        Some(u64::from_le_bytes(take(b, 8)?.try_into().ok()?))
    }
    fn i64_(b: &mut &[u8]) -> Option<i64> {
        Some(i64::from_le_bytes(take(b, 8)?.try_into().ok()?))
    }
    fn u128_(b: &mut &[u8]) -> Option<u128> {
        Some(u128::from_le_bytes(take(b, 16)?.try_into().ok()?))
    }

    if take(&mut b, 8)? != MAGIC || u32::from_le_bytes(take(&mut b, 4)?.try_into().ok()?) != VERSION {
        return None;
    }
    let n = u64_(&mut b)?;
    let mut map = HashMap::with_capacity(n.min(10_000_000) as usize);
    for _ in 0..n {
        let len = u32::from_le_bytes(take(&mut b, 4)?.try_into().ok()?) as usize;
        let path = PathBuf::from(std::ffi::OsStr::from_bytes(take(&mut b, len)?));
        let stamp = Stamp {
            size: u64_(&mut b)?,
            ino: u64_(&mut b)?,
            mtime: (i64_(&mut b)?, i64_(&mut b)?),
            ctime: (i64_(&mut b)?, i64_(&mut b)?),
        };
        let flags = take(&mut b, 1)?[0];
        let (partial, full) = (u128_(&mut b)?, u128_(&mut b)?);
        map.insert(
            path,
            Entry {
                stamp,
                partial: (flags & 1 != 0).then_some(partial),
                full: (flags & 2 != 0).then_some(full),
            },
        );
    }
    Some(map)
}

/// Remove the cache file. Used by `dupfind clear-cache`.
pub fn clear() -> io::Result<()> {
    let Some(file) = default_file() else { return Ok(()) };
    match fs::remove_file(&file) {
        Ok(()) => eprintln!("Removed {}", file.display()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => eprintln!("No hash cache to remove ({})", file.display()),
        Err(e) => return Err(e),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamp(size: u64) -> Stamp {
        Stamp { size, ino: 7, mtime: (1, 2), ctime: (3, 4) }
    }

    #[test]
    fn round_trip_and_invalidation() {
        let dir = std::env::temp_dir().join(format!("dupfind-cache-test-{}", std::process::id()));
        let file = dir.join("hashes.bin");
        let (a, b) = (PathBuf::from("/r/a"), PathBuf::from("/other/b"));

        let c = Cache::open(file.clone());
        c.put(&a, stamp(10), Kind::Partial, 42);
        c.put(&a, stamp(10), Kind::Full, 43);
        c.put(&b, stamp(5), Kind::Partial, 9);
        c.save(&[]);

        // Unchanged file: both hashes come back.
        let mut c = Cache::open(file.clone());
        c.confirm(&a, stamp(10));
        assert_eq!(c.get(&a, Kind::Partial), Some(42));
        assert_eq!(c.get(&a, Kind::Full), Some(43));
        // Changed file: nothing.
        let mut c2 = Cache::open(file.clone());
        c2.confirm(&a, stamp(11));
        assert_eq!(c2.get(&a, Kind::Partial), None);

        // Saving after scanning /r without seeing /r/a drops it, keeps /other/b.
        Cache::open(file.clone()).save(&[PathBuf::from("/r")]);
        let c = Cache::open(file.clone());
        assert!(!c.old.contains_key(&a));
        assert!(c.old.contains_key(&b));

        fs::remove_dir_all(&dir).ok();
    }
}
