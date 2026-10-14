//! Finding duplicates. Read-only: files are only ever opened for reading.
//!
//! Stages (each one only looks at files that survived the previous one):
//!   1. Parallel directory walk, group files by size     -> no file reads
//!   2. Hash first + last 64KB of same-size files        -> cheap, rules out most
//!   3. Full hash of files that still match              -> confirms duplicates

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

/// (size, dev, inode) stored on each walked file; filled in on the walker's worker threads.
type FileInfo = Option<(u64, u64, u64)>;

fn walk(roots: &[PathBuf], min_size: u64, excludes: &HashSet<PathBuf>) -> Vec<(PathBuf, u64, (u64, u64))> {
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
                                entry.client_state = Some((md.len(), md.dev(), md.ino()));
                            }
                        }
                    }
                }
            });
        for entry in walker.into_iter().flatten() {
            if let Some((size, dev, ino)) = entry.client_state {
                out.push((entry.path(), size, (dev, ino)));
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

/// Hash every file in every group in parallel, split groups by hash, drop singletons.
fn regroup<F>(groups: Vec<(u64, Vec<PathBuf>)>, label: &str, hash: F) -> Vec<(u64, Vec<PathBuf>)>
where
    F: Fn(&Path, u64) -> io::Result<u128> + Sync,
{
    let jobs: Vec<(u64, PathBuf)> = groups
        .into_iter()
        .flat_map(|(size, paths)| paths.into_iter().map(move |p| (size, p)))
        .collect();
    let total = jobs.len();
    if total == 0 {
        return Vec::new();
    }
    let done = AtomicUsize::new(0);
    let hashed: Vec<(u64, u128, PathBuf)> = jobs
        .into_par_iter()
        .filter_map(|(size, path)| {
            let d = done.fetch_add(1, Ordering::Relaxed) + 1;
            if d % 1000 == 0 || d == total {
                eprint!("\r  {label}: {d}/{total}");
            }
            hash(&path, size).ok().map(|h| (size, h, path))
        })
        .collect();
    eprintln!();

    let mut map: HashMap<(u64, u128), Vec<PathBuf>> = HashMap::new();
    for (size, h, path) in hashed {
        map.entry((size, h)).or_default().push(path);
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
    let mut seen = HashSet::new();
    let mut by_size: HashMap<u64, Vec<PathBuf>> = HashMap::new();
    for (path, size, inode) in files {
        if seen.insert(inode) {
            by_size.entry(size).or_default().push(path);
        }
    }
    let candidates: Vec<_> = by_size.into_iter().filter(|(_, v)| v.len() > 1).collect();
    let n_cand: usize = candidates.iter().map(|(_, v)| v.len()).sum();
    eprintln!(
        "\r  scanned {scanned} files; {n_cand} share a size with another file ({:.1}s)",
        t0.elapsed().as_secs_f64()
    );

    // Stage 2: partial hash. Files <= 128KB are hashed completely here, so they're final.
    let groups = regroup(candidates, "partial hash", partial_hash);
    let (mut dupes, large): (Vec<_>, Vec<_>) = groups.into_iter().partition(|(s, _)| *s <= PARTIAL * 2);

    // Stage 3: full hash for the larger files still matching.
    dupes.extend(regroup(large, "full hash", |p, _| full_hash(p)));

    let mut groups: Vec<Group> = dupes
        .into_iter()
        .map(|(size, mut paths)| {
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
