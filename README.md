# dupfind

Find duplicate files by content on any folder or drive, and clean them up without ever losing the last copy.

A cleanup is four commands, run in order. Only the last two delete anything, and both ask first.

| Step | Command | What it does | Changes files? |
| --- | --- | --- | --- |
| 1 | `dupfind scan` | Lists duplicates and which folders share them | Never |
| 2 | `dupfind clean` | Moves copies that have a twin in a folder you keep into `_TO_DELETE` | Moves only, after you type `YES` |
| 3 | `dupfind purge` | Permanently deletes the `_TO_DELETE` folder | Deletes, after you type `DELETE` |
| 4 | `dupfind empty-dirs` | Removes folders left empty | Deletes empty folders, after you type `YES` |

## Install

Needs [Rust](https://rustup.rs).

```bash
cargo install --path .
dupfind --help
```

This puts `dupfind` in `~/.cargo/bin`, so it runs from any folder. Add `--help` after any command for its options.

## Step 1: Scan (read-only)

```bash
dupfind scan /Volumes/T7 --folders              # which folders share copies
dupfind scan /Volumes/T7                        # every duplicate file, biggest first
dupfind scan ~ --min-size 1M --csv home.csv     # home folder, files 1 MB+, saved as CSV
```

| Option | What it does |
| --- | --- |
| `--folders` | Shows sets of folders holding the same files, by wasted space |
| `--min-size 1M` | Skips smaller files; much faster, still finds most wasted space |
| `--csv <file>` | Also saves every duplicate to a CSV (never overwrites an existing file) |
| `--exclude <folder>` | Skips a folder (repeatable) |
| `--threads <n>` | Limits CPU cores used |

Example `--folders` output:

```
  2.8 GB wasted, 232 files
     T7/Wedding/Wedding Raw
     T7/Wedding/Wedding Raw/duplicates
```

Pick the folder in each block you want to keep; it becomes `--keep` in step 2.

Always skipped: empty files, macOS `._` and `.DS_Store` files, Trash folders, `.git`, `node_modules` and macOS system folders.

## Step 2: Clean (moves copies, deletes nothing)

Every file inside a `--keep` folder stays. Each identical copy found elsewhere moves into a staging folder. A file with no copy in a keep folder is never touched.

```bash
# Preview: what would move, by folder
dupfind clean /Volumes/T7 \
  --keep "/Volumes/T7/Wedding/Video Short" \
  --keep "/Volumes/T7/Wedding/Wedding Raw" \
  --protect "/Volumes/T7/Mac Copy/Documents/Final Selected" \
  --dry-run

# Run the same command without --dry-run, then type YES
```

| Option | Meaning |
| --- | --- |
| `--keep <folder>` | Files here are kept, subfolders included. Repeatable. Required. |
| `--protect <folder>` | Files here never move, though it is not a keep folder. |
| `--stage <folder>` | Where copies go. Default `<scanned folder>/_TO_DELETE`. Must be on the same drive. |
| `--dry-run` | Show the plan only. |

When you type `YES`:

1. Each copy is compared byte by byte with its kept twin; a file that changed since the scan is skipped.
2. It moves into `_TO_DELETE` with its folder path, so `T7/a/b.jpg` lands at `_TO_DELETE/a/b.jpg`.
3. Every move is logged to `dupfind-clean-<timestamp>.log` in the current folder.
4. At the end, every moved file is checked to still have its kept copy.

Anything else, including lowercase `yes`, cancels. To undo, move a file from `_TO_DELETE` back to the same path.

## Step 3: Purge (frees the space)

```bash
open /Volumes/T7/_TO_DELETE            # check first
dupfind purge /Volumes/T7/_TO_DELETE   # type DELETE
```

This skips the Trash and cannot be undone. It refuses any folder that `clean` did not create, which `clean` marks with a hidden `.dupfind-stage` file.

## Step 4: Remove empty folders

```bash
dupfind empty-dirs /Volumes/T7 --dry-run   # list only
dupfind empty-dirs /Volumes/T7             # list, then type YES
```

| Mode | A folder counts as empty when |
| --- | --- |
| default | It has no files at all, hidden ones included |
| `--include-metadata` | Its only files are macOS `.DS_Store` / `._` files, removed with it |

Never removed: the folder you name, `.git` folders, `.app` bundles, Photos libraries and drive system folders (`.Trashes`, `.Spotlight-V100`, `.fseventsd`, `$RECYCLE.BIN`). Each folder is removed with a call that fails if it is not empty.

## Safety

- Nothing is deleted without typing `YES` or `DELETE` exactly.
- `clean` only moves a file whose byte-identical twin stays in a keep folder, so one copy always remains.
- `.git`, `.app` and `.photoslibrary` contents are never moved or deleted.
- CSV and log files are never overwritten.

## Good to know

- **GB in Terminal vs Disk Utility:** `df -h` counts 1 GB as 1,073,741,824 bytes; Disk Utility counts 1,000,000,000. Use `df -H` to match Disk Utility.
- **`._` files:** macOS creates them on exFAT drives. They are skipped and are not real duplicates.
- **Photos library:** clean duplicates inside it with the Photos app, not dupfind.
- **Whole Mac:** `sudo dupfind scan / --min-size 1M`. `sudo` only lets it read protected folders.
- **Backups:** after cleanup one copy remains. Keep important files on a second drive or in the cloud too.

## How it works

Duplicates are found in three stages, each only looking at files that survived the last:

1. Parallel directory walk, files grouped by size (no file reads).
2. Hash of the first and last 64 KB of same-size files.
3. Full hash of files still matching.

Hashing uses xxh3-128 on all CPU cores. Hard links count as one file.

| File | Contents |
| --- | --- |
| `src/main.rs` | Command-line interface |
| `src/scan.rs` | Finding duplicates, reports, CSV |
| `src/clean.rs` | `clean` and `purge` |
| `src/empty.rs` | `empty-dirs` |
| `src/util.rs` | Shared helpers |

## Quick reference

| I want to | Command |
| --- | --- |
| See which folders share copies | `dupfind scan <folder> --folders` |
| List every duplicate | `dupfind scan <folder>` |
| Save to CSV | `dupfind scan <folder> --csv <new-file>.csv` |
| Preview a cleanup | `dupfind clean <folder> --keep "<keep>" --dry-run` |
| Run the cleanup | same without `--dry-run`, type `YES` |
| Free the space | `dupfind purge <folder>/_TO_DELETE`, type `DELETE` |
| Remove empty folders | `dupfind empty-dirs <folder>`, type `YES` |
