//! Building a searchable segment out of documents.
//!
//! The pieces below this were each measurable on their own. This is where they
//! become an index: text goes in, a segment comes out, and the segment answers
//! which documents hold a term and how often.
//!
//! Indexing is one pass. A document is analysed, its terms are counted, and
//! each term's postings are appended to that term's own byte chain straight
//! away, delta coded, in the arena. Nothing is sorted per posting and nothing is
//! buffered per posting. At the end the only sort is over the vocabulary, which
//! is a few hundred thousand entries against tens of millions of postings.
//!
//! That is the decision this module is built around. The obvious way to write an
//! indexer is to collect `(term, document, frequency)` triples and sort them,
//! and it costs twelve bytes per posting plus a scratch buffer the same size
//! again. Delta coded varints in a chain cost about two. On a corpus of any
//! size that is the difference between an indexer that fits in memory and one
//! that does not.

use crate::analysis::Analyzer;
use crate::codec::{get_u32, get_u64, get_uvarint, put_u32, put_u64, put_uvarint};
use crate::error::{Error, Result};
use crate::search::{B, K1};
use crate::segment::{Segment, Writer as SegmentWriter, kind};
use crate::{DocId, bitmap::Bitmap, bound, filter, keys, posting, store, terms};

/// The size of one link in a term's posting chain, in bytes.
///
/// Small enough that a term appearing in one document does not pay for a
/// kilobyte, large enough that a term appearing in a million documents does not
/// spend its time following pointers. Sixty four bytes is also a cache line,
/// which is the unit the walk at the end reads in anyway.
const CHUNK: usize = 64;

/// The bytes at the front of a chunk holding the offset of the next one.
const LINK: usize = 4;

/// The bytes in a chunk that hold postings.
const PAYLOAD: usize = CHUNK - LINK;

/// The most a document gap and a frequency can take, as two 32 bit varints.
///
/// A chunk is closed as soon as fewer than this many bytes are left, so a pair
/// never straddles two chunks. That wastes up to nine bytes per chunk and buys a
/// writer and a reader that both know where a chunk ends without a terminator,
/// which a delta of zero would otherwise be needed for and which is a legal
/// first document.
const MAX_PAIR: usize = 10;

/// The end of a chain.
const NONE: u32 = u32::MAX;

/// The size of one block of the arena the posting chains live in.
///
/// The arena grows by adding a block rather than by growing a vector, so the
/// bytes already written are never copied and the process never holds two copies
/// of the postings at once. A vector that doubles does both: it copies
/// everything written so far at every growth, and for the length of that copy
/// the old block and the new one are live together, which on a corpus whose
/// postings come to forty six megabytes is a transient of about seventy.
///
/// A power of two, so that splitting an offset into a block and a position in it
/// is a shift and a mask. A multiple of [`CHUNK`], so that a chunk never
/// straddles two blocks and every read stays inside one slice.
///
/// Sixty four kilobytes is a thousand chunks, which is small enough that a
/// writer given ten documents does not hold a megabyte and large enough that a
/// writer given a large corpus spends its time indexing rather than allocating.
const BLOCK: usize = 64 << 10;

/// How far to shift an arena offset to get the block it is in.
const BLOCK_SHIFT: u32 = BLOCK.trailing_zeros();

/// What to mask an arena offset with to get its position in its block.
const BLOCK_MASK: usize = BLOCK - 1;

// The two things every accessor on the arena takes for granted. Checked here
// rather than in a test, because a test that fails is a test somebody has to
// run and this is a build that does not happen.
const _: () = assert!(
    BLOCK.is_power_of_two(),
    "a block that is not a power of two is a block an offset cannot be split by shifting"
);
const _: () = assert!(
    BLOCK.is_multiple_of(CHUNK),
    "a block that is not a whole number of chunks is a block a chunk can straddle"
);

/// The bytes the posting chains live in.
///
/// Chunks are handed out in order and never given back, and an offset into this
/// is a whole number that means the same thing for the life of the writer, which
/// is what lets a chain be a list of `u32` links rather than a list of pointers.
///
/// Every accessor here takes a length and every range it is asked for lies
/// inside a single chunk, which is why none of them has to deal with a read that
/// crosses a block. That is not a hope, it is [`BLOCK`] being a multiple of
/// [`CHUNK`].
#[derive(Debug, Default)]
struct Arena {
    blocks: Vec<Box<[u8]>>,
    /// How much of the last block has been handed out.
    used: usize,
}

impl Arena {
    /// Hands out one zeroed chunk and says where it starts.
    fn chunk(&mut self) -> u32 {
        if self.used == 0 || self.used == BLOCK {
            // A fresh block is zeroed, and a chunk is handed out once, so
            // nothing here ever has to be cleared again.
            self.blocks.push(vec![0u8; BLOCK].into_boxed_slice());
            self.used = 0;
        }
        let at = (self.blocks.len() - 1) * BLOCK + self.used;
        self.used += CHUNK;
        u32::try_from(at).expect("the arena is under four gigabytes")
    }

    /// `len` bytes from `at`, which must not run past the chunk `at` is in.
    fn at(&self, at: usize, len: usize) -> &[u8] {
        let offset = at & BLOCK_MASK;
        &self.blocks[at >> BLOCK_SHIFT][offset..offset + len]
    }

    /// The same, to write into.
    fn at_mut(&mut self, at: usize, len: usize) -> &mut [u8] {
        let offset = at & BLOCK_MASK;
        &mut self.blocks[at >> BLOCK_SHIFT][offset..offset + len]
    }

    /// How many bytes this is holding.
    ///
    /// The whole of every block, including the part of the last one that has not
    /// been handed out yet, because that is what the allocator is holding.
    fn held(&self) -> u64 {
        holding::<u8>(self.blocks.len().saturating_mul(BLOCK))
            + holding::<Box<[u8]>>(self.blocks.capacity())
    }
}

/// How many terms one block of the per term arrays holds.
///
/// A power of two so that splitting an identifier into a block and a position in
/// it is a shift and a mask, and eight thousand of them so that a block is tens
/// of kilobytes rather than a fraction of a page.
const TERMS: usize = 8 << 10;

/// How far to shift a term identifier to get the block it is in.
const TERMS_SHIFT: u32 = TERMS.trailing_zeros();

/// What to mask a term identifier with to get its position in its block.
const TERMS_MASK: usize = TERMS - 1;

const _: () = assert!(
    TERMS.is_power_of_two(),
    "a block that is not a power of two is a block an identifier cannot be split by shifting"
);

/// An array with an entry per term, grown by adding a block.
///
/// A vector would do everything this does and would double to do it, which on a
/// large vocabulary means a single document taking the writer megabytes past
/// whatever budget it was given, since the step a doubling takes is the size of
/// everything already held. This grows by a fixed amount instead, so the largest
/// step is a block however large the corpus is, and nothing already written is
/// ever copied.
///
/// Entries are handed out in order and never given back, so an identifier means
/// the same thing for the life of the writer.
#[derive(Debug, Default)]
struct Blocks<T> {
    blocks: Vec<Box<[T]>>,
    len: usize,
}

impl<T: Copy + Default> Blocks<T> {
    /// Adds an entry at the end.
    fn push(&mut self, value: T) {
        if self.len & TERMS_MASK == 0 {
            self.blocks
                .push(vec![T::default(); TERMS].into_boxed_slice());
        }
        self.blocks[self.len >> TERMS_SHIFT][self.len & TERMS_MASK] = value;
        self.len += 1;
    }

    /// How many entries there are.
    const fn len(&self) -> usize {
        self.len
    }

    /// How many bytes this is holding.
    ///
    /// The whole of every block, including the part of the last one that has not
    /// been handed out, because that is what the allocator is holding.
    fn held(&self) -> u64 {
        holding::<T>(self.blocks.len().saturating_mul(TERMS))
            + holding::<Box<[T]>>(self.blocks.capacity())
    }
}

impl<T> core::ops::Index<usize> for Blocks<T> {
    type Output = T;

    fn index(&self, at: usize) -> &T {
        &self.blocks[at >> TERMS_SHIFT][at & TERMS_MASK]
    }
}

impl<T> core::ops::IndexMut<usize> for Blocks<T> {
    fn index_mut(&mut self, at: usize) -> &mut T {
        &mut self.blocks[at >> TERMS_SHIFT][at & TERMS_MASK]
    }
}

/// The bytes of every term seen, grown by adding a block.
///
/// Same reasoning as [`Blocks`], and a separate type because the entries are
/// runs of bytes of different lengths rather than one fixed size thing. A term
/// that will not fit in what is left of the last block starts a new one, so a
/// term is always one slice and a reader never has to deal with a term that
/// straddles a block. Terms are capped at 255 bytes, so what that wastes is at
/// most a quarter of a kilobyte in every sixty four.
#[derive(Debug, Default)]
struct Text {
    blocks: Vec<Box<[u8]>>,
    /// How much of the last block has been handed out.
    used: usize,
}

impl Text {
    /// Copies a term in and says where it went.
    fn push(&mut self, term: &[u8]) -> u32 {
        if self.blocks.is_empty() || BLOCK - self.used < term.len() {
            self.blocks.push(vec![0u8; BLOCK].into_boxed_slice());
            self.used = 0;
        }
        let at = (self.blocks.len() - 1) * BLOCK + self.used;
        let block = self
            .blocks
            .last_mut()
            .expect("a block was just made sure of");
        block[self.used..self.used + term.len()].copy_from_slice(term);
        self.used += term.len();
        u32::try_from(at).expect("the vocabulary is under four gigabytes")
    }

    /// `len` bytes from `at`, which is one whole term.
    fn at(&self, at: usize, len: usize) -> &[u8] {
        let offset = at & BLOCK_MASK;
        &self.blocks[at >> BLOCK_SHIFT][offset..offset + len]
    }

    /// How many bytes this is holding.
    fn held(&self) -> u64 {
        holding::<u8>(self.blocks.len().saturating_mul(BLOCK))
            + holding::<Box<[u8]>>(self.blocks.capacity())
    }
}

/// What is known about a term while a document is being counted.
///
/// The two are together because they are read and written together, once per
/// occurrence of a term, which is the hottest line in an index build.
#[derive(Debug, Default, Clone, Copy)]
struct Counting {
    /// The document this term was last counted in, plus one, so that zero can
    /// mean a term no document has held yet.
    stamp: u32,
    /// How often it occurs in the document being counted.
    frequency: u32,
}

/// Where a term's posting chain is and what has gone into it so far.
///
/// These five are together because they are read and written together, once per
/// term per document, when the counted document is appended to the chains.
#[derive(Debug, Default, Clone, Copy)]
struct Chain {
    /// Where the chain starts.
    head: u32,
    /// Where the chunk being written to starts.
    tail: u32,
    /// How much of that chunk has been written.
    used: u32,
    /// How many documents are in the chain.
    documents: u32,
    /// The last document appended, which the next gap is measured from.
    last: DocId,
}

/// Builds an index from documents.
///
/// Documents are numbered in the order they are added, from zero, and that
/// number is what the posting lists hold.
#[derive(Debug, Default)]
pub struct Writer {
    analyzer: Analyzer,
    postings: Accumulator,
    lengths: Vec<u32>,
    total: u64,
    store: store::Writer,
    /// Whether any document stored anything, which decides whether the store
    /// section is written at all. An index nobody asks values back from should
    /// not carry eight bytes per document saying so.
    stored: bool,
    /// The primary key of each document that was given one, in the order they
    /// were given rather than in order. They are sorted once, in
    /// [`Writer::build`], because that is where the parts are folded and the
    /// document numbers move, so sorting here would sort the wrong numbers.
    keys: Vec<(Box<[u8]>, DocId)>,
    /// How many bytes those keys come to, kept as they arrive because
    /// [`Writer::held`] is asked after every document and walking the keys to
    /// add them up would make a budgeted run quadratic.
    key_bytes: u64,
    /// How much this writer is willing to hold before it says it is full, if
    /// anybody said.
    budget: Option<u64>,
}

/// How much memory a writer is holding, and where it is.
///
/// A writer that has not finished is holding the whole of what it has been
/// given, in a shape that is nothing like what the segment will be, and a caller
/// deciding when to stop feeding it has no other way to know how much that is.
/// Feeding it until the machine complains is not a plan, and the size of the
/// text that went in is a poor proxy, since the ratio between the two is neither
/// one nor constant.
///
/// Every number is capacity rather than length, because capacity is what the
/// allocator is actually holding on the caller's behalf and length is what would
/// be held by a program that had planned perfectly.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Held {
    /// The chains of postings, and the per term bookkeeping beside them.
    pub postings: u64,
    /// The terms themselves, and the table that finds them.
    pub vocabulary: u64,
    /// The values that will be handed back with a hit, already compressed.
    pub stored: u64,
    /// Per document numbers that are neither of the above, which is the length
    /// of each document.
    pub lengths: u64,
    /// The primary keys, which are held whole until the segment is written
    /// because they arrive in no particular order.
    pub keys: u64,
}

impl Held {
    /// Everything, which is the number a budget is usually compared against.
    #[must_use]
    pub const fn total(&self) -> u64 {
        self.postings
            .saturating_add(self.vocabulary)
            .saturating_add(self.stored)
            .saturating_add(self.lengths)
            .saturating_add(self.keys)
    }
}

/// What a capacity of `T` costs in bytes.
fn holding<T>(capacity: usize) -> u64 {
    let bytes = capacity.saturating_mul(core::mem::size_of::<T>());
    u64::try_from(bytes).unwrap_or(u64::MAX)
}

impl Writer {
    /// Creates an empty index.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates an empty index that says it is full once it holds `budget` bytes.
    ///
    /// The writer does not act on it. It has nowhere to write a segment to and
    /// no way to know where the caller wants one, so all it does is answer
    /// [`Writer::is_full`] and leave the decision where the decision belongs.
    /// What a caller does with a full writer is finish it, put the segment
    /// somewhere, and carry on with a fresh one.
    ///
    /// The budget is bytes of what the writer holds, which is
    /// [`Held::total`], and not bytes of text that went in. Those differ by a
    /// factor that depends on the vocabulary and on how repetitive the corpus
    /// is, so the second is not a budget anybody can set on purpose.
    ///
    /// It is also not the memory the process will use. A run holds the writer,
    /// the allocator's slack and whatever the caller is reading documents
    /// through, and on real corpora the process peaks at several times what the
    /// writer says it holds. This bounds the part the engine is responsible for.
    ///
    /// There is a floor. A writer holds the compressor's match table before it
    /// has been given anything, so `Writer::new().held().total()` is the least
    /// any budget can mean, and a budget under it makes a writer that is full
    /// from the first document. That is a legal thing to ask for and it is
    /// almost certainly not what the caller meant.
    #[must_use]
    pub fn with_budget(budget: u64) -> Self {
        Self {
            budget: Some(budget),
            ..Self::default()
        }
    }

    /// What this writer was told it may hold, if anything.
    #[must_use]
    pub const fn budget(&self) -> Option<u64> {
        self.budget
    }

    /// Whether it is holding as much as it was told it may.
    ///
    /// Always false on a writer nobody gave a budget to, which is what a writer
    /// that keeps everything until the end has always done.
    ///
    /// Ask it after adding a document rather than before. A writer asked first
    /// would refuse a document larger than the whole budget, and a budget that
    /// cannot index a large file is not a budget, it is a corpus filter.
    #[must_use]
    pub fn is_full(&self) -> bool {
        self.budget
            .is_some_and(|budget| self.held().total() >= budget)
    }

    /// Adds a document and returns the identifier it was given.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotSorted`] if more documents are added than a document
    /// identifier can hold.
    pub fn add(&mut self, text: &str) -> Result<DocId> {
        self.add_with_fields(text, core::iter::empty())
    }

    /// Adds a document, indexing `text` and keeping `fields` to hand back with
    /// a hit.
    ///
    /// The two are separate on purpose. What is worth searching and what is
    /// worth showing are different questions, and a path is the usual example
    /// of something that answers no to the first and yes to the second.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotSorted`] if more documents are added than a document
    /// identifier can hold.
    pub fn add_with_fields<'f>(
        &mut self,
        text: &str,
        fields: impl IntoIterator<Item = (&'f str, &'f [u8])>,
    ) -> Result<DocId> {
        self.insert(None, text, fields)
    }

    /// Adds a document under a primary key.
    ///
    /// The key is what names the document from outside the store. Nothing else
    /// does: a document identifier belongs to the segment it is in and moves
    /// when segments are merged, so it cannot be written down anywhere and used
    /// again later. A key can, which is what makes replacing a document and
    /// deleting it by name possible at all.
    ///
    /// It is bytes rather than a string because the caller knows what its keys
    /// are and this does not. A path, a URL, a message identifier and a row
    /// identifier are all keys somebody has, and the only property this needs
    /// from them is that they compare.
    ///
    /// The key goes in the segment beside the terms, not in the stored fields.
    /// A stored field is handed back with a hit and cannot be searched for,
    /// which is the wrong way round for the one value the caller looks documents
    /// up by.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotSorted`] if more documents are added than a document
    /// identifier can hold.
    pub fn add_keyed(&mut self, key: &[u8], text: &str) -> Result<DocId> {
        self.insert(Some(key), text, core::iter::empty())
    }

    /// Adds a document under a primary key, and keeps `fields` to hand back with
    /// a hit.
    ///
    /// [`add_keyed`](Self::add_keyed) and
    /// [`add_with_fields`](Self::add_with_fields) at once.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotSorted`] if more documents are added than a document
    /// identifier can hold.
    pub fn add_keyed_with_fields<'f>(
        &mut self,
        key: &[u8],
        text: &str,
        fields: impl IntoIterator<Item = (&'f str, &'f [u8])>,
    ) -> Result<DocId> {
        self.insert(Some(key), text, fields)
    }

    /// The one place a document is added, whether or not it was named.
    ///
    /// The key is taken here rather than in a method of its own so that a
    /// document has one key or none by construction. A writer that let a key be
    /// attached to a document already added would have to answer what a second
    /// key for the same document means, and there is no good answer to that.
    fn insert<'f>(
        &mut self,
        key: Option<&[u8]>,
        text: &str,
        fields: impl IntoIterator<Item = (&'f str, &'f [u8])>,
    ) -> Result<DocId> {
        let doc =
            u32::try_from(self.lengths.len()).map_err(|_| Error::NotSorted { at: u32::MAX })?;
        // The stamp is the document number plus one so that zero can mean a term
        // that has not been seen in any document yet, which is what lets the per
        // term scratch start as zeroes instead of being cleared per document.
        let stamp = doc + 1;
        let Self {
            analyzer,
            postings,
            lengths,
            total,
            store,
            stored,
            keys,
            key_bytes,
            budget: _,
        } = self;
        let length = analyzer.analyze(text, |term, _| postings.count(term, stamp));
        postings.flush(doc);
        lengths.push(length);
        *total += u64::from(length);
        let before = store.values();
        store.push(fields)?;
        *stored |= store.values() > before;
        if let Some(key) = key {
            keys.push((key.into(), doc));
            *key_bytes = key_bytes.saturating_add(u64::try_from(key.len()).unwrap_or(u64::MAX));
        }
        Ok(doc)
    }

    /// How many documents have been added.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lengths.len()
    }

    /// Whether no documents have been added.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lengths.is_empty()
    }

    /// How much memory this writer is holding, and where.
    ///
    /// This is the number to write a flushing rule against. A rule written
    /// against the size of the text that has gone in is measuring the wrong
    /// thing, because the ratio between the two depends on the vocabulary, on
    /// how repetitive the corpus is and on how much of each document is kept to
    /// hand back, and none of those is known before the corpus is read.
    ///
    /// It walks nothing and allocates nothing, so it is cheap enough to ask
    /// after every document.
    #[must_use]
    pub fn held(&self) -> Held {
        let mut held = self.postings.held();
        held.stored = self.store.held();
        held.lengths = holding::<u32>(self.lengths.capacity());
        // The keys themselves as well as the table pointing at them, because a
        // key is usually longer than the sixteen bytes of the pair that holds
        // it and a budget that counted only the pairs would be out by an order
        // of magnitude on a corpus keyed by path.
        held.keys =
            holding::<(Box<[u8]>, DocId)>(self.keys.capacity()).saturating_add(self.key_bytes);
        held
    }

    /// Writes the segment.
    ///
    /// # Errors
    ///
    /// Returns an error if a posting chain does not decode, which can only
    /// happen if this module wrote one wrong.
    pub fn finish(self) -> Result<Vec<u8>> {
        Self::concat(vec![self])
    }

    /// Writes one segment out of writers that indexed consecutive slices of the
    /// same corpus.
    ///
    /// This is how indexing goes wide without this crate deciding how. Split
    /// the corpus, hand each slice to a writer on a thread of its own, and fold
    /// the writers here in the order the slices were taken. The documents of a
    /// part are numbered after the documents of every part before it, which is
    /// the only thing that order decides.
    ///
    /// Nothing is sorted here either. Each part already knows its own
    /// vocabulary in order, and this walks all of them at once, so the cost is
    /// one pass over the postings and no pass over the text. The parts are not
    /// combined into a larger index first, because holding one is the memory
    /// this is trying not to spend.
    ///
    /// ```
    /// # use kura_core::index::Writer;
    /// let mut parts = Vec::new();
    /// for slice in [["the first document", "and the second"], ["a third", "a fourth"]] {
    ///     let mut part = Writer::new();
    ///     for text in slice {
    ///         part.add(text)?;
    ///     }
    ///     parts.push(part);
    /// }
    /// let segment = Writer::concat(parts)?;
    /// # Ok::<(), kura_core::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error if a posting chain does not decode, which can only
    /// happen if this module wrote one wrong, and [`Error::Overflow`] if the
    /// parts together hold more documents than a document identifier can name.
    pub fn concat(parts: Vec<Self>) -> Result<Vec<u8>> {
        Ok(Self::build(parts)?.finish())
    }

    /// Folds the parts and hands back the segment before it is laid out.
    ///
    /// This is [`Writer::concat`] without the copy at the end. A caller with a
    /// file to write into should use this and
    /// [`SegmentWriter::write_to`](crate::segment::Writer::write_to), because on
    /// a corpus of any size the vector `concat` returns is a few hundred
    /// megabytes that exist only to be handed straight on.
    ///
    /// # Errors
    ///
    /// The same as [`Writer::concat`].
    pub fn build(parts: Vec<Self>) -> Result<SegmentWriter> {
        // Where each part's documents start once they are all in one segment.
        let mut base = Vec::with_capacity(parts.len());
        let mut documents = 0u32;
        for part in &parts {
            base.push(documents);
            documents = u32::try_from(part.lengths.len())
                .ok()
                .and_then(|count| documents.checked_add(count))
                .ok_or(Error::Overflow)?;
        }

        // The ceilings need the mean length before the first posting is pushed,
        // which is why the total is summed here rather than where the norms are
        // laid out below.
        let total: u64 = parts.iter().map(|part| part.total).sum();
        let mut ceilings = bound::Writer::new(K1, B, average(total, documents));

        let order: Vec<Vec<u32>> = parts.iter().map(|part| part.postings.sorted()).collect();
        let mut front = vec![0usize; parts.len()];
        let mut dictionary = terms::Writer::new();
        let mut blob = Vec::new();
        // One list writer for the whole vocabulary. A segment has as many lists
        // as terms and most of them are a few bytes long, so a writer per term
        // would be an allocation per term for nothing.
        let mut list = posting::Writer::new();

        loop {
            // The next term is the smallest at the front of any part. There are
            // as many parts as there are threads, so finding it by looking at
            // all of them costs less than the heap that would avoid it.
            let mut next: Option<&[u8]> = None;
            for (index, part) in parts.iter().enumerate() {
                if let Some(&id) = order[index].get(front[index]) {
                    let term = part.postings.vocabulary.term(id);
                    if next.is_none_or(|held| term < held) {
                        next = Some(term);
                    }
                }
            }
            let Some(term) = next else { break };

            let mut docs = 0u32;
            for index in 0..parts.len() {
                let Some(&id) = order[index].get(front[index]) else {
                    continue;
                };
                let part = &parts[index];
                if part.postings.vocabulary.term(id) != term {
                    continue;
                }
                part.postings
                    .walk(id, base[index], &part.lengths, &mut list, &mut ceilings)?;
                docs += part.postings.chains[id as usize].documents;
                front[index] += 1;
            }
            let offset = blob.len() as u64;
            list.finish_into(&mut blob);
            ceilings.finish_term(offset);
            dictionary.push(
                term,
                terms::Entry {
                    docs,
                    offset,
                    len: blob.len() as u64 - offset,
                },
            )?;
        }

        let mut norms = Vec::with_capacity(16 + documents as usize * 4);
        put_u32(&mut norms, documents);
        put_u64(&mut norms, total);
        for part in &parts {
            for length in &part.lengths {
                put_u32(&mut norms, *length);
            }
        }

        // The stores are folded last and into the first of them rather than
        // into a new one, so the largest thing here is moved once instead of
        // twice.
        let mut stored = false;
        let mut store: Option<store::Writer> = None;
        let mut named: Vec<(Box<[u8]>, DocId)> = Vec::new();
        for (index, part) in parts.into_iter().enumerate() {
            stored |= part.stored;
            named.extend(
                part.keys
                    .into_iter()
                    .map(|(key, doc)| (key, doc.saturating_add(base[index]))),
            );
            match &mut store {
                Some(held) => held.merge(part.store)?,
                None => store = Some(part.store),
            }
        }
        let key_sections = key_index(named)?;

        let mut segment = SegmentWriter::new();
        segment.add(kind::TERMS, dictionary.finish())?;
        segment.add(kind::POSTINGS, blob)?;
        segment.add(kind::NORMS, norms)?;
        // A segment whose every list is shorter than a block has nothing to skip,
        // and an empty section would cost a row in the table to say so.
        if !ceilings.is_empty() {
            segment.add(kind::BOUNDS, ceilings.finish())?;
        }
        if let (true, Some(store)) = (stored, store) {
            segment.add(kind::FIELDS, store.finish()?)?;
        }
        // Both or neither. A filter without a table cannot answer anything, and
        // a table without a filter would be searched by every lookup for a key
        // that is not in this segment, which is the case the filter is for.
        if let Some((table, bits)) = key_sections {
            segment.add(kind::KEYS, table)?;
            segment.add(kind::KEY_FILTER, bits)?;
        }
        Ok(segment)
    }
}

/// Turns the keys the parts were given into the two sections a segment carries,
/// or nothing at all if no document was named.
///
/// The keys arrive in the order documents were added, which is no order, so they
/// are sorted here. They are sorted by key and then by document descending, so
/// that when a key was used twice the newest document is the one left standing:
/// a batch that updates the same record twice in one segment means the second
/// write, and that is the same rule a lookup across segments follows when it
/// takes the newest segment that holds the key.
///
/// # Errors
///
/// Returns [`Error::Overflow`] if the keys together are more than a four byte
/// offset can address, which is four gigabytes of key in one segment.
fn key_index(mut named: Vec<(Box<[u8]>, DocId)>) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
    if named.is_empty() {
        return Ok(None);
    }
    named.sort_unstable_by(|(left, at), (right, other)| left.cmp(right).then(other.cmp(at)));
    named.dedup_by(|(later, _), (first, _)| later == first);

    let mut table = keys::Writer::new();
    let mut bits = filter::Writer::new(named.len());
    for (key, doc) in &named {
        // The sort put them in order and the dedup made them distinct, so the
        // only thing left that can refuse one is a table too large to address.
        table.push(key, *doc)?;
        bits.insert(key);
    }
    Ok(Some((table.finish()?, bits.finish())))
}

/// A vocabulary and one posting chain per term in it.
#[derive(Debug, Default)]
struct Accumulator {
    vocabulary: Vocabulary,
    arena: Arena,
    /// Where each term's postings are, one entry per term.
    chains: Blocks<Chain>,
    /// What each term has been counted as in the document being indexed, one
    /// entry per term.
    counting: Blocks<Counting>,
    /// The terms counted in the document being counted, so that flushing does
    /// not have to walk the whole vocabulary.
    touched: Vec<u32>,
    scratch: Vec<u8>,
}

impl Accumulator {
    /// Counts one occurrence of a term in the document `stamp` names.
    fn count(&mut self, term: &[u8], stamp: u32) {
        let id = self.vocabulary.intern(term);
        let at = id as usize;
        if at == self.chains.len() {
            let chunk = self.chunk();
            self.chains.push(Chain {
                head: chunk,
                tail: chunk,
                ..Chain::default()
            });
            self.counting.push(Counting::default());
        }
        let counting = &mut self.counting[at];
        if counting.stamp == stamp {
            counting.frequency += 1;
        } else {
            counting.stamp = stamp;
            counting.frequency = 1;
            self.touched.push(id);
        }
    }

    /// Appends everything counted for `doc` to the chains it belongs to.
    fn flush(&mut self, doc: DocId) {
        for index in 0..self.touched.len() {
            let at = self.touched[index] as usize;
            let chain = self.chains[at];
            let gap = if chain.documents == 0 {
                doc
            } else {
                doc - chain.last
            };
            self.append(at, gap, self.counting[at].frequency);
            let chain = &mut self.chains[at];
            chain.documents += 1;
            chain.last = doc;
        }
        self.touched.clear();
    }

    /// Writes one document gap and frequency into a term's chain.
    fn append(&mut self, at: usize, gap: u32, frequency: u32) {
        if self.chains[at].used as usize + MAX_PAIR > PAYLOAD {
            let next = self.chunk();
            let tail = self.chains[at].tail as usize;
            self.arena
                .at_mut(tail, LINK)
                .copy_from_slice(&next.to_le_bytes());
            let chain = &mut self.chains[at];
            chain.tail = next;
            chain.used = 0;
        }
        self.scratch.clear();
        put_uvarint(&mut self.scratch, u64::from(gap));
        put_uvarint(&mut self.scratch, u64::from(frequency));
        let chain = self.chains[at];
        let start = chain.tail as usize + LINK + chain.used as usize;
        let len = self.scratch.len();
        self.arena.at_mut(start, len).copy_from_slice(&self.scratch);
        self.chains[at].used +=
            u32::try_from(self.scratch.len()).expect("a pair is at most ten bytes");
    }

    /// What this is holding, split into the chains and the vocabulary.
    ///
    /// The two arrays indexed by term identifier are counted with the arena
    /// rather than with the vocabulary, because they are per term bookkeeping
    /// for the postings and they grow with the postings. The vocabulary is the
    /// terms and the table that finds them, and nothing else.
    fn held(&self) -> Held {
        let per_term = self.chains.held() + self.counting.held();
        Held {
            postings: self.arena.held()
                + per_term
                + holding::<u32>(self.touched.capacity())
                + holding::<u8>(self.scratch.capacity()),
            vocabulary: self.vocabulary.held(),
            ..Held::default()
        }
    }

    /// Adds an empty chunk and returns where it starts.
    fn chunk(&mut self) -> u32 {
        let at = self.arena.chunk();
        self.arena
            .at_mut(at as usize, LINK)
            .copy_from_slice(&NONE.to_le_bytes());
        at
    }

    /// The vocabulary in term order, as identifiers.
    ///
    /// This is the only sort in an index build. A few hundred thousand terms
    /// against tens of millions of postings is the trade the whole module is
    /// arranged around.
    fn sorted(&self) -> Vec<u32> {
        let mut order: Vec<u32> = (0..self.vocabulary.count()).collect();
        order.sort_unstable_by(|a, b| self.vocabulary.term(*a).cmp(self.vocabulary.term(*b)));
        order
    }

    /// Walks a term's chain onto a posting list, shifting every document by
    /// `base`.
    ///
    /// The shift is what lets a part that numbered its documents from zero sit
    /// after another part in one segment.
    fn walk(
        &self,
        id: u32,
        base: DocId,
        lengths: &[u32],
        writer: &mut posting::Writer,
        ceilings: &mut bound::Writer,
    ) -> Result<()> {
        let at = id as usize;
        let chain = self.chains[at];
        let mut chunk = chain.head as usize;
        let mut offset = 0;
        let mut doc: DocId = 0;
        for index in 0..chain.documents {
            if offset + MAX_PAIR > PAYLOAD {
                let link = self.arena.at(chunk, LINK);
                chunk = u32::from_le_bytes(link.try_into().expect("four bytes")) as usize;
                offset = 0;
            }
            let start = chunk + LINK + offset;
            let rest = self.arena.at(start, chunk + CHUNK - start);
            let (gap, rest) = get_uvarint(rest)?;
            let (frequency, rest) = get_uvarint(rest)?;
            offset = chunk + CHUNK - rest.len() - chunk - LINK;
            let gap = u32::try_from(gap).map_err(|_| Error::NotSorted { at: doc })?;
            doc = if index == 0 { gap } else { doc + gap };
            let shifted = doc.checked_add(base).ok_or(Error::Overflow)?;
            let frequency = u32::try_from(frequency).unwrap_or(u32::MAX);
            writer.push(shifted, frequency)?;
            // The lengths are the part's own, indexed the way the part numbers
            // its documents, which is before the shift and not after it.
            ceilings.push(frequency, lengths.get(doc as usize).copied().unwrap_or(0));
        }
        Ok(())
    }
}

/// Every distinct term seen, and the identifier each one was given.
///
/// This is an open addressed table over one arena of term bytes. It is here
/// rather than being a standard hash map because a map keyed by an owned vector
/// allocates once per distinct term and hashes with a function chosen to resist
/// an attacker rather than to be quick, and neither is what an indexer wants.
#[derive(Debug)]
struct Vocabulary {
    text: Text,
    spans: Blocks<(u32, u32)>,
    /// The slots, which are the one thing here that does double.
    ///
    /// Everything else grows by adding a block, because an entry that has been
    /// written never moves. A slot is where a hash says it is, so growing the
    /// table moves every entry in it and the table has to be built again
    /// whatever shape it is kept in. Paging it would bound nothing and cost a
    /// shift and a mask on the hottest lookup in the writer.
    table: Vec<u32>,
}

impl Default for Vocabulary {
    fn default() -> Self {
        Self {
            text: Text::default(),
            spans: Blocks::default(),
            table: vec![NONE; 1024],
        }
    }
}

impl Vocabulary {
    /// Returns the identifier of a term, giving it one if it is new.
    fn intern(&mut self, term: &[u8]) -> u32 {
        let mask = self.table.len() - 1;
        let mut at = hash(term) as usize & mask;
        loop {
            let found = self.table[at];
            if found == NONE {
                break;
            }
            if self.term(found) == term {
                return found;
            }
            at = (at + 1) & mask;
        }
        let id = u32::try_from(self.spans.len()).expect("under four billion distinct terms");
        let start = self.text.push(term);
        self.spans.push((
            start,
            u32::try_from(term.len()).expect("terms are capped at 255 bytes"),
        ));
        self.table[at] = id;
        if self.spans.len() * 4 > self.table.len() * 3 {
            self.grow();
        }
        id
    }

    /// Doubles the table and reinserts everything.
    fn grow(&mut self) {
        let mut table = vec![NONE; self.table.len() * 2];
        let mask = table.len() - 1;
        for id in 0..self.spans.len() {
            let id = u32::try_from(id).expect("the identifiers came from a length");
            let mut at = hash(self.term(id)) as usize & mask;
            while table[at] != NONE {
                at = (at + 1) & mask;
            }
            table[at] = id;
        }
        self.table = table;
    }

    /// The bytes of a term.
    fn term(&self, id: u32) -> &[u8] {
        let (start, len) = self.spans[id as usize];
        self.text.at(start as usize, len as usize)
    }

    /// How many distinct terms there are.
    fn count(&self) -> u32 {
        u32::try_from(self.spans.len()).expect("under four billion distinct terms")
    }

    /// What the terms and the table that finds them are costing.
    fn held(&self) -> u64 {
        self.text.held() + self.spans.held() + holding::<u32>(self.table.capacity())
    }
}

/// Hashes a term.
///
/// Eight bytes at a time with a multiply and a shift. It is not a hash anybody
/// should use against untrusted keys, and it does not need to be: the keys are
/// words out of documents this process is indexing, and the table is thrown away
/// when the segment is written.
#[expect(
    clippy::cast_possible_truncation,
    reason = "folding the sixty four bit state down to the thirty two bits a slot \
              index needs is the whole point of the last line"
)]
fn hash(bytes: &[u8]) -> u32 {
    const MIX: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut state = 0xcbf2_9ce4_8422_2325 ^ (bytes.len() as u64);
    let (chunks, rest) = bytes.as_chunks::<8>();
    for chunk in chunks {
        state = (state ^ u64::from_le_bytes(*chunk)).wrapping_mul(MIX);
        state ^= state >> 29;
    }
    if !rest.is_empty() {
        let mut last = [0u8; 8];
        last[..rest.len()].copy_from_slice(rest);
        state = (state ^ u64::from_le_bytes(last)).wrapping_mul(MIX);
        state ^= state >> 29;
    }
    (state ^ (state >> 32)) as u32
}

/// The mean document length, in the one place both the writer and the reader can
/// call so that the two cannot drift apart.
///
/// A bound written against one denominator and read against another is not a
/// bound, so this being one function rather than two copies of a division is the
/// whole of what keeps them honest.
#[expect(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    reason = "an average over a corpus is wanted to a few significant figures, \
              and the division runs in f64 so that only the result is narrowed"
)]
pub(crate) fn average(total: u64, documents: u32) -> f32 {
    if documents == 0 {
        return 0.0;
    }
    (total as f64 / f64::from(documents)) as f32
}

/// The primary keys of one segment, and the filter in front of them.
///
/// This is separate from [`Reader`] because a lookup by key across a store opens
/// every segment and asks each of them, and all but one of those questions is
/// answered no by the filter. Opening a whole index to ask is decoding a term
/// dictionary header, a norms section and whatever else is there, all so that
/// sixty four bytes can be read and the segment dismissed.
#[derive(Debug)]
pub struct Keys<'a> {
    table: keys::Reader<'a>,
    filter: filter::Reader<'a>,
}

impl<'a> Keys<'a> {
    /// Opens the key index of a segment, or `None` if it does not have one.
    ///
    /// A segment without keys is ordinary rather than damaged. Nothing forces a
    /// document to be named, a build older than the sections did not write them,
    /// and a segment of documents that were all added without a key has nothing
    /// to put in one.
    ///
    /// # Errors
    ///
    /// Returns a decoding error if either section is not what it claims to be,
    /// and [`Error::MissingSection`] if one of the two is there and the other is
    /// not, which is a segment written by something that did not finish.
    pub fn open(segment: &Segment<'a>) -> Result<Option<Self>> {
        match (
            segment.section(kind::KEYS),
            segment.section(kind::KEY_FILTER),
        ) {
            (Some(table), Some(bits)) => Ok(Some(Self {
                table: keys::Reader::new(table)?,
                filter: filter::Reader::new(bits)?,
            })),
            (None, None) => Ok(None),
            (Some(_), None) => Err(Error::MissingSection {
                kind: kind::KEY_FILTER,
            }),
            (None, Some(_)) => Err(Error::MissingSection { kind: kind::KEYS }),
        }
    }

    /// How many keys the segment holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.table.len()
    }

    /// Whether it holds none, which no segment written by this build does.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.table.is_empty()
    }

    /// Whether the key might be here, from the filter alone.
    ///
    /// False means it is certainly not, which is the answer worth having: a key
    /// lives in one segment and every other segment in the store has to say no,
    /// and this says it for the price of one cache line.
    ///
    /// True means it is probably here, at about one in a hundred wrong, and the
    /// caller has to look in the table to find out.
    #[must_use]
    pub fn maybe_holds(&self, key: &[u8]) -> bool {
        self.filter.maybe_holds(key)
    }

    /// The document a key names, or `None` if this segment does not name it.
    ///
    /// Deleted documents are not considered here, because this does not know
    /// what has been deleted. [`Reader::document`] is the one that does.
    #[must_use]
    pub fn get(&self, key: &[u8]) -> Option<DocId> {
        if !self.filter.maybe_holds(key) {
            return None;
        }
        self.table.get(key)
    }

    /// The table underneath, for a caller walking every key rather than asking
    /// after one.
    #[must_use]
    pub const fn table(&self) -> &keys::Reader<'a> {
        &self.table
    }
}

/// Reads an index out of a segment.
#[derive(Debug)]
pub struct Reader<'a> {
    terms: terms::Reader<'a>,
    postings: &'a [u8],
    lengths: &'a [u8],
    bounds: Option<bound::Reader<'a>>,
    store: Option<store::Reader<'a>>,
    keys: Option<Keys<'a>>,
    documents: u32,
    total: u64,
    /// Which of this segment's documents have been deleted, if any.
    ///
    /// A segment is immutable and a deletion is not, so the set lives beside the
    /// segment rather than in it, and a reader is told about it rather than
    /// finding it. Nothing here removes a posting: a deleted document is still
    /// in every list it was in, still has a length, and still has an identifier
    /// that the numbering across segments depends on. What changes is that it is
    /// no longer an answer.
    deleted: Option<Bitmap>,
}

impl<'a> Reader<'a> {
    /// Opens the index in a segment.
    ///
    /// # Errors
    ///
    /// Returns [`Error::MissingSection`] if any of the three sections an index
    /// needs is not there, and a decoding error if one of them is not what it
    /// claims to be.
    pub fn open(segment: &Segment<'a>) -> Result<Self> {
        let dictionary = section(segment, kind::TERMS)?;
        let postings = section(segment, kind::POSTINGS)?;
        let norms = section(segment, kind::NORMS)?;
        let (documents, rest) = get_u32(norms)?;
        let (total, lengths) = get_u64(rest)?;
        let needed = documents as usize * 4;
        if lengths.len() < needed {
            return Err(Error::Truncated {
                needed,
                available: lengths.len(),
            });
        }
        let store = match segment.section(kind::FIELDS) {
            Some(bytes) => Some(store::Reader::new(bytes)?),
            None => None,
        };
        // Not every segment has one. A build that predates the section leaves it
        // out, and so does a segment whose lists are all shorter than a block, so
        // its absence is an ordinary case rather than a damaged file.
        let bounds = match segment.section(kind::BOUNDS) {
            Some(bytes) => Some(bound::Reader::new(bytes)?),
            None => None,
        };
        Ok(Self {
            terms: terms::Reader::new(dictionary)?,
            postings,
            lengths,
            bounds,
            store,
            keys: Keys::open(segment)?,
            documents,
            total,
            deleted: None,
        })
    }

    /// Hides the documents in `deleted` from everything that asks this reader a
    /// question.
    ///
    /// The set is the segment's own numbering, which is what a tombstone bitmap
    /// beside a segment holds, and it replaces whatever the reader was hiding
    /// before rather than adding to it. That is what makes a newer generation of
    /// tombstones simply a newer set.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NoSuchDocument`] if the set names a document this
    /// segment does not have. That is a manifest pointing at the wrong bitmap or
    /// a bitmap left over from a build with more documents in it, and either way
    /// the deletions being applied are not this segment's, so applying the part
    /// of them that happens to fit would hide the wrong documents.
    pub fn hide(&mut self, deleted: Bitmap) -> Result<()> {
        if let Some(doc) = deleted.max()
            && doc >= self.documents
        {
            return Err(Error::NoSuchDocument {
                doc,
                documents: self.documents,
            });
        }
        self.deleted = (!deleted.is_empty()).then_some(deleted);
        Ok(())
    }

    /// The same thing [`hide`](Self::hide) does, for a reader being built up.
    ///
    /// # Errors
    ///
    /// The same one, for the same reason.
    pub fn hiding(mut self, deleted: Bitmap) -> Result<Self> {
        self.hide(deleted)?;
        Ok(self)
    }

    /// Which of this segment's documents are deleted, if any were.
    #[must_use]
    pub const fn deleted(&self) -> Option<&Bitmap> {
        self.deleted.as_ref()
    }

    /// Whether anything in this segment has been deleted.
    ///
    /// Worth asking on its own, because a walk over a segment with nothing
    /// deleted keeps every shortcut that counts documents without looking at
    /// them, and a walk over one with a deletion in it cannot.
    #[must_use]
    pub const fn any_deleted(&self) -> bool {
        self.deleted.is_some()
    }

    /// Whether a document is still an answer.
    ///
    /// True for an identifier past the end of the segment, which cannot come out
    /// of a posting list and so is not worth an answer of its own.
    #[must_use]
    pub fn is_live(&self, doc: DocId) -> bool {
        self.deleted.as_ref().is_none_or(|gone| !gone.contains(doc))
    }

    /// How many documents the index holds, deleted ones included.
    ///
    /// This is the width of the segment's numbering rather than a count of
    /// answers, which is why deleting does not move it. A hit carries an
    /// identifier that says which document it is, and that identifier is worked
    /// out by adding up the segments before it, so a number that moved when
    /// something was deleted would move every hit in every segment after it.
    #[must_use]
    pub const fn documents(&self) -> u32 {
        self.documents
    }

    /// How many of them are still answers.
    #[must_use]
    pub fn live(&self) -> u32 {
        let gone = self.deleted.as_ref().map_or(0, Bitmap::len);
        self.documents
            .saturating_sub(u32::try_from(gone).unwrap_or(u32::MAX))
    }

    /// Whether the index holds no documents.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.documents == 0
    }

    /// How many distinct terms the index holds.
    #[must_use]
    pub const fn terms(&self) -> u32 {
        self.terms.len()
    }

    /// How many terms a document holds, or zero if there is no such document.
    #[must_use]
    pub fn length(&self, doc: DocId) -> u32 {
        if doc >= self.documents {
            return 0;
        }
        self.lengths
            .get(doc as usize * 4..)
            .and_then(<[u8]>::first_chunk::<4>)
            .map_or(0, |bytes| u32::from_le_bytes(*bytes))
    }

    /// How many terms the index holds across all of its documents.
    ///
    /// The numerator of the mean length, and what a search across several
    /// segments needs. Such a search cannot average the segments' averages,
    /// because a mean of means is only the mean when the parts are the same
    /// size, and segments are not the same size.
    #[must_use]
    pub const fn total_length(&self) -> u64 {
        self.total
    }

    /// The mean document length, which is the denominator BM25 normalises by.
    #[must_use]
    pub fn average_length(&self) -> f32 {
        average(self.total, self.documents)
    }

    /// What each block of postings can score at best, if the segment says.
    ///
    /// Nothing that reads this is allowed to need it. A segment written before
    /// the section existed does not have one, and neither does a segment of
    /// short lists, so a caller that cannot fall back on a looser bound has a
    /// bug rather than a missing section.
    #[must_use]
    pub const fn bounds(&self) -> Option<&bound::Reader<'a>> {
        self.bounds.as_ref()
    }

    /// The stored fields, or nothing if the index was built without any.
    #[must_use]
    pub const fn store(&self) -> Option<&store::Reader<'a>> {
        self.store.as_ref()
    }

    /// The keys of this segment, or nothing if no document in it was named.
    #[must_use]
    pub const fn keys(&self) -> Option<&Keys<'a>> {
        self.keys.as_ref()
    }

    /// The document a key names, or `None` if this segment has no live document
    /// under it.
    ///
    /// Deleted is the same answer as absent on purpose. A key naming a document
    /// somebody deleted is a key nothing holds any more, and a caller that had
    /// to tell the two apart would be reasoning about a document it can no
    /// longer read.
    ///
    /// A segment that was written without keys answers `None` to everything,
    /// which is the same thing it would answer for a key it does not hold.
    #[must_use]
    pub fn document(&self, key: &[u8]) -> Option<DocId> {
        let doc = self.keys.as_ref()?.get(key)?;
        self.is_live(doc).then_some(doc)
    }

    /// The posting list of a term, or nothing if the term is not in the index.
    ///
    /// # Errors
    ///
    /// Returns an error if the dictionary or the posting list does not decode.
    pub fn postings(&self, term: &[u8]) -> Result<Option<posting::Reader<'a>>> {
        match self.entry(term)? {
            Some(entry) => self.list(entry).map(Some),
            None => Ok(None),
        }
    }

    /// What the dictionary holds for a term, or nothing if it does not hold it.
    ///
    /// [`postings`](Self::postings) is this and [`list`](Self::list) together,
    /// and is what most callers want. This is for the one that also needs where
    /// the list sits, because that offset is how the ceilings are keyed.
    ///
    /// # Errors
    ///
    /// Returns an error if the dictionary does not decode.
    pub fn entry(&self, term: &[u8]) -> Result<Option<terms::Entry>> {
        self.terms.get(term)
    }

    /// Walks every term in the index, in order.
    ///
    /// Paired with [`list`](Self::list), which takes the entry a walk hands
    /// back without looking the term up a second time. Together they are what a
    /// tool reading the whole index needs, and nothing a query does.
    #[must_use]
    pub const fn entries(&self) -> terms::Entries<'a> {
        self.terms.entries()
    }

    /// The posting list an entry points at.
    ///
    /// # Errors
    ///
    /// Returns [`Error::SectionOutOfRange`] if the entry points outside the
    /// postings section, and a decoding error if the list it points at is not a
    /// posting list.
    pub fn list(&self, entry: terms::Entry) -> Result<posting::Reader<'a>> {
        let out_of_range = || Error::SectionOutOfRange {
            kind: kind::POSTINGS,
            offset: entry.offset,
            length: entry.len,
        };
        let start = usize::try_from(entry.offset).map_err(|_| out_of_range())?;
        let len = usize::try_from(entry.len).map_err(|_| out_of_range())?;
        let end = start.checked_add(len).ok_or_else(out_of_range)?;
        if end > self.postings.len() {
            return Err(out_of_range());
        }
        posting::Reader::new(&self.postings[start..end])
    }
}

/// Takes a section out of a segment, or says which one is missing.
fn section<'a>(segment: &Segment<'a>, want: u16) -> Result<&'a [u8]> {
    segment
        .section(want)
        .ok_or(Error::MissingSection { kind: want })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A corpus small enough to check by hand and varied enough to exercise the
    /// paths that matter.
    const DOCS: [&str; 4] = [
        "the quick brown fox jumps over the lazy dog",
        "the dog barks",
        "quick quick quick",
        "nothing in common with the others except a stop word",
    ];

    fn build(docs: &[&str]) -> Vec<u8> {
        let mut writer = Writer::new();
        for doc in docs {
            writer.add(doc).expect("a handful of documents fit");
        }
        writer.finish().expect("what was written decodes")
    }

    #[test]
    fn stored_fields_come_back_with_the_document_they_went_in_with() {
        let mut writer = Writer::new();
        writer
            .add_with_fields("the quick brown fox", [("id", &b"a"[..]), ("n", &b"1"[..])])
            .expect("adds");
        writer
            .add_with_fields("the lazy dog", [("id", &b"b"[..]), ("n", &b"2"[..])])
            .expect("adds");
        let bytes = writer.finish().expect("finishes");
        let segment = Segment::open(&bytes).expect("opens");
        let index = Reader::open(&segment).expect("opens");
        let store = index.store().expect("the fields were stored");
        assert_eq!(store.len(), 2);
        let mut scratch = store::Scratch::new();
        for (doc, want) in [(0, &b"a"[..]), (1, &b"b"[..])] {
            let fields = store.get(doc, &mut scratch).expect("reads");
            assert_eq!(fields.field("id").expect("reads"), Some(want));
        }
    }

    #[test]
    fn an_index_that_stores_nothing_carries_no_store() {
        // Eight bytes a document for an empty record is eight bytes a document
        // nobody asked for.
        let mut writer = Writer::new();
        writer.add("the quick brown fox").expect("adds");
        let bytes = writer.finish().expect("finishes");
        let segment = Segment::open(&bytes).expect("opens");
        assert!(segment.section(kind::FIELDS).is_none());
        assert!(Reader::open(&segment).expect("opens").store().is_none());
    }

    #[test]
    fn a_segment_built_in_parts_is_the_segment_built_in_one() {
        // The property the whole fold rests on. Whether a corpus went through
        // one writer or four is not something a reader should be able to tell,
        // beyond the store packing its blocks at different boundaries.
        let docs: Vec<String> = (0..400)
            .map(|i| format!("document {i} the quick brown fox term{} shared", i % 37))
            .collect();

        let mut whole = Writer::new();
        for text in &docs {
            whole
                .add_with_fields(text, [("n", text.as_bytes())])
                .expect("adds");
        }
        let one = whole.finish().expect("finishes");

        let mut parts = Vec::new();
        for slice in docs.chunks(97) {
            let mut part = Writer::new();
            for text in slice {
                part.add_with_fields(text, [("n", text.as_bytes())])
                    .expect("adds");
            }
            parts.push(part);
        }
        let many = Writer::concat(parts).expect("folds");

        let left = Segment::open(&one).expect("opens");
        let right = Segment::open(&many).expect("opens");
        for kind in [kind::TERMS, kind::POSTINGS, kind::NORMS] {
            assert_eq!(
                left.section(kind),
                right.section(kind),
                "section {kind} differs"
            );
        }

        let left = Reader::open(&left).expect("opens");
        let right = Reader::open(&right).expect("opens");
        assert_eq!(left.documents(), right.documents());
        assert_eq!(left.terms(), right.terms());
        let mut scratch = store::Scratch::new();
        for doc in 0..right.documents() {
            assert_eq!(left.length(doc), right.length(doc));
            let store = right.store().expect("the fields were stored");
            assert_eq!(
                store
                    .get(doc, &mut scratch)
                    .expect("reads")
                    .field("n")
                    .expect("decodes"),
                Some(docs[doc as usize].as_bytes()),
                "document {doc}"
            );
        }
    }

    #[test]
    fn folding_parts_that_hold_nothing_changes_nothing() {
        let mut part = Writer::new();
        part.add("the only document there is").expect("adds");
        let alone = part.finish().expect("finishes");

        let mut part = Writer::new();
        part.add("the only document there is").expect("adds");
        let folded = Writer::concat(vec![Writer::new(), part, Writer::new()]).expect("folds");
        assert_eq!(alone, folded);
    }

    #[test]
    fn a_fold_of_nothing_is_a_segment() {
        let bytes = Writer::concat(Vec::new()).expect("folds");
        let segment = Segment::open(&bytes).expect("opens");
        let index = Reader::open(&segment).expect("opens");
        assert!(index.is_empty());
        assert_eq!(index.terms(), 0);
    }

    #[test]
    fn a_term_only_one_part_has_still_finds_its_documents() {
        let mut first = Writer::new();
        first.add("alpha beta").expect("adds");
        let mut second = Writer::new();
        second.add("gamma").expect("adds");
        second.add("beta gamma").expect("adds");
        let bytes = Writer::concat(vec![first, second]).expect("folds");
        let segment = Segment::open(&bytes).expect("opens");
        let index = Reader::open(&segment).expect("opens");
        for (term, want) in [
            (&b"alpha"[..], vec![0]),
            (&b"beta"[..], vec![0, 2]),
            (&b"gamma"[..], vec![1, 2]),
        ] {
            let seen = index
                .postings(term)
                .expect("decodes")
                .expect("the term is there")
                .to_vec()
                .expect("the list decodes");
            assert_eq!(seen, want, "{}", String::from_utf8_lossy(term));
        }
    }

    #[test]
    fn every_term_finds_the_documents_it_was_in() {
        let bytes = build(&DOCS);
        let segment = Segment::open(&bytes).expect("a segment this writer wrote opens");
        let index = Reader::open(&segment).expect("the sections are all there");

        let docs = |term: &str| {
            index
                .postings(term.as_bytes())
                .expect("the term decodes")
                .expect("the term is in the index")
                .to_vec()
                .expect("the list decodes")
        };
        assert_eq!(docs("the"), [0, 1, 3]);
        assert_eq!(docs("dog"), [0, 1]);
        assert_eq!(docs("quick"), [0, 2]);
        assert_eq!(docs("fox"), [0]);
        assert_eq!(docs("barks"), [1]);
    }

    #[test]
    fn a_term_that_is_not_there_is_not_found() {
        let bytes = build(&DOCS);
        let segment = Segment::open(&bytes).expect("opens");
        let index = Reader::open(&segment).expect("opens");
        assert!(index.postings(b"aardvark").expect("decodes").is_none());
        assert!(index.postings(b"").expect("decodes").is_none());
    }

    #[test]
    fn frequencies_survive_the_round_trip() {
        let bytes = build(&DOCS);
        let segment = Segment::open(&bytes).expect("opens");
        let index = Reader::open(&segment).expect("opens");
        let quick = index
            .postings(b"quick")
            .expect("decodes")
            .expect("is there")
            .to_postings()
            .expect("decodes");
        assert_eq!(quick, [(0, 1), (2, 3)]);
        let the = index
            .postings(b"the")
            .expect("decodes")
            .expect("is there")
            .to_postings()
            .expect("decodes");
        assert_eq!(the, [(0, 2), (1, 1), (3, 1)]);
    }

    #[test]
    fn lengths_and_the_average_are_what_was_indexed() {
        let bytes = build(&DOCS);
        let segment = Segment::open(&bytes).expect("opens");
        let index = Reader::open(&segment).expect("opens");
        assert_eq!(index.documents(), 4);
        assert_eq!(index.length(0), 9);
        assert_eq!(index.length(1), 3);
        assert_eq!(index.length(2), 3);
        assert_eq!(index.length(3), 10);
        assert_eq!(index.length(4), 0);
        assert!((index.average_length() - 6.25).abs() < 1e-6);
    }

    #[test]
    fn an_index_with_no_documents_is_valid() {
        let bytes = build(&[]);
        let segment = Segment::open(&bytes).expect("opens");
        let index = Reader::open(&segment).expect("opens");
        assert_eq!(index.documents(), 0);
        assert_eq!(index.terms(), 0);
        assert!(index.is_empty());
        assert!((index.average_length() - 0.0).abs() < 1e-6);
        assert!(index.postings(b"anything").expect("decodes").is_none());
    }

    #[test]
    fn a_document_with_no_terms_still_counts() {
        let bytes = build(&["hello", "   ,,,   ", "hello"]);
        let segment = Segment::open(&bytes).expect("opens");
        let index = Reader::open(&segment).expect("opens");
        assert_eq!(index.documents(), 3);
        assert_eq!(index.length(1), 0);
        assert_eq!(
            index
                .postings(b"hello")
                .expect("decodes")
                .expect("is there")
                .to_vec()
                .expect("decodes"),
            [0, 2]
        );
    }

    #[test]
    fn a_chain_that_runs_over_many_chunks_decodes() {
        // A term in every one of ten thousand documents is a chain hundreds of
        // chunks long, which is the only way to find out whether the link
        // following is right.
        let docs: Vec<String> = (0..10_000).map(|i| format!("common term{i}")).collect();
        let refs: Vec<&str> = docs.iter().map(String::as_str).collect();
        let bytes = build(&refs);
        let segment = Segment::open(&bytes).expect("opens");
        let index = Reader::open(&segment).expect("opens");
        let common = index
            .postings(b"common")
            .expect("decodes")
            .expect("is there")
            .to_vec()
            .expect("decodes");
        assert_eq!(common.len(), 10_000);
        assert!(common.iter().copied().eq(0..10_000));
        assert_eq!(index.terms(), 10_001);
    }

    #[test]
    fn a_term_in_a_document_far_from_the_last_one_keeps_its_gap() {
        // Gaps are what the chain stores, so a term that skips most of the
        // corpus is the case that catches an off by one in the delta.
        let mut docs: Vec<String> = (0..1_000).map(|i| format!("filler{i}")).collect();
        docs[0] = "rare filler0".to_string();
        docs[500] = "rare filler500".to_string();
        docs[999] = "rare filler999".to_string();
        let refs: Vec<&str> = docs.iter().map(String::as_str).collect();
        let bytes = build(&refs);
        let segment = Segment::open(&bytes).expect("opens");
        let index = Reader::open(&segment).expect("opens");
        assert_eq!(
            index
                .postings(b"rare")
                .expect("decodes")
                .expect("is there")
                .to_vec()
                .expect("decodes"),
            [0, 500, 999]
        );
    }

    #[test]
    fn the_vocabulary_grows_without_losing_a_term() {
        // Past the load factor the table is rebuilt, and a term lost in the
        // rebuild would be a term that silently stops matching.
        let docs: Vec<String> = (0..20_000).map(|i| format!("term{i} shared")).collect();
        let refs: Vec<&str> = docs.iter().map(String::as_str).collect();
        let bytes = build(&refs);
        let segment = Segment::open(&bytes).expect("opens");
        let index = Reader::open(&segment).expect("opens");
        assert_eq!(index.terms(), 20_001);
        for i in (0..20_000).step_by(97) {
            let term = format!("term{i}");
            assert_eq!(
                index
                    .postings(term.as_bytes())
                    .expect("decodes")
                    .expect("every term that went in comes back")
                    .to_vec()
                    .expect("decodes"),
                [i]
            );
        }
    }

    #[test]
    fn a_truncated_segment_is_an_error_not_a_panic() {
        let bytes = build(&DOCS);
        for cut in 0..bytes.len() {
            let Ok(segment) = Segment::open_without_checksum(&bytes[..cut]) else {
                continue;
            };
            let Ok(index) = Reader::open(&segment) else {
                continue;
            };
            let _ = index.postings(b"the");
            let _ = index.length(0);
        }
    }

    #[test]
    fn the_arena_hands_out_each_chunk_once_and_keeps_them_apart() {
        // Enough to cross several blocks, because the failure this is looking
        // for is two chunks in different blocks landing on the same offset,
        // which nothing inside a single block can show.
        let wanted = 4 * BLOCK / CHUNK + 3;
        let mut arena = Arena::default();
        let handed: Vec<u32> = (0..wanted).map(|_| arena.chunk()).collect();

        let mut seen: Vec<u32> = handed.clone();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), handed.len(), "a chunk was handed out twice");

        // Written to through the accessor and read back through it, so that an
        // offset that decoded to the wrong block would show up as somebody
        // else's bytes rather than as nothing at all.
        for (nth, at) in handed.iter().enumerate() {
            let mark = u32::try_from(nth).expect("a few thousand");
            arena
                .at_mut(*at as usize, LINK)
                .copy_from_slice(&mark.to_le_bytes());
        }
        for (nth, at) in handed.iter().enumerate() {
            let bytes = arena.at(*at as usize, LINK);
            let read = u32::from_le_bytes(bytes.try_into().expect("four bytes"));
            assert_eq!(read as usize, nth, "chunk {nth} at {at} read back wrong");
        }
    }

    #[test]
    fn a_chain_that_runs_over_many_blocks_decodes() {
        // One term in every document, so its chain is the longest a corpus of
        // this size can produce, and enough documents that the chain alone runs
        // past a block. Every other term is there to push the chain's chunks
        // apart, because a chain whose chunks happen to be consecutive is a
        // chain that would decode even if the links were ignored.
        let documents = 8 * BLOCK / CHUNK;
        let mut writer = Writer::new();
        for doc in 0..documents {
            writer.add(&format!("common d{doc} d{doc}")).expect("adds");
        }
        let bytes = writer.finish().expect("finishes");
        let segment = Segment::open(&bytes).expect("opens");
        let index = Reader::open(&segment).expect("opens");

        let found = index
            .postings(b"common")
            .expect("decodes")
            .expect("the term is there")
            .to_postings()
            .expect("decodes");
        let wanted: Vec<(DocId, u32)> = (0..documents)
            .map(|doc| (DocId::try_from(doc).expect("a few thousand"), 1))
            .collect();
        assert_eq!(found, wanted);
    }

    #[test]
    fn a_writer_that_has_been_given_nothing_is_not_free() {
        // The compressor's match table is made when the writer is and is the
        // same size forever after, so a caller that budgeted from zero would be
        // budgeting from a number that was never true.
        let writer = Writer::new();
        assert!(writer.held().stored > 0, "the match table is already there");
    }

    #[test]
    fn what_a_writer_holds_grows_with_what_goes_into_it() {
        let mut writer = Writer::new();
        let empty = writer.held().total();
        for _ in 0..64 {
            for doc in DOCS {
                writer.add(doc).expect("adds");
            }
        }
        let full = writer.held();
        assert!(
            full.total() > empty,
            "{} is not more than {empty}",
            full.total()
        );
        assert!(full.postings > 0, "the postings are somewhere");
        assert!(full.vocabulary > 0, "so is the vocabulary");
        assert!(full.lengths > 0, "and a length a document");
    }

    #[test]
    fn the_parts_of_what_a_writer_holds_add_up_to_the_total() {
        // The point of the split is that it accounts for the whole of what is
        // held, so a part that went missing would be a part nobody knew to look
        // for.
        let mut writer = Writer::new();
        for doc in DOCS {
            writer
                .add_with_fields(doc, [("path", &b"a/b/c"[..])])
                .expect("adds");
        }
        let held = writer.held();
        assert_eq!(
            held.total(),
            held.postings + held.vocabulary + held.stored + held.lengths
        );
    }

    #[test]
    fn storing_fields_shows_up_against_the_fields_and_not_the_postings() {
        let mut bare = Writer::new();
        let mut stored = Writer::new();
        for doc in DOCS {
            bare.add(doc).expect("adds");
            stored
                .add_with_fields(doc, [("path", doc.as_bytes())])
                .expect("adds");
        }
        let bare = bare.held();
        let stored = stored.held();
        assert_eq!(bare.postings, stored.postings, "the same text was indexed");
        assert_eq!(bare.vocabulary, stored.vocabulary);
        assert!(stored.stored > bare.stored, "the fields are held somewhere");
    }

    #[test]
    fn a_fresh_writer_holds_what_the_one_it_replaced_let_go_of() {
        // Which is what makes a flush a way of bounding memory rather than a way
        // of cutting a file up. If the writer that is thrown away did not take
        // its postings with it there would be nothing to flush for.
        let mut writer = Writer::new();
        for _ in 0..64 {
            for doc in DOCS {
                writer.add(doc).expect("adds");
            }
        }
        let before = writer.held().total();
        let after = core::mem::replace(&mut writer, Writer::new()).finish();
        after.expect("finishes");
        assert!(
            writer.held().total() < before,
            "a writer that has been replaced still holds what the old one did"
        );
    }

    #[test]
    fn a_writer_nobody_gave_a_budget_to_is_never_full() {
        let mut writer = Writer::new();
        assert_eq!(writer.budget(), None);
        for _ in 0..256 {
            for doc in DOCS {
                writer.add(doc).expect("adds");
            }
        }
        assert!(writer.held().total() > 0);
        assert!(!writer.is_full(), "a writer with no budget filled up");
    }

    #[test]
    fn a_writer_says_it_is_full_once_it_holds_what_it_was_told_it_may() {
        // Small enough that a handful of documents reaches it, since what is
        // being tested is the comparison and not the size of anything.
        // Over the floor a writer costs before it has been given anything, and
        // small enough that a handful of documents crosses it.
        let budget = Writer::new().held().total() + (64 << 10);
        let mut writer = Writer::with_budget(budget);
        assert_eq!(writer.budget(), Some(budget));
        assert!(!writer.is_full(), "an empty writer is full");

        let mut added = 0;
        while !writer.is_full() {
            for doc in DOCS {
                writer.add(doc).expect("adds");
                added += 1;
            }
            assert!(added < 100_000, "the writer never filled up");
        }
        assert!(writer.held().total() >= budget);

        // And a fresh one with the same budget starts empty, which is what makes
        // this a bound on a run rather than on a segment.
        let held = writer.held().total();
        let next = core::mem::replace(&mut writer, Writer::with_budget(budget));
        next.finish().expect("finishes");
        assert!(!writer.is_full());
        assert!(writer.held().total() < held);
        assert_eq!(writer.budget(), Some(budget));
    }

    #[test]
    fn what_one_document_adds_to_what_a_writer_holds_does_not_grow_with_the_corpus() {
        // This is what a budget is worth. A writer that doubled its per term
        // arrays took a step the size of everything it already held, so on a
        // vocabulary of two hundred thousand terms one document could take it
        // 4.7 MB past whatever it had been told it may hold, and on a larger
        // one further still. Every array a term is counted in now grows by a
        // block, so the largest step is a handful of blocks and it is the same
        // handful at the end of a corpus as at the start.
        //
        // Forty thousand terms is enough to tell the two apart without being
        // enough to slow a test run down. The worst step here was 1.13 MB
        // before and is 416.4 KB after, and only the second of those stays put
        // as the corpus grows.
        let mut writer = Writer::new();
        let mut worst = 0;
        let mut held = writer.held().total();
        for doc in 0..40_000u32 {
            // A term nothing else holds, so every document grows every array.
            writer
                .add(&format!("term{doc} the quick brown fox"))
                .expect("adds");
            let now = writer.held().total();
            worst = worst.max(now - held);
            held = now;
        }
        assert!(
            worst < (512 << 10),
            "one document took the writer {worst} bytes further"
        );
    }

    #[test]
    fn a_budget_under_what_an_empty_writer_costs_is_full_from_the_first_document() {
        // The edge somebody will pass eventually. A writer holds the
        // compressor's match table before it has seen a document, so a budget
        // under that is a budget of one document per segment, and the thing that
        // must not happen is a loop that flushes an empty writer forever.
        let floor = Writer::new().held().total();
        assert!(floor > 0);
        let mut writer = Writer::with_budget(floor / 2);
        assert!(writer.is_full(), "a budget under the floor has room");
        writer.add(DOCS[0]).expect("adds");
        assert!(writer.is_full());
        assert_eq!(writer.len(), 1);
    }

    #[test]
    fn a_reader_told_what_is_gone_says_which_documents_are_left() {
        let bytes = build(&DOCS);
        let segment = Segment::open(&bytes).expect("opens");
        let mut index = Reader::open(&segment).expect("opens");
        assert!(!index.any_deleted());
        assert_eq!(index.deleted(), None);
        assert_eq!(index.live(), 4);
        assert!((0..4).all(|doc| index.is_live(doc)));

        index.hide(Bitmap::from_sorted(&[1, 3])).expect("hides");
        assert!(index.any_deleted());
        assert_eq!(index.deleted().map(Bitmap::len), Some(2));
        assert_eq!(index.live(), 2);
        assert_eq!(
            (0..4).filter(|&doc| index.is_live(doc)).collect::<Vec<_>>(),
            [0, 2]
        );
        // The count the numbering is built from does not move, because it is
        // about how many documents the segment was written with.
        assert_eq!(index.documents(), 4);
    }

    #[test]
    fn hiding_nothing_leaves_the_reader_as_it_was() {
        // Worth saying out loud, because every shortcut that counts documents
        // without decoding them is turned off by a set being present rather
        // than by it holding anything.
        let bytes = build(&DOCS);
        let segment = Segment::open(&bytes).expect("opens");
        let index = Reader::open(&segment)
            .expect("opens")
            .hiding(Bitmap::new())
            .expect("hides nothing");
        assert!(!index.any_deleted());
        assert_eq!(index.live(), 4);
    }

    #[test]
    fn hiding_again_replaces_what_was_hidden_rather_than_adding_to_it() {
        // A set of deletions is the whole answer for a segment at one moment,
        // not a change to be applied, so handing over a smaller one brings
        // documents back.
        let bytes = build(&DOCS);
        let segment = Segment::open(&bytes).expect("opens");
        let mut index = Reader::open(&segment).expect("opens");
        index.hide(Bitmap::from_sorted(&[0, 1, 2])).expect("hides");
        assert_eq!(index.live(), 1);
        index.hide(Bitmap::from_sorted(&[2])).expect("hides");
        assert_eq!(index.live(), 3);
        assert!(index.is_live(0));
        index.hide(Bitmap::new()).expect("hides");
        assert_eq!(index.live(), 4);
        assert!(!index.any_deleted());
    }

    #[test]
    fn a_deletion_naming_a_document_that_is_not_there_is_refused() {
        // The set and the segment came from different builds of the store, and
        // going on would hide whichever documents happen to hold those numbers
        // now, which is a wrong answer with nothing to notice it by.
        let bytes = build(&DOCS);
        let segment = Segment::open(&bytes).expect("opens");
        let mut index = Reader::open(&segment).expect("opens");
        let error = index
            .hide(Bitmap::from_sorted(&[0, 4]))
            .expect_err("four documents are numbered zero to three");
        assert!(matches!(
            error,
            Error::NoSuchDocument {
                doc: 4,
                documents: 4
            }
        ));
        // And it was refused rather than half applied.
        assert!(!index.any_deleted());
        assert_eq!(index.live(), 4);
    }

    #[test]
    fn every_document_deleted_leaves_a_reader_that_still_answers() {
        let bytes = build(&DOCS);
        let segment = Segment::open(&bytes).expect("opens");
        let index = Reader::open(&segment)
            .expect("opens")
            .hiding(Bitmap::from_sorted(&[0, 1, 2, 3]))
            .expect("hides");
        assert_eq!(index.live(), 0);
        assert_eq!(index.documents(), 4);
        assert!((0..4).all(|doc| !index.is_live(doc)));
        // The lists are untouched by a deletion, which is the whole trade: the
        // reader still holds everything it was written with.
        assert!(
            index
                .postings(b"quick")
                .expect("decodes")
                .is_some_and(|list| list.len() == 2)
        );
    }

    /// Indexes the documents under the keys beside them, in the order given.
    fn keyed(docs: &[(&[u8], &str)]) -> Vec<u8> {
        let mut writer = Writer::new();
        for (key, text) in docs {
            writer.add_keyed(key, text).expect("a handful fit");
        }
        writer.finish().expect("what was written decodes")
    }

    #[test]
    fn a_key_comes_back_with_the_document_it_was_given_to() {
        let bytes = keyed(&[
            (b"docs/second.md", DOCS[1]),
            (b"docs/first.md", DOCS[0]),
            (b"docs/third.md", DOCS[2]),
        ]);
        let segment = Segment::open(&bytes).expect("opens");
        let index = Reader::open(&segment).expect("opens");
        // The keys went in out of order, which is the ordinary case, and the
        // numbering follows the order documents were added rather than the
        // order the table ends up in.
        assert_eq!(index.document(b"docs/second.md"), Some(0));
        assert_eq!(index.document(b"docs/first.md"), Some(1));
        assert_eq!(index.document(b"docs/third.md"), Some(2));
    }

    #[test]
    fn a_key_nothing_was_written_under_is_absent_rather_than_an_error() {
        let bytes = keyed(&[(b"docs/first.md", DOCS[0])]);
        let segment = Segment::open(&bytes).expect("opens");
        let index = Reader::open(&segment).expect("opens");
        assert_eq!(index.document(b"docs/nothing.md"), None);
        assert_eq!(index.document(b""), None);
        // A prefix of a key that is there is not that key.
        assert_eq!(index.document(b"docs/first"), None);
    }

    #[test]
    fn a_segment_nobody_named_a_document_in_carries_no_keys() {
        let bytes = build(&DOCS);
        let segment = Segment::open(&bytes).expect("opens");
        assert!(segment.section(kind::KEYS).is_none());
        assert!(segment.section(kind::KEY_FILTER).is_none());
        let index = Reader::open(&segment).expect("opens");
        assert!(index.keys().is_none());
        // And it answers the way a segment without the key is expected to,
        // rather than refusing to be asked.
        assert_eq!(index.document(b"docs/first.md"), None);
        // Everything else about it still works, which is what makes the key
        // sections something a build can start writing without a version step.
        assert_eq!(index.documents(), 4);
        assert!(index.postings(b"quick").expect("decodes").is_some());
    }

    #[test]
    fn naming_some_of_the_documents_leaves_the_others_findable_only_by_term() {
        let mut writer = Writer::new();
        writer.add(DOCS[0]).expect("adds");
        writer.add_keyed(b"docs/second.md", DOCS[1]).expect("adds");
        writer.add(DOCS[2]).expect("adds");
        let bytes = writer.finish().expect("finishes");
        let segment = Segment::open(&bytes).expect("opens");
        let index = Reader::open(&segment).expect("opens");
        assert_eq!(index.keys().expect("there is a key").len(), 1);
        assert_eq!(index.document(b"docs/second.md"), Some(1));
        assert_eq!(index.documents(), 3);
    }

    #[test]
    fn a_key_used_twice_in_one_segment_names_the_later_document() {
        // A batch that writes the same record twice means the second write, and
        // that is the same rule a lookup across segments follows.
        let bytes = keyed(&[
            (b"docs/first.md", DOCS[0]),
            (b"docs/second.md", DOCS[1]),
            (b"docs/first.md", DOCS[2]),
        ]);
        let segment = Segment::open(&bytes).expect("opens");
        let index = Reader::open(&segment).expect("opens");
        assert_eq!(index.document(b"docs/first.md"), Some(2));
        assert_eq!(index.keys().expect("there are keys").len(), 2);
        // The document that lost the key is still in the segment. Nothing here
        // deletes anything, and the segment's numbering has to stay as wide as
        // the documents that went into it.
        assert_eq!(index.documents(), 3);
    }

    #[test]
    fn a_key_naming_a_deleted_document_is_absent() {
        let bytes = keyed(&[(b"docs/first.md", DOCS[0]), (b"docs/second.md", DOCS[1])]);
        let segment = Segment::open(&bytes).expect("opens");
        let index = Reader::open(&segment)
            .expect("opens")
            .hiding(Bitmap::from_sorted(&[0]))
            .expect("hides");
        assert_eq!(index.document(b"docs/first.md"), None);
        assert_eq!(index.document(b"docs/second.md"), Some(1));
        // The table still holds it. A deletion is beside the segment and the
        // segment is not rewritten for one, so the key is there and the answer
        // is no.
        assert_eq!(
            index.keys().expect("there are keys").get(b"docs/first.md"),
            Some(0)
        );
    }

    #[test]
    fn keys_survive_a_segment_written_in_parts() {
        // The document numbers move when the parts are folded, and the keys of
        // a later part sort in among the keys of an earlier one, so this is
        // both halves of the fold at once.
        let mut parts = Vec::new();
        for slice in [
            [(&b"b.md"[..], DOCS[0]), (&b"d.md"[..], DOCS[1])],
            [(&b"a.md"[..], DOCS[2]), (&b"c.md"[..], DOCS[3])],
        ] {
            let mut part = Writer::new();
            for (key, text) in slice {
                part.add_keyed(key, text).expect("adds");
            }
            parts.push(part);
        }
        let bytes = Writer::concat(parts).expect("folds");
        let segment = Segment::open(&bytes).expect("opens");
        let index = Reader::open(&segment).expect("opens");
        assert_eq!(index.document(b"b.md"), Some(0));
        assert_eq!(index.document(b"d.md"), Some(1));
        assert_eq!(index.document(b"a.md"), Some(2));
        assert_eq!(index.document(b"c.md"), Some(3));
        // In the table they are in key order, not in document order, which is
        // what a binary search needs.
        let keys = index.keys().expect("there are keys");
        let found: Vec<&[u8]> = keys.table().entries().map(|(key, _)| key).collect();
        assert_eq!(found, [&b"a.md"[..], b"b.md", b"c.md", b"d.md"]);
    }

    #[test]
    fn a_key_used_in_two_parts_names_the_document_in_the_later_one() {
        let mut first = Writer::new();
        first.add_keyed(b"a.md", DOCS[0]).expect("adds");
        let mut second = Writer::new();
        second.add_keyed(b"a.md", DOCS[1]).expect("adds");
        let bytes = Writer::concat(vec![first, second]).expect("folds");
        let segment = Segment::open(&bytes).expect("opens");
        let index = Reader::open(&segment).expect("opens");
        assert_eq!(index.document(b"a.md"), Some(1));
    }

    #[test]
    fn the_filter_answers_for_a_key_the_segment_does_not_hold() {
        // Not a test of the rate, which is measured on real keys elsewhere.
        // This is the arrangement: the filter is asked first, and a key it says
        // no to never reaches the table.
        let bytes = keyed(&[(b"docs/first.md", DOCS[0]), (b"docs/second.md", DOCS[1])]);
        let segment = Segment::open(&bytes).expect("opens");
        let index = Reader::open(&segment).expect("opens");
        let keys = index.keys().expect("there are keys");
        assert!(keys.maybe_holds(b"docs/first.md"));
        assert!(keys.maybe_holds(b"docs/second.md"));
        let absent = (0..200).map(|n| format!("docs/{n}.md"));
        let said_yes = absent
            .filter(|key| keys.maybe_holds(key.as_bytes()))
            .count();
        // Two keys in a filter sized for two, so the odds of any of two hundred
        // strangers getting through are small enough to assert on.
        assert_eq!(said_yes, 0);
    }

    #[test]
    fn a_segment_carrying_keys_without_the_filter_is_refused() {
        // Neither section is any use alone, and a reader that shrugged at one
        // of them missing would answer differently depending on which.
        let bytes = keyed(&[(b"a.md", DOCS[0])]);
        let segment = Segment::open(&bytes).expect("opens");
        let table = segment
            .section(kind::KEYS)
            .expect("there are keys")
            .to_vec();
        let mut without = SegmentWriter::new();
        without
            .add(kind::TERMS, b"not a dictionary".to_vec())
            .expect("adds");
        without.add(kind::KEYS, table).expect("adds");
        let bytes = without.finish();
        let segment = Segment::open(&bytes).expect("opens");
        assert!(matches!(
            Keys::open(&segment),
            Err(Error::MissingSection {
                kind: kind::KEY_FILTER
            })
        ));
    }
}
