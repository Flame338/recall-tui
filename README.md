# Recall

Recall is a small, local-first project-memory TUI built to dogfood [LuxCore](../LuxCore). It indexes a repository’s readable source and documentation files through LuxCore’s C API, then lets you search while you type.

## Build and run

Build LuxCore’s shared C library first:

```powershell
cd F:\LuxCore
make shared MODE=release
```

Then build and launch Recall:

```powershell
cd F:\LuxCoreTUI
$env:LUXCORE_LIB = 'F:\LuxCore\build\release\libluxcore.dll'
cargo run --release -- 'F:\LuxCore'
```

The app defaults to `F:\LuxCore\build\debug\libluxcore.dll`. Set `LUXCORE_LIB` or pass `--lib PATH` to choose another shared library. If no repository is passed, Recall indexes the current directory.

## Using Recall

Type words, a phrase, or a code symbol. Results update as you type. Use the arrows or mouse wheel to move through results; the preview shows the closest lines and marks query terms. Press Enter to open the selected file in its default application.

- `Esc`: clear the query
- `Ctrl-W`: delete the last query word
- `Ctrl-U`: clear the whole query
- `Page Up` / `Page Down`: move through results faster
- `F1`: show key help
- `Ctrl-Q`: quit

Recall currently uses LuxCore’s lexical BM25 search and typo tolerance. It does not embed text or perform vector/semantic retrieval, so it works best when the query shares words with the relevant source. The index is rebuilt at startup; the C API does not expose LuxCore persistence yet.

The scanner indexes common source and text formats, including C/C++, Rust, Python, JavaScript/TypeScript, Markdown, JSON, TOML, YAML, HTML, and SQL. It skips `.git`, build output, package directories, and files over 512 KiB. Binary or non-UTF-8 files are skipped.
