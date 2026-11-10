use std::{
    env,
    error::Error,
    path::{Path, PathBuf},
};

mod cache;
mod cli;
mod daemon;
mod flow;
mod ipc;
mod live;
mod lux;
mod manager;
mod opening;
mod picker;
mod preview;
mod scan;
mod search;
mod shelf;
#[cfg(test)]
mod testing;
mod truth;
mod view;
mod watch;

use flow::Screen;
use live::Live;
use lux::DEFAULT_LIBRARY;
use opening::{Opened, open_project};
use picker::{Choice, choose_root};
use search::{App, run_tui};
use view::compact_path;

struct Session {
    live: Live,
    app: App,
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
    if env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == "search" || arg == "projects")
    {
        cli::main(&env::args().skip(1).collect::<Vec<_>>());
    }
    let (root, library_path) = parse_args()?;
    if !library_path.is_file() {
        return Err(format!(
            "LuxCore shared library not found at {}. Build LuxCore with `make shared MODE=release`, then set LUXCORE_LIB or pass --lib PATH.",
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
    let mut library = env::var_os("LUXCORE_LIB")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    while let Some(arg) = args.next() {
        if arg == "--lib" {
            library = Some(PathBuf::from(args.next().ok_or("--lib needs a path")?));
        } else if arg == "--help" || arg == "-h" {
            println!(
                "Recall — local project memory powered by LuxCore\n\nUsage: recall [ROOT] [--lib PATH]\n       recall search [--syntax] [--limit N] [--json] [--lib PATH] QUERY...\n       recall projects [--lib PATH]\n       recall daemon run|start|status|stop\n\nsearch and projects query every saved project without opening the interface. daemon manages the background indexer.\nThe library path can also come from LUXCORE_LIB."
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
