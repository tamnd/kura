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
//!
//! # Several batches at once
//!
//! A commit is two syncs and a sync is milliseconds, so a store taking small
//! writes spends nearly all of its time waiting for the drive. [`commit_all`]
//! is the answer to that: the batches that are ready go into the file together
//! and one manifest names all of them, so the group pays what one of them would
//! have. Which copy of a key wins is decided by the order the batches are given
//! in, exactly as it is decided between commits, and the store that comes out is
//! the one the same batches committed one at a time would have made.
//!
//! # The log
//!
//! A batch holds everything in memory until it commits, so a machine that stops
//! halfway through one loses every document it had taken. [`Logged`] is the same
//! batch with the log underneath it: each document goes into the log as it
//! arrives, and the commit is ordered so that the log and the manifest cannot
//! disagree about which documents made it.
//!
//! [`replay`] is the other end of that. A store that stopped without warning
//! comes back with records in its log that no segment holds, and a replay turns
//! them into a segment and commits it, in the same order and for the same
//! reason. A store nobody crashed has an empty log and a replay does nothing to
//! it, which is what makes it something to do on every open rather than
//! something to decide about.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, HashMap};

use crate::DocId;
use crate::bitmap::Bitmap;
use crate::error::Error;
use crate::file::{Appending, Lookup, Result, Store, Trouble, View};
use crate::index::{self, Held};
use crate::segment::Writer as SegmentWriter;
use crate::upsert::{self, Upsert};

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

    /// Adds a document that came out of the log.
    ///
    /// The record holds the analysed form, so this does not run the analyser and
    /// cannot disagree with the index the record was written beside. A record
    /// with a key replaces under that key exactly as [`add_keyed`](Self::add_keyed)
    /// would, which is what makes a replay of a batch of replacements land where
    /// the batch would have.
    ///
    /// # Errors
    ///
    /// As [`index::Writer::add_record`], and a decoding error if the record's
    /// fields or a set of deletions in the view do not decode.
    pub fn add_record(&mut self, record: &upsert::Record<'_>) -> Result<DocId> {
        let doc = self.writer.add_record(record).map_err(Trouble::Format)?;
        if let Some(key) = record.key() {
            self.replacing(key, doc)?;
        }
        Ok(doc)
    }

    /// Adds a document and fills `into` with the record that goes in the log.
    ///
    /// Private because a record that is built and not written down is worse than
    /// no record at all: it costs the same and promises something it does not
    /// keep. [`Logged`] is the only caller, and it appends what this fills.
    fn add_logged(
        &mut self,
        key: Option<&[u8]>,
        text: &str,
        fields: &[(&str, &[u8])],
        into: &mut Upsert,
    ) -> Result<DocId> {
        let doc = self
            .writer
            .add_logged(key, text, fields, into)
            .map_err(Trouble::Format)?;
        if let Some(key) = key {
            self.replacing(key, doc)?;
        }
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
            keys: self.mine.into_iter().collect(),
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

/// A batch that writes what it takes into the store's log.
///
/// A [`Batch`] is a promise held in memory: nothing about it reaches the file
/// until it commits, so a machine that stops with a batch half full loses all of
/// it. This is the same batch with each document written into the log as it
/// arrives, so what has been taken survives a stop.
///
/// # The order a commit happens in
///
/// The records are already in the log, one per document, written as the
/// documents arrived. Committing then frees the log up to the last of them, and
/// only after that publishes the segment and the manifest. The manifest is what
/// carries the freed position to the platter, so the two land together.
///
/// A machine that stops anywhere before the manifest is written comes back with
/// the log still naming those records, and a replay puts the documents back. One
/// that stops after it comes back with the log already past them and replays
/// nothing, because the segment holds them. There is no window where a document
/// is in both and none where it is in neither.
///
/// A commit that fails leaves the store at the state it was, and the records of
/// that batch are logically freed although nothing has overwritten them yet. The
/// batch is gone with them, so a caller that wants those documents has to add
/// them again, which is the same thing it would do if the commit had failed
/// without a log.
///
/// # When the log fills
///
/// The ring is finite and a batch is bounded by it as well as by its memory
/// budget. A record that does not fit makes the batch full rather than failing:
/// the document is in the segment being built and will be committed with the
/// rest, it simply has nothing in the log until then. [`is_full`](Self::is_full)
/// says so and [`unlogged`](Self::unlogged) counts how many, so a run that keeps
/// feeding a full batch is a run that can be seen doing it rather than one that
/// quietly stops being durable.
pub struct Logged<'a> {
    /// The store whose log this writes to, and which the commit goes to.
    store: &'a mut Store,
    /// The documents, and the replacements they make.
    batch: Batch<'a>,
    /// The record being built, held across documents so that a run of a million
    /// of them is not a million allocations.
    record: Upsert,
    /// The bytes of that record, held for the same reason.
    payload: Vec<u8>,
    /// How many records went into the log.
    records: u64,
    /// What they came to, which is the number to compare a log against a corpus
    /// with.
    bytes: u64,
    /// How many documents the log had no room for.
    unlogged: u64,
}

impl<'a> Logged<'a> {
    /// Starts a logged batch against a view of the store.
    ///
    /// The view is taken before the store is borrowed, since a view owns its
    /// mapping and is a snapshot rather than a handle.
    ///
    /// # Errors
    ///
    /// As [`Batch::over`].
    pub fn over(view: &'a View, store: &'a mut Store) -> Result<Self> {
        Ok(Self::around(Batch::over(view)?, store))
    }

    /// Starts a logged batch that says it is full once it holds `budget` bytes.
    ///
    /// # Errors
    ///
    /// As [`Batch::with_budget`].
    pub fn with_budget(view: &'a View, store: &'a mut Store, budget: u64) -> Result<Self> {
        Ok(Self::around(Batch::with_budget(view, budget)?, store))
    }

    /// The parts, once the batch is made.
    fn around(batch: Batch<'a>, store: &'a mut Store) -> Self {
        Self {
            store,
            batch,
            record: Upsert::new(),
            payload: Vec::new(),
            records: 0,
            bytes: 0,
            unlogged: 0,
        }
    }

    /// Adds a document that replaces nothing.
    ///
    /// # Errors
    ///
    /// As [`Batch::add`], and [`Trouble::Io`] if the log cannot be written.
    pub fn add(&mut self, text: &str) -> Result<DocId> {
        self.write(None, text, &[])
    }

    /// Adds a document that replaces nothing, with values to hand back with a
    /// hit.
    ///
    /// # Errors
    ///
    /// As [`add`](Self::add).
    pub fn add_with_fields(&mut self, text: &str, fields: &[(&str, &[u8])]) -> Result<DocId> {
        self.write(None, text, fields)
    }

    /// Adds a document under a key, replacing whatever the store holds under it.
    ///
    /// # Errors
    ///
    /// As [`Batch::add_keyed`], and [`Trouble::Io`] if the log cannot be
    /// written.
    pub fn add_keyed(&mut self, key: &[u8], text: &str) -> Result<DocId> {
        self.write(Some(key), text, &[])
    }

    /// [`add_keyed`](Self::add_keyed), with values to hand back with a hit.
    ///
    /// # Errors
    ///
    /// As [`add_keyed`](Self::add_keyed).
    pub fn add_keyed_with_fields(
        &mut self,
        key: &[u8],
        text: &str,
        fields: &[(&str, &[u8])],
    ) -> Result<DocId> {
        self.write(Some(key), text, fields)
    }

    /// The one way in: index the document, then log what was indexed.
    ///
    /// In that order, because the record is filled out of the analyser as the
    /// index is built and there is nothing to write before that has happened.
    /// The gap it leaves is the width of one document, and a stop inside it
    /// loses a document whose add had not returned.
    fn write(&mut self, key: Option<&[u8]>, text: &str, fields: &[(&str, &[u8])]) -> Result<DocId> {
        let doc = self.batch.add_logged(key, text, fields, &mut self.record)?;
        self.payload.clear();
        self.record.write_to(&mut self.payload);
        match self.store.append(upsert::KIND, &self.payload) {
            Ok(_) => {
                self.records += 1;
                self.bytes = self
                    .bytes
                    .saturating_add(u64::try_from(self.payload.len()).unwrap_or(u64::MAX));
            }
            Err(Trouble::Format(Error::LogFull { .. })) => self.unlogged += 1,
            Err(other) => return Err(other),
        }
        Ok(doc)
    }

    /// Puts what has been logged so far on the platter.
    ///
    /// A commit does this anyway as part of writing the manifest. This is for a
    /// caller that wants the documents it has added durable before it is ready
    /// to commit a segment.
    ///
    /// # Errors
    ///
    /// As [`Store::sync`].
    pub fn sync(&self) -> Result<()> {
        self.store.sync()
    }

    /// How many documents have been added.
    #[must_use]
    pub fn len(&self) -> usize {
        self.batch.len()
    }

    /// Whether none have.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.batch.is_empty()
    }

    /// Whether it is holding as much as it was given a budget for, or the log
    /// has run out of room for what it holds.
    #[must_use]
    pub fn is_full(&self) -> bool {
        self.batch.is_full() || self.unlogged > 0
    }

    /// How many of the documents replaced one.
    #[must_use]
    pub const fn replacements(&self) -> usize {
        self.batch.replacements()
    }

    /// How much memory this is holding.
    #[must_use]
    pub fn held(&self) -> Held {
        self.batch.held()
    }

    /// How many records went into the log.
    #[must_use]
    pub const fn records(&self) -> u64 {
        self.records
    }

    /// What those records came to in bytes.
    #[must_use]
    pub const fn logged(&self) -> u64 {
        self.bytes
    }

    /// How many documents the log had no room for.
    ///
    /// Zero for a run that commits when the batch says it is full.
    #[must_use]
    pub const fn unlogged(&self) -> u64 {
        self.unlogged
    }

    /// Builds the segment, and keeps the store and the log position it will be
    /// committed with.
    ///
    /// Split from the commit for the same reason [`Batch::finish`] is: building
    /// a segment and writing it are the two halves of what a commit costs, and a
    /// caller measuring one of them has to be able to stand between them.
    ///
    /// # Errors
    ///
    /// As [`Batch::finish`].
    pub fn finish(self) -> Result<Pending<'a>> {
        Ok(Pending {
            prepared: self.batch.finish()?,
            // Read here rather than at the commit, so that what is freed is what
            // this batch wrote and not whatever else has reached the log since.
            through: self.store.log().tail(),
            store: self.store,
        })
    }

    /// Frees the log and commits the segment with its deletions.
    ///
    /// # Errors
    ///
    /// As [`Pending::commit`].
    pub fn commit(self, created: u64, written: u64) -> Result<u64> {
        self.finish()?.commit(created, written)
    }
}

/// A logged batch that has been built and not yet written.
///
/// Made by [`Logged::finish`]. It holds the store, so unlike [`Prepared`] it
/// cannot be carried to another thread, which is the price of the commit knowing
/// what to free without being told.
pub struct Pending<'a> {
    /// The store the segment goes into and the log belongs to.
    store: &'a mut Store,
    /// The segment and the deletions.
    prepared: Prepared,
    /// The log position this batch wrote up to.
    through: u64,
}

impl Pending<'_> {
    /// How long the segment will be, or zero if there is none.
    #[must_use]
    pub fn size(&self) -> u64 {
        self.prepared.segment.as_ref().map_or(0, |(built, _)| {
            u64::try_from(built.size()).unwrap_or(u64::MAX)
        })
    }

    /// How many documents it holds.
    #[must_use]
    pub fn documents(&self) -> u32 {
        self.prepared.segment.as_ref().map_or(0, |&(_, docs)| docs)
    }

    /// Frees the log and commits the segment with its deletions.
    ///
    /// # Errors
    ///
    /// As [`Prepared::commit`], and [`Trouble::Format`] with
    /// [`Error::BadPositions`] if the log has moved under this batch.
    pub fn commit(self, created: u64, written: u64) -> Result<u64> {
        // Before the publish and not after it. The head reaches the platter
        // inside the manifest the publish writes, so freeing it here is what
        // makes one commit out of the segment and the log position. Freeing it
        // afterwards would leave a committed manifest naming records that are
        // already in a segment, and a recovery would apply them twice.
        self.store.truncate_log(self.through)?;
        self.prepared.commit(self.store, created, written)
    }
}

/// What a replay put back.
///
/// Made by [`replay`]. Every count is about the records the log held, so a store
/// that was closed cleanly gives back one of these with nothing in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Replayed {
    /// How many records were read out of the log.
    pub records: u64,
    /// What their payloads came to.
    pub bytes: u64,
    /// How many documents went into the segment that was committed, which is
    /// the same as the records unless a record was damaged.
    pub documents: u32,
    /// How many of them replaced a document, in the store or earlier in the log.
    pub replacements: usize,
    /// The epoch of the commit that made them visible, or nothing if the log had
    /// nothing to put back and no commit happened.
    pub epoch: Option<u64>,
}

impl Replayed {
    /// Whether the log had nothing in it.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.records == 0
    }
}

/// Puts back the documents the log holds and nothing else is holding.
///
/// This is what a store does when it opens: the manifest says what is in
/// segments, the log says what was promised after that and never got there, and
/// this is the difference between the two turned into a segment and committed.
/// A store that was closed cleanly has an empty log and this does nothing to it,
/// so it costs a walk that ends at the first byte.
///
/// # The order, again
///
/// The same order [`Pending::commit`] uses and for the same reason. The log is
/// freed first and the segment published second, because the freed head only
/// reaches the platter inside the manifest the publish writes. So a machine that
/// stops in the middle of a replay comes back with the log exactly as it was and
/// replays the same records again, and one that stops after it comes back with
/// them in a segment and nothing to replay. Replaying twice is not a risk that
/// has to be reasoned about, it is the ordinary case.
///
/// A key replayed twice replaces under that key twice, which is what the batch
/// that logged it did, so the documents that come back are the ones that would
/// have been there rather than every version of them.
///
/// # What it holds
///
/// A segment, built from every record the log had, which is bounded by the ring
/// and not by anything this decides. A store with a quarter of a gigabyte of
/// records to put back builds a segment out of all of them at once. That is the
/// same bound a batch has and the same answer as the flush trigger, and it is
/// not solved here.
///
/// # Errors
///
/// Returns [`Trouble::Io`] if the log cannot be read, [`Trouble::Format`] with
/// [`Error::UnknownRecord`] if the log holds a record this build cannot apply,
/// and whatever [`Prepared::commit`] returns if the commit fails. Damage in the
/// log is not one of them: a torn record at the end is where the log ends, and
/// the records before it are put back.
pub fn replay(store: &mut Store, created: u64, written: u64) -> Result<Replayed> {
    let view = store.view()?;
    let mut batch = Batch::over(&view)?;
    // The first thing that goes wrong stops the replay applying records and is
    // returned once the walk is over. The walk hands records to a closure that
    // cannot fail, which is right, because the walk's business is what is in the
    // log and not what applying it does.
    let mut trouble: Option<Trouble> = None;
    let walked = store.recover(|record| {
        if trouble.is_some() {
            return;
        }
        if record.kind != upsert::KIND {
            trouble = Some(Trouble::Format(Error::UnknownRecord {
                kind: record.kind,
                position: record.position,
            }));
            return;
        }
        if let Err(problem) = upsert::Record::read(record.payload)
            .map_err(Trouble::Format)
            .and_then(|held| batch.add_record(&held))
        {
            trouble = Some(problem);
        }
    })?;
    if let Some(trouble) = trouble {
        return Err(trouble);
    }
    if walked.records == 0 {
        return Ok(Replayed::default());
    }

    let replacements = batch.replacements();
    let through = store.log().tail();
    let prepared = batch.finish()?;
    let documents = prepared.segment.as_ref().map_or(0, |&(_, docs)| docs);
    store.truncate_log(through)?;
    let epoch = prepared.commit(store, created, written)?;
    Ok(Replayed {
        records: walked.records,
        bytes: walked.bytes,
        documents,
        replacements,
        epoch: Some(epoch),
    })
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
    /// The key of every document in the segment that has one, with the document
    /// it belongs to.
    ///
    /// The batch had these already and they move here rather than being copied,
    /// so what they cost is holding them until the commit instead of until the
    /// segment was built. They are what [`commit_all`] needs: two batches that
    /// wrote the same key are two documents that both answer to it, and nothing
    /// else in either of them says so.
    ///
    /// A batch committed on its own has no use for them. They are one pointer
    /// and one length per key in a value that is about to be written to disk,
    /// against the segment beside them, which is megabytes.
    pub keys: Vec<(Box<[u8]>, DocId)>,
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

/// Commits a group of prepared batches as one commit.
///
/// A commit costs two syncs and a sync is milliseconds, so a store taking small
/// writes from several places at once spends nearly all of its time waiting for
/// the drive, and every one of those waits is for the same drive. This is the
/// answer to that: the batches that are ready go into the file together, one
/// manifest names all of them, and the group pays what one of them would have.
/// Nothing about the promise changes, because the sync that has to happen before
/// the manifest is written happens whether the manifest names one segment or
/// twenty.
///
/// The batches are committed in the order they are given, which is the order a
/// key written by two of them is decided by. The last one to write a key is the
/// one that answers to it and the copies before it are deleted, which is what a
/// batch that writes the same key twice already does. Nothing else could be
/// right: the group is one commit, and a store cannot answer one key with two
/// documents.
///
/// Every batch has to have been prepared against the committed state. A batch
/// prepared before some other commit landed is refused, as it is on its own and
/// for the same reason. The caller holding the group is the one that knows how
/// to answer that writer, which is why they are refused together here rather
/// than one at a time.
///
/// # The log
///
/// This does not free it. A [`Logged`] batch commits through [`Pending`], which
/// frees the log up to what that batch wrote and does it in the one order that
/// works, and a group has no equivalent because the position to free is the
/// furthest any of its members reached. So a group is of batches that were not
/// logged, or of batches whose caller frees the log itself with
/// [`Store::truncate_log`] before it calls this. A group that leaves records in
/// the log naming documents its segments already hold is a store that puts them
/// back on the next open, which for a keyed document is a rewrite and for one
/// without a key is a second copy.
///
/// # Errors
///
/// Returns [`Trouble::Format`] with [`Error::StaleView`] if any of the batches
/// was prepared against a state the store has moved on from, in which case
/// nothing is committed, and [`Error::MissingSection`] if a batch names a
/// segment of its own that it did not build. Otherwise as [`Store::publish_all`].
pub fn commit_all(
    store: &mut Store,
    parts: Vec<Prepared>,
    created: u64,
    written: u64,
) -> Result<u64> {
    let committed = store.manifest().epoch;
    for part in &parts {
        if part.epoch != committed {
            return Err(Trouble::Format(Error::StaleView {
                read: part.epoch,
                committed,
            }));
        }
    }

    // Where the next segment of the group lands. The batches were all worked
    // out against the same state, so they all name their own segment as the
    // position one past the last committed one, and each of them has to be told
    // which position that turned out to be.
    let base = store.manifest().segments.len();
    let mut at = base;
    let mut segments = Vec::with_capacity(parts.len());
    let mut sets: BTreeMap<usize, Bitmap> = BTreeMap::new();
    // Who wrote each key, and which of their documents it is.
    let mut written_keys: HashMap<Box<[u8]>, (usize, DocId)> = HashMap::new();

    for part in parts {
        let position = if part.segment.is_some() {
            Some(at)
        } else {
            None
        };
        for (target, set) in part.deletions {
            let target = if target == base {
                position.ok_or(Trouble::Format(Error::MissingSection { kind: 0 }))?
            } else {
                target
            };
            union(&mut sets, target, set);
        }
        for (key, doc) in part.keys {
            let Some(position) = position else {
                continue;
            };
            if let Some((was_in, was)) = written_keys.insert(key, (position, doc)) {
                // The earlier copy is in a segment of this same commit and it
                // stops answering the moment that segment appears, so the store
                // never shows both.
                let mut one = Bitmap::new();
                one.insert(was);
                union(&mut sets, was_in, one);
            }
        }
        if let Some((built, docs)) = part.segment {
            segments.push((docs, move |into: &mut Appending<'_>| built.write_to(into)));
            at += 1;
        }
    }

    let deletions: Vec<(usize, Bitmap)> = sets.into_iter().collect();
    store.publish_all(segments, created, &deletions, written)
}

/// Adds a set of deletions to whatever the group already deletes from that
/// segment.
///
/// A set is the whole answer for its segment, so two batches that each deleted
/// something from the same one have to be joined rather than to take turns
/// overwriting each other. Both of them read what the segment already hid, so
/// the join is a union and not a merge with a rule.
fn union(sets: &mut BTreeMap<usize, Bitmap>, at: usize, adding: Bitmap) {
    match sets.entry(at) {
        Entry::Occupied(mut held) => held.get_mut().union_with(&adding),
        Entry::Vacant(spot) => {
            spot.insert(adding);
        }
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

    /// The documents a logged batch is fed in these tests.
    const CORPUS: [(&[u8], &str); 4] = [
        (b"a", "the first quarter ledger"),
        (b"b", "the second quarter ledger"),
        (b"c", "notes on the ledger and the quarter it covers"),
        (b"d", "nothing to do with any of the others"),
    ];

    /// What the log holds, as the keys of the records in it, oldest first.
    fn replayable(store: &mut Store) -> Vec<Vec<u8>> {
        let mut keys = Vec::new();
        store
            .recover(|record| {
                assert_eq!(record.kind, upsert::KIND, "a record of another kind");
                let read = upsert::Record::read(record.payload).expect("a record this build wrote");
                keys.push(read.key().expect("a keyed document").to_vec());
            })
            .expect("the log walks");
        keys
    }

    #[test]
    fn what_a_logged_batch_takes_is_in_the_log_before_it_commits() {
        let path = path("logged");
        let mut store = empty(&path);
        let before = store.manifest().epoch;
        let view = store.view().expect("a view");
        let mut batch = Logged::over(&view, &mut store).expect("a batch");
        for (key, text) in CORPUS {
            batch.add_keyed(key, text).expect("added");
        }
        assert_eq!(batch.records(), 4);
        assert_eq!(batch.unlogged(), 0);
        assert!(batch.logged() > 0);
        // Dropped rather than committed, which is the machine stopping with a
        // batch half full.
        drop(batch);
        drop(view);

        assert_eq!(store.manifest().epoch, before, "nothing was committed");
        assert_eq!(count(&store, "ledger"), 0, "and nothing is searchable");
        assert_eq!(replayable(&mut store), vec![b"a", b"b", b"c", b"d"]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_commit_leaves_the_log_with_nothing_to_replay() {
        let path = path("logged-commit");
        let mut store = empty(&path);
        let view = store.view().expect("a view");
        let mut batch = Logged::over(&view, &mut store).expect("a batch");
        for (key, text) in CORPUS {
            batch.add_keyed(key, text).expect("added");
        }
        batch.commit(1_700_000_001, 1).expect("committed");
        drop(view);

        assert_eq!(count(&store, "ledger"), 3);
        assert_eq!(store.manifest().live, 4);
        assert!(replayable(&mut store).is_empty(), "the log was freed");
        // And the freed position is on the platter rather than only in memory,
        // which is what stops a reopened store from applying the batch again.
        let reopened = Store::open(&path).expect("a store");
        assert_eq!(reopened.manifest().wal_head, reopened.manifest().wal_tail);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_logged_batch_writes_the_segment_an_ordinary_batch_writes() {
        // The log is not allowed to change what is indexed. Two stores, the
        // same documents, and the segments compared byte for byte.
        let plain = path("logged-against-plain");
        let logged = path("logged-against-logged");
        let mut one = empty(&plain);
        write(&mut one, &CORPUS);

        let mut two = empty(&logged);
        let view = two.view().expect("a view");
        let mut batch = Logged::over(&view, &mut two).expect("a batch");
        for (key, text) in CORPUS {
            batch.add_keyed(key, text).expect("added");
        }
        batch.commit(1_700_000_001, 1).expect("committed");
        drop(view);

        let left = one.view().expect("a view");
        let right = two.view().expect("a view");
        assert_eq!(left.bytes(0), right.bytes(0));
        let _ = std::fs::remove_file(&plain);
        let _ = std::fs::remove_file(&logged);
    }

    #[test]
    fn a_batch_the_log_has_no_room_for_says_it_is_full() {
        let path = path("logged-full");
        // A ring of one page, which a handful of documents fills.
        let mut store = Store::create_with_log(&path, STORE, 1_700_000_000, 4096).expect("a store");
        let view = store.view().expect("a view");
        let mut batch = Logged::over(&view, &mut store).expect("a batch");
        let mut added = 0;
        while !batch.is_full() && added < 500 {
            let key = format!("record-{added:04}");
            batch
                .add_keyed(key.as_bytes(), "the quarter ledger and its notes")
                .expect("a document the log has no room for still goes in");
            added += 1;
        }

        assert!(
            batch.is_full(),
            "the log filled and the batch never said so"
        );
        assert_eq!(batch.unlogged(), 1, "it should stop at the first refusal");
        assert_eq!(batch.len(), added, "every document is in the segment");
        batch.commit(1_700_000_001, 1).expect("committed");
        drop(view);

        // The commit is what makes them durable, and it holds all of them
        // including the one the log had no room for.
        assert_eq!(store.manifest().live, added as u64);
        assert_eq!(count(&store, "ledger"), added as u64);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_logged_batch_replaces_what_the_store_holds_under_the_same_key() {
        let path = path("logged-replace");
        let mut store = empty(&path);
        write(&mut store, &[(b"a", "the first quarter ledger")]);

        let view = store.view().expect("a view");
        let mut batch = Logged::over(&view, &mut store).expect("a batch");
        batch
            .add_keyed(b"a", "the second quarter ledger")
            .expect("added");
        assert_eq!(batch.replacements(), 1);
        batch.commit(1_700_000_002, 2).expect("committed");
        drop(view);

        assert_eq!(count(&store, "ledger"), 1);
        assert_eq!(count(&store, "second"), 1);
        assert_eq!(count(&store, "first"), 0);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_logged_document_keeps_the_fields_it_was_given() {
        let path = path("logged-fields");
        let mut store = empty(&path);
        let view = store.view().expect("a view");
        let mut batch = Logged::over(&view, &mut store).expect("a batch");
        batch
            .add_keyed_with_fields(b"a", "the quarter ledger", &[("path", b"docs/a.md")])
            .expect("added");
        drop(batch);
        drop(view);

        // Out of the log rather than out of the segment, because the segment is
        // the part that was not written.
        let mut seen = Vec::new();
        store
            .recover(|record| {
                let read = upsert::Record::read(record.payload).expect("a record");
                let mut walk = read.fields();
                while let Some((name, value)) = walk.next_field().expect("a field") {
                    seen.push((name.to_string(), value.to_vec()));
                }
            })
            .expect("the log walks");
        assert_eq!(seen, vec![("path".to_string(), b"docs/a.md".to_vec())]);
        let _ = std::fs::remove_file(&path);
    }

    /// Fills a logged batch with the corpus and drops it, which is the machine
    /// stopping with a batch half full.
    fn stopped(store: &mut Store, documents: &[(&[u8], &str)]) {
        let view = store.view().expect("a view");
        let mut batch = Logged::over(&view, store).expect("a batch");
        for (key, text) in documents {
            batch.add_keyed(key, text).expect("added");
        }
    }

    #[test]
    fn a_store_that_stopped_comes_back_with_the_documents_its_log_holds() {
        let path = path("replay");
        let mut store = empty(&path);
        stopped(&mut store, &CORPUS);
        drop(store);

        let mut store = Store::open(&path).expect("the store opens");
        assert_eq!(count(&store, "ledger"), 0, "before the replay, nothing");
        let put_back = replay(&mut store, 1_700_000_001, 1).expect("the replay finishes");

        assert_eq!(put_back.records, 4);
        assert_eq!(put_back.documents, 4);
        assert_eq!(put_back.replacements, 0);
        assert!(put_back.bytes > 0);
        assert!(
            put_back.epoch.is_some(),
            "a replay that put documents back \
            committed them"
        );
        assert_eq!(store.manifest().live, 4);
        assert_eq!(count(&store, "ledger"), 3);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_replay_leaves_a_log_with_nothing_left_to_replay() {
        let path = path("replay-once");
        let mut store = empty(&path);
        stopped(&mut store, &CORPUS);
        drop(store);

        let mut store = Store::open(&path).expect("the store opens");
        replay(&mut store, 1_700_000_001, 1).expect("the replay finishes");
        drop(store);

        // Reopened rather than replayed again in place, because the freed head
        // has to be on the platter and not only in memory. A second replay that
        // found the same records would put every document in twice.
        let mut store = Store::open(&path).expect("the store opens");
        let again = replay(&mut store, 1_700_000_002, 2).expect("the replay finishes");
        assert!(again.is_empty(), "the log was freed by the first replay");
        assert_eq!(again.epoch, None, "and nothing was committed for it");
        assert_eq!(store.manifest().live, 4);
        assert_eq!(count(&store, "ledger"), 3);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_replay_of_a_store_that_was_closed_cleanly_does_nothing() {
        let path = path("replay-clean");
        let mut store = empty(&path);
        write(&mut store, &CORPUS);
        let before = store.manifest().epoch;
        drop(store);

        let mut store = Store::open(&path).expect("the store opens");
        let put_back = replay(&mut store, 1_700_000_002, 2).expect("the replay finishes");
        assert!(put_back.is_empty());
        assert_eq!(
            store.manifest().epoch,
            before,
            "a replay with nothing to do commits nothing"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_replayed_document_replaces_what_the_store_holds_under_its_key() {
        let path = path("replay-replace");
        let mut store = empty(&path);
        write(&mut store, &[(b"a", "the first quarter ledger")]);
        stopped(&mut store, &[(b"a", "the second quarter ledger")]);
        drop(store);

        let mut store = Store::open(&path).expect("the store opens");
        let put_back = replay(&mut store, 1_700_000_002, 2).expect("the replay finishes");

        assert_eq!(put_back.documents, 1);
        assert_eq!(put_back.replacements, 1, "the older copy went with it");
        assert_eq!(count(&store, "ledger"), 1);
        assert_eq!(count(&store, "second"), 1);
        assert_eq!(count(&store, "first"), 0);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_replay_writes_the_segment_the_batch_would_have_committed() {
        // The claim the whole thing rests on. A store that was interrupted and
        // replayed has to be the store that committed, byte for byte, or the
        // log is a different index that happens to hold the same documents.
        let committed = path("replay-against-committed");
        let interrupted = path("replay-against-replayed");
        let mut one = empty(&committed);
        {
            let view = one.view().expect("a view");
            let mut batch = Logged::over(&view, &mut one).expect("a batch");
            for (key, text) in CORPUS {
                batch.add_keyed(key, text).expect("added");
            }
            batch.commit(1_700_000_001, 1).expect("committed");
        }

        let mut two = empty(&interrupted);
        stopped(&mut two, &CORPUS);
        drop(two);
        let mut two = Store::open(&interrupted).expect("the store opens");
        replay(&mut two, 1_700_000_001, 1).expect("the replay finishes");

        let left = one.view().expect("a view");
        let right = two.view().expect("a view");
        assert_eq!(left.bytes(0), right.bytes(0));
        let _ = std::fs::remove_file(&committed);
        let _ = std::fs::remove_file(&interrupted);
    }

    #[test]
    fn a_record_this_build_cannot_apply_stops_the_replay_rather_than_being_skipped() {
        let path = path("replay-unknown");
        let mut store = empty(&path);
        let before = store.manifest().epoch;
        store
            .append(4242, b"a record from a build that came later")
            .expect("appended");
        store.sync().expect("synced");
        drop(store);

        let mut store = Store::open(&path).expect("the store opens");
        let refused = replay(&mut store, 1_700_000_001, 1);
        assert!(
            matches!(
                refused,
                Err(Trouble::Format(Error::UnknownRecord { kind: 4242, .. }))
            ),
            "a replay that skipped it would lose a write the store promised"
        );
        assert_eq!(
            store.manifest().epoch,
            before,
            "and it leaves the store where it was"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// Overwrites part of a store with zeroes, which is what a machine that
    /// stopped leaves behind when the bytes never reached the platter.
    fn tear(path: &std::path::Path, at: u64, len: u64) {
        use std::io::{Seek, SeekFrom, Write};
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("the store opens for writing");
        file.seek(SeekFrom::Start(at)).expect("seeks");
        file.write_all(&vec![
            0u8;
            usize::try_from(len).expect("a length that fits")
        ])
        .expect("writes");
        file.sync_all().expect("syncs");
    }

    #[test]
    fn a_log_torn_at_any_page_boundary_gives_back_what_was_promised() {
        // The fault matrix. Every 4 KiB boundary in the window that was written
        // since the last commit is a place a machine can stop, so the store is
        // torn at each of them in turn and opened. What a commit returned for is
        // there every time, and what was only logged comes back as far as the
        // tear and no further.
        let source = path("replay-torn");
        // A megabyte of ring rather than the default quarter of a gigabyte, so
        // that a copy of the file per boundary is cheap.
        let mut store =
            Store::create_with_log(&source, STORE, 1_700_000_000, 1 << 20).expect("a store");
        write(&mut store, &[(b"audited", "the audited quarter ledger")]);
        let keys: Vec<String> = (0..200).map(|n| format!("pending-{n:04}")).collect();
        let pending: Vec<(&[u8], &str)> = keys
            .iter()
            .map(|key| (key.as_bytes(), "a pending quarter ledger entry"))
            .collect();
        stopped(&mut store, &pending);

        let base = store.superblock().wal_offset;
        let head = store.manifest().wal_head;
        let tail = store.log().tail();
        drop(store);
        assert!(
            tail >= head + 3 * 4096,
            "the window is {} bytes, which is not enough boundaries to be a matrix",
            tail - head
        );

        let mut cuts: Vec<u64> = (head..tail).step_by(4096).collect();
        cuts.push(tail);
        let mut before = 0;
        for cut in cuts {
            let torn = path(&format!("replay-torn-at-{cut}"));
            std::fs::copy(&source, &torn).expect("a copy of the store");
            tear(&torn, base + cut, tail - cut);

            let mut store = Store::open(&torn).expect("the store opens");
            let put_back = replay(&mut store, 1_700_000_002, 2).expect("the replay finishes");
            let recovered = put_back.documents as usize;

            assert_eq!(
                count(&store, "audited"),
                1,
                "the committed document survived a tear at {cut}"
            );
            assert!(
                recovered <= keys.len(),
                "a tear at {cut} put back {recovered} documents out of {}",
                keys.len()
            );
            assert!(
                recovered >= before,
                "a tear at {cut} put back {recovered} where a tear before it put back {before}"
            );
            // A prefix and not a subset. The records are in the log in the order
            // they were written, so a tear takes the end of that order off and
            // cannot take a document out of the middle.
            let view = store.view().expect("a view");
            for (at, key) in keys.iter().enumerate() {
                let found = view
                    .document(key.as_bytes())
                    .expect("the key index reads")
                    .is_some();
                assert_eq!(
                    found,
                    at < recovered,
                    "document {at} after a tear at {cut} that put back {recovered}"
                );
            }
            before = recovered;
            drop(view);
            drop(store);
            let _ = std::fs::remove_file(&torn);
        }
        assert_eq!(before, keys.len(), "an untorn log gives everything back");
        let _ = std::fs::remove_file(&source);
    }

    /// Builds a batch against the store as it stands and stops short of
    /// committing it, which is what a writer waiting for a group holds.
    fn prepare(store: &Store, documents: &[(&[u8], &str)]) -> Prepared {
        let view = store.view().expect("a view");
        let mut batch = Batch::over(&view).expect("a batch");
        for (key, text) in documents {
            batch.add_keyed(key, text).expect("a document");
        }
        batch.finish().expect("prepared")
    }

    #[test]
    fn batches_committed_as_a_group_are_all_in_the_store_at_the_same_epoch() {
        let path = path("group");
        let mut store = empty(&path);
        let was = store.manifest().epoch;
        let group = vec![
            prepare(&store, &[(b"a", "the first quarter ledger")]),
            prepare(&store, &[(b"b", "the second quarter ledger")]),
            prepare(&store, &[(b"c", "the third quarter ledger")]),
        ];
        let epoch = commit_all(&mut store, group, 1_700_000_001, 1).expect("committed");

        assert_eq!(epoch, was + 1, "a group is one commit");
        assert_eq!(count(&store, "ledger"), 3);
        assert_eq!(store.manifest().segments.len(), 3);
        assert_eq!(store.manifest().live, 3);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_group_costs_the_syncs_one_commit_costs() {
        let path = path("group-syncs");
        let mut store = empty(&path);
        let group: Vec<Prepared> = (0..8u8)
            .map(|n| prepare(&store, &[(&[n], "the quarter ledger")]))
            .collect();

        let before = store.syncs();
        commit_all(&mut store, group, 1_700_000_001, 1).expect("committed");
        let group_cost = store.syncs() - before;

        let before = store.syncs();
        write(&mut store, &[(b"z", "one more ledger")]);
        let one_cost = store.syncs() - before;

        assert_eq!(one_cost, 2, "a commit is the data and then the manifest");
        assert_eq!(
            group_cost, one_cost,
            "eight commits in a group wait for the drive as often as one does"
        );
        assert_eq!(count(&store, "ledger"), 9);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_key_two_batches_of_a_group_wrote_answers_with_the_later_one() {
        let path = path("group-key");
        let mut store = empty(&path);
        let group = vec![
            prepare(&store, &[(b"a", "the first quarter ledger")]),
            prepare(&store, &[(b"a", "the second quarter ledger")]),
        ];
        commit_all(&mut store, group, 1_700_000_001, 1).expect("committed");

        assert_eq!(count(&store, "ledger"), 1, "one key is one document");
        assert_eq!(count(&store, "second"), 1);
        assert_eq!(count(&store, "first"), 0);
        assert_eq!(store.manifest().live, 1);
        assert_eq!(store.manifest().total, 2);
        let view = store.view().expect("a view");
        assert_eq!(
            view.document(b"a")
                .expect("the key index reads")
                .map(|(at, _)| at),
            Some(1),
            "the key answers with the later batch"
        );
        drop(view);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn two_batches_of_a_group_replacing_documents_in_one_segment_both_take_effect() {
        let path = path("group-replace");
        let mut store = empty(&path);
        write(
            &mut store,
            &[
                (b"a", "the first quarter ledger"),
                (b"b", "the second quarter ledger"),
            ],
        );
        let group = vec![
            prepare(&store, &[(b"a", "the first quarter report")]),
            prepare(&store, &[(b"b", "the second quarter report")]),
        ];
        commit_all(&mut store, group, 1_700_000_002, 2).expect("committed");

        assert_eq!(count(&store, "report"), 2);
        assert_eq!(count(&store, "ledger"), 0, "both replacements are deleted");
        assert_eq!(store.manifest().live, 2);
        assert_eq!(store.manifest().total, 4);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_batch_prepared_before_a_commit_takes_the_whole_group_with_it() {
        let path = path("group-stale");
        let mut store = empty(&path);
        let stale = prepare(&store, &[(b"a", "the first quarter ledger")]);
        write(&mut store, &[(b"b", "the second quarter ledger")]);
        let fresh = prepare(&store, &[(b"c", "the third quarter ledger")]);

        let epoch = store.manifest().epoch;
        let refused = commit_all(&mut store, vec![stale, fresh], 1_700_000_002, 2);
        assert!(
            matches!(refused, Err(Trouble::Format(Error::StaleView { .. }))),
            "a batch that read a store that has moved on is refused, and got {refused:?}"
        );
        assert_eq!(store.manifest().epoch, epoch, "nothing was committed");
        assert_eq!(count(&store, "ledger"), 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_batch_that_wrote_its_own_key_twice_keeps_its_own_answer_in_a_group() {
        let path = path("group-twice");
        let mut store = empty(&path);
        let mut group = Vec::new();
        group.push(prepare(&store, &[(b"a", "the first quarter ledger")]));
        let view = store.view().expect("a view");
        let mut batch = Batch::over(&view).expect("a batch");
        batch
            .add_keyed(b"b", "the second quarter ledger")
            .expect("added");
        batch
            .add_keyed(b"b", "the third quarter ledger")
            .expect("added");
        group.push(batch.finish().expect("prepared"));
        drop(view);
        commit_all(&mut store, group, 1_700_000_001, 1).expect("committed");

        assert_eq!(count(&store, "ledger"), 2);
        assert_eq!(count(&store, "second"), 0, "the copy it replaced itself");
        assert_eq!(count(&store, "third"), 1);
        assert_eq!(count(&store, "first"), 1);
        assert_eq!(store.manifest().live, 2);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_group_of_nothing_leaves_the_store_where_it_was() {
        let path = path("group-empty");
        let mut store = empty(&path);
        write(&mut store, &[(b"a", "the first quarter ledger")]);
        let segments = store.manifest().segments.len();
        let epoch = store.manifest().epoch;

        commit_all(&mut store, Vec::new(), 1_700_000_002, 2).expect("committed");

        assert_eq!(store.manifest().segments.len(), segments);
        assert_eq!(store.manifest().epoch, epoch + 1, "still a commit");
        assert_eq!(count(&store, "ledger"), 1);
        let _ = std::fs::remove_file(&path);
    }
}
