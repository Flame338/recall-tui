use std::{env, fs, path::PathBuf};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};

use crate::manager::IndexManager;

pub(crate) fn picker_dir(name: &str) -> PathBuf {
    let dir = env::temp_dir().join(format!("recall-picker-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

pub(crate) fn plain(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

pub(crate) fn ch(c: char) -> KeyEvent {
    plain(KeyCode::Char(c))
}

pub(crate) fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

pub(crate) fn wheel(kind: MouseEventKind) -> Event {
    Event::Mouse(MouseEvent {
        kind,
        column: 0,
        row: 0,
        modifiers: KeyModifiers::NONE,
    })
}

pub(crate) fn no_disk(root: Option<PathBuf>) -> IndexManager {
    IndexManager::open(None, root)
}
