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

Without a repository argument, Recall opens a folder picker at the current directory. Use the arrows to choose a folder and Enter to browse it; choose `..` or press Backspace to go up. At a Windows drive root, other available drives are listed. Press `S` to index the folder currently shown, or `P` to type or edit a path directly (`Ctrl-A` clears it). You can also pass a repository path to skip the picker:

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
- `F1`: show key help
- `Ctrl-Q`: quit

Recall currently uses LuxCore’s lexical BM25 search and typo tolerance. It does not embed text or perform vector/semantic retrieval, so it works best when the query shares words with the relevant source. Recall saves the index and a file manifest under `%LOCALAPPDATA%Recall` (or `~/.cache/recall`), one pair per folder. At the next launch it loads the saved index and re-reads only the files whose size or modification time changed, and drops deleted ones. A different LuxCore library file or a change to the scan rules rebuilds the index from scratch. Delete that folder to force a full rebuild.

While scanning, Recall counts the files it finds. During indexing it shows a progress bar, the completed file count, and the file currently being read. Press `Esc` or `Ctrl-Q` in either phase to cancel and quit. The scanner indexes common source and text formats, including C/C++, Rust, Python, JavaScript/TypeScript, Markdown, JSON, TOML, YAML, HTML, and SQL. It skips hidden folders (such as `.git` and `.claude`), build output, package directories, and files over 512 KiB. Binary or non-UTF-8 files are skipped.
