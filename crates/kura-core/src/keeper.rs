//! Folding a store while other threads are writing into it.
//!
//! Compaction has been something the thread doing the writing stops to do. A
//! tool that indexes a directory folds between batches when the policy says
//! level zero is full, and a run of several threads folds once at the end,
//! which is 151 ms of a 389 ms run on the machine the README measures. Either
//! way the documents behind the fold wait for it.
//!
//! A [`Keeper`] is that work on a thread of its own. It asks the policy what is
//! due against the view the [`Writer`] is handing out, which costs no lock, and
//! takes the store only to fold. The threads filling batches carry on filling
//! them while it does, because preparing a batch is the expensive half of
//! ingest and preparing needs the view rather than the store.
//!
//! What it does hold up is the handing over. A fold takes the store for as long
//! as it runs and nothing commits in that window. That is the honest cost of
//! this shape, and it is a cost the run was paying anyway at the end of itself,
//! moved to where it stops the segment count climbing rather than to where it
//! brings it back down in one jump.
//!
//! The reason it can be done at all is that a batch prepared before a fold is
//! carried through it now rather than refused, so the batches that queued
//! behind the fold go in when it is over.
//!
//! One keeper at a time, per store. Two would be two merged segments in memory
//! for no gain, since the second would be folding what the first is about to
//! replace, and the run of positions the second picked would be the run the
//! first just spliced.

use crate::file::Result;
use crate::policy::{Job, Policy, Pressure};
use crate::writer::Writer;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// How long to wait before asking again when nothing is due.
///
/// Short enough that a run which fills level zero in a few hundred milliseconds
/// is not sitting over the cap for any of it, long enough that a keeper beside
/// an idle store is not a core spinning. There is no signal to wait on instead:
/// the thing that would raise it is a commit, and a keeper woken by every commit
/// would be woken most often exactly when it should be leaving the store alone.
const QUIET: Duration = Duration::from_millis(2);

/// A compactor that runs beside the writers.
///
/// Borrows the writer rather than owning it, so it lives inside a
/// [`std::thread::scope`] with the threads it is keeping up with.
#[derive(Debug)]
pub struct Keeper<'a> {
    /// The writer whose store it folds.
    writer: &'a Writer,
    /// What it asks what is due.
    policy: Policy,
    /// Set by [`stop`](Keeper::stop), read at the top of every round.
    stop: AtomicBool,
    /// How many folds it has committed.
    folds: AtomicU64,
    /// How many segments those folds replaced.
    segments: AtomicU64,
    /// How many documents came out of them.
    documents: AtomicU64,
    /// How long it spent folding, in microseconds, which is small enough to
    /// hold a fold of every store this will ever see and large enough not to
    /// round a short one to nothing.
    took: AtomicU64,
    /// How many rounds found the store over the cap rather than merely due,
    /// which is the number that says the keeper is not keeping up.
    behind: AtomicU64,
    /// The most segments it ever saw at once.
    highest: AtomicU64,
}

impl<'a> Keeper<'a> {
    /// A keeper with the default policy.
    #[must_use]
    pub fn new(writer: &'a Writer) -> Self {
        Self::with_policy(writer, Policy::default())
    }

    /// A keeper that folds by the policy it is given.
    #[must_use]
    pub fn with_policy(writer: &'a Writer, policy: Policy) -> Self {
        Self {
            writer,
            policy,
            stop: AtomicBool::new(false),
            folds: AtomicU64::new(0),
            segments: AtomicU64::new(0),
            documents: AtomicU64::new(0),
            took: AtomicU64::new(0),
            behind: AtomicU64::new(0),
            highest: AtomicU64::new(0),
        }
    }

    /// Folds what is due until told to stop.
    ///
    /// Run this on a thread of its own. It returns when [`stop`](Keeper::stop)
    /// has been called and the fold it was in, if it was in one, is committed.
    /// A fold is never left half done, because nothing a fold writes is reachable
    /// until the commit at the end of it and the segments it was folding are all
    /// still where they were.
    ///
    /// `now` is asked for a timestamp per fold rather than once, so that a long
    /// run does not stamp every segment it folds with the moment it started.
    ///
    /// # Errors
    ///
    /// Whatever the fold returns. A keeper that fails stops, and the store is at
    /// the state it was at, since a compaction commits all of itself or none.
    pub fn run(&self, now: impl Fn() -> u64) -> Result<()> {
        while !self.stop.load(Ordering::Acquire) {
            let Some(job) = self.due()? else {
                std::thread::sleep(QUIET);
                continue;
            };
            self.fold(&job, now())?;
        }
        Ok(())
    }

    /// Tells it to stop, after the fold it is in.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
    }

    /// What it has done so far.
    #[must_use]
    pub fn tally(&self) -> Tally {
        Tally {
            folds: self.folds.load(Ordering::Relaxed),
            segments: self.segments.load(Ordering::Relaxed),
            documents: self.documents.load(Ordering::Relaxed),
            took: Duration::from_micros(self.took.load(Ordering::Relaxed)),
            behind: self.behind.load(Ordering::Relaxed),
            highest: usize::try_from(self.highest.load(Ordering::Relaxed)).unwrap_or(usize::MAX),
        }
    }

    /// Asks the policy what is due, off the lock.
    ///
    /// The view is the one the writer hands to everybody, so this costs a clone
    /// of an [`std::sync::Arc`] and a walk of the deletion sets. It may be a
    /// commit or two behind, which is the right amount of wrong for a question
    /// about whether a level has grown too large.
    fn due(&self) -> Result<Option<Job>> {
        let view = self.writer.view();
        self.highest.fetch_max(view.len() as u64, Ordering::Relaxed);
        let pressure = self.policy.pressure(view.described());
        if pressure.is_clear() {
            return Ok(None);
        }
        if pressure == Pressure::Stalled {
            self.behind.fetch_add(1, Ordering::Relaxed);
        }
        let mut deleted = Vec::with_capacity(view.len());
        for at in 0..view.len() {
            deleted.push(view.deleted(at)?.map_or(0, |set| set.len() as u64));
        }
        Ok(self.policy.choose_with(view.described(), &deleted))
    }

    /// Takes the store and folds the run, if it is still there.
    ///
    /// The run was picked against a view, and between then and here the only
    /// thing that can have happened to the manifest is commits, which append at
    /// the end and rewrite deletion sets in place. Neither moves a position
    /// below it, so a run that fitted the view fits the store unless something
    /// else folded, and something else folding is the case this module says not
    /// to create.
    fn fold(&self, job: &Job, now: u64) -> Result<()> {
        let mut store = self.writer.store();
        if job.run.end > store.manifest().segments.len() {
            return Ok(());
        }
        let folding = Instant::now();
        let done = store.compact_into(job.run.clone(), Some(job.into), now, now)?;
        let took = folding.elapsed();
        self.folds.fetch_add(1, Ordering::Relaxed);
        self.segments
            .fetch_add(done.folded as u64, Ordering::Relaxed);
        self.documents
            .fetch_add(u64::from(done.documents), Ordering::Relaxed);
        self.took.fetch_add(
            took.as_micros().try_into().unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        Ok(())
    }
}

/// What a keeper did, for a run that wants to say so.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Tally {
    /// How many folds it committed.
    pub folds: u64,
    /// How many segments those folds replaced.
    pub segments: u64,
    /// How many documents came out of them.
    pub documents: u64,
    /// How long it spent inside `compact_into`, which is the time nothing else
    /// could commit.
    pub took: Duration,
    /// How many times it found the store over the cap rather than merely due.
    pub behind: u64,
    /// The most segments it ever saw at once, which is what says whether the
    /// count sat near the cap over the run or climbed and was brought back.
    pub highest: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file::{Store, Trouble};
    use crate::ingest::Batch;
    use crate::search::Searcher;

    /// A store identifier, so a file written by a test says what wrote it.
    const STORE: u128 = 0x006b_7572_612d_6b65_6570_6572_0000_0001;

    /// A path in the system temporary directory, cleared if a run before this
    /// one left something there.
    fn path(name: &str) -> std::path::PathBuf {
        let path =
            std::env::temp_dir().join(format!("kura-keeper-{name}-{}.kura", std::process::id()));
        let _ = std::fs::remove_file(&path);
        path
    }

    /// A writer over a store with nothing in it.
    fn empty(path: &std::path::Path) -> Writer {
        let store = Store::create(path, STORE, 1_700_000_000).expect("a store");
        Writer::new(store).expect("a writer")
    }

    /// Prepares one batch and commits it, retrying whatever a fold overtook.
    fn write(writer: &Writer, key: &[u8], text: &str) {
        loop {
            let view = writer.view();
            let mut batch = Batch::over(&view).expect("a batch");
            batch.add_keyed(key, text).expect("a document");
            let prepared = batch.finish().expect("prepared");
            match writer.commit(prepared, 1_700_000_001, 1) {
                Ok(_) => return,
                Err(Trouble::Format(crate::error::Error::StaleView { .. })) => (),
                Err(other) => panic!("{other:?}"),
            }
        }
    }

    /// How many live documents the store answers `query` with.
    fn count(writer: &Writer) -> u64 {
        let store = writer.store();
        let view = store.view().expect("a view");
        let readers = view.readers().expect("readers");
        let searcher = Searcher::over(&readers).expect("a searcher");
        searcher.count("ledger").expect("counted")
    }

    #[test]
    fn a_keeper_beside_the_writers_holds_the_segment_count_down() {
        // The thing the module is for. Sixty four commits into a store with a
        // level zero cap of eight leaves sixty four segments if nothing folds,
        // and the keeper is what makes that not happen while the writing is
        // still going on.
        const EACH: usize = 64;

        let path = path("beside");
        let writer = empty(&path);
        let keeper = Keeper::new(&writer);
        let mut highest = 0;
        std::thread::scope(|scope| {
            let keeping = scope.spawn(|| keeper.run(|| 1_700_000_002));
            for round in 0..EACH {
                let key = format!("doc-{round}");
                write(&writer, key.as_bytes(), "the quarterly ledger");
                highest = highest.max(writer.view().len());
            }
            keeper.stop();
            keeping.join().expect("the keeper thread").expect("folded");
        });

        let tally = keeper.tally();
        assert!(tally.folds > 0, "it folded something");
        assert!(
            highest < EACH,
            "the count stayed under what no folding would leave, and got {highest}"
        );
        assert_eq!(
            count(&writer),
            u64::try_from(EACH).expect("small"),
            "every document is live once"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_keeper_that_never_had_anything_to_do_folds_nothing() {
        // A store under the cap is a store to leave alone, and the check is that
        // a keeper beside one is not quietly rewriting it.
        let path = path("idle");
        let writer = empty(&path);
        let keeper = Keeper::new(&writer);
        std::thread::scope(|scope| {
            let keeping = scope.spawn(|| keeper.run(|| 1_700_000_002));
            write(&writer, b"a", "the quarterly ledger");
            std::thread::sleep(QUIET * 4);
            keeper.stop();
            keeping.join().expect("the keeper thread").expect("nothing");
        });

        assert_eq!(keeper.tally().folds, 0, "nothing was due");
        assert_eq!(writer.view().len(), 1, "one commit, one segment");
        let _ = std::fs::remove_file(&path);
    }
}
