use std::fs::File;
use std::io::{self, BufRead, Read, Write};
use std::path::{Path, PathBuf};

/// macOS metadata files: `.DS_Store` (Finder settings) and `._name` (AppleDouble, on exFAT/FAT drives).
pub fn is_metadata(name: &str) -> bool {
    name == ".DS_Store" || name.starts_with("._")
}

/// Folders that are managed by an app and must never be edited file by file.
pub fn is_managed_bundle(name: &str) -> bool {
    name == ".git" || name.ends_with(".photoslibrary") || name.ends_with(".app")
}

pub fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim().to_uppercase();
    let s = s.trim_end_matches('B');
    let (num, mult) = match s.chars().last() {
        Some('K') => (&s[..s.len() - 1], 1u64 << 10),
        Some('M') => (&s[..s.len() - 1], 1 << 20),
        Some('G') => (&s[..s.len() - 1], 1 << 30),
        _ => (s, 1),
    };
    num.parse::<f64>()
        .map(|n| (n * mult as f64) as u64)
        .map_err(|e| format!("bad size '{s}': {e}"))
}

pub fn files_word(n: usize) -> String {
    if n == 1 { "1 file".to_string() } else { format!("{n} files") }
}

pub fn human(n: u64) -> String {
    let mut n = n as f64;
    for unit in ["B", "KB", "MB", "GB", "TB"] {
        if n < 1024.0 {
            return format!("{n:.1} {unit}");
        }
        n /= 1024.0;
    }
    format!("{n:.1} PB")
}

/// Absolute path without requiring it to exist (canonicalizes the longest existing parent).
pub fn absolute(p: &Path) -> PathBuf {
    if let Ok(c) = std::fs::canonicalize(p) {
        return c;
    }
    let abs = if p.is_absolute() { p.to_path_buf() } else { std::env::current_dir().unwrap().join(p) };
    match (abs.parent(), abs.file_name()) {
        (Some(parent), Some(name)) => absolute(parent).join(name),
        _ => abs,
    }
}

pub fn is_inside(path: &Path, dirs: &[PathBuf]) -> bool {
    dirs.iter().any(|d| path.starts_with(d) && path != d)
}

/// Ask the user to type an exact word; anything else means no.
pub fn confirm(prompt: &str, word: &str) -> bool {
    eprint!("{prompt}\nType {word} to continue, anything else to cancel: ");
    io::stderr().flush().ok();
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line).ok();
    line.trim().eq_ignore_ascii_case(word)
}

/// Byte-by-byte comparison of two files.
pub fn same_content(a: &Path, b: &Path) -> io::Result<bool> {
    let (mut fa, mut fb) = (File::open(a)?, File::open(b)?);
    if fa.metadata()?.len() != fb.metadata()?.len() {
        return Ok(false);
    }
    let mut ba = vec![0u8; 1 << 20];
    let mut bb = vec![0u8; 1 << 20];
    loop {
        let n = read_full(&mut fa, &mut ba)?;
        if n == 0 {
            return Ok(true);
        }
        if read_full(&mut fb, &mut bb[..n])? != n || ba[..n] != bb[..n] {
            return Ok(false);
        }
    }
}

fn read_full(f: &mut File, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match f.read(&mut buf[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    Ok(filled)
}

/// A fresh empty folder for a test, removed when dropped.
#[cfg(test)]
pub struct TestDir(pub PathBuf);

#[cfg(test)]
impl TestDir {
    pub fn new(name: &str) -> Self {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("dupfind-test-{}-{name}-{n}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        TestDir(std::fs::canonicalize(&dir).unwrap())
    }

    /// Write `content` to `rel`, creating parent folders.
    pub fn file(&self, rel: &str, content: &[u8]) -> PathBuf {
        let p = self.0.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, content).unwrap();
        p
    }
}

#[cfg(test)]
impl Drop for TestDir {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_size_units() {
        assert_eq!(parse_size("1"), Ok(1));
        assert_eq!(parse_size("4K"), Ok(4096));
        assert_eq!(parse_size("4kb"), Ok(4096));
        assert_eq!(parse_size("1M"), Ok(1 << 20));
        assert_eq!(parse_size(" 2G "), Ok(2 << 30));
        assert_eq!(parse_size("1.5K"), Ok(1536));
        assert!(parse_size("abc").is_err());
        assert!(parse_size("").is_err());
    }

    #[test]
    fn human_sizes() {
        assert_eq!(human(0), "0.0 B");
        assert_eq!(human(1023), "1023.0 B");
        assert_eq!(human(1024), "1.0 KB");
        assert_eq!(human(1536), "1.5 KB");
        assert_eq!(human(1 << 30), "1.0 GB");
        assert_eq!(human(1 << 50), "1.0 PB");
    }

    #[test]
    fn files_word_plural() {
        assert_eq!(files_word(0), "0 files");
        assert_eq!(files_word(1), "1 file");
        assert_eq!(files_word(2), "2 files");
    }

    #[test]
    fn metadata_and_bundles() {
        assert!(is_metadata(".DS_Store"));
        assert!(is_metadata("._photo.jpg"));
        assert!(!is_metadata("photo.jpg"));
        assert!(!is_metadata(".hidden"));
        assert!(is_managed_bundle(".git"));
        assert!(is_managed_bundle("Photos Library.photoslibrary"));
        assert!(is_managed_bundle("Safari.app"));
        assert!(!is_managed_bundle("apps"));
        assert!(!is_managed_bundle("git"));
    }

    #[test]
    fn inside_is_strict() {
        let dirs = vec![PathBuf::from("/a/b")];
        assert!(is_inside(Path::new("/a/b/c"), &dirs));
        assert!(!is_inside(Path::new("/a/b"), &dirs));
        assert!(!is_inside(Path::new("/a/bc"), &dirs));
        assert!(!is_inside(Path::new("/a"), &dirs));
    }

    #[test]
    fn absolute_resolves_missing_paths() {
        let t = TestDir::new("abs");
        assert_eq!(absolute(&t.0), t.0);
        assert_eq!(absolute(&t.0.join("missing/x")), t.0.join("missing/x"));
        std::fs::create_dir(t.0.join("sub")).unwrap();
        assert_eq!(absolute(&t.0.join("sub/../y")), t.0.join("y"));
    }

    #[test]
    fn same_content_compares_bytes() {
        let t = TestDir::new("same");
        let big: Vec<u8> = (0..3_000_000u32).map(|i| i as u8).collect();
        let mut changed = big.clone();
        changed[2_500_000] ^= 1;
        let a = t.file("a", &big);
        let b = t.file("b", &big);
        let c = t.file("c", &changed);
        let d = t.file("d", &big[..100]);
        let e = t.file("e", b"");
        let f = t.file("f", b"");
        assert!(same_content(&a, &b).unwrap());
        assert!(!same_content(&a, &c).unwrap());
        assert!(!same_content(&a, &d).unwrap());
        assert!(same_content(&e, &f).unwrap());
        assert!(same_content(&a, &t.0.join("missing")).is_err());
    }
}
