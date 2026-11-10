use std::{error::Error, fs, io::stdout, mem, path::PathBuf};

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
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
};

use crate::{
    manager::{IndexManager, Outcome},
    view::{Move, Opener, TerminalCleanup, draw_manager, open_manager},
};

struct DirectoryPicker {
    current: PathBuf,
    entries: Vec<PickerEntry>,
    selected: usize,
    mode: PickerMode,
    status: String,
}

enum PickerMode {
    Browsing,
    Typing(String),
    Manager {
        manager: IndexManager,
        covering: Covered,
    },
}

enum Covered {
    Browsing,
    Typing(String),
}

impl From<Covered> for PickerMode {
    fn from(covered: Covered) -> Self {
        match covered {
            Covered::Browsing => PickerMode::Browsing,
            Covered::Typing(text) => PickerMode::Typing(text),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Choice {
    Folder(PathBuf),
    Back,
    Quit,
}

enum PickerEntry {
    Parent(PathBuf),
    Folder(PathBuf),
    Drive(PathBuf),
}

pub(crate) fn choose_root(start: PathBuf) -> Result<Choice, Box<dyn Error>> {
    let mut picker = DirectoryPicker::new(start);

    enable_raw_mode()?;
    let _cleanup = TerminalCleanup;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(out);
    let mut terminal = Terminal::new(backend)?;

    loop {
        terminal.draw(|frame| draw_directory_picker(frame, &picker))?;
        if let Some(choice) = picker_event(&mut picker, event::read()?, &open_manager) {
            return Ok(choice);
        }
    }
}

fn picker_event(picker: &mut DirectoryPicker, event: Event, open: Opener) -> Option<Choice> {
    match event {
        Event::Key(key) if key.kind == KeyEventKind::Press => picker.key(key, open),
        Event::Mouse(mouse) => {
            if !matches!(picker.mode, PickerMode::Manager { .. }) {
                match mouse.kind {
                    MouseEventKind::ScrollUp => picker.go(Move::Up(3)),
                    MouseEventKind::ScrollDown => picker.go(Move::Down(3)),
                    _ => {}
                }
            }
            None
        }
        _ => None,
    }
}

impl DirectoryPicker {
    fn new(current: PathBuf) -> Self {
        let mut picker = Self {
            current,
            entries: Vec::new(),
            selected: 0,
            mode: PickerMode::Browsing,
            status: "Choose a folder to index".into(),
        };
        picker.refresh();
        picker
    }

    fn typed_path(&self) -> Option<&str> {
        match &self.mode {
            PickerMode::Typing(text)
            | PickerMode::Manager {
                covering: Covered::Typing(text),
                ..
            } => Some(text),
            PickerMode::Browsing
            | PickerMode::Manager {
                covering: Covered::Browsing,
                ..
            } => None,
        }
    }

    fn key(&mut self, key: KeyEvent, open: Opener) -> Option<Choice> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let mode = mem::replace(&mut self.mode, PickerMode::Browsing);
        let (mode, choice) = match (mode, key.code) {
            (mode, KeyCode::Char('q' | 'Q')) if ctrl => (mode, Some(Choice::Quit)),
            (
                PickerMode::Manager {
                    mut manager,
                    covering,
                },
                _,
            ) => {
                let mode = match manager.handle_key(key) {
                    Outcome::Close => covering.into(),
                    Outcome::Stay => PickerMode::Manager { manager, covering },
                };
                (mode, None)
            }
            (PickerMode::Browsing, KeyCode::F(2)) => (
                PickerMode::Manager {
                    manager: open(None),
                    covering: Covered::Browsing,
                },
                None,
            ),
            (PickerMode::Typing(text), KeyCode::F(2)) => (
                PickerMode::Manager {
                    manager: open(None),
                    covering: Covered::Typing(text),
                },
                None,
            ),
            (PickerMode::Typing(text), _) => (self.type_path(text, key), None),
            (PickerMode::Browsing, _) => self.browse(key),
        };
        self.mode = mode;
        choice
    }

    fn type_path(&mut self, mut text: String, key: KeyEvent) -> PickerMode {
        match key.code {
            KeyCode::Esc => return PickerMode::Browsing,
            KeyCode::Enter => match PathBuf::from(text.trim()).canonicalize() {
                Ok(path) if path.is_dir() => {
                    self.current = path;
                    self.refresh();
                    return PickerMode::Browsing;
                }
                Ok(_) => self.status = "That path is not a folder".into(),
                Err(error) => self.status = format!("Cannot open that path: {error}"),
            },
            KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => text.clear(),
            KeyCode::Backspace => {
                text.pop();
            }
            KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => text.push(ch),
            _ => {}
        }
        PickerMode::Typing(text)
    }

    fn browse(&mut self, key: KeyEvent) -> (PickerMode, Option<Choice>) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q' | 'Q') => {
                return (PickerMode::Browsing, Some(Choice::Back));
            }
            KeyCode::Char('p' | 'P') => {
                self.status = "Edit the folder path, then press Enter".into();
                let text = self.current.to_string_lossy().into_owned();
                return (PickerMode::Typing(text), None);
            }
            KeyCode::Char('s' | 'S' | ' ') => {
                return (
                    PickerMode::Browsing,
                    Some(Choice::Folder(self.current.clone())),
                );
            }
            KeyCode::Backspace | KeyCode::Left => {
                if let Some(parent) = self.current.parent() {
                    self.current = parent.to_path_buf();
                    self.refresh();
                }
            }
            KeyCode::Up => self.go(Move::Up(1)),
            KeyCode::Down => self.go(Move::Down(1)),
            KeyCode::Enter => {
                if let Some(entry) = self.entries.get(self.selected) {
                    self.current = match entry {
                        PickerEntry::Parent(path)
                        | PickerEntry::Folder(path)
                        | PickerEntry::Drive(path) => path.clone(),
                    };
                    self.refresh();
                }
            }
            _ => {}
        }
        (PickerMode::Browsing, None)
    }

    fn go(&mut self, step: Move) {
        let last = self.entries.len().saturating_sub(1);
        self.selected = match step {
            Move::Up(by) => self.selected.saturating_sub(by),
            Move::Down(by) => (self.selected + by).min(last),
            Move::Home => 0,
            Move::End => last,
        };
    }

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
    let path_label = match picker.typed_path() {
        Some(typed) => format!(" Folder path (editing) · {typed}▏ "),
        None => format!(
            " Current folder · {} ",
            picker.current.to_str().unwrap_or("")
        ),
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
    if let PickerMode::Manager { manager, .. } = &picker.mode {
        draw_manager(frame, manager);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{ch, ctrl, no_disk, picker_dir, plain, wheel};

    fn picker_in(path: PathBuf) -> DirectoryPicker {
        DirectoryPicker::new(path)
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

    fn picker_rig(name: &str) -> (DirectoryPicker, PathBuf) {
        let dir = picker_dir(name);
        fs::create_dir_all(dir.join("alpha")).unwrap();
        fs::create_dir_all(dir.join("beta")).unwrap();
        fs::write(dir.join("file.txt"), "x").unwrap();
        (picker_in(dir.clone()), dir)
    }

    fn pick(picker: &mut DirectoryPicker, event: Event) -> Option<Choice> {
        picker_event(picker, event, &no_disk)
    }

    fn pressed(picker: &mut DirectoryPicker, key: KeyEvent) -> Option<Choice> {
        pick(picker, Event::Key(key))
    }

    fn picker_mode(picker: &DirectoryPicker) -> &'static str {
        match picker.mode {
            PickerMode::Browsing => "browsing",
            PickerMode::Typing(_) => "typing",
            PickerMode::Manager { .. } => "manager",
        }
    }

    #[test]
    fn the_picker_quits_on_ctrl_q_from_every_mode() {
        let (mut picker, dir) = picker_rig("pin-picker-quit");
        assert_eq!(pressed(&mut picker, ctrl('q')), Some(Choice::Quit));
        pressed(&mut picker, ch('p'));
        assert_eq!(picker_mode(&picker), "typing");
        assert_eq!(pressed(&mut picker, ctrl('q')), Some(Choice::Quit));
        pressed(&mut picker, plain(KeyCode::F(2)));
        assert_eq!(picker_mode(&picker), "manager");
        assert_eq!(pressed(&mut picker, ctrl('Q')), Some(Choice::Quit));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn esc_and_q_go_back_from_browsing_and_s_or_space_pick_the_folder() {
        let (mut picker, dir) = picker_rig("pin-picker-browse");
        for key in [plain(KeyCode::Esc), ch('q'), ch('Q')] {
            assert_eq!(pressed(&mut picker, key), Some(Choice::Back));
        }
        for key in [ch('s'), ch('S'), ch(' ')] {
            assert_eq!(pressed(&mut picker, key), Some(Choice::Folder(dir.clone())));
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn browsing_letters_ignore_modifiers_so_ctrl_s_picks_and_ctrl_p_edits() {
        let (mut picker, dir) = picker_rig("pin-picker-ctrl-letters");
        assert_eq!(
            pressed(&mut picker, ctrl('s')),
            Some(Choice::Folder(dir.clone()))
        );
        assert_eq!(pressed(&mut picker, ctrl('p')), None);
        assert_eq!(picker_mode(&picker), "typing");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_picker_moves_enters_folders_and_walks_up() {
        let (mut picker, dir) = picker_rig("pin-picker-move");
        assert_eq!(picker.selected, 1);
        pressed(&mut picker, plain(KeyCode::Down));
        assert_eq!(picker.selected, 2);
        pressed(&mut picker, plain(KeyCode::Down));
        assert_eq!(picker.selected, 2);
        pressed(&mut picker, plain(KeyCode::Up));
        pressed(&mut picker, plain(KeyCode::Up));
        pressed(&mut picker, plain(KeyCode::Up));
        assert_eq!(picker.selected, 0);
        pressed(&mut picker, plain(KeyCode::Down));
        pressed(&mut picker, plain(KeyCode::Enter));
        assert_eq!(picker.current, dir.join("alpha"));
        pressed(&mut picker, plain(KeyCode::Left));
        assert_eq!(picker.current, dir);
        pressed(&mut picker, plain(KeyCode::Down));
        pressed(&mut picker, plain(KeyCode::Enter));
        assert_eq!(picker.current, dir.join("beta"));
        pressed(&mut picker, plain(KeyCode::Backspace));
        assert_eq!(picker.current, dir);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_picker_wheel_moves_by_three_and_clamps() {
        let (mut picker, dir) = picker_rig("pin-picker-wheel");
        pick(&mut picker, wheel(MouseEventKind::ScrollDown));
        assert_eq!(picker.selected, 2);
        pick(&mut picker, wheel(MouseEventKind::ScrollUp));
        assert_eq!(picker.selected, 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_picker_wheel_is_ignored_under_the_manager_but_not_while_typing() {
        let (mut picker, dir) = picker_rig("pin-picker-wheel-modes");
        pressed(&mut picker, ch('p'));
        pick(&mut picker, wheel(MouseEventKind::ScrollDown));
        assert_eq!(picker.selected, 2);
        pick(&mut picker, wheel(MouseEventKind::ScrollUp));
        pressed(&mut picker, plain(KeyCode::F(2)));
        pick(&mut picker, wheel(MouseEventKind::ScrollDown));
        assert_eq!(picker.selected, 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn p_starts_editing_the_current_path_and_keys_edit_it() {
        let (mut picker, dir) = picker_rig("pin-picker-edit");
        assert_eq!(pressed(&mut picker, ch('P')), None);
        assert_eq!(picker.typed_path(), Some(dir.to_string_lossy().as_ref()));
        assert_eq!(picker.status, "Edit the folder path, then press Enter");
        pressed(&mut picker, plain(KeyCode::Backspace));
        pressed(&mut picker, ch('q'));
        pressed(&mut picker, ch('s'));
        let full = dir.to_string_lossy();
        let typed = format!("{}qs", &full[..full.len() - 1]);
        assert_eq!(picker.typed_path(), Some(typed.as_str()));
        pressed(&mut picker, ctrl('a'));
        assert_eq!(picker.typed_path(), Some(""));
        pressed(&mut picker, ctrl('x'));
        pressed(&mut picker, plain(KeyCode::Tab));
        assert_eq!(picker.typed_path(), Some(""));
        assert_eq!(picker_mode(&picker), "typing");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn esc_in_the_typed_path_leaves_typing_without_going_back() {
        let (mut picker, dir) = picker_rig("pin-picker-edit-esc");
        pressed(&mut picker, ch('p'));
        assert_eq!(pressed(&mut picker, plain(KeyCode::Esc)), None);
        assert_eq!(picker_mode(&picker), "browsing");
        assert_eq!(picker.current, dir);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn enter_on_a_typed_folder_goes_there_and_stops_typing() {
        let (mut picker, dir) = picker_rig("pin-picker-edit-go");
        pressed(&mut picker, ch('p'));
        pressed(&mut picker, ctrl('a'));
        for c in dir.join("alpha").to_string_lossy().chars() {
            pressed(&mut picker, ch(c));
        }
        assert_eq!(pressed(&mut picker, plain(KeyCode::Enter)), None);
        assert_eq!(picker_mode(&picker), "browsing");
        assert_eq!(picker.current.file_name().unwrap(), "alpha");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn enter_on_a_typed_file_or_missing_path_explains_and_keeps_typing() {
        let (mut picker, dir) = picker_rig("pin-picker-edit-bad");
        pressed(&mut picker, ch('p'));
        pressed(&mut picker, ctrl('a'));
        for c in dir.join("file.txt").to_string_lossy().chars() {
            pressed(&mut picker, ch(c));
        }
        pressed(&mut picker, plain(KeyCode::Enter));
        assert_eq!(picker.status, "That path is not a folder");
        assert_eq!(picker_mode(&picker), "typing");
        pressed(&mut picker, ctrl('a'));
        for c in dir.join("missing").to_string_lossy().chars() {
            pressed(&mut picker, ch(c));
        }
        pressed(&mut picker, plain(KeyCode::Enter));
        assert!(
            picker.status.starts_with("Cannot open that path"),
            "{}",
            picker.status
        );
        assert_eq!(picker_mode(&picker), "typing");
        assert_eq!(picker.current, dir);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_picker_manager_swallows_keys_and_closes_on_its_closing_keys() {
        let (mut picker, dir) = picker_rig("pin-picker-manager");
        for closing in [plain(KeyCode::Esc), ch('q'), plain(KeyCode::F(2))] {
            assert_eq!(pressed(&mut picker, plain(KeyCode::F(2))), None);
            assert_eq!(picker_mode(&picker), "manager");
            for key in [
                ch('s'),
                ch('p'),
                plain(KeyCode::Enter),
                plain(KeyCode::Down),
            ] {
                assert_eq!(pressed(&mut picker, key), None, "{key:?}");
                assert_eq!(picker_mode(&picker), "manager");
            }
            assert_eq!(picker.current, dir);
            assert_eq!(picker.selected, 1);
            assert_eq!(pressed(&mut picker, closing), None);
            assert_eq!(picker_mode(&picker), "browsing");
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn f2_works_while_typing_and_the_typed_path_survives_the_manager() {
        let (mut picker, dir) = picker_rig("pin-picker-manager-typing");
        pressed(&mut picker, ch('p'));
        pressed(&mut picker, ch('z'));
        let typed = picker.typed_path().map(str::to_owned);
        pressed(&mut picker, plain(KeyCode::F(2)));
        assert_eq!(picker_mode(&picker), "manager");
        pressed(&mut picker, ch('x'));
        pressed(&mut picker, plain(KeyCode::Esc));
        assert_eq!(picker_mode(&picker), "typing");
        assert_eq!(picker.typed_path().map(str::to_owned), typed);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_picker_ignores_key_releases() {
        let (mut picker, dir) = picker_rig("pin-picker-release");
        let release = KeyEvent::new_with_kind(
            KeyCode::Char('q'),
            KeyModifiers::NONE,
            KeyEventKind::Release,
        );
        assert_eq!(pressed(&mut picker, release), None);
        let _ = fs::remove_dir_all(&dir);
    }
}
