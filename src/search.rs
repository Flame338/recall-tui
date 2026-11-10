use std::{
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
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};

use crate::{
    Exit,
    live::{self, Live},
    lux::{Hit, LuxIndex},
    manager::{IndexManager, Outcome},
    preview::build_preview,
    view::{
        Move, Opener, TerminalCleanup, centered_rect, compact_path, draw_manager, open_manager,
        sanitize_for_display,
    },
};

const MAX_RESULTS: usize = 40;
const GONE: &str = "The project folder is gone";

pub(crate) struct App {
    query: String,
    hits: Vec<Hit>,
    selected: usize,
    total_files: usize,
    pub(super) root: PathBuf,
    pub(super) status: Status,
    overlay: Overlay,
    preview_for: Option<(String, String)>,
    preview: Vec<Line<'static>>,
    live: String,
}

impl App {
    pub(crate) fn new(root: PathBuf, total_files: usize, status: String) -> Self {
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Status {
    Notice(String),
    Rescanning,
}

impl Status {
    pub(super) fn text(&self) -> &str {
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

type RecallTerminal = Terminal<CrosstermBackend<io::Stdout>>;

pub(super) fn run_tui(app: &mut App, live: &mut Live) -> Result<Exit, Box<dyn Error>> {
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

#[cfg(test)]
#[path = "search_tests.rs"]
mod tests;
