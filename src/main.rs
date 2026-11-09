use std::{
    collections::BTreeMap,
    env,
    error::Error,
    fs,
    io::{self, stdout},
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
        MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
mod cache;
mod lux;

use cache::{Scanned, Stamp, Summary};
use lux::{DEFAULT_LIBRARY, Hit, LuxIndex, MARK_END, MARK_START};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Gauge, List, ListItem, ListState, Paragraph, Wrap},
};
use walkdir::WalkDir;

const INDEX_EXTENSIONS: &[&str] = &[
    "c", "cc", "cpp", "cxx", "h", "hh", "hpp", "hxx", "rs", "md", "txt", "rst", "py", "go", "java",
    "js", "jsx", "ts", "tsx", "json", "toml", "yaml", "yml", "ini", "sh", "bat", "ps1", "html",
    "css", "sql", "cmake",
];
const MAX_FILE_BYTES: u64 = 512 * 1024;
const MAX_RESULTS: usize = 40;

struct App {
    query: String,
    hits: Vec<Hit>,
    selected: usize,
    total_files: usize,
    root: PathBuf,
    status: String,
    show_help: bool,
    preview_for: Option<(String, String)>,
    preview: Vec<Line<'static>>,
}

struct DirectoryPicker {
    current: PathBuf,
    entries: Vec<PickerEntry>,
    selected: usize,
    path_input: Option<String>,
    status: String,
}

enum PickerEntry {
    Parent(PathBuf),
    Folder(PathBuf),
    Drive(PathBuf),
}

fn main() -> Result<(), Box<dyn Error>> {
    let (root, library_path) = parse_args()?;
    if !library_path.is_file() {
        return Err(format!(
            "LuxCore shared library not found at {}. Build it with `make shared` in F:\\LuxCore or pass --lib PATH.",
            library_path.display()
        )
        .into());
    }
    let root = match root {
        Some(root) => root,
        None => {
            let Some(root) = choose_root(env::current_dir()?)? else {
                return Ok(());
            };
            root
        }
    };
    let root = root.canonicalize()?;
    if !root.is_dir() {
        return Err(format!("Not a directory: {}", root.display()).into());
    }

    let mut index = LuxIndex::load(&library_path)?;
    let Some(summary) = index_repository(&mut index, &root, &library_path)? else {
        return Ok(());
    };

    let mut app = App {
        query: String::new(),
        hits: Vec::new(),
        selected: 0,
        total_files: summary.indexed,
        root,
        status: format!("{} · F1 help", describe_startup(&summary)),
        show_help: false,
        preview_for: None,
        preview: Vec::new(),
    };
    run_tui(&mut app, &index)?;
    Ok(())
}

fn parse_args() -> Result<(Option<PathBuf>, PathBuf), Box<dyn Error>> {
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
    let library = library.unwrap_or_else(|| PathBuf::from(DEFAULT_LIBRARY));
    Ok((root, library))
}

fn choose_root(start: PathBuf) -> Result<Option<PathBuf>, Box<dyn Error>> {
    let mut picker = DirectoryPicker {
        current: start,
        entries: Vec::new(),
        selected: 0,
        path_input: None,
        status: "Choose a folder to index".into(),
    };
    picker.refresh();

    enable_raw_mode()?;
    let _cleanup = TerminalCleanup;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(out);
    let mut terminal = Terminal::new(backend)?;

    loop {
        terminal.draw(|frame| draw_directory_picker(frame, &picker))?;
        match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && matches!(key.code, KeyCode::Char('q' | 'Q'))
                {
                    return Ok(None);
                }
                if let Some(path_input) = picker.path_input.as_mut() {
                    match key.code {
                        KeyCode::Esc => picker.path_input = None,
                        KeyCode::Enter => {
                            let path = PathBuf::from(path_input.trim());
                            match path.canonicalize() {
                                Ok(path) if path.is_dir() => {
                                    picker.current = path;
                                    picker.path_input = None;
                                    picker.refresh();
                                }
                                Ok(_) => picker.status = "That path is not a folder".into(),
                                Err(error) => {
                                    picker.status = format!("Cannot open that path: {error}")
                                }
                            }
                        }
                        KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            path_input.clear();
                        }
                        KeyCode::Backspace => {
                            path_input.pop();
                        }
                        KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                            path_input.push(ch);
                        }
                        _ => {}
                    }
                    continue;
                }
                match key.code {
                    KeyCode::Esc => return Ok(None),
                    KeyCode::Char('q' | 'Q') => return Ok(None),
                    KeyCode::Char('p' | 'P') => {
                        picker.path_input = Some(picker.current.to_string_lossy().into_owned());
                        picker.status = "Edit the folder path, then press Enter".into();
                    }
                    KeyCode::Char('s' | 'S' | ' ') => {
                        return Ok(Some(picker.current.clone()));
                    }
                    KeyCode::Backspace | KeyCode::Left => {
                        if let Some(parent) = picker.current.parent() {
                            picker.current = parent.to_path_buf();
                            picker.refresh();
                        }
                    }
                    KeyCode::Up => {
                        picker.selected = picker.selected.saturating_sub(1);
                    }
                    KeyCode::Down => {
                        picker.selected =
                            (picker.selected + 1).min(picker.entries.len().saturating_sub(1));
                    }
                    KeyCode::Enter => {
                        if let Some(entry) = picker.entries.get(picker.selected) {
                            picker.current = match entry {
                                PickerEntry::Parent(path)
                                | PickerEntry::Folder(path)
                                | PickerEntry::Drive(path) => path.clone(),
                            };
                            picker.refresh();
                        }
                    }
                    _ => {}
                }
            }
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::ScrollUp => picker.selected = picker.selected.saturating_sub(3),
                MouseEventKind::ScrollDown => {
                    picker.selected =
                        (picker.selected + 3).min(picker.entries.len().saturating_sub(1));
                }
                _ => {}
            },
            _ => {}
        }
    }
}

impl DirectoryPicker {
    fn refresh(&mut self) {
        self.entries.clear();
        self.selected = 0;
        match fs::read_dir(&self.current) {
            Ok(entries) => {
                let mut folders: Vec<PathBuf> = entries
                    .filter_map(Result::ok)
                    .filter_map(|entry| {
                        entry
                            .file_type()
                            .ok()
                            .filter(|kind| kind.is_dir())
                            .map(|_| entry.path())
                    })
                    .collect();
                folders.sort_by_key(|path| {
                    path.file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_lowercase()
                });
                if let Some(parent) = self
                    .current
                    .parent()
                    .filter(|parent| *parent != &self.current)
                {
                    self.entries.push(PickerEntry::Parent(parent.to_path_buf()));
                }
                self.entries
                    .extend(folders.into_iter().map(PickerEntry::Folder));
                #[cfg(windows)]
                if self.current.parent().is_none() {
                    for letter in b'A'..=b'Z' {
                        let drive = PathBuf::from(format!("{}:\\", letter as char));
                        if drive != self.current && drive.is_dir() {
                            self.entries.push(PickerEntry::Drive(drive));
                        }
                    }
                }
                if !self.entries.is_empty() {
                    self.selected =
                        usize::from(matches!(self.entries.first(), Some(PickerEntry::Parent(_))));
                }
                self.status = format!(
                    "{} locations · press S to index this folder",
                    self.entries.len()
                );
            }
            Err(error) => self.status = format!("Cannot read this folder: {error}"),
        }
    }
}

fn draw_directory_picker(frame: &mut Frame, picker: &DirectoryPicker) {
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(5),
            Constraint::Min(5),
            Constraint::Length(2),
        ])
        .split(frame.area());
    let path = picker
        .path_input
        .as_deref()
        .unwrap_or_else(|| picker.current.to_str().unwrap_or(""));
    let path_label = if picker.path_input.is_some() {
        format!(" Folder path (editing) · {path}▏ ")
    } else {
        format!(" Current folder · {path} ")
    };
    let header = Paragraph::new(vec![
        Line::from(Span::styled(
            "Choose a project folder",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(path_label, Style::default().fg(Color::White))),
    ])
    .block(
        Block::default()
            .title(" Recall · Project setup ")
            .borders(Borders::ALL),
    );
    frame.render_widget(header, sections[0]);

    let items: Vec<ListItem> = if picker.entries.is_empty() {
        vec![ListItem::new(Line::from(Span::styled(
            "No subfolders here",
            Style::default().fg(Color::DarkGray),
        )))]
    } else {
        picker
            .entries
            .iter()
            .map(|entry| {
                let label = match entry {
                    PickerEntry::Parent(_) => "..  (parent folder)".to_owned(),
                    PickerEntry::Folder(path) => path
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned(),
                    PickerEntry::Drive(path) => path.display().to_string(),
                };
                ListItem::new(format!("  {label}"))
            })
            .collect()
    };
    let list = List::new(items)
        .block(
            Block::default()
                .title(" Locations · Enter to browse ")
                .borders(Borders::ALL),
        )
        .highlight_style(
            Style::default()
                .bg(Color::Rgb(36, 53, 68))
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▌ ");
    let mut state = ListState::default();
    if !picker.entries.is_empty() {
        state.select(Some(picker.selected));
    }
    frame.render_stateful_widget(list, sections[1], &mut state);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                &picker.status,
                Style::default().fg(Color::Yellow),
            )),
            Line::from(Span::styled(
                "↑/↓ browse  Enter open  .. / Backspace up  S index here  P type path  Esc quit",
                Style::default().fg(Color::DarkGray),
            )),
        ]),
        sections[2],
    );
}

type RecallTerminal = Terminal<CrosstermBackend<io::Stdout>>;

fn describe_startup(summary: &Summary) -> String {
    let files = summary.indexed;
    if !summary.reused {
        let cache = if summary.saved {
            "cached for next time"
        } else {
            "cache unavailable"
        };
        format!("Indexed {files} files · {cache}")
    } else if summary.updated == 0 && summary.removed == 0 {
        format!("Loaded {files} files from cache")
    } else {
        format!(
            "Cache refreshed: {} changed, {} removed · {files} files",
            summary.updated, summary.removed
        )
    }
}

fn index_repository(
    index: &mut LuxIndex,
    root: &Path,
    library: &Path,
) -> Result<Option<Summary>, Box<dyn Error>> {
    enable_raw_mode()?;
    let _cleanup = TerminalCleanup;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(out);
    let mut terminal = Terminal::new(backend)?;

    let Some(scanned) = scan_files(&mut terminal, root)? else {
        return Ok(None);
    };
    let files = cache::default_dir().map(|dir| cache::cache_files(&dir, root));
    let fingerprint = cache::fingerprint(library);
    let mut last_draw = Instant::now() - Duration::from_secs(1);
    cache::sync(
        index,
        files.as_ref(),
        &fingerprint,
        &scanned,
        |completed, total, current| {
            if last_draw.elapsed() < Duration::from_millis(60) {
                return Ok(true);
            }
            if cancel_requested()? {
                return Ok(false);
            }
            terminal.draw(|frame| draw_indexing(frame, root, completed, total, current))?;
            last_draw = Instant::now();
            Ok(true)
        },
    )
}

fn scan_files(
    terminal: &mut RecallTerminal,
    root: &Path,
) -> Result<Option<BTreeMap<String, Scanned>>, Box<dyn Error>> {
    let mut files = BTreeMap::new();
    let mut latest = String::new();
    let mut last_draw = Instant::now() - Duration::from_secs(1);
    let walker = WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            if !entry.file_type().is_dir() || entry.depth() == 0 {
                return true;
            }
            entry.file_name().to_str().is_some_and(should_descend)
        });
    for entry in walker.filter_map(Result::ok) {
        if entry.file_type().is_file()
            && is_indexable(entry.path())
            && let Ok(metadata) = entry.metadata()
            && metadata.len() <= MAX_FILE_BYTES
            && let Ok(relative) = entry.path().strip_prefix(root)
        {
            latest = relative.to_string_lossy().replace('\\', "/");
            files.insert(
                latest.clone(),
                Scanned {
                    path: entry.into_path(),
                    stamp: Stamp::of(&metadata),
                },
            );
        }
        if last_draw.elapsed() >= Duration::from_millis(60) {
            if cancel_requested()? {
                return Ok(None);
            }
            terminal.draw(|frame| draw_scanning(frame, root, files.len(), &latest))?;
            last_draw = Instant::now();
        }
    }
    Ok(Some(files))
}

fn cancel_requested() -> Result<bool, Box<dyn Error>> {
    let mut cancel = false;
    while event::poll(Duration::ZERO)? {
        if let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            cancel |= key.code == KeyCode::Esc
                || ctrl && matches!(key.code, KeyCode::Char('q' | 'Q' | 'c' | 'C'));
        }
    }
    Ok(cancel)
}

fn should_descend(dir_name: &str) -> bool {
    !dir_name.starts_with('.')
        && !matches!(
            dir_name,
            "build" | "target" | "node_modules" | "vendor" | "venv" | "__pycache__"
        )
}

fn indexing_layout(frame: &mut Frame, root: &Path) -> [Rect; 3] {
    let area = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(5),
            Constraint::Min(3),
        ])
        .split(frame.area());
    let title = Paragraph::new(format!("Preparing search for {}", compact_path(root, 64))).block(
        Block::default()
            .title(" Recall · Indexing ")
            .borders(Borders::ALL),
    );
    frame.render_widget(title, area[0]);
    [area[0], area[1], area[2]]
}

fn draw_detail(frame: &mut Frame, area: Rect, detail: &str) {
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(sanitize_for_display(detail)),
            Line::from(""),
            Line::from("Esc cancels"),
        ])
        .style(Style::default().fg(Color::DarkGray))
        .wrap(Wrap { trim: true }),
        area,
    );
}

fn draw_scanning(frame: &mut Frame, root: &Path, found: usize, latest: &str) {
    let area = indexing_layout(frame, root);
    frame.render_widget(
        Paragraph::new(format!("{found} files found so far")).block(
            Block::default()
                .title(" Scanning folders ")
                .borders(Borders::ALL),
        ),
        area[1],
    );
    let detail = if latest.is_empty() {
        "Looking for files…".to_owned()
    } else {
        format!("Found {latest}")
    };
    draw_detail(frame, area[2], &detail);
}

fn draw_indexing(frame: &mut Frame, root: &Path, completed: usize, total: usize, current: &str) {
    let area = indexing_layout(frame, root);
    let ratio = if total == 0 {
        1.0
    } else {
        (completed as f64 / total as f64).clamp(0.0, 1.0)
    };
    let gauge = Gauge::default()
        .block(Block::default().title(" Progress ").borders(Borders::ALL))
        .gauge_style(Style::default().fg(Color::Cyan).bg(Color::DarkGray))
        .ratio(ratio)
        .label(if total == 0 {
            "No matching files".to_owned()
        } else {
            format!("{:.0}% · {completed} / {total} files", ratio * 100.0)
        });
    frame.render_widget(gauge, area[1]);

    let detail = if current.is_empty() {
        "Starting index…".to_owned()
    } else {
        format!("Indexing {current}")
    };
    draw_detail(frame, area[2], &detail);
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
    terminal: &mut RecallTerminal,
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
        app.preview_for = None;
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
    let wanted = app
        .hits
        .get(app.selected)
        .map(|hit| (hit.key.clone(), app.query.clone()));
    if wanted == app.preview_for {
        return;
    }
    app.preview = match &wanted {
        Some((key, query)) => build_preview(index, key, query),
        None => Vec::new(),
    };
    app.preview_for = wanted;
}

fn build_preview(index: &LuxIndex, key: &str, query: &str) -> Vec<Line<'static>> {
    let body = index.stored(key, "body").unwrap_or_default();
    let marked = index
        .snippet(key, "body", query, body.len().max(1))
        .unwrap_or(body);
    preview_lines(&marked)
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
            Span::styled(
                sanitize_for_display(&app.query),
                Style::default().fg(Color::White),
            ),
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
            let name = sanitize_for_display(
                Path::new(&hit.key)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or(&hit.key),
            );
            ListItem::new(vec![
                Line::from(vec![
                    Span::styled(
                        name,
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
                    sanitize_for_display(&hit.key),
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
    let preview = selected.map(|_| app.preview.clone()).unwrap_or_else(|| {
        vec![
            Line::from(Span::styled(
                "Pick a result to peek inside.",
                Style::default().fg(Color::DarkGray),
            )),
            Line::from("Matching passages glow; Enter opens the file."),
        ]
    });
    let title = selected
        .map(|hit| format!(" {} · {:.3} ", sanitize_for_display(&hit.key), hit.score))
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
            Line::from("The index is cached; only changed files are re-read."),
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

fn preview_lines(marked: &str) -> Vec<Line<'static>> {
    let numbered = marked.lines().enumerate();
    let matching: Vec<(usize, &str)> = numbered
        .clone()
        .filter(|(_, line)| line.contains(MARK_START))
        .take(8)
        .collect();
    let chosen: Vec<(usize, &str)> = if matching.is_empty() {
        numbered
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
            let line = sanitize_for_display(line);
            let excerpt = truncate_utf8(&line, 400);
            let mut spans = vec![Span::styled(
                format!("{:>4} │ ", number + 1),
                Style::default().fg(Color::DarkGray),
            )];
            spans.extend(marked_spans(excerpt));
            Line::from(spans)
        })
        .collect()
}

fn marked_spans(line: &str) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut text = String::new();
    let mut marked = false;
    for ch in line.chars() {
        if ch == MARK_START || ch == MARK_END {
            if !text.is_empty() {
                spans.push(match_span(std::mem::take(&mut text), marked));
            }
            marked = ch == MARK_START;
        } else {
            text.push(ch);
        }
    }
    if !text.is_empty() {
        spans.push(match_span(text, marked));
    }
    spans
}

fn match_span(text: String, marked: bool) -> Span<'static> {
    if marked {
        Span::styled(
            text,
            Style::default()
                .fg(Color::Cyan)
                .bg(Color::Rgb(36, 53, 68))
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::raw(text)
    }
}

const TAB_WIDTH: usize = 4;

fn sanitize_for_display(text: &str) -> String {
    let mut clean = String::with_capacity(text.len());
    let mut column = 0;
    for ch in text.chars() {
        match ch {
            '\t' => {
                let pad = TAB_WIDTH - column % TAB_WIDTH;
                clean.extend(std::iter::repeat_n(' ', pad));
                column += pad;
            }
            MARK_START | MARK_END => clean.push(ch),
            ch if ch.is_control() || is_invisible_format(ch) => {}
            ch => {
                clean.push(ch);
                column += 1;
            }
        }
    }
    clean
}

fn is_invisible_format(ch: char) -> bool {
    matches!(
        ch,
        '\u{200B}'..='\u{200F}'
            | '\u{2028}'..='\u{202E}'
            | '\u{2060}'..='\u{206F}'
            | '\u{FE00}'..='\u{FE0F}'
            | '\u{FEFF}'
    )
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

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{buffer::Buffer, widgets::Widget};

    fn rendered_symbols(text: &str) -> Vec<String> {
        let area = Rect::new(0, 0, 40, 6);
        let mut buffer = Buffer::empty(area);
        Paragraph::new(preview_lines(text))
            .wrap(Wrap { trim: true })
            .render(area, &mut buffer);
        buffer
            .content
            .iter()
            .map(|cell| cell.symbol().to_owned())
            .collect()
    }

    #[test]
    fn hidden_and_build_directories_are_not_indexed() {
        for skipped in [
            ".git",
            ".claude",
            ".venv",
            "target",
            "node_modules",
            "build",
        ] {
            assert!(!should_descend(skipped), "{skipped} should be skipped");
        }
        for kept in ["src", "docs", "tests"] {
            assert!(should_descend(kept), "{kept} should be indexed");
        }
    }

    fn marked(text: &str) -> String {
        text.replace('[', &MARK_START.to_string())
            .replace(']', &MARK_END.to_string())
    }

    fn texts(line: &Line) -> Vec<(String, bool)> {
        line.spans
            .iter()
            .skip(1)
            .map(|span| (span.content.to_string(), span.style.bg.is_some()))
            .collect()
    }

    #[test]
    fn marker_characters_become_highlighted_spans() {
        let spans = marked_spans(&marked("a[lo]b"));
        let seen: Vec<(String, bool)> = spans
            .iter()
            .map(|span| (span.content.to_string(), span.style.bg.is_some()))
            .collect();
        assert_eq!(
            seen,
            [
                ("a".into(), false),
                ("lo".into(), true),
                ("b".into(), false)
            ]
        );
    }

    #[test]
    fn preview_shows_matching_lines_with_their_numbers_and_falls_back_to_the_top() {
        let hit = preview_lines(&marked("one\ntwo [lo]\nthree\nfour [lo]"));
        assert_eq!(hit.len(), 2);
        assert_eq!(hit[0].spans[0].content.trim(), "2 │");
        assert_eq!(hit[1].spans[0].content.trim(), "4 │");
        let none = preview_lines("\n\nalpha\nbeta");
        assert_eq!(none.len(), 2);
        assert_eq!(none[0].spans[0].content.trim(), "3 │");
    }

    #[test]
    fn luxcore_snippets_mark_prefix_completions_and_keep_line_numbers() {
        let mut index =
            LuxIndex::load(&lux::library_path()).expect("build libluxcore with `make shared`");
        index
            .add(
                "note.md",
                "note.md",
                "first line\n\twhile we are testing grounds\nlast line\n",
            )
            .unwrap();
        let lines = build_preview(&index, "note.md", "testin");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].spans[0].content.trim(), "2 │");
        let seen = texts(&lines[0]);
        assert!(
            seen.iter().any(|(text, hot)| *hot && text == "testing"),
            "expected `testing` highlighted, got {seen:?}"
        );
    }

    #[test]
    fn tabs_expand_to_the_next_stop_and_invisibles_vanish() {
        assert_eq!(sanitize_for_display("\tx"), "    x");
        assert_eq!(sanitize_for_display("ab\tx"), "ab  x");
        assert_eq!(sanitize_for_display("\u{FEFF}a\u{FE0F}\u{200D}b\x1b"), "ab");
        assert_eq!(sanitize_for_display("naïve → ✓"), "naïve → ✓");
    }

    #[test]
    fn preview_never_writes_control_characters_to_cells() {
        let body = "\twhile (lo < hi) {\n\t\tlo = 1;\x1b[31m\x08\x07\u{7f}\n\u{85}x lo\r\n";
        for symbol in rendered_symbols(body) {
            assert!(
                !symbol.chars().any(char::is_control),
                "control character reached the terminal buffer: {symbol:?}"
            );
        }
    }
}
