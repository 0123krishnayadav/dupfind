//! dupfind: find duplicate files and clean them up safely.
//!
//!   dupfind            guided mode: asks questions step by step
//!   dupfind scan       list duplicates (read-only)
//!   dupfind clean      move copies that have a twin in a --keep folder into a stage folder
//!   dupfind purge      permanently delete a stage folder created by `clean`
//!   dupfind empty-dirs remove empty folders

mod clean;
mod empty;
mod scan;
mod util;
mod wizard;

use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "dupfind",
    version,
    about = "Find duplicate files and clean them up safely",
    after_help = "Run `dupfind` with no command for guided mode: it asks what to check and what to keep."
)]
struct Cli {
    /// Number of threads (default: all CPU cores)
    #[arg(long, global = true)]
    threads: Option<usize>,

    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// List duplicate files. Read-only, never changes anything.
    Scan {
        #[command(flatten)]
        opts: scan::ScanOpts,

        /// List every duplicate file instead of the folder summary
        #[arg(long)]
        files: bool,

        /// Also save every duplicate to this CSV file (never overwrites)
        #[arg(long)]
        csv: Option<PathBuf>,
    },

    /// Move copies of files in a --keep folder into _TO_DELETE. Asks before moving.
    /// Example: dupfind clean --keep "Originals" --remove "Originals/duplicates"
    Clean(clean::CleanOpts),

    /// Permanently delete a stage folder created by `clean`. Asks first.
    Purge {
        /// The stage folder, e.g. /Volumes/T7/_TO_DELETE
        #[arg(default_value = "_TO_DELETE")]
        stage: PathBuf,
    },

    /// Delete empty folders. By default only folders with no files at all, hidden ones included.
    EmptyDirs {
        /// Folders to look in [default: current folder]
        #[arg(default_value = ".")]
        roots: Vec<PathBuf>,

        /// Also treat folders holding only macOS .DS_Store / ._ files as empty
        #[arg(long)]
        include_metadata: bool,

        /// Only list them
        #[arg(long)]
        dry_run: bool,
    },
}

/// After a scan typed by a person, offer to clean up right away.
fn offer_cleanup(groups: &[scan::Group], root: &std::path::Path) -> std::io::Result<()> {
    if groups.is_empty() || !wizard::interactive() {
        return Ok(());
    }
    if util::confirm("\nClean up now? You choose which folder to keep in each set.", "YES") {
        wizard::review(groups, &util::absolute(root))?;
    }
    Ok(())
}

fn main() {
    let cli = Cli::parse();
    if let Some(n) = cli.threads {
        rayon::ThreadPoolBuilder::new().num_threads(n).build_global().ok();
    }
    let Some(cmd) = &cli.cmd else {
        if let Err(e) = wizard::run() {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
        return;
    };
    let result = match cmd {
        Cmd::Scan { opts, files, csv } => {
            if let Some(csv) = csv.as_ref().filter(|c| c.exists()) {
                eprintln!("error: {} already exists (choose a new CSV name)", csv.display());
                std::process::exit(1);
            }
            let groups = scan::find_duplicates(opts, &[]);
            let shown = if *files { scan::print_files(&groups) } else { scan::print_folders(&groups, &opts.roots, 40) };
            shown
                .and_then(|_| csv.as_ref().map_or(Ok(()), |c| scan::write_csv(&groups, c)))
                .and_then(|_| offer_cleanup(&groups, &opts.roots[0]))
        }
        Cmd::Clean(o) => clean::clean(o),
        Cmd::Purge { stage } => clean::purge(stage),
        Cmd::EmptyDirs { roots, include_metadata, dry_run } => empty::empty_dirs(roots, *include_metadata, *dry_run),
    };
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
