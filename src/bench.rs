use std::{
    env,
    error::Error,
    fmt, fs,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant},
};

use crate::{
    cache,
    live::{Live, Mode as Feed},
    lux::DEFAULT_LIBRARY,
    preview::build_preview,
    shelf::{self, Mode, Project, Reach, Shelf, Source, parse_request},
};

const LIMIT: usize = 40;
const SUGGESTIONS: usize = 8;
const KEPT: usize = 20;
const MB: f64 = 1024.0 * 1024.0;
const SHELF_QUERY: &str = "index";

struct Args {
    root: PathBuf,
    cache: PathBuf,
    library: PathBuf,
    queries: Option<PathBuf>,
    out: Option<PathBuf>,
    phase: String,
    runs: usize,
    scope: Option<String>,
    variant: String,
    threads: usize,
    workers: usize,
}

pub fn main(args: &[String]) -> Result<(), Box<dyn Error>> {
    let args = parse(args)?;
    let report = match args.phase.as_str() {
        "all" => {
            let mut report = index(&args)?;
            report.push(("query".into(), child(&args, "query")?));
            report.push(("shelf".into(), child(&args, "shelf")?));
            Json::Obj(report)
        }
        "index" => Json::Obj(index(&args)?),
        "query" => query(&args)?,
        "shelf" => shelf_phase(&args),
        "walk" => walk_phase(&args)?,
        other => return Err(format!("unknown phase {other}").into()),
    };
    let text = report.to_string();
    if let Some(out) = &args.out {
        fs::write(out, &text)?;
    }
    println!("{text}");
    Ok(())
}

fn parse(args: &[String]) -> Result<Args, Box<dyn Error>> {
    let mut root = None;
    let mut cache_dir = None;
    let mut library = env::var_os("LUXCORE_LIB")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    let (mut queries, mut out, mut phase, mut runs) = (None, None, "all".to_owned(), 20);
    let mut scope = None;
    let (mut variant, mut threads) = ("new".to_owned(), 0);
    let mut workers = 0;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let mut value = || args.next().cloned().ok_or(format!("{arg} needs a value"));
        match arg.as_str() {
            "--cache" => cache_dir = Some(PathBuf::from(value()?)),
            "--lib" => library = Some(PathBuf::from(value()?)),
            "--queries" => queries = Some(PathBuf::from(value()?)),
            "--out" => out = Some(PathBuf::from(value()?)),
            "--phase" => phase = value()?,
            "--runs" => runs = value()?.parse()?,
            "--scope" => scope = Some(value()?),
            "--variant" => variant = value()?,
            "--threads" => threads = value()?.parse()?,
            "--workers" => workers = value()?.parse()?,
            _ if root.is_none() => root = Some(PathBuf::from(arg)),
            _ => return Err(format!("unexpected argument {arg}").into()),
        }
    }
    let usage = "usage: recall bench ROOT --cache DIR [--lib PATH] [--queries FILE] [--out FILE] [--phase all|index|query|shelf|walk] [--runs N] [--scope NAME] [--workers N] [--variant new|identity] [--threads N]";
    let cache_dir = cache_dir.ok_or(usage)?;
    fs::create_dir_all(&cache_dir)?;
    let cache_dir = cache_dir.canonicalize()?;
    let real = cache::default_dir().and_then(|dir| dir.canonicalize().ok());
    if real.is_some_and(|real| cache_dir.starts_with(real)) {
        return Err("the bench cache must not be Recall's own cache folder".into());
    }
    Ok(Args {
        root: root.ok_or(usage)?.canonicalize()?,
        cache: cache_dir,
        library: library.unwrap_or_else(|| PathBuf::from(DEFAULT_LIBRARY)),
        queries,
        out,
        phase,
        runs,
        scope,
        variant,
        threads,
        workers,
    })
}

fn walk_phase(args: &Args) -> Result<Json, Box<dyn Error>> {
    use crate::{cache::Stamp, scan, truth::Scope};
    use std::collections::BTreeMap;
    let mut runs = Vec::new();
    let mut maps: Vec<BTreeMap<String, Stamp>> = Vec::new();
    for _ in 0..args.runs.max(1) {
        let mut ticks = 0usize;
        let before = sample();
        let found = match args.variant.as_str() {
            "new" => {
                let tick = |_: usize, _: &str| {
                    ticks += 1;
                    Ok(true)
                };
                let walked = if args.threads == 0 {
                    scan::walk(&args.root, &Scope::ROOT, tick)?
                } else {
                    scan::walk_with(&args.root, &Scope::ROOT, args.threads, tick)?
                };
                walked.ok_or("cancelled")?.found
            }
            "identity" => {
                let old = reference_walk(&args.root)?;
                let new = scan::walk_with(&args.root, &Scope::ROOT, args.threads, |_, _| Ok(true))?
                    .ok_or("cancelled")?
                    .found;
                let again = reference_walk(&args.root)?;
                let keys: std::collections::BTreeSet<&String> =
                    old.keys().chain(new.keys()).chain(again.keys()).collect();
                let volatile: Vec<&String> = keys
                    .iter()
                    .copied()
                    .filter(|k| old.get(*k) != again.get(*k))
                    .collect();
                let differ: Vec<(&String, Option<&Stamp>, Option<&Stamp>)> = keys
                    .iter()
                    .copied()
                    .filter(|k| old.get(*k) != new.get(*k) && new.get(*k) != again.get(*k))
                    .map(|k| (k, old.get(k), new.get(k)))
                    .collect();
                let unexplained: Vec<_> = differ
                    .iter()
                    .filter(|(k, ..)| !volatile.contains(k))
                    .take(20)
                    .collect();
                eprintln!(
                    "identity: old {} new {} again {} exact_old {} exact_again {} volatile {} new_differs_from_both {} unexplained {:?}",
                    old.len(),
                    new.len(),
                    again.len(),
                    old == new,
                    again == new,
                    volatile.len(),
                    differ.len(),
                    unexplained
                );
                eprintln!(
                    "volatile sample: {:?}",
                    volatile.iter().take(10).collect::<Vec<_>>()
                );
                if !unexplained.is_empty() {
                    return Err("new walk differs from both serial walks around it".into());
                }
                new
            }
            other => return Err(format!("unknown variant {other}").into()),
        };
        let after = sample();
        let mut fields = span(&before, &after);
        fields.push(("files".into(), found.len().into()));
        fields.push(("ticks".into(), ticks.into()));
        runs.push(Json::Obj(fields));
        if maps.last().is_some_and(|last| *last != found) {
            eprintln!("warning: run maps differ between runs");
        }
        maps.push(found);
    }
    Ok(Json::obj([
        ("variant", args.variant.as_str().into()),
        ("threads", args.threads.into()),
        ("runs", Json::Arr(runs)),
    ]))
}

/// The one-thread walk the `ignore` crate does. `scan::walk` must find the
/// same files, so its tests and the `identity` variant compare against this.
pub(crate) fn reference_walk(
    root: &Path,
) -> Result<std::collections::BTreeMap<String, cache::Stamp>, Box<dyn Error>> {
    use crate::{cache::Stamp, scan, truth::Scope};
    let key_of = |path: &Path| {
        let relative = path.strip_prefix(root).ok()?;
        let parts: Option<Vec<&str>> = relative.iter().map(|part| part.to_str()).collect();
        Some(parts?.join("/"))
    };
    let prune_root = root.to_path_buf();
    let walker = ignore::WalkBuilder::new(root)
        .hidden(false)
        .require_git(false)
        .follow_links(false)
        .filter_entry(move |entry| {
            let dir = || {
                let relative = entry.path().strip_prefix(&prune_root).ok()?;
                let parts: Option<Vec<&str>> = relative.iter().map(|part| part.to_str()).collect();
                Some(parts?.join("/"))
            };
            entry.depth() == 0
                || !entry.file_type().is_some_and(|kind| kind.is_dir())
                || entry.file_name().to_str().is_some_and(scan::enters_dir)
                    && dir().is_some_and(|dir| Scope::ROOT.enters(&dir))
        })
        .build();
    let mut found = std::collections::BTreeMap::new();
    for entry in walker {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                let fatal = match error.io_error().map(std::io::Error::kind) {
                    Some(std::io::ErrorKind::PermissionDenied) | None => false,
                    Some(std::io::ErrorKind::NotFound) => !root.exists(),
                    Some(_) => true,
                };
                if fatal {
                    return Err(error.into());
                }
                continue;
            }
        };
        let admitted = entry.file_type().is_some_and(|kind| kind.is_file())
            && entry.file_name().to_str().is_some_and(scan::is_indexable);
        let Some(key) = admitted.then(|| key_of(entry.path())).flatten() else {
            continue;
        };
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if Scope::ROOT.covers(&key) && metadata.len() <= scan::MAX_FILE_BYTES {
            found.insert(key, Stamp::of(&metadata));
        }
    }
    Ok(found)
}

fn child(args: &Args, phase: &str) -> Result<Json, Box<dyn Error>> {
    let mut command = Command::new(env::current_exe()?);
    command
        .arg("bench")
        .arg(&args.root)
        .args(["--cache".as_ref(), args.cache.as_os_str()])
        .args(["--lib".as_ref(), args.library.as_os_str()])
        .args(["--phase", phase, "--runs", &args.runs.to_string()]);
    if let Some(queries) = &args.queries {
        command.args(["--queries".as_ref(), queries.as_os_str()]);
    }
    if let Some(scope) = &args.scope {
        command.args(["--scope", scope]);
    }
    let output = command.output()?;
    if !output.status.success() {
        return Err(format!(
            "{phase} phase failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(Json::Raw(
        String::from_utf8(output.stdout)?.trim().to_owned(),
    ))
}

fn index(args: &Args) -> Result<Vec<(String, Json)>, Box<dyn Error>> {
    let files = cache::cache_files(&args.cache, &args.root);
    for path in [&files.index, &files.manifest] {
        if path.exists() {
            fs::remove_file(path)?;
        }
    }
    let start = sample();
    let mut live = Live::headless(&args.root, &args.library, args.cache.clone())?;
    if args.workers > 0 {
        live.set_workers(args.workers);
    }
    let mut walked: Option<(Sample, usize)> = None;
    let mut last: Option<(Sample, usize)> = None;
    live.settle(|progress| {
        if !matches!(progress.mode, Feed::Scanning { .. }) {
            let now = sample();
            walked.get_or_insert((now, progress.total));
            last = Some((now, progress.done));
        }
        Ok(true)
    })?;
    let settled = sample();
    let ((walked, found), (last, done)) = walked.zip(last).ok_or("settle never left the walk")?;
    let in_settle = live.spent();
    if !live.finish() {
        return Err("saving the index failed".into());
    }
    let saved = sample();
    let spent = live.spent();
    thread::sleep(Duration::from_secs(2));
    let steady = sample();

    let secs = Duration::as_secs_f64;
    let last_save = secs(&spent.save) - secs(&in_settle.save);
    let index_s = secs(&settled.since(&walked)) - secs(&in_settle.save)
        + secs(&saved.since(&settled))
        - last_save;
    // Read and prepare are summed over the worker threads. Commit, wait and
    // other are the calling thread's wall time and add up to `index_s`.
    let split = Json::obj([
        ("read_s", secs(&spent.read).into()),
        ("prepare_s", secs(&spent.prepare).into()),
        ("commit_s", secs(&spent.commit).into()),
        ("wait_s", secs(&spent.wait).into()),
        (
            "other_s",
            (index_s - secs(&spent.commit) - secs(&spent.wait)).into(),
        ),
    ]);
    let source_mb = spent.read_bytes as f64 / MB;
    let summary = live.summary();
    let mut index_phase = span(&walked, &last);
    index_phase.extend([
        ("files_in_span".into(), done.into()),
        (
            "files_per_s_in_span".into(),
            (done as f64 / secs(&last.since(&walked))).into(),
        ),
        ("index_s".into(), index_s.into()),
        ("save_inside_settle_s".into(), secs(&in_settle.save).into()),
        (
            "files_per_s".into(),
            (summary.indexed as f64 / index_s).into(),
        ),
        ("source_mb_per_s".into(), (source_mb / index_s).into()),
        ("split".into(), split),
        ("workers".into(), live.workers().into()),
    ]);
    let mut save_phase = span(&settled, &saved);
    save_phase.extend([
        ("save_s".into(), last_save.into()),
        ("lux_mb".into(), file_mb(&files.index).into()),
        ("manifest_mb".into(), file_mb(&files.manifest).into()),
    ]);
    let mut steady_phase = span(&saved, &steady);
    steady_phase.push(("luxcore".into(), luxcore_stats(&live)));
    live.close();
    Ok(vec![
        ("root".into(), args.root.display().to_string().into()),
        (
            "corpus".into(),
            Json::obj([
                ("files_found", found.into()),
                ("indexed", summary.indexed.into()),
                ("unreadable", spent.unreadable.into()),
                ("source_mb", source_mb.into()),
            ]),
        ),
        (
            "logical_cpus".into(),
            thread::available_parallelism()
                .map_or(0, |n| n.get())
                .into(),
        ),
        ("start".into(), Json::Obj(memory(&start))),
        ("walk".into(), Json::Obj(span(&start, &walked))),
        ("settle".into(), Json::Obj(span(&start, &settled))),
        ("index".into(), Json::Obj(index_phase)),
        ("save".into(), Json::Obj(save_phase)),
        ("steady".into(), Json::Obj(steady_phase)),
    ])
}

fn query(args: &Args) -> Result<Json, Box<dyn Error>> {
    let start = sample();
    let live = Live::headless(&args.root, &args.library, args.cache.clone())?;
    let loaded = sample();
    if !live.summary().reused {
        return Err(
            "no saved index for this root in the bench cache; run the index phase first".into(),
        );
    }
    let mut load = span(&start, &loaded);
    load.push(("indexed".into(), live.indexed_files().into()));
    load.push(("luxcore".into(), luxcore_stats(&live)));

    let project = Project::at(&args.root);
    let none = Shelf::default();
    let sources = shelf::sources(Some((&project, live.index())), &none);
    let queries = read_queries(args.queries.as_deref())?;
    let mut timings = Timings::default();
    let mut results = Vec::new();
    for text in &queries {
        for mode in [Mode::Fuzzy, Mode::Syntax] {
            let request = parse_request(text, mode, Reach::Here);
            let kind = if mode == Mode::Fuzzy {
                "fuzzy"
            } else {
                "syntax"
            };
            let (outcome, times) = repeat(args.runs, || shelf::search(&sources, &request, LIMIT));
            if outcome.problem.is_some() {
                results.push(Json::obj([
                    ("query", text.as_str().into()),
                    ("mode", kind.into()),
                    ("invalid", true.into()),
                ]));
                continue;
            }
            timings.add(kind, times);
            if mode == Mode::Fuzzy {
                timings.add(
                    "suggest",
                    repeat(args.runs, || {
                        shelf::suggest(&sources, &request, SUGGESTIONS)
                    })
                    .1,
                );
            }
            if let Some(top) = outcome.found.first() {
                let source: &Source = &sources[top.source];
                timings.add(
                    "preview",
                    repeat(args.runs, || build_preview(source, &top.key, &request)).1,
                );
            }
            let hits = outcome
                .found
                .iter()
                .take(KEPT)
                .map(|found| Json::Arr(vec![found.key.as_str().into(), found.score.into()]));
            results.push(Json::obj([
                ("query", text.as_str().into()),
                ("mode", kind.into()),
                ("hits", Json::Arr(hits.collect())),
            ]));
        }
    }
    let searched = sample();
    let mut search = span(&loaded, &searched);
    search.push(("runs_per_query".into(), args.runs.into()));
    search.push(("latency_ms".into(), timings.report()));
    Ok(Json::obj([
        ("load", Json::Obj(load)),
        ("search", Json::Obj(search)),
        ("results", Json::Arr(results)),
    ]))
}

/// Open cost (listing the other saved projects), then the first query that
/// has `@scope` in scope, then the first query across every project. Each
/// step's span covers loading what it needs plus one search.
fn shelf_phase(args: &Args) -> Json {
    let start = sample();
    let mut shelf = Shelf::list(&args.cache, &args.library, Some(&args.root));
    let listed = sample();
    let mut open = span(&start, &listed);
    open.push(("projects".into(), shelf.len().into()));
    let mut report = vec![("open".to_owned(), Json::Obj(open))];
    let scoped = args
        .scope
        .as_ref()
        .map(|scope| format!("@{scope} {SHELF_QUERY}"));
    let steps = scoped
        .iter()
        .map(|input| ("scoped", input.as_str(), Reach::Here))
        .chain([("everywhere", SHELF_QUERY, Reach::Everywhere)]);
    for (name, input, reach) in steps {
        let request = parse_request(input, Mode::Fuzzy, reach);
        let before = sample();
        shelf.load_for(&request);
        let outcome = shelf::search(&shelf::sources(None, &shelf), &request, LIMIT);
        let after = sample();
        let mut step = span(&before, &after);
        step.extend([
            ("query".into(), input.into()),
            ("loaded".into(), shelf::sources(None, &shelf).len().into()),
            ("searched".into(), outcome.searched.len().into()),
            ("hits".into(), outcome.found.len().into()),
        ]);
        report.push((name.to_owned(), Json::Obj(step)));
    }
    Json::Obj(report)
}

fn repeat<T>(runs: usize, mut call: impl FnMut() -> T) -> (T, Vec<Duration>) {
    let mut times = Vec::with_capacity(runs);
    let mut last = None;
    for _ in 0..runs.max(1) {
        let started = Instant::now();
        last = Some(call());
        times.push(started.elapsed());
    }
    (last.expect("at least one run"), times)
}

#[derive(Default)]
struct Timings {
    kinds: Vec<(&'static str, Vec<Duration>, Vec<Duration>)>,
}

impl Timings {
    fn add(&mut self, kind: &'static str, times: Vec<Duration>) {
        let index = match self.kinds.iter().position(|(name, ..)| *name == kind) {
            Some(index) => index,
            None => {
                self.kinds.push((kind, Vec::new(), Vec::new()));
                self.kinds.len() - 1
            }
        };
        let (_, first, rest) = &mut self.kinds[index];
        first.push(times[0]);
        rest.extend_from_slice(&times[1..]);
    }

    fn report(&self) -> Json {
        Json::Obj(
            self.kinds
                .iter()
                .map(|(kind, first, rest)| {
                    let stats = Json::obj([
                        ("first_run", percentiles(first)),
                        ("repeated", percentiles(rest)),
                    ]);
                    (kind.to_string(), stats)
                })
                .collect(),
        )
    }
}

fn percentiles(times: &[Duration]) -> Json {
    let mut ms: Vec<f64> = times
        .iter()
        .map(|time| time.as_secs_f64() * 1000.0)
        .collect();
    ms.sort_by(f64::total_cmp);
    let at = |q: f64| {
        ms.get(((ms.len().saturating_sub(1)) as f64 * q).round() as usize)
            .copied()
            .unwrap_or(0.0)
    };
    Json::obj([
        ("n", ms.len().into()),
        ("p50", at(0.5).into()),
        ("p95", at(0.95).into()),
        ("max", at(1.0).into()),
    ])
}

fn read_queries(path: Option<&Path>) -> Result<Vec<String>, Box<dyn Error>> {
    let text = match path {
        Some(path) => fs::read_to_string(path)?,
        None => "index\nsearch query\nerror\nconfig\n".to_owned(),
    };
    Ok(text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_owned)
        .collect())
}

fn luxcore_stats(live: &Live) -> Json {
    live.index().debug_stats().map_or(Json::Null, Json::Raw)
}

fn file_mb(path: &Path) -> f64 {
    fs::metadata(path).map_or(0.0, |metadata| metadata.len() as f64 / MB)
}

#[derive(Clone, Copy)]
struct Sample {
    at: Instant,
    cpu: Duration,
    kernel: Duration,
    working: u64,
    peak: u64,
    private: u64,
    peak_private: u64,
}

impl Sample {
    fn since(&self, earlier: &Sample) -> Duration {
        self.at - earlier.at
    }
}

fn memory(at: &Sample) -> Vec<(String, Json)> {
    vec![
        ("working_set_mb".into(), (at.working as f64 / MB).into()),
        ("private_mb".into(), (at.private as f64 / MB).into()),
        ("peak_working_set_mb".into(), (at.peak as f64 / MB).into()),
        (
            "peak_private_mb".into(),
            (at.peak_private as f64 / MB).into(),
        ),
    ]
}

fn span(from: &Sample, to: &Sample) -> Vec<(String, Json)> {
    let wall = to.since(from).as_secs_f64();
    let cpu = (to.cpu - from.cpu).as_secs_f64();
    let mut fields = vec![
        ("wall_s".into(), wall.into()),
        ("cpu_s".into(), cpu.into()),
        ("effective_cores".into(), (cpu / wall).into()),
        (
            "kernel_s".into(),
            (to.kernel - from.kernel).as_secs_f64().into(),
        ),
        (
            "working_set_delta_mb".into(),
            ((to.working as f64 - from.working as f64) / MB).into(),
        ),
        (
            "private_delta_mb".into(),
            ((to.private as f64 - from.private as f64) / MB).into(),
        ),
    ];
    fields.extend(memory(to));
    fields
}

#[cfg(windows)]
fn sample() -> Sample {
    use windows_sys::Win32::{
        Foundation::FILETIME,
        System::{
            ProcessStatus::{
                GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
            },
            Threading::{GetCurrentProcess, GetProcessTimes},
        },
    };
    let at = Instant::now();
    let zero = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
    let mut counters: PROCESS_MEMORY_COUNTERS_EX = unsafe { std::mem::zeroed() };
    unsafe {
        let process = GetCurrentProcess();
        GetProcessTimes(process, &mut created, &mut exited, &mut kernel, &mut user);
        GetProcessMemoryInfo(
            process,
            (&mut counters as *mut PROCESS_MEMORY_COUNTERS_EX).cast::<PROCESS_MEMORY_COUNTERS>(),
            size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        );
    }
    let ticks =
        |time: FILETIME| (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime);
    Sample {
        at,
        cpu: Duration::from_nanos((ticks(kernel) + ticks(user)) * 100),
        kernel: Duration::from_nanos(ticks(kernel) * 100),
        working: counters.WorkingSetSize as u64,
        peak: counters.PeakWorkingSetSize as u64,
        private: counters.PrivateUsage as u64,
        peak_private: counters.PeakPagefileUsage as u64,
    }
}

#[cfg(not(windows))]
fn sample() -> Sample {
    Sample {
        at: Instant::now(),
        cpu: Duration::ZERO,
        kernel: Duration::ZERO,
        working: 0,
        peak: 0,
        private: 0,
        peak_private: 0,
    }
}

enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
    Raw(String),
}

impl Json {
    fn obj<const N: usize>(fields: [(&str, Json); N]) -> Json {
        Json::Obj(
            fields
                .into_iter()
                .map(|(key, value)| (key.to_owned(), value))
                .collect(),
        )
    }
}

impl From<f64> for Json {
    fn from(value: f64) -> Self {
        Json::Num(value)
    }
}

impl From<usize> for Json {
    fn from(value: usize) -> Self {
        Json::Num(value as f64)
    }
}

impl From<bool> for Json {
    fn from(value: bool) -> Self {
        Json::Bool(value)
    }
}

impl From<&str> for Json {
    fn from(value: &str) -> Self {
        Json::Str(value.to_owned())
    }
}

impl From<String> for Json {
    fn from(value: String) -> Self {
        Json::Str(value)
    }
}

impl fmt::Display for Json {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Json::Null => f.write_str("null"),
            Json::Bool(value) => write!(f, "{value}"),
            Json::Num(value) if value.is_finite() => write!(f, "{value}"),
            Json::Num(_) => f.write_str("null"),
            Json::Str(value) => write_string(f, value),
            Json::Raw(value) => f.write_str(value),
            Json::Arr(items) => {
                f.write_str("[")?;
                for (position, item) in items.iter().enumerate() {
                    if position > 0 {
                        f.write_str(",")?;
                    }
                    write!(f, "{item}")?;
                }
                f.write_str("]")
            }
            Json::Obj(fields) => {
                f.write_str("{")?;
                for (position, (key, value)) in fields.iter().enumerate() {
                    if position > 0 {
                        f.write_str(",")?;
                    }
                    write_string(f, key)?;
                    write!(f, ":{value}")?;
                }
                f.write_str("}")
            }
        }
    }
}

fn write_string(f: &mut fmt::Formatter<'_>, value: &str) -> fmt::Result {
    f.write_str("\"")?;
    for ch in value.chars() {
        match ch {
            '"' => f.write_str("\\\"")?,
            '\\' => f.write_str("\\\\")?,
            ch if u32::from(ch) < 0x20 => write!(f, "\\u{:04x}", u32::from(ch))?,
            ch => write!(f, "{ch}")?,
        }
    }
    f.write_str("\"")
}
