use std::{
    env,
    path::{Path, PathBuf},
};

use crate::{
    cache,
    lux::{DEFAULT_LIBRARY, MARK_END, MARK_START},
    shelf::{self, Mode, Outcome, Reach, Shelf, Source},
    view::plain_path,
};

const DEFAULT_LIMIT: usize = 10;
const SNIPPET_LENGTH: usize = 100;
const USAGE: &str = "usage: recall search [--syntax] [--limit N] [--json] [--lib PATH] QUERY...\n       recall projects [--lib PATH]";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchOptions {
    pub query: String,
    pub mode: Mode,
    pub limit: usize,
    pub json: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    Search(SearchOptions),
    Projects,
    Help,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invocation {
    pub command: Command,
    pub library: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    Matched,
    Nothing,
    Usage,
}

impl Exit {
    pub fn code(self) -> i32 {
        match self {
            Exit::Matched => 0,
            Exit::Nothing => 1,
            Exit::Usage => 2,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    pub stdout: String,
    pub stderr: String,
    pub exit: Exit,
}

impl Report {
    fn out(stdout: String, exit: Exit) -> Self {
        Self {
            stdout,
            stderr: String::new(),
            exit,
        }
    }

    fn err(stderr: String, exit: Exit) -> Self {
        Self {
            stdout: String::new(),
            stderr,
            exit,
        }
    }
}

fn value_of<'a>(
    flag: &str,
    rest: &mut impl Iterator<Item = &'a String>,
) -> Result<&'a String, String> {
    rest.next().ok_or_else(|| format!("{flag} needs a value"))
}

fn is_help(arg: &str) -> bool {
    matches!(arg, "--help" | "-h")
}

fn help() -> Invocation {
    Invocation {
        command: Command::Help,
        library: None,
    }
}

pub fn parse(args: &[String]) -> Result<Invocation, String> {
    let (name, rest) = args.split_first().ok_or("missing command")?;
    let mut rest = rest.iter();
    let mut library = None;
    match name.as_str() {
        "projects" => {
            while let Some(arg) = rest.next() {
                if arg == "--lib" {
                    library = Some(PathBuf::from(value_of("--lib", &mut rest)?));
                } else if is_help(arg) {
                    return Ok(help());
                } else {
                    return Err(format!("projects does not take {arg}"));
                }
            }
            Ok(Invocation {
                command: Command::Projects,
                library,
            })
        }
        "search" => {
            let mut mode = Mode::Fuzzy;
            let mut limit = DEFAULT_LIMIT;
            let mut json = false;
            let mut words: Vec<&str> = Vec::new();
            while let Some(arg) = rest.next() {
                match arg.as_str() {
                    "--syntax" => mode = Mode::Syntax,
                    "--json" => json = true,
                    "--lib" => library = Some(PathBuf::from(value_of("--lib", &mut rest)?)),
                    "--limit" => {
                        let value = value_of("--limit", &mut rest)?;
                        limit = value
                            .parse::<usize>()
                            .ok()
                            .filter(|limit| *limit > 0)
                            .ok_or_else(|| {
                                format!("--limit needs a positive number, not {value:?}")
                            })?;
                    }
                    "--" => words.extend(rest.by_ref().map(String::as_str)),
                    flag if is_help(flag) => return Ok(help()),
                    flag if flag.starts_with("--") => {
                        return Err(format!("unknown option {flag}"));
                    }
                    word => words.push(word),
                }
            }
            let query = words.join(" ");
            if shelf::parse_request(&query, mode, Reach::Everywhere)
                .text
                .trim()
                .is_empty()
            {
                return Err("search needs a query".into());
            }
            Ok(Invocation {
                command: Command::Search(SearchOptions {
                    query,
                    mode,
                    limit,
                    json,
                }),
                library,
            })
        }
        other => Err(format!("unknown command {other}")),
    }
}

pub fn resolve_library(flag: Option<PathBuf>) -> PathBuf {
    flag.or_else(|| {
        env::var_os("LUXCORE_LIB")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    })
    .unwrap_or_else(|| PathBuf::from(DEFAULT_LIBRARY))
}

pub fn missing_library(library: &Path) -> String {
    format!(
        "LuxCore shared library not found at {}. Build LuxCore with `make shared MODE=release`, then set LUXCORE_LIB or pass --lib PATH.",
        library.display()
    )
}

fn plain_root(root: &Path) -> String {
    plain_path(&root.to_string_lossy()).into_owned()
}

fn one_line(marked: &str) -> String {
    let bare: String = marked
        .chars()
        .filter(|c| *c != MARK_START && *c != MARK_END)
        .collect();
    bare.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

pub fn render_text(sources: &[Source], request: &shelf::Request, outcome: &Outcome) -> String {
    let mut out = String::new();
    for found in &outcome.found {
        let source = &sources[found.source];
        out.push_str(&format!(
            "{:>8.1}  {}/{}\n",
            found.score, source.project.name, found.key
        ));
        let line = shelf::snippet(source, &found.key, request, SNIPPET_LENGTH)
            .map(|marked| one_line(&marked))
            .unwrap_or_default();
        if !line.is_empty() {
            out.push_str(&format!("          {line}\n"));
        }
    }
    out
}

pub fn render_json(sources: &[Source], outcome: &Outcome) -> String {
    if outcome.found.is_empty() {
        return "[]\n".into();
    }
    let objects: Vec<String> = outcome
        .found
        .iter()
        .map(|found| {
            let project = sources[found.source].project;
            let score = if found.score.is_finite() {
                found.score
            } else {
                0.0
            };
            format!(
                "  {{\"project\": {}, \"root\": {}, \"key\": {}, \"score\": {}}}",
                json_string(&project.name),
                json_string(&plain_root(&project.root)),
                json_string(&found.key),
                score
            )
        })
        .collect();
    format!("[\n{}\n]\n", objects.join(",\n"))
}

pub fn render_problem(query: &str, offset: usize) -> String {
    let column = query
        .get(..offset)
        .map_or(offset, |before| before.chars().count());
    format!(
        "recall: LuxCore cannot read the query here\n  {query}\n  {}^\n",
        " ".repeat(column)
    )
}

pub fn render_projects(sources: &[Source]) -> String {
    sources
        .iter()
        .map(|source| format!("{}\n", plain_root(&source.project.root)))
        .collect()
}

pub fn exit_for(outcome: &Outcome) -> Exit {
    if outcome.problem.is_some() {
        Exit::Usage
    } else if outcome.found.is_empty() {
        Exit::Nothing
    } else {
        Exit::Matched
    }
}

pub fn failure_note(outcome: &Outcome) -> String {
    match outcome.failed.len() {
        0 => String::new(),
        count => format!("recall: {count} project(s) could not be searched\n"),
    }
}

fn report_of(
    sources: &[Source],
    options: &SearchOptions,
    request: &shelf::Request,
    outcome: &Outcome,
) -> Report {
    let note = failure_note(outcome);
    if let Some(problem) = outcome.problem {
        let stderr = render_problem(&options.query, problem.offset) + &note;
        return Report::err(stderr, Exit::Usage);
    }
    let stdout = if options.json {
        render_json(sources, outcome)
    } else {
        render_text(sources, request, outcome)
    };
    Report {
        stdout,
        stderr: note,
        exit: exit_for(outcome),
    }
}

pub fn search_report(sources: &[Source], options: &SearchOptions) -> Report {
    let request = shelf::parse_request(&options.query, options.mode, Reach::Everywhere);
    let outcome = shelf::search(sources, &request, options.limit);
    report_of(sources, options, &request, &outcome)
}

fn usage_report() -> Report {
    Report::out(
        format!(
            "{USAGE}
"
        ),
        Exit::Matched,
    )
}

pub fn run(command: &Command, cache_dir: &Path, library: &Path) -> Report {
    let shelf = Shelf::load(cache_dir, library, None);
    let all = shelf::sources(None, &shelf);
    if all.is_empty() {
        return Report::err(
            format!(
                "recall: no saved indexes in {}. Open a project in Recall first.\n",
                cache_dir.display()
            ),
            Exit::Nothing,
        );
    }
    match command {
        Command::Projects => Report::out(render_projects(&all), Exit::Matched),
        Command::Search(options) => search_report(&all, options),
        Command::Help => usage_report(),
    }
}

fn report_for(args: &[String]) -> Report {
    let invocation = match parse(args) {
        Ok(invocation) => invocation,
        Err(message) => {
            return Report::err(format!("recall: {message}\n{USAGE}\n"), Exit::Usage);
        }
    };
    if invocation.command == Command::Help {
        return usage_report();
    }
    let library = resolve_library(invocation.library);
    if !library.is_file() {
        return Report::err(
            format!("recall: {}\n", missing_library(&library)),
            Exit::Usage,
        );
    }
    match cache::default_dir() {
        Some(dir) => run(&invocation.command, &dir, &library),
        None => Report::err(
            "recall: no cache directory is available on this system\n".into(),
            Exit::Nothing,
        ),
    }
}

pub fn main(args: &[String]) -> ! {
    use std::io::Write;

    let report = report_for(args);
    let _ = std::io::stdout().write_all(report.stdout.as_bytes());
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().write_all(report.stderr.as_bytes());
    std::process::exit(report.exit.code());
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::{
        cache::Records,
        lux::{LuxIndex, library_path},
        shelf::{Project, Shard},
    };

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|word| (*word).to_owned()).collect()
    }

    fn search_options(list: &[&str]) -> SearchOptions {
        match parse(&args(list)).unwrap().command {
            Command::Search(options) => options,
            Command::Projects | Command::Help => panic!("expected a search"),
        }
    }

    fn shard(name: &str, docs: &[(&str, &str)]) -> Shard {
        let mut index = LuxIndex::load(&library_path()).expect("build libluxcore");
        for (key, body) in docs {
            index.add(key, key, body).unwrap();
        }
        Shard {
            project: Project::at(format!("F:/work/{name}")),
            index,
        }
    }

    fn options(query: &str, mode: Mode, json: bool) -> SearchOptions {
        SearchOptions {
            query: query.into(),
            mode,
            limit: 10,
            json,
        }
    }

    #[test]
    fn a_plain_search_uses_the_defaults() {
        let parsed = parse(&args(&["search", "watcher"])).unwrap();
        assert_eq!(parsed.library, None);
        assert_eq!(
            parsed.command,
            Command::Search(options("watcher", Mode::Fuzzy, false))
        );
    }

    #[test]
    fn every_search_flag_is_read_and_query_words_are_joined() {
        let parsed = parse(&args(&[
            "search",
            "--json",
            "watcher",
            "--limit",
            "3",
            "--syntax",
            "--lib",
            "x/lux.dll",
            "debounce",
        ]))
        .unwrap();
        assert_eq!(parsed.library, Some(PathBuf::from("x/lux.dll")));
        assert_eq!(
            parsed.command,
            Command::Search(SearchOptions {
                query: "watcher debounce".into(),
                mode: Mode::Syntax,
                limit: 3,
                json: true,
            })
        );
    }

    #[test]
    fn a_dash_prefixed_word_is_query_text_and_a_double_dash_ends_flags() {
        assert_eq!(search_options(&["search", "+a", "-b"]).query, "+a -b");
        assert_eq!(search_options(&["search", "--", "--json"]).query, "--json");
    }

    #[test]
    fn projects_reads_only_the_library_flag() {
        let parsed = parse(&args(&["projects", "--lib", "a.dll"])).unwrap();
        assert_eq!(parsed.command, Command::Projects);
        assert_eq!(parsed.library, Some(PathBuf::from("a.dll")));
        assert!(parse(&args(&["projects", "--json"])).is_err());
        assert!(parse(&args(&["projects", "--lib"])).is_err());
    }

    #[test]
    fn bad_arguments_are_errors() {
        let failing: [&[&str]; 8] = [
            &[],
            &["frobnicate"],
            &["search"],
            &["search", "--json"],
            &["search", "--bogus", "x"],
            &["search", "x", "--lib"],
            &["search", "x", "--limit"],
            &["search", "x", "--limit", "many"],
        ];
        for list in failing {
            assert!(parse(&args(list)).is_err(), "{list:?}");
        }
        assert!(parse(&args(&["search", "x", "--limit", "0"])).is_err());
        assert!(parse(&args(&["search", "x", "--limit", "-1"])).is_err());
    }

    #[test]
    fn a_query_of_only_scopes_is_a_usage_error() {
        for list in [
            &["search", "@name"][..],
            &["search", "@a", "@b"],
            &["search", "@a", " "],
        ] {
            assert_eq!(
                parse(&args(list)),
                Err("search needs a query".to_owned()),
                "{list:?}"
            );
        }
        assert!(parse(&args(&["search", "@"])).is_ok());
        assert!(parse(&args(&["search", "@name", "text"])).is_ok());
        let done = report_for(&args(&["search", "@name"]));
        assert_eq!(done.exit, Exit::Usage);
        assert!(done.stderr.starts_with(
            "recall: search needs a query
"
        ));
    }

    #[test]
    fn help_after_a_command_prints_the_usage_and_exits_zero() {
        for list in [
            &["search", "--help"][..],
            &["search", "-h"],
            &["search", "watcher", "--help"],
            &["projects", "--help"],
            &["projects", "-h"],
        ] {
            let parsed = parse(&args(list)).unwrap_or_else(|error| panic!("{list:?}: {error}"));
            assert_eq!(parsed.command, Command::Help, "{list:?}");
            let done = report_for(&args(list));
            assert_eq!(done.exit, Exit::Matched, "{list:?}");
            assert_eq!(
                done.stdout,
                format!(
                    "{USAGE}
"
                ),
                "{list:?}"
            );
            assert!(done.stderr.is_empty(), "{list:?}");
        }
        assert_eq!(search_options(&["search", "--", "--help"]).query, "--help");
    }

    #[test]
    fn the_library_flag_beats_the_default() {
        assert_eq!(
            resolve_library(Some(PathBuf::from("flag.dll"))),
            PathBuf::from("flag.dll")
        );
    }

    fn corpus() -> Shelf {
        Shelf::from_shards(vec![
            shard(
                "alpha",
                &[("a.md", "the watcher debounces\n\n  bursts   of changes")],
            ),
            shard("beta", &[("b.md", "watcher watcher watcher")]),
        ])
    }

    fn report(shelf: &Shelf, options: &SearchOptions) -> Report {
        search_report(&shelf::sources(None, shelf), options)
    }

    #[test]
    fn text_output_has_a_scored_line_and_a_clean_snippet_per_hit() {
        let shelf = corpus();
        let done = report(&shelf, &options("watcher", Mode::Fuzzy, false));
        assert_eq!(done.exit, Exit::Matched);
        assert!(done.stderr.is_empty());
        let lines: Vec<&str> = done.stdout.lines().collect();
        assert_eq!(lines.len(), 4, "{lines:?}");
        let head = lines[0];
        assert!(head.ends_with("  beta/b.md"), "{head:?}");
        let score = head.trim_end_matches("  beta/b.md");
        assert_eq!(score.len(), 8, "{head:?}");
        let digits = score.trim_start();
        assert!(digits.parse::<f64>().is_ok());
        assert_eq!(digits.split('.').nth(1).map(str::len), Some(1));
        assert!(lines[2].ends_with("  alpha/a.md"));
        for snippet in [lines[1], lines[3]] {
            assert!(snippet.starts_with("          ") && !snippet[10..].starts_with(' '));
            assert!(!snippet.contains(MARK_START) && !snippet.contains(MARK_END));
            assert!(!snippet[10..].contains("  "));
        }
        assert!(
            lines[3].contains("watcher debounces bursts"),
            "{}",
            lines[3]
        );
    }

    #[test]
    fn a_verbatim_root_prints_in_its_plain_form() {
        assert_eq!(plain_root(Path::new(r"\\?\C:\work")), r"C:\work");
        assert_eq!(
            plain_root(Path::new(r"\\?\UNC\srv\share\p")),
            r"\\srv\share\p"
        );
    }

    #[test]
    fn one_line_strips_markers_and_collapses_whitespace() {
        let marked = format!("a{MARK_START}b{MARK_END} \n\t c  ");
        assert_eq!(one_line(&marked), "ab c");
        assert_eq!(one_line(" \n "), "");
    }

    #[test]
    fn json_escapes_quotes_backslashes_and_control_characters() {
        let shelf =
            Shelf::from_shards(vec![shard("alpha", &[("we\"ird\\k\u{1}ey.md", "watcher")])]);
        let done = report(&shelf, &options("watcher", Mode::Fuzzy, true));
        assert_eq!(done.exit, Exit::Matched);
        let lines: Vec<&str> = done.stdout.lines().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!((lines[0], lines[2]), ("[", "]"));
        assert!(lines[1].starts_with(
            r#"  {"project": "alpha", "root": "F:/work/alpha", "key": "we\"ird\\k\u0001ey.md", "score": "#
        ));
        assert!(lines[1].ends_with('}'));
    }

    #[test]
    fn json_separates_objects_with_commas_one_per_line() {
        let shelf = corpus();
        let done = report(&shelf, &options("watcher", Mode::Fuzzy, true));
        let lines: Vec<&str> = done.stdout.lines().collect();
        assert_eq!(lines.len(), 4);
        assert!(lines[1].ends_with("},") && lines[2].ends_with('}'));
    }

    #[test]
    fn json_escapes_the_named_controls() {
        assert_eq!(
            json_string("a\n\r\t\u{8}\u{c}\u{1f}é"),
            "\"a\\n\\r\\t\\b\\f\\u001fé\""
        );
    }

    #[test]
    fn no_matches_print_an_empty_array_and_exit_one() {
        let shelf = corpus();
        let json = report(&shelf, &options("zzzqqqxxx", Mode::Fuzzy, true));
        assert_eq!((json.stdout.as_str(), json.exit), ("[]\n", Exit::Nothing));
        let text = report(&shelf, &options("zzzqqqxxx", Mode::Fuzzy, false));
        assert_eq!((text.stdout.as_str(), text.exit), ("", Exit::Nothing));
    }

    #[test]
    fn a_syntax_problem_points_a_caret_past_the_scope_prefix() {
        let shelf = corpus();
        let query = "@alpha watcher AND (debt";
        let done = report(&shelf, &options(query, Mode::Syntax, false));
        assert_eq!(done.exit, Exit::Usage);
        assert!(done.stdout.is_empty());
        let lines: Vec<&str> = done.stderr.lines().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "recall: LuxCore cannot read the query here");
        assert_eq!(lines[1], format!("  {query}"));
        let caret = lines[2].find('^').unwrap();
        assert_eq!(caret - 2, query.find('(').unwrap());
    }

    #[test]
    fn the_caret_counts_characters_not_bytes() {
        let text = render_problem("é(", 2);
        assert_eq!(text.lines().nth(2), Some("   ^"));
    }

    #[test]
    fn a_failed_source_is_named_on_stderr_without_changing_the_exit() {
        let shelf = corpus();
        let all = shelf::sources(None, &shelf);
        let opts = options("watcher", Mode::Fuzzy, false);
        let request = shelf::parse_request(&opts.query, opts.mode, Reach::Everywhere);
        let mut outcome = shelf::search(&all, &request, opts.limit);
        let clean = report_of(&all, &opts, &request, &outcome);
        assert!(clean.stderr.is_empty());

        outcome.failed = vec![1];
        let matched = report_of(&all, &opts, &request, &outcome);
        assert_eq!(
            matched.stderr,
            "recall: 1 project(s) could not be searched
"
        );
        assert_eq!(
            (matched.stdout, matched.exit),
            (clean.stdout, Exit::Matched)
        );

        outcome.failed = vec![0, 1];
        outcome.found.clear();
        let nothing = report_of(&all, &opts, &request, &outcome);
        assert_eq!(
            nothing.stderr,
            "recall: 2 project(s) could not be searched
"
        );
        assert_eq!(nothing.exit, Exit::Nothing);

        outcome.problem = Some(shelf::Problem { offset: 0 });
        let usage = report_of(&all, &opts, &request, &outcome);
        assert!(usage.stderr.ends_with(
            "recall: 2 project(s) could not be searched
"
        ));
        assert_eq!(usage.exit, Exit::Usage);
    }

    #[test]
    fn exit_codes_follow_the_outcome() {
        let shelf = corpus();
        let matched = report(&shelf, &options("watcher", Mode::Fuzzy, false));
        let nothing = report(&shelf, &options("zzzqqqxxx", Mode::Fuzzy, false));
        let broken = report(&shelf, &options("(", Mode::Syntax, false));
        let codes = [matched.exit, nothing.exit, broken.exit].map(Exit::code);
        assert_eq!(codes, [0, 1, 2]);
    }

    fn save_entry(dir: &Path, root: &Path, body: &str) {
        let mut index = LuxIndex::load(&library_path()).unwrap();
        index.add("a.md", "a.md", body).unwrap();
        let files = cache::cache_files(dir, root);
        let fingerprint = cache::fingerprint(&library_path());
        cache::save(&index, &files, &fingerprint, None, &Records::new()).unwrap();
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!("recall-cli-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn run_searches_and_lists_a_saved_cache() {
        let dir = temp_dir("saved");
        let zeta = dir.join("Zeta");
        fs::create_dir_all(&zeta).unwrap();
        let verbatim = format!(r"\\?\{}", zeta.display());
        save_entry(&dir, Path::new(&verbatim), "zeta watcher");
        let library = library_path();

        let projects = run(&Command::Projects, &dir, &library);
        assert_eq!(projects.stdout, format!("{}\n", zeta.display()));
        assert_eq!(projects.exit, Exit::Matched);

        let found = run(
            &Command::Search(options("watcher", Mode::Fuzzy, false)),
            &dir,
            &library,
        );
        assert_eq!(found.exit, Exit::Matched);
        assert!(found.stdout.contains("Zeta/a.md"), "{}", found.stdout);

        let json = run(
            &Command::Search(options("watcher", Mode::Fuzzy, true)),
            &dir,
            &library,
        );
        let root = format!(r#""root": {}"#, json_string(&zeta.to_string_lossy()));
        assert!(json.stdout.contains(&root), "{}", json.stdout);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_on_an_empty_cache_says_so_and_exits_one() {
        let dir = temp_dir("empty");
        let library = library_path();
        for command in [
            Command::Projects,
            Command::Search(options("watcher", Mode::Fuzzy, false)),
        ] {
            let done = run(&command, &dir, &library);
            assert_eq!(done.exit, Exit::Nothing);
            assert!(done.stdout.is_empty());
            assert_eq!(
                done.stderr,
                format!(
                    "recall: no saved indexes in {}. Open a project in Recall first.\n",
                    dir.display()
                )
            );
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_missing_library_text_matches_the_tui() {
        let text = missing_library(Path::new("gone.dll"));
        assert!(text.starts_with("LuxCore shared library not found at gone.dll. Build LuxCore"));
    }
}
