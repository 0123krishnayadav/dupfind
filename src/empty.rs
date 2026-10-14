//! Removing empty folders.
//!
//! By default a folder counts as empty only if it has nothing in it at all,
//! hidden files included. With --include-metadata, macOS `.DS_Store` and `._`
//! files are allowed and get removed along with the folder.
//! Folders are only ever removed with `remove_dir`, which fails on a non-empty folder.

use crate::util::{absolute, confirm, is_managed_bundle, is_metadata};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

// System folders on drives that must stay even when empty.
const SYSTEM_DIRS: &[&str] = &[".Trashes", ".Trash", ".Spotlight-V100", ".fseventsd", ".TemporaryItems", "$RECYCLE.BIN"];

/// Returns true if `dir` is removable. Collects the top-most removable folders under it into `found`.
fn scan(dir: &Path, metadata_ok: bool, found: &mut Vec<PathBuf>) -> bool {
    let Ok(entries) = fs::read_dir(dir) else { return false };
    let mut removable = true;
    let mut empty_children = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else { return false };
        let name = entry.file_name().to_string_lossy().into_owned();
        let Ok(ft) = entry.file_type() else { return false };
        if ft.is_dir() {
            if SYSTEM_DIRS.contains(&name.as_str()) || is_managed_bundle(&name) {
                removable = false;
            } else if scan(&entry.path(), metadata_ok, found) {
                empty_children.push(entry.path());
            } else {
                removable = false;
            }
        } else if !(metadata_ok && ft.is_file() && is_metadata(&name)) {
            removable = false; // a real file, a hidden file, or a symlink
        }
    }
    if !removable {
        found.extend(empty_children);
    }
    removable
}

fn remove(dir: &Path, metadata_ok: bool) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if entry.file_type()?.is_dir() {
            remove(&entry.path(), metadata_ok)?;
        } else if metadata_ok && is_metadata(&name) {
            match fs::remove_file(entry.path()) {
                // macOS removes a `._x` file by itself when `x` is removed.
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                r => r?,
            }
        } else {
            return Err(io::Error::other(format!("not empty, has {}", entry.path().display())));
        }
    }
    fs::remove_dir(dir) // refuses if anything is still inside
}

pub fn empty_dirs(roots: &[PathBuf], metadata_ok: bool, dry_run: bool) -> io::Result<()> {
    let mut found = Vec::new();
    for root in roots {
        let root = absolute(root);
        if scan(&root, metadata_ok, &mut found) {
            // The root itself is empty: offer its children, never the root.
            found.retain(|p| p.parent() != Some(&root));
            found.extend(fs::read_dir(&root)?.flatten().map(|e| e.path()).filter(|p| p.is_dir()));
        }
    }
    found.sort();
    if found.is_empty() {
        println!("No empty folders found.");
        return Ok(());
    }
    let mode = if metadata_ok { "empty apart from .DS_Store / ._ files" } else { "completely empty, no hidden files" };
    println!("Empty folders ({mode}):");
    for d in &found {
        println!("  {}", d.display());
    }
    if dry_run || !confirm(&format!("\nDelete these {} folders?", found.len()), "YES") {
        if !dry_run {
            eprintln!("Cancelled. Nothing was deleted.");
        }
        return Ok(());
    }
    for d in &found {
        match remove(d, metadata_ok) {
            Ok(()) => {
                println!("DELETED {}", d.display());
                if metadata_ok {
                    if let (Some(parent), Some(name)) = (d.parent(), d.file_name()) {
                        let sidecar = parent.join(format!("._{}", name.to_string_lossy()));
                        if sidecar.is_file() {
                            fs::remove_file(sidecar).ok();
                        }
                    }
                }
            }
            Err(e) => println!("SKIPPED {} ({e})", d.display()),
        }
    }
    Ok(())
}
