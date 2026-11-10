use std::{
    io::stdout,
    path::{Path, PathBuf},
};

use crossterm::{
    event::DisableMouseCapture,
    execute,
    terminal::{LeaveAlternateScreen, disable_raw_mode},
};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
};

use crate::{
    cache,
    lux::{MARK_END, MARK_START},
    manager::IndexManager,
};

#[derive(Clone, Copy)]
pub(crate) enum Move {
    Up(usize),
    Down(usize),
    Home,
    End,
}

pub(crate) struct TerminalCleanup;

impl Drop for TerminalCleanup {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(stdout(), LeaveAlternateScreen, DisableMouseCapture);
    }
}

pub(crate) fn open_manager(root: Option<PathBuf>) -> IndexManager {
    IndexManager::open(cache::default_dir(), root)
}

pub(crate) type Opener<'a> = &'a dyn Fn(Option<PathBuf>) -> IndexManager;

pub(crate) fn draw_manager(frame: &mut Frame, manager: &IndexManager) {
    manager.draw(frame, centered_rect(80, 70, frame.area()));
}

const TAB_WIDTH: usize = 4;

pub(crate) fn sanitize_for_display(text: &str) -> String {
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

pub(crate) fn compact_path(path: &Path, max_chars: usize) -> String {
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

pub(crate) fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
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

    #[test]
    fn tabs_expand_to_the_next_stop_and_invisibles_vanish() {
        assert_eq!(sanitize_for_display("\tx"), "    x");
        assert_eq!(sanitize_for_display("ab\tx"), "ab  x");
        assert_eq!(sanitize_for_display("\u{FEFF}a\u{FE0F}\u{200D}b\x1b"), "ab");
        assert_eq!(sanitize_for_display("naïve → ✓"), "naïve → ✓");
    }
}
