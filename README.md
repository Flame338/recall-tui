# Recall

Recall is a small, local-first project-memory TUI built to dogfood [LuxCore](../LuxCore). It indexes a repository’s readable source and documentation files through LuxCore’s C API, then lets you search while you type.

## Build and run

Build LuxCore’s shared C library first:

```powershell
cd F:\LuxCore
make shared MODE=release
```

Then launch Recall:

```powershell
cd F:\LuxCoreTUI
$env:LUXCORE_LIB = 'F:\LuxCore\build\release\libluxcore.dll'
cargo run --release
```

Without a repository argument, Recall opens a folder picker at the current directory. Use the arrows to choose a folder and Enter to browse it; choose `..` or press Backspace to go up. At a Windows drive root, other available drives are listed. Press `S` to index the folder currently shown, or `P` to type or edit a path directly (`Ctrl-A` clears it). `Esc` or `Q` goes back, which quits when no project is open yet, and `Ctrl-Q` always quits. You can also pass a repository path to skip the picker:

```powershell
cargo run --release -- 'F:\path\to\project'
```

The app defaults to `F:\LuxCore\build\debug\libluxcore.dll`. Set `LUXCORE_LIB` or pass `--lib PATH` to choose another shared library.

## Using Recall

Type words, a phrase, or a code symbol. Results update as you type. Use the arrows or mouse wheel to move through results; the preview shows the closest lines and marks query terms. Press Enter to open the selected file in its default application.

- `Esc`: clear the query
- `Ctrl-W`: delete the last query word
- `Ctrl-U`: clear the whole query
- `Page Up` / `Page Down`: move through results faster
- `Ctrl-R`: rescan the project now
- `Ctrl-O`: switch to another project; the folder picker opens in the parent of the current project, and `Esc` there returns to your search untouched
- `F1`: show key help
- `F2`: manage saved indexes
- `Ctrl-Q`: quit

Recall currently uses LuxCore’s lexical BM25 search and typo tolerance. It does not embed text or perform vector/semantic retrieval, so it works best when the query shares words with the relevant source. Recall saves the index and a file manifest under `%LOCALAPPDATA%\Recall` (or `~/.cache/recall`), one pair per folder: a `.lux` index and a `.manifest` that records the folder path and a stamp for each file. At the next launch it loads the saved index and catches up on what changed (see Live updates). A different LuxCore library file, a change to the scan rules, or an index saved by an older version of Recall rebuilds the index from scratch.

## Live updates

Recall keeps the index current while it runs. On Windows it watches the project folder with `ReadDirectoryChangesW` and re-reads only the files that changed, so a file you save becomes searchable shortly after. The status bar shows `● live` when changes are flowing, `● live, updating N` while a burst such as a `git checkout` is absorbed, and `○ manual (Ctrl-R)` where watching is not possible (non-Windows systems, network paths). `Ctrl-R` rescans the project in any mode, which also picks up changes the watcher cannot see, such as a `.gitignore` above the project folder, `.git/info/exclude`, or your global git excludes. If the project folder itself is renamed away or replaced, the status bar says `○ project gone`; reopen the project. If the daemon stops while a window is open (a crash, or `recall daemon stop`), that window shows `○ manual (Ctrl-R)`; `Ctrl-R` starts the daemon again and reconnects.

The watcher runs in a small background daemon, started the first time you open a project. It keeps watching after you quit, so the next launch only catches up on what changed meanwhile, and several terminals share one watcher. The daemon is not started at logon, so after a reboot the first open of each project scans once.

- `recall daemon status`: list the projects the daemon watches
- `recall daemon stop`: stop the daemon
- `RECALL_DAEMON=0`: skip the daemon and watch inside the Recall process itself, for debugging

The index is saved when you quit, and in the background after a quiet stretch with enough changes. A project folder named `daemon` needs a path such as `.\daemon` to open.

Press `F2` on the folder picker or the search screen to manage saved indexes. The list shows each folder with its file count, size, and age, and marks the folder you have open with `●`. Indexes saved by an older version show as `(unknown folder)` until they are rebuilt.

- `↑` / `↓`, `Page Up` / `Page Down`, `Home` / `End`: move through the list
- `D` or `Delete`: delete the selected index, then `Y` or `Enter` to confirm, `N` or `Esc` to keep it
- `A`: delete every saved index, with the same confirmation
- `Esc`, `Q` or `F2`: close the list

A deleted index is rebuilt the next time you index that folder. Deleting the folder you have open does not affect the running search.

While scanning, Recall counts the files it finds. During indexing it shows a progress bar, the completed file count, and the file currently being read. Press `Esc`, `Ctrl-C` or `Ctrl-Q` in either phase to cancel. If you were switching from another project, Recall returns to that search with its query and results as you left them. Otherwise it quits. The scanner indexes common source and text formats, including C/C++, Rust, Python, JavaScript/TypeScript, Markdown, JSON, TOML, YAML, HTML, and SQL. It skips hidden folders (such as `.git` and `.claude`), anything a `.gitignore` excludes (nested files and parent folders count, and it works outside git repositories too), build output, package directories, and files over 512 KiB. Binary or non-UTF-8 files are skipped.
