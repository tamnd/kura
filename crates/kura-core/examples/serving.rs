//! What a query costs while the store it is reading is being written to.
//!
//! Run it with `cargo run --release --example serving -- <corpus> [<directory>]
//! [threads=<n>] [readers=<n>] [<queries per second> ...]`, where the corpus is
//! a directory of text files and the directory is where the store goes,
//! defaulting to the temporary directory. Every argument that is a number is an
//! offered rate and the whole measurement is run again at each of them.
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
//! A run at a rate prints the wait as well, and a run of any kind prints what
//! the writing itself took, since a condition that costs a reader nothing by
//! taking twice as long over the ingest has not saved anybody anything.
//!
//! Four more rows follow them and they are there to take the first three apart.
//! A writing row differs from the quiet row in four things at once: it holds
//! more documents, it holds them across several segments rather than one, there
//! are writer threads on the cores while the query runs, and those threads are
//! committing. So the run builds the store the middle query of the writing
//! condition was looking at, by adding batches to the base with one thread and
//! nothing reading until the segment count that query walked is reached, and
//! folds a copy of that to one segment.
//!
//! Three of the four ask their questions of those two stores. The first two run
//! with nothing else happening, which says what the documents cost and then what
//! the segment count costs on top of them. The third puts the writer threads
//! back beside the spread out store, reading the same files and building the
//! same segments and committing none of them, which says what the cores cost.
//!
//! What is left between the third and the writing row is not the commits, and
//! the run says so rather than printing it as though it were. Every row but the
//! writing rows holds one segment count for a whole round, while the writing row
//! climbs from one to a dozen, so half of its queries walked fewer segments than
//! any query in a row it is being read against and the subtraction picks up the
//! ramp. Two things follow from that. The rows are read against each other at a
//! segment count both of them walked, which every query records for itself after
//! its clock has stopped. And the commits are read off the fourth row instead,
//! which writes exactly as the writing row does with the store asking the drive
//! to order the writes rather than to finish them, so it ramps the same way and
//! differs in the syncs and nothing else.
//!
//! What the syncs leave over is the rest of a commit, and there are two kinds
//! of thing it could be. One kind is paid once per commit however large it was:
//! extending the mapping, taking the lock, handing the view over. The other is
//! paid by the byte: a segment written a moment ago is being read for the first
//! time, where a store sitting still has been read a quarter of a million times
//! by the time its median query runs. So one more row commits the same way with
//! a fraction of the documents in each batch, over the same fraction of the
//! files so that the store still ends the round with the same segments in it,
//! and it is read against a control built the way it was rather than the way
//! the writing row was. Two subtractions, each of a row against its own
//! control, and what is read is the two of them against each other.
//!
//! The count all this is held at is what the middle query of the writing
//! condition walked rather than what the store held when the round was over, and
//! the two are not close. A writing condition that finishes at thirteen segments
//! started at one, so its middle query walked five, and a quiet row held at
//! thirteen for a whole round is being compared against something no query in
//! the writing row ever saw. Held at thirteen it came out slower than the
//! writing row it was there to explain, which is how that got noticed. The
//! documents go with the count for the same reason: the store the middle query
//! saw held what had been added by then, and folding the end of the round down
//! to the middle's segment count would have handed it documents that were not
//! there yet.
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
//! One thread offering a rate runs out of room well below where the store does,
//! since going to sleep and being woken costs more than a query, so `readers=<n>`
//! puts several on it. They share one schedule rather than each holding their
//! own, so the offered rate stays the rate the store is asked at rather than the
//! rate times the reader count, and a query goes to whichever thread is free.
//! The count is printed, because a rate held by two threads and a rate held by
//! twenty are different loads on the same machine, and because every reader is
//! one more thing wanting a core.
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
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use kura_core::analysis::Analyzer;
use kura_core::durability::Reach;
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

/// How many threads ask questions, unless the command line says otherwise.
///
/// One, because one is enough to say what a query costs and every reader beyond
/// the first is a thread competing for the cores the writing and the folding
/// want. It is not enough to say what a query costs at a load worth quoting,
/// since one thread that sleeps between queries runs out of room at about fifty
/// thousand a second on this machine while the store answers several times
/// that, which is what `readers=<n>` is for.
///
/// They share one schedule rather than each holding their own, so the offered
/// rate is the rate the store is asked at rather than the rate times the reader
/// count, and a query goes to whichever thread is free.
const READERS: usize = 1;

/// How much text goes into a batch before it is handed over.
///
/// Small, so that commits happen often enough over the run for a reader to be
/// asking questions between them rather than during one long one.
const BUDGET: u64 = 4 * 1024 * 1024;

/// What the small batch condition divides both the documents a commit holds and
/// the file list by.
///
/// Dividing both is what keeps the row readable against the writing row. Fewer
/// documents a commit alone would leave the same corpus in eight times as many
/// segments, and a reader walking eight times as many of them is a heavier
/// reader, which is a second difference between the two rows and the whole
/// point of the condition is that there is only one.
const FRACTION: usize = 8;

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

/// How many queries have to have walked a given number of segments before a
/// percentile taken over them is worth printing.
///
/// A p99 over a hundred queries is the single slowest one of them, which is a
/// sample of one and says more about what else the machine was doing.
const ENOUGH: usize = 1_000;

/// The four things a query does, in the order it does them.
///
/// Only timed when the run is asked for them, because timing them means four
/// more clock reads inside something that takes a few microseconds, and the
/// tables everywhere else in this repository are of a query with nothing in it
/// but the query.
const PARTS: [&str; 4] = [
    "taking the view",
    "opening the readers",
    "building the searcher",
    "running the query",
];

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(corpus) = args.next().map(PathBuf::from) else {
        eprintln!(
            "usage: serving <corpus> [<directory>] [threads=<n>] [readers=<n>] [parts] [<queries per second> ...]"
        );
        eprintln!("the corpus is a directory of text files, and the store goes in the directory");
        eprintln!("a rate is how many queries a second the reader offers, and without one it asks");
        eprintln!("as fast as it can");
        eprintln!("parts times the four things a query does rather than the query, and it costs");
        eprintln!("four clock reads a query to do it, so the two are not run together");
        std::process::exit(2);
    };

    // Anything that reads as a number is a rate, anything of the shape
    // threads=<n> or readers=<n> is a thread count, and anything else is where
    // the store goes, so the optional arguments can be given in any order.
    let mut directory = std::env::temp_dir();
    let mut rates = Vec::new();
    let mut threads = THREADS;
    let mut readers = READERS;
    let mut parts = false;
    for arg in args {
        if arg == "parts" {
            parts = true;
        } else if let Some(count) = arg.strip_prefix("threads=") {
            match count.parse::<usize>() {
                Ok(count) if count > 0 => threads = count,
                _ => {
                    eprintln!("serving: {count} is not a number of threads");
                    std::process::exit(2);
                }
            }
        } else if let Some(count) = arg.strip_prefix("readers=") {
            match count.parse::<usize>() {
                Ok(count) if count > 0 => readers = count,
                _ => {
                    eprintln!("serving: {count} is not a number of readers");
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

    match run(&corpus, &directory, &rates, threads, readers, parts) {
        Ok(()) => (),
        Err(problem) => {
            eprintln!("serving: {problem}");
            std::process::exit(1);
        }
    }
}

/// The whole measurement.
fn run(
    corpus: &Path,
    directory: &Path,
    rates: &[f64],
    threads: usize,
    readers: usize,
    parts: bool,
) -> Result<(), String> {
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
    println!("readers       {readers} sharing one schedule");
    if parts {
        println!("parts         timed, so the query times below carry four more clock reads");
    }
    if rates.is_empty() {
        let load = Load {
            rate: None,
            threads,
            readers,
            parts,
        };
        conditions(&base, directory, &queries, rest, load)?;
    } else {
        for rate in rates {
            let load = Load {
                rate: Some(*rate),
                threads,
                readers,
                parts,
            };
            conditions(&base, directory, &queries, rest, load)?;
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
    load: Load,
) -> Result<(), String> {
    println!();
    match load.rate {
        Some(rate) => println!("offered       {rate:.0} queries a second"),
        None => println!("offered       as fast as the reader can ask"),
    }
    println!(
        "{:<22} {:>9} {:>6} {:>9} {:>11} {:>11} {:>11}",
        "condition", "queries", "segs", "q/s", "median", "p95", "p99"
    );

    let quiet = rounds(base, directory, "quiet", queries, Doing::Nothing, load)?;
    quiet.tell("quiet");
    let writing = rounds(
        base,
        directory,
        "writing",
        queries,
        Doing::Writing(rest),
        load,
    )?;
    writing.tell("writing");
    let folding = rounds(
        base,
        directory,
        "folding",
        queries,
        Doing::Folding(rest),
        load,
    )?;
    folding.tell("writing and folding");
    let loosely = rounds(
        base,
        directory,
        "loosely",
        queries,
        Doing::Loosely(rest),
        load,
    )?;
    loosely.tell("writing, ordered only");
    // An eighth of the files, taken every eighth rather than the first eighth
    // of them, so that the batches hold the same spread of the corpus as the
    // writing row and are smaller rather than different.
    let thinned: Vec<PathBuf> = rest.iter().step_by(FRACTION).cloned().collect();
    let most = (rest.len() / writing.segments.max(1) / FRACTION).max(1);
    let smaller = rounds(
        base,
        directory,
        "smaller",
        queries,
        Doing::Smaller {
            files: &thinned,
            most,
        },
        load,
    )?;
    smaller.tell("writing, smaller batches");

    if load.parts {
        // The rows below this one are the decomposition of the writing row and
        // they are a different question, asked of a query that is not carrying
        // four extra clock reads. A run that printed both would be putting two
        // measurements of different things in one table.
        inside(&[
            ("quiet", &quiet),
            ("writing", &writing),
            ("writing and folding", &folding),
            ("writing, ordered only", &loosely),
            ("writing, smaller batches", &smaller),
        ]);
        return Ok(());
    }

    // The count the second quiet row is held at is what the middle query of the
    // writing condition walked, not what the store held when the round was
    // over. The writing condition starts at one segment and climbs, so the
    // count at the end is what its last query paid and holding a quiet row
    // there for a whole round would be comparing against something no query in
    // the writing row ever saw.
    let apart = taken_apart(
        base,
        directory,
        queries,
        rest,
        load,
        writing.walked(),
        writing.wrote_in().unwrap_or_default(),
    )?;

    let tight = without_commits(
        base,
        directory,
        queries,
        &thinned,
        load,
        Matched {
            each: most,
            segments: smaller.walked(),
            pace: smaller.wrote_in().unwrap_or_default(),
        },
    )?;
    report(
        &quiet,
        &writing,
        &folding,
        &loosely,
        &Small {
            row: &smaller,
            control: tight.as_ref(),
            each: most,
        },
        &apart,
        load,
    );
    Ok(())
}

/// What the four things a query does cost in each condition.
///
/// The interesting column is not the largest one, it is the one that grows
/// between the quiet row and the writing rows, because that is the part of a
/// query the writing reaches.
fn inside(conditions: &[(&str, &Answered)]) {
    println!();
    println!(
        "{:<22} {:>13} {:>13} {:>13} {:>13}",
        "the median of", PARTS[0], PARTS[1], PARTS[2], PARTS[3]
    );
    for (name, answered) in conditions {
        answered.parted(name, 50);
    }
    println!();
    println!(
        "{:<22} {:>13} {:>13} {:>13} {:>13}",
        "and of p99, ", PARTS[0], PARTS[1], PARTS[2], PARTS[3]
    );
    for (name, answered) in conditions {
        answered.parted(name, 99);
    }
    let Some((_, quiet)) = conditions.first() else {
        return;
    };
    println!();
    for (name, answered) in conditions.iter().skip(1) {
        let said = (0..PARTS.len())
            .map(|part| {
                format!(
                    "{:.0} percent on {}",
                    share(answered.part(part, 50), quiet.part(part, 50)),
                    PARTS[part]
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        println!("against quiet, {name} costs {said}");
    }
}

/// Everything a run says under its table.
fn report(
    quiet: &Answered,
    writing: &Answered,
    folding: &Answered,
    loosely: &Answered,
    smaller: &Small<'_>,
    apart: &Apart,
    load: Load,
) {
    let (settled, spread, busy) = (&apart.settled, &apart.spread, &apart.busy);

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
    // The same cost taken apart. Each step is against the row above it rather
    // than against quiet, because the four do not add up: a percentage of a
    // percentage is not a percentage of the floor, and printing them as though
    // they were would be the sort of arithmetic that makes a table wrong.
    if let (Some(spread), Some(busy)) = (spread, busy) {
        println!(
            "the middle query of the writing condition walked {} segments and the one at the end of it {}, and folding held the middle at {}",
            writing.walked(),
            writing.segments,
            folding.walked(),
        );
        println!(
            "of that, the documents the writing adds cost {:.0} percent at the median and {:.0} at p99, holding them across {} segments rather than one costs {:.0} and {:.0} more, and the cores the threads take cost {:.0} and {:.0} on top of that",
            share(settled.median(), quiet.median()),
            share(settled.p99(), quiet.p99()),
            spread.segments,
            share(spread.median(), settled.median()),
            share(spread.p99(), settled.p99()),
            share(busy.median(), spread.median()),
            share(busy.p99(), spread.p99()),
        );
        if let (Some(all), Some(most)) = (writing.wrote_in(), busy.wrote_in()) {
            println!(
                "the threads with the commits taken out were held to {most:.2?} over the same files against the {all:.2?} the writing took, so the cores they wanted are the cores the writing wanted"
            );
        }
        commits("the commits", writing, busy, spread.segments);
        println!(
            "taken whole rather than at a matched count that comes to {:.0} percent, which is the ramp from one segment to {} and not the commits, since a row that holds one count all round has no cheap queries in it",
            share(writing.median(), busy.median()),
            writing.segments,
        );
    }
    commits("the syncs", writing, loosely, writing.walked());
    // The one that tells a cost per byte from a cost per commit. It is the same
    // subtraction as the line above the syncs, done a second time with a
    // fraction of the documents in each commit, so what is read is not the two
    // rows against each other but the two subtractions against each other.
    if let Some(control) = smaller.control {
        commits(
            &format!("the commits, at {} documents each", smaller.each),
            smaller.row,
            control,
            control.segments,
        );
    }
    if let (Some(all), Some(small)) = (writing.wrote_in(), smaller.row.wrote_in()) {
        println!(
            "the smaller batches went over an eighth of the files in {small:.2?} against the {all:.2?} the writing took over all of them, and left {} segments against {}",
            smaller.row.segments, writing.segments,
        );
    }
    if let (Some(all), Some(ordered)) = (writing.wrote_in(), loosely.wrote_in()) {
        println!(
            "the same writing with the syncs turned into barriers took {ordered:.2?} against {all:.2?}, and left {} segments against {}",
            loosely.segments, writing.segments,
        );
    }
    if let Some(rate) = load.rate {
        // A condition that did not hold the offered rate has a queue that grew
        // for the whole round, so its percentiles are how long the round was
        // rather than how long a query takes, and saying so is the difference
        // between a measurement and a misleading table.
        let rows = [
            ("quiet", Some(quiet)),
            ("writing", Some(writing)),
            ("writing and folding", Some(folding)),
            ("writing, ordered only", Some(loosely)),
            ("writing, smaller batches", Some(smaller.row)),
            ("quiet, all of it", Some(settled)),
            ("quiet, spread out", spread.as_ref()),
            ("threads, no commits", busy.as_ref()),
        ];
        for (name, answered) in rows
            .into_iter()
            .filter_map(|(name, row)| Some((name, row?)))
        {
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
}

/// What the writing row costs over a row with something taken out of it, read at
/// a segment count both of them held.
///
/// Taking the whole writing row against the whole of a row that holds one count
/// for the round gives a negative number, and it is not the thing being free.
/// The writing condition climbs from one segment to a dozen, so half of its
/// queries walked fewer segments than any query in the row it is being read
/// against, and the subtraction picks up the ramp rather than the difference it
/// was asked about. The queries that walked the same count in both rows are the
/// ones that can be compared, and this is them.
fn commits(what: &str, writing: &Answered, without: &Answered, segments: usize) {
    let taken = [50, 95, 99].map(|percentile| {
        (
            percentile,
            writing.at_walk(segments, percentile),
            without.at_walk(segments, percentile),
        )
    });
    let Some(counted) = taken
        .iter()
        .find_map(|(_, wrote, was)| Some((wrote.as_ref()?.1, was.as_ref()?.1)))
    else {
        println!("too few queries walked {segments} segments in both rows to say what {what} cost");
        return;
    };
    let said = taken
        .iter()
        .filter_map(|(percentile, wrote, was)| {
            let (Some((wrote, _)), Some((was, _))) = (wrote, was) else {
                return None;
            };
            Some(format!("{:.0} at p{percentile}", share(*wrote, *was)))
        })
        .collect::<Vec<_>>()
        .join(", ");
    println!(
        "read at the {segments} segments both of them walked, over the {} queries of the row that has them and the {} of the row without them, {what} cost {said} percent",
        counted.0, counted.1,
    );
}

/// The rows the writing row is taken apart against.
struct Apart {
    /// Every document the middle query of the writing condition was looking at,
    /// in one segment, with nothing else running.
    settled: Answered,
    /// The same documents across the segments that middle query walked, still
    /// with nothing else running.
    ///
    /// Nothing when the writing condition walked one segment, since it would be
    /// [`Self::settled`] again.
    spread: Option<Answered>,
    /// The same store as [`Self::spread`], with the writer threads reading the
    /// same files and building the same segments and committing none of them.
    busy: Option<Answered>,
}

/// The small batch row and the row it is read against.
struct Small<'a> {
    /// The writing condition with a fraction of the documents in each commit.
    row: &'a Answered,
    /// The same batches over the same files with the commits taken out.
    ///
    /// Nothing when the small batch row came to one segment, since there is
    /// then no count for the two of them to be read at.
    control: Option<&'a Answered>,
    /// How many documents a batch of that row took.
    each: usize,
}

/// What a control row has to match for the row it stands in for.
#[derive(Clone, Copy)]
struct Matched {
    /// How many documents a batch takes before it is finished.
    each: usize,
    /// The segment count the store is brought to before the queries start.
    segments: usize,
    /// How long the row it stands in for took over the same files.
    pace: Duration,
}

/// The rows the writing row is taken apart against.
///
/// The writing row differs from the quiet row in four things at once. It holds
/// more documents, it holds them across several segments rather than one, there
/// are writer threads on the cores while the query runs, and those threads are
/// committing. So the store its middle query was looking at is built here, once,
/// by adding batches to the base until the segment count that query walked is
/// reached. That store is asked the questions with nothing else running, and so
/// is a copy of it folded to one segment, and then it is asked them again with
/// the writer threads beside it doing everything a writer does except the
/// commit, against a store that therefore does not change under it.
///
/// Read one against the next, that is the documents, then the segments they
/// landed in, then the cores the threads take. What is left between the last of
/// them and the writing row is not the commits, whatever it looks like, because
/// the writing row is the only one of them whose segment count moves while it is
/// being asked. The commits are read off a row that ramps the same way and syncs
/// differently, which is [`Doing::Loosely`].
fn taken_apart(
    base: &Path,
    directory: &Path,
    queries: &[String],
    rest: &[PathBuf],
    load: Load,
    segments: usize,
    pace: Duration,
) -> Result<Apart, String> {
    let many = directory.join("kura-serving-mid.kura");
    let (added, count) = midway(base, &many, rest, usize::MAX, segments)?;
    let one = directory.join("kura-serving-mid-one.kura");
    folded(&many, &one, 1)?;
    let settled = rounds(&one, directory, "settled", queries, Doing::Nothing, load)?;
    settled.tell(&format!("quiet, {added} more, 1 seg"));
    std::fs::remove_file(&one).ok();

    let (mut spread, mut busy) = (None, None);
    if count > 1 {
        let measured = rounds(&many, directory, "spread", queries, Doing::Nothing, load)?;
        measured.tell(&format!("quiet, {added} more, {count}"));
        let churning = rounds(
            &many,
            directory,
            "busy",
            queries,
            Doing::Busy {
                files: rest,
                pace,
                most: usize::MAX,
            },
            load,
        )?;
        churning.tell("threads, no commits");
        if count != segments {
            // The writing condition's segments are full batches and so are
            // these, so the two land on the same count unless a thread there
            // committed a short batch. If this line prints, the rows below it
            // are being read against a store a query never saw.
            println!(
                "that row was asked for {segments} segments and the store came to {count}, so the counts below are not the counts above"
            );
        }
        spread = Some(measured);
        busy = Some(churning);
    }
    std::fs::remove_file(&many).ok();
    Ok(Apart {
        settled,
        spread,
        busy,
    })
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
    index_into(&mut store, files, |text| {
        bytes += text.len() as u64;
        analyzer.analyze(text, |term, _| {
            *counts.entry(term.to_vec()).or_default() += 1;
        });
    })?;

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

/// Adds files to a store, one batch of [`BUDGET`] at a time, on this thread.
///
/// `saw` is handed the text of every file that went in, which is how the first
/// pass counts the vocabulary the queries come out of. The conditions do their
/// writing through [`Writer`] rather than this, because they are measuring what
/// several threads handing batches over costs a reader. This is for building
/// the stores they are measured against, where there is nothing to race with
/// and one thread is the simplest thing that works.
fn index_into(store: &mut Store, files: &[PathBuf], saw: impl FnMut(&str)) -> Result<(), String> {
    index_into_with(store, files, BUDGET, usize::MAX, usize::MAX, saw).map(|_| ())
}

/// The same thing with the batch size given rather than taken from [`BUDGET`],
/// a cap on how many documents a batch takes, and a cap on how many batches it
/// commits.
///
/// Returns how many files it got through, which is the whole of `files` unless
/// the cap stopped it.
fn index_into_with(
    store: &mut Store,
    files: &[PathBuf],
    budget: u64,
    each: usize,
    most: usize,
    mut saw: impl FnMut(&str),
) -> Result<usize, String> {
    let mut at = 0;
    let mut committed = 0;
    while at < files.len() && committed < most {
        let view = store.view().map_err(|problem| problem.to_string())?;
        let mut batch = Batch::with_budget(&view, budget).map_err(|problem| problem.to_string())?;
        let mut held = 0;
        while at < files.len() {
            let path = &files[at];
            at += 1;
            let Some(text) = text_of(path) else { continue };
            saw(&text);
            let key = path.to_string_lossy().into_owned();
            batch
                .add_keyed(key.as_bytes(), &text)
                .map_err(|problem| problem.to_string())?;
            held += 1;
            if batch.is_full() || held >= each {
                break;
            }
        }
        if batch.is_empty() {
            break;
        }
        let prepared = batch.finish().map_err(|problem| problem.to_string())?;
        drop(view);
        prepared
            .commit(&mut *store, 1_700_000_001, 1)
            .map_err(|problem| problem.to_string())?;
        committed += 1;
    }
    Ok(at)
}

/// Builds the store the middle query of the writing condition was looking at.
///
/// Not the store the writing condition ends up with. The two are different in
/// the documents as well as in the segment count, because a query in the middle
/// of the round is asking a store that holds what had been added by then and not
/// what would be added later. Folding the end of the round down to the middle's
/// segment count leaves the middle's count and the end's documents, which is a
/// store that never existed, and reading the writing row against it hands the
/// documents that were not there yet to whichever step of the decomposition
/// comes next.
///
/// So the files go in at the batch size the conditions use, and only as many
/// batches as it takes to reach the count. That leaves the same segment count
/// and, because every one of those segments is a full batch in both, close to
/// the same documents. Returns how many files went in and what the count came
/// to.
fn midway(
    base: &Path,
    path: &Path,
    files: &[PathBuf],
    each: usize,
    segments: usize,
) -> Result<(usize, usize), String> {
    std::fs::remove_file(path).ok();
    std::fs::copy(base, path).map_err(|problem| format!("{}: {problem}", path.display()))?;
    let mut store = Store::open(path).map_err(|problem| problem.to_string())?;
    let held = store.manifest().segments.len();
    let added = index_into_with(
        &mut store,
        files,
        BUDGET,
        each,
        segments.saturating_sub(held),
        |_| (),
    )?;
    Ok((added, store.manifest().segments.len()))
}

/// The same store the small batch condition queries, with the writer threads
/// beside it building the same batches and committing none of them.
///
/// This is what the small batch row is read against, and it is built the way
/// the row it stands in for was built rather than the way the writing row was,
/// because a control that matched the writing row would differ from the row it
/// is subtracted from in the batch size as well as in the commits.
fn without_commits(
    base: &Path,
    directory: &Path,
    queries: &[String],
    files: &[PathBuf],
    load: Load,
    matched: Matched,
) -> Result<Option<Answered>, String> {
    let many = directory.join("kura-serving-tight-mid.kura");
    let (added, count) = midway(base, &many, files, matched.each, matched.segments)?;
    if count < 2 {
        std::fs::remove_file(&many).ok();
        return Ok(None);
    }
    let churning = rounds(
        &many,
        directory,
        "tight",
        queries,
        Doing::Busy {
            files,
            pace: matched.pace,
            most: matched.each,
        },
        load,
    )?;
    churning.tell(&format!("small threads, no commits, {added} more"));
    std::fs::remove_file(&many).ok();
    Ok(Some(churning))
}

/// Copies a store and folds it down to a segment count.
///
/// The newest segments are left where they are and everything older is folded
/// into one, so two stores made this way from the same source hold the same
/// documents and differ in the segment count and in nothing else. Returns what
/// it reached, which is what was asked for unless the store did not hold enough
/// segments to fold that far.
fn folded(from: &Path, path: &Path, want: usize) -> Result<usize, String> {
    std::fs::remove_file(path).ok();
    std::fs::copy(from, path).map_err(|problem| format!("{}: {problem}", path.display()))?;
    let mut store = Store::open(path).map_err(|problem| problem.to_string())?;
    let held = store.manifest().segments.len();
    if held > want {
        store
            .compact(0..held - want + 1, 1_700_000_002, 2)
            .map_err(|problem| problem.to_string())?;
    }
    Ok(store.manifest().segments.len())
}

/// What one reader thread came back with.
struct Asked {
    /// How long each of its queries took, in microseconds.
    times: Vec<f64>,
    /// How long each took counting the wait for its turn, in microseconds,
    /// which is empty unless a rate was offered.
    waited: Vec<f64>,
    /// How many segments each of them walked.
    walked: Vec<usize>,
    /// How long each of the four things a query does took, in microseconds,
    /// which is empty unless the run was asked for them.
    parts: [Vec<f64>; PARTS.len()],
}

/// What one condition came to.
#[derive(Default)]
struct Answered {
    /// How long each query took, in microseconds.
    times: Vec<f64>,
    /// How long each query took counting the wait for its turn, in
    /// microseconds, which is empty unless a rate was offered.
    waited: Vec<f64>,
    /// How many segments each query walked.
    walked: Vec<usize>,
    /// How long each of the four things a query does took, in microseconds.
    parts: [Vec<f64>; PARTS.len()],
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

    /// How many segments the middle query of the condition walked.
    ///
    /// Not the same thing as what the store held when it was over, on a
    /// condition that was writing. That store started at one segment and
    /// finished at a dozen, so the count at the end is what the last query paid
    /// and this is what a query paid.
    fn walked(&self) -> usize {
        if self.walked.is_empty() {
            return self.segments;
        }
        let mut sorted = self.walked.clone();
        sorted.sort_unstable();
        sorted[sorted.len() / 2]
    }

    /// What the queries that walked exactly this many segments took, at a
    /// percentile, and how many of them there were.
    ///
    /// The writing condition starts at one segment and climbs, so its queries
    /// are not all paying the same walk and its median is a median over a mixed
    /// population. Every other condition holds one count for the whole round.
    /// Reading two conditions against each other at the same count is the only
    /// way to compare them without the ramp doing the arithmetic.
    fn at_walk(&self, segments: usize, percentile: usize) -> Option<(f64, usize)> {
        if self.walked.len() != self.times.len() {
            return None;
        }
        let held: Vec<f64> = self
            .times
            .iter()
            .zip(&self.walked)
            .filter(|(_, walked)| **walked == segments)
            .map(|(time, _)| *time)
            .collect();
        if held.len() < ENOUGH {
            return None;
        }
        Some((percentile_of(&held, percentile), held.len()))
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

    /// What one of the four things a query does took at a percentile, in
    /// microseconds.
    fn part(&self, part: usize, percentile: usize) -> f64 {
        percentile_of(&self.parts[part], percentile)
    }

    /// Prints the four of them at a percentile.
    fn parted(&self, name: &str, percentile: usize) {
        println!(
            "{:<22} {:>10.2} µs {:>10.2} µs {:>10.2} µs {:>10.2} µs",
            name,
            self.part(0, percentile),
            self.part(1, percentile),
            self.part(2, percentile),
            self.part(3, percentile),
        );
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

/// What the machine is being asked to do while the queries run.
///
/// One thing rather than three arguments threaded through four functions, since
/// every one of them wants all three and none of them wants to change any.
#[derive(Clone, Copy)]
struct Load {
    /// How many queries a second the readers offer between them, or nothing at
    /// all if they ask as fast as they can.
    rate: Option<f64>,
    /// How many threads fill batches.
    threads: usize,
    /// How many threads ask questions.
    readers: usize,
    /// Whether to time the four things a query does rather than the query.
    parts: bool,
}

/// What is going on beside the queries.
#[derive(Clone, Copy)]
enum Doing<'a> {
    /// Nothing at all, which is the floor every other condition is read
    /// against.
    Nothing,
    /// The writer threads reading the files and building the segments and
    /// committing none of them, so the store does not change under the reader
    /// and the cores are as busy as the writing condition makes them.
    ///
    /// Paced to the wall clock the writing condition took, because the commits
    /// are most of that clock and threads that skip them get through the corpus
    /// in a fifth of the time. Left to run flat out they are a heavier load than
    /// the writers they stand in for, not a lighter one, and the row would
    /// overstate what the cores cost by however much it understated the wall
    /// clock.
    Busy {
        /// The files the threads work through.
        files: &'a [PathBuf],
        /// How long the writing condition took over the same files.
        pace: Duration,
        /// How many documents a batch takes before it is thrown away, so that
        /// the threads finish a batch as often as the row they stand in for.
        most: usize,
    },
    /// The writer threads adding the files as fast as they can fill batches,
    /// with nothing folding, so the segment count climbs for the whole round.
    Writing(&'a [PathBuf]),
    /// The same, with a keeper beside them.
    Folding(&'a [PathBuf]),
    /// The same as [`Doing::Writing`] with the batches an eighth of the
    /// documents over an eighth of the files, so that a commit writes a
    /// fraction of the bytes and the store still ends the round with the same
    /// segments in it.
    ///
    /// Read against the writing condition at a segment count both of them
    /// walked, which is the only way to read any two of these against each
    /// other, this holds the number of commits a query has lived through the
    /// same and changes how much each of them wrote. If what a commit costs a
    /// reader follows the bytes it added, this is cheaper by about that
    /// fraction. If it follows the commit, the two are the same.
    Smaller {
        /// The files the threads work through, an eighth of the writing row's.
        files: &'a [PathBuf],
        /// How many documents a batch takes before it commits, an eighth of
        /// what a batch of the writing row held.
        most: usize,
    },
    /// The same as [`Doing::Writing`], with the store asking the drive to order
    /// the writes rather than to finish them.
    ///
    /// Every other thing a commit does is still done: the same batch, the same
    /// lock, the same segment appended, the same manifest slot written, the same
    /// view handed to the readers, the same ramp from one segment to a dozen.
    /// The one difference is that the two syncs a commit costs are barriers
    /// rather than waits, so what this row is short of the writing row is what
    /// the waiting costs a reader and nothing else.
    Loosely(&'a [PathBuf]),
}

impl<'a> Doing<'a> {
    /// The files the threads work through, or nothing when there are no
    /// threads.
    fn files(self) -> Option<&'a [PathBuf]> {
        match self {
            Doing::Nothing => None,
            Doing::Busy { files, .. }
            | Doing::Writing(files)
            | Doing::Folding(files)
            | Doing::Loosely(files)
            | Doing::Smaller { files, .. } => Some(files),
        }
    }

    /// How far a commit is to push what it writes.
    fn reach(self) -> Reach {
        match self {
            Doing::Loosely(_) => Reach::Ordered,
            _ => Reach::Platter,
        }
    }

    /// How long the threads are to take over the files, when they are being
    /// held to a clock rather than let run.
    fn pace(self) -> Option<Duration> {
        match self {
            Doing::Busy { pace, .. } => Some(pace),
            _ => None,
        }
    }

    /// Whether the threads commit what they build.
    fn commits(self) -> bool {
        matches!(
            self,
            Doing::Writing(_) | Doing::Folding(_) | Doing::Loosely(_) | Doing::Smaller { .. }
        )
    }

    /// How much text goes into a batch before it is handed over.
    /// How many documents a batch takes before it commits.
    fn most(self) -> usize {
        match self {
            Doing::Smaller { most, .. } | Doing::Busy { most, .. } => most,
            _ => usize::MAX,
        }
    }

    /// Whether a keeper runs beside them.
    fn folds(self) -> bool {
        matches!(self, Doing::Folding(_))
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
    doing: Doing<'_>,
    load: Load,
) -> Result<Answered, String> {
    let mut pooled = Answered::default();
    for _ in 0..ROUNDS {
        let round = measure(base, directory, name, queries, doing, load)?;
        pooled.times.extend(round.times);
        pooled.waited.extend(round.waited);
        pooled.walked.extend(round.walked);
        for (held, theirs) in pooled.parts.iter_mut().zip(round.parts) {
            held.extend(theirs);
        }
        pooled.segments = round.segments;
        pooled.ingest.extend(round.ingest);
        pooled.span += round.span;
    }
    Ok(pooled)
}

/// Runs one condition against a copy of the base store.
fn measure(
    base: &Path,
    directory: &Path,
    name: &str,
    queries: &[String],
    doing: Doing<'_>,
    load: Load,
) -> Result<Answered, String> {
    let path = directory.join(format!("kura-serving-{name}.kura"));
    std::fs::remove_file(&path).ok();
    std::fs::copy(base, &path).map_err(|problem| format!("{}: {problem}", path.display()))?;
    let mut store = Store::open(&path).map_err(|problem| problem.to_string())?;
    store.set_durability(doing.reach());
    let writer = Writer::new(store).map_err(|problem| problem.to_string())?;

    let stop = AtomicBool::new(false);
    let next = AtomicUsize::new(0);
    let keeper = Keeper::new(&writer);
    let turn = AtomicU64::new(0);
    let outcome = std::thread::scope(|scope| {
        let opened = Instant::now();
        let asking: Vec<_> = (0..load.readers)
            .map(|_| {
                let (writer, stop, turn) = (&writer, &stop, &turn);
                scope.spawn(move || ask(writer, queries, stop, load, opened, turn))
            })
            .collect();
        let started = Instant::now();
        if let Some(files) = doing.files() {
            let keeper = &keeper;
            let keeping = doing
                .folds()
                .then(|| scope.spawn(move || keeper.run(|| 1_700_000_003)));
            let running: Vec<_> = (0..load.threads)
                .map(|_| {
                    let (writer, next) = (&writer, &next);
                    scope.spawn(move || add(writer, files, next, doing, started))
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
        let (mut times, mut waited, mut walked) = (Vec::new(), Vec::new(), Vec::new());
        let mut parts: [Vec<f64>; PARTS.len()] = Default::default();
        for handle in asking {
            let theirs = handle
                .join()
                .unwrap_or_else(|_| Err("a reader stopped".into()))?;
            times.extend(theirs.times);
            waited.extend(theirs.waited);
            walked.extend(theirs.walked);
            for (held, theirs) in parts.iter_mut().zip(theirs.parts) {
                held.extend(theirs);
            }
        }
        Ok::<_, String>((times, waited, walked, parts, took))
    });

    let (times, waited, walked, parts, took) = outcome?;
    let segments = writer.view().len();
    drop(writer);
    std::fs::remove_file(&path).ok();

    Ok(Answered {
        times,
        waited,
        walked,
        parts,
        segments,
        ingest: doing.files().map(|_| took).into_iter().collect(),
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
    load: Load,
    opened: Instant,
    turn: &AtomicU64,
) -> Result<Asked, String> {
    let mut times = Vec::new();
    let mut waited = Vec::new();
    let mut walked = Vec::new();
    let mut parts: [Vec<f64>; PARTS.len()] = Default::default();
    while !stop.load(Ordering::Acquire) {
        // The turn is taken from the counter every reader shares, so which
        // question gets asked and when it was due are both properties of the
        // schedule rather than of the thread that happened to be free. Two
        // readers asking the same term at the same moment would be measuring
        // one posting list in cache rather than the query set.
        let mine = turn.fetch_add(1, Ordering::Relaxed);
        let query = &queries[mine as usize % queries.len()];
        let due = load.rate.map(|rate| {
            let due = opened + Duration::from_secs_f64(mine as f64 / rate);
            if let Some(left) = due.checked_duration_since(Instant::now()) {
                std::thread::sleep(left);
            }
            due
        });
        let started = Instant::now();
        // The whole of what a reader pays to see the newest commit, which is
        // where the segment count shows up. The clock is read between the four
        // of them only when the run was asked for the parts, since four reads
        // of it are a measurable share of something this short.
        let view = writer.view();
        let took_view = load.parts.then(Instant::now);
        let readers = view.readers().map_err(|problem| problem.to_string())?;
        let opened_readers = load.parts.then(Instant::now);
        let searcher = Searcher::over(&readers).map_err(|problem| problem.to_string())?;
        let built = load.parts.then(Instant::now);
        let _ = searcher
            .search(query, 10)
            .map_err(|problem| problem.to_string())?;
        let done = Instant::now();
        if let (Some(took_view), Some(opened_readers), Some(built)) =
            (took_view, opened_readers, built)
        {
            let each = [
                took_view.duration_since(started),
                opened_readers.duration_since(took_view),
                built.duration_since(opened_readers),
                done.duration_since(built),
            ];
            for (held, took) in parts.iter_mut().zip(each) {
                held.push(took.as_secs_f64() * 1_000_000.0);
            }
        }
        times.push(done.duration_since(started).as_secs_f64() * 1_000_000.0);
        if let Some(due) = due {
            waited.push(done.duration_since(due).as_secs_f64() * 1_000_000.0);
        }
        // After the clock has stopped, since it is bookkeeping rather than part
        // of the query. The view is the one the query ran against, so this is
        // what that query walked and not what the store holds now.
        walked.push(view.len());
    }
    Ok(Asked {
        times,
        waited,
        walked,
        parts,
    })
}

/// One writer, taking files off the shared counter until they run out.
///
/// With `commit` false it does everything up to the commit and then throws the
/// segment away, which is the whole of what a writer costs a reader except the
/// commit itself: the same files read, the same analysis, the same segment
/// built, the same core taken. The store it is pointed at does not change under
/// the reader, so a condition run this way holds the segment count still.
///
/// `pace` is how long the threads between them are to take over the files,
/// measured from `began`, and a thread waits at the end of each batch for the
/// share of that time the files handed out so far have earned. Without it a
/// thread that skips the commits gets through the corpus several times faster
/// than one that does not, which is a heavier load on the cores rather than the
/// matching one it is there to be.
fn add(
    writer: &Writer,
    files: &[PathBuf],
    next: &AtomicUsize,
    doing: Doing<'_>,
    began: Instant,
) -> Result<(), String> {
    let (commit, most, pace) = (doing.commits(), doing.most(), doing.pace());
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
            if batch.is_full() || taken.len() >= most {
                break;
            }
        }
        if !batch.is_empty() {
            let prepared = batch.finish().map_err(|problem| problem.to_string())?;
            drop(view);
            if !commit {
                drop(prepared);
                hold(files.len(), next, began, pace);
                if drained {
                    return Ok(());
                }
                continue;
            }
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

/// Waits until the files handed out so far have earned their share of the pace.
///
/// Nothing at all when there is no pace, which is every condition that commits,
/// since those set the clock the others are held to rather than following one.
fn hold(files: usize, next: &AtomicUsize, began: Instant, pace: Option<Duration>) {
    let Some(pace) = pace else { return };
    if files == 0 {
        return;
    }
    let done = next.load(Ordering::Relaxed).min(files) as f64 / files as f64;
    let due = began + pace.mul_f64(done);
    if let Some(left) = due.checked_duration_since(Instant::now()) {
        std::thread::sleep(left);
    }
}
