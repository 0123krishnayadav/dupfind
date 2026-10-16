//! Interactive cleanup.
//!
//! `review` goes through the duplicate sets biggest first, asks which folder to
//! keep in each, then moves the other copies after one final YES. It is used by
//! guided mode (`dupfind` with no command) and offered at the end of `dupfind scan`.

use crate::clean::{self, Move, in_managed_bundle, rel};
use crate::empty;
use crate::scan::{self, Group, ScanOpts};
use crate::util::{absolute, confirm, files_word, human};
use std::collections::HashMap;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

/// Duplicate groups that live in exactly the same set of folders.
struct Set<'a> {
    dirs: Vec<PathBuf>,
    groups: Vec<&'a Group>,
    wasted: u64,
}

fn ask(prompt: &str) -> String {
    print!("{prompt}");
    io::stdout().flush().ok();
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line).ok();
    line.trim().to_string()
}

/// A folder dragged in from Finder arrives quoted, or with a backslash before each
/// special character: `/Volumes/T7/Trip\ \(1\)`.
fn typed_path(s: &str) -> PathBuf {
    let s = s.trim().trim_matches(|c| c == '\'' || c == '"');
    let mut out = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        out.push(if c == '\\' { chars.next().unwrap_or(c) } else { c });
    }
    PathBuf::from(out)
}

fn is_yes(answer: &str) -> bool {
    !answer.to_lowercase().starts_with('n')
}

/// True when a person is typing and reading, not a script or a redirected output.
pub fn interactive() -> bool {
    io::stdin().is_terminal() && io::stdout().is_terminal()
}

/// Guided mode: ask which folder, scan it, then review.
pub fn run() -> io::Result<()> {
    println!("dupfind finds duplicate files and removes the extra copies safely.");
    println!("Nothing is deleted without you typing YES or DELETE.\n");

    let folder = loop {
        let a = ask("Which folder or drive should I check?\n(Drag it here from Finder, then press Enter): ");
        if a.is_empty() {
            return Ok(());
        }
        let p = absolute(&typed_path(&a));
        if p.is_dir() {
            break p;
        }
        println!("That is not a folder: {}\n", p.display());
    };
    let min_size = if is_yes(&ask("\nOnly check files of 1 MB and up? Much faster, finds almost all wasted space. [Y/n]: ")) {
        1 << 20
    } else {
        1
    };
    println!();
    let opts = ScanOpts { roots: vec![folder.clone()], min_size, exclude: vec![], no_cache: false };
    let groups = scan::find_duplicates(&opts, &[]);
    review(&groups, &folder)
}

fn build_sets(groups: &[Group]) -> Vec<Set<'_>> {
    let mut by_dirs: HashMap<Vec<PathBuf>, Vec<&Group>> = HashMap::new();
    for g in groups {
        let mut dirs: Vec<PathBuf> = g.paths.iter().map(|p| p.parent().unwrap().to_path_buf()).collect();
        dirs.sort();
        dirs.dedup();
        by_dirs.entry(dirs).or_default().push(g);
    }
    let mut sets: Vec<Set> = by_dirs
        .into_iter()
        .map(|(dirs, groups)| Set { wasted: groups.iter().map(|g| g.wasted()).sum(), dirs, groups })
        .collect();
    sets.sort_by(|a, b| b.wasted.cmp(&a.wasted).then_with(|| a.dirs.cmp(&b.dirs)));
    sets
}

/// The copy that stays when several sit in one folder: the shortest name is usually
/// the original (`x.jpg` over `x (1).jpg` or `x copy.jpg`).
fn original<'a>(paths: impl Iterator<Item = &'a PathBuf>) -> &'a PathBuf {
    paths.min_by_key(|p| (p.file_name().map_or(0, |n| n.len()), p.as_path())).unwrap()
}

/// The copies that move when the `keep` folders are chosen. In a single-folder set,
/// one copy of each file stays.
fn moves_for(set: &Set, keep: &[&PathBuf]) -> Vec<Move> {
    let kept = |p: &PathBuf| p.parent().is_some_and(|d| keep.iter().any(|k| k.as_path() == d));
    let mut moves = Vec::new();
    for g in &set.groups {
        let keeper = original(g.paths.iter().filter(|p| kept(p)));
        for p in &g.paths {
            let stays = p == keeper || (set.dirs.len() > 1 && kept(p));
            if !stays && !in_managed_bundle(p) {
                moves.push(Move { src: p.clone(), keeper: keeper.clone(), size: g.size });
            }
        }
    }
    moves
}

/// "2" or "1,3" or "1 3" -> folder numbers, all within 1..=n.
fn parse_choice(a: &str, n: usize) -> Option<Vec<usize>> {
    let mut picks: Vec<usize> = a
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|t| !t.is_empty())
        .map(|t| t.parse().ok().filter(|k| (1..=n).contains(k)))
        .collect::<Option<_>>()?;
    picks.sort();
    picks.dedup();
    (!picks.is_empty()).then_some(picks)
}

/// Ask set by set which folder to keep, then move everything chosen after one YES.
pub fn review(groups: &[Group], folder: &Path) -> io::Result<()> {
    // Inside the chosen folder: same drive, easy to find, and skipped by later scans.
    let stage = folder.join("_TO_DELETE");
    let sets = build_sets(groups);
    if sets.is_empty() {
        println!("\nNo duplicates to clean.");
        return Ok(());
    }
    println!("\nFor each set, choose the folder whose files you want to KEEP.");
    println!("Type Q any time to stop and move just what you chose. Nothing moves until you type YES.");

    let (mut plan, mut chosen) = (Vec::new(), 0);
    for (i, set) in sets.iter().enumerate() {
        let extra: usize = set.groups.iter().map(|g| g.paths.len() - 1).sum();
        let copies = if extra == 1 { "1 extra copy".to_string() } else { format!("{extra} extra copies") };
        println!("\n[{}/{}]  {}  {copies}", i + 1, sets.len(), human(set.wasted));
        for (j, d) in set.dirs.iter().enumerate() {
            let note = if in_managed_bundle(d) {
                "   (app or Photos library: never touched)"
            } else if set.dirs.len() == 1 {
                "   (copies inside this one folder)"
            } else {
                ""
            };
            println!("   {}) {}{note}", j + 1, rel(d, folder).display());
        }
        let n = set.dirs.len();
        let prompt = if n == 1 {
            "Type 1 to keep one copy of each (the shortest name), Enter = skip, Q = done: ".to_string()
        } else {
            format!("Keep which folder? 1-{n} (or several, like 1,3), Enter = skip, Q = done: ")
        };
        let a = ask(&prompt);
        if a.is_empty() {
            continue;
        }
        let Some(picks) = parse_choice(&a, n) else {
            break; // Q or anything else: done choosing
        };
        let keep: Vec<&PathBuf> = picks.iter().map(|k| &set.dirs[k - 1]).collect();
        let moves = moves_for(set, &keep);
        if moves.is_empty() {
            println!("   Nothing to move: every copy is in a folder you keep, an app or a Photos library.");
            continue;
        }
        let bytes: u64 = moves.iter().map(|m| m.size).sum();
        let names: Vec<String> = keep.iter().map(|k| rel(k, folder).display().to_string()).collect();
        println!("   Keeping {}. {} ({}) will move.", names.join(" + "), files_word(moves.len()), human(bytes));
        if n == 1 {
            let m = &moves[0];
            println!(
                "   For example, keeps \"{}\" and moves \"{}\".",
                m.keeper.file_name().unwrap().to_string_lossy(),
                m.src.file_name().unwrap().to_string_lossy()
            );
        }
        plan.extend(moves);
        chosen += 1;
        let total: u64 = plan.iter().map(|m| m.size).sum();
        println!("   Chosen so far: {chosen} {}, {} ({}). Type Q at the next set to move just these.",
            if chosen == 1 { "set" } else { "sets" }, files_word(plan.len()), human(total));
    }

    if plan.is_empty() {
        println!("\nNothing chosen, nothing moved.");
        return Ok(());
    }
    let bytes: u64 = plan.iter().map(|m| m.size).sum();
    let prompt = format!(
        "\nYou chose {chosen} {}: move {} ({}) into {}.\nNothing is deleted yet; each file is checked byte by byte first.",
        if chosen == 1 { "set" } else { "sets" },
        files_word(plan.len()),
        human(bytes),
        stage.display()
    );
    if !confirm(&prompt, "YES") {
        println!("Cancelled. Nothing was moved.");
        return Ok(());
    }
    clean::execute(&plan, &stage, folder)?;
    finish(&stage, folder)
}

/// After moving: look, free the space, tidy empty folders. Each step asks.
fn finish(stage: &Path, folder: &Path) -> io::Result<()> {
    println!("\nThe extra copies are in {}.", stage.display());
    if is_yes(&ask("Open it in Finder to look through? [Y/n]: ")) {
        std::process::Command::new("open").arg(stage).status().ok();
        ask("Press Enter when you are done looking: ");
    }
    clean::purge(stage)?;
    if stage.exists() {
        println!("Kept. To delete it later: dupfind purge \"{}\"", stage.display());
    }
    if is_yes(&ask("\nLook for folders that are now empty? [Y/n]: ")) {
        empty::empty_dirs(&[folder.to_path_buf()], false, false)?;
    }
    println!("\nDone.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g(size: u64, paths: &[&str]) -> Group {
        Group { size, paths: paths.iter().map(PathBuf::from).collect() }
    }

    #[test]
    fn typed_path_handles_finder_drag() {
        assert_eq!(typed_path("/Volumes/T7/Trip\\ \\(1\\) "), PathBuf::from("/Volumes/T7/Trip (1)"));
        assert_eq!(typed_path("'/a b/c'"), PathBuf::from("/a b/c"));
        assert_eq!(typed_path("\"/a b\""), PathBuf::from("/a b"));
        assert_eq!(typed_path("/plain"), PathBuf::from("/plain"));
    }

    #[test]
    fn parse_choice_accepts_numbers_in_range() {
        assert_eq!(parse_choice("2", 3), Some(vec![2]));
        assert_eq!(parse_choice("3,1", 3), Some(vec![1, 3]));
        assert_eq!(parse_choice("1 3 1", 3), Some(vec![1, 3]));
        assert_eq!(parse_choice("4", 3), None);
        assert_eq!(parse_choice("0", 3), None);
        assert_eq!(parse_choice("q", 3), None);
        assert_eq!(parse_choice("1,q", 3), None);
        assert_eq!(parse_choice("", 3), None);
    }

    #[test]
    fn is_yes_defaults_to_yes() {
        assert!(is_yes(""));
        assert!(is_yes("y"));
        assert!(!is_yes("n"));
        assert!(!is_yes("No"));
    }

    #[test]
    fn original_is_shortest_name() {
        let ps = [PathBuf::from("/d/x (1).jpg"), PathBuf::from("/d/x.jpg"), PathBuf::from("/d/x copy.jpg")];
        assert_eq!(original(ps.iter()), &PathBuf::from("/d/x.jpg"));
    }

    #[test]
    fn sets_group_by_folders_biggest_first() {
        let groups = [g(10, &["/a/1", "/b/1"]), g(1, &["/a/2", "/b/2"]), g(5, &["/c/x", "/c/x copy"])];
        let sets = build_sets(&groups);
        assert_eq!(sets.len(), 2);
        assert_eq!(sets[0].dirs, vec![PathBuf::from("/a"), PathBuf::from("/b")]);
        assert_eq!(sets[0].wasted, 11);
        assert_eq!(sets[1].dirs, vec![PathBuf::from("/c")]);
    }

    #[test]
    fn moves_for_keeps_chosen_folders() {
        let groups = [g(3, &["/a/f", "/b/f", "/c/f"])];
        let sets = build_sets(&groups);
        let (a, b) = (PathBuf::from("/a"), PathBuf::from("/b"));

        let m = moves_for(&sets[0], &[&a]);
        let srcs: Vec<_> = m.iter().map(|m| m.src.to_str().unwrap()).collect();
        assert_eq!(srcs, ["/b/f", "/c/f"]);
        assert!(m.iter().all(|m| m.keeper == PathBuf::from("/a/f")));

        let m = moves_for(&sets[0], &[&a, &b]);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].src, PathBuf::from("/c/f"));
    }

    #[test]
    fn moves_for_single_folder_keeps_one_copy() {
        let groups = [g(3, &["/d/x.jpg", "/d/x (1).jpg", "/d/x copy.jpg"])];
        let sets = build_sets(&groups);
        let d = PathBuf::from("/d");
        let m = moves_for(&sets[0], &[&d]);
        assert_eq!(m.len(), 2);
        assert!(m.iter().all(|m| m.keeper == PathBuf::from("/d/x.jpg") && m.src != m.keeper));
    }

    #[test]
    fn moves_for_never_touches_bundles() {
        let groups = [g(3, &["/a/f", "/L.photoslibrary/f"])];
        let sets = build_sets(&groups);
        let a = PathBuf::from("/a");
        assert!(moves_for(&sets[0], &[&a]).is_empty());
    }
}
