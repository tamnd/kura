//! What a segment count costs a query.
//!
//! Run it with `cargo run --release --example layers -- <corpus> [<directory>]`,
//! where the corpus is a directory of text files.
//!
//! The claim this settles is one that gets made all over this repository: a
//! store of several segments answers more slowly than the same documents in
//! one, because a term costs a posting list walk per segment and a key costs a
//! filter per segment. That is the reason an index run pays to fold what it
//! wrote on the way out, and the reason a fold beside the writers is worth the
//! ingest it costs. It has been asserted from the shape of the code and from a
//! handful of queries timed one process at a time, which is not a number.
//!
//! This builds the ladder and measures it. The corpus is indexed once into a
//! store of many segments, and then a copy of that store is folded down to each
//! of a series of counts, so every rung holds exactly the same documents and
//! differs only in how many segments they are spread across. The queries come
//! out of the corpus, taken at ranks 1, 10, 100, 1,000 and 10,000 in what the
//! analyser produced, which is a spread from a term in nearly every document to
//! one in a handful.
//!
//! Two costs are reported separately, because a caller can do something about
//! one of them and nothing about the other.
//!
//! Opening is what a reader pays before it asks anything: a reader per segment
//! and a searcher over them. A server that holds a searcher open across queries
//! pays this once per commit rather than once per query.
//!
//! Asking is the query itself against a searcher that is already open, which is
//! the part no amount of caching removes.
//!
//! Opening turns out not to grow with the segment count at all, and the reason
//! is worth the extra column in the table. [`kura_core::file::View::reader`]
//! goes through [`kura_core::segment::Segment::open`], which hashes every
//! section against the digest in its table before handing back a reader. That
//! costs what the segment holds rather than what the query wants, and the bytes
//! a rung holds are the same however many segments they are cut into, so the
//! whole ladder pays about the same. The unchecked column opens the same
//! segments through
//! [`kura_core::segment::Segment::open_without_checksum`] and is the same
//! structural parse without the hashing, which is what the difference between
//! the two columns is measuring.
//!
//! The file size is in the table because the ladder is built by folding, and a
//! fold appends the segment it made rather than replacing the ones it read, so
//! a rung with fewer segments sits in a longer file. Nothing reads the stranded
//! part, but it is mapped, and it is the honest thing to show rather than to
//! leave for somebody to find. The live column beside it is what the manifest
//! still points at, which is the part an open walks and the part a query reads.

// Every cast here feeds a printed number that is already approximate.
#![allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use kura_core::analysis::Analyzer;
use kura_core::file::{Store, View};
use kura_core::index::Reader;
use kura_core::ingest::Batch;
use kura_core::search::Searcher;
use kura_core::segment::Segment;

/// A store identifier, so a file written by this says what wrote it.
const STORE: u128 = 0x006b_7572_612d_6c61_7965_7273_0000_0001;

/// How much text goes into a segment before a new one is started.
///
/// Small enough that a corpus of a hundred megabytes lands at a couple of dozen
/// segments, which is further than a store should ever be allowed to get and so
/// the right end of the range to measure from.
const BUDGET: u64 = 4 * 1024 * 1024;

/// The rungs, in segments.
///
/// Anything the store did not reach is skipped rather than made up.
const RUNGS: [usize; 6] = [1, 2, 4, 8, 16, 24];

/// The ranks in the corpus vocabulary the queries are taken from.
const RANKS: [usize; 5] = [1, 10, 100, 1_000, 10_000];

/// How many times each query is asked, after a warming pass.
const ASKS: usize = 200;

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(corpus) = args.next().map(PathBuf::from) else {
        eprintln!("usage: layers <corpus> [<directory>]");
        eprintln!("the corpus is a directory of text files, and the stores go in the directory");
        std::process::exit(2);
    };
    let directory = args.next().map_or_else(std::env::temp_dir, PathBuf::from);

    if let Err(problem) = run(&corpus, &directory) {
        eprintln!("layers: {problem}");
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

    let base = directory.join("kura-layers-base.kura");
    let (queries, documents, most) = fill(&base, &files)?;
    println!("corpus        {}", corpus.display());
    println!("documents     {documents} in {most} segments before any folding");
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
        "{:>9} {:>10} {:>10} {:>11} {:>11} {:>11} {:>11} {:>11} {:>12}",
        "segments", "file", "live", "opening", "unchecked", "median", "p95", "p99", "postings"
    );

    let mut floor = None;
    for rung in RUNGS {
        if rung > most {
            continue;
        }
        let measured = rung_of(&base, directory, rung, &queries)?;
        measured.tell(rung);
        if floor.is_none() {
            floor = Some(measured);
        }
    }
    std::fs::remove_file(&base).ok();

    if let Some(floor) = floor {
        println!();
        println!(
            "one segment answers in {:.1} µs and opens in {:.1} µs, and everything above is what the extra segments cost",
            floor.median(),
            floor.opening
        );
        println!(
            "opening the same segment without checking its digests is {:.1} µs, which is {:.0} times less, so what an open costs is hashing bytes the query never reads",
            floor.unchecked,
            floor.opening / floor.unchecked
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

/// Indexes the corpus into a store of many segments and picks the queries.
///
/// Returns the queries, how many documents went in and how many segments they
/// landed in, which is the top of the ladder.
fn fill(path: &Path, files: &[PathBuf]) -> Result<(Vec<String>, usize, usize), String> {
    std::fs::remove_file(path).ok();
    let mut store =
        Store::create(path, STORE, 1_700_000_000).map_err(|problem| problem.to_string())?;
    let mut analyzer = Analyzer::new();
    let mut counts: HashMap<Vec<u8>, u64> = HashMap::new();
    let mut documents = 0;

    let mut at = 0;
    while at < files.len() {
        let view = store.view().map_err(|problem| problem.to_string())?;
        let mut batch = Batch::with_budget(&view, BUDGET).map_err(|problem| problem.to_string())?;
        while at < files.len() {
            let path = &files[at];
            at += 1;
            let Some(text) = text_of(path) else { continue };
            analyzer.analyze(&text, |term, _| {
                *counts.entry(term.to_vec()).or_default() += 1;
            });
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
    drop(store);

    let mut vocabulary: Vec<_> = counts.into_iter().collect();
    vocabulary.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    let queries = RANKS
        .iter()
        .filter_map(|rank| vocabulary.get(rank - 1))
        .filter_map(|(term, _)| String::from_utf8(term.clone()).ok())
        .collect();
    Ok((queries, documents, most))
}

/// What one rung of the ladder came to.
struct Rung {
    /// How long each query took, in microseconds, pooled across the queries.
    times: Vec<f64>,
    /// How long opening a searcher over the store took, in microseconds.
    opening: f64,
    /// How long the same opening took with the digests left unchecked, in
    /// microseconds.
    unchecked: f64,
    /// How many postings the queries decoded between them.
    postings: u64,
    /// How long the file is.
    bytes: u64,
    /// How many bytes of the file the live segments hold.
    live: u64,
}

impl Rung {
    /// The time at a percentile, in microseconds.
    fn at(&self, percentile: usize) -> f64 {
        if self.times.is_empty() {
            return 0.0;
        }
        let mut sorted = self.times.clone();
        sorted.sort_by(f64::total_cmp);
        sorted[(sorted.len() - 1) * percentile / 100]
    }

    /// The middle of the times, in microseconds.
    fn median(&self) -> f64 {
        self.at(50)
    }

    /// Prints it.
    fn tell(&self, segments: usize) {
        println!(
            "{:>9} {:>7} MB {:>7} MB {:>8.1} µs {:>8.1} µs {:>8.1} µs {:>8.1} µs {:>8.1} µs {:>12}",
            segments,
            self.bytes / 1_000_000,
            self.live / 1_000_000,
            self.opening,
            self.unchecked,
            self.median(),
            self.at(95),
            self.at(99),
            self.postings,
        );
    }
}

/// Folds a copy of the base store down to `segments` and measures it.
fn rung_of(
    base: &Path,
    directory: &Path,
    segments: usize,
    queries: &[String],
) -> Result<Rung, String> {
    let path = directory.join(format!("kura-layers-{segments}.kura"));
    std::fs::remove_file(&path).ok();
    std::fs::copy(base, &path).map_err(|problem| format!("{}: {problem}", path.display()))?;
    let mut store = Store::open(&path).map_err(|problem| problem.to_string())?;

    // Everything but the newest few, folded into one. The newest are left where
    // they are so that the rung differs from the one below it in the segment
    // count and in nothing else.
    let held = store.manifest().segments.len();
    if held > segments {
        store
            .compact(0..held - segments + 1, 1_700_000_002, 2)
            .map_err(|problem| problem.to_string())?;
    }
    let reached = store.manifest().segments.len();
    if reached != segments {
        return Err(format!("asked for {segments} segments and got {reached}"));
    }

    let bytes = std::fs::metadata(&path)
        .map(|about| about.len())
        .unwrap_or_default();
    let live = store
        .manifest()
        .segments
        .iter()
        .map(|segment| segment.len)
        .sum();
    let measured = ask(&store, queries)?;
    drop(store);
    std::fs::remove_file(&path).ok();
    Ok(Rung {
        bytes,
        live,
        ..measured
    })
}

/// Opens a reader per segment the way [`View::reader`] does, minus the digests.
///
/// This is the comparison the unchecked column reports. It is deliberately the
/// same sequence of calls, so that the only difference between the two numbers
/// is the hashing that [`Segment::open`] does and this does not.
fn unchecked(view: &View) -> Result<Vec<Reader<'_>>, String> {
    let mut readers = Vec::with_capacity(view.len());
    for at in 0..view.len() {
        let bytes = view
            .bytes(at)
            .ok_or_else(|| format!("segment {at} is not in the file"))?;
        let segment =
            Segment::open_without_checksum(bytes).map_err(|problem| problem.to_string())?;
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

/// Times the queries against a store that is already open.
fn ask(store: &Store, queries: &[String]) -> Result<Rung, String> {
    let view = store.view().map_err(|problem| problem.to_string())?;

    // Warmed first, so that the times are of the query rather than of the page
    // faults the first pass takes.
    {
        let readers = view.readers().map_err(|problem| problem.to_string())?;
        let searcher = Searcher::over(&readers).map_err(|problem| problem.to_string())?;
        for query in queries {
            searcher
                .search(query, 10)
                .map_err(|problem| problem.to_string())?;
        }
    }

    let mut opening = Vec::with_capacity(ASKS);
    for _ in 0..ASKS {
        let started = Instant::now();
        let readers = view.readers().map_err(|problem| problem.to_string())?;
        let searcher = Searcher::over(&readers).map_err(|problem| problem.to_string())?;
        opening.push(started.elapsed().as_secs_f64() * 1_000_000.0);
        std::hint::black_box(&searcher);
    }
    opening.sort_by(f64::total_cmp);

    let mut skipped = Vec::with_capacity(ASKS);
    for _ in 0..ASKS {
        let started = Instant::now();
        let readers = unchecked(&view)?;
        let searcher = Searcher::over(&readers).map_err(|problem| problem.to_string())?;
        skipped.push(started.elapsed().as_secs_f64() * 1_000_000.0);
        std::hint::black_box(&searcher);
    }
    skipped.sort_by(f64::total_cmp);

    let readers = view.readers().map_err(|problem| problem.to_string())?;
    let searcher = Searcher::over(&readers).map_err(|problem| problem.to_string())?;
    let mut times = Vec::with_capacity(ASKS * queries.len());
    let mut postings = 0;
    for query in queries {
        for ask in 0..ASKS {
            let started = Instant::now();
            let (hits, counters) = searcher
                .search_explained(query, 10)
                .map_err(|problem| problem.to_string())?;
            times.push(started.elapsed().as_secs_f64() * 1_000_000.0);
            // Once per query, since every ask of the same query decodes the
            // same postings and a sum over the asks would be a count of how
            // long the loop ran.
            if ask == 0 {
                postings += counters.postings_decoded;
            }
            std::hint::black_box(hits);
        }
    }

    Ok(Rung {
        times,
        opening: opening[opening.len() / 2],
        unchecked: skipped[skipped.len() / 2],
        postings,
        bytes: 0,
        live: 0,
    })
}
