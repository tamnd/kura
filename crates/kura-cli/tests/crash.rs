//! A run killed while it is writing leaves a store that opens.
//!
//! The claim the format makes is that nothing a fold writes is reachable until
//! the commit at the end of it, and that the segments it is folding are all
//! still where they were, so there is nothing for a half written fold to
//! corrupt. The same goes for a batch: a commit is one manifest write into the
//! slot that was not being read, so a run that stops holds whatever it had
//! committed and nothing else.
//!
//! That was checked by hand for #159, twelve runs killed at delays from a
//! quarter of a second to nine tenths of one. This is that by machine. It lives
//! in `tests` rather than beside the code because the thing being tested is a
//! process ending, and the tests inside the crate call `index` in process, so
//! there is nothing there to kill.
//!
//! The moment of the kill is not deterministic and this does not pretend it is.
//! A full run is timed first and the kills land at fractions of what it took,
//! which is what makes the delays mean the same thing on a fast machine and a
//! slow one. Whether a given kill lands inside a fold, inside a commit or
//! between two batches is up to the scheduler. What makes it worth running
//! anyway is that the assertions hold wherever it lands, so every run of the
//! suite is another sample and the interesting moments accumulate rather than
//! having to be aimed at.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use kura_core::file::Store;
use kura_core::search::Searcher;

/// How many documents the corpus holds.
const DOCUMENTS: u64 = 2_000;

/// How many words go in a document, which is what makes the run long enough to
/// be killed partway rather than only before or after.
const LENGTH: usize = 400;

/// The vocabulary, arranged so that every word lands in every document and any
/// one of them counts the corpus.
const WORDS: [&str; 8] = [
    "ledger", "invoice", "quarter", "audit", "balance", "entry", "account", "posting",
];

/// The word the counting is done with.
const EVERYWHERE: &str = "ledger";

/// How much text a writer holds before it starts a new segment.
///
/// Small enough that the run commits several times and folds rather than
/// writing one segment at the end, which is what puts a fold in the window a
/// kill can land in. Below about seven hundred kilobytes on this corpus the
/// segment count explodes and the run takes twenty seconds, which is not a
/// thing to put in a test suite.
const BUDGET: &str = "800k";

/// How many threads fill batches.
const THREADS: &str = "4";

/// One thread, which is the setting that keeps a log.
const ALONE: &str = "1";

/// Where the kills land, as percentages of how long the full run took.
const FRACTIONS: [u32; 5] = [15, 30, 45, 60, 80];

/// How many times a delay that outlasted its run is halved before the round
/// gives up on landing inside one.
const HALVINGS: usize = 5;

#[test]
fn a_run_killed_partway_leaves_a_store_that_opens_and_verifies() {
    let work = scratch("many");
    let corpus = work.join("corpus");
    write_corpus(&corpus);

    let whole = work.join("whole.kura");
    let full = timed(&corpus, &whole, THREADS);
    assert_eq!(counted(&whole), (DOCUMENTS, DOCUMENTS));

    let mut checked = 0;
    for (round, percent) in FRACTIONS.into_iter().enumerate() {
        let store = work.join(format!("killed-{round}.kura"));
        if !interrupted(&corpus, &store, THREADS, full * percent / 100) || !store.exists() {
            // Either the run outran every delay this round had, or the kill
            // landed before the store was created, which leaves nothing to say
            // anything about.
            continue;
        }
        checked += 1;

        let (live, found) = counted(&store);
        assert!(
            live <= DOCUMENTS,
            "round {round} left {live} live documents out of a corpus of {DOCUMENTS}"
        );
        assert_eq!(
            live, found,
            "round {round} has a document live more than once, or a live one no query finds"
        );
        verifies(&store, round);

        // What the kill cost is put back by running again over the same store,
        // because indexing a directory into a store is an update rather than a
        // second copy of it.
        let again = index(&corpus, &store, THREADS)
            .status()
            .expect("the tool ran");
        assert!(again.success(), "round {round} indexes again");
        assert_eq!(
            counted(&store),
            (DOCUMENTS, DOCUMENTS),
            "round {round} does not come back to the whole corpus"
        );
        verifies(&store, round);
    }

    assert!(
        checked > 0,
        "no kill landed inside a run of {full:?}, so this proved nothing"
    );
    let _ = std::fs::remove_dir_all(&work);
}

#[test]
fn what_a_killed_run_had_not_committed_comes_back_out_of_the_log() {
    let work = scratch("log");
    let corpus = work.join("corpus");
    write_corpus(&corpus);

    // One thread, because that is the setting with a log. Above one a record
    // would go into the ring as its document arrives and the ring is reached
    // through the store, which one thread at a time holds, so the tool says
    // there is no log and a run that stops loses what it had not committed.
    let whole = work.join("whole.kura");
    let full = timed(&corpus, &whole, ALONE);

    let mut proved = false;
    for (round, percent) in FRACTIONS.into_iter().enumerate() {
        let store = work.join(format!("logged-{round}.kura"));
        if !interrupted(&corpus, &store, ALONE, full * percent / 100) || !store.exists() {
            continue;
        }

        let (live, found) = counted(&store);
        assert_eq!(
            live, found,
            "round {round} has a document live more than once, or a live one no query finds"
        );

        let said = ran(&corpus, &store, ALONE, round);
        let Some(put_back) = put_back(&said) else {
            // The kill landed in the moment after a commit and before the next
            // document, so there was nothing left over to put back. Nothing is
            // wrong with that, but the round says nothing about a log either.
            continue;
        };
        assert!(
            live + put_back <= DOCUMENTS,
            "round {round} put back {put_back} documents on top of {live} live ones, \
             so the log held what the store already had"
        );
        assert_eq!(
            counted(&store),
            (DOCUMENTS, DOCUMENTS),
            "round {round} does not come back to the whole corpus"
        );
        verifies(&store, round);
        proved = true;
        break;
    }

    assert!(
        proved,
        "no kill left anything in the log across runs of {full:?}, so this proved nothing"
    );
    let _ = std::fs::remove_dir_all(&work);
}

/// A directory of this test's own, cleared if a run before this one left
/// something in it.
fn scratch(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("kura-crash-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("a working directory");
    path
}

/// Writes the corpus.
fn write_corpus(at: &Path) {
    std::fs::create_dir_all(at).expect("a corpus directory");
    for n in 0..DOCUMENTS {
        let at_word = usize::try_from(n).expect("a small corpus");
        let mut text = format!("key{n}");
        for word in 0..LENGTH {
            text.push(' ');
            text.push_str(WORDS[(at_word + word) % WORDS.len()]);
        }
        text.push('\n');
        std::fs::write(at.join(format!("doc-{n:05}.txt")), text).expect("a document");
    }
}

/// How long a whole run takes, which is what the delays below are fractions of.
///
/// A run before it and thrown away, because the first run of a test is the one
/// that reads the corpus off the disk and loads the binary, and timing that one
/// gives a number a third larger than any run after it. Delays taken from the
/// larger number all land after the run they were meant to interrupt.
fn timed(corpus: &Path, at: &Path, threads: &str) -> Duration {
    let warm = at.with_extension("warm");
    let done = index(corpus, &warm, threads)
        .status()
        .expect("the tool ran");
    assert!(done.success(), "a warming run finishes");
    let _ = std::fs::remove_file(&warm);

    let started = Instant::now();
    let done = index(corpus, at, threads).status().expect("the tool ran");
    let full = started.elapsed();
    assert!(done.success(), "an uninterrupted run finishes");
    full
}

/// Kills a run at a delay, halving the delay until one lands inside a run.
///
/// How long a run takes moves with whatever else the machine is doing, so a
/// delay is a guess however carefully the run before it was timed. A guess that
/// came out high would leave a finished store and prove nothing, so rather than
/// fail on it this halves and goes again. The store the finished run left is
/// removed first, so that what the caller checks is only ever a store a kill
/// left behind.
fn interrupted(corpus: &Path, store: &Path, threads: &str, mut after: Duration) -> bool {
    for _ in 0..HALVINGS {
        let mut child = index(corpus, store, threads)
            .spawn()
            .expect("the tool started");
        std::thread::sleep(after);
        let _ = child.kill();
        let stopped = child.wait().expect("the tool stopped");
        if !stopped.success() {
            return true;
        }
        let _ = std::fs::remove_file(store);
        after /= 2;
    }
    false
}

/// The command that indexes the corpus into a store.
fn index(corpus: &Path, store: &Path, threads: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_kura-cli"));
    command
        .arg("index")
        .arg(corpus)
        .arg("-o")
        .arg(store)
        .arg("--store")
        .arg("--threads")
        .arg(threads)
        .arg("--memory")
        .arg(BUDGET)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    command
}

/// Indexes and hands back what the run said about itself.
fn ran(corpus: &Path, store: &Path, threads: &str, round: usize) -> String {
    let said = index(corpus, store, threads)
        .stdout(std::process::Stdio::piped())
        .output()
        .expect("the tool ran");
    assert!(said.status.success(), "round {round} indexes again");
    String::from_utf8_lossy(&said.stdout).into_owned()
}

/// How many documents a run said it took out of the log, if it found any.
fn put_back(said: &str) -> Option<u64> {
    let line = said.lines().find(|line| line.starts_with("put back "))?;
    line.split_whitespace().nth(2)?.parse().ok()
}

/// How many documents a store says are live, and how many a query finds.
///
/// The two are the same for a store nothing went wrong in. They come apart if a
/// document is live twice, which is what a commit applied without its deletion
/// would leave, or if one is live and unreachable, which is what a manifest
/// pointing past what was written would leave.
fn counted(store: &Path) -> (u64, u64) {
    let store = Store::open(store).expect("the store opens");
    let live = store.manifest().live;
    let view = store.view().expect("a view");
    let readers = view.readers().expect("readers");
    let searcher = Searcher::over(&readers).expect("a searcher");
    let found = searcher.count(EVERYWHERE).expect("counted");
    (live, found)
}

/// Runs the tool's own verify over a store and fails with what it said.
fn verifies(store: &Path, round: usize) {
    let said = Command::new(env!("CARGO_BIN_EXE_kura-cli"))
        .arg("verify")
        .arg(store)
        .output()
        .expect("the tool ran");
    assert!(
        said.status.success(),
        "round {round} does not verify: {}",
        String::from_utf8_lossy(&said.stderr)
    );
}
