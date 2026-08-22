//! What a query costs while the store it is reading is being written to.
//!
//! Run it with `cargo run --release --example serving -- <corpus> [<directory>]`,
//! where the corpus is a directory of text files and the directory is where the
//! store goes, defaulting to the temporary directory.
//!
//! Every other measurement in this repository is of one thing at a time. A run
//! indexes and then it is asked questions, or it is asked questions and nothing
//! is writing. That is not the case a store is built for, and it hides the
//! question this example is for: what does a reader pay while an ingest is
//! going on beside it, and how much of what it pays is the folding rather than
//! the writing.
//!
//! Three conditions, the same queries in each:
//!
//! - quiet, nothing writing, which is the number every other measurement here
//!   reports and the floor for the other two
//! - writing, threads handing batches over as fast as they can fill them, with
//!   nothing folding, so the segment count climbs for the whole run
//! - writing and folding, the same threads with a keeper beside them, which
//!   holds the segment count down and takes the store to do it
//!
//! The second and third are the pair that matters. Folding is not free for a
//! reader: it takes the store for the length of a fold, and it rewrites the
//! segments the reader is about to open. Not folding is not free either, for
//! the plainer reason that every query walks every segment. Which of those two
//! costs more is the thing to measure rather than to reason about, and it is
//! what a rate limit would be tuned against.
//!
//! The queries come out of the corpus rather than being made up. The first pass
//! counts what the analyser produces and takes the terms at ranks 1, 10, 100,
//! 1,000 and 10,000, which is a spread from a term in nearly every document to
//! one in a handful. A query set of only common terms measures the scorer and a
//! query set of only rare ones measures the dictionary, and the interesting
//! thing here is neither of those on its own.
//!
//! What each query costs is the whole of what a reader pays to see the newest
//! commit: taking the view, opening a reader over each of its segments and
//! running the query. A server that held one searcher open would pay less than
//! this and would be answering out of date. The segment count is in that cost,
//! which is the point.
//!
//! The slowest query of a round is not reported, though it used to be. Now that
//! a query is a few microseconds, the largest number in a million of them is the
//! scheduler taking the core away rather than the store doing anything, and it
//! came out at 1.8, 15.2 and 17.5 milliseconds across three runs of the same
//! thing. A number that moves by ten times between identical runs is not
//! measuring the thing it is printed beside.

// Every cast here feeds a printed number that is already approximate.
#![allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use kura_core::analysis::Analyzer;
use kura_core::file::Store;
use kura_core::ingest::Batch;
use kura_core::keeper::Keeper;
use kura_core::search::Searcher;
use kura_core::writer::Writer;

/// A store identifier, so a file written by this says what wrote it.
const STORE: u128 = 0x006b_7572_612d_7365_7276_696e_6700_0001;

/// How many threads fill batches in the two writing conditions.
///
/// Four rather than the core count, because the question is what a reader pays
/// while an ingest is going on and not how fast the ingest can be made to go. A
/// run that took every core would be measuring the scheduler.
const THREADS: usize = 4;

/// How much text goes into a batch before it is handed over.
///
/// Small, so that commits happen often enough over the run for a reader to be
/// asking questions between them rather than during one long one.
const BUDGET: u64 = 4 * 1024 * 1024;

/// How many documents to hold back from the first pass for the writing to add.
///
/// The store has to hold something before a query means anything, and the run
/// has to have something left to write. Half each.
const SHARE: usize = 2;

/// The ranks in the corpus vocabulary the queries are taken from.
const RANKS: [usize; 5] = [1, 10, 100, 1_000, 10_000];

/// How long the quiet condition runs for.
///
/// The writing conditions stop when the documents run out, and this is what
/// stops the quiet one, which has nothing to run out of.
const QUIET_FOR: Duration = Duration::from_secs(1);

/// How many times each condition is run, with the times pooled.
///
/// A writing condition lasts as long as the writing does, which is a few
/// hundred milliseconds, and a few hundred queries is not enough samples for a
/// p99 to mean anything. Repeating the condition from the same starting store
/// is the way to get them, since making the run longer would be measuring a
/// larger corpus rather than the same one for longer.
const ROUNDS: usize = 5;

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(corpus) = args.next().map(PathBuf::from) else {
        eprintln!("usage: serving <corpus> [<directory>]");
        eprintln!("the corpus is a directory of text files, and the store goes in the directory");
        std::process::exit(2);
    };
    let directory = args.next().map_or_else(std::env::temp_dir, PathBuf::from);

    match run(&corpus, &directory) {
        Ok(()) => (),
        Err(problem) => {
            eprintln!("serving: {problem}");
            std::process::exit(1);
        }
    }
}

/// The whole measurement.
fn run(corpus: &Path, directory: &Path) -> Result<(), String> {
    let mut files = Vec::new();
    walk(corpus, &mut files)?;
    files.sort();
    if files.len() < 32 {
        return Err(format!(
            "{} holds {} files, which is not enough to measure anything",
            corpus.display(),
            files.len()
        ));
    }
    let split = files.len() / SHARE;
    let (first, rest) = files.split_at(split);

    println!("corpus        {}", corpus.display());
    println!(
        "files         {} in all, {} indexed first and {} written while the queries run",
        files.len(),
        first.len(),
        rest.len()
    );

    let base = directory.join("kura-serving-base.kura");
    let (queries, bytes) = fill(&base, first)?;
    println!("text          {} MB in the first pass", bytes / 1_000_000);
    println!(
        "queries       {}",
        queries
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!();
    println!(
        "{:<22} {:>9} {:>7} {:>11} {:>11} {:>11}",
        "condition", "queries", "segs", "median", "p95", "p99"
    );

    let quiet = rounds(&base, directory, "quiet", &queries, None)?;
    quiet.tell("quiet");
    let writing = rounds(&base, directory, "writing", &queries, Some((rest, false)))?;
    writing.tell("writing");
    let folding = rounds(&base, directory, "folding", &queries, Some((rest, true)))?;
    folding.tell("writing and folding");

    println!();
    if let (Some(w), Some(f)) = (writing.wrote_in(), folding.wrote_in()) {
        println!(
            "the writing took {w:.2?} with nothing folding and {f:.2?} with a keeper beside it, medians of {ROUNDS} rounds"
        );
    }
    println!(
        "against quiet, writing costs {:.0} percent at the median and {:.0} at p99, and folding beside it {:.0} and {:.0}",
        share(writing.median(), quiet.median()),
        share(writing.p99(), quiet.p99()),
        share(folding.median(), quiet.median()),
        share(folding.p99(), quiet.p99()),
    );

    std::fs::remove_file(&base).ok();
    Ok(())
}

/// How much more one time is than another, as a percentage.
fn share(now: f64, before: f64) -> f64 {
    if before <= 0.0 {
        return 0.0;
    }
    100.0 * (now - before) / before
}

/// Every file under a directory, in no particular order.
fn walk(at: &Path, into: &mut Vec<PathBuf>) -> Result<(), String> {
    let listing =
        std::fs::read_dir(at).map_err(|problem| format!("{}: {problem}", at.display()))?;
    for entry in listing {
        let entry = entry.map_err(|problem| format!("{}: {problem}", at.display()))?;
        let path = entry.path();
        let kind = entry
            .file_type()
            .map_err(|problem| format!("{}: {problem}", path.display()))?;
        if kind.is_dir() {
            walk(&path, into)?;
        } else if kind.is_file() {
            into.push(path);
        }
    }
    Ok(())
}

/// The text of a file, if it is text.
fn text_of(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    if bytes.len() > 4 * 1024 * 1024 || bytes.contains(&0) {
        return None;
    }
    String::from_utf8(bytes).ok()
}

/// Indexes the first share of the corpus and picks the queries out of it.
///
/// One segment when it returns, which is the store the three conditions all
/// start from. A baseline taken against a store that was already several
/// segments would be measuring the segments rather than the writing.
fn fill(path: &Path, files: &[PathBuf]) -> Result<(Vec<String>, u64), String> {
    std::fs::remove_file(path).ok();
    let mut store =
        Store::create(path, STORE, 1_700_000_000).map_err(|problem| problem.to_string())?;
    let mut analyzer = Analyzer::new();
    let mut counts: HashMap<Vec<u8>, u64> = HashMap::new();
    let mut bytes = 0u64;

    let mut at = 0;
    while at < files.len() {
        let view = store.view().map_err(|problem| problem.to_string())?;
        let mut batch = Batch::with_budget(&view, BUDGET).map_err(|problem| problem.to_string())?;
        while at < files.len() {
            let path = &files[at];
            at += 1;
            let Some(text) = text_of(path) else { continue };
            bytes += text.len() as u64;
            analyzer.analyze(&text, |term, _| {
                *counts.entry(term.to_vec()).or_default() += 1;
            });
            let key = path.to_string_lossy().into_owned();
            batch
                .add_keyed(key.as_bytes(), &text)
                .map_err(|problem| problem.to_string())?;
            if batch.is_full() {
                break;
            }
        }
        if batch.is_empty() {
            break;
        }
        let prepared = batch.finish().map_err(|problem| problem.to_string())?;
        drop(view);
        prepared
            .commit(&mut store, 1_700_000_001, 1)
            .map_err(|problem| problem.to_string())?;
    }

    let segments = store.manifest().segments.len();
    if segments > 1 {
        store
            .compact(0..segments, 1_700_000_002, 2)
            .map_err(|problem| problem.to_string())?;
    }
    drop(store);

    let mut vocabulary: Vec<_> = counts.into_iter().collect();
    vocabulary.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    let queries = RANKS
        .iter()
        .filter_map(|rank| vocabulary.get(rank - 1))
        .filter_map(|(term, _)| String::from_utf8(term.clone()).ok())
        .collect();
    Ok((queries, bytes))
}

/// What one condition came to.
#[derive(Default)]
struct Answered {
    /// How long each query took, in milliseconds.
    times: Vec<f64>,
    /// How many segments the store held when it was over.
    segments: usize,
    /// How long the writing took in each round, on the two conditions that
    /// wrote.
    ingest: Vec<Duration>,
}

impl Answered {
    /// The middle of what the writing took, over the rounds.
    fn wrote_in(&self) -> Option<Duration> {
        if self.ingest.is_empty() {
            return None;
        }
        let mut sorted = self.ingest.clone();
        sorted.sort_unstable();
        Some(sorted[sorted.len() / 2])
    }

    /// The middle of the times, in microseconds.
    fn median(&self) -> f64 {
        self.at(50)
    }

    /// The ninety ninth percentile, in microseconds.
    fn p99(&self) -> f64 {
        self.at(99)
    }

    /// The time at a percentile, in microseconds.
    fn at(&self, percentile: usize) -> f64 {
        if self.times.is_empty() {
            return 0.0;
        }
        let mut sorted = self.times.clone();
        sorted.sort_by(f64::total_cmp);
        sorted[(sorted.len() - 1) * percentile / 100]
    }

    /// Prints it.
    fn tell(&self, name: &str) {
        println!(
            "{:<22} {:>9} {:>7} {:>8.1} µs {:>8.1} µs {:>8.1} µs",
            name,
            self.times.len(),
            self.segments,
            self.median(),
            self.at(95),
            self.p99(),
        );
    }
}

/// Runs one condition [`ROUNDS`] times and pools what the rounds answered.
///
/// The segment count and the ingest time are the last round's rather than a
/// total, because both of them are the same in every round and a sum of five
/// identical things says less than one of them.
fn rounds(
    base: &Path,
    directory: &Path,
    name: &str,
    queries: &[String],
    writing: Option<(&[PathBuf], bool)>,
) -> Result<Answered, String> {
    let mut pooled = Answered::default();
    for _ in 0..ROUNDS {
        let round = measure(base, directory, name, queries, writing)?;
        pooled.times.extend(round.times);
        pooled.segments = round.segments;
        pooled.ingest.extend(round.ingest);
    }
    Ok(pooled)
}

/// Runs one condition against a copy of the base store.
///
/// `writing` is the files to add and whether a keeper runs beside the threads
/// adding them, or nothing at all for the quiet condition.
fn measure(
    base: &Path,
    directory: &Path,
    name: &str,
    queries: &[String],
    writing: Option<(&[PathBuf], bool)>,
) -> Result<Answered, String> {
    let path = directory.join(format!("kura-serving-{name}.kura"));
    std::fs::remove_file(&path).ok();
    std::fs::copy(base, &path).map_err(|problem| format!("{}: {problem}", path.display()))?;
    let store = Store::open(&path).map_err(|problem| problem.to_string())?;
    let writer = Writer::new(store).map_err(|problem| problem.to_string())?;

    let stop = AtomicBool::new(false);
    let next = AtomicUsize::new(0);
    let keeper = Keeper::new(&writer);
    let outcome = std::thread::scope(|scope| {
        let asking = {
            let (writer, stop) = (&writer, &stop);
            scope.spawn(move || ask(writer, queries, stop))
        };
        let started = Instant::now();
        if let Some((files, folding)) = writing {
            let keeper = &keeper;
            let keeping = folding.then(|| scope.spawn(move || keeper.run(|| 1_700_000_003)));
            let running: Vec<_> = (0..THREADS)
                .map(|_| {
                    let (writer, next) = (&writer, &next);
                    scope.spawn(move || add(writer, files, next))
                })
                .collect();
            for handle in running {
                handle
                    .join()
                    .unwrap_or_else(|_| Err("a writer stopped".into()))?;
            }
            keeper.stop();
            if let Some(keeping) = keeping {
                keeping
                    .join()
                    .map_err(|_| "the folding thread stopped".to_string())?
                    .map_err(|problem| problem.to_string())?;
            }
        } else {
            std::thread::sleep(QUIET_FOR);
        }
        let took = started.elapsed();
        stop.store(true, Ordering::Release);
        let times = asking
            .join()
            .unwrap_or_else(|_| Err("a reader stopped".into()))?;
        Ok::<_, String>((times, took))
    });

    let (times, took) = outcome?;
    let segments = writer.view().len();
    drop(writer);
    std::fs::remove_file(&path).ok();

    Ok(Answered {
        times,
        segments,
        ingest: writing.map(|_| took).into_iter().collect(),
    })
}

/// The reader, asking the same questions over and over until it is told to
/// stop, and timing each of them.
fn ask(writer: &Writer, queries: &[String], stop: &AtomicBool) -> Result<Vec<f64>, String> {
    let mut times = Vec::new();
    while !stop.load(Ordering::Acquire) {
        for query in queries {
            let started = Instant::now();
            // The whole of what a reader pays to see the newest commit, which
            // is where the segment count shows up.
            let view = writer.view();
            let readers = view.readers().map_err(|problem| problem.to_string())?;
            let searcher = Searcher::over(&readers).map_err(|problem| problem.to_string())?;
            let _ = searcher
                .search(query, 10)
                .map_err(|problem| problem.to_string())?;
            times.push(started.elapsed().as_secs_f64() * 1_000_000.0);
        }
    }
    Ok(times)
}

/// One writer, taking files off the shared counter until they run out.
fn add(writer: &Writer, files: &[PathBuf], next: &AtomicUsize) -> Result<(), String> {
    let mut again: Vec<usize> = Vec::new();
    let mut drained = false;
    loop {
        let view = writer.view();
        let mut batch = Batch::with_budget(&view, BUDGET).map_err(|problem| problem.to_string())?;
        let mut taken = Vec::new();
        let retrying = !again.is_empty();
        for at in again.drain(..) {
            let Some(text) = files.get(at).and_then(|path| text_of(path)) else {
                continue;
            };
            let key = files[at].to_string_lossy().into_owned();
            batch
                .add_keyed(key.as_bytes(), &text)
                .map_err(|problem| problem.to_string())?;
            taken.push(at);
        }
        while !drained && !retrying {
            let at = next.fetch_add(1, Ordering::Relaxed);
            let Some(path) = files.get(at) else {
                drained = true;
                break;
            };
            let Some(text) = text_of(path) else { continue };
            let key = path.to_string_lossy().into_owned();
            batch
                .add_keyed(key.as_bytes(), &text)
                .map_err(|problem| problem.to_string())?;
            taken.push(at);
            if batch.is_full() {
                break;
            }
        }
        if !batch.is_empty() {
            let prepared = batch.finish().map_err(|problem| problem.to_string())?;
            drop(view);
            match writer.commit(prepared, 1_700_000_004, 1) {
                Ok(_) => (),
                // Two folds went past while this was being filled, so there is
                // nothing left to shift it by and it is filled again.
                Err(kura_core::file::Trouble::Format(kura_core::Error::StaleView { .. })) => {
                    again = taken;
                }
                Err(other) => return Err(other.to_string()),
            }
        }
        if drained && again.is_empty() {
            return Ok(());
        }
    }
}
