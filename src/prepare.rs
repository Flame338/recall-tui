use std::{
    collections::VecDeque,
    fs, io,
    num::NonZero,
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex, MutexGuard, PoisonError,
        mpsc::{Receiver, Sender, channel},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use crate::lux::{Prepared, Preparer};

// A read of a file the OS has not cached blocks for 4 to 5 ms (first-touch disk
// read plus the virus scanner), so on a first index the threads mostly wait.
// Measured on 2,500 cold files, files per second at 1, 3, 8, 16 and 24 threads:
// 208, 701, 1,735, 2,798 and 3,414 on NVMe, and 185, 588, 1,395, 1,794 and
// 2,578 on a hard disk. Warm, the serial commit limits the index and 8 to 16
// threads tie, so the extra threads cost only their stacks. How far the window
// lets them run ahead, not their number, bounds the memory.
const MIN_WORKERS: usize = 8;
const MAX_WORKERS: usize = 16;

pub enum Body {
    /// Tokenized and waiting for `LuxIndex::commit`. `bytes` is the text's size.
    Ready {
        document: Prepared,
        bytes: u64,
    },
    /// Read, but LuxCore would not take it.
    Refused {
        bytes: u64,
    },
    Unread(io::ErrorKind),
}

/// One file read from disk and tokenized, with what each step cost.
pub struct Fetched {
    pub body: Body,
    pub read: Duration,
    pub prepare: Duration,
}

/// Reads `root/key` and tokenizes it. Touches no shared state, so any thread
/// may run it while another commits to the same index.
pub fn fetch(preparer: &Preparer, root: &Path, key: &str) -> Fetched {
    let reading = Instant::now();
    let text = fs::read_to_string(root.join(key));
    let read = reading.elapsed();
    let preparing = Instant::now();
    let body = match text {
        Ok(text) => {
            let bytes = text.len() as u64;
            let title = key.rsplit('/').next().unwrap_or(key);
            match preparer.prepare(title, &text) {
                Ok(document) => Body::Ready { document, bytes },
                Err(_) => Body::Refused { bytes },
            }
        }
        Err(error) => Body::Unread(error.kind()),
    };
    Fetched {
        body,
        read,
        prepare: preparing.elapsed(),
    }
}

pub fn default_workers() -> usize {
    let cpus = thread::available_parallelism().map_or(1, NonZero::get);
    cpus.clamp(MIN_WORKERS, MAX_WORKERS)
}

#[derive(Default)]
struct Queue {
    asks: VecDeque<(u64, String)>,
    closed: bool,
}

struct Shared {
    root: PathBuf,
    preparer: Preparer,
    queue: Mutex<Queue>,
    wake: Condvar,
}

impl Shared {
    fn queue(&self) -> MutexGuard<'_, Queue> {
        self.queue.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Worker threads that fetch files. Each ask is answered once, under the
/// ticket it was asked with, in whatever order the fetches finish. A worker
/// starts only when every started one is busy, and sleeps when none is asked.
pub struct Pool {
    shared: Arc<Shared>,
    answers: Receiver<(u64, Fetched)>,
    answer_to: Sender<(u64, Fetched)>,
    workers: Vec<JoinHandle<()>>,
    limit: usize,
    unanswered: usize,
}

impl Pool {
    pub fn new(root: PathBuf, preparer: Preparer, limit: usize) -> Self {
        let (answer_to, answers) = channel();
        Self {
            shared: Arc::new(Shared {
                root,
                preparer,
                queue: Mutex::default(),
                wake: Condvar::new(),
            }),
            answers,
            answer_to,
            workers: Vec::new(),
            limit: limit.max(1),
            unanswered: 0,
        }
    }

    pub fn ask(&mut self, ticket: u64, key: String) {
        if self.unanswered >= self.workers.len() && self.workers.len() < self.limit {
            let (shared, answer_to) = (Arc::clone(&self.shared), self.answer_to.clone());
            let spawned = thread::Builder::new()
                .name("recall-prepare".into())
                .spawn(move || serve(&shared, &answer_to));
            self.workers.extend(spawned.ok());
        }
        self.unanswered += 1;
        if self.workers.is_empty() {
            // No thread could start, so this one does the work.
            let fetched = fetch(&self.shared.preparer, &self.shared.root, &key);
            let _ = self.answer_to.send((ticket, fetched));
            return;
        }
        self.shared.queue().asks.push_back((ticket, key));
        self.shared.wake.notify_one();
    }

    /// The next finished fetch, waiting up to `wait` for one that is still out.
    pub fn answer(&mut self, wait: Duration) -> Option<(u64, Fetched)> {
        if self.unanswered == 0 {
            return None;
        }
        let answer = if wait.is_zero() {
            self.answers.try_recv().ok()
        } else {
            self.answers.recv_timeout(wait).ok()
        }?;
        self.unanswered -= 1;
        Some(answer)
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        {
            let mut queue = self.shared.queue();
            queue.closed = true;
            queue.asks.clear();
        }
        self.shared.wake.notify_all();
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

fn serve(shared: &Shared, answer_to: &Sender<(u64, Fetched)>) {
    loop {
        let (ticket, key) = {
            let mut queue = shared.queue();
            loop {
                if queue.closed {
                    return;
                }
                if let Some(ask) = queue.asks.pop_front() {
                    break ask;
                }
                queue = shared
                    .wake
                    .wait(queue)
                    .unwrap_or_else(PoisonError::into_inner);
            }
        };
        let fetched = fetch(&shared.preparer, &shared.root, &key);
        if answer_to.send((ticket, fetched)).is_err() {
            return;
        }
    }
}
