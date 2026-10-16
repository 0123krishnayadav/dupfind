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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::TestDir;

    fn found(t: &TestDir, metadata_ok: bool) -> Vec<String> {
        let mut f = Vec::new();
        scan(&t.0, metadata_ok, &mut f);
        f.sort();
        f.iter().map(|p| p.strip_prefix(&t.0).unwrap().display().to_string()).collect()
    }

    #[test]
    fn finds_topmost_empty_folders() {
        let t = TestDir::new("empty-basic");
        t.file("full/a", b"x");
        fs::create_dir_all(t.0.join("full/empty")).unwrap();
        fs::create_dir_all(t.0.join("nested/a/b/c")).unwrap();
        assert_eq!(found(&t, false), ["full/empty", "nested"]);
    }

    #[test]
    fn hidden_files_count_unless_metadata_allowed() {
        let t = TestDir::new("empty-meta");
        t.file("ds/.DS_Store", b"x");
        t.file("hidden/.secret", b"x");
        t.file("keep/file", b"x");
        assert!(found(&t, false).is_empty());
        assert_eq!(found(&t, true), ["ds"]);
    }

    #[test]
    fn system_folders_and_bundles_stay() {
        let t = TestDir::new("empty-system");
        t.file("keep/file", b"x");
        for d in [".Trashes", ".Spotlight-V100", "Foo.app", "repo/.git"] {
            fs::create_dir_all(t.0.join(d)).unwrap();
        }
        assert!(found(&t, false).is_empty());
    }

    #[test]
    fn remove_deletes_metadata_but_refuses_real_files() {
        let t = TestDir::new("empty-remove");
        t.file("m/.DS_Store", b"x");
        t.file("m/sub/._y", b"x");
        remove(&t.0.join("m"), true).unwrap();
        assert!(!t.0.join("m").exists());

        t.file("r/real.txt", b"x");
        assert!(remove(&t.0.join("r"), true).is_err());
        assert!(t.0.join("r/real.txt").exists());
        t.file("s/.DS_Store", b"x");
        assert!(remove(&t.0.join("s"), false).is_err());
    }
}
