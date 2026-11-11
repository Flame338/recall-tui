use std::{
    cmp::Ordering,
    fmt,
    fs::File,
    io::{self, Read},
    path::{Path, PathBuf},
};

use crate::{
    cache,
    lux::{Hit, LuxIndex, QueryFailure},
    scan::MAX_FILE_BYTES,
    view::plain_path,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Fuzzy,
    Syntax,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Reach {
    #[default]
    Here,
    Everywhere,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request<'a> {
    pub scopes: Vec<String>,
    pub text: &'a str,
    pub text_offset: usize,
    pub mode: Mode,
    pub reach: Reach,
}

pub fn parse_request(input: &str, mode: Mode, reach: Reach) -> Request<'_> {
    let mut scopes = Vec::new();
    let mut start = 0;
    loop {
        start = input.len() - input[start..].trim_start().len();
        let token = input[start..]
            .split(char::is_whitespace)
            .next()
            .unwrap_or("");
        match token.strip_prefix('@') {
            Some(name) if !name.is_empty() => {
                scopes.push(name.to_lowercase());
                start += token.len();
            }
            _ => {
                return Request {
                    scopes,
                    text: &input[start..],
                    text_offset: start,
                    mode,
                    reach,
                };
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Project {
    pub name: String,
    pub root: PathBuf,
}

impl Project {
    pub fn at(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let shown = root.to_string_lossy();
        let plain = plain_path(&shown);
        let name = Path::new(plain.as_ref()).file_name().map_or_else(
            || plain.to_string(),
            |name| name.to_string_lossy().into_owned(),
        );
        Self { name, root }
    }
}

#[cfg(test)]
pub struct Shard {
    pub project: Project,
    pub index: LuxIndex,
}

/// A saved project stays on disk until a request first has it in scope.
enum Held {
    Saved(cache::CacheFiles),
    Loaded(LuxIndex),
}

struct Shelved {
    project: Project,
    held: Held,
}

struct Origin {
    cache_dir: PathBuf,
    library: PathBuf,
    fingerprint: String,
    except: Option<PathBuf>,
}

#[derive(Default)]
pub struct Shelf {
    entries: Vec<Shelved>,
    origin: Option<Origin>,
}

fn same_root(left: &str, right: &str) -> bool {
    let left = plain_path(left);
    let right = plain_path(right);
    if cfg!(windows) {
        left.to_lowercase() == right.to_lowercase()
    } else {
        left == right
    }
}

impl Shelf {
    /// Lists the saved projects another project can search, reading only
    /// manifest headers. No index is read until `load_for` needs it.
    pub fn list(cache_dir: &Path, library: &Path, except: Option<&Path>) -> Self {
        let except_text = except.map(|path| path.to_string_lossy().into_owned());
        let fingerprint = cache::fingerprint(library);
        let entries = cache::list_entries(cache_dir)
            .into_iter()
            .filter_map(|entry| {
                let header = entry.header?;
                if except_text
                    .as_deref()
                    .is_some_and(|except| same_root(&header.root, except))
                {
                    return None;
                }
                let root = Path::new(&header.root);
                if !root.is_dir() {
                    return None;
                }
                let files = cache::entry_files(cache_dir, &entry.name, root);
                if !cache::restorable(&header, &files, &fingerprint) {
                    return None;
                }
                Some(Shelved {
                    project: Project::at(&header.root),
                    held: Held::Saved(files),
                })
            })
            .collect();
        Self {
            entries: sorted(entries),
            origin: Some(Origin {
                cache_dir: cache_dir.to_path_buf(),
                library: library.to_path_buf(),
                fingerprint,
                except: except.map(Path::to_path_buf),
            }),
        }
    }

    #[cfg(test)]
    pub fn from_shards(shards: Vec<Shard>) -> Self {
        let entries = shards
            .into_iter()
            .map(|shard| Shelved {
                project: shard.project,
                held: Held::Loaded(shard.index),
            })
            .collect();
        Self {
            entries: sorted(entries),
            origin: None,
        }
    }

    /// Lists the cache again and forgets every loaded index, so the next
    /// request reads the saved files afresh.
    pub fn reload(&mut self) {
        if let Some(origin) = self.origin.take() {
            *self = Self::list(&origin.cache_dir, &origin.library, origin.except.as_deref());
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn projects(&self) -> impl Iterator<Item = &Project> {
        self.entries.iter().map(|entry| &entry.project)
    }

    /// Saved projects the request has in scope that are still on disk.
    pub fn unloaded(&self, request: &Request) -> Vec<&Project> {
        self.entries
            .iter()
            .filter(|entry| matches!(entry.held, Held::Saved(_)))
            .map(|entry| &entry.project)
            .filter(|project| wanted(project, false, request))
            .collect()
    }

    /// Reads every saved index the request has in scope. One that no longer
    /// restores leaves the shelf, as if it had never been listed.
    pub fn load_for(&mut self, request: &Request) {
        let Some(origin) = &self.origin else {
            return;
        };
        self.entries.retain_mut(|entry| {
            let Held::Saved(files) = &entry.held else {
                return true;
            };
            if !wanted(&entry.project, false, request) {
                return true;
            }
            let Some(mut index) = LuxIndex::load(&origin.library).ok() else {
                return false;
            };
            if cache::restore(&mut index, files, &origin.fingerprint).is_none() {
                return false;
            }
            entry.held = Held::Loaded(index);
            true
        });
    }

    #[cfg(test)]
    pub fn loaded(&self) -> Vec<&str> {
        self.entries
            .iter()
            .filter(|entry| matches!(entry.held, Held::Loaded(_)))
            .map(|entry| entry.project.name.as_str())
            .collect()
    }
}

fn sorted(mut entries: Vec<Shelved>) -> Vec<Shelved> {
    entries.sort_by_cached_key(|entry| {
        (
            entry.project.name.to_lowercase(),
            entry.project.name.clone(),
            entry.project.root.clone(),
        )
    });
    entries
}

#[derive(Clone, Copy)]
pub struct Source<'a> {
    pub project: &'a Project,
    pub index: &'a LuxIndex,
    pub here: bool,
}

/// The open project, then every loaded project on the shelf. A saved project
/// that `load_for` has not read yet is not a source.
pub fn sources<'a>(here: Option<(&'a Project, &'a LuxIndex)>, shelf: &'a Shelf) -> Vec<Source<'a>> {
    let loaded = shelf.entries.iter().filter_map(|entry| match &entry.held {
        Held::Loaded(index) => Some(Source {
            project: &entry.project,
            index,
            here: false,
        }),
        Held::Saved(_) => None,
    });
    here.into_iter()
        .map(|(project, index)| Source {
            project,
            index,
            here: true,
        })
        .chain(loaded)
        .collect()
}

/// Names of the projects the request has in scope, from the listing alone.
pub fn scoped_names<'a>(
    here: Option<&'a Project>,
    shelf: &'a Shelf,
    request: &Request,
) -> Vec<&'a str> {
    here.filter(|project| wanted(project, true, request))
        .into_iter()
        .chain(
            shelf
                .projects()
                .filter(|project| wanted(project, false, request)),
        )
        .map(|project| project.name.as_str())
        .collect()
}

#[derive(Clone, Debug, PartialEq)]
pub struct Found {
    pub source: usize,
    pub key: String,
    pub score: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Problem {
    pub offset: usize,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Outcome {
    pub found: Vec<Found>,
    pub problem: Option<Problem>,
    pub searched: Vec<usize>,
    pub failed: Vec<usize>,
}

fn in_scope(source: &Source, request: &Request) -> bool {
    wanted(source.project, source.here, request)
}

fn wanted(project: &Project, here: bool, request: &Request) -> bool {
    if request.scopes.is_empty() {
        return here || request.reach == Reach::Everywhere;
    }
    let name = project.name.to_lowercase();
    request
        .scopes
        .iter()
        .any(|scope| name.contains(scope.as_str()))
}

fn in_scope_sources<'s, 'a>(
    sources: &'s [Source<'a>],
    request: &'s Request,
) -> impl Iterator<Item = (usize, &'s Source<'a>)> {
    sources
        .iter()
        .enumerate()
        .filter(move |(_, source)| in_scope(source, request))
}

fn order(left: &Found, right: &Found) -> Ordering {
    right
        .score
        .total_cmp(&left.score)
        .then(left.source.cmp(&right.source))
        .then_with(|| left.key.cmp(&right.key))
}

fn query(source: &Source, request: &Request, limit: usize) -> Result<Vec<Hit>, QueryFailure> {
    match request.mode {
        Mode::Fuzzy => Ok(source.index.search(request.text, limit)?),
        Mode::Syntax => source.index.search_query(request.text, limit),
    }
}

pub fn search(sources: &[Source], request: &Request, limit: usize) -> Outcome {
    let in_scope: Vec<(usize, &Source)> = in_scope_sources(sources, request).collect();
    let searched: Vec<usize> = in_scope.iter().map(|(position, _)| *position).collect();
    if request.text.trim().is_empty() {
        return Outcome {
            searched,
            ..Outcome::default()
        };
    }
    let mut found = Vec::new();
    let mut failed = Vec::new();
    for (position, source) in in_scope {
        match query(source, request, limit) {
            Ok(hits) => found.extend(hits.into_iter().map(|hit| Found {
                source: position,
                key: hit.key,
                score: hit.score,
            })),
            Err(QueryFailure::Invalid { offset }) => {
                return Outcome {
                    found: Vec::new(),
                    problem: Some(Problem {
                        offset: request.text_offset + offset,
                    }),
                    searched,
                    failed,
                };
            }
            Err(QueryFailure::Other(_)) => failed.push(position),
        }
    }
    found.sort_by(order);
    found.truncate(limit);
    Outcome {
        found,
        problem: None,
        searched,
        failed,
    }
}

pub fn suggest(sources: &[Source], request: &Request, limit: usize) -> Vec<String> {
    if request.mode == Mode::Syntax || request.text.trim().is_empty() {
        return Vec::new();
    }
    let lists: Vec<Vec<String>> = in_scope_sources(sources, request)
        .filter_map(|(_, source)| source.index.suggest(request.text, limit).ok())
        .collect();
    let rounds = lists.iter().map(Vec::len).max().unwrap_or(0);
    let mut merged: Vec<String> = Vec::new();
    for round in 0..rounds {
        for word in lists.iter().filter_map(|list| list.get(round)) {
            if merged.len() == limit {
                return merged;
            }
            if !merged.contains(word) {
                merged.push(word.clone());
            }
        }
    }
    merged
}

pub fn ghost<'a>(input: &str, suggestions: &'a [String]) -> &'a str {
    let word = input.rsplit(char::is_whitespace).next().unwrap_or("");
    if word.is_empty() {
        return "";
    }
    suggestions
        .iter()
        .find_map(|suggestion| {
            let prefix = suggestion.as_bytes().get(..word.len())?;
            if suggestion.len() > word.len() && prefix.eq_ignore_ascii_case(word.as_bytes()) {
                suggestion.get(word.len()..)
            } else {
                None
            }
        })
        .unwrap_or("")
}

/// Why a hit has no text to show.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoText {
    Gone,
    TooLarge,
    NotText,
    Unreadable,
}

impl fmt::Display for NoText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            NoText::Gone => "This file is no longer on disk",
            NoText::TooLarge => "This file is now over 512 KiB, too large to index or preview",
            NoText::NotText => "This file is no longer UTF-8 text",
            NoText::Unreadable => "This file cannot be read right now",
        })
    }
}

impl From<io::Error> for NoText {
    fn from(error: io::Error) -> Self {
        match error.kind() {
            io::ErrorKind::NotFound => NoText::Gone,
            io::ErrorKind::InvalidData => NoText::NotText,
            _ => NoText::Unreadable,
        }
    }
}

impl Source<'_> {
    /// The file's text as it is on disk now. The index keeps no copy of it.
    pub fn read(&self, key: &str) -> Result<String, NoText> {
        let mut file = File::open(self.project.root.join(key))?;
        let size = file.metadata()?.len();
        if size > MAX_FILE_BYTES {
            return Err(NoText::TooLarge);
        }
        let mut text = String::with_capacity(size as usize);
        file.read_to_string(&mut text)?;
        Ok(text)
    }
}

/// `text` with the words the request matches marked.
pub fn snippet(source: &Source, text: &str, request: &Request, length: usize) -> Option<String> {
    match request.mode {
        Mode::Fuzzy => source.index.snippet_text(text, request.text, length).ok(),
        Mode::Syntax => source
            .index
            .snippet_text_query(text, request.text, length)
            .ok(),
    }
}

#[cfg(test)]
mod tests {
    use std::{env, fs};

    use super::*;
    use crate::{
        cache::{CacheFiles, Header, Manifest, Records, Stamp, serialize},
        lux::{MARK_END, MARK_START, library_path},
    };

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

    fn watchers(name: &str) -> Shard {
        shard(name, &[("a.md", "the watcher debounces bursts of changes")])
    }

    fn request(input: &str, mode: Mode, reach: Reach) -> Request<'_> {
        parse_request(input, mode, reach)
    }

    fn found(outcome: &Outcome) -> Vec<(usize, &str)> {
        outcome
            .found
            .iter()
            .map(|found| (found.source, found.key.as_str()))
            .collect()
    }

    #[test]
    fn leading_at_words_become_lowercase_scopes() {
        let parsed = request("  @Api \t@WEB  fix the bug ", Mode::Fuzzy, Reach::Here);
        assert_eq!(parsed.scopes, ["api", "web"]);
        assert_eq!(parsed.text, "fix the bug ");
        assert_eq!(parsed.text_offset, 14);
        assert_eq!(
            &"  @Api \t@WEB  fix the bug "[parsed.text_offset..],
            parsed.text
        );
    }

    #[test]
    fn the_first_plain_word_ends_scope_parsing() {
        let parsed = request("fix @api bug", Mode::Syntax, Reach::Everywhere);
        assert!(parsed.scopes.is_empty());
        assert_eq!(parsed.text, "fix @api bug");
        assert_eq!(parsed.text_offset, 0);
        assert_eq!(parsed.mode, Mode::Syntax);
        assert_eq!(parsed.reach, Reach::Everywhere);
    }

    #[test]
    fn a_lone_at_is_text_not_a_scope() {
        let parsed = request("@ api", Mode::Fuzzy, Reach::Here);
        assert!(parsed.scopes.is_empty());
        assert_eq!(parsed.text, "@ api");
        let parsed = request("@a @ api", Mode::Fuzzy, Reach::Here);
        assert_eq!(parsed.scopes, ["a"]);
        assert_eq!(parsed.text, "@ api");
        assert_eq!(parsed.text_offset, 3);
        assert_eq!(request("@", Mode::Fuzzy, Reach::Here).text, "@");
    }

    #[test]
    fn nothing_after_the_scopes_leaves_empty_text_at_the_end() {
        let input = "@a @b  ";
        let parsed = request(input, Mode::Fuzzy, Reach::Here);
        assert_eq!(parsed.scopes, ["a", "b"]);
        assert_eq!(parsed.text, "");
        assert_eq!(parsed.text_offset, input.len());
        let empty = request("", Mode::Fuzzy, Reach::Here);
        assert_eq!((empty.text, empty.text_offset), ("", 0));
    }

    #[test]
    fn multi_byte_scopes_keep_byte_offsets() {
        let input = "@Über wörd";
        let parsed = request(input, Mode::Fuzzy, Reach::Here);
        assert_eq!(parsed.scopes, ["über"]);
        assert_eq!(parsed.text, "wörd");
        assert_eq!(&input[parsed.text_offset..], "wörd");
    }

    #[test]
    fn a_project_is_named_after_the_last_component_of_its_root() {
        assert_eq!(Project::at("F:/work/Alpha").name, "Alpha");
        let verbatim = Project::at(r"\\?\F:/work/Alpha");
        assert_eq!(verbatim.name, "Alpha");
        assert_eq!(verbatim.root, PathBuf::from(r"\\?\F:/work/Alpha"));
    }

    #[test]
    fn a_verbatim_unc_root_reads_as_the_share_it_is() {
        assert_eq!(Project::at(r"\\?\UNC\srv\share").name, r"\\srv\share");
        assert!(same_root(r"\\?\UNC\srv\share\Alpha", r"\\srv\share\Alpha"));
        assert!(!same_root(
            r"\\?\UNC\srv\share\Alpha",
            r"UNC\srv\share\Alpha"
        ));
    }

    fn corpus() -> (Shard, Shelf) {
        let here = watchers("here");
        let shelf = Shelf::from_shards(vec![watchers("beta"), watchers("alpha")]);
        (here, shelf)
    }

    fn listed_names(shelf: &Shelf) -> Vec<&str> {
        shelf
            .projects()
            .map(|project| project.name.as_str())
            .collect()
    }

    #[test]
    fn a_shelf_orders_its_shards_by_name() {
        let (_, shelf) = corpus();
        assert_eq!(listed_names(&shelf), ["alpha", "beta"]);
    }

    #[test]
    fn a_shelf_orders_its_shards_without_regard_to_case() {
        let shelf = Shelf::from_shards(vec![
            watchers("Zeta"),
            watchers("api"),
            watchers("Beta"),
            watchers("alpha"),
            watchers("Api"),
        ]);
        assert_eq!(
            listed_names(&shelf),
            ["alpha", "Api", "api", "Beta", "Zeta"]
        );
    }

    #[test]
    fn sources_put_here_first_then_the_shelf() {
        let (here, shelf) = corpus();
        let all = sources(Some((&here.project, &here.index)), &shelf);
        let flags: Vec<(bool, &str)> = all
            .iter()
            .map(|source| (source.here, source.project.name.as_str()))
            .collect();
        assert_eq!(flags, [(true, "here"), (false, "alpha"), (false, "beta")]);
        assert_eq!(sources(None, &shelf).len(), 2);
    }

    #[test]
    fn without_scopes_here_searches_only_here() {
        let (here, shelf) = corpus();
        let all = sources(Some((&here.project, &here.index)), &shelf);
        let outcome = search(&all, &request("watcher", Mode::Fuzzy, Reach::Here), 10);
        assert_eq!(outcome.searched, [0]);
        assert_eq!(found(&outcome), [(0, "a.md")]);
    }

    #[test]
    fn without_scopes_everywhere_searches_every_source() {
        let (here, shelf) = corpus();
        let all = sources(Some((&here.project, &here.index)), &shelf);
        let outcome = search(
            &all,
            &request("watcher", Mode::Fuzzy, Reach::Everywhere),
            10,
        );
        assert_eq!(outcome.searched, [0, 1, 2]);
        assert_eq!(found(&outcome), [(0, "a.md"), (1, "a.md"), (2, "a.md")]);
    }

    #[test]
    fn a_scope_picks_matching_projects_whatever_the_reach() {
        let (here, shelf) = corpus();
        let all = sources(Some((&here.project, &here.index)), &shelf);
        for reach in [Reach::Here, Reach::Everywhere] {
            let outcome = search(&all, &request("@ALP watcher", Mode::Fuzzy, reach), 10);
            assert_eq!(outcome.searched, [1], "{reach:?}");
            assert_eq!(found(&outcome), [(1, "a.md")], "{reach:?}");
            let outcome = search(&all, &request("@he @bet watcher", Mode::Fuzzy, reach), 10);
            assert_eq!(outcome.searched, [0, 2], "{reach:?}");
            assert_eq!(found(&outcome), [(0, "a.md"), (2, "a.md")], "{reach:?}");
            let outcome = search(&all, &request("@nothing watcher", Mode::Fuzzy, reach), 10);
            assert_eq!(outcome, Outcome::default(), "{reach:?}");
        }
    }

    #[test]
    fn hits_merge_by_raw_score_across_sources() {
        let weak = shard(
            "aaa",
            &[(
                "weak.md",
                "notes about many unrelated things with a single mention of the watcher far from the start and lots of other words after it",
            )],
        );
        let strong = shard("bbb", &[("strong.md", "watcher watcher watcher")]);
        let shelf = Shelf::from_shards(vec![weak, strong]);
        let all = sources(None, &shelf);
        let outcome = search(
            &all,
            &request("watcher", Mode::Fuzzy, Reach::Everywhere),
            10,
        );
        assert_eq!(found(&outcome), [(1, "strong.md"), (0, "weak.md")]);
        assert!(outcome.found[0].score > outcome.found[1].score);
    }

    #[test]
    fn equal_scores_break_by_source_then_key() {
        let twin = |name: &str| shard(name, &[("b.md", "watcher"), ("a.md", "watcher")]);
        let shelf = Shelf::from_shards(vec![twin("one"), twin("two")]);
        let all = sources(None, &shelf);
        let outcome = search(
            &all,
            &request("watcher", Mode::Fuzzy, Reach::Everywhere),
            10,
        );
        assert_eq!(outcome.found.len(), 4);
        assert!(
            outcome
                .found
                .windows(2)
                .all(|pair| pair[0].score == pair[1].score)
        );
        assert_eq!(
            found(&outcome),
            [(0, "a.md"), (0, "b.md"), (1, "a.md"), (1, "b.md")]
        );
    }

    #[test]
    fn the_limit_caps_the_merged_hits() {
        let many = |name: &str| shard(name, &[("a.md", "watcher"), ("b.md", "watcher")]);
        let shelf = Shelf::from_shards(vec![many("one"), many("two")]);
        let all = sources(None, &shelf);
        let outcome = search(&all, &request("watcher", Mode::Fuzzy, Reach::Everywhere), 3);
        assert_eq!(outcome.found.len(), 3);
        assert_eq!(outcome.searched, [0, 1]);
    }

    #[test]
    fn blank_text_finds_nothing_but_still_names_the_sources_in_scope() {
        let (here, shelf) = corpus();
        let all = sources(Some((&here.project, &here.index)), &shelf);
        let cases: [(&str, Reach, &[usize]); 5] = [
            ("", Reach::Everywhere, &[0, 1, 2]),
            ("   ", Reach::Here, &[0]),
            ("@alpha", Reach::Everywhere, &[1]),
            ("@alpha   ", Reach::Here, &[1]),
            ("@nothing", Reach::Everywhere, &[]),
        ];
        for (input, reach, expected) in cases {
            for mode in [Mode::Fuzzy, Mode::Syntax] {
                let outcome = search(&all, &request(input, mode, reach), 10);
                let wanted = Outcome {
                    searched: expected.to_vec(),
                    ..Outcome::default()
                };
                assert_eq!(outcome, wanted, "{input:?} {mode:?} {reach:?}");
            }
        }
    }

    #[test]
    fn syntax_mode_honors_operators() {
        let shelf = Shelf::from_shards(vec![shard(
            "docs",
            &[
                ("a.md", "the watcher debounces bursts of changes"),
                ("b.md", "a debt is paid down by the watcher"),
            ],
        )]);
        let all = sources(None, &shelf);
        let outcome = search(
            &all,
            &request("@docs +watcher -debt", Mode::Syntax, Reach::Here),
            10,
        );
        assert_eq!(found(&outcome), [(0, "a.md")]);
        assert_eq!(outcome.problem, None);
    }

    #[test]
    fn a_syntax_problem_points_into_the_original_input() {
        let shelf = Shelf::from_shards(vec![watchers("alpha"), watchers("abc")]);
        let all = sources(None, &shelf);
        let input = "@a x AND (y";
        let outcome = search(&all, &request(input, Mode::Syntax, Reach::Here), 10);
        assert!(outcome.found.is_empty());
        let problem = outcome.problem.expect("the query is malformed");
        assert_eq!(problem.offset, input.find('(').unwrap());
        let bare = "watcher AND (debt";
        let outcome = search(&all, &request(bare, Mode::Syntax, Reach::Everywhere), 10);
        assert_eq!(outcome.problem, Some(Problem { offset: 12 }));
    }

    #[test]
    fn a_fuzzy_search_never_reports_a_syntax_problem() {
        let (here, shelf) = corpus();
        let all = sources(Some((&here.project, &here.index)), &shelf);
        let outcome = search(
            &all,
            &request("watcher AND (", Mode::Fuzzy, Reach::Here),
            10,
        );
        assert_eq!(outcome.problem, None);
    }

    fn suggesters() -> Shelf {
        Shelf::from_shards(vec![
            shard("one", &[("a.md", "debounces debounce debt debts")]),
            shard("two", &[("a.md", "debt debris debut")]),
        ])
    }

    #[test]
    fn suggestions_interleave_sources_without_duplicates() {
        let shelf = suggesters();
        let all = sources(None, &shelf);
        let req = request("deb", Mode::Fuzzy, Reach::Everywhere);
        let one = all[0].index.suggest("deb", 10).unwrap();
        let two = all[1].index.suggest("deb", 10).unwrap();
        let merged = suggest(&all, &req, 20);
        assert_eq!(merged[0], one[0]);
        let second = two.iter().find(|word| **word != one[0]).unwrap();
        assert_eq!(&merged[1], second);
        let mut sorted = merged.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), merged.len());
        let mut every: Vec<String> = one.into_iter().chain(two).collect();
        every.sort();
        every.dedup();
        assert_eq!(sorted, every);
    }

    #[test]
    fn here_leads_the_suggestions_and_the_limit_applies() {
        let here = shard("here", &[("a.md", "debounces")]);
        let shelf = Shelf::from_shards(vec![shard("other", &[("a.md", "debris")])]);
        let all = sources(Some((&here.project, &here.index)), &shelf);
        let req = request("deb", Mode::Fuzzy, Reach::Everywhere);
        assert_eq!(suggest(&all, &req, 10), ["debounces", "debris"]);
        assert_eq!(suggest(&all, &req, 1), ["debounces"]);
        let req = request("deb", Mode::Fuzzy, Reach::Here);
        assert_eq!(suggest(&all, &req, 10), ["debounces"]);
    }

    #[test]
    fn syntax_mode_and_blank_text_suggest_nothing() {
        let shelf = suggesters();
        let all = sources(None, &shelf);
        let syntax = request("deb", Mode::Syntax, Reach::Everywhere);
        assert!(suggest(&all, &syntax, 10).is_empty());
        let blank = request("@one  ", Mode::Fuzzy, Reach::Everywhere);
        assert!(suggest(&all, &blank, 10).is_empty());
    }

    fn words(list: &[&str]) -> Vec<String> {
        list.iter().map(|word| (*word).to_owned()).collect()
    }

    #[test]
    fn ghost_is_the_rest_of_the_first_longer_match_of_the_last_word() {
        let list = words(&["debt", "debounces", "other"]);
        assert_eq!(ghost("watcher deb", &list), "t");
        assert_eq!(ghost("watcher debo", &list), "unces");
        assert_eq!(ghost("oth", &list), "er");
    }

    #[test]
    fn ghost_ignores_ascii_case_and_returns_the_suggestions_own_text() {
        let list = words(&["Debounces"]);
        assert_eq!(ghost("DEB", &list), "ounces");
        assert_eq!(ghost("deb", &list), "ounces");
    }

    #[test]
    fn ghost_needs_a_word_and_a_longer_suggestion() {
        let list = words(&["debt"]);
        assert_eq!(ghost("", &list), "");
        assert_eq!(ghost("deb ", &list), "");
        assert_eq!(ghost("deb\t", &list), "");
        assert_eq!(ghost("debt", &list), "");
        assert_eq!(ghost("debts", &list), "");
        assert_eq!(ghost("xyz", &list), "");
        assert_eq!(ghost("deb", &[]), "");
    }

    #[test]
    fn ghost_never_splits_a_multi_byte_character() {
        let list = words(&["naïve", "日本語テキスト"]);
        assert_eq!(ghost("na", &list), "ïve");
        assert_eq!(ghost("naï", &list), "ve");
        assert_eq!(ghost("日本", &list), "語テキスト");
        assert_eq!(ghost("日", &list), "本語テキスト");
        let list = words(&["écrit"]);
        assert_eq!(ghost("é", &list), "crit");
        assert_eq!(ghost("\u{e0}", &list), "");
    }

    #[test]
    fn snippets_mark_the_matched_words_in_both_modes() {
        let text = "a debt is paid down by the watcher";
        let shelf = Shelf::from_shards(vec![shard("docs", &[("b.md", text)])]);
        let all = sources(None, &shelf);
        for mode in [Mode::Fuzzy, Mode::Syntax] {
            let req = request("debt", mode, Reach::Here);
            let marked = snippet(&all[0], text, &req, 80).expect("a snippet");
            assert_eq!(
                marked,
                format!("a {MARK_START}debt{MARK_END} is paid down by the watcher"),
                "{mode:?}"
            );
        }
    }

    #[test]
    fn a_snippet_for_a_query_luxcore_cannot_read_is_none() {
        let shelf = Shelf::from_shards(vec![shard("docs", &[("b.md", "a debt")])]);
        let all = sources(None, &shelf);
        let bad = request("(debt", Mode::Syntax, Reach::Here);
        assert_eq!(snippet(&all[0], "a debt", &bad, 80), None);
    }

    #[test]
    fn a_hit_is_read_from_its_own_project_folder_as_the_file_is_now() {
        let dir = temp_dir("read");
        let (near, far) = (project_dir(&dir, "near"), project_dir(&dir, "far"));
        fs::create_dir_all(far.join("docs")).unwrap();
        fs::write(near.join("a.md"), "near text").unwrap();
        fs::write(far.join("docs/a.md"), "indexed text").unwrap();
        let shelf = Shelf::from_shards(vec![
            Shard {
                project: Project::at(&far),
                index: index_of("indexed text"),
            },
            Shard {
                project: Project::at(&near),
                index: index_of("near text"),
            },
        ]);
        let all = sources(None, &shelf);
        assert_eq!(all[0].read("docs/a.md").as_deref(), Ok("indexed text"));
        assert_eq!(all[1].read("a.md").as_deref(), Ok("near text"));

        fs::write(far.join("docs/a.md"), "edited since").unwrap();
        assert_eq!(all[0].read("docs/a.md").as_deref(), Ok("edited since"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_that_cannot_be_shown_says_why() {
        let dir = temp_dir("no-text");
        let root = project_dir(&dir, "docs");
        fs::write(root.join("binary.md"), [0xff, 0xfe, 0x00, 0x9f]).unwrap();
        fs::write(root.join("at-cap.md"), vec![b'a'; 512 * 1024]).unwrap();
        fs::write(root.join("huge.md"), vec![b'a'; 512 * 1024 + 1]).unwrap();
        fs::create_dir_all(root.join("folder.md")).unwrap();
        let shelf = Shelf::from_shards(vec![Shard {
            project: Project::at(&root),
            index: index_of("anything"),
        }]);
        let all = sources(None, &shelf);
        assert_eq!(all[0].read("deleted.md"), Err(NoText::Gone));
        assert_eq!(all[0].read("binary.md"), Err(NoText::NotText));
        assert_eq!(all[0].read("huge.md"), Err(NoText::TooLarge));
        assert_eq!(all[0].read("folder.md"), Err(NoText::Unreadable));
        assert_eq!(
            all[0].read("at-cap.md").map(|text| text.len()),
            Ok(512 * 1024)
        );
        let _ = fs::remove_dir_all(&dir);
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!("recall-shelf-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn project_dir(dir: &Path, name: &str) -> PathBuf {
        let root = dir.join("projects").join(name);
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn index_of(body: &str) -> LuxIndex {
        let mut index = LuxIndex::load(&library_path()).unwrap();
        index.add("a.md", "a.md", body).unwrap();
        index
    }

    fn save_index(dir: &Path, root: &Path, fingerprint: &str, body: &str) -> CacheFiles {
        let files = cache::cache_files(dir, root);
        cache::save(&index_of(body), &files, fingerprint, None, &Records::new()).unwrap();
        files
    }

    fn save_current(dir: &Path, root: &Path, body: &str) -> CacheFiles {
        save_index(dir, root, &cache::fingerprint(&library_path()), body)
    }

    fn save_broken(dir: &Path, root: &Path) {
        let files = cache::cache_files(dir, root);
        fs::write(&files.index, b"not an index").unwrap();
        let header = Header {
            fingerprint: cache::fingerprint(&library_path()),
            root: root.to_string_lossy().into_owned(),
            index: Stamp::of(&fs::metadata(&files.index).unwrap()),
            cursor: None,
        };
        fs::write(&files.manifest, serialize(&header, &Manifest::new())).unwrap();
    }

    fn everywhere() -> Request<'static> {
        request("watcher", Mode::Fuzzy, Reach::Everywhere)
    }

    fn load_all(shelf: &mut Shelf) -> Vec<&str> {
        shelf.load_for(&everywhere());
        shelf.loaded()
    }

    fn hits(shelf: &Shelf, request: &Request) -> Vec<(String, String, u64)> {
        let all = sources(None, shelf);
        search(&all, request, 10)
            .found
            .iter()
            .map(|found| {
                (
                    all[found.source].project.name.clone(),
                    found.key.clone(),
                    found.score.to_bits(),
                )
            })
            .collect()
    }

    #[test]
    fn listing_a_shelf_reads_no_index() {
        let dir = temp_dir("listed");
        save_current(&dir, &project_dir(&dir, "alpha"), "alpha watcher");
        save_current(&dir, &project_dir(&dir, "beta"), "beta watcher");
        let shelf = Shelf::list(&dir, &library_path(), None);
        assert_eq!(listed_names(&shelf), ["alpha", "beta"]);
        assert_eq!(shelf.len(), 2);
        assert!(shelf.loaded().is_empty());
        assert!(sources(None, &shelf).is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_request_loads_only_the_saved_projects_it_has_in_scope() {
        let dir = temp_dir("in-scope");
        for name in ["alpha", "beta", "gamma"] {
            save_current(&dir, &project_dir(&dir, name), "watcher");
        }
        let mut shelf = Shelf::list(&dir, &library_path(), None);
        let here = request("watcher", Mode::Fuzzy, Reach::Here);
        assert!(shelf.unloaded(&here).is_empty());
        shelf.load_for(&here);
        assert!(shelf.loaded().is_empty());

        let scoped = request("@bet watcher", Mode::Fuzzy, Reach::Here);
        let waiting: Vec<&str> = shelf
            .unloaded(&scoped)
            .iter()
            .map(|project| project.name.as_str())
            .collect();
        assert_eq!(waiting, ["beta"]);
        shelf.load_for(&scoped);
        assert_eq!(shelf.loaded(), ["beta"]);
        assert!(shelf.unloaded(&scoped).is_empty());

        assert_eq!(load_all(&mut shelf), ["alpha", "beta", "gamma"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn loading_on_demand_finds_what_loading_everything_at_once_finds() {
        let dir = temp_dir("same-hits");
        save_current(&dir, &project_dir(&dir, "alpha"), "the watcher debounces");
        save_current(&dir, &project_dir(&dir, "beta"), "watcher watcher debt");
        save_current(&dir, &project_dir(&dir, "gamma"), "a debt of watchers");
        let library = library_path();
        let mut eager = Shelf::list(&dir, &library, None);
        load_all(&mut eager);
        let mut lazy = Shelf::list(&dir, &library, None);
        let queries = [
            ("@gam watcher", Mode::Fuzzy, Reach::Here),
            ("watcher", Mode::Fuzzy, Reach::Everywhere),
            ("+watcher -debt", Mode::Syntax, Reach::Everywhere),
            ("@a deb", Mode::Fuzzy, Reach::Here),
        ];
        for (input, mode, reach) in queries {
            let request = request(input, mode, reach);
            lazy.load_for(&request);
            assert_eq!(hits(&lazy, &request), hits(&eager, &request), "{input:?}");
            let lazy_all = sources(None, &lazy);
            let eager_all = sources(None, &eager);
            assert_eq!(
                suggest(&lazy_all, &request, 8),
                suggest(&eager_all, &request, 8),
                "{input:?}"
            );
        }
        assert!(!hits(&lazy, &request("watcher", Mode::Fuzzy, Reach::Everywhere)).is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_saved_index_that_fails_to_load_leaves_the_shelf_for_good() {
        let dir = temp_dir("fails-late");
        save_current(&dir, &project_dir(&dir, "kept"), "watcher");
        let spoiled = save_current(&dir, &project_dir(&dir, "spoiled"), "watcher");
        let mut shelf = Shelf::list(&dir, &library_path(), None);
        assert_eq!(listed_names(&shelf), ["kept", "spoiled"]);

        fs::write(&spoiled.index, b"overwritten after the listing").unwrap();
        assert_eq!(load_all(&mut shelf), ["kept"]);
        assert_eq!(listed_names(&shelf), ["kept"]);
        assert!(shelf.unloaded(&everywhere()).is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_shelf_loads_saved_indexes_from_disk() {
        let dir = temp_dir("disk");
        let zeta = project_dir(&dir, "Zeta");
        let alpha = project_dir(&dir, "Alpha");
        save_current(&dir, &zeta, "zeta watcher");
        save_current(&dir, &alpha, "alpha watcher");
        save_broken(&dir, &project_dir(&dir, "Broken"));
        fs::write(
            dir.join("0000000000000004.lux"),
            b"orphan without a manifest",
        )
        .unwrap();
        let library = library_path();

        let mut shelf = Shelf::list(&dir, &library, None);
        assert_eq!(listed_names(&shelf), ["Alpha", "Broken", "Zeta"]);
        assert_eq!(load_all(&mut shelf), ["Alpha", "Zeta"]);
        let roots: Vec<&Path> = shelf
            .projects()
            .map(|project| project.root.as_path())
            .collect();
        assert_eq!(roots, [alpha.as_path(), zeta.as_path()]);
        let all = sources(None, &shelf);
        assert_eq!(search(&all, &everywhere(), 10).found.len(), 2);

        let shouted = format!(r"\\?\{}", alpha.to_string_lossy().to_uppercase());
        let mut skipped = Shelf::list(&dir, &library, Some(Path::new(&shouted)));
        let expected: &[&str] = if cfg!(windows) {
            &["Zeta"]
        } else {
            &["Alpha", "Zeta"]
        };
        assert_eq!(load_all(&mut skipped), expected);
        let mut skipped = Shelf::list(&dir, &library, Some(&alpha));
        assert_eq!(load_all(&mut skipped).len(), 1);

        assert_eq!(Shelf::list(&dir.join("missing"), &library, None).len(), 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn reload_rereads_the_saved_indexes_from_where_the_shelf_was_loaded() {
        let dir = temp_dir("reload");
        let library = library_path();
        let kept = project_dir(&dir, "kept");
        let open = project_dir(&dir, "open");
        let dropped = project_dir(&dir, "dropped");
        save_current(&dir, &kept, "watcher");
        save_current(&dir, &open, "watcher");
        save_current(&dir, &dropped, "watcher");
        let mut shelf = Shelf::list(&dir, &library, Some(&open));
        assert_eq!(load_all(&mut shelf), ["dropped", "kept"]);

        fs::remove_dir_all(&dropped).unwrap();
        save_current(&dir, &project_dir(&dir, "added"), "watcher");
        save_current(&dir, &kept, "watcher rewritten");
        shelf.reload();
        assert!(shelf.loaded().is_empty());
        assert_eq!(listed_names(&shelf), ["added", "kept"]);
        assert_eq!(load_all(&mut shelf), ["added", "kept"]);
        let rewritten = request("rewritten", Mode::Fuzzy, Reach::Everywhere);
        assert_eq!(hits(&shelf, &rewritten).len(), 1);

        index_of(&"fresh body ".repeat(40))
            .save(&cache::cache_files(&dir, &kept).index)
            .unwrap();
        shelf.reload();
        assert_eq!(load_all(&mut shelf), ["added"]);

        fs::remove_dir_all(&dir).unwrap();
        shelf.reload();
        assert_eq!(shelf.len(), 0);
    }

    #[test]
    fn reloading_a_shelf_made_from_shards_changes_nothing() {
        let mut shelf = Shelf::from_shards(vec![watchers("beta"), watchers("alpha")]);
        shelf.reload();
        assert_eq!(shelf.loaded(), ["alpha", "beta"]);
        Shelf::default().reload();
    }

    #[test]
    fn a_shelf_skips_an_index_saved_with_another_library_or_rules() {
        let dir = temp_dir("stale");
        save_current(&dir, &project_dir(&dir, "fresh"), "watcher");
        save_index(
            &dir,
            &project_dir(&dir, "stale"),
            "rules 0 lib 1 1",
            "watcher",
        );
        let mut shelf = Shelf::list(&dir, &library_path(), None);
        assert_eq!(load_all(&mut shelf), ["fresh"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_shelf_skips_a_torn_index_and_manifest_pair() {
        let dir = temp_dir("torn");
        save_current(&dir, &project_dir(&dir, "whole"), "watcher");
        let torn = save_current(&dir, &project_dir(&dir, "torn"), "watcher");
        index_of(&"another body entirely ".repeat(50))
            .save(&torn.index)
            .unwrap();
        let mut shelf = Shelf::list(&dir, &library_path(), None);
        assert_eq!(load_all(&mut shelf), ["whole"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_shelf_skips_a_project_whose_folder_is_gone() {
        let dir = temp_dir("gone");
        save_current(&dir, &project_dir(&dir, "kept"), "watcher");
        let removed = project_dir(&dir, "removed");
        save_current(&dir, &removed, "watcher");
        fs::remove_dir_all(&removed).unwrap();
        let mut shelf = Shelf::list(&dir, &library_path(), None);
        assert_eq!(load_all(&mut shelf), ["kept"]);
        let _ = fs::remove_dir_all(&dir);
    }
}
