//! A store that more than one thread can commit into.
//!
//! Every write path on [`Store`] takes it by exclusive reference, which is the
//! right shape for the file and the wrong one for the machine: a commit is two
//! syncs and a sync is milliseconds, so a store taking small writes from one
//! thread at a time is a store that spends nearly all of its time waiting for a
//! drive that is not being asked for anything. This is the thing that lets the
//! other threads be asking.
//!
//! # How a commit happens here
//!
//! A writer prepares a batch against a view it holds and hands it over. Whoever
//! finds nobody committing takes everything that is waiting, including its own
//! batch, and commits all of it as one commit. Everybody else waits for their
//! answer. Whoever arrives while that is in flight is waiting when it finishes,
//! and one of them leads the next one.
//!
//! So the group is whatever showed up during the commit before it, and nobody
//! waits for company. That is deliberate. A commit is one sync of latency and
//! nothing else, so a leader that lingers hoping for more batches is a leader
//! that made a lone writer slower for nothing, and the window that costs nobody
//! anything is the length of the sync already in flight. Under load that window
//! fills, and the busier the store the larger the groups get, which is the shape
//! this wants: the cost per commit falls exactly when there is a reason for it
//! to.
//!
//! # What a writer prepares against
//!
//! [`view`](Writer::view) hands out the store as it was after the last commit,
//! and hands out the same one to everybody until the next commit replaces it.
//! Nobody touches the store to get it, which matters more than it sounds: a
//! writer that had to take a lock on the store to read it would be waiting for
//! the drive before it could even start analysing, and analysing is the half of
//! ingest worth doing in parallel.
//!
//! That view is one commit behind for anybody who took it before the commit now
//! in flight, which is most writers most of the time. It does not have to be
//! current. A batch is joined onto whatever happened while it was being
//! prepared, and only a compaction refuses it. See [`crate::ingest::commit_all`]
//! for what the join does.
//!
//! # The log
//!
//! Batches committed here are not logged, for the reason
//! [`crate::ingest::commit_all`] gives: the position to free is the furthest any
//! member of the group reached, and a commit that frees less than that leaves
//! records a replay would put back. Durability is the commit itself, which is
//! two syncs and is what it always was. What is lost by a machine stopping is
//! the batches that had not been handed over, which is what is lost by a machine
//! stopping in the middle of building one.

use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use crate::Error;
use crate::file::{Result, Store, Trouble, View};
use crate::ingest::{Prepared, commit_all};

/// A store several threads can commit into.
///
/// Made with [`new`](Self::new), which takes the store, and taken apart with
/// [`into_store`](Self::into_store).
#[derive(Debug)]
pub struct Writer {
    /// The store. Locked only by whoever is committing, and only while the
    /// commit takes.
    store: Mutex<Store>,
    /// Everybody waiting, and the answers of everybody who is not any more.
    hall: Mutex<Hall>,
    /// Where they wait.
    ready: Condvar,
    /// The store as it was after the last commit, for whoever is about to
    /// prepare a batch.
    ///
    /// A view owns its mapping and the descriptors it was made with, so this is
    /// a value and not a borrow of the store. The file only grows and nothing a
    /// view reads is ever written over, so one made two commits ago reads what
    /// it always read.
    seen: Mutex<Arc<View>>,
}

/// The batches waiting and the answers nobody has taken yet.
#[derive(Debug, Default)]
struct Hall {
    /// What has been handed over and not yet committed.
    queue: Vec<Waiting>,
    /// The next ticket to hand out.
    next: u64,
    /// Whether somebody is committing.
    leading: bool,
    /// What happened to each batch, until whoever handed it over takes it.
    answers: HashMap<u64, Answer>,
    /// How many commits have been made here.
    rounds: u64,
    /// How many batches went into them, which against the rounds is the average
    /// group size.
    members: u64,
}

/// One batch handed over and not yet answered.
#[derive(Debug)]
struct Waiting {
    /// What its answer will be filed under.
    ticket: u64,
    /// The batch.
    part: Prepared,
    /// The clock the caller read for the segment descriptor.
    created: u64,
    /// The clock it read for the manifest.
    written: u64,
}

/// What a commit did for one of its members.
#[derive(Debug)]
enum Answer {
    /// It is in the store, at this epoch.
    Went(u64),
    /// It is not, because of this.
    Failed(Trouble),
}

impl Answer {
    /// The answer as the caller's result.
    fn taken(self) -> Result<u64> {
        match self {
            Self::Went(epoch) => Ok(epoch),
            Self::Failed(problem) => Err(problem),
        }
    }
}

impl Writer {
    /// Takes a store and lets several threads commit into it.
    ///
    /// # Errors
    ///
    /// Returns whatever taking the first view of the store fails with, which is
    /// [`Trouble::Io`] if the file cannot be mapped and [`Trouble::Format`] if
    /// the manifest names bytes that are not inside it.
    pub fn new(store: Store) -> Result<Self> {
        let seen = Arc::new(store.view()?);
        Ok(Self {
            store: Mutex::new(store),
            hall: Mutex::new(Hall::default()),
            ready: Condvar::new(),
            seen: Mutex::new(seen),
        })
    }

    /// The store as it was after the last commit.
    ///
    /// This is what to build a batch against. It is shared, so every writer that
    /// asks between two commits gets the same one and none of them pays for a
    /// mapping of their own.
    #[must_use]
    pub fn view(&self) -> Arc<View> {
        Arc::clone(&held(&self.seen))
    }

    /// How many commits have been made here.
    #[must_use]
    pub fn rounds(&self) -> u64 {
        held(&self.hall).rounds
    }

    /// How many batches went into them.
    ///
    /// Against [`rounds`](Self::rounds), this is the average group size, which
    /// is the number that says whether the writers are actually meeting each
    /// other or arriving one at a time.
    #[must_use]
    pub fn members(&self) -> u64 {
        held(&self.hall).members
    }

    /// How many times the store has waited for the drive.
    ///
    /// Takes the store, so a caller that asks while a commit is in flight waits
    /// for it. Ask at the end of a run.
    #[must_use]
    pub fn syncs(&self) -> u64 {
        held(&self.store).syncs()
    }

    /// The store itself, for the things that are not a commit.
    ///
    /// Compaction, verification and reading the manifest all want the store and
    /// not a view of it. Whoever holds this holds up every commit, so hold it
    /// for what it is for and give it back.
    ///
    /// Giving it back is also when the shared view catches up, if what was done
    /// with it moved the store. That is not politeness. A compaction is the one
    /// thing a batch cannot be joined onto, so a compaction that left every
    /// writer preparing against the view before it would be a compaction that
    /// refused the next batch from each of them.
    #[must_use]
    pub fn store(&self) -> Locked<'_> {
        Locked {
            writer: self,
            store: held(&self.store),
        }
    }

    /// Gives the store back.
    #[must_use]
    pub fn into_store(self) -> Store {
        self.store
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Commits a batch, with whatever else is ready at the same time.
    ///
    /// Returns the epoch the commit was given, which every member of the group
    /// shares because the group is one commit.
    ///
    /// The clocks are the caller's own, and a group takes the latest of the ones
    /// its members handed over. A commit is one moment however many batches are
    /// in it, so there is one answer to when it happened, and the newest reading
    /// is the closest to it.
    ///
    /// # Errors
    ///
    /// Returns [`Trouble::Format`] with [`Error::StaleView`] if a compaction has
    /// moved the segments this batch counted positions into, which is the one
    /// thing a commit in between cannot be joined onto. That answer is this
    /// batch's alone: the rest of the group commits without it.
    ///
    /// Anything else that goes wrong is the commit failing, and it fails for
    /// every member of the group, because the group is one commit and nothing in
    /// it landed. Each of them is told what it was.
    pub fn commit(&self, part: Prepared, created: u64, written: u64) -> Result<u64> {
        let ticket = {
            let mut hall = held(&self.hall);
            let ticket = hall.next;
            hall.next = hall.next.saturating_add(1);
            hall.queue.push(Waiting {
                ticket,
                part,
                created,
                written,
            });
            ticket
        };

        loop {
            let mut hall = held(&self.hall);
            if let Some(answer) = hall.answers.remove(&ticket) {
                return answer.taken();
            }
            if hall.leading {
                // Somebody is committing and this batch is either in that commit
                // or in the queue behind it. Either way the next thing to happen
                // is that commit finishing.
                let _held = self
                    .ready
                    .wait(hall)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                continue;
            }

            // Nobody is committing, so this thread does it, and takes everything
            // that is waiting with it. Its own batch is in there: an answer that
            // has been filed is filed before `leading` is cleared, so a queue
            // reached with `leading` false and no answer in it is a queue this
            // batch is still in.
            hall.leading = true;
            let taken = core::mem::take(&mut hall.queue);
            debug_assert!(!taken.is_empty(), "a leader with nothing to commit");
            drop(hall);
            self.lead(taken);
        }
    }

    /// Commits a group and files an answer for every member of it.
    fn lead(&self, taken: Vec<Waiting>) {
        let mut leading = Leading {
            writer: self,
            owed: taken.iter().map(|waiting| waiting.ticket).collect(),
        };
        let answers = self.round(taken);
        let mut hall = held(&self.hall);
        hall.rounds = hall.rounds.saturating_add(1);
        hall.members = hall
            .members
            .saturating_add(answers.len().try_into().unwrap_or(u64::MAX));
        hall.answers.extend(answers);
        drop(hall);
        // Everybody has an answer, so nothing is owed and the guard has only the
        // flag left to clear.
        leading.owed.clear();
    }

    /// Puts a group into the store and says what happened to each of them.
    fn round(&self, taken: Vec<Waiting>) -> Vec<(u64, Answer)> {
        let mut store = held(&self.store);
        let mut answers = Vec::with_capacity(taken.len());
        let mut going = Vec::with_capacity(taken.len());
        let mut created = 0;
        let mut written = 0;
        for mut waiting in taken {
            // A fold moved what this batch counted positions into, and the fold
            // wrote down what it moved, so the batch goes through it. Only a
            // batch from before a fold the store no longer remembers is left,
            // and that is about this batch rather than about the group, so the
            // group goes in without it.
            if !waiting.part.fits(&store)
                && let Some(fold) = store.folded()
            {
                let _ = waiting.part.through(fold);
            }
            if waiting.part.fits(&store) {
                created = created.max(waiting.created);
                written = written.max(waiting.written);
                going.push(waiting);
            } else {
                answers.push((
                    waiting.ticket,
                    Answer::Failed(Trouble::Format(Error::StaleView {
                        read: waiting.part.epoch,
                        committed: store.manifest().epoch,
                    })),
                ));
            }
        }
        if going.is_empty() {
            return answers;
        }

        let mut tickets = Vec::with_capacity(going.len());
        let mut parts = Vec::with_capacity(going.len());
        for waiting in going {
            tickets.push(waiting.ticket);
            parts.push(waiting.part);
        }
        match commit_all(&mut store, parts, created, written) {
            Ok(epoch) => {
                self.catch_up(&store);
                answers.extend(
                    tickets
                        .into_iter()
                        .map(|ticket| (ticket, Answer::Went(epoch))),
                );
            }
            Err(problem) => {
                answers.extend(
                    tickets
                        .into_iter()
                        .map(|ticket| (ticket, Answer::Failed(echo(&problem)))),
                );
            }
        }
        answers
    }

    /// Points the shared view at the store as it is now, if it has moved.
    ///
    /// The epoch is the test, so a store that was only read costs a comparison
    /// and no mapping. A mapping that cannot be made leaves the view before it
    /// in place, which is a view a batch still commits against and is why a
    /// failure here is not worth taking anything down for.
    fn catch_up(&self, store: &Store) {
        let mut seen = held(&self.seen);
        if seen.epoch() == store.manifest().epoch {
            return;
        }
        if let Ok(view) = store.view() {
            *seen = Arc::new(view);
        }
    }
}

/// Takes a lock, and takes it whether or not somebody panicked holding it.
///
/// A panic somewhere in a commit is a bug, and the answer to it is the one
/// [`Leading`] gives: everybody waiting is told the commit did not happen.
/// Refusing to hand out the lock after that would turn a bug into a store that
/// nothing can be written to for as long as the process lives, and the state
/// under these locks is a queue and a mapping rather than anything a half
/// finished commit could have left wrong.
fn held<T>(lock: &Mutex<T>) -> MutexGuard<'_, T> {
    lock.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The store, locked, from [`Writer::store`].
///
/// Reads and writes as the store does. Handing it back is what lets the next
/// commit happen, and is where the shared view catches up with whatever was done
/// through it.
#[derive(Debug)]
pub struct Locked<'a> {
    /// The writer whose view is behind this.
    writer: &'a Writer,
    /// The store.
    store: MutexGuard<'a, Store>,
}

impl core::ops::Deref for Locked<'_> {
    type Target = Store;

    fn deref(&self) -> &Self::Target {
        &self.store
    }
}

impl core::ops::DerefMut for Locked<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.store
    }
}

impl Drop for Locked<'_> {
    fn drop(&mut self) {
        self.writer.catch_up(&self.store);
    }
}

/// Clears the flag that says somebody is committing, whatever happens.
///
/// The unwinding case is the point of it. A commit that panics leaves the
/// batches it took with nobody to answer them and a flag saying somebody still
/// is, which is a store that quietly stops taking writes. So the flag is cleared
/// here and everybody still owed an answer is told, in the one place that runs
/// on the way out either way.
struct Leading<'a> {
    /// The writer whose flag it is.
    writer: &'a Writer,
    /// Whoever has not been answered yet.
    owed: Vec<u64>,
}

impl Drop for Leading<'_> {
    fn drop(&mut self) {
        let mut hall = held(&self.writer.hall);
        for ticket in self.owed.drain(..) {
            hall.answers.insert(
                ticket,
                Answer::Failed(Trouble::Io(io::Error::other(
                    "the commit this batch was in did not finish",
                ))),
            );
        }
        hall.leading = false;
        drop(hall);
        self.writer.ready.notify_all();
    }
}

/// A copy of a failure, for the members of a group that did not raise it.
///
/// Everybody in a group that failed has to be told the same thing, and the
/// failure is one value. A format error copies. An io error does not, so what
/// goes out is its kind and what it said, which is what a caller reads and
/// matches on. The one thing lost is the operating system's own error value
/// underneath, which nothing here looks at.
fn echo(problem: &Trouble) -> Trouble {
    match problem {
        Trouble::Format(error) => Trouble::Format(error.clone()),
        Trouble::Io(error) => Trouble::Io(io::Error::new(error.kind(), error.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::Batch;
    use crate::search::Searcher;

    /// A store identifier, so a file written by a test says what wrote it.
    const STORE: u128 = 0x006b_7572_612d_7772_6974_6572_0000_0001;

    /// A path in the system temporary directory, cleared if a run before this
    /// one left something there.
    fn path(name: &str) -> std::path::PathBuf {
        let path =
            std::env::temp_dir().join(format!("kura-writer-{name}-{}.kura", std::process::id()));
        let _ = std::fs::remove_file(&path);
        path
    }

    /// A writer over a store with nothing in it.
    fn empty(path: &std::path::Path) -> Writer {
        let store = Store::create(path, STORE, 1_700_000_000).expect("a store");
        Writer::new(store).expect("a writer")
    }

    /// Prepares one batch against the writer's view.
    fn prepare(writer: &Writer, documents: &[(&[u8], &str)]) -> Prepared {
        let view = writer.view();
        let mut batch = Batch::over(&view).expect("a batch");
        for (key, text) in documents {
            batch.add_keyed(key, text).expect("a document");
        }
        batch.finish().expect("prepared")
    }

    /// Prepares one batch and commits it.
    fn write(writer: &Writer, documents: &[(&[u8], &str)]) -> u64 {
        let prepared = prepare(writer, documents);
        writer
            .commit(prepared, 1_700_000_001, 1)
            .expect("committed")
    }

    /// How many live documents the store answers `query` with.
    fn count(writer: &Writer, query: &str) -> u64 {
        let store = writer.store();
        let view = store.view().expect("a view");
        let readers = view.readers().expect("readers");
        let searcher = Searcher::over(&readers).expect("a searcher");
        searcher.count(query).expect("counted")
    }

    #[test]
    fn a_writer_on_its_own_commits_the_way_a_store_does() {
        // The floor. One thread handing over one batch at a time is one commit
        // each, and a commit is two syncs, which is what the store did before
        // any of this.
        let path = path("alone");
        let writer = empty(&path);
        write(&writer, &[(b"a", "the first quarter ledger")]);
        write(&writer, &[(b"b", "the second quarter ledger")]);

        assert_eq!(count(&writer, "ledger"), 2);
        assert_eq!(writer.rounds(), 2, "one commit each");
        assert_eq!(writer.members(), 2);
        assert_eq!(writer.syncs(), 4, "two syncs a commit");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn every_document_several_threads_hand_over_is_in_the_store() {
        // The thing the whole module is for. Nothing here says how the batches
        // were grouped, because that is up to how the threads happen to meet,
        // and every grouping has to end with all sixty four documents live.
        const THREADS: usize = 8;
        const EACH: usize = 8;

        let path = path("threads");
        let writer = empty(&path);
        std::thread::scope(|scope| {
            for thread in 0..THREADS {
                let writer = &writer;
                scope.spawn(move || {
                    for round in 0..EACH {
                        let key = format!("{thread}-{round}");
                        let text = format!("document {round} of writer {thread} says ledger");
                        let prepared = prepare(writer, &[(key.as_bytes(), text.as_str())]);
                        writer
                            .commit(prepared, 1_700_000_001, 1)
                            .expect("committed");
                    }
                });
            }
        });

        let wrote = u64::try_from(THREADS * EACH).expect("small");
        assert_eq!(count(&writer, "ledger"), wrote);
        assert_eq!(writer.members(), wrote, "every batch was in a commit");
        assert!(
            writer.rounds() <= wrote,
            "no more commits than batches, and got {}",
            writer.rounds()
        );
        assert_eq!(writer.syncs(), writer.rounds() * 2);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn threads_that_meet_each_other_share_a_commit() {
        // What the sharing is worth, stated as loosely as it can be and still
        // mean something. Eight threads writing at once on any machine with more
        // than one core will find each other, because the commit they arrive
        // during is milliseconds long and preparing a one document batch is not.
        const THREADS: usize = 8;
        const EACH: usize = 16;

        let path = path("meeting");
        let writer = empty(&path);
        std::thread::scope(|scope| {
            for thread in 0..THREADS {
                let writer = &writer;
                scope.spawn(move || {
                    for round in 0..EACH {
                        let key = format!("{thread}-{round}");
                        let prepared = prepare(writer, &[(key.as_bytes(), "shared ledger")]);
                        writer
                            .commit(prepared, 1_700_000_001, 1)
                            .expect("committed");
                    }
                });
            }
        });

        let wrote = u64::try_from(THREADS * EACH).expect("small");
        assert_eq!(count(&writer, "ledger"), wrote);
        assert!(
            writer.rounds() < wrote,
            "somebody joined somebody, and got {} commits for {wrote} batches",
            writer.rounds()
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_key_two_threads_write_at_the_same_time_is_live_once() {
        // The join doing its work under threads. Whichever order they land in,
        // and whether or not they land together, the key has one live document
        // when they are done.
        const THREADS: usize = 6;
        const EACH: usize = 8;

        let path = path("same-key");
        let writer = empty(&path);
        std::thread::scope(|scope| {
            for thread in 0..THREADS {
                let writer = &writer;
                scope.spawn(move || {
                    for round in 0..EACH {
                        let text = format!("revision {round} by {thread} of the ledger");
                        let prepared = prepare(writer, &[(b"the-key", text.as_str())]);
                        writer
                            .commit(prepared, 1_700_000_001, 1)
                            .expect("committed");
                    }
                });
            }
        });

        assert_eq!(count(&writer, "ledger"), 1, "one key, one live document");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_view_a_writer_is_handed_moves_on_after_a_commit() {
        // A batch prepared against a view two commits old still commits, so this
        // is not about correctness. It is about the view being fresh enough that
        // the join has little to do, which is what keeps the leader's work small.
        let path = path("fresh");
        let writer = empty(&path);
        let before = writer.view().epoch();
        write(&writer, &[(b"a", "the first quarter ledger")]);
        let after = writer.view().epoch();

        assert!(
            after > before,
            "the view moved from {before} to {after} across a commit"
        );
        assert_eq!(writer.view().len(), 1, "and it can see the new segment");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_batch_a_compaction_moved_goes_in_with_the_group() {
        // A fold used to be the one thing a leader had to answer a batch about
        // rather than commit it. Now the fold says where everything went and the
        // leader moves the batch through it, so a fold that lands between a
        // thread preparing and a thread handing over costs nothing.
        let path = path("folded");
        let writer = empty(&path);
        write(&writer, &[(b"a", "the first quarter ledger")]);
        write(&writer, &[(b"b", "the second quarter ledger")]);
        let stale = prepare(&writer, &[(b"c", "the third quarter ledger")]);

        writer
            .store()
            .compact(0..2, 1_700_000_002, 2)
            .expect("folded");
        let fresh = prepare(&writer, &[(b"d", "the fourth quarter ledger")]);

        writer
            .commit(stale, 1_700_000_003, 3)
            .expect("the batch the fold moved went in");
        writer.commit(fresh, 1_700_000_003, 3).expect("committed");
        assert_eq!(count(&writer, "ledger"), 4, "both of them went in");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_batch_from_before_a_fold_the_store_forgot_is_refused_on_its_own() {
        // The refusal that is left, and the thing worth checking is that it is
        // one batch's answer rather than the group's. The batch beside it in the
        // queue counted no positions a fold touched, so it goes in.
        let path = path("forgotten");
        let writer = empty(&path);
        write(&writer, &[(b"a", "the first quarter ledger")]);
        write(&writer, &[(b"b", "the second quarter ledger")]);
        let stale = prepare(&writer, &[(b"c", "the third quarter ledger")]);

        {
            let mut store = writer.store();
            store.compact(0..2, 1_700_000_002, 2).expect("folded");
            store.forget_fold();
        }
        let fresh = prepare(&writer, &[(b"d", "the fourth quarter ledger")]);

        let refused = writer.commit(stale, 1_700_000_003, 3);
        assert!(
            matches!(refused, Err(Trouble::Format(Error::StaleView { .. }))),
            "the batch with nothing to move it is refused, and got {refused:?}"
        );
        writer.commit(fresh, 1_700_000_003, 3).expect("committed");
        assert_eq!(count(&writer, "ledger"), 3, "the fresh one went in");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_fold_running_beside_the_threads_leaves_every_document_live() {
        // The case the rest of this was for. A store that folds only when the
        // writing stops is a store that grows a segment per commit while it is
        // busy, so the fold has to be able to land in the middle. Eight threads
        // fill, a ninth folds the bottom of the manifest over and over, and each
        // thread rewrites one key of its own every round so that the deletions a
        // fold has to carry are there to carry.
        use std::sync::atomic::{AtomicUsize, Ordering};

        const THREADS: usize = 8;
        const EACH: usize = 8;

        let path = path("folding");
        let writer = empty(&path);
        let done = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for thread in 0..THREADS {
                let writer = &writer;
                let done = &done;
                scope.spawn(move || {
                    for round in 0..EACH {
                        let key = format!("{thread}-{round}");
                        let carry = format!("{thread}-carry");
                        let text = format!("document {round} of writer {thread} says ledger");
                        let documents = [
                            (key.as_bytes(), text.as_str()),
                            (carry.as_bytes(), text.as_str()),
                        ];
                        loop {
                            let prepared = prepare(writer, &documents);
                            // A batch a second fold overtook is refused, and
                            // the answer to that is the one a caller gives.
                            match writer.commit(prepared, 1_700_000_001, 1) {
                                Ok(_) => break,
                                Err(Trouble::Format(Error::StaleView { .. })) => (),
                                Err(other) => panic!("{other:?}"),
                            }
                        }
                    }
                    done.fetch_add(1, Ordering::Release);
                });
            }
            let writer = &writer;
            let done = &done;
            scope.spawn(move || {
                while done.load(Ordering::Acquire) < THREADS {
                    if writer.view().len() >= 3 {
                        writer
                            .store()
                            .compact(0..2, 1_700_000_002, 2)
                            .expect("folded");
                    }
                    std::thread::yield_now();
                }
            });
        });

        let wrote = u64::try_from(THREADS * (EACH + 1)).expect("small");
        assert_eq!(
            count(&writer, "ledger"),
            wrote,
            "every round of every thread, and one live copy of each carried key"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_compaction_through_the_store_leaves_the_view_current() {
        // Otherwise every writer would be preparing against the segments the
        // fold replaced, and the first batch each of them handed over would be
        // refused for a reason that was over before they started.
        let path = path("caught-up");
        let writer = empty(&path);
        write(&writer, &[(b"a", "the first quarter ledger")]);
        write(&writer, &[(b"b", "the second quarter ledger")]);
        assert_eq!(writer.view().len(), 2);

        writer
            .store()
            .compact(0..2, 1_700_000_002, 2)
            .expect("folded");
        assert_eq!(writer.view().len(), 1, "the view saw the fold");

        write(&writer, &[(b"c", "the third quarter ledger")]);
        assert_eq!(count(&writer, "ledger"), 3);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn reading_the_store_does_not_remap_the_view() {
        // The epoch is the test, so the things that only read are free. This is
        // the same view and not an equal one.
        let path = path("read-only");
        let writer = empty(&path);
        write(&writer, &[(b"a", "the first quarter ledger")]);
        let before = writer.view();
        assert_eq!(writer.store().manifest().segments.len(), 1);
        assert!(
            Arc::ptr_eq(&before, &writer.view()),
            "a read left the view alone"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_leader_that_takes_the_queue_finds_its_own_batch_in_it() {
        // The invariant the loop rests on, checked where it is cheap to check.
        // A thread that arrives with nobody leading has just pushed, and a
        // thread that wakes with nobody leading has either been answered or is
        // still in the queue, because answers are filed before the flag clears.
        let path = path("invariant");
        let writer = empty(&path);
        {
            let hall = held(&writer.hall);
            assert!(!hall.leading, "nobody is committing an empty store");
            assert!(hall.queue.is_empty());
        }
        write(&writer, &[(b"a", "the first quarter ledger")]);
        let hall = held(&writer.hall);
        assert!(!hall.leading, "and nobody is after the commit");
        assert!(hall.queue.is_empty(), "the leader took everything");
        assert!(hall.answers.is_empty(), "and the answer was taken");
        drop(hall);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_copied_failure_says_what_the_first_one_said() {
        // Everybody in a group that failed is told the same thing, and this is
        // what they are told.
        let format = Trouble::Format(Error::StaleView {
            read: 4,
            committed: 7,
        });
        let Trouble::Format(copied) = echo(&format) else {
            panic!("a format error copies as one");
        };
        assert_eq!(
            copied,
            Error::StaleView {
                read: 4,
                committed: 7
            }
        );

        let broken = Trouble::Io(io::Error::new(io::ErrorKind::PermissionDenied, "no"));
        let Trouble::Io(copied) = echo(&broken) else {
            panic!("an io error copies as one");
        };
        assert_eq!(copied.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(copied.to_string(), "no");
    }
}
