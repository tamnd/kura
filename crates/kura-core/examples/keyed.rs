//! What a key table costs an index run, measured on real text.
//!
//! Run it with `cargo run --release --example keyed -- <corpus> [<megabytes>]`,
//! where the corpus is a directory of text files.
//!
//! Every document in a store that can be updated goes in under a key, because a
//! document nobody can name is a document nobody can replace. That costs
//! something on the way in, and until now the only readings of it were of a run
//! that had keys against a different run on a different day that did not, which
//! is not a reading of anything. The benchmark suite noticed it the same way:
//! when its runner started writing keys, the index phase moved, and the machine
//! it moved on was busy, so nothing could be said about how much of the move was
//! the keys.
//!
//! This measures the two against each other in one process. The corpus is read
//! into memory once, and then indexed twice, once through
//! [`kura_core::index::Writer::add`] and once through
//! [`kura_core::index::Writer::add_keyed`], with nothing else different: the
//! same documents in the same order into a writer of the same shape.
//!
//! The two cases alternate which of them goes first, round by round. A machine
//! with somebody else's work on it drifts over the minutes a run takes, and a
//! run that always measured the plain case first would hand the keyed case
//! whatever the machine had become by then. Alternating cancels a drift in one
//! direction, which is the common one. It cannot cancel a spike, which is what
//! the median over the rounds is for.
//!
//! The corpus is held in memory on purpose. Reading a file is a syscall and a
//! page fault and neither of them is what this is asking about, and a corpus
//! read from the disk on one case and from the page cache on the other would
//! produce a difference that has nothing to do with keys. The megabyte argument
//! is what bounds it, so a small machine runs the same measurement over less.
//!
//! What it prints is the wall time of adding, the wall time of building the
//! segment, and the bytes the segment came to, for each case, and then what the
//! difference between them comes to per document.

// Every cast here feeds a printed number that is already approximate.
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use std::path::{Path, PathBuf};
use std::time::Instant;

use kura_core::index;

/// How many times each case is run.
///
/// Even, so that each case leads the same number of rounds. Enough that a
/// median survives one round landing on somebody else's compile, and few enough
/// that a corpus of tens of megabytes finishes while somebody is watching.
const ROUNDS: usize = 6;

/// How much text is read when the run does not say.
const MEGABYTES: usize = 64;

/// What one run of one case cost.
struct Case {
    /// Wall seconds handing every document to the writer.
    adding: f64,
    /// Wall seconds turning the writer into the bytes of a segment.
    building: f64,
    /// What those bytes came to.
    bytes: usize,
}

fn main() {
    if let Err(problem) = run() {
        eprintln!("keyed: {problem}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let root = args.next().unwrap_or_else(|| ".".to_string());
    let budget = args
        .next()
        .map_or(Ok(MEGABYTES), |value| value.parse::<usize>())
        .map_err(|problem| format!("the megabyte argument: {problem}"))?
        * 1024
        * 1024;

    let mut files = Vec::new();
    walk(Path::new(&root), &mut files)?;
    files.sort();
    let documents = load(&files, budget);
    if documents.is_empty() {
        return Err(format!("{root} holds no text files"));
    }
    let bytes = documents.iter().map(|(_, text)| text.len()).sum::<usize>();
    println!(
        "{} documents, {:.1} MB of text, {} rounds of each case",
        documents.len(),
        bytes as f64 / (1024.0 * 1024.0),
        ROUNDS,
    );

    let mut plain = Vec::with_capacity(ROUNDS);
    let mut keyed = Vec::with_capacity(ROUNDS);
    for round in 0..ROUNDS {
        // The case that goes first is the one that gets the machine as it was
        // at the start of the round, so the two take turns having it.
        if round % 2 == 0 {
            plain.push(once(&documents, false)?);
            keyed.push(once(&documents, true)?);
        } else {
            keyed.push(once(&documents, true)?);
            plain.push(once(&documents, false)?);
        }
    }
    report(&plain, &keyed, documents.len());
    Ok(())
}

/// Indexes the corpus once, with keys or without them.
fn once(documents: &[(String, String)], keyed: bool) -> Result<Case, String> {
    let mut writer = index::Writer::new();
    let started = Instant::now();
    for (key, text) in documents {
        if keyed {
            writer.add_keyed(key.as_bytes(), text)
        } else {
            writer.add(text)
        }
        .map_err(|problem| problem.to_string())?;
    }
    let adding = started.elapsed().as_secs_f64();

    let started = Instant::now();
    let segment = writer.finish().map_err(|problem| problem.to_string())?;
    let building = started.elapsed().as_secs_f64();
    Ok(Case {
        adding,
        building,
        bytes: segment.len(),
    })
}

/// The middle of what the rounds gave, taken column by column.
fn middle(cases: &[Case], of: impl Fn(&Case) -> f64) -> f64 {
    let mut times: Vec<f64> = cases.iter().map(&of).collect();
    times.sort_by(f64::total_cmp);
    times[times.len() / 2]
}

/// Prints the table and what the difference between the two rows comes to.
fn report(plain: &[Case], keyed: &[Case], documents: usize) {
    let rows = [("plain", plain), ("keyed", keyed)];
    println!();
    println!(
        "{:<8} {:>10} {:>10} {:>10} {:>10} {:>12}",
        "case", "adding", "building", "total", "quickest", "segment"
    );
    for (name, cases) in rows {
        let adding = middle(cases, |case| case.adding);
        let building = middle(cases, |case| case.building);
        let quickest = cases
            .iter()
            .map(|case| case.adding + case.building)
            .fold(f64::INFINITY, f64::min);
        println!(
            "{:<8} {:>9.2}s {:>9.2}s {:>9.2}s {:>9.2}s {:>9.1} MB",
            name,
            adding,
            building,
            adding + building,
            quickest,
            middle(cases, |case| case.bytes as f64) / (1024.0 * 1024.0),
        );
    }

    // The rounds themselves, because a median of six on a machine somebody else
    // is using says nothing about how far apart they were, and a reader who can
    // see them can tell a difference from a spread.
    for (name, cases) in rows {
        print!("{name} rounds");
        for case in cases {
            print!(" {:.2}", case.adding + case.building);
        }
        println!();
    }

    let adding = (middle(plain, |c| c.adding), middle(keyed, |c| c.adding));
    let building = (middle(plain, |c| c.building), middle(keyed, |c| c.building));
    let size = (
        middle(plain, |c| c.bytes as f64),
        middle(keyed, |c| c.bytes as f64),
    );
    let total = (adding.0 + building.0, adding.1 + building.1);
    println!();
    println!(
        "keys cost {:.0} percent of the adding, {:.0} of the building and {:.0} percent of the run",
        share(adding),
        share(building),
        share(total),
    );
    println!(
        "and {:.0} percent of the segment, which is {:.1} MB on {:.1}",
        share(size),
        (size.1 - size.0) / (1024.0 * 1024.0),
        size.0 / (1024.0 * 1024.0),
    );
    let each = documents as f64;
    println!(
        "a document costs {:.2} µs more to add and {:.0} more bytes to keep",
        (adding.1 - adding.0) * 1_000_000.0 / each,
        (size.1 - size.0) / each,
    );
}

/// What the second of a pair is above the first, as a percentage.
fn share((without, with): (f64, f64)) -> f64 {
    if without <= 0.0 {
        return 0.0;
    }
    (with - without) / without * 100.0
}

/// Reads the files as documents, stopping once they come to `budget` bytes.
///
/// The key is the path, which is what a caller indexing a tree would use and
/// has the properties that make key data awkward: long, sharing prefixes, and
/// differing at the end rather than the beginning.
fn load(files: &[PathBuf], budget: usize) -> Vec<(String, String)> {
    let mut documents = Vec::new();
    let mut held = 0;
    for path in files {
        if held >= budget {
            break;
        }
        let Some(text) = text_of(path) else { continue };
        held += text.len();
        documents.push((path.to_string_lossy().into_owned(), text));
    }
    documents
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
