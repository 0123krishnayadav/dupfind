//! End-to-end tests: run the real `dupfind` binary on temporary folders, answering its
//! prompts through stdin like a person would. HOME points into the temporary folder,
//! so the hash cache and default stage folder never touch the real home folder.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

struct Env {
    root: PathBuf,
    home: PathBuf,
    data: PathBuf,
}

struct Out {
    ok: bool,
    stdout: String,
    stderr: String,
}

impl Env {
    fn new(name: &str) -> Self {
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("dupfind-cli-{}-{name}-{n}", std::process::id()));
        fs::remove_dir_all(&root).ok();
        fs::create_dir_all(root.join("home")).unwrap();
        fs::create_dir_all(root.join("data")).unwrap();
        let root = fs::canonicalize(root).unwrap();
        Env { home: root.join("home"), data: root.join("data"), root }
    }

    fn file(&self, rel: &str, content: &[u8]) -> PathBuf {
        let p = self.data.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, content).unwrap();
        p
    }

    fn has(&self, rel: &str) -> bool {
        self.data.join(rel).exists()
    }

    /// Run dupfind in the data folder with `args`, typing `input` on stdin.
    fn run(&self, args: &[&str], input: &str) -> Out {
        let mut child = Command::new(env!("CARGO_BIN_EXE_dupfind"))
            .args(args)
            .current_dir(&self.data)
            .env("HOME", &self.home)
            .env_remove("XDG_CACHE_HOME")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
        let o = child.wait_with_output().unwrap();
        Out {
            ok: o.status.success(),
            stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
        }
    }

    fn cache_file(&self) -> PathBuf {
        if cfg!(target_os = "macos") {
            self.home.join("Library/Caches/dupfind/hashes.bin")
        } else {
            self.home.join(".cache/dupfind/hashes.bin")
        }
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).ok();
    }
}

fn photos(e: &Env) {
    e.file("Originals/a.jpg", b"photo a");
    e.file("Originals/b.jpg", b"photo bb");
    e.file("Originals/unique.jpg", b"only here");
    e.file("Originals/duplicates/a.jpg", b"photo a");
    e.file("Originals/duplicates/b copy.jpg", b"photo bb");
    e.file("Elsewhere/a.jpg", b"photo a");
    e.file("Elsewhere/lonely.jpg", b"no twin");
}

// ---------- scan ----------

#[test]
fn scan_lists_duplicates_and_changes_nothing() {
    let e = Env::new("scan");
    photos(&e);
    let o = e.run(&["scan", "--files"], "");
    assert!(o.ok, "{}", o.stderr);
    assert!(o.stderr.contains("2 duplicate groups"), "{}", o.stderr);
    for p in ["Originals/a.jpg", "Originals/duplicates/a.jpg", "Elsewhere/a.jpg", "Originals/duplicates/b copy.jpg"] {
        assert!(o.stdout.contains(p), "missing {p} in:\n{}", o.stdout);
    }
    assert!(!o.stdout.contains("unique.jpg") && !o.stdout.contains("lonely.jpg"));
    assert!(!o.stdout.contains("Clean up now"), "must not prompt when not interactive");
    assert!(e.has("Originals/duplicates/a.jpg") && e.has("Elsewhere/a.jpg"));
}

#[test]
fn scan_folder_summary() {
    let e = Env::new("summary");
    photos(&e);
    let o = e.run(&["scan"], "");
    assert!(o.ok);
    assert!(o.stdout.contains("Folders holding copies"), "{}", o.stdout);
    assert!(o.stdout.contains("Originals/duplicates"));
}

#[test]
fn scan_no_duplicates() {
    let e = Env::new("none");
    e.file("a", b"one");
    e.file("b", b"two!");
    let o = e.run(&["scan"], "");
    assert!(o.ok);
    assert!(o.stderr.contains("0 duplicate groups"), "{}", o.stderr);
}

#[test]
fn scan_csv_never_overwrites() {
    let e = Env::new("csv");
    photos(&e);
    let csv = e.root.join("out.csv");
    let csv_arg = csv.to_str().unwrap();
    assert!(e.run(&["scan", "--csv", csv_arg], "").ok);
    let text = fs::read_to_string(&csv).unwrap();
    assert!(text.starts_with("group,size_bytes,path\n"));
    assert_eq!(text.lines().count(), 1 + 5);

    let again = e.run(&["scan", "--csv", csv_arg], "");
    assert!(!again.ok);
    assert!(again.stderr.contains("already exists"));
    assert_eq!(fs::read_to_string(&csv).unwrap(), text);
}

#[test]
fn scan_min_size_and_exclude() {
    let e = Env::new("filters");
    photos(&e);
    let o = e.run(&["scan", "--files", "--min-size", "8"], "");
    assert!(o.stderr.contains("1 duplicate groups"), "{}", o.stderr);
    let o = e.run(&["scan", "--files", "--exclude", "Originals/duplicates"], "");
    assert!(o.stderr.contains("1 duplicate groups"), "{}", o.stderr);
    assert!(!o.stdout.contains("duplicates/"));
}

#[test]
fn scan_bad_min_size_is_an_error() {
    let e = Env::new("badsize");
    assert!(!e.run(&["scan", "--min-size", "lots"], "").ok);
}

// ---------- hash cache ----------

#[test]
fn second_scan_uses_cache_and_sees_edits() {
    let e = Env::new("cache");
    let big: Vec<u8> = (0..300_000u32).map(|i| i as u8).collect();
    e.file("a/big", &big);
    e.file("b/big", &big);
    e.file("a/small", b"tiny");
    e.file("b/small", b"tiny");

    let first = e.run(&["scan"], "");
    assert!(first.stderr.contains("2 duplicate groups"), "{}", first.stderr);
    assert!(!first.stderr.contains("from cache"));
    assert!(e.cache_file().is_file());

    let second = e.run(&["scan"], "");
    assert!(second.stderr.contains("partial hash: 4/4 (4 from cache)"), "{}", second.stderr);
    assert!(second.stderr.contains("full hash: 2/2 (2 from cache)"), "{}", second.stderr);
    assert!(second.stderr.contains("2 duplicate groups"));

    // Same size, one byte changed in the middle: must be read again and no longer match.
    let mut edited = big.clone();
    edited[150_000] ^= 1;
    fs::write(e.data.join("b/big"), &edited).unwrap();
    let third = e.run(&["scan"], "");
    assert!(third.stderr.contains("1 duplicate groups"), "{}", third.stderr);
}

#[test]
fn no_cache_flag_reads_everything() {
    let e = Env::new("nocache");
    e.file("a", b"same");
    e.file("b", b"same");
    e.run(&["scan"], "");
    let o = e.run(&["scan", "--no-cache"], "");
    assert!(o.stderr.contains("1 duplicate groups"));
    assert!(!o.stderr.contains("from cache"), "{}", o.stderr);
}

#[test]
fn deleted_file_is_not_reported_from_cache() {
    let e = Env::new("deleted");
    e.file("a", b"same");
    e.file("b", b"same");
    e.file("c", b"same");
    e.run(&["scan"], "");
    fs::remove_file(e.data.join("c")).unwrap();
    let o = e.run(&["scan", "--files"], "");
    assert!(!o.stdout.contains("/c\n"), "{}", o.stdout);
    assert!(o.stdout.contains("x 2]"), "{}", o.stdout);
}

#[test]
fn corrupt_cache_is_ignored() {
    let e = Env::new("corrupt");
    e.file("a", b"same");
    e.file("b", b"same");
    fs::create_dir_all(e.cache_file().parent().unwrap()).unwrap();
    fs::write(e.cache_file(), b"DUPFINDC garbage").unwrap();
    let o = e.run(&["scan"], "");
    assert!(o.ok);
    assert!(o.stderr.contains("1 duplicate groups"), "{}", o.stderr);
}

#[test]
fn clear_cache() {
    let e = Env::new("clear");
    e.file("a", b"same");
    e.file("b", b"same");
    e.run(&["scan"], "");
    assert!(e.cache_file().exists());
    let o = e.run(&["clear-cache"], "");
    assert!(o.ok && !e.cache_file().exists());
    assert!(e.run(&["clear-cache"], "").ok, "clearing twice is fine");
}

// ---------- clean + purge ----------

fn stage(e: &Env) -> String {
    e.root.join("_TO_DELETE").to_str().unwrap().to_string()
}

#[test]
fn clean_dry_run_moves_nothing() {
    let e = Env::new("dry");
    photos(&e);
    let o = e.run(&["clean", "--keep", "Originals", "--remove", "Originals/duplicates", "--stage", &stage(&e), "--dry-run"], "");
    assert!(o.ok, "{}", o.stderr);
    assert_eq!(o.stdout.matches("WOULD MOVE").count(), 2, "{}", o.stdout);
    assert!(e.has("Originals/duplicates/a.jpg") && e.has("Originals/duplicates/b copy.jpg"));
}

#[test]
fn clean_cancel_moves_nothing() {
    let e = Env::new("cancel");
    photos(&e);
    let o = e.run(&["clean", "--keep", "Originals", "--remove", "Originals/duplicates", "--stage", &stage(&e)], "no\n");
    assert!(o.ok);
    assert!(o.stderr.contains("Nothing was moved"));
    assert!(e.has("Originals/duplicates/a.jpg"));
}

#[test]
fn clean_moves_only_copies_from_remove_folder() {
    let e = Env::new("clean");
    photos(&e);
    let st = stage(&e);
    let o = e.run(&["clean", "--keep", "Originals", "--remove", "Originals/duplicates", "--stage", &st], "YES\n");
    assert!(o.ok, "{}\n{}", o.stdout, o.stderr);
    assert!(o.stdout.contains("Moved 2 files"), "{}", o.stdout);
    assert!(o.stdout.contains("Verified: 2 of 2"));

    assert!(!e.has("Originals/duplicates/a.jpg") && !e.has("Originals/duplicates/b copy.jpg"));
    for kept in ["Originals/a.jpg", "Originals/b.jpg", "Originals/unique.jpg", "Elsewhere/a.jpg", "Elsewhere/lonely.jpg"] {
        assert!(e.has(kept), "{kept} must stay");
    }
    let st = PathBuf::from(st);
    assert!(st.join(".dupfind-stage").is_file());
    let staged: Vec<_> = walk(&st).into_iter().filter(|p| p.ends_with("a.jpg") || p.ends_with("b copy.jpg")).collect();
    assert_eq!(staged.len(), 2, "{staged:?}");
}

#[test]
fn clean_without_remove_moves_copies_outside_keep() {
    let e = Env::new("clean-all");
    photos(&e);
    let o = e.run(&["clean", ".", "--keep", "Originals", "--stage", &stage(&e)], "YES\n");
    assert!(o.ok, "{}", o.stderr);
    assert!(!e.has("Elsewhere/a.jpg"));
    assert!(e.has("Elsewhere/lonely.jpg"));
    // Copies inside the keep folder (its subfolders too) stay.
    assert!(e.has("Originals/duplicates/a.jpg"));
}

#[test]
fn clean_protect_and_bundles_never_move() {
    let e = Env::new("protect");
    e.file("keep/a", b"same");
    e.file("other/a", b"same");
    e.file("safe/a", b"same");
    e.file("Lib.photoslibrary/a", b"same");
    let o = e.run(&["clean", ".", "--keep", "keep", "--protect", "safe", "--stage", &stage(&e)], "YES\n");
    assert!(o.ok, "{}", o.stderr);
    assert!(!e.has("other/a"));
    assert!(e.has("safe/a") && e.has("Lib.photoslibrary/a") && e.has("keep/a"));
}

#[test]
fn clean_never_moves_a_group_without_a_kept_copy() {
    let e = Env::new("nokeeper");
    e.file("keep/x", b"keep me");
    e.file("dups/a", b"twins");
    e.file("dups/b", b"twins");
    let o = e.run(&["clean", "--keep", "keep", "--remove", "dups", "--stage", &stage(&e)], "YES\n");
    assert!(o.ok);
    assert!(o.stdout.contains("Nothing to move"), "{}", o.stdout);
    assert!(e.has("dups/a") && e.has("dups/b"));
}

#[test]
fn clean_rejects_bad_arguments() {
    let e = Env::new("cleanbad");
    e.file("A/x", b"1");
    assert!(!e.run(&["clean", "--keep", "missing", "--stage", &stage(&e)], "YES\n").ok);
    assert!(!e.run(&["clean", "--keep", "A", "--remove", "A", "--stage", &stage(&e)], "YES\n").ok);
    assert!(!e.run(&["clean", "--keep", "A", "--stage", "A/stage"], "YES\n").ok);
    assert!(!e.run(&["clean"], "").ok, "--keep is required");
}

#[test]
fn second_scan_skips_stage_folder() {
    let e = Env::new("restage");
    e.file("keep/a", b"same");
    e.file("dups/a", b"same");
    let st = e.data.join("_TO_DELETE");
    let o = e.run(&["clean", "--keep", "keep", "--remove", "dups", "--stage", st.to_str().unwrap()], "YES\n");
    assert!(o.ok, "{}", o.stderr);
    let o = e.run(&["scan"], "");
    assert!(o.stderr.contains("0 duplicate groups"), "{}", o.stderr);
}

#[test]
fn purge_needs_delete_and_marker() {
    let e = Env::new("purge");
    photos(&e);
    let st = stage(&e);
    e.run(&["clean", "--keep", "Originals", "--remove", "Originals/duplicates", "--stage", &st], "YES\n");
    assert!(Path::new(&st).exists());

    let o = e.run(&["purge", &st], "yes\n");
    assert!(o.stderr.contains("Nothing was deleted"));
    assert!(Path::new(&st).exists());

    let o = e.run(&["purge", &st], "DELETE\n");
    assert!(o.ok, "{}", o.stderr);
    assert!(!Path::new(&st).exists());
    assert!(e.has("Originals/a.jpg"));

    let o = e.run(&["purge", "Originals"], "DELETE\n");
    assert!(!o.ok);
    assert!(e.has("Originals/a.jpg"), "purge must refuse folders clean did not create");
}

// ---------- empty-dirs ----------

#[test]
fn empty_dirs_dry_run_then_delete() {
    let e = Env::new("emptydirs");
    e.file("full/a", b"x");
    fs::create_dir_all(e.data.join("gone/deep/er")).unwrap();
    e.file("meta/.DS_Store", b"x");

    let o = e.run(&["empty-dirs", "--dry-run"], "");
    assert!(o.stdout.contains("gone") && !o.stdout.contains("meta"), "{}", o.stdout);
    assert!(e.has("gone/deep/er"));

    let o = e.run(&["empty-dirs"], "nope\n");
    assert!(o.stderr.contains("Nothing was deleted"));
    assert!(e.has("gone"));

    let o = e.run(&["empty-dirs", "--include-metadata"], "YES\n");
    assert!(o.ok);
    assert!(!e.has("gone") && !e.has("meta"));
    assert!(e.has("full/a"));
}

// ---------- guided mode ----------

#[test]
fn guided_mode_keeps_chosen_folder() {
    let e = Env::new("guided");
    e.file("Trip/a.jpg", b"photo a");
    e.file("Trip/b.jpg", b"photo bb");
    e.file("Trip copy/a.jpg", b"photo a");
    e.file("Trip copy/b.jpg", b"photo bb");
    let folder = e.data.to_str().unwrap();
    // folder, all sizes, keep folder 2, YES, don't open Finder, don't DELETE, no empty-dir pass
    let input = format!("{folder}\nn\n2\nYES\nn\nkeep\nn\n");
    let o = e.run(&[], &input);
    assert!(o.ok, "{}\n{}", o.stdout, o.stderr);
    // Sets list folders sorted, so "Trip" is 1 and "Trip copy" is 2: picking 2 keeps "Trip copy".
    assert!(e.has("Trip copy/a.jpg") && e.has("Trip copy/b.jpg"));
    assert!(!e.has("Trip/a.jpg") && !e.has("Trip/b.jpg"));
    assert!(e.has("_TO_DELETE/Trip/a.jpg"));
}

#[test]
fn guided_mode_quit_moves_nothing() {
    let e = Env::new("guided-q");
    e.file("x/a", b"same");
    e.file("y/a", b"same");
    let o = e.run(&[], &format!("{}\nn\nq\n", e.data.to_str().unwrap()));
    assert!(o.ok);
    assert!(o.stdout.contains("Nothing chosen"), "{}", o.stdout);
    assert!(e.has("x/a") && e.has("y/a"));
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in fs::read_dir(dir).unwrap().flatten() {
        let p = entry.path();
        if p.is_dir() { out.extend(walk(&p)) } else { out.push(p) }
    }
    out
}
