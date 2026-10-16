# dupfind

Find duplicate files on any folder or drive and remove the extra copies without ever losing the last one. Files are compared by content, so renamed copies are found too.

## The easy way

Open Terminal and run either:

```bash
dupfind                      # asks which folder or drive to check
dupfind scan /Volumes/T7     # shows the folders that share copies, then offers to clean up
```

After the scan, type `YES` to clean up. dupfind then goes through the sets of folders holding the same files, biggest first, and asks which folder to keep:

```
[1/40]  2.8 GB  232 extra copies
   1) Photos/Originals
   2) Photos/Originals/duplicates
Keep which folder? 1-2 (or several, like 1,3), Enter = skip, Q = done: 1
   Keeping Photos/Originals. 232 files (2.8 GB) will move.
   Chosen so far: 1 set, 232 files (2.8 GB). Type Q at the next set to move just these.
```

| You type | What happens |
| --- | --- |
| A number, like `2` | Keep that folder. The copies in the set's other folders will move. |
| Several numbers, like `1,3` | Keep all of those folders. Only the copies in the others will move. |
| Enter | Skip this set and leave it as it is. |
| `Q` (or any other text) | Stop choosing and move just the sets chosen so far. |

A set with only one folder holds several copies of the same file side by side. Typing `1` keeps the copy with the shortest name, usually the original (`x.jpg` over `x (1).jpg` or `x copy.jpg`), and moves the rest.

Then it shows what you chose in total and asks once more:

1. **Type `YES`** to move the extra copies into `_TO_DELETE` inside the folder you scanned. Each file is checked byte by byte against the copy you keep first.
2. **Look through `_TO_DELETE`?** It opens in Finder.
3. **Type `DELETE`** to delete `_TO_DELETE` and free the space, or anything else to keep it for now.
4. **Remove folders that are now empty?** Type `YES` to remove them.

Nothing moves without `YES`, and nothing is deleted without `DELETE` (capitals or not). Any other answer cancels that step. When `scan` runs from a script or its output goes to a file, it only lists and never asks.

## Install

### On a new Mac

```bash
# 1. Apple's developer tools (Rust needs them to build)
xcode-select --install

# 2. Rust: press Enter to accept the defaults, then open a new Terminal window
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# 3. Get the code and install dupfind
git clone git@github.com:0123krishnayadav/dupfind.git
cd dupfind
cargo install --path .

# 4. Check it works
dupfind --help
```

The first build takes about a minute. `cargo install` puts `dupfind` in `~/.cargo/bin`, so it runs from any folder.

Step 3 clones over SSH, which needs an SSH key added to your GitHub account. Without one, clone over HTTPS instead: `git clone https://github.com/0123krishnayadav/dupfind.git`.

### Update to the latest version

```bash
cd dupfind
git pull
cargo install --path .
```

### Uninstall

```bash
cargo uninstall dupfind
```

## Commands, for scripts and advanced use

Every command works on the current folder unless you name one, so you can `cd /Volumes/T7` and use short paths. Add `--help` after any command for all options.

### `dupfind scan`: see what is duplicated

Scanning only reads files. When you run it yourself, it then offers the cleanup above; press Enter to just exit.

```bash
dupfind scan /Volumes/T7                  # folders that hold the same files
dupfind scan /Volumes/T7 --files          # every duplicate file
dupfind scan ~ --min-size 1M --csv home.csv
```

| Option | What it does |
| --- | --- |
| `--files` | Lists every duplicate file instead of the folder summary |
| `--min-size 1M` | Skips smaller files: much faster |
| `--csv <file>` | Saves every duplicate to a CSV (never overwrites one) |
| `--exclude <folder>` | Skips a folder (repeatable) |
| `--no-cache` | Reads every file again instead of reusing hashes from earlier scans |

### `dupfind clean`: move the extra copies into `_TO_DELETE`

```bash
cd /Volumes/T7

# Keep "Originals", move the copies found inside its "duplicates" subfolder
dupfind clean --keep "Photos/Originals" --remove "Photos/Originals/duplicates"

# Keep two folders, move their copies from anywhere on the drive
dupfind clean . --keep "Photos/Edited" --keep "Photos/Originals"
```

| Option | Meaning |
| --- | --- |
| `--keep <folder>` | Files here stay (subfolders too, unless named in `--remove`). Required, repeatable. |
| `--remove <folder>` | Move copies only out of this folder. It may sit inside a keep folder; the deeper folder wins. |
| `<folders>` | Where to look for copies. Default: just the `--keep` and `--remove` folders. Without `--remove`, copies anywhere in these folders outside the keep folders move. |
| `--protect <folder>` | Files here never move. |
| `--stage <folder>` | Where copies go. Default: `_TO_DELETE` at the top of the drive, or in your home folder. |
| `--dry-run` | Show every file that would move, without asking or moving. |

Before asking `YES`, it shows how many copies would move from each folder. Then for each file:

1. It is compared byte by byte with its kept copy; a file that changed since the scan is skipped.
2. It moves into `_TO_DELETE` with its folder path, for example `_TO_DELETE/Photos/Originals/duplicates/a.jpg`.
3. The move is logged in `_TO_DELETE/dupfind-log-<time>.tsv`.

A file with no copy in a keep folder is never moved. To undo, move a file from `_TO_DELETE` back to the same path.

### `dupfind purge`: free the space

```bash
dupfind purge /Volumes/T7/_TO_DELETE      # or just `dupfind purge` from /Volumes/T7
```

Type `DELETE` to confirm. This skips the Trash and cannot be undone. It refuses any folder that `clean` did not create, which `clean` marks with a hidden `.dupfind-stage` file.

### `dupfind clear-cache`: forget saved hashes

Scans remember each file's hash in `~/Library/Caches/dupfind/hashes.bin`, so the next scan only reads files that are new or changed. This removes that file; the next scan reads everything again. It is safe to delete at any time.

### `dupfind empty-dirs`: remove empty folders

```bash
dupfind empty-dirs /Volumes/T7 --dry-run  # list only
dupfind empty-dirs /Volumes/T7            # list, then type YES
```

By default a folder counts as empty only if it has no files at all, hidden files included. With `--include-metadata`, folders holding only macOS `.DS_Store` / `._` files count too, and those files are removed with them.

Never removed: `.git` folders, `.app` bundles, Photos libraries and drive system folders (`.Trashes`, `.Spotlight-V100`, `.fseventsd`, `$RECYCLE.BIN`).

## Safety

- One copy always stays: a file only moves when a byte-identical copy remains in the folder you keep.
- Nothing moves without `YES` and nothing is deleted without `DELETE`.
- Files inside `.git`, `.app` and Photos libraries (`.photoslibrary`) never move.
- CSV files are never overwritten.

## Good to know

- **GB in Terminal vs Disk Utility:** `df -h` counts 1 GB as 1,073,741,824 bytes; Disk Utility counts 1,000,000,000. Use `df -H` to match Disk Utility.
- **`._` files:** macOS creates them on exFAT drives. They are ignored and are not real duplicates.
- **Photos library:** clean duplicates inside it with the Photos app.
- **Whole Mac:** `sudo dupfind scan / --min-size 1M`. `sudo` only lets it read protected folders.
- **Backups:** after cleanup one copy remains. Keep important files on a second drive or in the cloud too.

## How it works

Duplicates are found in three stages, each only looking at files that survived the last:

1. Parallel directory walk, files grouped by size (no file reads).
2. Hash of the first and last 64 KB of same-size files.
3. Full hash of the files still matching.

Hashing uses xxh3-128 on all CPU cores. Hard links count as one file.

Hashes from stages 2 and 3 are saved in a cache. The next scan still walks every folder, which is how it finds new, moved and deleted files, but it reuses a saved hash when the file's size, inode, modified time and change time are all unchanged. A rescan of a mostly unchanged drive therefore takes about as long as the walk alone. A wrong cache entry cannot cause data loss, because `clean` compares each file byte by byte with its kept copy before moving it.

| File | Contents |
| --- | --- |
| `src/main.rs` | Command-line interface |
| `src/wizard.rs` | Guided mode |
| `src/scan.rs` | Finding duplicates, reports, CSV |
| `src/cache.rs` | Hash cache between scans |
| `src/clean.rs` | `clean` and `purge` |
| `src/empty.rs` | `empty-dirs` |
| `src/util.rs` | Shared helpers |

## Development

```bash
cargo test
```

Unit tests sit at the bottom of each file in `src/`. `tests/cli.rs` runs the real `dupfind` binary on temporary folders and answers its prompts, covering scan, the hash cache, clean, purge, empty-dirs and guided mode. Tests point `HOME` at a temporary folder, so they never touch your files or cache. Run them before every commit.

Add a test for every change, and write it before the code.

## License

[MIT](LICENSE)
