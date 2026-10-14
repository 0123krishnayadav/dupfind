//! dupfind: find duplicate files and clean them up safely.
//!
//!   dupfind scan       list duplicates (read-only)
//!   dupfind clean      move copies that have a twin in a --keep folder into a stage folder
//!   dupfind purge      permanently delete a stage folder created by `clean`
//!   dupfind empty-dirs remove empty folders

mod clean;
mod empty;
mod scan;
mod util;

use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "dupfind", version, about = "Find duplicate files and clean them up safely")]
struct Cli {
    /// Number of threads (default: all CPU cores)
    #[arg(long, global = true)]
    threads: Option<usize>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List duplicate files. Read-only, never changes anything.
    Scan {
        #[command(flatten)]
        opts: scan::ScanOpts,

        /// Show which folders hold copies of each other instead of every file
        #[arg(long)]
        folders: bool,

        /// Also save every duplicate to this CSV file (never overwrites)
        #[arg(long)]
        csv: Option<PathBuf>,
    },

    /// Move copies that have an identical twin in a --keep folder into a stage folder.
    /// Files with no copy in a keep folder are never touched. Asks before moving.
    Clean {
        #[command(flatten)]
        opts: scan::ScanOpts,

        /// Folder whose copies are kept (repeatable). Its subfolders are kept too.
        #[arg(long, required = true)]
        keep: Vec<PathBuf>,

        /// Folder whose files are never moved (repeatable). .git, .app and Photos libraries always are.
        #[arg(long)]
        protect: Vec<PathBuf>,

        /// Where copies are moved to [default: <first folder>/_TO_DELETE]. Must be on the same drive.
        #[arg(long)]
        stage: Option<PathBuf>,

        /// Only show what would be moved
        #[arg(long)]
        dry_run: bool,
    },

    /// Permanently delete a stage folder created by `clean`. Asks first.
    Purge {
        /// The stage folder, e.g. /Volumes/T7/_TO_DELETE
        stage: PathBuf,
    },

    /// Delete empty folders. By default only folders with no files at all, hidden ones included.
    EmptyDirs {
        #[arg(required = true)]
        roots: Vec<PathBuf>,

        /// Also treat folders holding only macOS .DS_Store / ._ files as empty
        #[arg(long)]
        include_metadata: bool,

        /// Only list them
        #[arg(long)]
        dry_run: bool,
    },
}

fn main() {
    let cli = Cli::parse();
    if let Some(n) = cli.threads {
        rayon::ThreadPoolBuilder::new().num_threads(n).build_global().ok();
    }
    let result = match &cli.cmd {
        Cmd::Scan { opts, folders, csv } => {
            if let Some(csv) = csv.as_ref().filter(|c| c.exists()) {
                eprintln!("error: {} already exists (choose a new CSV name)", csv.display());
                std::process::exit(1);
            }
            let groups = scan::find_duplicates(opts, &[]);
            let shown = if *folders { scan::print_folders(&groups, &opts.roots, 40) } else { scan::print_files(&groups) };
            shown.and_then(|_| csv.as_ref().map_or(Ok(()), |c| scan::write_csv(&groups, c)))
        }
        Cmd::Clean { opts, keep, protect, stage, dry_run } => clean::clean(opts, keep, protect, stage.as_deref(), *dry_run),
        Cmd::Purge { stage } => clean::purge(stage),
        Cmd::EmptyDirs { roots, include_metadata, dry_run } => empty::empty_dirs(roots, *include_metadata, *dry_run),
    };
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
