use std::{
    mem,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};

use crate::{cache, cache::CacheEntry, compact_path, sanitize_for_display};

const PAGE: usize = 8;
const LABEL_LIMIT: usize = 60;
const UNKNOWN_FOLDER: &str = "(unknown folder)";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Stay,
    Close,
}

enum Mode {
    Browsing,
    ConfirmOne { name: String, label: String },
    ConfirmAll,
}

pub struct IndexManager {
    dir: Option<PathBuf>,
    open_root: Option<PathBuf>,
    entries: Vec<CacheEntry>,
    selected: usize,
    mode: Mode,
    notice: String,
}

impl IndexManager {
    pub fn open(dir: Option<PathBuf>, open_root: Option<PathBuf>) -> Self {
        let mut manager = Self {
            dir,
            open_root,
            entries: Vec::new(),
            selected: 0,
            mode: Mode::Browsing,
            notice: String::new(),
        };
        manager.refresh();
        manager
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Outcome {
        if key.kind != KeyEventKind::Press
            || key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return Outcome::Stay;
        }
        match self.mode {
            Mode::Browsing => self.browse(key.code),
            Mode::ConfirmOne { .. } | Mode::ConfirmAll => {
                self.confirm(key.code);
                Outcome::Stay
            }
        }
    }

    fn browse(&mut self, code: KeyCode) -> Outcome {
        let last = self.entries.len().saturating_sub(1);
        match code {
            KeyCode::Esc | KeyCode::F(2) | KeyCode::Char('q' | 'Q') => return Outcome::Close,
            KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down => self.selected = (self.selected + 1).min(last),
            KeyCode::PageUp => self.selected = self.selected.saturating_sub(PAGE),
            KeyCode::PageDown => self.selected = (self.selected + PAGE).min(last),
            KeyCode::Home => self.selected = 0,
            KeyCode::End => self.selected = last,
            KeyCode::Delete | KeyCode::Char('d' | 'D') => {
                if let Some(entry) = self.entries.get(self.selected) {
                    self.mode = Mode::ConfirmOne {
                        name: entry.name.clone(),
                        label: label_of(entry),
                    };
                }
            }
            KeyCode::Char('a' | 'A') if !self.entries.is_empty() => self.mode = Mode::ConfirmAll,
            _ => {}
        }
        Outcome::Stay
    }

    fn confirm(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('y' | 'Y') | KeyCode::Enter => {
                let mode = mem::replace(&mut self.mode, Mode::Browsing);
                self.notice = self.delete(mode);
                self.refresh();
            }
            KeyCode::Char('n' | 'N') | KeyCode::Esc => {
                self.mode = Mode::Browsing;
                self.notice.clear();
            }
            _ => {}
        }
    }

    fn delete(&self, mode: Mode) -> String {
        let Some(dir) = &self.dir else {
            return String::new();
        };
        match mode {
            Mode::Browsing => String::new(),
            Mode::ConfirmOne { name, label } => match cache::delete_entry(dir, &name) {
                Ok(removed) => format!(
                    "Deleted {removed} {} for {}",
                    plural(removed, "file", "files"),
                    compact_path(Path::new(&sanitize_for_display(&label)), LABEL_LIMIT)
                ),
                Err(error) => format!("Could not delete: {error}"),
            },
            Mode::ConfirmAll => {
                let count = self.entries.len();
                match cache::delete_all(dir) {
                    Ok(_) => format!(
                        "Deleted {count} saved {}",
                        plural(count, "index", "indexes")
                    ),
                    Err(error) => format!("Could not delete: {error}"),
                }
            }
        }
    }

    fn refresh(&mut self) {
        self.entries = self
            .dir
            .as_deref()
            .map(cache::list_entries)
            .unwrap_or_default();
        self.selected = self.selected.min(self.entries.len().saturating_sub(1));
    }

    fn is_open(&self, entry: &CacheEntry) -> bool {
        entry
            .root
            .as_deref()
            .zip(self.open_root.as_deref())
            .is_some_and(|(root, open)| Path::new(root) == open)
    }

    fn prompt(&self) -> Option<String> {
        match &self.mode {
            Mode::Browsing => None,
            Mode::ConfirmOne { name, label } => {
                let label = compact_path(Path::new(&sanitize_for_display(label)), LABEL_LIMIT);
                let open = self
                    .entries
                    .iter()
                    .find(|entry| entry.name == *name)
                    .is_some_and(|entry| self.is_open(entry));
                Some(if open {
                    format!(
                        "Delete the saved index for {label}? It is open now and will be rebuilt next launch."
                    )
                } else {
                    format!(
                        "Delete the saved index for {label}? It will be rebuilt the next time you index it."
                    )
                })
            }
            Mode::ConfirmAll => {
                let total: u64 = self.entries.iter().map(|entry| entry.bytes).sum();
                let count = self.entries.len();
                Some(format!(
                    "Delete all {count} saved {} ({})? Each folder will be rebuilt the next time you index it.",
                    plural(count, "index", "indexes"),
                    format_bytes(total)
                ))
            }
        }
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect) {
        frame.render_widget(Clear, area);
        let block = Block::default()
            .title(" Saved indexes ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let [header, list, prompt, hint] = Layout::vertical([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(2),
            Constraint::Length(1),
        ])
        .areas(inner);

        self.draw_header(frame, header);
        self.draw_entries(frame, list);

        let (message, color) = match self.prompt() {
            Some(prompt) => (prompt, Color::Yellow),
            None => (self.notice.clone(), Color::Cyan),
        };
        frame.render_widget(
            Paragraph::new(message)
                .style(Style::default().fg(color))
                .wrap(Wrap { trim: true }),
            prompt,
        );
        let keys = if self.prompt().is_some() {
            "Y/Enter delete  N/Esc cancel"
        } else {
            "↑/↓ move  D delete  A delete all  Esc close"
        };
        frame.render_widget(
            Paragraph::new(keys).style(Style::default().fg(Color::DarkGray)),
            hint,
        );
    }

    fn draw_header(&self, frame: &mut Frame, area: Rect) {
        let place = match &self.dir {
            Some(dir) => sanitize_for_display(&compact_path(
                dir,
                usize::from(area.width).saturating_sub(2).max(8),
            )),
            None => "No cache folder".to_owned(),
        };
        let count = self.entries.len();
        let total: u64 = self.entries.iter().map(|entry| entry.bytes).sum();
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(place, Style::default().fg(Color::DarkGray))),
                Line::from(format!(
                    "{count} {} · {}",
                    plural(count, "index", "indexes"),
                    format_bytes(total)
                )),
            ]),
            area,
        );
    }

    fn draw_entries(&self, frame: &mut Frame, area: Rect) {
        if self.entries.is_empty() {
            let lines = match &self.dir {
                Some(_) => vec![
                    Line::from(Span::styled(
                        "No saved indexes",
                        Style::default().fg(Color::Yellow),
                    )),
                    Line::from(Span::styled(
                        "An index is saved here each time you index a folder.",
                        Style::default().fg(Color::DarkGray),
                    )),
                ],
                None => vec![
                    Line::from(Span::styled(
                        "No saved indexes",
                        Style::default().fg(Color::Yellow),
                    )),
                    Line::from(Span::styled(
                        "Recall found no cache folder (LOCALAPPDATA, XDG_CACHE_HOME or HOME), so it saves nothing.",
                        Style::default().fg(Color::DarkGray),
                    )),
                ],
            };
            frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), area);
            return;
        }
        let width = usize::from(area.width).saturating_sub(2);
        let items: Vec<ListItem> = self
            .entries
            .iter()
            .map(|entry| self.row(entry, width))
            .collect();
        let list = List::new(items)
            .highlight_style(
                Style::default()
                    .bg(Color::Rgb(36, 53, 68))
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("▌ ");
        let mut state = ListState::default();
        state.select(Some(self.selected));
        frame.render_stateful_widget(list, area, &mut state);
    }

    fn row(&self, entry: &CacheEntry, width: usize) -> ListItem<'static> {
        let marker = if self.is_open(entry) { "● " } else { "  " };
        let stats = stats_of(entry);
        let budget = width
            .saturating_sub(marker.chars().count() + stats.chars().count() + 2)
            .max(8);
        let label = match &entry.root {
            Some(root) => compact_path(Path::new(&sanitize_for_display(root)), budget),
            None => format!("{UNKNOWN_FOLDER} {}", entry.name),
        };
        let gap = width
            .saturating_sub(marker.chars().count() + label.chars().count() + stats.chars().count())
            .max(2);
        ListItem::new(Line::from(vec![
            Span::styled(marker, Style::default().fg(Color::Green)),
            Span::styled(label, Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" ".repeat(gap)),
            Span::styled(stats, Style::default().fg(Color::DarkGray)),
        ]))
    }
}

fn label_of(entry: &CacheEntry) -> String {
    match &entry.root {
        Some(root) => root.clone(),
        None => format!("{UNKNOWN_FOLDER} {}", entry.name),
    }
}

fn stats_of(entry: &CacheEntry) -> String {
    let mut parts = Vec::new();
    if entry.root.is_some() {
        parts.push(format!(
            "{} {}",
            entry.files,
            plural(entry.files, "file", "files")
        ));
    }
    parts.push(format_bytes(entry.bytes));
    if let Some(modified) = entry.modified {
        let age = SystemTime::now()
            .duration_since(modified)
            .unwrap_or(Duration::ZERO);
        parts.push(format_age(age));
    }
    parts.join(" · ")
}

fn plural(count: usize, one: &'static str, many: &'static str) -> &'static str {
    if count == 1 { one } else { many }
}

pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while unit < UNITS.len() - 1 && (value * 10.0).round() >= 10240.0 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

pub fn format_age(age: Duration) -> String {
    let seconds = age.as_secs();
    match seconds {
        0..60 => "just now".to_owned(),
        60..3600 => format!("{}m ago", seconds / 60),
        3600..86400 => format!("{}h ago", seconds / 3600),
        _ => format!("{}d ago", seconds / 86400),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        env, fs, process,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use ratatui::{Terminal, backend::TestBackend};

    use super::*;

    static NEXT: AtomicUsize = AtomicUsize::new(0);

    struct Home(PathBuf);

    impl Home {
        fn new(name: &str) -> Self {
            let id = NEXT.fetch_add(1, Ordering::Relaxed);
            let dir = env::temp_dir().join(format!("recall-manager-{}-{id}-{name}", process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn add(&self, name: &str, root: Option<&str>, files: usize, lux_bytes: u64, age: u64) {
            let lux = self.0.join(format!("{name}.lux"));
            fs::File::create(&lux).unwrap().set_len(lux_bytes).unwrap();
            let mut touched = vec![lux];
            if let Some(root) = root {
                let manifest: cache::Manifest = (0..files)
                    .map(|n| {
                        (
                            format!("f{n}.md"),
                            cache::Entry {
                                stamp: cache::Stamp {
                                    size: 1,
                                    modified: 1,
                                },
                                indexed: true,
                            },
                        )
                    })
                    .collect();
                let path = self.0.join(format!("{name}.manifest"));
                fs::write(&path, cache::serialize("fp", Path::new(root), &manifest)).unwrap();
                touched.push(path);
            }
            let long_ago = SystemTime::now() - Duration::from_secs(age);
            for path in touched {
                let file = fs::OpenOptions::new().write(true).open(path).unwrap();
                file.set_modified(long_ago).unwrap();
            }
        }

        fn three() -> Self {
            let home = Self::new("three");
            home.add(
                "00000000000000a1",
                Some(r"F:\alpha"),
                1234,
                2 * 1024 * 1024,
                7200,
            );
            home.add("00000000000000b2", Some(r"F:\beta"), 12, 700, 300);
            home.add("00000000000000c3", None, 0, 40, 3 * 86400);
            home
        }

        fn names(&self) -> Vec<String> {
            let mut names: Vec<String> = fs::read_dir(&self.0)
                .unwrap()
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        }

        fn manager(&self, open: Option<&str>) -> IndexManager {
            IndexManager::open(Some(self.0.clone()), open.map(PathBuf::from))
        }

        fn size_of(&self, stem: &str) -> u64 {
            ["lux", "manifest"]
                .iter()
                .filter_map(|ext| fs::metadata(self.0.join(format!("{stem}.{ext}"))).ok())
                .map(|metadata| metadata.len())
                .sum()
        }
    }

    impl Drop for Home {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn press(manager: &mut IndexManager, code: KeyCode) -> Outcome {
        manager.handle_key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn type_keys(manager: &mut IndexManager, keys: &str) {
        for key in keys.chars() {
            press(manager, KeyCode::Char(key));
        }
    }

    fn screen(manager: &IndexManager, width: u16, height: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| manager.draw(frame, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol().to_owned())
                    .collect::<String>()
            })
            .collect()
    }

    fn shows(rows: &[String], needle: &str) -> bool {
        rows.iter().any(|row| row.contains(needle))
    }

    fn row_with<'a>(rows: &'a [String], needle: &str) -> &'a str {
        rows.iter()
            .find(|row| row.contains(needle))
            .unwrap_or_else(|| panic!("no row shows {needle:?} in {rows:#?}"))
    }

    #[test]
    fn bytes_use_binary_units_with_one_decimal() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1024), "1.0 KB");
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(format_bytes(1024 * 1024 - 1), "1.0 MB");
        assert_eq!(format_bytes(18 * 1024 * 1024), "18.0 MB");
        assert_eq!(format_bytes(1288490189), "1.2 GB");
        assert_eq!(format_bytes(u64::MAX), "16777216.0 TB");
    }

    #[test]
    fn ages_round_down_to_the_largest_unit() {
        let age = |seconds| format_age(Duration::from_secs(seconds));
        assert_eq!(age(0), "just now");
        assert_eq!(age(59), "just now");
        assert_eq!(age(60), "1m ago");
        assert_eq!(age(300), "5m ago");
        assert_eq!(age(3599), "59m ago");
        assert_eq!(age(7200), "2h ago");
        assert_eq!(age(86399), "23h ago");
        assert_eq!(age(3 * 86400 + 5), "3d ago");
    }

    #[test]
    fn delete_then_no_keeps_every_file() {
        let home = Home::three();
        let before = home.names();
        let mut manager = home.manager(None);
        press(&mut manager, KeyCode::Char('d'));
        assert_eq!(press(&mut manager, KeyCode::Char('n')), Outcome::Stay);
        assert_eq!(home.names(), before);
        press(&mut manager, KeyCode::Char('d'));
        assert_eq!(press(&mut manager, KeyCode::Esc), Outcome::Stay);
        assert_eq!(home.names(), before);
        assert!(!shows(&screen(&manager, 90, 14), "Delete the saved index"));
    }

    #[test]
    fn delete_then_yes_removes_exactly_the_selected_entry() {
        let home = Home::three();
        let mut manager = home.manager(None);
        press(&mut manager, KeyCode::Down);
        press(&mut manager, KeyCode::Char('d'));
        press(&mut manager, KeyCode::Char('y'));
        assert_eq!(
            home.names(),
            [
                "00000000000000a1.lux",
                "00000000000000a1.manifest",
                "00000000000000c3.lux",
            ]
        );
        let rows = screen(&manager, 90, 14);
        assert!(shows(&rows, r"Deleted 2 files for F:\beta"));
        assert!(shows(&rows, r"F:\alpha"));
        assert!(!shows(&rows, "12 files"));
        assert!(shows(&rows, "2 indexes"));
    }

    #[test]
    fn enter_confirms_and_the_selection_stays_inside_the_shorter_list() {
        let home = Home::three();
        let mut manager = home.manager(None);
        press(&mut manager, KeyCode::End);
        press(&mut manager, KeyCode::Delete);
        press(&mut manager, KeyCode::Enter);
        assert_eq!(home.names().len(), 4);
        let rows = screen(&manager, 90, 14);
        assert!(row_with(&rows, "▌").contains(r"F:\beta"));
        press(&mut manager, KeyCode::Char('d'));
        press(&mut manager, KeyCode::Char('y'));
        assert_eq!(home.names().len(), 2);
    }

    #[test]
    fn delete_all_then_enter_empties_the_folder_of_cache_files() {
        let home = Home::three();
        fs::write(home.0.join("keep.txt"), "mine").unwrap();
        fs::write(home.0.join("00000000000000a1.lux.5.tmp"), "half").unwrap();
        let mut manager = home.manager(None);
        press(&mut manager, KeyCode::Char('a'));
        press(&mut manager, KeyCode::Enter);
        assert_eq!(home.names(), ["keep.txt"]);
        let rows = screen(&manager, 90, 14);
        assert!(shows(&rows, "Deleted 3 saved indexes"));
        assert!(shows(&rows, "No saved indexes"));
    }

    #[test]
    fn closing_keys_close_only_while_browsing() {
        let home = Home::three();
        for code in [KeyCode::Esc, KeyCode::Char('q'), KeyCode::F(2)] {
            let mut manager = home.manager(None);
            assert_eq!(press(&mut manager, code), Outcome::Close);
        }
        let mut manager = home.manager(None);
        press(&mut manager, KeyCode::Char('d'));
        assert_eq!(press(&mut manager, KeyCode::F(2)), Outcome::Stay);
        assert_eq!(press(&mut manager, KeyCode::Char('q')), Outcome::Stay);
        assert!(shows(&screen(&manager, 90, 14), "Delete the saved index"));
    }

    #[test]
    fn other_keys_do_nothing_in_a_confirm_prompt() {
        let home = Home::three();
        let before = home.names();
        let mut manager = home.manager(None);
        press(&mut manager, KeyCode::Char('a'));
        type_keys(&mut manager, "xdaq");
        press(&mut manager, KeyCode::Down);
        press(&mut manager, KeyCode::Tab);
        assert_eq!(home.names(), before);
        assert!(shows(
            &screen(&manager, 100, 14),
            "Delete all 3 saved indexes"
        ));
    }

    #[test]
    fn release_and_control_keys_are_ignored() {
        let home = Home::three();
        let mut manager = home.manager(None);
        let release = KeyEvent::new_with_kind(
            KeyCode::Char('d'),
            KeyModifiers::NONE,
            KeyEventKind::Release,
        );
        manager.handle_key(release);
        manager.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
        assert!(!shows(&screen(&manager, 100, 14), "Delete"));
    }

    #[test]
    fn deleting_with_nothing_saved_does_nothing() {
        let home = Home::new("empty");
        let mut manager = home.manager(None);
        press(&mut manager, KeyCode::Char('d'));
        press(&mut manager, KeyCode::Char('a'));
        press(&mut manager, KeyCode::Enter);
        press(&mut manager, KeyCode::Char('y'));
        press(&mut manager, KeyCode::PageDown);
        press(&mut manager, KeyCode::End);
        let rows = screen(&manager, 90, 14);
        assert!(shows(&rows, "No saved indexes"));
        assert!(shows(&rows, "0 indexes · 0 B"));
        assert!(!shows(&rows, "Delete"));
        assert_eq!(press(&mut manager, KeyCode::Esc), Outcome::Close);
    }

    #[test]
    fn without_a_cache_folder_it_explains_and_stays_inert() {
        let mut manager = IndexManager::open(None, None);
        press(&mut manager, KeyCode::Char('d'));
        press(&mut manager, KeyCode::Char('a'));
        let rows = screen(&manager, 100, 14);
        assert!(shows(&rows, "No cache folder"));
        assert!(shows(&rows, "saves nothing"));
        assert!(!shows(&rows, "Delete "));
        assert_eq!(press(&mut manager, KeyCode::F(2)), Outcome::Close);
    }

    #[test]
    fn navigation_moves_the_highlight_and_clamps_at_both_ends() {
        let home = Home::three();
        let mut manager = home.manager(None);
        let highlighted =
            |manager: &IndexManager| row_with(&screen(manager, 90, 14), "▌").to_owned();
        assert!(highlighted(&manager).contains(r"F:\alpha"));
        press(&mut manager, KeyCode::Up);
        assert!(highlighted(&manager).contains(r"F:\alpha"));
        press(&mut manager, KeyCode::Down);
        assert!(highlighted(&manager).contains(r"F:\beta"));
        press(&mut manager, KeyCode::PageDown);
        assert!(highlighted(&manager).contains(UNKNOWN_FOLDER));
        press(&mut manager, KeyCode::Down);
        assert!(highlighted(&manager).contains(UNKNOWN_FOLDER));
        press(&mut manager, KeyCode::Home);
        assert!(highlighted(&manager).contains(r"F:\alpha"));
        press(&mut manager, KeyCode::End);
        assert!(highlighted(&manager).contains(UNKNOWN_FOLDER));
        press(&mut manager, KeyCode::PageUp);
        assert!(highlighted(&manager).contains(r"F:\alpha"));
    }

    #[test]
    fn the_list_shows_folders_counts_sizes_ages_and_marks_the_open_folder() {
        let home = Home::three();
        let manager = home.manager(Some(r"F:\alpha"));
        let rows = screen(&manager, 90, 14);

        let alpha = row_with(&rows, r"F:\alpha");
        assert!(alpha.contains("●"));
        assert!(alpha.contains("1234 files"));
        assert!(alpha.contains(&format_bytes(home.size_of("00000000000000a1"))));
        assert!(alpha.contains("2h ago"));

        let beta = row_with(&rows, r"F:\beta");
        assert!(!beta.contains("●"));
        assert!(beta.contains("12 files"));
        assert!(beta.contains("5m ago"));

        let orphan = row_with(&rows, "(unknown folder) 00000000000000c3");
        assert!(!orphan.contains("●"));
        assert!(orphan.contains("3d ago"));
        assert!(!orphan.contains("files"));

        assert!(shows(&rows, "Saved indexes"));
        assert!(shows(&rows, "3 indexes"));
        assert!(shows(&rows, "↑/↓ move  D delete  A delete all  Esc close"));
        assert!(!shows(&rows, "Delete"));
    }

    #[test]
    fn a_confirm_prompt_names_the_folder_and_what_happens_next() {
        let home = Home::three();
        let mut manager = home.manager(Some(r"F:\alpha"));

        press(&mut manager, KeyCode::Char('d'));
        let open = screen(&manager, 100, 14);
        assert!(shows(&open, r"Delete the saved index for F:\alpha?"));
        assert!(shows(&open, "is open now and will be rebuilt next launch"));
        assert!(shows(&open, "Y/Enter delete  N/Esc cancel"));
        press(&mut manager, KeyCode::Char('n'));

        press(&mut manager, KeyCode::Down);
        press(&mut manager, KeyCode::Char('d'));
        let other = screen(&manager, 100, 14);
        assert!(shows(&other, r"Delete the saved index for F:\beta?"));
        assert!(shows(&other, "rebuilt the next time you index it"));
        assert!(!shows(&other, "is open now"));
        press(&mut manager, KeyCode::Char('n'));

        press(&mut manager, KeyCode::Char('a'));
        let all = screen(&manager, 100, 14);
        assert!(shows(&all, "Delete all 3 saved indexes"));
        let total: u64 = ["00000000000000a1", "00000000000000b2", "00000000000000c3"]
            .iter()
            .map(|stem| home.size_of(stem))
            .sum();
        assert!(shows(&all, &format_bytes(total)));
    }

    #[test]
    fn the_layout_of_three_entries_with_a_prompt() {
        let home = Home::three();
        let mut manager = home.manager(Some(r"F:\alpha"));
        press(&mut manager, KeyCode::Down);
        press(&mut manager, KeyCode::Char('d'));
        let rows = screen(&manager, 72, 12);
        for row in &rows {
            println!("|{row}|");
        }
        assert!(shows(&rows, "Delete the saved index for F:\\beta?"));
    }

    #[test]
    fn long_paths_are_shortened_and_control_characters_never_reach_cells() {
        let home = Home::new("long");
        let long = format!("F:\\{}\\evil\u{1b}[31m\u{7}name", "deep".repeat(40));
        home.add("00000000000000d4", Some(&long), 3, 100, 10);
        let manager = home.manager(None);
        let rows = screen(&manager, 60, 12);
        assert!(shows(&rows, "…"));
        assert!(shows(&rows, "3 files"));
        assert!(rows.iter().all(|row| !row.contains(char::is_control)));
    }

    #[test]
    fn tiny_areas_never_panic() {
        let home = Home::three();
        let mut manager = home.manager(Some(r"F:\alpha"));
        for (width, height) in [(1, 1), (5, 3), (12, 4), (20, 6)] {
            screen(&manager, width, height);
        }
        press(&mut manager, KeyCode::Char('a'));
        for (width, height) in [(1, 1), (5, 3), (12, 4), (20, 6)] {
            screen(&manager, width, height);
        }
    }
}
