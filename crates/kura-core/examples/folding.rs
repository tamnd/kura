//! Where the time in a fold goes.
//!
//! Run it with `cargo run --release --example folding -- <corpus> [<directory>]`,
//! where the corpus is a directory of text files.
//!
//! An index run ends by folding the level zero segments it wrote into one, and
//! on a four thread run over a hundred megabytes that fold is about an eighth of
//! the run. #162 settled that it should stay, because it pays for itself in tens
//! of thousands of queries against the store it leaves. #169 is about it costing
//! what it costs anyway, and the first thing that issue asks for is where the
//! time actually goes, before anything is rewritten around a guess.
//!
//! A fold is four things, and this times three of them separately and the fourth
//! by difference.
//!
//! Opening the sources, which is [`Source::new`] per segment. That checks the
//! digests, and unlike the read path it is right to: a merge is the last time
//! anything reads these bytes before the manifest stops pointing at them, so it
//! is the last chance to notice that they are damaged. The hashing column is
//! what that check costs, measured by opening the same segments through
//! [`Segment::open_without_checksum`] and subtracting.
//!
//! Merging, which is [`merge`]: the vocabularies walked together, the postings
//! renumbered and re-encoded, the lengths and the keys rebuilt. Nothing is
//! written to the file here. The result is a segment laid out in memory.
//!
//! The rest, which is what [`Store::compact`] costs beyond the two above:
//! appending the segment to the file, writing the manifest and waiting for the
//! drive twice.
//!
//! The rows are runs of the newest segments in the store, which are the ones a
//! run's own fold would be folding. They are all about the same size as each
//! other, so the row for eight sources is a fold of twice as many bytes as the
//! row for four, and the columns should be read that way: the interesting
//! number is not which row is largest but which column grows faster than the
//! bytes do.

// Every cast here feeds a printed number that is already approximate.
#![allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::Instant;

use kura_core::analysis::Analyzer;
use kura_core::compact::{Source, carries, merge};
use kura_core::file::{Store, View};
use kura_core::index::Reader;
use kura_core::ingest::Batch;
use kura_core::segment::Segment;

/// A store identifier, so a file written by this says what wrote it.
const STORE: u128 = 0x006b_7572_612d_666f_6c64_696e_6700_0001;

/// How much text goes into a segment before a new one is started.
///
/// The same budget the ladder in `layers` uses, so the two measurements are of
/// segments of the same size and can be read against each other.
const BUDGET: u64 = 4 * 1024 * 1024;

/// How many segments each row folds.
///
/// Anything the store did not reach is skipped rather than made up.
const RUNS: [usize; 4] = [2, 4, 8, 16];

/// How many times each phase is timed.
///
/// A fold is a tenth of a second, so a handful of rounds is a second of
/// measurement and the median of them is steady enough to compare rows with.
const ROUNDS: usize = 5;

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(corpus) = args.next().map(PathBuf::from) else {
        eprintln!("usage: folding <corpus> [<directory>]");
        eprintln!("the corpus is a directory of text files, and the stores go in the directory");
        std::process::exit(2);
    };
    let directory = args.next().map_or_else(std::env::temp_dir, PathBuf::from);

    if let Err(problem) = run(&corpus, &directory) {
        eprintln!("folding: {problem}");
        std::process::exit(1);
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

    let base = directory.join("kura-folding-base.kura");
    let (documents, most) = fill(&base, &files)?;
    println!("corpus        {}", corpus.display());
    println!("documents     {documents} in {most} segments");
    println!();
    println!(
        "{:>8} {:>10} {:>10} {:>11} {:>11} {:>11} {:>11} {:>11}",
        "sources", "in", "out", "opening", "hashing", "merging", "the rest", "whole"
    );

    let mut rows = Vec::new();
    for sources in RUNS {
        if sources > most {
            continue;
        }
        let measured = fold_of(&base, directory, sources)?;
        measured.tell();
        rows.push(measured);
    }
    std::fs::remove_file(&base).ok();

    if let (Some(first), Some(last)) = (rows.first(), rows.last()) {
        println!();
        println!(
            "{} sources is {:.1} times the bytes of {} and takes {:.1} times as long to merge",
            last.sources,
            last.bytes_in as f64 / first.bytes_in as f64,
            first.sources,
            last.merging / first.merging
        );
        println!(
            "merging is {:.0} percent of the fold at the widest row, and the file and the drive are {:.0}",
            100.0 * last.merging / last.whole,
            100.0 * last.rest() / last.whole
        );
        println!(
            "checking the digests of the sources is {:.0} percent of it, which is what a merge pays to be the last thing that reads them",
            100.0 * last.hashing() / last.whole
        );
    }
    Ok(())
}

/// Every file under a directory.
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

/// Indexes the corpus into a store of many segments.
///
/// Returns how many documents went in and how many segments they landed in.
fn fill(path: &Path, files: &[PathBuf]) -> Result<(usize, usize), String> {
    std::fs::remove_file(path).ok();
    let mut store =
        Store::create(path, STORE, 1_700_000_000).map_err(|problem| problem.to_string())?;
    let mut analyzer = Analyzer::new();
    let mut documents = 0;

    let mut at = 0;
    while at < files.len() {
        let view = store.view().map_err(|problem| problem.to_string())?;
        let mut batch = Batch::with_budget(&view, BUDGET).map_err(|problem| problem.to_string())?;
        while at < files.len() {
            let path = &files[at];
            at += 1;
            let Some(text) = text_of(path) else { continue };
            analyzer.analyze(&text, |_, _| {});
            let key = path.to_string_lossy().into_owned();
            batch
                .add_keyed(key.as_bytes(), &text)
                .map_err(|problem| problem.to_string())?;
            documents += 1;
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
    let most = store.manifest().segments.len();
    Ok((documents, most))
}

/// What one row came to. Times are in milliseconds.
struct Fold {
    /// How many segments were folded.
    sources: usize,
    /// How many bytes they held.
    bytes_in: u64,
    /// How many bytes the segment they became holds.
    bytes_out: u64,
    /// Opening them all, digests checked, which is what a fold does.
    opening: f64,
    /// Opening them all with the digests left unread.
    unchecked: f64,
    /// Merging them into one, in memory, nothing written.
    merging: f64,
    /// The whole compaction, opening and merging included.
    whole: f64,
}

impl Fold {
    /// What the digest check costs.
    fn hashing(&self) -> f64 {
        (self.opening - self.unchecked).max(0.0)
    }

    /// What is left once the opening and the merging are taken out, which is the
    /// append, the manifest and the two syncs.
    fn rest(&self) -> f64 {
        (self.whole - self.opening - self.merging).max(0.0)
    }

    /// Prints it.
    fn tell(&self) {
        println!(
            "{:>8} {:>7.1} MB {:>7.1} MB {:>8.1} ms {:>8.1} ms {:>8.1} ms {:>8.1} ms {:>8.1} ms",
            self.sources,
            self.bytes_in as f64 / 1_000_000.0,
            self.bytes_out as f64 / 1_000_000.0,
            self.opening,
            self.hashing(),
            self.merging,
            self.rest(),
            self.whole,
        );
    }
}

/// Folds the newest `sources` segments of a copy of the base store and measures
/// every phase of it.
fn fold_of(base: &Path, directory: &Path, sources: usize) -> Result<Fold, String> {
    let path = directory.join(format!("kura-folding-{sources}.kura"));
    std::fs::remove_file(&path).ok();
    std::fs::copy(base, &path).map_err(|problem| format!("{}: {problem}", path.display()))?;
    let mut store = Store::open(&path).map_err(|problem| problem.to_string())?;

    // The newest segments rather than the oldest, because those are the ones a
    // run's own fold folds, and because they are all about the size of the
    // budget where an older run may have left something larger behind.
    let held = store.manifest().segments.len();
    let run = held - sources..held;
    let bytes_in = store.manifest().segments[run.clone()]
        .iter()
        .map(|segment| segment.len)
        .sum();

    let (opening, unchecked, merging, bytes_out) = phases(&store, run.clone())?;

    let started = Instant::now();
    store
        .compact(run, 1_700_000_002, 2)
        .map_err(|problem| problem.to_string())?;
    let whole = millis(started);

    drop(store);
    std::fs::remove_file(&path).ok();
    Ok(Fold {
        sources,
        bytes_in,
        bytes_out,
        opening,
        unchecked,
        merging,
        whole,
    })
}

/// Times opening and merging, which is everything a compaction does before it
/// touches the file.
fn phases(store: &Store, run: Range<usize>) -> Result<(f64, f64, f64, u64), String> {
    let view = store.view().map_err(|problem| problem.to_string())?;

    // Warmed, so the first row of the table is not the one that faults the file
    // in and the rest are not reading what it left behind.
    std::hint::black_box(opened(&view, run.clone())?);

    let mut opening = Vec::with_capacity(ROUNDS);
    for _ in 0..ROUNDS {
        let started = Instant::now();
        let sources = opened(&view, run.clone())?;
        opening.push(millis(started));
        std::hint::black_box(&sources);
    }

    let mut unchecked = Vec::with_capacity(ROUNDS);
    for _ in 0..ROUNDS {
        let started = Instant::now();
        let readers = unhashed(&view, run.clone())?;
        unchecked.push(millis(started));
        std::hint::black_box(&readers);
    }

    let sources = opened(&view, run.clone())?;
    let mut merging = Vec::with_capacity(ROUNDS);
    let mut bytes_out = 0;
    for _ in 0..ROUNDS {
        let started = Instant::now();
        let merged = merge(&sources).map_err(|problem| problem.to_string())?;
        merging.push(millis(started));
        bytes_out = merged.segment.size() as u64;
    }

    Ok((
        middle(&mut opening),
        middle(&mut unchecked),
        middle(&mut merging),
        bytes_out,
    ))
}

/// The sources of a fold, opened the way a fold opens them.
fn opened(view: &View, run: Range<usize>) -> Result<Vec<Source<'_>>, String> {
    let mut sources = Vec::with_capacity(run.len());
    for at in run {
        let bytes = view
            .bytes(at)
            .ok_or_else(|| format!("segment {at} is not in the file"))?;
        let deleted = view.deleted(at).map_err(|problem| problem.to_string())?;
        sources.push(Source::new(bytes, deleted).map_err(|problem| problem.to_string())?);
    }
    Ok(sources)
}

/// The same segments opened without their digests being read.
///
/// Deliberately the same sequence of calls as [`Source::new`], the checks it
/// makes included, so that the difference between the two is the hashing and
/// nothing else. It hands back readers rather than sources because a source
/// cannot be built without the check, which is the point of a source.
fn unhashed(view: &View, run: Range<usize>) -> Result<Vec<Reader<'_>>, String> {
    let mut readers = Vec::with_capacity(run.len());
    for at in run {
        let bytes = view
            .bytes(at)
            .ok_or_else(|| format!("segment {at} is not in the file"))?;
        let segment =
            Segment::open_without_checksum(bytes).map_err(|problem| problem.to_string())?;
        carries(&segment).map_err(|problem| problem.to_string())?;
        let reader = Reader::open(&segment).map_err(|problem| problem.to_string())?;
        let reader = match view.deleted(at).map_err(|problem| problem.to_string())? {
            Some(deleted) => reader
                .hiding(deleted)
                .map_err(|problem| problem.to_string())?,
            None => reader,
        };
        readers.push(reader);
    }
    Ok(readers)
}

/// How long since an instant, in milliseconds.
fn millis(since: Instant) -> f64 {
    since.elapsed().as_secs_f64() * 1_000.0
}

/// The middle of a set of times.
fn middle(times: &mut [f64]) -> f64 {
    times.sort_by(f64::total_cmp);
    times.get(times.len() / 2).copied().unwrap_or_default()
}
