//! Writing documents into a store, including the ones that are already in it.
//!
//! An index writer turns documents into a segment and a store appends segments,
//! and between the two there is nobody whose job it is to notice that a document
//! coming in is one the store already holds. That is what this is. A batch is
//! given documents, some of them under a key, and works out as it goes which of
//! the store's documents each key replaces. When it is committed the new segment
//! and the deletion of everything it replaced land in one commit, so a query
//! either sees the old copies or the new ones and never both.
//!
//! The keys are the ones [`index::Writer::add_keyed`] writes and
//! [`View::lookup`] reads. A document given no key is simply added, which
//! is what a corpus being loaded for the first time does, and it costs nothing
//! extra.
//!
//! A batch is built against a view, which is a snapshot, so what it decides to
//! delete is only true of the store as that view found it. Committing checks
//! that the store is still there and refuses otherwise, rather than writing a
//! set of deletions that would undo whatever was committed in between.

use std::collections::HashMap;

use crate::DocId;
use crate::bitmap::Bitmap;
use crate::error::Error;
use crate::file::{Appending, Lookup, Result, Store, Trouble, View};
use crate::index::{self, Held};
use crate::segment::Writer as SegmentWriter;

/// Documents on their way into a store, with the ones they replace.
///
/// Made with [`Batch::over`] on a view of the store, filled with
/// [`add`](Self::add) and [`add_keyed`](Self::add_keyed), and finished with
/// [`commit`](Self::commit).
///
/// It holds everything in memory until it is committed, because a segment is
/// written in one piece. [`held`](Self::held) says how much, which is what a
/// caller deciding when to stop feeding it needs.
pub struct Batch<'a> {
    /// The store as it was when this started, which is what the deletions are
    /// worked out against and what the commit is checked against.
    view: &'a View,
    /// Every segment's key index, opened once. Opening them per document is
    /// most of what a lookup costs.
    lookup: Lookup<'a>,
    /// The segment being built.
    writer: index::Writer,
    /// Per segment of the view, the documents this batch has replaced. Kept
    /// apart from what the segment already hides and joined with it at the end,
    /// so that nothing here depends on reading the committed sets twice.
    replaced: Vec<Option<Bitmap>>,
    /// The documents of the segment being built that a later document in the
    /// same batch replaced.
    superseded: Bitmap,
    /// What this batch has written, by key. A key given twice in one batch has
    /// to find the first copy, and the first copy is not in the store yet.
    mine: HashMap<Box<[u8]>, DocId>,
    /// What those keys come to, so [`held`](Self::held) is a read rather than a
    /// walk.
    key_bytes: u64,
    /// How many documents have been replaced, counted as it happens because the
    /// sets are per segment and adding their lengths up is not the same number
    /// once a batch replaces the same key twice.
    replacements: usize,
}

impl<'a> Batch<'a> {
    /// Starts a batch against a view of the store.
    ///
    /// # Errors
    ///
    /// Returns a decoding error if a segment of the view, one of its key
    /// sections or one of its sets of deletions is not what it claims to be.
    pub fn over(view: &'a View) -> Result<Self> {
        Self::with(view, index::Writer::new())
    }

    /// Starts a batch that says it is full once it holds `budget` bytes.
    ///
    /// A run loading a corpus commits a segment at a time rather than one
    /// segment at the end, and this is what tells it when.
    ///
    /// # Errors
    ///
    /// As [`over`](Self::over).
    pub fn with_budget(view: &'a View, budget: u64) -> Result<Self> {
        Self::with(view, index::Writer::with_budget(budget))
    }

    /// Starts a batch around a writer that has already been set up.
    fn with(view: &'a View, writer: index::Writer) -> Result<Self> {
        Ok(Self {
            view,
            lookup: view.lookup()?,
            writer,
            replaced: (0..view.len()).map(|_| None).collect(),
            superseded: Bitmap::new(),
            mine: HashMap::new(),
            key_bytes: 0,
            replacements: 0,
        })
    }

    /// Adds a document that replaces nothing.
    ///
    /// # Errors
    ///
    /// As [`index::Writer::add`].
    pub fn add(&mut self, text: &str) -> Result<DocId> {
        self.writer.add(text).map_err(Trouble::Format)
    }

    /// Adds a document that replaces nothing, with values to hand back with a
    /// hit.
    ///
    /// # Errors
    ///
    /// As [`index::Writer::add_with_fields`].
    pub fn add_with_fields<'f>(
        &mut self,
        text: &str,
        fields: impl IntoIterator<Item = (&'f str, &'f [u8])>,
    ) -> Result<DocId> {
        self.writer
            .add_with_fields(text, fields)
            .map_err(Trouble::Format)
    }

    /// Adds a document under a key, replacing whatever the store holds under
    /// that key.
    ///
    /// The document that is replaced is remembered as a deletion against the
    /// segment it is in, and nothing is written until the batch is committed, so
    /// a batch that is dropped changes nothing.
    ///
    /// A key this batch has already used replaces that document instead, which
    /// is the case the store cannot answer because the earlier copy is not in it
    /// yet.
    ///
    /// # Errors
    ///
    /// As [`index::Writer::add_keyed`], and a decoding error if a set of
    /// deletions in the view does not decode.
    pub fn add_keyed(&mut self, key: &[u8], text: &str) -> Result<DocId> {
        let doc = self.writer.add_keyed(key, text).map_err(Trouble::Format)?;
        self.replacing(key, doc)?;
        Ok(doc)
    }

    /// [`add_keyed`](Self::add_keyed), with values to hand back with a hit.
    ///
    /// # Errors
    ///
    /// As [`add_keyed`](Self::add_keyed).
    pub fn add_keyed_with_fields<'f>(
        &mut self,
        key: &[u8],
        text: &str,
        fields: impl IntoIterator<Item = (&'f str, &'f [u8])>,
    ) -> Result<DocId> {
        let doc = self
            .writer
            .add_keyed_with_fields(key, text, fields)
            .map_err(Trouble::Format)?;
        self.replacing(key, doc)?;
        Ok(doc)
    }

    /// Works out what the document just written under `key` replaces.
    fn replacing(&mut self, key: &[u8], doc: DocId) -> Result<()> {
        // What this batch has already written first. A key it has seen before
        // has already had whatever the store holds under it marked for
        // deletion, so asking the store again would find the same document, mark
        // it a second time and count a replacement that does not happen.
        if let Some(old) = self.mine.insert(key.into(), doc) {
            self.superseded.insert(old);
            self.replacements += 1;
            return Ok(());
        }
        self.key_bytes = self
            .key_bytes
            .saturating_add(u64::try_from(key.len()).unwrap_or(u64::MAX));
        if let Some((at, old)) = self.lookup.document(key)? {
            self.replaced[at]
                .get_or_insert_with(Bitmap::new)
                .insert(old);
            self.replacements += 1;
        }
        Ok(())
    }

    /// How many documents have been added.
    #[must_use]
    pub fn len(&self) -> usize {
        self.writer.len()
    }

    /// Whether none have.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.writer.is_empty()
    }

    /// Whether it is holding as much as it was given a budget for.
    ///
    /// Always false without a budget. Asked after a document rather than before
    /// one, because a document cannot be split across two segments and a budget
    /// checked first would refuse documents larger than itself.
    #[must_use]
    pub fn is_full(&self) -> bool {
        self.writer.is_full()
    }

    /// How many of them replaced a document, in the store or in this batch.
    ///
    /// This is how many documents the commit will delete, which is not the same
    /// as how many keys were given: a batch that writes the same key three times
    /// replaces twice.
    #[must_use]
    pub const fn replacements(&self) -> usize {
        self.replacements
    }

    /// How much memory this is holding.
    ///
    /// The segment being built is most of it. The rest is the keys, which are
    /// held a second time here because the batch has to be able to answer what
    /// it has written and the writer will not be asked until it is finished.
    #[must_use]
    pub fn held(&self) -> Held {
        let mut held = self.writer.held();
        held.keys = held.keys.saturating_add(self.key_bytes).saturating_add(
            u64::try_from(self.mine.capacity())
                .unwrap_or(u64::MAX)
                .saturating_mul(
                    u64::try_from(core::mem::size_of::<(Box<[u8]>, DocId)>()).unwrap_or(u64::MAX),
                ),
        );
        held
    }

    /// Builds the segment and works out the deletions that go with it.
    ///
    /// Nothing is written to the store here. The result is what
    /// [`Prepared::commit`] hands to [`Store::publish`], and it is a value, so a
    /// caller that builds batches on other threads can build them there and
    /// commit them on the one that owns the store.
    ///
    /// # Errors
    ///
    /// Returns a decoding error if the segment cannot be built or if a set of
    /// deletions in the view does not decode.
    pub fn finish(self) -> Result<Prepared> {
        let mut deletions = Vec::new();
        for (at, replaced) in self.replaced.into_iter().enumerate() {
            let Some(replaced) = replaced else {
                continue;
            };
            // Joined with what the segment already hides, because a set of
            // deletions is the whole answer for its segment and this one is only
            // the part this batch is adding to it.
            let mut whole = self.view.deleted(at)?.unwrap_or_default();
            whole.union_with(&replaced);
            deletions.push((at, whole));
        }

        let segment = if self.writer.is_empty() {
            None
        } else {
            let docs = u32::try_from(self.writer.len()).unwrap_or(u32::MAX);
            let built = index::Writer::build(vec![self.writer]).map_err(Trouble::Format)?;
            // The segment this commit adds answers to the position it is about
            // to take, which is where a key used twice in one batch puts the
            // copy that lost.
            if !self.superseded.is_empty() {
                deletions.push((self.view.len(), self.superseded));
            }
            Some((built, docs))
        };

        Ok(Prepared {
            segment,
            deletions,
            epoch: self.view.epoch(),
        })
    }

    /// Builds the segment and commits it with its deletions.
    ///
    /// `created` is written into the segment's descriptor and `written` into the
    /// manifest, both as the caller's own clock.
    ///
    /// # Errors
    ///
    /// As [`finish`](Self::finish) and [`Prepared::commit`].
    pub fn commit(self, store: &mut Store, created: u64, written: u64) -> Result<u64> {
        self.finish()?.commit(store, created, written)
    }
}

/// A batch that has been turned into a segment and a set of deletions.
///
/// Made by [`Batch::finish`]. It is a value with nothing borrowed in it, so the
/// work of building a segment and the commit that publishes it do not have to
/// happen in the same place.
#[derive(Debug)]
pub struct Prepared {
    /// The segment to append, and how many documents it holds. A batch of
    /// nothing but deletions has none.
    ///
    /// It is a writer rather than the bytes, so the segment is laid out straight
    /// into the store rather than into a vector on the way there. The vector is
    /// a copy of the largest thing an index run makes.
    pub segment: Option<(SegmentWriter, u32)>,
    /// What to delete, per segment, each set being the whole answer for its
    /// segment. The last of them may name the segment above, which is a document
    /// the batch replaced with a later one of its own.
    pub deletions: Vec<(usize, Bitmap)>,
    /// The epoch of the view this was worked out from.
    pub epoch: u64,
}

impl Prepared {
    /// Publishes it.
    ///
    /// # Errors
    ///
    /// Returns [`Trouble::Format`] with [`Error::StaleView`] if the store has
    /// been committed to since the batch read it, because the deletions are then
    /// about a store that no longer exists and committing them would undo
    /// whatever that commit did. Otherwise as [`Store::publish`].
    pub fn commit(self, store: &mut Store, created: u64, written: u64) -> Result<u64> {
        let committed = store.manifest().epoch;
        if committed != self.epoch {
            return Err(Trouble::Format(Error::StaleView {
                read: self.epoch,
                committed,
            }));
        }
        let segment = self
            .segment
            .map(|(built, docs)| (docs, move |into: &mut Appending<'_>| built.write_to(into)));
        store.publish_with(segment, created, &self.deletions, written)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::Searcher;

    /// A store identifier, so a file written by a test says what wrote it.
    const STORE: u128 = 0x006b_7572_612d_696e_6765_7374_0000_0001;

    /// A path in the system temporary directory, cleared if a run before this
    /// one left something there.
    fn path(name: &str) -> std::path::PathBuf {
        let path =
            std::env::temp_dir().join(format!("kura-ingest-{name}-{}.kura", std::process::id()));
        let _ = std::fs::remove_file(&path);
        path
    }

    /// A store with nothing in it.
    fn empty(path: &std::path::Path) -> Store {
        Store::create(path, STORE, 1_700_000_000).expect("a store")
    }

    /// Writes the documents into the store as one batch, keyed.
    fn write(store: &mut Store, documents: &[(&[u8], &str)]) -> u64 {
        let view = store.view().expect("a view");
        let mut batch = Batch::over(&view).expect("a batch");
        for (key, text) in documents {
            batch.add_keyed(key, text).expect("a document");
        }
        batch.commit(store, 1_700_000_001, 1).expect("committed")
    }

    /// How many live documents the store answers `query` with.
    fn count(store: &Store, query: &str) -> u64 {
        let view = store.view().expect("a view");
        let readers = view.readers().expect("readers");
        let searcher = Searcher::over(&readers).expect("a searcher");
        searcher.count(query).expect("counted")
    }

    #[test]
    fn a_document_under_a_key_the_store_already_holds_replaces_it() {
        let path = path("replace");
        let mut store = empty(&path);
        write(&mut store, &[(b"a", "the first quarter ledger")]);
        write(&mut store, &[(b"a", "the second quarter ledger")]);

        assert_eq!(count(&store, "ledger"), 1);
        assert_eq!(count(&store, "second"), 1);
        assert_eq!(count(&store, "first"), 0);
        assert_eq!(store.manifest().live, 1);
        assert_eq!(store.manifest().total, 2);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_key_the_store_does_not_hold_is_simply_added() {
        let path = path("add");
        let mut store = empty(&path);
        write(&mut store, &[(b"a", "the first ledger")]);
        write(&mut store, &[(b"b", "the second ledger")]);

        assert_eq!(count(&store, "ledger"), 2);
        assert_eq!(store.manifest().live, 2);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_batch_that_writes_the_same_key_twice_leaves_the_later_document() {
        let path = path("twice");
        let mut store = empty(&path);
        let view = store.view().expect("a view");
        let mut batch = Batch::over(&view).expect("a batch");
        batch.add_keyed(b"a", "the first ledger").expect("added");
        batch.add_keyed(b"a", "the second ledger").expect("added");
        batch.add_keyed(b"a", "the third ledger").expect("added");
        assert_eq!(batch.len(), 3);
        assert_eq!(batch.replacements(), 2);
        batch.commit(&mut store, 1, 1).expect("committed");
        drop(view);

        assert_eq!(count(&store, "ledger"), 1);
        assert_eq!(count(&store, "third"), 1);
        assert_eq!(count(&store, "first"), 0);
        assert_eq!(count(&store, "second"), 0);
        assert_eq!(store.manifest().live, 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_key_the_store_holds_and_the_batch_writes_twice_replaces_two_documents() {
        // The copy in the store and the earlier copy in the batch, which is two
        // documents and not three. Counting the store's copy once a document
        // would say a commit deletes more than it deletes, and the count is what
        // a run reports as replaced.
        let path = path("twice-over");
        let mut store = empty(&path);
        write(&mut store, &[(b"a", "the first ledger")]);

        let view = store.view().expect("a view");
        let mut batch = Batch::over(&view).expect("a batch");
        batch.add_keyed(b"a", "the second ledger").expect("added");
        batch.add_keyed(b"a", "the third ledger").expect("added");
        assert_eq!(batch.len(), 2);
        assert_eq!(batch.replacements(), 2);
        batch.commit(&mut store, 1, 1).expect("committed");
        drop(view);

        assert_eq!(count(&store, "ledger"), 1);
        assert_eq!(count(&store, "third"), 1);
        assert_eq!(store.manifest().live, 1);
        assert_eq!(store.manifest().total, 3);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_batch_replaces_documents_in_several_segments_in_one_commit() {
        let path = path("several");
        let mut store = empty(&path);
        write(&mut store, &[(b"a", "the first ledger")]);
        write(&mut store, &[(b"b", "the second ledger")]);
        write(&mut store, &[(b"c", "the third ledger")]);
        let epoch = store.manifest().epoch;

        write(
            &mut store,
            &[(b"a", "the fourth ledger"), (b"c", "the fifth ledger")],
        );
        assert_eq!(store.manifest().epoch, epoch + 1);
        assert_eq!(count(&store, "ledger"), 3);
        assert_eq!(count(&store, "fourth"), 1);
        assert_eq!(count(&store, "fifth"), 1);
        assert_eq!(count(&store, "second"), 1);
        assert_eq!(count(&store, "first"), 0);
        assert_eq!(count(&store, "third"), 0);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_document_with_no_key_replaces_nothing() {
        let path = path("unkeyed");
        let mut store = empty(&path);
        let view = store.view().expect("a view");
        let mut batch = Batch::over(&view).expect("a batch");
        batch.add("the first ledger").expect("added");
        batch.add("the first ledger").expect("added");
        assert_eq!(batch.replacements(), 0);
        batch.commit(&mut store, 1, 1).expect("committed");
        drop(view);

        assert_eq!(count(&store, "ledger"), 2);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn what_a_batch_replaced_is_still_replaced_when_the_store_is_opened_again() {
        let path = path("reopen");
        let mut store = empty(&path);
        write(&mut store, &[(b"a", "the first ledger")]);
        write(&mut store, &[(b"a", "the second ledger")]);
        drop(store);

        let store = Store::open(&path).expect("a store");
        assert_eq!(store.manifest().live, 1);
        assert_eq!(count(&store, "second"), 1);
        assert_eq!(count(&store, "first"), 0);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_batch_that_is_dropped_leaves_the_store_alone() {
        let path = path("dropped");
        let mut store = empty(&path);
        write(&mut store, &[(b"a", "the first ledger")]);
        let epoch = store.manifest().epoch;

        let view = store.view().expect("a view");
        let mut batch = Batch::over(&view).expect("a batch");
        batch.add_keyed(b"a", "the second ledger").expect("added");
        assert_eq!(batch.replacements(), 1);
        drop(batch);
        drop(view);

        assert_eq!(store.manifest().epoch, epoch);
        assert_eq!(count(&store, "first"), 1);
        assert_eq!(count(&store, "second"), 0);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_batch_built_on_a_view_the_store_has_moved_past_is_refused() {
        // Two batches replacing the same document. The second one worked out
        // what to delete before the first one committed, and its set says
        // nothing about what the first one deleted, so committing it would put
        // that document back.
        let path = path("stale");
        let mut store = empty(&path);
        write(&mut store, &[(b"a", "the first ledger")]);

        let view = store.view().expect("a view");
        let mut batch = Batch::over(&view).expect("a batch");
        batch.add_keyed(b"a", "the second ledger").expect("added");
        let prepared = batch.finish().expect("prepared");

        write(&mut store, &[(b"a", "the third ledger")]);
        let outcome = prepared.commit(&mut store, 1, 1);
        assert!(matches!(
            outcome,
            Err(Trouble::Format(Error::StaleView { .. }))
        ));
        assert_eq!(count(&store, "ledger"), 1);
        assert_eq!(count(&store, "third"), 1);
        assert_eq!(count(&store, "second"), 0);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_replacement_leaves_what_was_already_deleted_from_that_segment_deleted() {
        // The set of deletions a batch commits is the whole answer for its
        // segment, so it has to carry what the segment already hid. Getting this
        // wrong brings a deleted document back, which is the failure this whole
        // shape exists to avoid.
        let path = path("keepgone");
        let mut store = empty(&path);
        write(
            &mut store,
            &[
                (b"a", "the first ledger"),
                (b"b", "the second ledger"),
                (b"c", "the third ledger"),
            ],
        );
        let doc = {
            let view = store.view().expect("a view");
            let lookup = view.lookup().expect("a lookup");
            lookup.document(b"b").expect("looked up").expect("a hit").1
        };
        store
            .delete(0, &Bitmap::from_sorted(&[doc]), 2)
            .expect("deleted");
        assert_eq!(count(&store, "ledger"), 2);

        write(&mut store, &[(b"a", "the fourth ledger")]);
        assert_eq!(count(&store, "ledger"), 2);
        assert_eq!(count(&store, "second"), 0);
        assert_eq!(count(&store, "fourth"), 1);
        assert_eq!(count(&store, "third"), 1);
        assert_eq!(store.manifest().live, 2);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_batch_of_nothing_commits_nothing_but_still_commits() {
        let path = path("nothing");
        let mut store = empty(&path);
        let epoch = store.manifest().epoch;
        let view = store.view().expect("a view");
        let batch = Batch::over(&view).expect("a batch");
        assert!(batch.is_empty());
        let prepared = batch.finish().expect("prepared");
        assert!(prepared.segment.is_none());
        assert!(prepared.deletions.is_empty());
        prepared.commit(&mut store, 1, 1).expect("committed");
        drop(view);

        assert_eq!(store.manifest().epoch, epoch + 1);
        assert_eq!(store.manifest().segments.len(), 0);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_view_taken_before_a_replacement_still_answers_with_the_old_document() {
        let path = path("snapshot");
        let mut store = empty(&path);
        write(&mut store, &[(b"a", "the first ledger")]);
        let before = store.view().expect("a view");

        write(&mut store, &[(b"a", "the second ledger")]);

        let readers = before.readers().expect("readers");
        let searcher = Searcher::over(&readers).expect("a searcher");
        assert_eq!(searcher.count("ledger").expect("counted"), 1);
        assert_eq!(searcher.count("first").expect("counted"), 1);
        assert_eq!(count(&store, "second"), 1);
        assert_eq!(count(&store, "first"), 0);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn what_a_batch_holds_grows_with_the_keys_it_is_given() {
        let path = path("held");
        let store = empty(&path);
        let view = store.view().expect("a view");
        let mut batch = Batch::over(&view).expect("a batch");
        let bare = batch.held().keys;
        for n in 0..100 {
            batch
                .add_keyed(format!("record-{n:04}").as_bytes(), "the ledger")
                .expect("added");
        }
        // Ten bytes a key of key, twice, plus what the two tables cost to hold
        // them.
        assert!(batch.held().keys > bare + 2_000);
        let _ = std::fs::remove_file(&path);
    }
}
