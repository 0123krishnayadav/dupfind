//! Moving duplicate copies into a staging folder, and deleting that folder later.
//!
//! Nothing is deleted here except by `purge`, and only inside a folder that
//! `clean` created (it carries a marker file).

use crate::scan::{self, ScanOpts};
use crate::util::{absolute, confirm, human, is_inside, is_managed_bundle, same_content};
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const MARKER: &str = ".dupfind-stage";

struct Move {
    src: PathBuf,
    keeper: PathBuf,
    size: u64,
}

fn err(msg: String) -> io::Error {
    io::Error::other(msg)
}

/// A copy is never moved if it sits inside a managed bundle (.git, .app, Photos library).
fn in_managed_bundle(p: &Path) -> bool {
    p.components().any(|c| is_managed_bundle(&c.as_os_str().to_string_lossy()))
}

/// Path inside the stage folder: relative to the scanned folder, so
/// /Volumes/T7/a/b.jpg becomes _TO_DELETE/a/b.jpg. With several scanned folders,
/// each one gets its own subfolder named after it.
fn staged_name(src: &Path, roots: &[PathBuf]) -> PathBuf {
    for r in roots {
        if let Ok(rel) = src.strip_prefix(r) {
            return match (roots.len(), r.file_name()) {
                (1, _) | (_, None) => rel.to_path_buf(),
                (_, Some(name)) => Path::new(name).join(rel),
            };
        }
    }
    src.strip_prefix("/").unwrap_or(src).to_path_buf()
}

pub fn clean(opts: &ScanOpts, keep: &[PathBuf], protect: &[PathBuf], stage: Option<&Path>, dry_run: bool) -> io::Result<()> {
    let keep: Vec<PathBuf> = keep.iter().map(|k| absolute(k)).collect();
    for k in &keep {
        if !k.is_dir() {
            return Err(err(format!("keep folder not found: {}", k.display())));
        }
    }
    let protect: Vec<PathBuf> = protect.iter().map(|p| absolute(p)).collect();
    let stage = absolute(stage.unwrap_or(&opts.roots[0].join("_TO_DELETE")));
    if keep.iter().any(|k| stage.starts_with(k) || k.starts_with(&stage)) {
        return Err(err(format!("stage folder {} overlaps a keep folder", stage.display())));
    }

    let roots: Vec<PathBuf> = opts.roots.iter().map(|r| absolute(r)).collect();
    let groups = scan::find_duplicates(opts, std::slice::from_ref(&stage));

    // Plan: for every group with a copy in a keep folder, every other copy moves.
    let mut plan = Vec::new();
    for g in &groups {
        let Some(keeper) = g.paths.iter().find(|p| is_inside(p, &keep)) else {
            continue; // no copy in a keep folder -> never touch this group
        };
        for p in &g.paths {
            if is_inside(p, &keep) || is_inside(p, &protect) || in_managed_bundle(p) {
                continue;
            }
            plan.push(Move { src: p.clone(), keeper: keeper.clone(), size: g.size });
        }
    }
    let total: u64 = plan.iter().map(|m| m.size).sum();
    if plan.is_empty() {
        eprintln!("\nNothing to move: no copies outside the keep folders.");
        return Ok(());
    }

    let mut by_dir: HashMap<&Path, (usize, u64)> = HashMap::new();
    for m in &plan {
        let e = by_dir.entry(m.src.parent().unwrap()).or_default();
        e.0 += 1;
        e.1 += m.size;
    }
    let mut by_dir: Vec<_> = by_dir.into_iter().collect();
    by_dir.sort_by(|a, b| b.1.1.cmp(&a.1.1));
    println!("\nCopies that would be moved, by folder:");
    for (d, (n, bytes)) in &by_dir {
        println!("  {:>9}  {n:>5} files  {}", human(*bytes), d.display());
    }
    println!("\nTotal: {} files, {} -> {}", plan.len(), human(total), stage.display());

    if dry_run {
        println!();
        for m in &plan {
            println!("WOULD MOVE {}   (kept: {})", m.src.display(), m.keeper.display());
        }
        return Ok(());
    }
    let prompt = format!(
        "\nMove {} files ({}) into {}?\nEach file is checked byte by byte against its kept copy first.",
        plan.len(),
        human(total),
        stage.display()
    );
    if !confirm(&prompt, "YES") {
        eprintln!("Cancelled. Nothing was moved.");
        return Ok(());
    }

    fs::create_dir_all(&stage)?;
    let marker = stage.join(MARKER);
    if !marker.exists() {
        File::create(&marker)?;
    }
    let stage_dev = fs::metadata(&stage)?.dev();
    let ts = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    let log_path = absolute(Path::new(&format!("dupfind-clean-{ts}.log")));
    let mut log = BufWriter::new(File::options().write(true).create_new(true).open(&log_path)?);
    writeln!(log, "status\tsource\tstaged_as\tkept_copy")?;

    let (mut moved, mut moved_bytes, mut skipped) = (Vec::new(), 0u64, 0usize);
    for (i, m) in plan.iter().enumerate() {
        let dest = stage.join(staged_name(&m.src, &roots));
        let reason = if fs::metadata(&m.src).map(|md| md.dev() != stage_dev).unwrap_or(true) {
            Some("missing or on a different drive than the stage folder")
        } else if !same_content(&m.src, &m.keeper).unwrap_or(false) {
            Some("not identical to the kept copy anymore")
        } else if dest.exists() {
            Some("already exists in stage folder")
        } else {
            None
        };
        if let Some(reason) = reason {
            eprintln!("\nSKIP ({reason}): {}", m.src.display());
            writeln!(log, "SKIP\t{}\t\t{}", m.src.display(), m.keeper.display())?;
            skipped += 1;
            continue;
        }
        fs::create_dir_all(dest.parent().unwrap())?;
        fs::rename(&m.src, &dest)?;
        writeln!(log, "MOVED\t{}\t{}\t{}", m.src.display(), dest.display(), m.keeper.display())?;
        moved_bytes += m.size;
        moved.push((dest, &m.keeper));
        eprint!("\r  moved {}/{} files, {} of {}", i + 1, plan.len(), human(moved_bytes), human(total));
    }
    log.flush()?;
    eprintln!();

    // Verify: every moved file is in the stage folder and its kept copy is still in place.
    let broken = moved
        .iter()
        .filter(|(dest, keeper)| {
            let (a, b) = (fs::metadata(dest), fs::metadata(keeper));
            !(a.is_ok() && b.is_ok() && a.unwrap().len() == b.unwrap().len())
        })
        .count();
    println!("\nMoved {} files ({}), skipped {skipped}.", moved.len(), human(moved_bytes));
    println!("Verified: {} of {} moved files have their kept copy in place.", moved.len() - broken, moved.len());
    println!("Log: {}", log_path.display());
    println!("\nNext: check {}, then free the space with:\n  dupfind purge \"{}\"", stage.display(), stage.display());
    Ok(())
}

pub fn purge(stage: &Path) -> io::Result<()> {
    let stage = absolute(stage);
    if !stage.join(MARKER).is_file() {
        return Err(err(format!(
            "{} was not created by `dupfind clean` (no {MARKER} marker), refusing to delete it",
            stage.display()
        )));
    }
    let (mut files, mut bytes) = (0usize, 0u64);
    for e in jwalk::WalkDir::new(&stage).skip_hidden(false).into_iter().flatten() {
        if e.file_type().is_file() {
            files += 1;
            bytes += e.metadata().map(|m| m.len()).unwrap_or(0);
        }
    }
    let prompt = format!(
        "\nPermanently delete {} and everything in it: {files} files, {}?\nThis does not go to the Trash and cannot be undone.",
        stage.display(),
        human(bytes)
    );
    if !confirm(&prompt, "DELETE") {
        eprintln!("Cancelled. Nothing was deleted.");
        return Ok(());
    }
    fs::remove_dir_all(&stage)?;
    println!("Deleted {} ({files} files, {} freed).", stage.display(), human(bytes));
    Ok(())
}
