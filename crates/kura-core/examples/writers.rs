//! What several threads writing into one store costs on the machine it is run
//! on.
//!
//! Run it with `cargo run --release --example writers -- <directory>`, or with
//! no argument to use the temporary directory. It writes a store there, has one
//! thread and then two and then four and so on hand small batches to it as fast
//! as they can, and reports what each of them managed.
//!
//! The example beside this one, `group`, forms the groups by hand: it prepares
//! four batches and commits four batches, so the group size is whatever the run
//! was told to use. This is the same question asked the way a program actually
//! asks it. Nobody here decides to be in a group. Every thread prepares a batch
//! and hands it over, and the ones that arrive while a commit is in flight are
//! waiting together when it finishes, so they go in together. The group size is
//! an outcome, and it is reported rather than set.
//!
//! What to look at is syncs per thousand documents against the average group
//! size. One thread pays two syncs a batch and there is nothing to be done about
//! that. What the runs above one show is a group of about half the threads, and
//! that is not an accident of the machine: the threads a commit releases spend
//! the next commit preparing and queue for the one after it, so the writers fall
//! into two cohorts that take turns leading. Half the threads is therefore the
//! number to expect, and the cost per document falls with it.
//!
//! The two thread run is the one to read carefully. Half of two is one, so the
//! two of them alternate and neither ever joins the other, which means each pays
//! its own commit and waits behind the other's. That is the same throughput as
//! one thread for twice the latency, and it is the honest floor of this design.
//! It is worth what it costs from four threads on.
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
use kura_core::ingest::Batch;
use kura_core::writer::Writer;

/// How many batches a run hands over, however many threads are handing them.
///
/// Fixed, so the store ends up holding the same segments every time and the only
/// thing the thread count changes is how many commits it took to put them there.
const BATCHES: usize = 512;

/// How many documents in each of them.
///
/// Small, because this is about a store taking writes as they arrive rather than
/// a corpus being loaded. A batch large enough to be worth a commit of its own
/// does not need any of this.
const DOCS: usize = 4;

/// The thread counts to measure, each of which divides `BATCHES`.
const THREADS: [usize; 5] = [1, 2, 4, 8, 16];

/// A store identifier, so a file written by this says what wrote it.
const STORE: u128 = 0x006b_7572_612d_7772_6974_6572_0000_0001;

fn main() {
    let directory = std::env::args()
        .nth(1)
        .map_or_else(std::env::temp_dir, std::path::PathBuf::from);

    println!("directory     {}", directory.display());
    println!(
        "batches       {BATCHES} of {DOCS} documents each, {} in all",
        BATCHES * DOCS
    );
    println!();
    println!(
        "{:<8} {:>10} {:>8} {:>10} {:>7} {:>11} {:>11} {:>11}",
        "threads", "docs/s", "syncs", "syncs/1k", "group", "median", "p99", "worst"
    );

    for threads in THREADS {
        match measure(&directory, threads) {
            Ok(run) => run.tell(threads),
            Err(problem) => println!("{threads:<8} {problem}"),
        }
    }
}

/// What one run came to.
struct Run {
    /// How many times it waited for the drive.
    syncs: u64,
    /// How many commits it took.
    rounds: u64,
    /// How many batches went into them.
    members: u64,
    /// How long the whole run took, in seconds.
    seconds: f64,
    /// How long each thread waited for each of its batches, in milliseconds.
    times: Vec<f64>,
}

impl Run {
    /// Prints it.
    fn tell(&self, threads: usize) {
        let mut sorted = self.times.clone();
        sorted.sort_by(f64::total_cmp);
        let documents = (BATCHES * DOCS) as f64;
        println!(
            "{:<8} {:>10.0} {:>8} {:>10.1} {:>7.1} {:>9.3}ms {:>9.3}ms {:>9.3}ms",
            threads,
            documents / self.seconds.max(f64::MIN_POSITIVE),
            self.syncs,
            self.syncs as f64 * 1000.0 / documents,
            self.members as f64 / self.rounds.max(1) as f64,
            sorted[sorted.len() / 2],
            sorted[sorted.len() * 99 / 100],
            sorted[sorted.len() - 1],
        );
    }
}

/// Runs `threads` writers against a store of their own.
fn measure(directory: &std::path::Path, threads: usize) -> Result<Run, String> {
    let path = directory.join(format!("kura-writers-{threads}.kura"));
    std::fs::remove_file(&path).ok();
    let store =
        Store::create(&path, STORE, 1_700_000_000).map_err(|problem| problem.to_string())?;
    let writer = Writer::new(store).map_err(|problem| problem.to_string())?;

    let started = Instant::now();
    let times = std::thread::scope(|scope| {
        let running: Vec<_> = (0..threads)
            .map(|thread| {
                let writer = &writer;
                scope.spawn(move || run(writer, thread, BATCHES / threads))
            })
            .collect();
        running
            .into_iter()
            .map(|handle| {
                handle
                    .join()
                    .unwrap_or_else(|_| Err("a writer stopped".into()))
            })
            .collect::<Result<Vec<_>, String>>()
    })?;
    let seconds = started.elapsed().as_secs_f64();

    let run = Run {
        syncs: writer.syncs(),
        rounds: writer.rounds(),
        members: writer.members(),
        seconds,
        times: times.into_iter().flatten().collect(),
    };
    drop(writer);
    std::fs::remove_file(&path).ok();
    Ok(run)
}

/// One writer, preparing and handing over `each` batches in turn.
fn run(writer: &Writer, thread: usize, each: usize) -> Result<Vec<f64>, String> {
    let mut times = Vec::with_capacity(each);
    for round in 0..each {
        // Against the view the writer holds, which is the store as it was after
        // the last commit and may be several commits old by the time this is
        // handed over. That is the point of it.
        let view = writer.view();
        let mut batch = Batch::over(&view).map_err(|problem| problem.to_string())?;
        for document in 0..DOCS {
            let key = format!("{thread}-{round}-{document}");
            let text = format!("the quarter ledger for account {key}");
            batch
                .add_keyed(key.as_bytes(), &text)
                .map_err(|problem| problem.to_string())?;
        }
        let prepared = batch.finish().map_err(|problem| problem.to_string())?;
        drop(view);

        let started = Instant::now();
        writer
            .commit(prepared, 1_700_000_001, 1)
            .map_err(|problem| problem.to_string())?;
        times.push(started.elapsed().as_secs_f64() * 1000.0);
    }
    Ok(times)
}
