//! Moving duplicate copies into a staging folder, and deleting that folder later.
//!
//! Nothing is deleted here except by `purge`, and only inside a folder that
//! `clean` created (it carries a marker file).

use crate::scan::{self, ScanOpts};
use crate::util::{absolute, confirm, files_word, human, is_inside, is_managed_bundle, parse_size, same_content};
use clap::Args;
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const MARKER: &str = ".dupfind-stage";
const STAGE_NAME: &str = "_TO_DELETE";

#[derive(Args)]
pub struct CleanOpts {
    /// Extra folders to look for copies in [default: the current folder when there is no --remove]
    pub folders: Vec<PathBuf>,

    /// Folder whose files are kept (repeatable). Its subfolders too, unless named in --remove.
    #[arg(long, required = true)]
    pub keep: Vec<PathBuf>,

    /// Move copies only out of this folder (repeatable). May sit inside a --keep folder.
    /// Without --remove, copies anywhere outside the keep folders are moved.
    #[arg(long)]
    pub remove: Vec<PathBuf>,

    /// Folder whose files never move (repeatable). .git, .app and Photos libraries never do.
    #[arg(long)]
    pub protect: Vec<PathBuf>,

    /// Where copies are moved to [default: _TO_DELETE at the top of the drive, or in your home folder]
    #[arg(long)]
    pub stage: Option<PathBuf>,

    /// Ignore files smaller than this (e.g. 4K, 1M, 2G)
    #[arg(long, default_value = "1", value_parser = parse_size)]
    pub min_size: u64,

    /// Folder to skip while searching (repeatable)
    #[arg(long)]
    pub exclude: Vec<PathBuf>,

    /// Show the plan and every file, without asking or moving anything
    #[arg(long)]
    pub dry_run: bool,
}

pub(crate) struct Move {
    pub src: PathBuf,
    pub keeper: PathBuf,
    pub size: u64,
}

#[derive(PartialEq)]
enum Role {
    Keep,
    Remove,
    Other,
}

fn err(msg: String) -> io::Error {
    io::Error::other(msg)
}

/// A copy is never moved if it sits inside a managed bundle (.git, .app, Photos library).
pub(crate) fn in_managed_bundle(p: &Path) -> bool {
    p.components().any(|c| is_managed_bundle(&c.as_os_str().to_string_lossy()))
}

/// The deepest named folder decides: with --keep A and --remove A/dups, A/dups/x is Remove.
fn role(p: &Path, keep: &[PathBuf], remove: &[PathBuf]) -> Role {
    let depth = |dirs: &[PathBuf]| {
        dirs.iter()
            .filter(|d| p.starts_with(d) && p != d.as_path())
            .map(|d| d.components().count())
            .max()
    };
    match (depth(keep), depth(remove)) {
        (Some(k), Some(r)) if r > k => Role::Remove,
        (Some(_), _) => Role::Keep,
        (None, Some(_)) => Role::Remove,
        (None, None) => Role::Other,
    }
}

/// `_TO_DELETE` in the home folder for files under it, else at the top of the drive,
/// so it is on the same drive (moves are instant) and outside the folders being cleaned.
pub(crate) fn default_stage(first: &Path) -> PathBuf {
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        if first.starts_with(&home) {
            return home.join(STAGE_NAME);
        }
    }
    // Climb while still on the same drive and in folders owned by you (system folders are not).
    let me = std::env::var_os("HOME").and_then(|h| fs::metadata(h).ok()).map(|m| m.uid());
    let owned_here = |p: &Path, dev: u64| fs::metadata(p).map(|m| m.dev() == dev && Some(m.uid()) == me).unwrap_or(false);
    let dev = fs::metadata(first).map(|m| m.dev()).unwrap_or(0);
    let mut top = first.to_path_buf();
    while let Some(parent) = top.parent() {
        if !owned_here(parent, dev) {
            break;
        }
        top = parent.to_path_buf();
    }
    top.join(STAGE_NAME)
}

/// Path relative to `base` for display and for the layout inside the stage folder.
pub(crate) fn rel<'a>(p: &'a Path, base: &Path) -> &'a Path {
    p.strip_prefix(base).unwrap_or_else(|_| p.strip_prefix("/").unwrap_or(p))
}

pub fn clean(o: &CleanOpts) -> io::Result<()> {
    let keep: Vec<PathBuf> = o.keep.iter().map(|k| absolute(k)).collect();
    let remove: Vec<PathBuf> = o.remove.iter().map(|k| absolute(k)).collect();
    for d in keep.iter().chain(&remove) {
        if !d.is_dir() {
            return Err(err(format!("folder not found: {}", d.display())));
        }
    }
    if let Some(d) = remove.iter().find(|r| keep.contains(r)) {
        return Err(err(format!("{} is both --keep and --remove", d.display())));
    }
    let protect: Vec<PathBuf> = o.protect.iter().map(|p| absolute(p)).collect();

    // Search the keep + remove folders, plus the given folders. Without --remove, copies
    // can be anywhere, so with no folders given the current folder is searched too.
    let mut roots: Vec<PathBuf> = keep.iter().chain(&remove).cloned().collect();
    if o.folders.is_empty() && remove.is_empty() {
        roots.push(absolute(Path::new(".")));
    }
    roots.extend(o.folders.iter().map(|f| absolute(f)));
    roots.sort();
    roots.dedup();
    let roots: Vec<PathBuf> = roots.iter().filter(|r| !is_inside(r, &roots)).cloned().collect();

    let stage = o.stage.as_deref().map(absolute).unwrap_or_else(|| default_stage(&keep[0]));
    if keep.iter().chain(&remove).any(|d| stage.starts_with(d) || d.starts_with(&stage)) {
        return Err(err(format!(
            "stage folder {} is inside a --keep or --remove folder; choose another with --stage",
            stage.display()
        )));
    }
    let base = stage.parent().unwrap_or(Path::new("/")).to_path_buf();

    let opts = ScanOpts { roots, min_size: o.min_size, exclude: o.exclude.clone(), no_cache: false };
    let groups = scan::find_duplicates(&opts, std::slice::from_ref(&stage));

    // Plan: in every group with a copy in a keep folder, the copies in removable places move.
    let movable = |p: &Path| {
        let r = role(p, &keep, &remove);
        let wanted = if remove.is_empty() { Role::Other } else { Role::Remove };
        r == wanted && !is_inside(p, &protect) && !in_managed_bundle(p)
    };
    let mut plan = Vec::new();
    let mut no_keeper = 0;
    for g in &groups {
        let Some(keeper) = g.paths.iter().find(|p| role(p, &keep, &remove) == Role::Keep) else {
            no_keeper += g.paths.iter().any(|p| movable(p)) as usize;
            continue; // no copy in a keep folder -> never touch this group
        };
        for p in g.paths.iter().filter(|p| movable(p)) {
            plan.push(Move { src: p.clone(), keeper: keeper.clone(), size: g.size });
        }
    }
    if no_keeper > 0 {
        println!("\n{no_keeper} sets of copies have no copy in a --keep folder; they are left alone.");
    }
    let total: u64 = plan.iter().map(|m| m.size).sum();
    if plan.is_empty() {
        println!("\nNothing to move: no copies of kept files found in the folders to clean.");
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
    println!("\nCopies to move (paths inside {}):", base.display());
    for (d, (n, bytes)) in &by_dir {
        println!("  {:>9}  {n:>5} files  {}", human(*bytes), rel(d, &base).display());
    }
    println!("\nTotal: {}, {}. They go to {}", files_word(plan.len()), human(total), stage.display());

    if o.dry_run {
        println!();
        for m in &plan {
            println!("WOULD MOVE {}   (kept: {})", rel(&m.src, &base).display(), rel(&m.keeper, &base).display());
        }
        return Ok(());
    }
    let prompt = format!(
        "\nMove these {} into {}? Nothing is deleted yet.\nEach file is checked byte by byte against its kept copy first.",
        files_word(plan.len()),
        stage.display()
    );
    if !confirm(&prompt, "YES") {
        eprintln!("Cancelled. Nothing was moved.");
        return Ok(());
    }

    execute(&plan, &stage, &base)?;
    println!("\nNext: look through {}, then free the space with:\n  dupfind purge \"{}\"", stage.display(), stage.display());
    Ok(())
}

/// Check each copy byte by byte against its kept twin, move it into `stage`, log it, verify.
pub(crate) fn execute(plan: &[Move], stage: &Path, base: &Path) -> io::Result<()> {
    let total: u64 = plan.iter().map(|m| m.size).sum();
    fs::create_dir_all(stage)?;
    let marker = stage.join(MARKER);
    if !marker.exists() {
        File::create(&marker)?;
    }
    let stage_dev = fs::metadata(stage)?.dev();
    // The log lives with the files it describes; a unique name per run.
    let ts = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    let log_path = stage.join(format!("dupfind-log-{}-{:09}.tsv", ts.as_secs(), ts.subsec_nanos()));
    let mut log = BufWriter::new(File::options().write(true).create_new(true).open(&log_path)?);
    writeln!(log, "status\tsource\tstaged_as\tkept_copy")?;

    let (mut moved, mut moved_bytes, mut skipped) = (Vec::new(), 0u64, 0usize);
    for (i, m) in plan.iter().enumerate() {
        let dest = stage.join(rel(&m.src, base));
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
            eprintln!("\nSKIP ({reason}): {}", rel(&m.src, base).display());
            writeln!(log, "SKIP\t{}\t\t{}", m.src.display(), m.keeper.display())?;
            skipped += 1;
            continue;
        }
        if let Err(e) = fs::create_dir_all(dest.parent().unwrap()).and_then(|_| fs::rename(&m.src, &dest)) {
            eprintln!("\nSKIP (could not move: {e}): {}", rel(&m.src, base).display());
            writeln!(log, "SKIP\t{}\t\t{}", m.src.display(), m.keeper.display())?;
            skipped += 1;
            continue;
        }
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
    println!("\nMoved {} ({}), skipped {skipped}.", files_word(moved.len()), human(moved_bytes));
    println!("Verified: {} of {} moved files have their kept copy in place.", moved.len() - broken, moved.len());
    println!("Log: {}", log_path.display());
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
        let name = e.file_name().to_string_lossy().into_owned();
        // Count only the moved copies, not dupfind's own marker and logs.
        if e.file_type().is_file() && name != MARKER && !name.starts_with("dupfind-log-") {
            files += 1;
            bytes += e.metadata().map(|m| m.len()).unwrap_or(0);
        }
    }
    let prompt = format!(
        "\nPermanently delete {} and everything in it: {}, {}?\nThis does not go to the Trash and cannot be undone.",
        stage.display(),
        files_word(files),
        human(bytes)
    );
    if !confirm(&prompt, "DELETE") {
        eprintln!("Cancelled. Nothing was deleted.");
        return Ok(());
    }
    fs::remove_dir_all(&stage)?;
    println!("Deleted {} ({}, {} freed).", stage.display(), files_word(files), human(bytes));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::TestDir;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn deepest_named_folder_decides_role() {
        let keep = [p("/d/A")];
        let remove = [p("/d/A/dups")];
        assert!(role(Path::new("/d/A/x"), &keep, &remove) == Role::Keep);
        assert!(role(Path::new("/d/A/dups/x"), &keep, &remove) == Role::Remove);
        assert!(role(Path::new("/d/B/x"), &keep, &remove) == Role::Other);
        assert!(role(Path::new("/d/AB/x"), &keep, &remove) == Role::Other);
        // A keep folder inside a remove folder wins too.
        assert!(role(Path::new("/r/k/x"), &[p("/r/k")], &[p("/r")]) == Role::Keep);
        assert!(role(Path::new("/r/x"), &[p("/r/k")], &[p("/r")]) == Role::Remove);
    }

    #[test]
    fn managed_bundles_detected_anywhere_in_path() {
        assert!(in_managed_bundle(Path::new("/x/Lib.photoslibrary/originals/a.jpg")));
        assert!(in_managed_bundle(Path::new("/x/repo/.git/objects/ab")));
        assert!(in_managed_bundle(Path::new("/Applications/Foo.app/Contents/x")));
        assert!(!in_managed_bundle(Path::new("/x/apps/a.jpg")));
    }

    #[test]
    fn rel_strips_base_or_root() {
        assert_eq!(rel(Path::new("/v/T7/a/b"), Path::new("/v/T7")), Path::new("a/b"));
        assert_eq!(rel(Path::new("/other/a"), Path::new("/v/T7")), Path::new("other/a"));
    }

    #[test]
    fn default_stage_in_home() {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        assert_eq!(default_stage(&home.join("Pictures/x")), home.join(STAGE_NAME));
    }

    #[test]
    fn execute_moves_verified_copies_and_skips_changed_ones() {
        let t = TestDir::new("clean-exec");
        let keeper = t.file("keep/a.jpg", b"photo");
        let copy = t.file("dups/a.jpg", b"photo");
        let changed = t.file("dups/b.jpg", b"PHOTO"); // differs from its "keeper"
        let gone = t.0.join("dups/missing.jpg");
        let stage = t.0.join("_TO_DELETE");
        let plan = vec![
            Move { src: copy.clone(), keeper: keeper.clone(), size: 5 },
            Move { src: changed.clone(), keeper: keeper.clone(), size: 5 },
            Move { src: gone, keeper: keeper.clone(), size: 5 },
        ];
        execute(&plan, &stage, &t.0).unwrap();

        assert!(keeper.exists(), "kept copy must stay");
        assert!(!copy.exists());
        assert_eq!(fs::read(stage.join("dups/a.jpg")).unwrap(), b"photo");
        assert!(changed.exists(), "a file that no longer matches must not move");
        assert!(stage.join(MARKER).is_file());

        let log = fs::read_dir(&stage)
            .unwrap()
            .flatten()
            .find(|e| e.file_name().to_string_lossy().starts_with("dupfind-log-"))
            .unwrap();
        let log = fs::read_to_string(log.path()).unwrap();
        assert_eq!(log.lines().filter(|l| l.starts_with("MOVED")).count(), 1);
        assert_eq!(log.lines().filter(|l| l.starts_with("SKIP")).count(), 2);
    }

    #[test]
    fn execute_never_overwrites_in_stage() {
        let t = TestDir::new("clean-exist");
        let keeper = t.file("keep/a", b"x");
        let copy = t.file("dups/a", b"x");
        let stage = t.0.join("_TO_DELETE");
        t.file("_TO_DELETE/dups/a", b"earlier");
        execute(&[Move { src: copy.clone(), keeper, size: 1 }], &stage, &t.0).unwrap();
        assert!(copy.exists());
        assert_eq!(fs::read(stage.join("dups/a")).unwrap(), b"earlier");
    }

    #[test]
    fn purge_refuses_folders_clean_did_not_make() {
        let t = TestDir::new("clean-purge");
        t.file("stuff/a", b"precious");
        assert!(purge(&t.0.join("stuff")).is_err());
        assert!(t.0.join("stuff/a").exists());
    }

    #[test]
    fn clean_rejects_bad_folders() {
        let t = TestDir::new("clean-bad");
        t.file("A/x", b"1");
        let o = |keep: &str, remove: &str, stage: &str| CleanOpts {
            folders: vec![],
            keep: vec![t.0.join(keep)],
            remove: if remove.is_empty() { vec![] } else { vec![t.0.join(remove)] },
            protect: vec![],
            stage: Some(t.0.join(stage)),
            min_size: 1,
            exclude: vec![],
            dry_run: true,
        };
        assert!(clean(&o("missing", "", "S")).is_err());
        assert!(clean(&o("A", "A", "S")).is_err());
        assert!(clean(&o("A", "", "A/S")).is_err());
    }
}
