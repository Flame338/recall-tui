use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Screen {
    Pick { start: PathBuf },
    Open { root: PathBuf },
    Search,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Picked(PathBuf),
    Back,
    Quit,
    Opened,
    OpenCancelled,
    SearchQuit,
    SearchSwitch { start: PathBuf },
}

pub fn next(screen: &Screen, event: Event, has_session: bool) -> Option<Screen> {
    match (screen, event) {
        (Screen::Pick { .. }, Event::Picked(root)) => Some(Screen::Open { root }),
        (Screen::Pick { .. }, Event::Back) | (Screen::Open { .. }, Event::OpenCancelled) => {
            has_session.then_some(Screen::Search)
        }
        (Screen::Open { .. }, Event::Opened) => Some(Screen::Search),
        (Screen::Search, Event::SearchSwitch { start }) => Some(Screen::Pick { start }),
        _ => None,
    }
}

pub fn picker_start(root: &Path) -> PathBuf {
    let plain = root
        .to_str()
        .and_then(|text| text.strip_prefix(r"\\?\"))
        .filter(|text| !text.starts_with(r"UNC\"))
        .map(PathBuf::from)
        .unwrap_or_else(|| root.to_path_buf());
    plain.parent().map(Path::to_path_buf).unwrap_or(plain)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pick() -> Screen {
        Screen::Pick {
            start: PathBuf::from("start"),
        }
    }

    fn open() -> Screen {
        Screen::Open {
            root: PathBuf::from("root"),
        }
    }

    #[test]
    fn picking_a_folder_opens_it() {
        for has_session in [false, true] {
            assert_eq!(
                next(&pick(), Event::Picked(PathBuf::from("p")), has_session),
                Some(Screen::Open {
                    root: PathBuf::from("p")
                })
            );
        }
    }

    #[test]
    fn going_back_from_the_picker_resumes_the_session_or_exits() {
        assert_eq!(next(&pick(), Event::Back, true), Some(Screen::Search));
        assert_eq!(next(&pick(), Event::Back, false), None);
    }

    #[test]
    fn quitting_the_picker_exits_even_with_a_session() {
        assert_eq!(next(&pick(), Event::Quit, true), None);
        assert_eq!(next(&pick(), Event::Quit, false), None);
    }

    #[test]
    fn a_finished_open_shows_the_search() {
        for has_session in [false, true] {
            assert_eq!(
                next(&open(), Event::Opened, has_session),
                Some(Screen::Search)
            );
        }
    }

    #[test]
    fn a_cancelled_open_resumes_the_session_or_exits() {
        assert_eq!(
            next(&open(), Event::OpenCancelled, true),
            Some(Screen::Search)
        );
        assert_eq!(next(&open(), Event::OpenCancelled, false), None);
    }

    #[test]
    fn quitting_the_search_exits() {
        for has_session in [false, true] {
            assert_eq!(next(&Screen::Search, Event::SearchQuit, has_session), None);
        }
    }

    #[test]
    fn switching_from_the_search_opens_the_picker_where_asked() {
        let event = Event::SearchSwitch {
            start: PathBuf::from("parent"),
        };
        assert_eq!(
            next(&Screen::Search, event, true),
            Some(Screen::Pick {
                start: PathBuf::from("parent")
            })
        );
    }

    #[test]
    fn events_that_do_not_belong_to_the_screen_exit() {
        let stray = [
            (pick(), Event::Opened),
            (pick(), Event::OpenCancelled),
            (pick(), Event::SearchQuit),
            (open(), Event::Picked(PathBuf::from("p"))),
            (open(), Event::Back),
            (open(), Event::Quit),
            (Screen::Search, Event::Opened),
            (Screen::Search, Event::Back),
            (Screen::Search, Event::Picked(PathBuf::from("p"))),
        ];
        for (screen, event) in stray {
            for has_session in [false, true] {
                assert_eq!(next(&screen, event.clone(), has_session), None);
            }
        }
    }

    #[test]
    fn the_picker_starts_in_the_parent_of_the_project() {
        assert_eq!(
            picker_start(Path::new("/work/alpha")),
            PathBuf::from("/work")
        );
    }

    #[test]
    fn a_project_without_a_parent_starts_the_picker_at_itself() {
        let top = std::env::current_dir()
            .unwrap()
            .ancestors()
            .last()
            .unwrap()
            .to_path_buf();
        assert_eq!(picker_start(&top), top);
    }

    #[cfg(windows)]
    #[test]
    fn the_verbatim_prefix_is_dropped_from_the_picker_start() {
        assert_eq!(
            picker_start(Path::new(r"\\?\F:\work\alpha")),
            PathBuf::from(r"F:\work")
        );
        assert_eq!(picker_start(Path::new(r"\\?\F:\")), PathBuf::from(r"F:\"));
    }
}
