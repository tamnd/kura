//! Getting back the space a fold left behind, by rewriting the file.
//!
//! A fold appends the segment it made and leaves the ones it replaced where
//! they are. That is the right thing for a commit to do, because a query that
//! started before it is still reading those segments through a mapping and a
//! set of offsets it took at the time, and taking their bytes away underneath
//! it would be the one bug in this engine nobody could debug. It is also the
//! wrong thing for a store to keep doing, because the file only ever grows.
//! The ladder in `examples/layers` folds sixteen megabytes of documents down
//! from twenty two segments to one inside a file of three hundred, so nineteen
//! bytes in twenty are bytes nothing will read again.
//!
//! There are two ways to take them back and this is the easy one.
//!
//! Doing it to a store that is open means knowing that no view is reading a
//! range, which means the store has to know about the views, which it does not.
//! Doing it offline means none of that, because there are no views: the whole
//! file is written again beside itself, holding only what the committed
//! manifest names, and the caller puts the new one in the old one's place.
//!
//! # What is carried across and what is not
//!
//! Everything the manifest names goes over whole, in the order it names them,
//! with each segment's own descriptor carried with it. Two fields cannot be:
//! where the segment is and where its tombstones are, since the whole point is
//! that both move. Every other field is the one that was committed, including
//! the level a compaction policy reads and the generation a reader tells its
//! deletions apart by, so a store that has been through this is a store that
//! makes the same decisions afterwards as it would have made before.
//!
//! # The log is the one number worth choosing again
//!
//! A store's log region is fixed when the store is made, because the segments
//! start where it ends, so this is the only place it can change at all. That
//! makes it worth offering: a log is often most of a small store, since a run
//! picks a size for the largest corpus it might see and then indexes a corpus
//! that fits in a fraction of it. [`rewrite`] carries the source's log across
//! and [`rewrite_with_log`] takes one, which is how an archival copy is made.
//!
//! Shrinking it is safe here and nowhere else. A rewrite is refused unless the
//! log is already consumed, so there is nothing in the region to lose, and the
//! new store starts its own ring empty at the size it was given.
//!
//! The tombstones are decoded and written again rather than copied byte for
//! byte. It costs a decode per segment that has any, and it buys the check that
//! a set naming a document its segment does not have is refused here, on a file
//! nothing is pointing at yet, rather than carried into the new one.
//!
//! Nothing outside the manifest goes over at all, which is the point.
//!
//! # What it will not do
//!
//! It refuses a store whose log still holds records the committed manifest has
//! not consumed. Those records are documents somebody was promised and they
//! name segments by position, so a file whose segments have moved is a file
//! they no longer describe. Recovering first and then reclaiming is the answer
//! and the error says so.
//!
//! # The order that matters
//!
//! Nothing is renamed and nothing is deleted here. The rename is the step that
//! destroys something, and it belongs to whoever decided to do this rather than
//! to a library function. A run that dies halfway leaves a partial file that
//! nothing points at and a store that has not been touched, which is the same
//! property the segment region has had all along.

use std::path::Path;

use crate::error::Error;
use crate::file::{Result, Store, Trouble};
use crate::manifest::Manifest;

/// What a rewrite came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reclaimed {
    /// How many segments went across.
    pub segments: usize,
    /// How many documents they hold between them, tombstoned ones included.
    pub documents: u64,
    /// How long the file was.
    pub was: u64,
    /// How long the one that came out is.
    pub now: u64,
}

impl Reclaimed {
    /// How many bytes the rewrite gave back, or nothing if it gave none.
    ///
    /// Saturating rather than signed, because a store with nothing stranded in
    /// it comes out the same size or a page longer, and reporting that as
    /// negative savings would invite somebody to add it up.
    #[must_use]
    pub const fn saved(&self) -> u64 {
        self.was.saturating_sub(self.now)
    }
}

/// Rewrites a store into a new file holding only what its manifest names.
///
/// `from` is opened and not modified. `into` must not exist, and what is left
/// there when this returns is a store carrying the same identifier, creation
/// time and log length as the one it came from, holding the same documents in
/// the same order with the same deletions applied.
///
/// The caller does the rename. See the module documentation for why.
///
/// # Errors
///
/// As [`rewrite_with_log`].
pub fn rewrite(from: &Path, into: &Path, written: u64) -> Result<Reclaimed> {
    rewrite_with_log(from, into, written, None)
}

/// As [`rewrite`], with the log region a length of the caller's choosing.
///
/// `log` of `None` carries the source's length across, which is what [`rewrite`]
/// asks for. `Some` gives the new store that length instead, rounded up to a
/// page the way any other store creation rounds it.
///
/// This is the only way a store's log length changes, and it can go either way.
/// A smaller one is the reason to reach for this, since a log picked for the
/// largest corpus a run might see is most of the file when the corpus that
/// arrived was small.
///
/// # Errors
///
/// Returns [`Trouble::Format`] with [`Error::LogNotConsumed`] if the source
/// still has log records no commit has consumed, and with whatever the source
/// is wrong about if it does not open or a tombstone set does not belong to the
/// segment it is filed under. Returns [`Trouble::Io`] if `into` already exists,
/// cannot be created, or a write or a sync fails.
pub fn rewrite_with_log(
    from: &Path,
    into: &Path,
    written: u64,
    log: Option<u64>,
) -> Result<Reclaimed> {
    let source = Store::open(from)?;
    let committed = source.manifest();
    if committed.wal_head != committed.wal_tail {
        return Err(Trouble::Format(Error::LogNotConsumed {
            head: committed.wal_head,
            tail: committed.wal_tail,
        }));
    }

    let block = source.superblock();
    // The source's unless somebody said otherwise. Not the default, because a
    // store that came back with a log nobody chose would be a store whose size
    // changed for a reason not in the command that ran.
    let wal_len = log.unwrap_or(block.wal_len);
    let mut fresh = Store::create_with_log(into, block.store, block.created, wal_len)?;
    // Held across the loop rather than taken per segment, because it is one
    // mapping of the whole source file and taking it again for each segment
    // would be a mapping per segment of a file that is not changing.
    let view = source.view()?;

    let mut segments = Vec::with_capacity(view.len());
    for at in 0..view.len() {
        let was = &committed.segments[at];
        let bytes = view.bytes(at).ok_or(Error::MissingSection { kind: 0 })?;
        let mut now = fresh.append_segment_with(was.docs, was.created, |appending| {
            use std::io::Write as _;
            appending.write_all(bytes)
        })?;
        // Carried rather than recomputed. A level says how many folds a segment
        // has been through and a first live ordinal says where a compaction may
        // start reading, and both of those are true of the bytes rather than of
        // where they sit, so a rewrite that worked them out again would be
        // guessing at something it was told.
        now.first_live = was.first_live;
        now.level = was.level;
        now.flags = was.flags;
        now.footer = was.footer;
        if let Some(deleted) = view.deleted(at)? {
            now = fresh.append_tombstones(&now, &deleted)?;
        }
        // After the tombstones, because writing a set moves the generation on by
        // one and this is not a new set. It is the committed one in a different
        // place, and a reader that told the two apart by generation would be
        // right to be confused by a store that came back a generation ahead of
        // the deletions it holds.
        now.generation = was.generation;
        segments.push(now);
    }

    let documents = committed.total;
    let manifest = Manifest {
        live: committed.live,
        total: committed.total,
        terms: committed.terms,
        flags: committed.flags,
        segments,
        ..Manifest::default()
    };
    let count = manifest.segments.len();
    fresh.commit(manifest, written)?;

    Ok(Reclaimed {
        segments: count,
        documents,
        was: std::fs::metadata(from)?.len(),
        now: std::fs::metadata(into)?.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::bitmap::Bitmap;
    use crate::search::Searcher;

    /// A store identifier, so a file written by these tests says so.
    const STORE: u128 = 0x006b_7572_612d_7265_636c_6169_6d00_0001;

    const WORDS: [&str; 8] = [
        "alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta", "theta",
    ];

    fn path(name: &str) -> std::path::PathBuf {
        let directory = std::env::temp_dir().join(format!("kura-reclaim-{}", std::process::id()));
        std::fs::create_dir_all(&directory).expect("a temporary directory");
        let path = directory.join(format!("{name}.kura"));
        let _ = std::fs::remove_file(&path);
        path
    }

    fn text(n: usize) -> String {
        let mut out = String::new();
        for at in 0..=(n % 5) {
            out.push_str(WORDS[(n + at) % WORDS.len()]);
            out.push(' ');
        }
        out.push_str(WORDS[n % WORDS.len()]);
        out
    }

    /// The log every source here is made with.
    ///
    /// Not the default, so that a rewrite which quietly used the default rather
    /// than the source's would be caught rather than agreed with.
    const LOG: u64 = 8 << 20;

    /// A store of one segment per entry, each entry saying how many documents.
    fn stored(path: &Path, parts: &[usize]) -> Store {
        let mut store = Store::create_with_log(path, STORE, 1_700_000_000, LOG).expect("a store");
        let mut manifest = store.manifest().clone();
        let mut from = 0;
        for (n, &count) in parts.iter().enumerate() {
            let mut writer = crate::index::Writer::new();
            for at in from..from + count {
                writer.add(&text(at)).expect("a document");
            }
            let docs = u32::try_from(writer.len()).expect("a small segment");
            let bytes = writer.finish().expect("a segment");
            let described = store
                .append_segment(&bytes, docs, 1_700_000_000 + n as u64)
                .expect("appended");
            manifest.segments.push(described);
            manifest.total += u64::from(described.docs);
            manifest.live += u64::from(described.docs);
            from += count;
        }
        store.commit(manifest, 1_700_000_001).expect("committed");
        store
    }

    /// What a query gets back, named by segment and ordinal so that a store
    /// split one way and the same store split another are comparable.
    fn hits(store: &Store, query: &str) -> Vec<(usize, u32)> {
        let view = store.view().expect("a view");
        let readers = view.readers().expect("readers");
        let searcher = Searcher::over(readers).expect("a searcher");
        searcher
            .search(query, 50)
            .expect("searched")
            .into_iter()
            .map(|hit| searcher.locate(hit.doc).expect("a segment"))
            .collect()
    }

    #[test]
    fn a_folded_store_comes_back_at_about_the_size_of_what_it_holds() {
        let from = path("folded-source");
        let into = path("folded-rewritten");
        let mut store = stored(&from, &[40, 40, 40, 40, 40, 40]);
        let held = store.manifest().segments.len();
        store.compact(0..held, 1_700_000_002, 1).expect("a fold");
        let live: u64 = store
            .manifest()
            .segments
            .iter()
            .map(|segment| segment.len)
            .sum();
        drop(store);

        let done = rewrite(&from, &into, 1_700_000_003).expect("a rewrite");
        assert_eq!(done.segments, 1);
        // The stranded segments are gone, so what is left is the front of the
        // file, the log region and the one segment the fold made. The front and
        // the log are the same in both files, so the difference between them is
        // the space the fold stranded and nothing else.
        let front = Store::open(&into)
            .expect("a store")
            .superblock()
            .segments_offset;
        assert!(
            done.now < front + live + 4096,
            "the rewrite left {} bytes for {live} of segment past {front}",
            done.now
        );
        assert!(
            done.saved() > 0,
            "the rewrite saved nothing out of {} bytes",
            done.was
        );
        let _ = std::fs::remove_file(&from);
        let _ = std::fs::remove_file(&into);
    }

    #[test]
    fn the_store_that_comes_out_answers_what_the_one_that_went_in_answered() {
        let from = path("answers-source");
        let into = path("answers-rewritten");
        let store = stored(&from, &[7, 9, 11]);
        let before: Vec<_> = WORDS.iter().map(|word| hits(&store, word)).collect();
        drop(store);

        let done = rewrite(&from, &into, 1_700_000_003).expect("a rewrite");
        assert_eq!(done.segments, 3);
        assert_eq!(done.documents, 27);

        let store = Store::open(&into).expect("a store");
        let after: Vec<_> = WORDS.iter().map(|word| hits(&store, word)).collect();
        assert_eq!(before, after);
        drop(store);
        let _ = std::fs::remove_file(&from);
        let _ = std::fs::remove_file(&into);
    }

    #[test]
    fn deletions_survive_the_rewrite() {
        let from = path("deleted-source");
        let into = path("deleted-rewritten");
        let mut store = stored(&from, &[8, 8]);
        // Twice, and the second set is the first with one more in it, because a
        // set is the whole answer for its segment rather than a change to it.
        // Twice rather than once so that the generation on the committed
        // descriptor is two, which is a number a rewrite that wrote the set again
        // and took whatever came back could not produce.
        let mut deleted = Bitmap::new();
        for ordinal in [0, 2] {
            deleted.insert(ordinal);
        }
        store.delete(1, &deleted, 1_700_000_002).expect("deleted");
        deleted.insert(5);
        store.delete(1, &deleted, 1_700_000_003).expect("deleted");
        let was = store.manifest().clone();
        assert_eq!(
            was.segments[1].generation, 2,
            "the test did not manage to move the generation twice"
        );
        let before: Vec<_> = WORDS.iter().map(|word| hits(&store, word)).collect();
        drop(store);

        rewrite(&from, &into, 1_700_000_004).expect("a rewrite");
        let store = Store::open(&into).expect("a store");
        let now = store.manifest();
        assert_eq!(now.live, was.live);
        assert_eq!(now.total, was.total);
        assert_eq!(now.segments[1].generation, was.segments[1].generation);
        assert_eq!(
            now.segments[1].tombstones_len,
            was.segments[1].tombstones_len
        );
        assert_ne!(now.segments[1].tombstones_offset, 0);
        // The hits themselves, because a live count that agrees and a set of
        // deletions that does not is exactly the bug this is here to catch.
        let after: Vec<_> = WORDS.iter().map(|word| hits(&store, word)).collect();
        assert_eq!(before, after);
        drop(store);
        let _ = std::fs::remove_file(&from);
        let _ = std::fs::remove_file(&into);
    }

    #[test]
    fn a_segment_keeps_the_level_a_fold_gave_it() {
        let from = path("levels-source");
        let into = path("levels-rewritten");
        let mut store = stored(&from, &[5, 5, 5]);
        store.compact(0..2, 1_700_000_002, 4).expect("a fold");
        let was: Vec<u32> = store
            .manifest()
            .segments
            .iter()
            .map(|segment| segment.level)
            .collect();
        drop(store);

        rewrite(&from, &into, 1_700_000_003).expect("a rewrite");
        let store = Store::open(&into).expect("a store");
        let now: Vec<u32> = store
            .manifest()
            .segments
            .iter()
            .map(|segment| segment.level)
            .collect();
        assert_eq!(was, now, "a rewrite worked the levels out again");
        drop(store);
        let _ = std::fs::remove_file(&from);
        let _ = std::fs::remove_file(&into);
    }

    #[test]
    fn a_rewrite_carries_the_log_across_unless_it_is_given_one() {
        let from = path("log-kept-source");
        let kept = path("log-kept");
        let cut = path("log-cut");
        let store = stored(&from, &[6]);
        let was = store.superblock().wal_len;
        assert_eq!(was, LOG, "the fixture did not get the log it asked for");
        assert_ne!(
            was,
            crate::manifest::DEFAULT_WAL_LEN,
            "the fixture's log is the default, so this test cannot tell the two apart"
        );
        drop(store);

        rewrite(&from, &kept, 1_700_000_003).expect("a rewrite");
        assert_eq!(
            Store::open(&kept).expect("a store").superblock().wal_len,
            was,
            "a rewrite asked for nothing changed the log anyway"
        );

        rewrite_with_log(&from, &cut, 1_700_000_003, Some(64 << 10)).expect("a rewrite");
        let smaller = Store::open(&cut).expect("a store");
        assert_eq!(smaller.superblock().wal_len, 64 << 10);
        // The whole point of asking, so it is asserted rather than left to be
        // read off the two lengths by somebody trusting the arithmetic.
        assert!(
            smaller.superblock().segments_offset < was,
            "the segments still start past where the old log ended"
        );
        // And it is still a store, since a log is a region the segments are
        // placed after and getting that arithmetic wrong would put them
        // somewhere the manifest does not say.
        let before: Vec<_> = {
            let one = Store::open(&kept).expect("a store");
            WORDS.iter().map(|word| hits(&one, word)).collect()
        };
        let after: Vec<_> = WORDS.iter().map(|word| hits(&smaller, word)).collect();
        assert_eq!(before, after);
        drop(smaller);
        for path in [&from, &kept, &cut] {
            let _ = std::fs::remove_file(path);
        }
    }

    #[test]
    fn a_log_a_rewrite_shrank_is_a_ring_the_new_store_can_still_write_to() {
        let from = path("log-usable-source");
        let into = path("log-usable");
        let store = stored(&from, &[6]);
        drop(store);

        rewrite_with_log(&from, &into, 1_700_000_003, Some(1 << 20)).expect("a rewrite");
        let mut store = Store::open(&into).expect("a store");
        assert!(
            store.log().is_empty(),
            "the new ring came back with records"
        );
        store
            .append(1, b"a record the smaller ring has to hold")
            .expect("appended");
        store.sync().expect("synced");
        assert!(!store.log().is_empty());
        drop(store);
        let _ = std::fs::remove_file(&from);
        let _ = std::fs::remove_file(&into);
    }

    #[test]
    fn a_store_with_records_still_in_its_log_is_refused() {
        let from = path("log-source");
        let into = path("log-rewritten");
        let mut store = stored(&from, &[4]);
        store
            .append(1, b"a record nothing has consumed")
            .expect("appended");
        store.sync().expect("synced");
        let manifest = store.manifest().clone();
        store.commit(manifest, 1_700_000_002).expect("committed");
        assert_ne!(
            store.manifest().wal_head,
            store.manifest().wal_tail,
            "the test did not manage to leave anything in the log"
        );
        drop(store);

        let refused = rewrite(&from, &into, 1_700_000_003);
        assert!(
            matches!(refused, Err(Trouble::Format(Error::LogNotConsumed { .. }))),
            "a store with an unconsumed log was rewritten anyway"
        );
        assert!(
            !into.exists(),
            "the refusal left a file behind at {}",
            into.display()
        );
        let _ = std::fs::remove_file(&from);
        let _ = std::fs::remove_file(&into);
    }

    #[test]
    fn a_rewrite_will_not_write_over_a_store_that_is_already_there() {
        let from = path("clobber-source");
        let into = path("clobber-target");
        let store = stored(&from, &[4]);
        drop(store);
        let other = stored(&into, &[9]);
        let held = other.manifest().total;
        drop(other);

        let refused = rewrite(&from, &into, 1_700_000_003);
        assert!(
            matches!(refused, Err(Trouble::Io(_))),
            "a rewrite wrote over a file that was already there"
        );
        let store = Store::open(&into).expect("a store");
        assert_eq!(
            store.manifest().total,
            held,
            "the store at the target moved"
        );
        drop(store);
        let _ = std::fs::remove_file(&from);
        let _ = std::fs::remove_file(&into);
    }
}
