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
    line.trim() == word
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
