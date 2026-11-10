use std::{
    error::Error,
    io::{self, stdout},
    mem,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
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
    lux::LuxIndex,
    manager::{IndexManager, Outcome},
    preview::build_preview,
    shelf::{
        self, Found, Mode, Problem, Project, Reach, Request, Shelf, Source, parse_request, sources,
    },
    view::{
        Move, Opener, TerminalCleanup, centered_rect, compact_path, draw_manager, open_manager,
        sanitize_for_display,
    },
};

const HERE: usize = 0;
const MAX_RESULTS: usize = 40;
const MAX_SUGGESTIONS: usize = 8;
const PALETTE: [Color; 6] = [
    Color::Magenta,
    Color::Blue,
    Color::Yellow,
    Color::LightRed,
    Color::LightGreen,
    Color::LightCyan,
];
const BAD_QUERY: &str = "LuxCore cannot read the query from the red mark";
const GONE: &str = "The project folder is gone";
/// The shortest time the Rescanning notice stays up.
const RESCAN_NOTICE_MIN: Duration = Duration::from_secs(1);

#[derive(Clone, Debug, PartialEq, Eq)]
struct PreviewKey {
    source: usize,
    key: String,
    mode: Mode,
    query: String,
}

pub(crate) struct App {
    query: String,
    mode: Mode,
    reach: Reach,
    pub(super) shelf: Shelf,
    project: Project,
    suggestions: Vec<String>,
    problem: Option<Problem>,
    hits: Vec<Found>,
    selected: usize,
    total_files: usize,
    pub(super) root: PathBuf,
    pub(super) status: Status,
    overlay: Overlay,
    preview_for: Option<PreviewKey>,
    preview: Vec<Line<'static>>,
    live: String,
}

impl App {
    pub(crate) fn new(root: PathBuf, total_files: usize, status: String) -> Self {
        Self {
            query: String::new(),
            mode: Mode::default(),
            reach: Reach::default(),
            shelf: Shelf::default(),
            project: Project::at(root.clone()),
            suggestions: Vec::new(),
            problem: None,
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
    Rescanning { since: Instant },
}

impl Status {
    pub(super) fn text(&self) -> &str {
        match self {
            Status::Notice(text) => text,
            Status::Rescanning { .. } => "Rescanning the project…",
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
    Accept,
    ToggleMode,
    ToggleReach,
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
            terminal.draw(|frame| draw(frame, app, live.index()))?;
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
        KeyCode::Char('t' | 'T') if ctrl => Action::Edit(Edit::ToggleMode),
        KeyCode::Char('e' | 'E') if ctrl => Action::Edit(Edit::ToggleReach),
        KeyCode::Tab => Action::Edit(Edit::Accept),
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
                Status::Rescanning {
                    since: Instant::now(),
                }
            };
        }
        Action::Edit(edit) => {
            match edit {
                Edit::Accept => {
                    let tail = shelf::ghost(&app.query, &app.suggestions).to_owned();
                    if tail.is_empty() {
                        return None;
                    }
                    app.query.push_str(&tail);
                }
                Edit::ToggleMode => {
                    app.mode = match app.mode {
                        Mode::Fuzzy => Mode::Syntax,
                        Mode::Syntax => Mode::Fuzzy,
                    };
                }
                Edit::ToggleReach => {
                    app.reach = match app.reach {
                        Reach::Here => Reach::Everywhere,
                        Reach::Everywhere => Reach::Here,
                    };
                }
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
            if matches!(edit, Edit::ToggleReach) && app.query.trim().is_empty() {
                app.status = reach_status(app.reach, app.shelf.len()).into();
            }
        }
        Action::Move(step) => app.go(step),
        Action::OpenFile => open_selected(app, live.index()),
    }
    None
}

fn finish_rescan(app: &mut App, live: &Live) -> bool {
    let Status::Rescanning { since } = app.status else {
        return false;
    };
    // A rescan sends no update when nothing changed, so nothing observable
    // marks its start. Before the minimum, a quiet Live may just be waiting.
    let done = since.elapsed() >= RESCAN_NOTICE_MIN
        && match live.mode() {
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
    let keep = app
        .hits
        .get(app.selected)
        .map(|hit| (hit.source, hit.key.clone()));
    if keep
        .as_ref()
        .is_some_and(|(source, key)| *source == HERE && changed.contains(key))
    {
        app.preview_for = None;
    }
    run_query(app, live.index(), keep);
}

fn refresh_hits(app: &mut App, index: &LuxIndex) {
    run_query(app, index, None);
}

fn run_query(app: &mut App, index: &LuxIndex, keep: Option<(usize, String)>) {
    let (outcome, suggestions, status) = {
        let all = sources(Some((&app.project, index)), &app.shelf);
        let request = parse_request(&app.query, app.mode, app.reach);
        let outcome = shelf::search(&all, &request, MAX_RESULTS);
        let status = status_text(&request, &outcome, &all, app.total_files);
        (
            outcome,
            shelf::suggest(&all, &request, MAX_SUGGESTIONS),
            status,
        )
    };
    app.hits = outcome.found;
    app.suggestions = suggestions;
    app.problem = outcome.problem;
    let moved_to = keep.and_then(|(source, key)| {
        app.hits
            .iter()
            .position(|hit| hit.source == source && hit.key == key)
    });
    app.selected = moved_to
        .unwrap_or(app.selected)
        .min(app.hits.len().saturating_sub(1));
    sync_preview(app, index);
    app.status = status.into();
}

fn reach_status(reach: Reach, others: usize) -> String {
    match (reach, others) {
        (_, 0) => "No other saved projects yet · open one in Recall to search it from here".into(),
        (Reach::Everywhere, others) => format!("Searching every saved project · {others} others"),
        (Reach::Here, _) => "Searching this project only".into(),
    }
}

fn status_text(
    request: &Request,
    outcome: &shelf::Outcome,
    sources: &[Source],
    total_files: usize,
) -> String {
    let blank = request.text.trim().is_empty();
    let found = outcome.found.len();
    let name_of = |position: &usize| {
        sources
            .get(*position)
            .map(|source| source.project.name.as_str())
    };
    let mut text = match (request.scopes.as_slice(), outcome.searched.as_slice()) {
        ([], _) if blank => format!("Indexed {total_files} files · type a query"),
        ([_, ..], []) => {
            let typed: Vec<String> = request
                .scopes
                .iter()
                .map(|scope| format!("@{scope}"))
                .collect();
            format!("No saved project matches {}", typed.join(" "))
        }
        (_, searched) if blank => {
            let names: Vec<&str> = searched.iter().filter_map(name_of).collect();
            format!("Searching {} · type a query", names.join(", "))
        }
        _ if outcome.problem.is_some() => BAD_QUERY.into(),
        (_, [only]) if sources.get(*only).is_some_and(|source| !source.here) => format!(
            "{found} results in {} · Enter opens selected file",
            name_of(only).unwrap_or_default()
        ),
        (_, [_, _, ..]) => format!(
            "{found} results across {} projects · Enter opens selected file",
            outcome.searched.len()
        ),
        _ => format!("{found} results · {total_files} indexed · Enter opens selected file"),
    };
    if !outcome.failed.is_empty() {
        text.push_str(&format!(
            " · {} could not be searched",
            outcome.failed.len()
        ));
    }
    text
}

fn sync_preview(app: &mut App, index: &LuxIndex) {
    let wanted = app.hits.get(app.selected).map(|hit| PreviewKey {
        source: hit.source,
        key: hit.key.clone(),
        mode: app.mode,
        query: app.query.clone(),
    });
    if wanted == app.preview_for {
        return;
    }
    let lines = wanted.as_ref().and_then(|wanted| {
        let all = sources(Some((&app.project, index)), &app.shelf);
        let request = parse_request(&wanted.query, wanted.mode, app.reach);
        Some(build_preview(
            all.get(wanted.source)?,
            &wanted.key,
            &request,
        ))
    });
    app.preview = lines.unwrap_or_default();
    app.preview_for = wanted;
}

fn target_path(all: &[Source], found: &Found) -> Option<PathBuf> {
    Some(all.get(found.source)?.project.root.join(&found.key))
}

fn open_selected(app: &mut App, index: &LuxIndex) {
    let Some(hit) = app.hits.get(app.selected) else {
        return;
    };
    let all = sources(Some((&app.project, index)), &app.shelf);
    let Some(path) = target_path(&all, hit) else {
        return;
    };
    #[cfg(target_os = "windows")]
    let result = Command::new("explorer.exe").arg(&path).spawn();
    #[cfg(target_os = "macos")]
    let result = Command::new("open").arg(&path).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let result = Command::new("xdg-open").arg(&path).spawn();
    let status = match result {
        Ok(_) => format!("Opened {}", hit.key),
        Err(error) => format!("Could not open file: {error}"),
    };
    app.status = status.into();
}

fn clamp_to_boundary(text: &str, offset: usize) -> usize {
    let mut at = offset.min(text.len());
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

fn query_line(app: &App) -> Line<'static> {
    if app.query.is_empty() {
        return Line::from(Span::styled(
            "Search terms, a phrase, or a code symbol…",
            Style::default().fg(Color::DarkGray),
        ));
    }
    let split = app.problem.map_or(app.query.len(), |problem| {
        clamp_to_boundary(&app.query, problem.offset)
    });
    let (good, bad) = app.query.split_at(split);
    let mut spans = vec![Span::styled(
        sanitize_for_display(good),
        Style::default().fg(Color::White),
    )];
    if !bad.is_empty() {
        spans.push(Span::styled(
            sanitize_for_display(bad),
            Style::default().fg(Color::Red),
        ));
    }
    let ghost = shelf::ghost(&app.query, &app.suggestions);
    if !ghost.is_empty() {
        spans.push(Span::styled(
            sanitize_for_display(ghost),
            Style::default().fg(Color::DarkGray),
        ));
    }
    spans.push(Span::styled("▏", Style::default().fg(Color::Cyan)));
    Line::from(spans)
}

fn box_title(app: &App) -> String {
    let reach = match app.reach {
        Reach::Here => "",
        Reach::Everywhere => " · everywhere",
    };
    let mode = match app.mode {
        Mode::Fuzzy => "",
        Mode::Syntax => " · syntax",
    };
    format!(" ✦ RECALL · {}{reach}{mode} ", compact_path(&app.root, 48))
}

fn draw(frame: &mut Frame, app: &App, index: &LuxIndex) {
    let all = sources(Some((&app.project, index)), &app.shelf);
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(7),
            Constraint::Length(2),
        ])
        .split(frame.area());
    let search = Paragraph::new(query_line(app))
        .block(Block::default().title(box_title(app)).borders(Borders::ALL));
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
            let key = Span::styled(
                sanitize_for_display(&hit.key),
                Style::default().fg(Color::DarkGray),
            );
            let place = match all.get(hit.source) {
                Some(source) if !source.here => vec![
                    Span::styled(
                        sanitize_for_display(&source.project.name),
                        Style::default().fg(PALETTE[hit.source % PALETTE.len()]),
                    ),
                    Span::styled(" › ", Style::default().fg(Color::DarkGray)),
                    key,
                ],
                _ => vec![key],
            };
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
                Line::from(place),
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
        .map(|hit| {
            let key = sanitize_for_display(&hit.key);
            match all.get(hit.source) {
                Some(source) if !source.here => format!(
                    " {} › {key} · {:.3} ",
                    sanitize_for_display(&source.project.name),
                    hit.score
                ),
                _ => format!(" {key} · {:.3} ", hit.score),
            }
        })
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
                "Tab complete  Ctrl-E all  Ctrl-T syntax  Enter open  Ctrl-O project  F1 help  Ctrl-Q quit",
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
            Line::from("Type to search as you go. Typos are forgiven; files stay current."),
            Line::from(""),
            Line::from("↑ / ↓      Move through results (mouse wheel works too)"),
            Line::from("PgUp / PgDn  Jump through results faster"),
            Line::from("Enter      Open selected file    Esc clear search"),
            Line::from("Ctrl-W     Delete last word    Ctrl-U clear all"),
            Line::from("Tab        Accept the grey completion"),
            Line::from("Ctrl-E     Search every saved project, or only this one"),
            Line::from("Ctrl-T     Switch between fuzzy and query syntax"),
            Line::from("@name      At the start: only saved projects whose name has it"),
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
