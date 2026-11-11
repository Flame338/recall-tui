use std::fs;

use super::*;
use crate::{
    lux,
    shelf::Shard,
    testing::{ch, ctrl, no_disk, picker_dir, plain, wheel},
};
use crossterm::event::MouseButton;
use ratatui::buffer::Buffer;

fn app_with_help(show_help: bool) -> App {
    let mut app = App::new(PathBuf::from("root"), 0, String::new());
    if show_help {
        app.overlay = Overlay::Help;
    }
    app
}

fn render(app: &App, index: &LuxIndex, width: u16, height: u16) -> Buffer {
    let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| draw(frame, app, index)).unwrap();
    terminal.backend().buffer().clone()
}

fn screen_text(app: &App, width: u16, height: u16) -> String {
    let index = LuxIndex::load(&lux::library_path()).expect("build libluxcore");
    text_of(&render(app, &index, width, height), width)
}

fn rig_screen(rig: &Rig, width: u16, height: u16) -> String {
    text_of(&render(&rig.app, rig.live.index(), width, height), width)
}

fn text_of(buffer: &Buffer, width: u16) -> String {
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
    for expected in ["Tab", "Ctrl-E", "Ctrl-T"] {
        assert!(hint.contains(expected), "hint is missing {expected:?}");
    }
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
        "Tab",
        "Ctrl-E",
        "Ctrl-T",
        "@name",
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
        app.preview_for.as_ref().map(|wanted| wanted.key.clone()),
        Some(selected)
    );
    let _ = fs::remove_dir_all(&dir);
}

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
        let exit = handle_event(&mut self.app, &mut self.live, event, &no_disk);
        if self.app.loading {
            load_shelf(&mut self.app, self.live.index());
        }
        exit
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

    fn shelve(&mut self, projects: &[(&str, &[(&str, &str)])]) {
        let shards = projects
            .iter()
            .map(|(name, docs)| {
                let mut index = LuxIndex::load(&lux::library_path()).expect("build libluxcore");
                for (key, body) in *docs {
                    index.add(key, key, body).unwrap();
                }
                Shard {
                    project: Project::at(format!("F:/work/{name}")),
                    index,
                }
            })
            .collect();
        self.app.shelf = Shelf::from_shards(shards);
    }

    fn shelved(name: &str) -> Rig {
        let mut rig = Rig::new(name);
        rig.shelve(&[
            (
                "alpha",
                &[
                    ("far.md", "zebra zebra zebra zebra stripes"),
                    ("note3.md", "zebra three"),
                ],
            ),
            ("beta", &[("other.md", "giraffe")]),
        ]);
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
        assert!(rescanning(&rig.app.status), "{wanted:?}");
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
        rig.app
            .preview_for
            .as_ref()
            .map(|wanted| wanted.key.clone()),
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

fn rescanning(status: &Status) -> bool {
    matches!(status, Status::Rescanning { .. })
}

fn started_long_ago(app: &mut App) {
    app.status = Status::Rescanning {
        since: Instant::now() - RESCAN_NOTICE_MIN * 2,
    };
}

#[test]
fn the_rescanning_notice_ends_with_a_rescan_that_finds_nothing() {
    let mut rig = Rig::new("pin-rescan-ends");
    rig.press(ctrl('r'));
    assert!(rescanning(&rig.app.status));
    assert!(!finish_rescan(&mut rig.app, &rig.live));
    assert!(rescanning(&rig.app.status));
    rig.tick_until_quiet();
    started_long_ago(&mut rig.app);
    assert!(finish_rescan(&mut rig.app, &rig.live));
    assert_eq!(rig.app.status.text(), "Rescan finished · 12 files indexed");
    assert!(!finish_rescan(&mut rig.app, &rig.live));
}

#[test]
fn the_notice_does_not_end_before_the_rescan_could_have_started() {
    let mut rig = Rig::new("pin-rescan-early");
    rig.press(ctrl('r'));
    rig.tick_until_quiet();
    rig.press(ctrl('r'));
    assert!(
        matches!(rig.live.mode(), live::Mode::Live { .. }) && rig.live.pending() == 0,
        "a quiet Live looks finished the moment Ctrl-R is pressed"
    );
    assert!(!finish_rescan(&mut rig.app, &rig.live));
    assert!(rescanning(&rig.app.status));
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

fn sources_of(rig: &Rig) -> Vec<Source<'_>> {
    sources(Some((&rig.app.project, rig.live.index())), &rig.app.shelf)
}

fn hit_names(rig: &Rig) -> Vec<(usize, &str)> {
    rig.app
        .hits
        .iter()
        .map(|hit| (hit.source, hit.key.as_str()))
        .collect()
}

#[test]
fn the_default_reach_ignores_the_shelf() {
    let mut rig = Rig::shelved("ui-here");
    rig.type_text("zebra");
    assert_eq!(rig.app.hits.len(), 12);
    assert!(rig.app.hits.iter().all(|hit| hit.source == HERE));
    assert!(
        rig.app.status.text().starts_with("12 results · "),
        "{}",
        rig.app.status.text()
    );
}

#[test]
fn ctrl_e_merges_other_projects_by_score_and_toggles_back() {
    let mut rig = Rig::shelved("ui-everywhere");
    rig.type_text("zebra");
    assert_eq!(rig.press(ctrl('e')), None);
    assert_eq!(rig.app.reach, Reach::Everywhere);
    assert_eq!(rig.app.hits.len(), 14);
    assert_eq!(hit_names(&rig)[0], (1, "far.md"));
    assert!(hit_names(&rig).contains(&(1, "note3.md")));
    assert!(
        rig.app
            .hits
            .windows(2)
            .all(|pair| pair[0].score >= pair[1].score)
    );
    assert!(
        rig.app
            .status
            .text()
            .starts_with("14 results across 3 projects"),
        "{}",
        rig.app.status.text()
    );
    assert_eq!(rig.press(ctrl('e')), None);
    assert_eq!(rig.app.reach, Reach::Here);
    assert_eq!(rig.app.hits.len(), 12);
    assert!(rig.app.hits.iter().all(|hit| hit.source == HERE));
    assert!(rig.app.status.text().starts_with("12 results · "));
}

#[test]
fn an_at_name_scopes_the_search_whatever_the_reach() {
    let mut rig = Rig::shelved("ui-scope");
    rig.type_text("@ALP zebra");
    assert_eq!(hit_names(&rig).len(), 2);
    assert!(rig.app.hits.iter().all(|hit| hit.source == 1));
    rig.press(ctrl('e'));
    assert_eq!(hit_names(&rig).len(), 2);
    assert!(rig.app.hits.iter().all(|hit| hit.source == 1));
    rig.press(ctrl('e'));
    rig.press(ctrl('u'));
    rig.type_text("@note zebra");
    assert!(rig.app.hits.is_empty());
}

#[test]
fn a_foreign_hit_shows_its_project_and_a_local_hit_does_not() {
    let mut rig = Rig::shelved("ui-names");
    rig.type_text("zebra");
    let here = rig_screen(&rig, 120, 30);
    assert!(!here.contains("alpha"), "{here}");
    rig.press(ctrl('e'));
    let everywhere = rig_screen(&rig, 120, 30);
    assert!(everywhere.contains("alpha › far.md"), "{everywhere}");
    assert!(everywhere.contains("alpha › note3.md"), "{everywhere}");
    assert!(everywhere.contains("Results (14)"), "{everywhere}");
    assert!(
        everywhere.lines().any(|line| line.contains("note0.md")),
        "{everywhere}"
    );
    assert!(
        !everywhere
            .lines()
            .filter(|line| line.contains("note0.md"))
            .any(|line| line.contains('›')),
        "{everywhere}"
    );
    assert!(everywhere.contains("alpha › far.md ·"), "{everywhere}");
}

#[test]
fn the_preview_title_names_the_project_of_a_foreign_hit() {
    let mut rig = Rig::shelved("ui-preview-title");
    rig.type_text("zebra");
    rig.press(ctrl('e'));
    let text = rig_screen(&rig, 120, 30);
    assert!(text.contains("┌ alpha › far.md ·"), "{text}");
    rig.press(plain(KeyCode::End));
    let text = rig_screen(&rig, 120, 30);
    assert!(!text.contains("┌ alpha ›"), "{text}");
}

#[test]
fn tab_accepts_the_ghost_and_does_nothing_without_one() {
    let mut rig = Rig::new("ui-tab");
    assert_eq!(rig.press(plain(KeyCode::Tab)), None);
    assert_eq!(rig.app.query, "");
    assert_eq!(rig.app.status.text(), "ready");
    rig.type_text("zebr");
    assert_eq!(shelf::ghost(&rig.app.query, &rig.app.suggestions), "a");
    assert_eq!(rig.press(plain(KeyCode::Tab)), None);
    assert_eq!(rig.app.query, "zebra");
    let status = rig.app.status.clone();
    assert_eq!(rig.press(plain(KeyCode::Tab)), None);
    assert_eq!(rig.app.query, "zebra");
    assert_eq!(rig.app.status, status);
}

#[test]
fn tab_does_nothing_behind_the_help_popup() {
    let mut rig = Rig::new("ui-tab-help");
    rig.type_text("zebr");
    rig.show(Shown::Help);
    rig.press(plain(KeyCode::Tab));
    assert_eq!(rig.app.query, "zebr");
}

#[test]
fn the_ghost_is_drawn_in_dark_gray_before_the_cursor() {
    let mut rig = Rig::new("ui-ghost");
    rig.type_text("zebr");
    let buffer = render(&rig.app, rig.live.index(), 120, 30);
    let row: Vec<&ratatui::buffer::Cell> = (0..120).map(|x| &buffer[(x, 1)]).collect();
    let symbols: String = row.iter().map(|cell| cell.symbol()).collect();
    assert!(symbols.contains("zebra▏"), "{symbols}");
    let at = symbols.chars().position(|c| c == 'a').unwrap();
    assert_eq!(row[at].fg, Color::DarkGray);
    assert_eq!(row[at - 1].fg, Color::White);
}

#[test]
fn ctrl_t_switches_the_mode_and_refreshes_the_preview() {
    let mut rig = Rig::searched("ui-mode");
    assert_eq!(rig.app.mode, Mode::Fuzzy);
    let before = rig.app.preview_for.clone().unwrap();
    assert_eq!(rig.press(ctrl('t')), None);
    assert_eq!(rig.app.mode, Mode::Syntax);
    let after = rig.app.preview_for.clone().unwrap();
    assert_eq!((after.source, &after.key), (before.source, &before.key));
    assert_eq!(after.mode, Mode::Syntax);
    assert_ne!(after, before);
    assert_eq!(rig.app.hits.len(), 12);
    assert_eq!(rig.press(ctrl('T')), None);
    assert_eq!(rig.app.mode, Mode::Fuzzy);
    assert_eq!(rig.app.preview_for.clone().unwrap(), before);
}

#[test]
fn a_malformed_syntax_query_marks_the_problem_and_finds_nothing() {
    let mut rig = Rig::new("ui-problem");
    rig.press(ctrl('t'));
    rig.type_text("zebra AND (zebra");
    assert!(rig.app.hits.is_empty());
    let problem = rig.app.problem.expect("the query is malformed");
    assert_eq!(problem.offset, rig.app.query.find('(').unwrap());
    assert_eq!(
        rig.app.status.text(),
        "LuxCore cannot read the query from the red mark"
    );
    let buffer = render(&rig.app, rig.live.index(), 120, 30);
    let row: Vec<&ratatui::buffer::Cell> = (0..120).map(|x| &buffer[(x, 1)]).collect();
    let symbols: String = row.iter().map(|cell| cell.symbol()).collect();
    let paren = symbols.chars().position(|c| c == '(').unwrap();
    assert_eq!(row[paren].fg, Color::Red);
    assert_eq!(row[paren - 1].fg, Color::White);
    rig.press(ctrl('t'));
    assert_eq!(rig.app.problem, None);
    assert_eq!(rig.app.mode, Mode::Fuzzy);
}

#[test]
fn a_problem_offset_inside_a_character_is_clamped() {
    let mut app = App::new(PathBuf::from("root"), 0, String::new());
    app.query = "añb".into();
    app.problem = Some(Problem { offset: 2 });
    let line = query_line(&app);
    let shown: Vec<&str> = line
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect();
    assert_eq!(shown, ["a", "ñb", "▏"]);
    app.problem = Some(Problem { offset: 99 });
    let line = query_line(&app);
    assert_eq!(line.spans[0].content, "añb");
}

#[test]
fn the_box_title_marks_the_reach_and_the_mode() {
    let mut rig = Rig::new("ui-title");
    let title = |rig: &Rig| rig_screen(rig, 160, 30).lines().next().unwrap().to_owned();
    let bare = title(&rig);
    assert!(!bare.contains("everywhere"));
    assert!(!bare.contains("syntax"));
    rig.press(ctrl('e'));
    assert!(title(&rig).contains(" · everywhere "));
    rig.press(ctrl('t'));
    assert!(title(&rig).contains(" · everywhere · syntax "));
    rig.press(ctrl('e'));
    let last = title(&rig);
    assert!(last.contains(" · syntax "));
    assert!(!last.contains("everywhere"));
}

#[test]
fn enter_targets_the_root_of_the_hits_own_project() {
    let mut rig = Rig::shelved("ui-open");
    rig.type_text("zebra");
    rig.press(ctrl('e'));
    let all = sources_of(&rig);
    let foreign = rig.app.hits[0].clone();
    assert_eq!(foreign.source, 1);
    assert_eq!(
        target_path(&all, &foreign),
        Some(PathBuf::from("F:/work/alpha").join("far.md"))
    );
    let local = rig.app.hits.iter().find(|hit| hit.source == HERE).unwrap();
    assert_eq!(target_path(&all, local), Some(rig.dir.join(&local.key)));
    let missing = Found {
        source: 9,
        key: "x".into(),
        score: 0.0,
    };
    assert_eq!(target_path(&all, &missing), None);
}

impl Rig {
    fn rescan_until_changed(&mut self) -> Vec<String> {
        self.live.rescan();
        let end = std::time::Instant::now() + std::time::Duration::from_secs(6);
        loop {
            let changed = self.live.tick().changed;
            if !changed.is_empty() {
                return changed;
            }
            assert!(
                std::time::Instant::now() < end,
                "the rescan never reached the index"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}

#[test]
fn a_live_change_keeps_a_foreign_hit_selected_by_source_and_key() {
    let mut rig = Rig::shelved("ui-live-foreign");
    rig.type_text("zebra");
    rig.press(ctrl('e'));
    let at = rig
        .app
        .hits
        .iter()
        .position(|hit| hit.source == 1 && hit.key == "note3.md")
        .unwrap();
    rig.app.selected = at;
    fs::write(
        rig.dir.join("c.md"),
        "zebra zebra zebra zebra zebra zebra zebra zebra",
    )
    .unwrap();
    let changed = rig.rescan_until_changed();
    assert_eq!(changed, ["c.md"]);
    on_live_change(&mut rig.app, &rig.live, &changed);
    assert_eq!(rig.app.hits.len(), 15);
    let kept = &rig.app.hits[rig.app.selected];
    assert_eq!((kept.source, kept.key.as_str()), (1, "note3.md"));
    assert_ne!(rig.app.selected, at);
}

#[test]
fn the_empty_query_clears_the_suggestions_and_the_problem() {
    let mut rig = Rig::new("ui-clear");
    rig.press(ctrl('t'));
    rig.type_text("(zebra");
    assert!(rig.app.problem.is_some());
    rig.press(plain(KeyCode::Esc));
    assert_eq!(rig.app.problem, None);
    assert!(rig.app.suggestions.is_empty());
    assert_eq!(rig.app.mode, Mode::Syntax);
}

fn status_of(rig: &Rig) -> &str {
    rig.app.status.text()
}

#[test]
fn a_scope_that_matches_nothing_says_so_with_or_without_query_text() {
    let mut rig = Rig::shelved("status-nomatch");
    rig.type_text("@zzz @Yyy");
    assert_eq!(status_of(&rig), "No saved project matches @zzz @yyy");
    rig.type_text(" zebra");
    assert_eq!(status_of(&rig), "No saved project matches @zzz @yyy");
    assert!(rig.app.hits.is_empty());
}

#[test]
fn scopes_without_query_text_name_the_projects_they_pick() {
    let mut rig = Rig::shelved("status-scope-only");
    rig.type_text("@alp");
    assert_eq!(status_of(&rig), "Searching alpha · type a query");
    rig.type_text(" @bet");
    assert_eq!(status_of(&rig), "Searching alpha, beta · type a query");
}

#[test]
fn a_search_of_one_other_project_names_it() {
    let mut rig = Rig::shelved("status-other");
    rig.type_text("@alp zebra");
    assert_eq!(
        status_of(&rig),
        "2 results in alpha · Enter opens selected file"
    );
}

#[test]
fn a_search_of_only_the_open_project_keeps_its_file_count() {
    let mut rig = Rig::shelved("status-here");
    rig.type_text("@recall zebra");
    assert_eq!(
        status_of(&rig),
        "12 results · 12 indexed · Enter opens selected file"
    );
    rig.press(ctrl('u'));
    rig.type_text("zebra");
    assert_eq!(
        status_of(&rig),
        "12 results · 12 indexed · Enter opens selected file"
    );
    rig.press(ctrl('u'));
    assert_eq!(status_of(&rig), "Indexed 12 files · type a query");
}

#[test]
fn a_search_of_several_projects_counts_them() {
    let mut rig = Rig::shelved("status-many");
    rig.press(ctrl('e'));
    rig.type_text("zebra");
    assert_eq!(
        status_of(&rig),
        "14 results across 3 projects · Enter opens selected file"
    );
}

fn status_sources() -> (Shard, Shelf) {
    let blank = |name: &str| Shard {
        project: Project::at(format!("F:/work/{name}")),
        index: LuxIndex::load(&lux::library_path()).expect("build libluxcore"),
    };
    (
        blank("here"),
        Shelf::from_shards(vec![blank("alpha"), blank("beta")]),
    )
}

fn outcome_of(found: usize, searched: &[usize], failed: &[usize]) -> shelf::Outcome {
    shelf::Outcome {
        found: (0..found)
            .map(|n| Found {
                source: searched.first().copied().unwrap_or(HERE),
                key: format!("{n}.md"),
                score: 1.0,
            })
            .collect(),
        problem: None,
        searched: searched.to_vec(),
        failed: failed.to_vec(),
    }
}

#[test]
fn the_status_says_how_many_projects_could_not_be_searched() {
    let (here, shelf) = status_sources();
    let all = sources(Some((&here.project, &here.index)), &shelf);
    let request = parse_request("zebra", Mode::Fuzzy, Reach::Everywhere);
    let text = |outcome: &shelf::Outcome| {
        let scoped: Vec<&str> = outcome
            .searched
            .iter()
            .map(|position| all[*position].project.name.as_str())
            .collect();
        status_text(&request, outcome, &all, &scoped, 7)
    };
    assert_eq!(
        text(&outcome_of(3, &[0, 1, 2], &[2])),
        "3 results across 3 projects · Enter opens selected file · 1 could not be searched"
    );
    assert_eq!(
        text(&outcome_of(0, &[1], &[1])),
        "0 results in alpha · Enter opens selected file · 1 could not be searched"
    );
    assert_eq!(
        text(&outcome_of(2, &[0, 1, 2], &[1, 2])),
        "2 results across 3 projects · Enter opens selected file · 2 could not be searched"
    );
    assert_eq!(
        text(&outcome_of(1, &[0, 1], &[])),
        "1 results across 2 projects · Enter opens selected file"
    );
}

#[test]
fn a_query_problem_wins_over_the_counts() {
    let (here, shelf) = status_sources();
    let all = sources(Some((&here.project, &here.index)), &shelf);
    let request = parse_request("(", Mode::Syntax, Reach::Everywhere);
    let mut outcome = outcome_of(0, &[0, 1, 2], &[]);
    outcome.problem = Some(Problem { offset: 0 });
    let scoped = ["here", "alpha", "beta"];
    assert_eq!(status_text(&request, &outcome, &all, &scoped, 7), BAD_QUERY);
}

#[test]
fn ctrl_e_on_an_empty_query_says_what_it_now_searches() {
    let mut rig = Rig::shelved("reach-status");
    rig.press(ctrl('e'));
    assert_eq!(
        status_of(&rig),
        "Searching every saved project · 2 other projects"
    );
    rig.press(ctrl('e'));
    assert_eq!(status_of(&rig), "Searching this project only");
}

#[test]
fn one_other_project_is_counted_in_the_singular() {
    assert_eq!(
        reach_status(Reach::Everywhere, 1),
        "Searching every saved project · 1 other project"
    );
}

#[test]
fn ctrl_e_with_nothing_on_the_shelf_says_so() {
    let mut rig = Rig::new("reach-empty");
    let empty = "No other saved projects yet · open one in Recall to search it from here";
    rig.press(ctrl('e'));
    assert_eq!(status_of(&rig), empty);
    rig.press(ctrl('e'));
    assert_eq!(status_of(&rig), empty);
}

#[test]
fn ctrl_e_with_a_query_keeps_reporting_results() {
    let mut rig = Rig::shelved("reach-query");
    rig.type_text("zebra");
    rig.press(ctrl('e'));
    assert!(status_of(&rig).starts_with("14 results across 3 projects"));
}

#[test]
fn only_a_file_that_exists_is_launched() {
    let dir = picker_dir("launch-target");
    fs::write(dir.join("here.md"), "zebra").unwrap();
    assert_eq!(
        launch_target(dir.join("here.md"), "here.md"),
        Ok(dir.join("here.md"))
    );
    assert_eq!(
        launch_target(dir.join("gone.md"), "gone.md"),
        Err("File is gone: gone.md".to_owned())
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn enter_on_a_hit_whose_file_is_gone_says_so_instead_of_opening() {
    let mut rig = Rig::shelved("open-gone");
    rig.type_text("zebra");
    rig.press(ctrl('e'));
    assert_eq!(rig.app.hits[0].key, "far.md");
    rig.press(plain(KeyCode::Enter));
    assert_eq!(status_of(&rig), "File is gone: far.md");
}

fn save_project(cache_dir: &Path, root: &Path, body: &str) {
    let mut index = LuxIndex::load(&lux::library_path()).expect("build libluxcore");
    index.add("a.md", "a.md", body).unwrap();
    fs::write(root.join("a.md"), body).unwrap();
    let files = crate::cache::cache_files(cache_dir, root);
    let fingerprint = crate::cache::fingerprint(&lux::library_path());
    crate::cache::save(&index, &files, &fingerprint, None, &Default::default()).unwrap();
}

#[test]
fn ctrl_r_reloads_the_other_projects_and_reruns_the_query() {
    let mut rig = Rig::new("reload-ui");
    let base = picker_dir("reload-ui-saved");
    let cache = base.join("cache");
    let alpha = base.join("alpha");
    let zeta = base.join("zeta");
    for dir in [&cache, &alpha, &zeta] {
        fs::create_dir_all(dir).unwrap();
    }
    save_project(&cache, &alpha, "zebra alpha");
    save_project(&cache, &zeta, "zebra zeta");
    rig.app.shelf = Shelf::list(&cache, &lux::library_path(), None);
    rig.press(ctrl('e'));
    rig.type_text("zebra");
    assert_eq!(rig.app.hits.len(), 14);
    rig.app.selected = rig.app.hits.iter().position(|hit| hit.source == 2).unwrap();

    fs::remove_dir_all(&zeta).unwrap();
    assert_eq!(rig.press(ctrl('r')), None);
    assert_eq!(rig.app.shelf.len(), 1);
    assert_eq!(rig.app.hits.len(), 13);
    assert!(rig.app.hits.iter().all(|hit| hit.source <= 1));
    assert!(rig.app.selected < rig.app.hits.len());
    assert!(rescanning(&rig.app.status));
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn ctrl_r_rebuilds_the_preview_of_a_hit_whose_source_position_now_holds_another_project() {
    let mut rig = Rig::new("reload-preview");
    let base = picker_dir("reload-preview-saved");
    let cache = base.join("cache");
    let first = base.join("pa_x");
    let second = base.join("pb_x");
    for dir in [&cache, &first, &second] {
        fs::create_dir_all(dir).unwrap();
    }
    save_project(&cache, &first, "zebra firstword");
    save_project(&cache, &second, "zebra secondword");
    rig.app.shelf = Shelf::list(&cache, &lux::library_path(), None);
    rig.type_text("@_x zebra");
    let preview = |rig: &Rig| {
        rig.app
            .preview
            .iter()
            .map(|line| line.to_string())
            .collect::<String>()
    };
    rig.app.selected = rig.app.hits.iter().position(|hit| hit.source == 1).unwrap();
    rig.press(plain(KeyCode::Down));
    rig.press(plain(KeyCode::Up));
    assert!(preview(&rig).contains("firstword"), "{}", preview(&rig));

    fs::remove_dir_all(&first).unwrap();
    rig.press(ctrl('r'));
    assert_eq!(rig.app.hits.len(), 1);
    assert_eq!(rig.app.hits[0].source, 1);
    assert!(preview(&rig).contains("secondword"), "{}", preview(&rig));
    let _ = fs::remove_dir_all(&base);
}

fn saved_rig(name: &str) -> (Rig, PathBuf) {
    let mut rig = Rig::new(name);
    let base = picker_dir(&format!("{name}-saved"));
    let cache = base.join("cache");
    for (project, body) in [("alpha", "zebra alpha"), ("beta", "zebra beta")] {
        let root = base.join(project);
        fs::create_dir_all(&root).unwrap();
        save_project(&cache, &root, body);
    }
    rig.app.shelf = Shelf::list(&cache, &lux::library_path(), None);
    (rig, base)
}

#[test]
fn a_scope_alone_names_saved_projects_without_reading_them() {
    let (mut rig, base) = saved_rig("lazy-scope");
    rig.type_text("@alp");
    assert_eq!(status_of(&rig), "Searching alpha · type a query");
    assert!(rig.app.shelf.loaded().is_empty());

    rig.type_text("ha zebra");
    assert_eq!(rig.app.shelf.loaded(), ["alpha"]);
    assert_eq!(
        status_of(&rig),
        "1 results in alpha · Enter opens selected file"
    );
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn ctrl_e_says_it_is_loading_before_it_reads_the_saved_projects() {
    let (mut rig, base) = saved_rig("lazy-loading");
    rig.type_text("zebra");
    assert!(rig.app.shelf.loaded().is_empty());
    handle_event(&mut rig.app, &mut rig.live, Event::Key(ctrl('e')), &no_disk);
    assert_eq!(status_of(&rig), "Loading 2 saved projects…");
    assert_eq!(rig.app.hits.len(), 12);
    load_shelf(&mut rig.app, rig.live.index());
    assert_eq!(rig.app.shelf.loaded(), ["alpha", "beta"]);
    assert_eq!(
        status_of(&rig),
        "14 results across 3 projects · Enter opens selected file"
    );
    let _ = fs::remove_dir_all(&base);
}
