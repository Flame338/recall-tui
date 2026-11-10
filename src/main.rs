use std::{
    env,
    error::Error,
    fs,
    io::{self, stdout},
    path::{Path, PathBuf},
    process::Command,
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
mod daemon;
mod flow;
mod ipc;
mod live;
mod lux;
mod manager;
mod opening;
mod scan;
mod truth;
mod watch;

use flow::Screen;
use live::Live;
use lux::{DEFAULT_LIBRARY, Hit, LuxIndex, MARK_END, MARK_START};
use manager::{IndexManager, Outcome};
use opening::open_project;
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};

const MAX_RESULTS: usize = 40;

struct App {
    query: String,
    hits: Vec<Hit>,
    selected: usize,
    total_files: usize,
    root: PathBuf,
    status: String,
    show_help: bool,
    manager: Option<IndexManager>,
    preview_for: Option<(String, String)>,
    preview: Vec<Line<'static>>,
    live: String,
}

impl App {
    fn new(root: PathBuf, total_files: usize, status: String) -> Self {
        Self {
            query: String::new(),
            hits: Vec::new(),
            selected: 0,
            total_files,
            root,
            status,
            show_help: false,
            manager: None,
            preview_for: None,
            preview: Vec::new(),
            live: String::new(),
        }
    }
}

struct Session {
    live: Live,
    app: App,
}

struct DirectoryPicker {
    current: PathBuf,
    entries: Vec<PickerEntry>,
    selected: usize,
    path_input: Option<String>,
    status: String,
}

enum Exit {
    Quit,
    SwitchProject,
}

enum Choice {
    Folder(PathBuf),
    Back,
    Quit,
}

enum PickerEntry {
    Parent(PathBuf),
    Folder(PathBuf),
    Drive(PathBuf),
}

fn main() -> Result<(), Box<dyn Error>> {
    if env::args_os().nth(1).is_some_and(|arg| arg == "daemon") {
        return daemon_command(&env::args().skip(2).collect::<Vec<_>>());
    }
    let (root, library_path) = parse_args()?;
    if !library_path.is_file() {
        return Err(format!(
            "LuxCore shared library not found at {}. Build it with `make shared` in F:\\LuxCore or pass --lib PATH.",
            library_path.display()
        )
        .into());
    }
    let mut screen = match root {
        Some(root) => Screen::Open { root },
        None => Screen::Pick {
            start: env::current_dir()?,
        },
    };
    let mut session: Option<Session> = None;
    loop {
        let event = run_screen(&screen, &mut session, &library_path)?;
        match flow::next(&screen, event, session.is_some()) {
            Some(next) => screen = next,
            None => {
                session.into_iter().for_each(|ended| ended.live.close());
                return Ok(());
            }
        }
    }
}

fn daemon_command(args: &[String]) -> Result<(), Box<dyn Error>> {
    daemon::cli(args)
}

fn run_screen(
    screen: &Screen,
    session: &mut Option<Session>,
    library_path: &Path,
) -> Result<flow::Event, Box<dyn Error>> {
    Ok(match screen {
        Screen::Pick { start } => match choose_root(start.clone())? {
            Choice::Folder(root) => flow::Event::Picked(root),
            Choice::Back => flow::Event::Back,
            Choice::Quit => flow::Event::Quit,
        },
        Screen::Open { root } => match open_project(root.clone(), library_path) {
            Ok(Some(opened)) => {
                session
                    .replace(opened)
                    .into_iter()
                    .for_each(|old| old.live.close());
                flow::Event::Opened
            }
            Ok(None) => flow::Event::OpenCancelled,
            Err(error) => recover_open_failure(session, root, error)?,
        },
        Screen::Search => {
            let Some(current) = session.as_mut() else {
                return Ok(flow::Event::SearchQuit);
            };
            match run_tui(&mut current.app, &mut current.live)? {
                Exit::Quit => flow::Event::SearchQuit,
                Exit::SwitchProject => flow::Event::SearchSwitch {
                    start: flow::picker_start(&current.app.root),
                },
            }
        }
    })
}

fn recover_open_failure(
    session: &mut Option<Session>,
    root: &Path,
    error: Box<dyn Error>,
) -> Result<flow::Event, Box<dyn Error>> {
    let Some(previous) = session.as_mut() else {
        return Err(error);
    };
    previous.app.status = format!("Could not open {}: {error}", compact_path(root, 48));
    Ok(flow::Event::OpenCancelled)
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

fn choose_root(start: PathBuf) -> Result<Choice, Box<dyn Error>> {
    let mut picker = DirectoryPicker {
        current: start,
        entries: Vec::new(),
        selected: 0,
        path_input: None,
        status: "Choose a folder to index".into(),
    };
    picker.refresh();
    let mut manager: Option<IndexManager> = None;

    enable_raw_mode()?;
    let _cleanup = TerminalCleanup;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(out);
    let mut terminal = Terminal::new(backend)?;

    loop {
        terminal.draw(|frame| {
            draw_directory_picker(frame, &picker);
            draw_manager(frame, manager.as_ref());
        })?;
        match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && matches!(key.code, KeyCode::Char('q' | 'Q'))
                {
                    return Ok(Choice::Quit);
                }
                if let Some(open) = manager.as_mut() {
                    if open.handle_key(key) == Outcome::Close {
                        manager = None;
                    }
                    continue;
                }
                if key.code == KeyCode::F(2) {
                    manager = Some(IndexManager::open(cache::default_dir(), None));
                    continue;
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
                    KeyCode::Esc | KeyCode::Char('q' | 'Q') => return Ok(Choice::Back),
                    KeyCode::Char('p' | 'P') => {
                        picker.path_input = Some(picker.current.to_string_lossy().into_owned());
                        picker.status = "Edit the folder path, then press Enter".into();
                    }
                    KeyCode::Char('s' | 'S' | ' ') => {
                        return Ok(Choice::Folder(picker.current.clone()));
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
            Event::Mouse(mouse) if manager.is_none() => match mouse.kind {
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
        if let Some(parent) = self
            .current
            .parent()
            .filter(|parent| *parent != self.current)
        {
            self.entries.push(PickerEntry::Parent(parent.to_path_buf()));
        }
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
                self.status = format!(
                    "{} locations · press S to index this folder",
                    self.entries.len()
                );
            }
            Err(error) => self.status = format!("Cannot read this folder: {error}"),
        }
        let starts_with_parent = matches!(self.entries.first(), Some(PickerEntry::Parent(_)));
        self.selected = usize::from(starts_with_parent && self.entries.len() > 1);
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
                "↑/↓ browse  Enter open  .. / Backspace up  S index here  P type path  F2 indexes  Esc back",
                Style::default().fg(Color::DarkGray),
            )),
        ]),
        sections[2],
    );
}

type RecallTerminal = Terminal<CrosstermBackend<io::Stdout>>;

fn run_tui(app: &mut App, live: &mut Live) -> Result<Exit, Box<dyn Error>> {
    enable_raw_mode()?;
    let _cleanup = TerminalCleanup;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(out);
    let mut terminal = Terminal::new(backend)?;
    event_loop(&mut terminal, app, live)
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
    live: &mut Live,
) -> Result<Exit, Box<dyn Error>> {
    let mut redraw = true;
    loop {
        let label = live.status();
        if label != app.live {
            app.live = label;
            redraw = true;
        }
        if redraw {
            terminal.draw(|frame| draw(frame, app))?;
            redraw = false;
        }
        if !event::poll(live.poll_wait())? {
            let tick = live.tick();
            if !tick.changed.is_empty() {
                on_live_change(app, live, &tick.changed);
                redraw = true;
            }
            continue;
        }
        redraw = true;
        match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                if ctrl && matches!(key.code, KeyCode::Char('q' | 'Q')) {
                    return Ok(Exit::Quit);
                }
                if ctrl && matches!(key.code, KeyCode::Char('r' | 'R')) {
                    live.rescan();
                    app.status = if *live.mode() == live::Mode::Gone {
                        "The project folder is gone".into()
                    } else {
                        "Rescanning the project…".into()
                    };
                    continue;
                }
                if let Some(open) = app.manager.as_mut() {
                    if open.handle_key(key) == Outcome::Close {
                        app.manager = None;
                    }
                    continue;
                }
                if key.code == KeyCode::F(2) {
                    app.show_help = false;
                    app.manager = Some(IndexManager::open(
                        cache::default_dir(),
                        Some(app.root.clone()),
                    ));
                    continue;
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
                        refresh_hits(app, live.index());
                        app.status = "Search cleared".into();
                    }
                    KeyCode::Backspace => {
                        app.query.pop();
                        refresh_hits(app, live.index());
                    }
                    KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        app.query.clear();
                        refresh_hits(app, live.index());
                    }
                    KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        remove_last_word(&mut app.query);
                        refresh_hits(app, live.index());
                    }
                    KeyCode::Char('o' | 'O') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        return Ok(Exit::SwitchProject);
                    }
                    KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                        app.query.push(ch);
                        refresh_hits(app, live.index());
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
            Event::Mouse(_) if app.manager.is_some() => {}
            Event::Mouse(mouse) if !app.hits.is_empty() => match mouse.kind {
                MouseEventKind::ScrollUp => app.selected = app.selected.saturating_sub(3),
                MouseEventKind::ScrollDown => {
                    app.selected = (app.selected + 3).min(app.hits.len() - 1)
                }
                _ => {}
            },
            _ => {}
        }
        sync_preview(app, live.index());
    }
}

fn remove_last_word(query: &mut String) {
    let end = query.trim_end().len();
    let start = query[..end]
        .rfind(char::is_whitespace)
        .map_or(0, |index| index + 1);
    query.truncate(start);
}

fn on_live_change(app: &mut App, live: &Live, changed: &[String]) {
    app.total_files = live.indexed_files();
    let keep = app.hits.get(app.selected).map(|hit| hit.key.clone());
    if keep.as_ref().is_some_and(|key| changed.contains(key)) {
        app.preview_for = None;
    }
    run_query(app, live.index(), keep.as_deref());
}

fn refresh_hits(app: &mut App, index: &LuxIndex) {
    run_query(app, index, None);
}

fn run_query(app: &mut App, index: &LuxIndex, keep: Option<&str>) {
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
            let moved_to = keep.and_then(|key| app.hits.iter().position(|hit| hit.key == key));
            app.selected = moved_to
                .unwrap_or(app.selected)
                .min(app.hits.len().saturating_sub(1));
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
                format!("{} · {}", app.status, app.live),
                Style::default().fg(Color::Yellow),
            )),
            Line::from(Span::styled(
                "↑/↓ move  Enter open  Esc clear  Ctrl-O project  F2 indexes  F1 help  Ctrl-Q quit",
                Style::default().fg(Color::DarkGray),
            )),
        ])
    };
    frame.render_widget(footer, sections[2]);
    if app.show_help {
        let area = centered_rect(80, 80, frame.area());
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
            Line::from("BM25 ranks results, tolerates typos, and stays current as files change."),
            Line::from(""),
            Line::from("Type       Search as you go"),
            Line::from("↑ / ↓      Move through results (mouse wheel works too)"),
            Line::from("PgUp / PgDn  Jump through results faster"),
            Line::from("Enter      Open selected file"),
            Line::from("Esc        Clear search"),
            Line::from("Ctrl-W     Delete last word    Ctrl-U clear all"),
            Line::from("Ctrl-O     Switch to another project"),
            Line::from("Ctrl-R     Rescan the project now"),
            Line::from("F2         Saved indexes: see and delete them"),
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
    draw_manager(frame, app.manager.as_ref());
}

fn draw_manager(frame: &mut Frame, manager: Option<&IndexManager>) {
    if let Some(manager) = manager {
        manager.draw(frame, centered_rect(80, 70, frame.area()));
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

    fn session_for(root: &str) -> Session {
        let live = Live::detached(Path::new(root), &lux::library_path()).expect("build libluxcore");
        Session {
            live,
            app: App::new(PathBuf::from(root), 0, "ready".into()),
        }
    }

    #[test]
    fn a_failed_open_returns_to_the_previous_search_and_says_why() {
        let mut session = Some(session_for("F:/old"));
        let event =
            recover_open_failure(&mut session, Path::new("F:/gone"), "folder vanished".into())
                .unwrap();
        assert_eq!(event, flow::Event::OpenCancelled);
        let current = session.unwrap();
        assert_eq!(current.app.root, PathBuf::from("F:/old"));
        let status = current.app.status;
        assert!(
            status.contains("F:/gone") && status.contains("folder vanished"),
            "status was {status:?}"
        );
    }

    fn picker_in(path: PathBuf) -> DirectoryPicker {
        let mut picker = DirectoryPicker {
            current: path,
            entries: Vec::new(),
            selected: 0,
            path_input: None,
            status: String::new(),
        };
        picker.refresh();
        picker
    }

    fn picker_dir(name: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!("recall-picker-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn an_empty_folder_selects_the_parent_so_enter_walks_out() {
        let dir = picker_dir("empty");
        let picker = picker_in(dir.clone());
        assert!(
            matches!(
                picker.entries.get(picker.selected),
                Some(PickerEntry::Parent(parent)) if Some(parent.as_path()) == dir.parent()
            ),
            "selected {} of {} entries",
            picker.selected,
            picker.entries.len()
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_folder_with_subfolders_starts_on_the_first_subfolder() {
        let dir = picker_dir("children");
        fs::create_dir_all(dir.join("beta")).unwrap();
        fs::create_dir_all(dir.join("alpha")).unwrap();
        let picker = picker_in(dir.clone());
        assert!(matches!(
            picker.entries.get(picker.selected),
            Some(PickerEntry::Folder(path)) if path.ends_with("alpha")
        ));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unreadable_folder_still_offers_the_parent() {
        let dir = picker_dir("unreadable");
        let picker = picker_in(dir.join("does-not-exist"));
        assert!(
            picker.status.starts_with("Cannot read this folder"),
            "status was {:?}",
            picker.status
        );
        assert!(matches!(
            picker.entries.get(picker.selected),
            Some(PickerEntry::Parent(parent)) if *parent == dir
        ));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_first_open_is_still_an_error() {
        let mut session = None;
        let outcome = recover_open_failure(&mut session, Path::new("F:/gone"), "boom".into());
        assert_eq!(outcome.unwrap_err().to_string(), "boom");
    }

    #[test]
    fn opening_a_missing_folder_is_an_error_not_a_panic() {
        let missing = env::temp_dir().join("recall-missing-folder-xyz");
        assert!(open_project(missing, &lux::library_path()).is_err());
    }

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

    fn app_with_help(show_help: bool) -> App {
        App {
            query: String::new(),
            hits: Vec::new(),
            selected: 0,
            total_files: 0,
            root: PathBuf::from("root"),
            status: String::new(),
            show_help,
            manager: None,
            preview_for: None,
            preview: Vec::new(),
            live: String::new(),
        }
    }

    fn screen_text(app: &App, width: u16, height: u16) -> String {
        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        buffer
            .content
            .chunks(width as usize)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn footer_hint_fits_a_100_column_terminal() {
        let text = screen_text(&app_with_help(false), 100, 24);
        let hint = text
            .lines()
            .find(|line| line.contains("Ctrl-O project"))
            .expect("footer hint should list Ctrl-O");
        assert!(hint.contains("Ctrl-Q quit"), "hint truncated: {hint:?}");
    }

    #[test]
    fn help_popup_lists_every_key_on_a_short_terminal() {
        let text = screen_text(&app_with_help(true), 100, 22);
        for expected in [
            "PgUp / PgDn",
            "Ctrl-W",
            "Ctrl-U",
            "Ctrl-O",
            "Switch to another project",
            "Saved indexes",
            "Close this panel",
        ] {
            assert!(text.contains(expected), "help is missing {expected:?}");
        }
    }

    #[test]
    fn the_status_bar_shows_the_live_state() {
        let mut app = app_with_help(false);
        app.live = "● live, updating 3".into();
        let text = screen_text(&app, 100, 24);
        assert!(text.contains("● live, updating 3"), "screen was {text}");
    }

    #[test]
    fn a_live_change_keeps_the_selected_file_selected_wherever_it_moves() {
        let dir = picker_dir("live-change");
        fs::write(dir.join("a.md"), "zebra one").unwrap();
        fs::write(dir.join("b.md"), "zebra two").unwrap();
        let mut live = Live::detached(&dir, &lux::library_path()).unwrap();
        assert!(live.settle(|_| Ok(true)).unwrap());
        let mut app = App::new(dir.clone(), live.indexed_files(), "ready".into());
        app.query = "zebra".into();
        refresh_hits(&mut app, live.index());
        assert_eq!(app.hits.len(), 2);
        app.selected = 1;
        let selected = app.hits[1].key.clone();

        fs::write(dir.join("c.md"), "zebra zebra zebra zebra zebra").unwrap();
        live.rescan();
        let end = std::time::Instant::now() + std::time::Duration::from_secs(6);
        let mut changed = Vec::new();
        while changed.is_empty() {
            changed = live.tick().changed;
            assert!(std::time::Instant::now() < end, "the rescan never reached the index");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(changed, ["c.md"]);
        on_live_change(&mut app, &live, &changed);

        assert_eq!(app.total_files, 3);
        assert_eq!(app.hits.len(), 3);
        assert_eq!(app.hits[app.selected].key, selected);
        assert_eq!(
            app.preview_for.as_ref().map(|(key, _)| key.clone()),
            Some(selected)
        );
        let _ = fs::remove_dir_all(&dir);
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
