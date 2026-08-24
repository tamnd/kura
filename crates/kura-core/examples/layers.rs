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
//! and a searcher over them. A view opens its readers the first time anybody
//! asks and hands the same ones back afterwards, so this is two numbers rather
//! than one. The opening column is a view nothing has asked anything of yet,
//! which is what the first query after a commit pays, and the again column is
//! the same call on a view that has already answered one, which is what every
//! query after that pays.
//!
//! Asking is the query itself against a searcher that is already open, which is
//! the part no amount of caching removes.
//!
//! The first run of this found that opening did not grow with the segment count
//! at all, because [`kura_core::file::View::reader`] went through
//! [`kura_core::segment::Segment::open`], which hashes every section against the
//! digest in its table before handing back a reader. That costs what the segment
//! holds rather than what the query wants, and the bytes a rung holds are the
//! same however many segments they are cut into, so the whole ladder paid the
//! same 900 microseconds against a query of 2. It does not any more, and the
//! unchecked column is what is left of that: it opens the same segments through
//! [`kura_core::segment::Segment::open_without_checksum`] directly on the same
//! warm view, holding nothing back for a second caller. That is what a query
//! paid before a view kept what it opened, so it is the number the again column
//! is to be read against. It is also what says the hashing has not come back,
//! because it grows with the segment count rather than with the bytes: the
//! ladder holds the same documents on every rung, so a column that hashed
//! everything it opened would be flat and enormous instead.
//!
//! The opening column is not to be read against unchecked, because the two
//! differ in more than one thing. Opening builds a view of its own each time
//! and unchecked runs on a view that has been asked already, so opening pays
//! the first touch of a fresh mapping and unchecked does not. Opening is what
//! the first query after a commit really costs, and that first touch is part of
//! what it really costs, which is why it is left in.
//!
//! The ladder says a segment is a fixed toll rather than a share of the work,
//! and the second table is what the toll is made of. A query does one
//! dictionary lookup per segment before it decodes anything, and the lookup
//! either lands or it does not, so the toll divides three ways and the three
//! have different fixes. A lookup that lands is a binary search over the block
//! index and a scan inside a block, and making it cheaper is a format question.
//! A list header read is what the entry it found points at, and it is probably
//! near the floor already. A miss is the whole of the lookup spent to find out
//! that the segment holds nothing, and the fix for it is a filter in front of
//! the dictionary rather than a faster dictionary.
//!
//! Three terms measure them. One every segment holds, taken from the top of the
//! corpus vocabulary, so a lookup for it lands everywhere. One nothing holds,
//! so a lookup for it misses everywhere. And one a single document holds, so a
//! lookup for it lands once and misses everywhere else, which is the shape a
//! real query for a rare term has. The third is a check on the other two rather
//! than a fourth number: it should come out at a miss in every segment but one
//! plus a landing in that one, and if it does not then the split is wrong.
//!
//! The columns are totals over the whole view rather than per segment figures,
//! because what a query pays is the total. The per segment figures are in the
//! lines under the table, and they are what the toll in the ladder above is to
//! be read against.
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

/// Candidates for a term the corpus does not hold.
///
/// A miss has to be a miss in the dictionary rather than a term the analyser
/// throws away, so these are ordinary letters in an order no language puts
/// them in, and the first one the vocabulary does not hold is the one used. A
/// corpus that holds all of them is a corpus this refuses to guess about.
const ABSENT: [&str; 3] = ["qzvwxkjf", "qzvwxkjfg", "qzvwxkjfgh"];

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
    let filled = fill(&base, &files)?;
    let (documents, most) = (filled.documents, filled.most);
    println!("corpus        {}", corpus.display());
    println!("documents     {documents} in {most} segments before any folding");
    println!(
        "queries       {}",
        filled
            .queries
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!(
        "terms         {} in every segment, {} in one, {} in none",
        filled.picked.common, filled.picked.rare, filled.picked.absent
    );
    println!();
    println!(
        "{:>9} {:>10} {:>10} {:>11} {:>11} {:>11} {:>11} {:>11} {:>11} {:>12}",
        "segments",
        "file",
        "live",
        "opening",
        "again",
        "unchecked",
        "median",
        "p95",
        "p99",
        "postings"
    );

    let mut ladder = Vec::with_capacity(RUNGS.len());
    for rung in RUNGS {
        if rung > most {
            continue;
        }
        let measured = rung_of(&base, directory, rung, &filled)?;
        measured.tell(rung);
        ladder.push((rung, measured));
    }
    std::fs::remove_file(&base).ok();

    if let Some((_, floor)) = ladder.first() {
        println!();
        println!(
            "one segment answers in {:.1} µs and opens in {:.1} µs, and everything above is what the extra segments cost",
            floor.median(),
            floor.opening
        );
        println!(
            "opening the same segment straight through open_without_checksum is {:.1} µs, which is what a query paid before a view kept what it opened",
            floor.unchecked
        );
        println!(
            "asking a view that has already answered one for its readers is {:.2} µs, which is what every query after the first pays",
            floor.again
        );
    }
    tolls(&ladder);
    Ok(())
}

/// The second table, and what the ladder is to be read against.
fn tolls(ladder: &[(usize, Rung)]) {
    println!();
    println!(
        "{:>9} {:>11} {:>11} {:>11} {:>11} {:>11}",
        "segments", "landing", "header", "missing", "rare", "rare holds"
    );
    for (rung, measured) in ladder {
        let toll = &measured.toll;
        println!(
            "{rung:>9} {:>8.2} µs {:>8.2} µs {:>8.2} µs {:>8.2} µs {:>11}",
            toll.landing, toll.header, toll.missing, toll.rare, toll.holds
        );
    }

    let Some((top, measured)) = ladder.last() else {
        return;
    };
    let toll = &measured.toll;
    let each = *top as f64;
    println!();
    println!(
        "at {top} segments a lookup that lands costs {:.0} ns a segment, the list header it found {:.0} ns, and a lookup that misses {:.0} ns",
        toll.landing / each * 1_000.0,
        toll.header / each * 1_000.0,
        toll.missing / each * 1_000.0
    );
    // The rare term is the check rather than a fourth measurement, so what is
    // printed is the difference between what it cost and what the split above
    // says it should have cost. A split that is wrong shows up here as a figure
    // that is not near zero.
    let predicted = toll.missing / each * (each - 1.0) + toll.landing / each;
    println!(
        "a term one segment holds costs {:.2} µs against the {:.2} µs that split predicts, so the split is out by {:.2} µs",
        toll.rare,
        predicted,
        toll.rare - predicted
    );
    if let Some((bottom, floor)) = ladder.first()
        && top > bottom
    {
        let per = (measured.median() - floor.median()) / (each - *bottom as f64);
        println!(
            "a segment costs a query {:.2} µs on this ladder, and the lookup for a term it does not hold is {:.0} percent of that",
            per,
            toll.missing / each / per * 100.0
        );
    }
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

/// The three terms the toll table is measured over.
struct Picked {
    /// A term every segment holds, so a lookup for it lands everywhere.
    common: String,
    /// A term one document holds, so a lookup for it lands in one segment and
    /// misses in the rest.
    rare: String,
    /// A term nothing holds, so a lookup for it misses everywhere.
    absent: String,
}

/// What the corpus came to, and what is to be asked of it.
struct Filled {
    /// The queries the ladder is timed over.
    queries: Vec<String>,
    /// The three terms the toll table is timed over.
    picked: Picked,
    /// How many documents went in.
    documents: usize,
    /// How many segments they landed in, which is the top of the ladder.
    most: usize,
}

/// Indexes the corpus into a store of many segments and picks the queries.
fn fill(path: &Path, files: &[PathBuf]) -> Result<Filled, String> {
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
    let queries: Vec<String> = RANKS
        .iter()
        .filter_map(|rank| vocabulary.get(rank - 1))
        .filter_map(|(term, _)| String::from_utf8(term.clone()).ok())
        .collect();
    let picked = pick(&vocabulary)?;
    Ok(Filled {
        queries,
        picked,
        documents,
        most,
    })
}

/// Whether a term is the shape the toll table wants to probe with.
///
/// The three columns are read against each other, so the three terms have to
/// be alike in everything except how many segments hold them. A dictionary
/// lookup compares the first four bytes of a term against the block index and
/// then scans inside a block, so a one byte term and a twelve byte one are not
/// the same probe, and a term of characters no other term shares a prefix with
/// is not either. Plain lowercase letters of a middling length are what the
/// absent term has to be, since it has to be a term nothing holds, so it is
/// what the other two are held to as well.
fn plain(term: &[u8]) -> bool {
    (4..=12).contains(&term.len()) && term.iter().all(u8::is_ascii_lowercase)
}

/// The three terms the toll table is measured over.
///
/// The vocabulary is in count order, so the common term is the front of it and
/// the rare one is the back. Both ends are walked rather than taken outright,
/// because what is wanted at the front is the most frequent term of the right
/// shape and at the back a term of the right shape that appears exactly once.
fn pick(vocabulary: &[(Vec<u8>, u64)]) -> Result<Picked, String> {
    let common = vocabulary
        .iter()
        .filter(|(term, _)| plain(term))
        .find_map(|(term, _)| String::from_utf8(term.clone()).ok())
        .ok_or_else(|| "the corpus has no common term of the shape this wanted".to_owned())?;
    let rare = vocabulary
        .iter()
        .rev()
        .filter(|(term, count)| *count == 1 && plain(term))
        .find_map(|(term, _)| String::from_utf8(term.clone()).ok())
        .ok_or_else(|| {
            "the corpus holds no term of the shape this wanted exactly once".to_owned()
        })?;
    let held: std::collections::HashSet<&[u8]> =
        vocabulary.iter().map(|(term, _)| term.as_slice()).collect();
    let absent = ABSENT
        .iter()
        .find(|candidate| !held.contains(candidate.as_bytes()))
        .map(|candidate| (*candidate).to_owned())
        .ok_or_else(|| "the corpus holds every term meant to be missing from it".to_owned())?;
    Ok(Picked {
        common,
        rare,
        absent,
    })
}

/// What a term lookup costs across the segments of one rung.
///
/// Totals over the whole view rather than per segment figures, because a query
/// asks every segment and what it pays is the total.
struct Toll {
    /// Looking up a term every segment holds.
    landing: f64,
    /// Taking the posting lists the entries of that lookup point at.
    header: f64,
    /// Looking up a term no segment holds.
    missing: f64,
    /// Looking up a term one segment holds, which is one landing and the rest
    /// misses, and is the check on the other two.
    rare: f64,
    /// How many of the segments hold the rare term, which should be one.
    holds: usize,
}

/// What one rung of the ladder came to.
struct Rung {
    /// How long each query took, in microseconds, pooled across the queries.
    times: Vec<f64>,
    /// How long opening a searcher over a view nothing has asked yet took, in
    /// microseconds.
    opening: f64,
    /// How long the same call took on a view that has already answered one, in
    /// microseconds.
    again: f64,
    /// How long the same opening took with the digests left unchecked and
    /// nothing kept for a second caller, in microseconds.
    unchecked: f64,
    /// How many postings the queries decoded between them.
    postings: u64,
    /// How long the file is.
    bytes: u64,
    /// How many bytes of the file the live segments hold.
    live: u64,
    /// What a term lookup cost across the segments of this rung.
    toll: Toll,
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
            "{:>9} {:>7} MB {:>7} MB {:>8.1} µs {:>8.2} µs {:>8.1} µs {:>8.1} µs {:>8.1} µs {:>8.1} µs {:>12}",
            segments,
            self.bytes / 1_000_000,
            self.live / 1_000_000,
            self.opening,
            self.again,
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
    filled: &Filled,
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
    let measured = ask(&store, &filled.queries, &filled.picked)?;
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

/// The middle of [`ASKS`] runs of something, in microseconds.
fn timed(mut once: impl FnMut() -> Result<(), String>) -> Result<f64, String> {
    let mut times = Vec::with_capacity(ASKS);
    for _ in 0..ASKS {
        let started = Instant::now();
        once()?;
        times.push(started.elapsed().as_secs_f64() * 1_000_000.0);
    }
    times.sort_by(f64::total_cmp);
    Ok(times[times.len() / 2])
}

/// Looks a term up in every segment, the way a query does before it decodes
/// anything.
fn lookup(readers: &[Reader<'_>], term: &[u8]) -> Result<(), String> {
    for reader in readers {
        let found = reader.entry(term).map_err(|problem| problem.to_string())?;
        std::hint::black_box(found);
    }
    Ok(())
}

/// What a term lookup costs across a set of readers that are already open.
fn toll_of(readers: &[Reader<'_>], picked: &Picked) -> Result<Toll, String> {
    // Found outside the timer, because what the header column is about is what
    // the list costs once the dictionary has already said where it sits.
    let mut entries = Vec::with_capacity(readers.len());
    for reader in readers {
        if let Some(entry) = reader
            .entry(picked.common.as_bytes())
            .map_err(|problem| problem.to_string())?
        {
            entries.push((reader, entry));
        }
    }
    if entries.len() != readers.len() {
        return Err(format!(
            "the common term is in {} of {} segments, so it is not the term this wanted",
            entries.len(),
            readers.len()
        ));
    }
    let mut holds = 0;
    for reader in readers {
        if reader
            .entry(picked.rare.as_bytes())
            .map_err(|problem| problem.to_string())?
            .is_some()
        {
            holds += 1;
        }
    }

    let landing = timed(|| lookup(readers, picked.common.as_bytes()))?;
    let missing = timed(|| lookup(readers, picked.absent.as_bytes()))?;
    let rare = timed(|| lookup(readers, picked.rare.as_bytes()))?;
    let header = timed(|| {
        for (reader, entry) in &entries {
            let list = reader.list(*entry).map_err(|problem| problem.to_string())?;
            std::hint::black_box(&list);
        }
        Ok(())
    })?;

    Ok(Toll {
        landing,
        header,
        missing,
        rare,
        holds,
    })
}

/// Times the queries against a store that is already open.
fn ask(store: &Store, queries: &[String], picked: &Picked) -> Result<Rung, String> {
    let view = store.view().map_err(|problem| problem.to_string())?;

    // Warmed first, so that the times are of the query rather than of the page
    // faults the first pass takes.
    {
        let readers = view.readers().map_err(|problem| problem.to_string())?;
        let searcher = Searcher::over(readers).map_err(|problem| problem.to_string())?;
        for query in queries {
            searcher
                .search(query, 10)
                .map_err(|problem| problem.to_string())?;
        }
    }

    // A view of its own for each try, because a view opens its readers the
    // first time anybody asks and hands the same ones back afterwards, so a
    // loop over one view would be timing the handing back. Building the view
    // is outside the timer: what is wanted here is the opening rather than the
    // mapping, and the two are separate costs a commit pays.
    let mut opening = Vec::with_capacity(ASKS);
    for _ in 0..ASKS {
        let fresh = store.view().map_err(|problem| problem.to_string())?;
        let started = Instant::now();
        let readers = fresh.readers().map_err(|problem| problem.to_string())?;
        let searcher = Searcher::over(readers).map_err(|problem| problem.to_string())?;
        opening.push(started.elapsed().as_secs_f64() * 1_000_000.0);
        std::hint::black_box(&searcher);
    }
    opening.sort_by(f64::total_cmp);

    // The same call on the view the warm up already asked, which is what every
    // query after the first one pays.
    let mut again = Vec::with_capacity(ASKS);
    for _ in 0..ASKS {
        let started = Instant::now();
        let readers = view.readers().map_err(|problem| problem.to_string())?;
        let searcher = Searcher::over(readers).map_err(|problem| problem.to_string())?;
        again.push(started.elapsed().as_secs_f64() * 1_000_000.0);
        std::hint::black_box(&searcher);
    }
    again.sort_by(f64::total_cmp);

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
    let toll = toll_of(readers, picked)?;
    let searcher = Searcher::over(readers).map_err(|problem| problem.to_string())?;
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
        again: again[again.len() / 2],
        unchecked: skipped[skipped.len() / 2],
        postings,
        bytes: 0,
        live: 0,
        toll,
    })
}
