use std::{
    env,
    error::Error,
    ffi::{c_char, c_void},
    fs,
    io::{self, stdout},
    path::{Path, PathBuf},
    process::Command,
    ptr,
};

use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
        MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use libloading::Library;
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};
use walkdir::WalkDir;

const INDEX_EXTENSIONS: &[&str] = &[
    "c", "cc", "cpp", "cxx", "h", "hh", "hpp", "hxx", "rs", "md", "txt", "rst", "py", "go", "java",
    "js", "jsx", "ts", "tsx", "json", "toml", "yaml", "yml", "ini", "sh", "bat", "ps1", "html",
    "css", "sql", "cmake",
];
const MAX_FILE_BYTES: u64 = 512 * 1024;
const MAX_RESULTS: usize = 40;

// These signatures mirror the stable, flat C API in luxcore_c.h.
type IndexCreate = unsafe extern "C" fn(*mut *mut c_void) -> i32;
type IndexFree = unsafe extern "C" fn(*mut c_void);
type IndexAdd = unsafe extern "C" fn(
    *mut c_void,
    *const c_char,
    usize,
    *const c_char,
    usize,
    *const c_char,
    usize,
) -> i32;
type IndexSearch =
    unsafe extern "C" fn(*const c_void, *const c_char, usize, usize, *mut *mut c_void) -> i32;
type IndexStored = unsafe extern "C" fn(
    *const c_void,
    *const c_char,
    usize,
    *const c_char,
    usize,
    *mut *mut c_void,
) -> i32;
type HitsCount = unsafe extern "C" fn(*const c_void) -> usize;
type HitsKey = unsafe extern "C" fn(*const c_void, usize, *mut usize) -> *const c_char;
type HitsScore = unsafe extern "C" fn(*const c_void, usize) -> f64;
type HitsFree = unsafe extern "C" fn(*mut c_void);
type TextData = unsafe extern "C" fn(*const c_void) -> *const c_char;
type TextLength = unsafe extern "C" fn(*const c_void) -> usize;
type TextFree = unsafe extern "C" fn(*mut c_void);

struct LuxFns {
    create: IndexCreate,
    free: IndexFree,
    add: IndexAdd,
    search: IndexSearch,
    stored: IndexStored,
    hits_count: HitsCount,
    hits_key: HitsKey,
    hits_score: HitsScore,
    hits_free: HitsFree,
    text_data: TextData,
    text_length: TextLength,
    text_free: TextFree,
}

struct LuxIndex {
    raw: *mut c_void,
    _library: Library,
    fns: LuxFns,
}

impl LuxIndex {
    fn load(path: &Path) -> Result<Self, Box<dyn Error>> {
        // The library is retained for the lifetime of every function pointer and index handle.
        let library = unsafe { Library::new(path)? };
        let fns = unsafe {
            LuxFns {
                create: *library.get(b"lux_index_create\0")?,
                free: *library.get(b"lux_index_free\0")?,
                add: *library.get(b"lux_index_add\0")?,
                search: *library.get(b"lux_index_search\0")?,
                stored: *library.get(b"lux_index_stored\0")?,
                hits_count: *library.get(b"lux_hits_count\0")?,
                hits_key: *library.get(b"lux_hits_key\0")?,
                hits_score: *library.get(b"lux_hits_score\0")?,
                hits_free: *library.get(b"lux_hits_free\0")?,
                text_data: *library.get(b"lux_text_data\0")?,
                text_length: *library.get(b"lux_text_length\0")?,
                text_free: *library.get(b"lux_text_free\0")?,
            }
        };
        let mut raw = ptr::null_mut();
        let status = unsafe { (fns.create)(&mut raw) };
        check_status(status, "creating the LuxCore index")?;
        if raw.is_null() {
            return Err("LuxCore returned a null index".into());
        }
        Ok(Self {
            raw,
            _library: library,
            fns,
        })
    }

    fn add(&mut self, key: &str, title: &str, body: &str) -> Result<(), Box<dyn Error>> {
        let status = unsafe {
            (self.fns.add)(
                self.raw,
                key.as_ptr().cast(),
                key.len(),
                title.as_ptr().cast(),
                title.len(),
                body.as_ptr().cast(),
                body.len(),
            )
        };
        check_status(status, "adding a file")
    }

    fn search(&self, query: &str, limit: usize) -> Result<Vec<Hit>, Box<dyn Error>> {
        let mut hits = ptr::null_mut();
        let status = unsafe {
            (self.fns.search)(
                self.raw,
                query.as_ptr().cast(),
                query.len(),
                limit,
                &mut hits,
            )
        };
        check_status(status, "searching")?;
        if hits.is_null() {
            return Err("LuxCore returned a null result handle".into());
        }
        let count = unsafe { (self.fns.hits_count)(hits) };
        let mut result = Vec::with_capacity(count);
        for position in 0..count {
            let mut key_len = 0;
            let key = unsafe { (self.fns.hits_key)(hits, position, &mut key_len) };
            if key.is_null() {
                unsafe { (self.fns.hits_free)(hits) };
                return Err("LuxCore returned a null result key".into());
            }
            let key = unsafe { std::slice::from_raw_parts(key.cast::<u8>(), key_len) };
            let key = String::from_utf8_lossy(key).into_owned();
            let score = unsafe { (self.fns.hits_score)(hits, position) };
            result.push(Hit { key, score });
        }
        unsafe { (self.fns.hits_free)(hits) };
        Ok(result)
    }

    fn stored(&self, key: &str, field: &str) -> Result<String, Box<dyn Error>> {
        let mut text = ptr::null_mut();
        let status = unsafe {
            (self.fns.stored)(
                self.raw,
                key.as_ptr().cast(),
                key.len(),
                field.as_ptr().cast(),
                field.len(),
                &mut text,
            )
        };
        check_status(status, "reading a stored field")?;
        if text.is_null() {
            return Err("LuxCore returned a null text handle".into());
        }
        let length = unsafe { (self.fns.text_length)(text) };
        let data = unsafe { (self.fns.text_data)(text) };
        let value = if length == 0 {
            String::new()
        } else if data.is_null() {
            unsafe { (self.fns.text_free)(text) };
            return Err("LuxCore returned null text data".into());
        } else {
            let bytes = unsafe { std::slice::from_raw_parts(data.cast::<u8>(), length) };
            String::from_utf8_lossy(bytes).into_owned()
        };
        unsafe { (self.fns.text_free)(text) };
        Ok(value)
    }
}

impl Drop for LuxIndex {
    fn drop(&mut self) {
        if !self.raw.is_null() {
            unsafe { (self.fns.free)(self.raw) };
            self.raw = ptr::null_mut();
        }
    }
}

fn check_status(status: i32, action: &str) -> Result<(), Box<dyn Error>> {
    if status == 0 {
        Ok(())
    } else {
        Err(format!("LuxCore error {status} while {action}").into())
    }
}

struct Hit {
    key: String,
    score: f64,
}

struct App {
    query: String,
    hits: Vec<Hit>,
    selected: usize,
    total_files: usize,
    root: PathBuf,
    status: String,
    show_help: bool,
    preview_key: Option<String>,
    preview: String,
}

fn main() -> Result<(), Box<dyn Error>> {
    let (root, library_path) = parse_args()?;
    let root = root.canonicalize()?;
    if !root.is_dir() {
        return Err(format!("Not a directory: {}", root.display()).into());
    }
    if !library_path.is_file() {
        return Err(format!(
            "LuxCore shared library not found at {}. Build it with `make shared` in F:\\LuxCore or pass --lib PATH.",
            library_path.display()
        ).into());
    }

    let mut index = LuxIndex::load(&library_path)?;
    eprintln!("Indexing text files under {} ...", root.display());
    let total_files = index_repository(&mut index, &root)?;
    eprintln!("Indexed {total_files} files. Opening Recall.");

    let mut app = App {
        query: String::new(),
        hits: Vec::new(),
        selected: 0,
        total_files,
        root,
        status: "Search words or symbols · F1 help".into(),
        show_help: false,
        preview_key: None,
        preview: String::new(),
    };
    run_tui(&mut app, &index)?;
    Ok(())
}

fn parse_args() -> Result<(PathBuf, PathBuf), Box<dyn Error>> {
    let mut args = env::args_os().skip(1);
    let mut root = None;
    let mut library = env::var_os("LUXCORE_LIB").map(PathBuf::from);
    while let Some(arg) = args.next() {
        if arg == "--lib" {
            library = Some(PathBuf::from(args.next().ok_or("--lib needs a path")?));
        } else if arg == "--help" || arg == "-h" {
            println!(
                "Recall — local project memory powered by LuxCore\n\nUsage: recall [ROOT] [--lib PATH]\n\nThe library path can also come from LUXCORE_LIB."
            );
            std::process::exit(0);
        } else if root.is_none() {
            root = Some(PathBuf::from(arg));
        } else {
            return Err(
                "Only one repository root is supported; use --lib for the library path".into(),
            );
        }
    }
    let root = root.unwrap_or(env::current_dir()?);
    let library =
        library.unwrap_or_else(|| PathBuf::from(r"F:\LuxCore\build\debug\libluxcore.dll"));
    Ok((root, library))
}

fn index_repository(index: &mut LuxIndex, root: &Path) -> Result<usize, Box<dyn Error>> {
    let mut count = 0;
    for entry in WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            if !entry.file_type().is_dir() || entry.depth() == 0 {
                return true;
            }
            !matches!(
                entry.file_name().to_str(),
                Some(
                    ".git"
                        | "build"
                        | "target"
                        | "node_modules"
                        | "vendor"
                        | ".venv"
                        | "venv"
                        | "__pycache__"
                )
            )
        })
    {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                eprintln!("Skipping unreadable path: {error}");
                continue;
            }
        };
        if !entry.file_type().is_file() || !is_indexable(entry.path()) {
            continue;
        }
        if !matches!(entry.metadata(), Ok(metadata) if metadata.len() <= MAX_FILE_BYTES) {
            continue;
        }
        let body = match fs::read_to_string(entry.path()) {
            Ok(body) => body,
            Err(_) => continue,
        };
        let relative = entry.path().strip_prefix(root)?;
        let key = relative.to_string_lossy().replace('\\', "/");
        let title = entry.file_name().to_string_lossy();
        index.add(&key, &title, &body)?;
        count += 1;
    }
    Ok(count)
}

fn is_indexable(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    if matches!(
        name,
        "Makefile" | "GNUmakefile" | "Dockerfile" | "Justfile" | ".gitignore" | "CMakeLists.txt"
    ) {
        return true;
    }
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            INDEX_EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str())
        })
}

fn run_tui(app: &mut App, index: &LuxIndex) -> Result<(), Box<dyn Error>> {
    enable_raw_mode()?;
    let _cleanup = TerminalCleanup;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(out);
    let mut terminal = Terminal::new(backend)?;
    event_loop(&mut terminal, app, index)
}

struct TerminalCleanup;

impl Drop for TerminalCleanup {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(stdout(), LeaveAlternateScreen, DisableMouseCapture);
    }
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    index: &LuxIndex,
) -> Result<(), Box<dyn Error>> {
    loop {
        terminal.draw(|frame| draw(frame, app))?;
        match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && matches!(key.code, KeyCode::Char('q' | 'Q'))
                {
                    break;
                }
                if key.code == KeyCode::F(1) {
                    app.show_help = !app.show_help;
                    continue;
                }
                if app.show_help {
                    if matches!(key.code, KeyCode::Esc | KeyCode::Enter | KeyCode::Char('?')) {
                        app.show_help = false;
                    }
                    continue;
                }
                match key.code {
                    KeyCode::Esc => {
                        app.query.clear();
                        refresh_hits(app, index);
                        app.status = "Search cleared".into();
                    }
                    KeyCode::Backspace => {
                        app.query.pop();
                        refresh_hits(app, index);
                    }
                    KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        app.query.clear();
                        refresh_hits(app, index);
                    }
                    KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        remove_last_word(&mut app.query);
                        refresh_hits(app, index);
                    }
                    KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                        app.query.push(ch);
                        refresh_hits(app, index);
                    }
                    KeyCode::Up | KeyCode::PageUp => {
                        app.selected = app
                            .selected
                            .saturating_sub(if key.code == KeyCode::PageUp { 8 } else { 1 });
                    }
                    KeyCode::Down | KeyCode::PageDown => {
                        if !app.hits.is_empty() {
                            let step = if key.code == KeyCode::PageDown { 8 } else { 1 };
                            app.selected = (app.selected + step).min(app.hits.len() - 1);
                        }
                    }
                    KeyCode::Home => app.selected = 0,
                    KeyCode::End => app.selected = app.hits.len().saturating_sub(1),
                    KeyCode::Enter => open_selected(app),
                    _ => {}
                }
            }
            Event::Mouse(mouse) if !app.hits.is_empty() => match mouse.kind {
                MouseEventKind::ScrollUp => app.selected = app.selected.saturating_sub(3),
                MouseEventKind::ScrollDown => {
                    app.selected = (app.selected + 3).min(app.hits.len() - 1)
                }
                _ => {}
            },
            _ => {}
        }
        sync_preview(app, index);
    }
    Ok(())
}

fn remove_last_word(query: &mut String) {
    let end = query.trim_end().len();
    let start = query[..end]
        .rfind(char::is_whitespace)
        .map_or(0, |index| index + 1);
    query.truncate(start);
}

fn refresh_hits(app: &mut App, index: &LuxIndex) {
    if app.query.trim().is_empty() {
        app.hits.clear();
        app.selected = 0;
        app.preview_key = None;
        app.preview.clear();
        app.status = format!("Indexed {} files · type a query", app.total_files);
        return;
    }
    match index.search(&app.query, MAX_RESULTS) {
        Ok(hits) => {
            let found = hits.len();
            app.hits = hits;
            app.selected = app.selected.min(app.hits.len().saturating_sub(1));
            sync_preview(app, index);
            app.status = format!(
                "{found} results · {} indexed · Enter opens selected file",
                app.total_files
            );
        }
        Err(error) => app.status = error.to_string(),
    }
}

fn sync_preview(app: &mut App, index: &LuxIndex) {
    let selected_key = app.hits.get(app.selected).map(|hit| hit.key.clone());
    if selected_key == app.preview_key {
        return;
    }
    app.preview_key = selected_key.clone();
    app.preview = selected_key
        .and_then(|key| index.stored(&key, "body").ok())
        .unwrap_or_default();
}

fn open_selected(app: &mut App) {
    let Some(hit) = app.hits.get(app.selected) else {
        return;
    };
    let path = app.root.join(&hit.key);
    #[cfg(target_os = "windows")]
    let result = Command::new("explorer.exe").arg(&path).spawn();
    #[cfg(target_os = "macos")]
    let result = Command::new("open").arg(&path).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let result = Command::new("xdg-open").arg(&path).spawn();
    app.status = match result {
        Ok(_) => format!("Opened {}", hit.key),
        Err(error) => format!("Could not open file: {error}"),
    };
}

fn draw(frame: &mut Frame, app: &App) {
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(7),
            Constraint::Length(2),
        ])
        .split(frame.area());
    let query = if app.query.is_empty() {
        Line::from(Span::styled(
            "Search terms, a phrase, or a code symbol…",
            Style::default().fg(Color::DarkGray),
        ))
    } else {
        Line::from(vec![
            Span::styled(&app.query, Style::default().fg(Color::White)),
            Span::styled("▏", Style::default().fg(Color::Cyan)),
        ])
    };
    let search = Paragraph::new(query).block(
        Block::default()
            .title(format!(" ✦ RECALL · {} ", compact_path(&app.root, 48)))
            .borders(Borders::ALL),
    );
    frame.render_widget(search, sections[0]);

    let content = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(48), Constraint::Percentage(52)])
        .split(sections[1]);
    let mut items: Vec<ListItem> = app
        .hits
        .iter()
        .map(|hit| {
            let name = Path::new(&hit.key)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(&hit.key);
            ListItem::new(vec![
                Line::from(vec![
                    Span::styled(
                        name.to_owned(),
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw("  "),
                    Span::styled(
                        format!("{:.2}", hit.score),
                        Style::default().fg(Color::Green),
                    ),
                ]),
                Line::from(Span::styled(
                    hit.key.clone(),
                    Style::default().fg(Color::DarkGray),
                )),
            ])
        })
        .collect();
    if items.is_empty() {
        items.push(ListItem::new(vec![
            Line::from(Span::styled(
                if app.query.is_empty() {
                    "Your repo is indexed"
                } else {
                    "No matching files yet"
                },
                Style::default().fg(if app.query.is_empty() {
                    Color::Green
                } else {
                    Color::Yellow
                }),
            )),
            Line::from(if app.query.is_empty() {
                "Try: PhraseQuery, persistence, fuzzy search"
            } else {
                "Try a shorter phrase or a code symbol."
            }),
        ]));
    }
    let list = List::new(items)
        .block(
            Block::default()
                .title(format!(" Results ({}) ", app.hits.len()))
                .borders(Borders::ALL),
        )
        .highlight_style(
            Style::default()
                .bg(Color::Rgb(36, 53, 68))
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▌ ");
    let mut list_state = ListState::default();
    if !app.hits.is_empty() {
        list_state.select(Some(app.selected));
    }
    frame.render_stateful_widget(list, content[0], &mut list_state);

    let selected = app.hits.get(app.selected);
    let preview = selected
        .map(|_| preview_lines(&app.preview, &app.query))
        .unwrap_or_else(|| {
            vec![
                Line::from(Span::styled(
                    "Pick a result to peek inside.",
                    Style::default().fg(Color::DarkGray),
                )),
                Line::from("Matching passages glow; Enter opens the file."),
            ]
        });
    let title = selected
        .map(|hit| format!(" {} · {:.3} ", hit.key, hit.score))
        .unwrap_or_else(|| " Preview ".into());
    frame.render_widget(
        Paragraph::new(preview)
            .block(Block::default().title(title).borders(Borders::ALL))
            .wrap(Wrap { trim: true }),
        content[1],
    );

    let footer = if app.show_help {
        Paragraph::new("F1 close help · Ctrl-Q quit")
    } else {
        Paragraph::new(vec![
            Line::from(Span::styled(
                &app.status,
                Style::default().fg(Color::Yellow),
            )),
            Line::from(Span::styled(
                "↑/↓ move  PgUp/PgDn jump  Enter open  Esc clear  Ctrl-W erase word  F1 help  Ctrl-Q quit",
                Style::default().fg(Color::DarkGray),
            )),
        ])
    };
    frame.render_widget(footer, sections[2]);
    if app.show_help {
        let area = centered_rect(80, 62, frame.area());
        frame.render_widget(Clear, area);
        let help = Paragraph::new(vec![
            Line::from(Span::styled(
                "A tiny memory for your codebase",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from("Search by words, filenames, or exact code symbols."),
            Line::from("BM25 ranks results and tolerates typos."),
            Line::from("Readable text files are indexed again each launch."),
            Line::from(""),
            Line::from("Type       Search as you go"),
            Line::from("↑ / ↓      Move through results (mouse wheel works too)"),
            Line::from("Enter      Open selected file"),
            Line::from("Esc        Clear search"),
            Line::from("Ctrl-W     Delete last word    Ctrl-U clear all"),
            Line::from("F1         Close this panel    Ctrl-Q quit"),
        ])
        .wrap(Wrap { trim: true })
        .block(
            Block::default()
                .title(" How to use Recall ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan)),
        );
        frame.render_widget(help, area);
    }
}

fn preview_lines(body: &str, query: &str) -> Vec<Line<'static>> {
    let terms: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    let matching: Vec<(usize, &str)> = body
        .lines()
        .enumerate()
        .filter(|(_, line)| {
            terms
                .iter()
                .any(|term| !term.is_empty() && line.to_lowercase().contains(term))
        })
        .take(8)
        .collect();
    let chosen: Vec<(usize, &str)> = if matching.is_empty() {
        body.lines()
            .enumerate()
            .filter(|(_, line)| !line.trim().is_empty())
            .take(8)
            .collect()
    } else {
        matching
    };
    if chosen.is_empty() {
        return vec![Line::from("(No preview text)")];
    }
    chosen
        .into_iter()
        .map(|(number, line)| {
            let excerpt = truncate_utf8(line, 400);
            let mut spans = vec![Span::styled(
                format!("{:>4} │ ", number + 1),
                Style::default().fg(Color::DarkGray),
            )];
            spans.extend(highlight_line(excerpt, &terms));
            Line::from(spans)
        })
        .collect()
}

fn highlight_line(line: &str, terms: &[String]) -> Vec<Span<'static>> {
    let folded = line.to_lowercase();
    if folded.len() != line.len() {
        return vec![Span::raw(line.to_owned())];
    }
    let mut ranges: Vec<(usize, usize)> = terms
        .iter()
        .filter(|term| !term.is_empty())
        .filter_map(|term| folded.find(term).map(|start| (start, start + term.len())))
        .collect();
    ranges.sort_unstable();
    let mut spans = Vec::new();
    let mut cursor = 0;
    for (start, end) in ranges {
        if start < cursor || !line.is_char_boundary(start) || !line.is_char_boundary(end) {
            continue;
        }
        if cursor < start {
            spans.push(Span::raw(line[cursor..start].to_owned()));
        }
        spans.push(Span::styled(
            line[start..end].to_owned(),
            Style::default()
                .fg(Color::Cyan)
                .bg(Color::Rgb(36, 53, 68))
                .add_modifier(Modifier::BOLD),
        ));
        cursor = end;
    }
    if cursor < line.len() {
        spans.push(Span::raw(line[cursor..].to_owned()));
    }
    if spans.is_empty() {
        spans.push(Span::raw(line.to_owned()));
    }
    spans
}

fn truncate_utf8(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut boundary = max_bytes;
    while !text.is_char_boundary(boundary) {
        boundary -= 1;
    }
    &text[..boundary]
}

fn compact_path(path: &Path, max_chars: usize) -> String {
    let raw = path.to_string_lossy();
    let value = raw.strip_prefix(r"\\?\").unwrap_or(&raw);
    let chars: Vec<char> = value.chars().collect();
    if chars.len() <= max_chars {
        return value.to_owned();
    }
    let tail: String = chars[chars.len().saturating_sub(max_chars - 1)..]
        .iter()
        .collect();
    format!("…{tail}")
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - height) / 2),
            Constraint::Percentage(height),
            Constraint::Percentage((100 - height) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - width) / 2),
            Constraint::Percentage(width),
            Constraint::Percentage((100 - width) / 2),
        ])
        .split(vertical[1])[1]
}
