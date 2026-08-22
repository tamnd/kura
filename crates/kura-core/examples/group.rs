//! What committing several batches together is worth on the machine it is run
//! on.
//!
//! Run it with `cargo run --release --example group -- <directory>`, or with no
//! argument to use the temporary directory. It writes a store there, commits the
//! same number of small batches into it in groups of one, two, four and so on,
//! and reports what each group size cost.
//!
//! A commit is two syncs, whatever it holds, and a sync is milliseconds. So the
//! cost of taking small writes is the number of commits rather than the number
//! of documents, and the way to move it is to put more into each commit. That is
//! all a group commit is. The store ends up the same either way: the same
//! segments, in the same order, with the same documents in them.
//!
//! The documents are made up, and that is deliberate. What is being measured is
//! how often the store waits for the drive, which the text of a document has
//! nothing to do with. The numbers to compare against a real corpus are the
//! indexing ones, and they are measured elsewhere.
//!
//! Point it at the filesystem the store will live on. The answer is a property
//! of that filesystem and that device.

// Every cast here feeds a printed number that is already approximate.
#![allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]

use std::time::Instant;

use kura_core::file::Store;
use kura_core::ingest::{Batch, Prepared, commit_all};

/// How many commits per run, whatever the group size.
///
/// The store ends up holding this many segments every time, so the group size
/// changes how often the run waited for the drive and nothing else about it.
const COMMITS: usize = 128;

/// How many documents in each of them.
///
/// Small, because this is about a store taking writes as they arrive rather
/// than a corpus being loaded. A batch large enough to be worth a commit of its
/// own does not need any of this.
const DOCS: usize = 4;

/// The group sizes to measure.
const SIZES: [usize; 6] = [1, 2, 4, 8, 16, 32];

/// A store identifier, so a file written by this says what wrote it.
const STORE: u128 = 0x006b_7572_612d_6772_6f75_7000_0000_0001;

fn main() {
    let directory = std::env::args()
        .nth(1)
        .map_or_else(std::env::temp_dir, std::path::PathBuf::from);

    println!("directory     {}", directory.display());
    println!(
        "commits       {COMMITS} of {DOCS} documents each, {} in all",
        COMMITS * DOCS
    );
    println!();
    println!(
        "{:<7} {:>8} {:>10} {:>12} {:>11} {:>11} {:>11}",
        "group", "syncs", "syncs/1k", "commits/s", "median", "p99", "worst"
    );

    for size in SIZES {
        match measure(&directory, size) {
            Ok(run) => run.tell(size),
            Err(problem) => println!("{size:<7} {problem}"),
        }
    }
}

/// What one run came to.
struct Run {
    /// How many times it waited for the drive.
    syncs: u64,
    /// How long each group's commit took, in milliseconds.
    times: Vec<f64>,
}

impl Run {
    /// Prints it.
    fn tell(&self, size: usize) {
        let mut sorted = self.times.clone();
        sorted.sort_by(f64::total_cmp);
        let median = sorted[sorted.len() / 2];
        let total: f64 = sorted.iter().sum();
        let documents = (COMMITS * DOCS) as f64;
        println!(
            "{:<7} {:>8} {:>10.1} {:>12.0} {:>9.3}ms {:>9.3}ms {:>9.3}ms",
            size,
            self.syncs,
            self.syncs as f64 * 1000.0 / documents,
            COMMITS as f64 / (total / 1000.0).max(f64::MIN_POSITIVE),
            median,
            sorted[sorted.len() * 99 / 100],
            sorted[sorted.len() - 1],
        );
    }
}

/// Commits `COMMITS` batches in groups of `size` into a store of its own.
fn measure(directory: &std::path::Path, size: usize) -> Result<Run, String> {
    let path = directory.join(format!("kura-group-{size}.kura"));
    std::fs::remove_file(&path).ok();
    let mut store =
        Store::create(&path, STORE, 1_700_000_000).map_err(|problem| problem.to_string())?;

    let mut times = Vec::with_capacity(COMMITS / size);
    let before = store.syncs();
    let outcome = (|| {
        let mut written = 0usize;
        while written < COMMITS {
            let group = size.min(COMMITS - written);
            let prepared = prepare(&store, written, group)?;
            let started = Instant::now();
            commit_all(&mut store, prepared, 1_700_000_001, 1)
                .map_err(|problem| problem.to_string())?;
            times.push(started.elapsed().as_secs_f64() * 1000.0);
            written += group;
        }
        Ok(())
    })();
    let syncs = store.syncs() - before;
    drop(store);
    std::fs::remove_file(&path).ok();
    outcome.map(|()| Run { syncs, times })
}

/// Builds `group` batches against the store as it stands, which is what the
/// writers waiting for a group are holding when it forms.
fn prepare(store: &Store, from: usize, group: usize) -> Result<Vec<Prepared>, String> {
    let view = store.view().map_err(|problem| problem.to_string())?;
    let mut prepared = Vec::with_capacity(group);
    for writer in 0..group {
        let mut batch = Batch::over(&view).map_err(|problem| problem.to_string())?;
        for document in 0..DOCS {
            let key = format!("{}-{document}", from + writer);
            let text = format!("the quarter ledger for account {key}");
            batch
                .add_keyed(key.as_bytes(), &text)
                .map_err(|problem| problem.to_string())?;
        }
        prepared.push(batch.finish().map_err(|problem| problem.to_string())?);
    }
    Ok(prepared)
}
