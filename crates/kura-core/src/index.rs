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
use crate::segment::{Segment, Writer as SegmentWriter, kind};
use crate::{DocId, posting, store, terms};

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
}

impl Writer {
    /// Creates an empty index.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
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
        } = self;
        let length = analyzer.analyze(text, |term, _| postings.count(term, stamp));
        postings.flush(doc);
        lengths.push(length);
        *total += u64::from(length);
        let before = store.values();
        store.push(fields)?;
        *stored |= store.values() > before;
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
                part.postings.walk(id, base[index], &mut list)?;
                docs += part.postings.documents[id as usize];
                front[index] += 1;
            }
            let offset = blob.len() as u64;
            list.finish_into(&mut blob);
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
        let total: u64 = parts.iter().map(|part| part.total).sum();
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
        for part in parts {
            stored |= part.stored;
            match &mut store {
                Some(held) => held.merge(part.store)?,
                None => store = Some(part.store),
            }
        }

        let mut segment = SegmentWriter::new();
        segment.add(kind::TERMS, dictionary.finish())?;
        segment.add(kind::POSTINGS, blob)?;
        segment.add(kind::NORMS, norms)?;
        if let (true, Some(store)) = (stored, store) {
            segment.add(kind::FIELDS, store.finish()?)?;
        }
        Ok(segment)
    }
}

/// A vocabulary and one posting chain per term in it.
#[derive(Debug, Default)]
struct Accumulator {
    vocabulary: Vocabulary,
    arena: Vec<u8>,
    head: Vec<u32>,
    tail: Vec<u32>,
    used: Vec<u32>,
    documents: Vec<u32>,
    last: Vec<DocId>,
    /// The document a term was last counted in, plus one.
    stamp: Vec<u32>,
    /// How often a term occurs in the document being counted.
    frequency: Vec<u32>,
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
        if at == self.head.len() {
            let chunk = self.chunk();
            self.head.push(chunk);
            self.tail.push(chunk);
            self.used.push(0);
            self.documents.push(0);
            self.last.push(0);
            self.stamp.push(0);
            self.frequency.push(0);
        }
        if self.stamp[at] == stamp {
            self.frequency[at] += 1;
        } else {
            self.stamp[at] = stamp;
            self.frequency[at] = 1;
            self.touched.push(id);
        }
    }

    /// Appends everything counted for `doc` to the chains it belongs to.
    fn flush(&mut self, doc: DocId) {
        for index in 0..self.touched.len() {
            let at = self.touched[index] as usize;
            let gap = if self.documents[at] == 0 {
                doc
            } else {
                doc - self.last[at]
            };
            self.append(at, gap, self.frequency[at]);
            self.documents[at] += 1;
            self.last[at] = doc;
        }
        self.touched.clear();
    }

    /// Writes one document gap and frequency into a term's chain.
    fn append(&mut self, at: usize, gap: u32, frequency: u32) {
        if self.used[at] as usize + MAX_PAIR > PAYLOAD {
            let next = self.chunk();
            let tail = self.tail[at] as usize;
            self.arena[tail..tail + LINK].copy_from_slice(&next.to_le_bytes());
            self.tail[at] = next;
            self.used[at] = 0;
        }
        self.scratch.clear();
        put_uvarint(&mut self.scratch, u64::from(gap));
        put_uvarint(&mut self.scratch, u64::from(frequency));
        let start = self.tail[at] as usize + LINK + self.used[at] as usize;
        self.arena[start..start + self.scratch.len()].copy_from_slice(&self.scratch);
        self.used[at] += u32::try_from(self.scratch.len()).expect("a pair is at most ten bytes");
    }

    /// Adds an empty chunk and returns where it starts.
    fn chunk(&mut self) -> u32 {
        let at = u32::try_from(self.arena.len()).expect("the arena is under four gigabytes");
        self.arena.resize(self.arena.len() + CHUNK, 0);
        self.arena[at as usize..at as usize + LINK].copy_from_slice(&NONE.to_le_bytes());
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
    fn walk(&self, id: u32, base: DocId, writer: &mut posting::Writer) -> Result<()> {
        let at = id as usize;
        let mut chunk = self.head[at] as usize;
        let mut offset = 0;
        let mut doc: DocId = 0;
        for index in 0..self.documents[at] {
            if offset + MAX_PAIR > PAYLOAD {
                let link = &self.arena[chunk..chunk + LINK];
                chunk = u32::from_le_bytes(link.try_into().expect("four bytes")) as usize;
                offset = 0;
            }
            let start = chunk + LINK + offset;
            let rest = &self.arena[start..chunk + CHUNK];
            let (gap, rest) = get_uvarint(rest)?;
            let (frequency, rest) = get_uvarint(rest)?;
            offset = chunk + CHUNK - rest.len() - chunk - LINK;
            let gap = u32::try_from(gap).map_err(|_| Error::NotSorted { at: doc })?;
            doc = if index == 0 { gap } else { doc + gap };
            let shifted = doc.checked_add(base).ok_or(Error::Overflow)?;
            writer.push(shifted, u32::try_from(frequency).unwrap_or(u32::MAX))?;
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
    arena: Vec<u8>,
    spans: Vec<(u32, u32)>,
    table: Vec<u32>,
}

impl Default for Vocabulary {
    fn default() -> Self {
        Self {
            arena: Vec::new(),
            spans: Vec::new(),
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
        let start =
            u32::try_from(self.arena.len()).expect("the vocabulary is under four gigabytes");
        self.arena.extend_from_slice(term);
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
        &self.arena[start as usize..start as usize + len as usize]
    }

    /// How many distinct terms there are.
    fn count(&self) -> u32 {
        u32::try_from(self.spans.len()).expect("under four billion distinct terms")
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

/// Reads an index out of a segment.
#[derive(Debug)]
pub struct Reader<'a> {
    terms: terms::Reader<'a>,
    postings: &'a [u8],
    lengths: &'a [u8],
    store: Option<store::Reader<'a>>,
    documents: u32,
    total: u64,
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
        Ok(Self {
            terms: terms::Reader::new(dictionary)?,
            postings,
            lengths,
            store,
            documents,
            total,
        })
    }

    /// How many documents the index holds.
    #[must_use]
    pub const fn documents(&self) -> u32 {
        self.documents
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
    #[expect(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        reason = "an average over a corpus is wanted to a few significant figures, \
                  and the division runs in f64 so that only the result is narrowed"
    )]
    pub fn average_length(&self) -> f32 {
        if self.documents == 0 {
            return 0.0;
        }
        (self.total as f64 / f64::from(self.documents)) as f32
    }

    /// The stored fields, or nothing if the index was built without any.
    #[must_use]
    pub const fn store(&self) -> Option<&store::Reader<'a>> {
        self.store.as_ref()
    }

    /// The posting list of a term, or nothing if the term is not in the index.
    ///
    /// # Errors
    ///
    /// Returns an error if the dictionary or the posting list does not decode.
    pub fn postings(&self, term: &[u8]) -> Result<Option<posting::Reader<'a>>> {
        match self.terms.get(term)? {
            Some(entry) => self.list(entry).map(Some),
            None => Ok(None),
        }
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
}
