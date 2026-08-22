//! What a query costs while the store it is reading is being written to.
//!
//! Run it with `cargo run --release --example serving -- <corpus> [<directory>]
//! [threads=<n>] [<queries per second> ...]`, where the corpus is a directory
//! of text files and the directory is where the store goes, defaulting to the
//! temporary directory. Every argument that is a number is an offered rate and
//! the whole measurement is run again at each of them.
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
//! With no rate given the reader asks as fast as it can, which reports the
//! latency of a saturated reader and is a different question from the one an
//! operator asks. A budget is a promise about the latency at a load somebody
//! chose, so the reader can also be given a rate and will hold to it: the
//! hundredth query is due a hundred gaps after the first whatever the
//! ninety ninth cost, and if the reader is behind it does not wait.
//!
//! Two numbers come out of a run at a rate and both are printed. What the query
//! cost is the store's answer and it is what the table holds. What the client
//! waited is that plus the time the query sat waiting for its turn, measured
//! from when it was due rather than from when it started, and it goes on a line
//! under the table. A reader that sleeps, wakes, and starts its clock on waking
//! reports the first and calls it the second, and it reports a store that is
//! keeping up long after it has stopped keeping up, because the queries it
//! could not get to are the slow ones and they are the ones it left out.
//!
//! The wait is the number a budget is written against, and on an idle machine
//! most of it is not the store. A reader that asks to be woken in a hundred
//! microseconds is woken late, by about a third of whatever it asked for on
//! this operating system, which is timer coalescing rather than anything here.
//! The tail of it is worse than that: three runs of the quiet condition at ten
//! thousand queries a second, where the query itself is twenty microseconds at
//! p99 every time, put the wait at p99 at 21,170, 269 and 532 microseconds.
//! That is the reader thread being taken off its core and it says nothing about
//! a store. So the wait is printed with the quiet condition first, and a
//! conclusion drawn from it wants the gap between the conditions to be larger
//! than the gap between two runs of the same one.
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

/// How many threads fill batches in the two writing conditions, unless the
/// command line says otherwise.
///
/// Four rather than the core count, because the question is what a reader pays
/// while an ingest is going on and not how fast the ingest can be made to go. A
/// run that took every core would be measuring the scheduler.
///
/// Taking every core is worth doing on purpose, though, which is what
/// `threads=<n>` is for. Everything folding costs a reader on a machine with
/// cores to spare is the cores it takes, so the case that would argue for a
/// rate limit is the one where there are none, and setting this to the core
/// count is that case without needing a smaller machine to run it on.
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

/// How close to the offered rate a condition has to come to count as keeping up.
///
/// Not one, because a round is a few hundred milliseconds and the last query of
/// it is cut off partway, which costs a fraction of a percent whatever the store
/// is doing.
const KEEPING_UP: f64 = 0.99;

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(corpus) = args.next().map(PathBuf::from) else {
        eprintln!("usage: serving <corpus> [<directory>] [threads=<n>] [<queries per second> ...]");
        eprintln!("the corpus is a directory of text files, and the store goes in the directory");
        eprintln!("a rate is how many queries a second the reader offers, and without one it asks");
        eprintln!("as fast as it can");
        std::process::exit(2);
    };

    // Anything that reads as a number is a rate, anything of the shape
    // threads=<n> is the writer count, and anything else is where the store
    // goes, so the optional arguments can be given in any order.
    let mut directory = std::env::temp_dir();
    let mut rates = Vec::new();
    let mut threads = THREADS;
    for arg in args {
        if let Some(count) = arg.strip_prefix("threads=") {
            match count.parse::<usize>() {
                Ok(count) if count > 0 => threads = count,
                _ => {
                    eprintln!("serving: {count} is not a number of threads");
                    std::process::exit(2);
                }
            }
        } else if let Ok(rate) = arg.parse::<f64>() {
            if rate <= 0.0 || !rate.is_finite() {
                eprintln!("serving: {rate} is not a rate anybody could offer");
                std::process::exit(2);
            }
            rates.push(rate);
        } else {
            directory = PathBuf::from(arg);
        }
    }

    match run(&corpus, &directory, &rates, threads) {
        Ok(()) => (),
        Err(problem) => {
            eprintln!("serving: {problem}");
            std::process::exit(1);
        }
    }
}

/// The whole measurement.
fn run(corpus: &Path, directory: &Path, rates: &[f64], threads: usize) -> Result<(), String> {
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
    println!("writers       {threads} filling batches in the two writing conditions");
    if rates.is_empty() {
        conditions(&base, directory, &queries, rest, None, threads)?;
    } else {
        for rate in rates {
            conditions(&base, directory, &queries, rest, Some(*rate), threads)?;
        }
    }

    std::fs::remove_file(&base).ok();
    Ok(())
}

/// The three conditions at one offered rate, or at no rate at all.
fn conditions(
    base: &Path,
    directory: &Path,
    queries: &[String],
    rest: &[PathBuf],
    rate: Option<f64>,
    threads: usize,
) -> Result<(), String> {
    println!();
    match rate {
        Some(rate) => println!("offered       {rate:.0} queries a second"),
        None => println!("offered       as fast as the reader can ask"),
    }
    println!(
        "{:<22} {:>9} {:>6} {:>9} {:>11} {:>11} {:>11}",
        "condition", "queries", "segs", "q/s", "median", "p95", "p99"
    );

    let quiet = rounds(base, directory, "quiet", queries, None, rate, threads)?;
    quiet.tell("quiet");
    let writing = rounds(
        base,
        directory,
        "writing",
        queries,
        Some((rest, false)),
        rate,
        threads,
    )?;
    writing.tell("writing");
    let folding = rounds(
        base,
        directory,
        "folding",
        queries,
        Some((rest, true)),
        rate,
        threads,
    )?;
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
    if let Some(rate) = rate {
        // A condition that did not hold the offered rate has a queue that grew
        // for the whole round, so its percentiles are how long the round was
        // rather than how long a query takes, and saying so is the difference
        // between a measurement and a misleading table.
        for (name, answered) in [
            ("quiet", &quiet),
            ("writing", &writing),
            ("writing and folding", &folding),
        ] {
            if answered.rate() < rate * KEEPING_UP {
                println!(
                    "{name} got through {:.0} of the {rate:.0} offered, so it did not keep up and what it reports is a backlog rather than a latency",
                    answered.rate()
                );
            }
        }
        // The table is what the store did. This is what the client waited, and
        // the difference between the two is the wait for a turn, most of which
        // on an idle machine is the operating system waking the reader late.
        // The quiet numbers are printed first for that reason: they are what
        // the waiting costs when nothing is competing for anything.
        println!(
            "counting the wait for its turn, at the median and p99: quiet {:.0} and {:.0} µs, writing {:.0} and {:.0}, folding {:.0} and {:.0}",
            quiet.waited_at(50),
            quiet.waited_at(99),
            writing.waited_at(50),
            writing.waited_at(99),
            folding.waited_at(50),
            folding.waited_at(99),
        );
    }
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
    /// How long each query took, in microseconds.
    times: Vec<f64>,
    /// How long each query took counting the wait for its turn, in
    /// microseconds, which is empty unless a rate was offered.
    waited: Vec<f64>,
    /// How many segments the store held when it was over.
    segments: usize,
    /// How long the writing took in each round, on the two conditions that
    /// wrote.
    ingest: Vec<Duration>,
    /// How long the reader was asking, added up over the rounds.
    span: Duration,
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

    /// The middle of what the query took, in microseconds.
    fn median(&self) -> f64 {
        self.at(50)
    }

    /// The ninety ninth percentile of what the query took, in microseconds.
    fn p99(&self) -> f64 {
        self.at(99)
    }

    /// What the query took at a percentile, in microseconds.
    fn at(&self, percentile: usize) -> f64 {
        percentile_of(&self.times, percentile)
    }

    /// What the client waited at a percentile, in microseconds.
    ///
    /// The same thing as [`Self::at`] when no rate was offered, since a reader
    /// asking as fast as it can never waits for a turn.
    fn waited_at(&self, percentile: usize) -> f64 {
        if self.waited.is_empty() {
            return self.at(percentile);
        }
        percentile_of(&self.waited, percentile)
    }

    /// How many queries a second the reader got through.
    fn rate(&self) -> f64 {
        if self.span.is_zero() {
            return 0.0;
        }
        self.times.len() as f64 / self.span.as_secs_f64()
    }

    /// Prints it.
    fn tell(&self, name: &str) {
        println!(
            "{:<22} {:>9} {:>6} {:>9.0} {:>8.1} µs {:>8.1} µs {:>8.1} µs",
            name,
            self.times.len(),
            self.segments,
            self.rate(),
            self.median(),
            self.at(95),
            self.p99(),
        );
    }
}

/// A percentile of a set of times.
fn percentile_of(times: &[f64], percentile: usize) -> f64 {
    if times.is_empty() {
        return 0.0;
    }
    let mut sorted = times.to_vec();
    sorted.sort_by(f64::total_cmp);
    sorted[(sorted.len() - 1) * percentile / 100]
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
    rate: Option<f64>,
    threads: usize,
) -> Result<Answered, String> {
    let mut pooled = Answered::default();
    for _ in 0..ROUNDS {
        let round = measure(base, directory, name, queries, writing, rate, threads)?;
        pooled.times.extend(round.times);
        pooled.waited.extend(round.waited);
        pooled.segments = round.segments;
        pooled.ingest.extend(round.ingest);
        pooled.span += round.span;
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
    rate: Option<f64>,
    threads: usize,
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
            scope.spawn(move || ask(writer, queries, stop, rate))
        };
        let started = Instant::now();
        if let Some((files, folding)) = writing {
            let keeper = &keeper;
            let keeping = folding.then(|| scope.spawn(move || keeper.run(|| 1_700_000_003)));
            let running: Vec<_> = (0..threads)
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
        let (times, waited) = asking
            .join()
            .unwrap_or_else(|_| Err("a reader stopped".into()))?;
        Ok::<_, String>((times, waited, took))
    });

    let (times, waited, took) = outcome?;
    let segments = writer.view().len();
    drop(writer);
    std::fs::remove_file(&path).ok();

    Ok(Answered {
        times,
        waited,
        segments,
        ingest: writing.map(|_| took).into_iter().collect(),
        span: took,
    })
}

/// The reader, asking the same questions over and over until it is told to
/// stop, and timing each of them.
///
/// With a rate, the turn a query takes is fixed when the reader starts rather
/// than when the query before it finished, so a slow query pushes the one after
/// it into a queue instead of pushing the whole schedule back. What comes back
/// is both clocks: what the query took, and what it took counting the wait from
/// when it was due, which are the same thing whenever the reader is keeping up
/// and come apart exactly when it stops.
fn ask(
    writer: &Writer,
    queries: &[String],
    stop: &AtomicBool,
    rate: Option<f64>,
) -> Result<(Vec<f64>, Vec<f64>), String> {
    let mut times = Vec::new();
    let mut waited = Vec::new();
    let opened = Instant::now();
    let mut turn = 0u64;
    while !stop.load(Ordering::Acquire) {
        for query in queries {
            let due = rate.map(|rate| {
                let due = opened + Duration::from_secs_f64(turn as f64 / rate);
                turn += 1;
                if let Some(left) = due.checked_duration_since(Instant::now()) {
                    std::thread::sleep(left);
                }
                due
            });
            let started = Instant::now();
            // The whole of what a reader pays to see the newest commit, which
            // is where the segment count shows up.
            let view = writer.view();
            let readers = view.readers().map_err(|problem| problem.to_string())?;
            let searcher = Searcher::over(&readers).map_err(|problem| problem.to_string())?;
            let _ = searcher
                .search(query, 10)
                .map_err(|problem| problem.to_string())?;
            let done = Instant::now();
            times.push(done.duration_since(started).as_secs_f64() * 1_000_000.0);
            if let Some(due) = due {
                waited.push(done.duration_since(due).as_secs_f64() * 1_000_000.0);
            }
        }
    }
    Ok((times, waited))
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
