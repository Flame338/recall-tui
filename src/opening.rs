use std::{
    error::Error,
    io::stdout,
    path::{Path, PathBuf},
    time::Duration,
};

use crossterm::{
    event::{self, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    text::Line,
    widgets::{Block, Borders, Gauge, Paragraph, Wrap},
};

use crate::{
    App, Session, TerminalCleanup, compact_path,
    live::{Live, Mode, Progress, Summary},
    sanitize_for_display,
};

pub enum Opened {
    Ready(Box<Session>),
    Cancelled,
}

pub fn open_project(root: PathBuf, library_path: &Path) -> Result<Opened, Box<dyn Error>> {
    let root = root.canonicalize()?;
    if !root.is_dir() {
        return Err(format!("Not a directory: {}", root.display()).into());
    }
    enable_raw_mode()?;
    let _cleanup = TerminalCleanup;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen, EnableMouseCapture)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(out))?;
    terminal.draw(|frame| draw_scanning(frame, &root, 0))?;

    let mut live = Live::open(&root, library_path)?;
    let finished = live.settle(|progress| {
        if cancel_requested()? {
            return Ok(false);
        }
        terminal.draw(|frame| draw_progress(frame, &root, &progress))?;
        Ok(true)
    })?;
    if !finished {
        return Ok(Opened::Cancelled);
    }
    let status = format!("{} · F1 help", describe_startup(&live.summary()));
    let app = App::new(root, live.indexed_files(), status);
    Ok(Opened::Ready(Box::new(Session { live, app })))
}

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

fn draw_progress(frame: &mut Frame, root: &Path, progress: &Progress) {
    match progress.mode {
        Mode::Scanning { found } => draw_scanning(frame, root, *found),
        _ if progress.total == 0 => draw_scanning(frame, root, 0),
        _ => draw_indexing(frame, root, progress.done, progress.total, progress.current),
    }
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

fn draw_scanning(frame: &mut Frame, root: &Path, found: usize) {
    let area = indexing_layout(frame, root);
    frame.render_widget(
        Paragraph::new(format!("{found} files found so far")).block(
            Block::default()
                .title(" Scanning folders ")
                .borders(Borders::ALL),
        ),
        area[1],
    );
    draw_detail(frame, area[2], "Looking for files…");
}

fn draw_indexing(frame: &mut Frame, root: &Path, completed: usize, total: usize, current: &str) {
    let area = indexing_layout(frame, root);
    let ratio = (completed as f64 / total as f64).clamp(0.0, 1.0);
    let gauge = Gauge::default()
        .block(Block::default().title(" Progress ").borders(Borders::ALL))
        .gauge_style(Style::default().fg(Color::Cyan).bg(Color::DarkGray))
        .ratio(ratio)
        .label(format!(
            "{:.0}% · {completed} / {total} files",
            ratio * 100.0
        ));
    frame.render_widget(gauge, area[1]);

    let detail = if current.is_empty() {
        "Starting index…".to_owned()
    } else {
        format!("Indexing {current}")
    };
    draw_detail(frame, area[2], &detail);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(reused: bool, updated: usize, removed: usize, saved: bool) -> Summary {
        Summary {
            indexed: 12,
            reused,
            updated,
            removed,
            saved,
        }
    }

    #[test]
    fn the_startup_message_says_what_the_open_did() {
        assert_eq!(
            describe_startup(&summary(false, 12, 0, true)),
            "Indexed 12 files · cached for next time"
        );
        assert_eq!(
            describe_startup(&summary(false, 12, 0, false)),
            "Indexed 12 files · cache unavailable"
        );
        assert_eq!(
            describe_startup(&summary(true, 0, 0, false)),
            "Loaded 12 files from cache"
        );
        assert_eq!(
            describe_startup(&summary(true, 3, 1, false)),
            "Cache refreshed: 3 changed, 1 removed · 12 files"
        );
    }
}
