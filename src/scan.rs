//! Finding duplicates. Read-only: files are only ever opened for reading.
//!
//! Stages (each one only looks at files that survived the previous one):
//!   1. Parallel directory walk, group files by size     -> no file reads
//!   2. Hash first + last 64KB of same-size files        -> cheap, rules out most
//!   3. Full hash of files that still match              -> confirms duplicates
//!
//! Hashes from stages 2 and 3 are kept in a cache (see cache.rs), so unchanged files
//! are not read again on the next run.

use crate::cache::{self, Cache, Kind, Stamp};
use crate::util::{absolute, human, is_metadata, parse_size};
use clap::Args;
use jwalk::{Parallelism, WalkDirGeneric};
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;
use xxhash_rust::xxh3::{Xxh3, xxh3_128};

const PARTIAL: u64 = 64 * 1024;
const CHUNK: usize = 1024 * 1024;

// macOS system / virtual locations that are slow, huge, or not real user files.
const DEFAULT_EXCLUDES: &[&str] = &[
    "/System",
    "/private/var/vm",
    "/private/var/folders",
    "/dev",
    "/Volumes",
    "/cores",
    "/proc",
    "/sys",
];
const SKIP_DIR_NAMES: &[&str] = &[
    ".Trash",
    ".Trashes",
    "$RECYCLE.BIN",
    ".git",
    "node_modules",
    "__pycache__",
    ".Spotlight-V100",
    ".fseventsd",
    ".TemporaryItems",
];

#[derive(Args)]
pub struct ScanOpts {
    /// Folders or drives to scan, e.g. ~ or /Volumes/T7 [default: current folder]
    #[arg(default_value = ".")]
    pub roots: Vec<PathBuf>,

    /// Ignore files smaller than this (e.g. 4K, 1M, 2G)
    #[arg(long, default_value = "1", value_parser = parse_size)]
    pub min_size: u64,

    /// Extra path to skip (repeatable)
    #[arg(long)]
    pub exclude: Vec<PathBuf>,

    /// Read every file again instead of reusing hashes saved by earlier scans
    #[arg(long)]
    pub no_cache: bool,
}

pub struct Group {
    pub size: u64,
    pub paths: Vec<PathBuf>,
}

impl Group {
    pub fn wasted(&self) -> u64 {
        self.size * (self.paths.len() as u64 - 1)
    }
}

/// (stamp, dev) stored on each walked file; filled in on the walker's worker threads.
type FileInfo = Option<(Stamp, u64)>;

fn walk(roots: &[PathBuf], min_size: u64, excludes: &HashSet<PathBuf>) -> Vec<(PathBuf, Stamp, u64)> {
    let mut out = Vec::new();
    for root in roots {
        let excludes = excludes.clone();
        let walker = WalkDirGeneric::<((), FileInfo)>::new(root)
            .skip_hidden(false)
            .follow_links(false)
            .parallelism(Parallelism::RayonDefaultPool { busy_timeout: std::time::Duration::from_secs(1) })
            .process_read_dir(move |_, _, _, children| {
                for entry in children.iter_mut().flatten() {
                    let ft = entry.file_type;
                    let name = entry.file_name.to_string_lossy();
                    if ft.is_dir() {
                        // A stage folder from `clean` holds copies already set aside.
                        let path = entry.path();
                        if SKIP_DIR_NAMES.contains(&name.as_ref())
                            || excludes.contains(&path)
                            || path.join(".dupfind-stage").exists()
                        {
                            entry.read_children_path = None;
                        }
                    } else if ft.is_file() && !is_metadata(&name) {
                        if let Ok(md) = entry.metadata() {
                            if md.len() >= min_size {
                                let stamp = Stamp {
                                    size: md.len(),
                                    ino: md.ino(),
                                    mtime: (md.mtime(), md.mtime_nsec()),
                                    ctime: (md.ctime(), md.ctime_nsec()),
                                };
                                entry.client_state = Some((stamp, md.dev()));
                            }
                        }
                    }
                }
            });
        for entry in walker.into_iter().flatten() {
            if let Some((stamp, dev)) = entry.client_state {
                out.push((entry.path(), stamp, dev));
                if out.len() % 50_000 == 0 {
                    eprint!("\r  scanned {} files", out.len());
                }
            }
        }
    }
    out
}

fn partial_hash(path: &Path, size: u64) -> io::Result<u128> {
    let mut f = File::open(path)?;
    if size <= PARTIAL * 2 {
        let mut buf = Vec::with_capacity(size as usize);
        f.read_to_end(&mut buf)?;
        return Ok(xxh3_128(&buf));
    }
    let mut buf = vec![0u8; (PARTIAL * 2) as usize];
    let (head, tail) = buf.split_at_mut(PARTIAL as usize);
    f.read_exact(head)?;
    f.seek(SeekFrom::End(-(PARTIAL as i64)))?;
    f.read_exact(tail)?;
    Ok(xxh3_128(&buf))
}

fn full_hash(path: &Path) -> io::Result<u128> {
    let mut f = File::open(path)?;
    let mut h = Xxh3::new();
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.digest128())
}

type Files = Vec<(PathBuf, Stamp)>;

/// Hash every file in every group in parallel, split groups by hash, drop singletons.
/// Hashes in `cache` are reused; new ones are added to it.
fn regroup<F>(groups: Vec<(u64, Files)>, label: &str, cache: &Cache, kind: Kind, hash: F) -> Vec<(u64, Files)>
where
    F: Fn(&Path, u64) -> io::Result<u128> + Sync,
{
    let jobs: Vec<(u64, (PathBuf, Stamp))> = groups
        .into_iter()
        .flat_map(|(size, paths)| paths.into_iter().map(move |p| (size, p)))
        .collect();
    let total = jobs.len();
    if total == 0 {
        return Vec::new();
    }
    let done = AtomicUsize::new(0);
    let hits_before = cache.hits.load(Ordering::Relaxed);
    let hashed: Vec<(u64, u128, (PathBuf, Stamp))> = jobs
        .into_par_iter()
        .filter_map(|(size, (path, stamp))| {
            let d = done.fetch_add(1, Ordering::Relaxed) + 1;
            if d % 1000 == 0 || d == total {
                eprint!("\r  {label}: {d}/{total}");
            }
            let h = match cache.get(&path, kind) {
                Some(h) => h,
                None => {
                    let h = hash(&path, size).ok()?;
                    cache.put(&path, stamp, kind, h);
                    h
                }
            };
            Some((size, h, (path, stamp)))
        })
        .collect();
    let hits = cache.hits.load(Ordering::Relaxed) - hits_before;
    if hits > 0 {
        eprint!(" ({hits} from cache)");
    }
    eprintln!();

    let mut map: HashMap<(u64, u128), Files> = HashMap::new();
    for (size, h, file) in hashed {
        map.entry((size, h)).or_default().push(file);
    }
    map.into_iter()
        .filter(|(_, v)| v.len() > 1)
        .map(|((size, _), v)| (size, v))
        .collect()
}

/// Find all duplicate groups under `opts.roots`, biggest wasted space first.
pub fn find_duplicates(opts: &ScanOpts, extra_excludes: &[PathBuf]) -> Vec<Group> {
    let t0 = Instant::now();
    let roots: Vec<PathBuf> = opts.roots.iter().map(|r| absolute(r)).collect();
    let mut excludes: HashSet<PathBuf> = DEFAULT_EXCLUDES.iter().map(PathBuf::from).collect();
    // A root inside a default exclude (e.g. /Volumes/T7) must still be scanned.
    excludes.retain(|e| !roots.iter().any(|r| r.starts_with(e)));
    excludes.extend(opts.exclude.iter().map(|e| absolute(e)));
    excludes.extend(extra_excludes.iter().cloned());

    // Stage 1: walk + group by size. Hard links (same dev+inode) are one file, count once.
    eprintln!("Scanning {} ...", roots.iter().map(|r| r.display().to_string()).collect::<Vec<_>>().join(", "));
    let files = walk(&roots, opts.min_size, &excludes);
    let scanned = files.len();
    let mut cache = match cache::default_file().filter(|_| !opts.no_cache) {
        Some(file) => Cache::open(file),
        None => Cache::disabled(),
    };
    let mut seen = HashSet::new();
    let mut by_size: HashMap<u64, Files> = HashMap::new();
    for (path, stamp, dev) in files {
        cache.confirm(&path, stamp);
        if seen.insert((dev, stamp.ino)) {
            by_size.entry(stamp.size).or_default().push((path, stamp));
        }
    }
    let candidates: Vec<_> = by_size.into_iter().filter(|(_, v)| v.len() > 1).collect();
    let n_cand: usize = candidates.iter().map(|(_, v)| v.len()).sum();
    eprintln!(
        "\r  scanned {scanned} files; {n_cand} share a size with another file ({:.1}s)",
        t0.elapsed().as_secs_f64()
    );

    // Stage 2: partial hash. Files <= 128KB are hashed completely here, so they're final.
    let groups = regroup(candidates, "partial hash", &cache, Kind::Partial, partial_hash);
    let (mut dupes, large): (Vec<_>, Vec<_>) = groups.into_iter().partition(|(s, _)| *s <= PARTIAL * 2);

    // Stage 3: full hash for the larger files still matching.
    dupes.extend(regroup(large, "full hash", &cache, Kind::Full, |p, _| full_hash(p)));
    cache.save(&roots);

    let mut groups: Vec<Group> = dupes
        .into_iter()
        .map(|(size, files)| {
            let mut paths: Vec<PathBuf> = files.into_iter().map(|(p, _)| p).collect();
            paths.sort();
            Group { size, paths }
        })
        .collect();
    groups.sort_by(|a, b| b.wasted().cmp(&a.wasted()).then_with(|| a.paths.cmp(&b.paths)));
    let wasted: u64 = groups.iter().map(Group::wasted).sum();
    eprintln!(
        "{} duplicate groups, {} reclaimable. Took {:.1}s.",
        groups.len(),
        human(wasted),
        t0.elapsed().as_secs_f64()
    );
    groups
}

pub fn print_files(groups: &[Group]) -> io::Result<()> {
    let mut out = BufWriter::new(io::stdout().lock());
    for g in groups {
        writeln!(out, "\n[{} x {}]  wasted {}", human(g.size), g.paths.len(), human(g.wasted()))?;
        for p in &g.paths {
            writeln!(out, "   {}", p.display())?;
        }
    }
    out.flush()
}

/// Which sets of folders hold copies of each other, biggest wasted space first.
pub fn print_folders(groups: &[Group], roots: &[PathBuf], limit: usize) -> io::Result<()> {
    let roots: Vec<PathBuf> = roots.iter().map(|r| absolute(r)).collect();
    let short = |d: &Path| -> String {
        roots
            .iter()
            .find_map(|r| d.strip_prefix(r).ok().map(|rel| r.file_name().map(Path::new).unwrap_or(r).join(rel)))
            .unwrap_or_else(|| d.to_path_buf())
            .display()
            .to_string()
    };
    // key: the folders a duplicate group lives in -> (shared files, wasted bytes)
    let mut sets: HashMap<Vec<PathBuf>, (usize, u64)> = HashMap::new();
    for g in groups {
        let mut dirs: Vec<PathBuf> = g.paths.iter().map(|p| p.parent().unwrap_or(Path::new("/")).to_path_buf()).collect();
        dirs.sort();
        dirs.dedup();
        let e = sets.entry(dirs).or_default();
        e.0 += 1;
        e.1 += g.wasted();
    }
    let mut sets: Vec<_> = sets.into_iter().collect();
    sets.sort_by(|a, b| b.1.1.cmp(&a.1.1));
    let mut out = BufWriter::new(io::stdout().lock());
    writeln!(out, "\nFolders holding copies of the same files (top {limit} by wasted space):")?;
    for (dirs, (n, bytes)) in sets.iter().take(limit) {
        let note = if dirs.len() == 1 { "  (copies inside this one folder)" } else { "" };
        writeln!(out, "\n  {:>9} wasted, {n} files{note}", human(*bytes))?;
        for d in dirs {
            writeln!(out, "     {}", short(d))?;
        }
    }
    if sets.len() > limit {
        writeln!(out, "\n  ... and {} more folder sets. Use --csv for the full list.", sets.len() - limit)?;
    }
    out.flush()
}

pub fn write_csv(groups: &[Group], path: &Path) -> io::Result<()> {
    // create_new: refuse to overwrite an existing file.
    let file = File::options().write(true).create_new(true).open(path)?;
    let mut w = BufWriter::new(file);
    writeln!(w, "group,size_bytes,path")?;
    for (i, g) in groups.iter().enumerate() {
        for p in &g.paths {
            writeln!(w, "{},{},\"{}\"", i + 1, g.size, p.display().to_string().replace('"', "\"\""))?;
        }
    }
    w.flush()?;
    eprintln!("Saved to {}", path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::TestDir;

    fn opts(t: &TestDir) -> ScanOpts {
        ScanOpts { roots: vec![t.0.clone()], min_size: 1, exclude: vec![], no_cache: true }
    }

    fn names(groups: &[Group], t: &TestDir) -> Vec<Vec<String>> {
        groups
            .iter()
            .map(|g| g.paths.iter().map(|p| p.strip_prefix(&t.0).unwrap().display().to_string()).collect())
            .collect()
    }

    #[test]
    fn finds_copies_by_content_not_name() {
        let t = TestDir::new("scan-basic");
        t.file("a/photo.jpg", b"same bytes");
        t.file("b/renamed.jpg", b"same bytes");
        t.file("b/other.jpg", b"diff bytes"); // same size, different content
        t.file("c/unique.txt", b"only one of me");
        let g = find_duplicates(&opts(&t), &[]);
        assert_eq!(names(&g, &t), vec![vec!["a/photo.jpg", "b/renamed.jpg"]]);
        assert_eq!(g[0].size, 10);
        assert_eq!(g[0].wasted(), 10);
    }

    #[test]
    fn large_files_differing_only_in_the_middle_are_not_duplicates() {
        let t = TestDir::new("scan-middle");
        let a: Vec<u8> = (0..(PARTIAL * 3) as u32).map(|i| (i % 251) as u8).collect();
        let mut b = a.clone();
        b[(PARTIAL * 3 / 2) as usize] ^= 0xff;
        t.file("a.bin", &a);
        t.file("b.bin", &b);
        t.file("c.bin", &a);
        let g = find_duplicates(&opts(&t), &[]);
        assert_eq!(names(&g, &t), vec![vec!["a.bin", "c.bin"]]);
    }

    #[test]
    fn three_copies_waste_two() {
        let t = TestDir::new("scan-three");
        for n in ["x", "y", "z"] {
            t.file(n, b"12345");
        }
        let g = find_duplicates(&opts(&t), &[]);
        assert_eq!(g[0].paths.len(), 3);
        assert_eq!(g[0].wasted(), 10);
    }

    #[test]
    fn biggest_waste_first() {
        let t = TestDir::new("scan-order");
        t.file("s1", b"ab");
        t.file("s2", b"ab");
        t.file("b1", b"abcdef");
        t.file("b2", b"abcdef");
        let g = find_duplicates(&opts(&t), &[]);
        assert_eq!(names(&g, &t), vec![vec!["b1", "b2"], vec!["s1", "s2"]]);
    }

    #[test]
    fn hard_links_are_one_file() {
        let t = TestDir::new("scan-hardlink");
        let a = t.file("a", b"linked");
        std::fs::hard_link(&a, t.0.join("b")).unwrap();
        assert!(find_duplicates(&opts(&t), &[]).is_empty());
    }

    #[test]
    fn symlinks_are_not_followed() {
        let t = TestDir::new("scan-symlink");
        let a = t.file("a", b"target");
        std::os::unix::fs::symlink(&a, t.0.join("link")).unwrap();
        assert!(find_duplicates(&opts(&t), &[]).is_empty());
    }

    #[test]
    fn min_size_skips_small_files() {
        let t = TestDir::new("scan-min");
        t.file("a", b"tiny");
        t.file("b", b"tiny");
        let mut o = opts(&t);
        o.min_size = 5;
        assert!(find_duplicates(&o, &[]).is_empty());
        o.min_size = 4;
        assert_eq!(find_duplicates(&o, &[]).len(), 1);
    }

    #[test]
    fn skips_excluded_system_and_stage_folders_and_metadata() {
        let t = TestDir::new("scan-skip");
        t.file("keep/a", b"content");
        for dir in ["excluded", "extra", ".git", "node_modules", ".Trash", "stage"] {
            t.file(&format!("{dir}/a"), b"content");
        }
        t.file("stage/.dupfind-stage", b"");
        t.file("x/.DS_Store", b"meta");
        t.file("y/.DS_Store", b"meta");
        t.file("x/._a", b"meta");
        t.file("y/._a", b"meta");
        let mut o = opts(&t);
        o.exclude = vec![t.0.join("excluded")];
        assert!(find_duplicates(&o, &[t.0.join("extra")]).is_empty());
        t.file("other/a", b"content");
        assert_eq!(names(&find_duplicates(&o, &[t.0.join("extra")]), &t), vec![vec!["keep/a", "other/a"]]);
    }

    #[test]
    fn empty_files_are_ignored_by_default() {
        let t = TestDir::new("scan-empty");
        t.file("a", b"");
        t.file("b", b"");
        assert!(find_duplicates(&opts(&t), &[]).is_empty());
    }

    #[test]
    fn several_roots() {
        let (t1, t2) = (TestDir::new("scan-root1"), TestDir::new("scan-root2"));
        let a = t1.file("a", b"across roots");
        let b = t2.file("b", b"across roots");
        let o = ScanOpts { roots: vec![t1.0.clone(), t2.0.clone()], min_size: 1, exclude: vec![], no_cache: true };
        let g = find_duplicates(&o, &[]);
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].paths, { let mut v = vec![a, b]; v.sort(); v });
    }

    #[test]
    fn hashes_match_content() {
        let t = TestDir::new("scan-hash");
        let small = t.file("small", b"hello");
        assert_eq!(partial_hash(&small, 5).unwrap(), xxh3_128(b"hello"));
        assert_eq!(full_hash(&small).unwrap(), xxh3_128(b"hello"));
        let data: Vec<u8> = (0..(CHUNK * 2 + 7) as u32).map(|i| i as u8).collect();
        let big = t.file("big", &data);
        assert_eq!(full_hash(&big).unwrap(), xxh3_128(&data));
        let p = PARTIAL as usize;
        let edges = [&data[..p], &data[data.len() - p..]].concat();
        assert_eq!(partial_hash(&big, data.len() as u64).unwrap(), xxh3_128(&edges));
    }

    #[test]
    fn regroup_uses_and_fills_cache() {
        let t = TestDir::new("scan-regroup");
        let stamp = |size| Stamp { size, ino: 1, mtime: (0, 0), ctime: (0, 0) };
        let a = t.file("a", b"aaaa");
        let b = t.file("b", b"aaaa");
        let groups = vec![(4u64, vec![(a.clone(), stamp(4)), (b.clone(), stamp(4))])];
        let cache = Cache::open(t.0.join("cache.bin"));
        let reads = AtomicUsize::new(0);
        let hash = |p: &Path, _| {
            reads.fetch_add(1, Ordering::Relaxed);
            full_hash(p)
        };
        assert_eq!(regroup(groups.clone(), "t", &cache, Kind::Full, hash).len(), 1);
        assert_eq!(reads.load(Ordering::Relaxed), 2);
        // Second pass: both from cache, no reads.
        assert_eq!(regroup(groups, "t", &cache, Kind::Full, hash).len(), 1);
        assert_eq!(reads.load(Ordering::Relaxed), 2);
        assert_eq!(cache.hits.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn csv_quotes_paths_and_never_overwrites() {
        let t = TestDir::new("scan-csv");
        let groups = vec![Group { size: 3, paths: vec![PathBuf::from("/a/x\"y.txt"), PathBuf::from("/b/z, w.txt")] }];
        let csv = t.0.join("out.csv");
        write_csv(&groups, &csv).unwrap();
        assert_eq!(
            std::fs::read_to_string(&csv).unwrap(),
            "group,size_bytes,path\n1,3,\"/a/x\"\"y.txt\"\n1,3,\"/b/z, w.txt\"\n"
        );
        assert!(write_csv(&groups, &csv).is_err());
    }
}
