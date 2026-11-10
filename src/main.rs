use std::{
    env,
    error::Error,
    io::{self, stdout},
    mem,
    path::{Path, PathBuf},
    process::Command,
};

use crossterm::{
    event::{
        self, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
        MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, enable_raw_mode},
};
mod cache;
mod daemon;
mod flow;
mod ipc;
mod live;
mod lux;
mod manager;
mod opening;
mod picker;
mod scan;
#[cfg(test)]
mod testing;
mod truth;
mod view;
mod watch;

use flow::Screen;
use live::Live;
use lux::{DEFAULT_LIBRARY, Hit, LuxIndex, MARK_END, MARK_START};
use manager::{IndexManager, Outcome};
use opening::{Opened, open_project};
use picker::{Choice, choose_root};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};
use view::{
    Move, Opener, TerminalCleanup, centered_rect, compact_path, draw_manager, open_manager,
    sanitize_for_display,
};

const MAX_RESULTS: usize = 40;
const GONE: &str = "The project folder is gone";

struct App {
    query: String,
    hits: Vec<Hit>,
    selected: usize,
    total_files: usize,
    root: PathBuf,
    status: Status,
    overlay: Overlay,
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
            status: status.into(),
            overlay: Overlay::None,
            preview_for: None,
            preview: Vec::new(),
            live: String::new(),
        }
    }

    fn go(&mut self, step: Move) {
        let last = self.hits.len().saturating_sub(1);
        self.selected = match step {
            Move::Up(by) => self.selected.saturating_sub(by),
            Move::Down(by) => (self.selected + by).min(last),
            Move::Home => 0,
            Move::End => last,
        };
    }
}

struct Session {
    live: Live,
    app: App,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Status {
    Notice(String),
    Rescanning,
}

impl Status {
    fn text(&self) -> &str {
        match self {
            Status::Notice(text) => text,
            Status::Rescanning => "Rescanning the project…",
        }
    }
}

impl<T: Into<String>> From<T> for Status {
    fn from(text: T) -> Self {
        Status::Notice(text.into())
    }
}

#[derive(Default)]
enum Overlay {
    #[default]
    None,
    Help,
    Manager(IndexManager),
}

enum Action {
    Idle,
    Quit,
    Rescan,
    SwitchProject,
    Edit(Edit),
    Move(Move),
    OpenFile,
}

enum Edit {
    Cancel,
    Backspace,
    Wipe,
    DropWord,
    Type(char),
}

#[derive(Debug, PartialEq, Eq)]
enum Exit {
    Quit,
    SwitchProject,
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
            Ok(Opened::Ready(opened)) => {
                session
                    .replace(*opened)
                    .into_iter()
                    .for_each(|old| old.live.close());
                flow::Event::Opened
            }
            Ok(Opened::Cancelled) => flow::Event::OpenCancelled,
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
    previous.app.status = format!("Could not open {}: {error}", compact_path(root, 48)).into();
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
            redraw |= finish_rescan(app, live);
            continue;
        }
        redraw = true;
        if let Some(exit) = handle_event(app, live, event::read()?, &open_manager) {
            return Ok(exit);
        }
    }
}

fn handle_event(app: &mut App, live: &mut Live, event: Event, open: Opener) -> Option<Exit> {
    let exit = match event {
        Event::Key(key) if key.kind == KeyEventKind::Press => {
            let (overlay, action) = route(mem::take(&mut app.overlay), key, || {
                open(Some(app.root.clone()))
            });
            app.overlay = overlay;
            apply(app, live, action)
        }
        Event::Mouse(mouse) => {
            if !matches!(app.overlay, Overlay::Manager(_)) {
                match mouse.kind {
                    MouseEventKind::ScrollUp => app.go(Move::Up(3)),
                    MouseEventKind::ScrollDown => app.go(Move::Down(3)),
                    _ => {}
                }
            }
            None
        }
        _ => None,
    };
    sync_preview(app, live.index());
    exit
}

fn route(
    overlay: Overlay,
    key: KeyEvent,
    open: impl FnOnce() -> IndexManager,
) -> (Overlay, Action) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match (overlay, key.code) {
        (overlay, KeyCode::Char('q' | 'Q')) if ctrl => (overlay, Action::Quit),
        (overlay, KeyCode::Char('r' | 'R')) if ctrl => (overlay, Action::Rescan),
        (Overlay::Manager(mut manager), _) => {
            let overlay = match manager.handle_key(key) {
                Outcome::Close => Overlay::None,
                Outcome::Stay => Overlay::Manager(manager),
            };
            (overlay, Action::Idle)
        }
        (Overlay::None | Overlay::Help, KeyCode::F(2)) => (Overlay::Manager(open()), Action::Idle),
        (Overlay::None, KeyCode::F(1)) => (Overlay::Help, Action::Idle),
        (Overlay::Help, KeyCode::F(1) | KeyCode::Esc | KeyCode::Enter | KeyCode::Char('?')) => {
            (Overlay::None, Action::Idle)
        }
        (Overlay::Help, _) => (Overlay::Help, Action::Idle),
        (Overlay::None, code) => (Overlay::None, search_action(code, ctrl)),
    }
}

fn search_action(code: KeyCode, ctrl: bool) -> Action {
    match code {
        KeyCode::Esc => Action::Edit(Edit::Cancel),
        KeyCode::Backspace => Action::Edit(Edit::Backspace),
        KeyCode::Char('u') if ctrl => Action::Edit(Edit::Wipe),
        KeyCode::Char('w') if ctrl => Action::Edit(Edit::DropWord),
        KeyCode::Char('o' | 'O') if ctrl => Action::SwitchProject,
        KeyCode::Char(ch) if !ctrl => Action::Edit(Edit::Type(ch)),
        KeyCode::Up => Action::Move(Move::Up(1)),
        KeyCode::PageUp => Action::Move(Move::Up(8)),
        KeyCode::Down => Action::Move(Move::Down(1)),
        KeyCode::PageDown => Action::Move(Move::Down(8)),
        KeyCode::Home => Action::Move(Move::Home),
        KeyCode::End => Action::Move(Move::End),
        KeyCode::Enter => Action::OpenFile,
        _ => Action::Idle,
    }
}

fn apply(app: &mut App, live: &mut Live, action: Action) -> Option<Exit> {
    match action {
        Action::Idle => {}
        Action::Quit => return Some(Exit::Quit),
        Action::SwitchProject => return Some(Exit::SwitchProject),
        Action::Rescan => {
            live.rescan();
            app.status = if *live.mode() == live::Mode::Gone {
                GONE.into()
            } else {
                Status::Rescanning
            };
        }
        Action::Edit(edit) => {
            match edit {
                Edit::Cancel | Edit::Wipe => app.query.clear(),
                Edit::Backspace => {
                    app.query.pop();
                }
                Edit::DropWord => remove_last_word(&mut app.query),
                Edit::Type(ch) => app.query.push(ch),
            }
            refresh_hits(app, live.index());
            if matches!(edit, Edit::Cancel) {
                app.status = "Search cleared".into();
            }
        }
        Action::Move(step) => app.go(step),
        Action::OpenFile => open_selected(app),
    }
    None
}

fn finish_rescan(app: &mut App, live: &Live) -> bool {
    if app.status != Status::Rescanning {
        return false;
    }
    let done = match live.mode() {
        live::Mode::Connecting | live::Mode::Scanning { .. } => false,
        _ => live.pending() == 0,
    };
    if !done {
        return false;
    }
    app.status = match live.mode() {
        live::Mode::Gone => GONE.into(),
        _ => format!("Rescan finished · {} files indexed", live.indexed_files()).into(),
    };
    true
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
        app.status = format!("Indexed {} files · type a query", app.total_files).into();
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
            )
            .into();
        }
        Err(error) => app.status = error.to_string().into(),
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
        Ok(_) => format!("Opened {}", hit.key).into(),
        Err(error) => format!("Could not open file: {error}").into(),
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

    let footer = if matches!(app.overlay, Overlay::Help) {
        Paragraph::new("F1 close help · Ctrl-Q quit")
    } else {
        Paragraph::new(vec![
            Line::from(Span::styled(
                format!("{} · {}", app.status.text(), app.live),
                Style::default().fg(Color::Yellow),
            )),
            Line::from(Span::styled(
                "↑/↓ move  Enter open  Esc clear  Ctrl-O project  F2 indexes  F1 help  Ctrl-Q quit",
                Style::default().fg(Color::DarkGray),
            )),
        ])
    };
    frame.render_widget(footer, sections[2]);
    match &app.overlay {
        Overlay::None => {}
        Overlay::Help => draw_help(frame),
        Overlay::Manager(manager) => draw_manager(frame, manager),
    }
}

fn draw_help(frame: &mut Frame) {
    {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    use crate::testing::{ch, ctrl, no_disk, picker_dir, plain, wheel};
    use ratatui::{buffer::Buffer, layout::Rect, widgets::Widget};

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
        let status = current.app.status.text().to_owned();
        assert!(
            status.contains("F:/gone") && status.contains("folder vanished"),
            "status was {status:?}"
        );
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

    fn app_with_help(show_help: bool) -> App {
        let mut app = App::new(PathBuf::from("root"), 0, String::new());
        if show_help {
            app.overlay = Overlay::Help;
        }
        app
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
            assert!(
                std::time::Instant::now() < end,
                "the rescan never reached the index"
            );
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

    use crossterm::event::MouseButton;

    #[derive(Debug, PartialEq, Eq, Clone, Copy)]
    enum Shown {
        Nothing,
        Help,
        Manager,
    }

    fn shown(app: &App) -> Shown {
        match app.overlay {
            Overlay::None => Shown::Nothing,
            Overlay::Help => Shown::Help,
            Overlay::Manager(_) => Shown::Manager,
        }
    }

    struct Rig {
        live: Live,
        app: App,
        dir: PathBuf,
    }

    impl Rig {
        fn new(name: &str) -> Rig {
            let dir = picker_dir(name);
            for number in 0..12 {
                fs::write(
                    dir.join(format!("note{number}.md")),
                    format!("zebra {number}"),
                )
                .unwrap();
            }
            let mut live = Live::detached(&dir, &lux::library_path()).expect("build libluxcore");
            assert!(live.settle(|_| Ok(true)).unwrap());
            let app = App::new(dir.clone(), live.indexed_files(), "ready".into());
            Rig { live, app, dir }
        }

        fn event(&mut self, event: Event) -> Option<Exit> {
            handle_event(&mut self.app, &mut self.live, event, &no_disk)
        }

        fn press(&mut self, key: KeyEvent) -> Option<Exit> {
            self.event(Event::Key(key))
        }

        fn type_text(&mut self, text: &str) {
            for c in text.chars() {
                assert_eq!(self.press(ch(c)), None);
            }
        }

        fn searched(name: &str) -> Rig {
            let mut rig = Rig::new(name);
            rig.type_text("zebra");
            assert_eq!(rig.app.hits.len(), 12);
            rig
        }

        fn show(&mut self, wanted: Shown) {
            match wanted {
                Shown::Nothing => {}
                Shown::Help => {
                    self.press(plain(KeyCode::F(1)));
                }
                Shown::Manager => {
                    self.press(plain(KeyCode::F(2)));
                }
            }
            assert_eq!(shown(&self.app), wanted);
        }
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    const EVERY_OVERLAY: [Shown; 3] = [Shown::Nothing, Shown::Help, Shown::Manager];

    #[test]
    fn ctrl_q_quits_whatever_is_open() {
        let mut rig = Rig::new("pin-quit");
        for wanted in EVERY_OVERLAY {
            rig.show(wanted);
            assert_eq!(rig.press(ctrl('q')), Some(Exit::Quit), "{wanted:?}");
            assert_eq!(rig.press(ctrl('Q')), Some(Exit::Quit), "{wanted:?}");
            assert_eq!(shown(&rig.app), wanted);
            rig.press(plain(KeyCode::Esc));
            rig.show(Shown::Nothing);
        }
    }

    #[test]
    fn f1_toggles_the_help_popup() {
        let mut rig = Rig::new("pin-f1");
        assert_eq!(rig.press(plain(KeyCode::F(1))), None);
        assert_eq!(shown(&rig.app), Shown::Help);
        assert_eq!(rig.press(plain(KeyCode::F(1))), None);
        assert_eq!(shown(&rig.app), Shown::Nothing);
    }

    #[test]
    fn f2_opens_the_manager_and_closes_help() {
        let mut rig = Rig::new("pin-f2");
        rig.show(Shown::Help);
        assert_eq!(rig.press(plain(KeyCode::F(2))), None);
        assert_eq!(shown(&rig.app), Shown::Manager);
        assert_eq!(rig.press(plain(KeyCode::F(2))), None);
        assert_eq!(shown(&rig.app), Shown::Nothing);
    }

    #[test]
    fn the_manager_is_opened_on_the_open_project() {
        let mut rig = Rig::new("pin-f2-root");
        let seen = std::cell::RefCell::new(None);
        let record = |root: Option<PathBuf>| {
            *seen.borrow_mut() = Some(root.clone());
            no_disk(root)
        };
        handle_event(
            &mut rig.app,
            &mut rig.live,
            Event::Key(plain(KeyCode::F(2))),
            &record,
        );
        assert_eq!(seen.into_inner(), Some(Some(rig.dir.clone())));
    }

    #[test]
    fn help_swallows_every_key_except_esc_enter_and_question_mark() {
        let mut rig = Rig::searched("pin-help-swallow");
        rig.press(plain(KeyCode::Down));
        let (query, selected, status) = (
            rig.app.query.clone(),
            rig.app.selected,
            rig.app.status.clone(),
        );
        rig.show(Shown::Help);
        for key in [
            ch('x'),
            ch(' '),
            plain(KeyCode::Backspace),
            plain(KeyCode::Down),
            plain(KeyCode::PageDown),
            plain(KeyCode::Home),
            plain(KeyCode::End),
            plain(KeyCode::Tab),
            ctrl('u'),
            ctrl('w'),
            ctrl('o'),
        ] {
            assert_eq!(rig.press(key), None, "{key:?}");
            assert_eq!(shown(&rig.app), Shown::Help, "{key:?}");
        }
        assert_eq!(rig.app.query, query);
        assert_eq!(rig.app.selected, selected);
        assert_eq!(rig.app.status, status);
    }

    #[test]
    fn esc_and_question_mark_close_help_without_clearing_the_search() {
        let mut rig = Rig::searched("pin-help-close");
        for closing in [plain(KeyCode::Esc), ch('?')] {
            rig.show(Shown::Help);
            assert_eq!(rig.press(closing), None);
            assert_eq!(shown(&rig.app), Shown::Nothing);
            assert_eq!(rig.app.query, "zebra");
        }
    }

    #[test]
    fn enter_closes_help_and_does_not_open_a_file() {
        let mut rig = Rig::new("pin-help-enter");
        rig.show(Shown::Help);
        assert_eq!(rig.press(plain(KeyCode::Enter)), None);
        assert_eq!(shown(&rig.app), Shown::Nothing);
        assert_eq!(rig.app.status.text(), "ready");
    }

    #[test]
    fn the_manager_gets_every_key_while_open() {
        let mut rig = Rig::searched("pin-manager-keys");
        rig.show(Shown::Manager);
        for key in [
            ch('x'),
            ch('?'),
            plain(KeyCode::Down),
            plain(KeyCode::Home),
            plain(KeyCode::F(1)),
            ctrl('u'),
            ctrl('o'),
        ] {
            assert_eq!(rig.press(key), None, "{key:?}");
            assert_eq!(shown(&rig.app), Shown::Manager, "{key:?}");
        }
        assert_eq!(rig.app.query, "zebra");
        assert_eq!(rig.app.selected, 0);
    }

    #[test]
    fn the_manager_closes_on_its_closing_keys_and_keeps_the_search() {
        let mut rig = Rig::searched("pin-manager-close");
        for closing in [plain(KeyCode::Esc), ch('q'), plain(KeyCode::F(2))] {
            rig.show(Shown::Manager);
            assert_eq!(rig.press(closing), None);
            assert_eq!(shown(&rig.app), Shown::Nothing);
            assert_eq!(rig.app.query, "zebra");
        }
    }

    #[test]
    fn ctrl_r_rescans_under_every_overlay_and_leaves_the_overlay_alone() {
        for wanted in EVERY_OVERLAY {
            let mut rig = Rig::new("pin-rescan");
            rig.show(wanted);
            assert!(matches!(rig.live.mode(), live::Mode::Manual { .. }));
            assert_eq!(rig.press(ctrl('r')), None, "{wanted:?}");
            assert_eq!(rig.live.mode(), &live::Mode::Connecting, "{wanted:?}");
            assert_eq!(rig.app.status, Status::Rescanning, "{wanted:?}");
            assert_eq!(shown(&rig.app), wanted);
        }
    }

    #[test]
    fn ctrl_o_asks_to_switch_project_only_from_the_bare_search() {
        let mut rig = Rig::new("pin-switch");
        assert_eq!(rig.press(ctrl('o')), Some(Exit::SwitchProject));
        assert_eq!(rig.press(ctrl('O')), Some(Exit::SwitchProject));
        rig.show(Shown::Help);
        assert_eq!(rig.press(ctrl('o')), None);
        rig.press(plain(KeyCode::Esc));
        rig.show(Shown::Manager);
        assert_eq!(rig.press(ctrl('o')), None);
    }

    #[test]
    fn typing_searches_as_you_go_and_backspace_edits() {
        let mut rig = Rig::new("pin-type");
        rig.type_text("zebr");
        assert_eq!(rig.app.query, "zebr");
        assert_eq!(rig.app.hits.len(), 12);
        assert!(
            rig.app.status.text().starts_with("12 results"),
            "{}",
            rig.app.status.text()
        );
        rig.press(plain(KeyCode::Backspace));
        assert_eq!(rig.app.query, "zeb");
        rig.press(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::ALT));
        assert_eq!(rig.app.query, "zeba");
    }

    #[test]
    fn esc_clears_the_query_the_hits_and_says_so() {
        let mut rig = Rig::searched("pin-esc");
        assert_eq!(rig.press(plain(KeyCode::Esc)), None);
        assert_eq!(rig.app.query, "");
        assert!(rig.app.hits.is_empty());
        assert_eq!(rig.app.status.text(), "Search cleared");
    }

    #[test]
    fn ctrl_u_clears_and_ctrl_w_drops_the_last_word() {
        let mut rig = Rig::new("pin-edit");
        rig.type_text("zebra two ");
        rig.press(ctrl('w'));
        assert_eq!(rig.app.query, "zebra ");
        rig.press(ctrl('w'));
        assert_eq!(rig.app.query, "");
        rig.type_text("zebra");
        rig.press(ctrl('u'));
        assert_eq!(rig.app.query, "");
        assert!(rig.app.hits.is_empty());
        assert!(
            rig.app.status.text().starts_with("Indexed"),
            "{}",
            rig.app.status.text()
        );
    }

    #[test]
    fn other_control_chords_do_nothing() {
        let mut rig = Rig::searched("pin-ctrl-other");
        let status = rig.app.status.clone();
        for key in [ctrl('x'), ctrl('z'), ctrl('U')] {
            assert_eq!(rig.press(key), None);
        }
        assert_eq!(rig.app.query, "zebra");
        assert_eq!(rig.app.status, status);
    }

    #[test]
    fn arrows_page_keys_home_and_end_move_the_selection_and_clamp() {
        let mut rig = Rig::searched("pin-move");
        let at = |rig: &Rig| rig.app.selected;
        rig.press(plain(KeyCode::Up));
        assert_eq!(at(&rig), 0);
        rig.press(plain(KeyCode::PageUp));
        assert_eq!(at(&rig), 0);
        rig.press(plain(KeyCode::Down));
        assert_eq!(at(&rig), 1);
        rig.press(plain(KeyCode::PageDown));
        assert_eq!(at(&rig), 9);
        rig.press(plain(KeyCode::PageDown));
        assert_eq!(at(&rig), 11);
        rig.press(plain(KeyCode::Down));
        assert_eq!(at(&rig), 11);
        rig.press(plain(KeyCode::PageUp));
        assert_eq!(at(&rig), 3);
        rig.press(plain(KeyCode::Up));
        assert_eq!(at(&rig), 2);
        rig.press(plain(KeyCode::End));
        assert_eq!(at(&rig), 11);
        rig.press(plain(KeyCode::Home));
        assert_eq!(at(&rig), 0);
    }

    #[test]
    fn moving_with_no_results_stays_on_zero() {
        let mut rig = Rig::new("pin-move-empty");
        for code in [
            KeyCode::Down,
            KeyCode::PageDown,
            KeyCode::End,
            KeyCode::Up,
            KeyCode::Home,
        ] {
            rig.press(plain(code));
            assert_eq!(rig.app.selected, 0, "{code:?}");
        }
    }

    #[test]
    fn selecting_a_result_shows_its_preview() {
        let mut rig = Rig::searched("pin-preview");
        let first = rig.app.preview_for.clone();
        rig.press(plain(KeyCode::Down));
        assert_ne!(rig.app.preview_for, first);
        assert_eq!(
            rig.app.preview_for.as_ref().map(|(key, _)| key.clone()),
            Some(rig.app.hits[1].key.clone())
        );
    }

    #[test]
    fn enter_with_no_results_opens_nothing() {
        let mut rig = Rig::new("pin-enter-empty");
        assert_eq!(rig.press(plain(KeyCode::Enter)), None);
        assert_eq!(rig.app.status.text(), "ready");
    }

    #[test]
    fn only_key_presses_count() {
        let mut rig = Rig::new("pin-release");
        let release = KeyEvent::new_with_kind(
            KeyCode::Char('x'),
            KeyModifiers::NONE,
            KeyEventKind::Release,
        );
        assert_eq!(rig.press(release), None);
        let quit = KeyEvent::new_with_kind(
            KeyCode::Char('q'),
            KeyModifiers::CONTROL,
            KeyEventKind::Release,
        );
        assert_eq!(rig.press(quit), None);
        assert_eq!(rig.app.query, "");
    }

    #[test]
    fn the_mouse_wheel_moves_the_selection_by_three_and_clamps() {
        let mut rig = Rig::searched("pin-wheel");
        rig.event(wheel(MouseEventKind::ScrollDown));
        assert_eq!(rig.app.selected, 3);
        for _ in 0..5 {
            rig.event(wheel(MouseEventKind::ScrollDown));
        }
        assert_eq!(rig.app.selected, 11);
        rig.event(wheel(MouseEventKind::ScrollUp));
        assert_eq!(rig.app.selected, 8);
        rig.event(wheel(MouseEventKind::Down(MouseButton::Left)));
        assert_eq!(rig.app.selected, 8);
    }

    #[test]
    fn the_mouse_wheel_does_nothing_with_no_results() {
        let mut rig = Rig::new("pin-wheel-empty");
        rig.event(wheel(MouseEventKind::ScrollDown));
        assert_eq!(rig.app.selected, 0);
    }

    #[test]
    fn the_mouse_wheel_is_ignored_while_the_manager_is_open() {
        let mut rig = Rig::searched("pin-wheel-manager");
        rig.show(Shown::Manager);
        rig.event(wheel(MouseEventKind::ScrollDown));
        assert_eq!(rig.app.selected, 0);
    }

    #[test]
    fn the_mouse_wheel_still_scrolls_the_results_behind_the_help_popup() {
        let mut rig = Rig::searched("pin-wheel-help");
        rig.show(Shown::Help);
        rig.event(wheel(MouseEventKind::ScrollDown));
        assert_eq!(rig.app.selected, 3);
        assert_eq!(shown(&rig.app), Shown::Help);
    }

    impl Rig {
        fn tick_until_quiet(&mut self) {
            let end = std::time::Instant::now() + std::time::Duration::from_secs(6);
            while matches!(
                self.live.mode(),
                live::Mode::Connecting | live::Mode::Scanning { .. }
            ) || self.live.pending() > 0
            {
                self.live.tick();
                assert!(std::time::Instant::now() < end, "the rescan never ended");
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
    }

    #[test]
    fn the_rescanning_notice_ends_with_a_rescan_that_finds_nothing() {
        let mut rig = Rig::new("pin-rescan-ends");
        rig.press(ctrl('r'));
        assert_eq!(rig.app.status, Status::Rescanning);
        assert!(!finish_rescan(&mut rig.app, &rig.live));
        assert_eq!(rig.app.status, Status::Rescanning);
        rig.tick_until_quiet();
        assert!(finish_rescan(&mut rig.app, &rig.live));
        assert_eq!(rig.app.status.text(), "Rescan finished · 12 files indexed");
        assert!(!finish_rescan(&mut rig.app, &rig.live));
    }

    #[test]
    fn a_newer_notice_is_not_overwritten_when_the_rescan_ends() {
        let mut rig = Rig::new("pin-rescan-newer");
        rig.press(ctrl('r'));
        rig.type_text("zebra");
        let shown = rig.app.status.clone();
        assert!(shown.text().starts_with("12 results"), "{shown:?}");
        rig.tick_until_quiet();
        assert!(!finish_rescan(&mut rig.app, &rig.live));
        assert_eq!(rig.app.status, shown);
    }

    #[test]
    fn finishing_a_rescan_that_never_started_changes_nothing() {
        let mut rig = Rig::new("pin-rescan-none");
        assert!(!finish_rescan(&mut rig.app, &rig.live));
        assert_eq!(rig.app.status.text(), "ready");
    }
}
