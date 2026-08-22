//! Folding segments into one and leaving out what was deleted.
//!
//! A store gains a segment every time a batch is committed and never loses one.
//! A directory indexed eight times is a store of sixteen segments holding one
//! live copy of each document and fifteen sixteenths dead weight, every lookup
//! by key asks all sixteen, every search opens all sixteen, and the file is as
//! large as everything ever written into it rather than as large as what is in
//! it. Compaction is what turns that back into one segment.
//!
//! This module is the half that writes the replacement. Given some segments and
//! the deletions beside them, [`merge`] produces one segment holding exactly
//! their live documents, numbered again from zero in the order the sources were
//! given. Swapping it into a store in place of the segments it was made from is
//! the store's business and is not here, which is what makes this testable
//! without a file and reusable by anything that has segment bytes.
//!
//! The merge is a rewrite rather than a copy. Postings cannot be copied across
//! because every document identifier in them moves, so each term's list is
//! decoded and encoded again against the new numbering, and the block ceilings
//! are recomputed as it goes because they are scored against the mean document
//! length and that moves too. What is copied is the part that does not depend on
//! the numbering: the stored fields of the documents that survive, and their
//! keys.

use crate::bitmap::Bitmap;
use crate::codec::{put_u32, put_u64};
use crate::error::{Error, Result};
use crate::index::{Reader, add_ceilings, average, key_index};
use crate::search::{B, K1};
use crate::segment::{Segment, Writer as SegmentWriter, kind};
use crate::{DocId, bound, length, posting, store, terms};

/// What a document that did not survive is numbered.
const GONE: DocId = DocId::MAX;

/// The sections a merge knows how to carry.
///
/// Everything a segment can hold is either in here or a reason to refuse, and
/// the list is stated once so that adding a section to the format is a compile
/// time visit to this module rather than a silent loss the next compaction
/// causes.
const CARRIED: [u16; 9] = [
    kind::TERMS,
    kind::POSTINGS,
    kind::NORMS,
    kind::ROUNDED,
    kind::BOUNDS,
    kind::WIDE_BOUNDS,
    kind::FIELDS,
    kind::KEYS,
    kind::KEY_FILTER,
];

/// One segment going into a merge.
///
/// Making one checks the segment over before anything is written: that its
/// digests match, that it is an index, that its deletions belong to it, and that
/// it holds nothing a merge would have to drop. That is why the merge takes
/// these rather than readers. A reader has already forgotten which sections its
/// segment had, so a merge given readers would quietly write a segment without
/// the vectors or the columns of the one it replaced.
#[derive(Debug)]
pub struct Source<'a> {
    reader: Reader<'a>,
}

impl<'a> Source<'a> {
    /// Opens a segment for merging, with the deletions beside it applied.
    ///
    /// # Errors
    ///
    /// Returns a decoding error if the bytes are not a segment, are not an
    /// index, or do not match their digests, [`Error::NoSuchDocument`] if the
    /// deletions name a document the segment does not hold, and
    /// [`Error::UncarriedSection`] if the segment holds a section a merge cannot
    /// carry across.
    pub fn new(bytes: &'a [u8], deleted: Option<Bitmap>) -> Result<Self> {
        let segment = Segment::open(bytes)?;
        carries(&segment)?;
        let reader = Reader::open(&segment)?;
        let reader = match deleted {
            Some(deleted) => reader.hiding(deleted)?,
            None => reader,
        };
        Ok(Self { reader })
    }

    /// The index underneath, with the deletions already applied.
    #[must_use]
    pub const fn reader(&self) -> &Reader<'a> {
        &self.reader
    }
}

/// Checks that everything a segment holds is something a merge can carry.
///
/// # Errors
///
/// Returns [`Error::UncarriedSection`] naming the first section that is not.
pub fn carries(segment: &Segment<'_>) -> Result<()> {
    for kind in segment.kinds() {
        if !CARRIED.contains(&kind) {
            return Err(Error::UncarriedSection { kind });
        }
    }
    Ok(())
}

/// Where the documents of the segments a merge replaced ended up in it.
///
/// A merge numbers the live documents of its sources in order, so a document
/// that goes in with one identifier comes out with another, and anything that
/// named the old one is talking about a store that no longer exists. Mostly
/// nothing did: a segment names its own documents and a merge rewrites it whole.
/// What does is a batch that was prepared before the merge and deletes a
/// document out of one of the segments it folded, which is every batch in flight
/// when a compaction lands.
///
/// This is what the merge worked out on its way through, kept rather than
/// dropped, so that such a batch can be moved along with the documents it names
/// instead of being refused. It costs four bytes per document of the sources,
/// against a merge that was already holding the whole merged segment, so it is
/// the smaller half of something that has already happened.
#[derive(Debug, Clone, Default)]
pub struct Moved {
    /// One vector per source, old identifier to new, [`GONE`] for the documents
    /// the merge did not carry.
    into: Vec<Vec<DocId>>,
}

impl Moved {
    /// Where the document that was `doc` in the source at `source` is now, or
    /// `None` if the merge left it behind because it was already deleted.
    ///
    /// `None` for a document that is not in that source at all, and for a source
    /// that was not in the merge, because both of those are the same question
    /// asked about a document this merge is not carrying.
    #[must_use]
    pub fn of(&self, source: usize, doc: DocId) -> Option<DocId> {
        let moved = *self.into.get(source)?.get(doc as usize)?;
        (moved != GONE).then_some(moved)
    }

    /// How many segments went into the merge.
    #[must_use]
    pub fn sources(&self) -> usize {
        self.into.len()
    }
}

/// One segment, written out of several.
#[derive(Debug)]
pub struct Merged {
    /// The segment, laid out but not yet copied anywhere.
    pub segment: SegmentWriter,
    /// How many documents it holds, which is how many of the sources were live.
    pub documents: u32,
    /// How many were left behind because they had been deleted.
    pub dropped: u64,
    /// How many distinct terms survived.
    ///
    /// Lower than the sum over the sources by however many terms the deleted
    /// documents were the last holders of, which is the part of a merge that
    /// makes the dictionary smaller rather than just the postings.
    pub terms: u32,
    /// Where the documents of the sources ended up.
    ///
    /// Kept because a batch prepared before the merge names documents by the
    /// identifiers they had in the segments it folded, and this is what turns
    /// those into the ones they have now.
    pub moved: Moved,
}

/// Writes one segment holding the live documents of all the sources.
///
/// The sources are given oldest first, the order the manifest lists them in.
/// Order decides two things. Documents are numbered in it, so the merged segment
/// reads back in the same order a search across the sources would have walked
/// them. And when the same key names a document in more than one source, the
/// later source wins, which is the same rule a lookup across a store follows
/// when it takes the newest segment holding the key.
///
/// A merge of segments with nothing deleted is the segment the index writer
/// would have built from the same documents. Nothing is thrown away in that
/// case, and nothing is rearranged either.
///
/// ```
/// # use kura_core::compact::{Source, merge};
/// # use kura_core::index::Writer;
/// let mut first = Writer::new();
/// first.add("the quick brown fox")?;
/// let first = first.finish()?;
/// let mut second = Writer::new();
/// second.add("the lazy dog")?;
/// let second = second.finish()?;
///
/// let sources = [Source::new(&first, None)?, Source::new(&second, None)?];
/// let merged = merge(&sources)?;
/// assert_eq!(merged.documents, 2);
/// # Ok::<(), kura_core::Error>(())
/// ```
///
/// # Errors
///
/// Returns a decoding error if a source does not read back, which is a damaged
/// segment, and [`Error::Overflow`] if the sources together hold more live
/// documents than a document identifier can name.
pub fn merge(sources: &[Source<'_>]) -> Result<Merged> {
    let (mapping, documents, total) = numbering(sources)?;
    let dropped = sources
        .iter()
        .map(|source| {
            u64::from(
                source
                    .reader
                    .documents()
                    .saturating_sub(source.reader.live()),
            )
        })
        .sum();

    let mut ceilings = bound::Writer::new(K1, B, average(total, documents));
    let mut dictionary = terms::Writer::new();
    let mut blob = Vec::new();
    let mut terms = 0u32;
    vocabulary(
        sources,
        &mapping,
        &mut dictionary,
        &mut blob,
        &mut ceilings,
        &mut terms,
    )?;

    let mut norms = Vec::with_capacity(16 + documents as usize * 4);
    put_u32(&mut norms, documents);
    put_u64(&mut norms, total);
    let mut rounded = Vec::with_capacity(documents as usize);
    for (source, mapping) in sources.iter().zip(&mapping) {
        for doc in 0..source.reader.documents() {
            if mapping[doc as usize] == GONE {
                continue;
            }
            let length = source.reader.length(doc);
            put_u32(&mut norms, length);
            rounded.push(length::round(length));
        }
    }

    let mut segment = SegmentWriter::new();
    segment.add(kind::TERMS, dictionary.finish())?;
    segment.add(kind::POSTINGS, blob)?;
    segment.add(kind::NORMS, norms)?;
    segment.add(kind::ROUNDED, rounded)?;
    add_ceilings(&mut segment, ceilings)?;
    if let Some(fields) = fields(sources, &mapping)? {
        segment.add(kind::FIELDS, fields)?;
    }
    if let Some((table, bits)) = key_index(named(sources, &mapping))? {
        segment.add(kind::KEYS, table)?;
        segment.add(kind::KEY_FILTER, bits)?;
    }
    Ok(Merged {
        segment,
        documents,
        dropped,
        terms,
        moved: Moved { into: mapping },
    })
}

/// Works out what each source's documents are numbered in the merged segment,
/// and how long they are altogether.
///
/// The mapping is one entry per document of each source, [`GONE`] where the
/// document was deleted. Four bytes a document of the sources, which is the
/// memory a merge holds beyond what it is writing, and it is worth it because
/// every posting of every term is looked up in it.
fn numbering(sources: &[Source<'_>]) -> Result<(Vec<Vec<DocId>>, u32, u64)> {
    let mut mapping = Vec::with_capacity(sources.len());
    let mut documents = 0u32;
    let mut total = 0u64;
    for source in sources {
        let reader = &source.reader;
        let mut into = vec![GONE; reader.documents() as usize];
        for doc in 0..reader.documents() {
            if !reader.is_live(doc) {
                continue;
            }
            into[doc as usize] = documents;
            documents = documents.checked_add(1).ok_or(Error::Overflow)?;
            total = total.saturating_add(u64::from(reader.length(doc)));
        }
        mapping.push(into);
    }
    Ok((mapping, documents, total))
}

/// Walks the term dictionaries of every source at once and writes the merged
/// dictionary, the postings and the ceilings.
///
/// The dictionaries are already in order, so this is a merge of sorted runs
/// rather than a sort. The smallest term at the front of any source is found by
/// looking at all of them, which costs a comparison per source per term and no
/// heap. There are as many sources as a compaction chose to fold, which is a
/// handful.
///
/// A term whose every posting belonged to a deleted document is not written at
/// all. That is the case that makes a merged dictionary smaller than the sum of
/// the ones it came from.
fn vocabulary(
    sources: &[Source<'_>],
    mapping: &[Vec<DocId>],
    dictionary: &mut terms::Writer,
    blob: &mut Vec<u8>,
    ceilings: &mut bound::Writer,
    count: &mut u32,
) -> Result<()> {
    let mut walks: Vec<terms::Entries<'_>> = sources
        .iter()
        .map(|source| source.reader.entries())
        .collect();
    // The front term of each source, held across the walk. A copy rather than a
    // borrow because taking the next term out of a dictionary invalidates the
    // one before it, and the buffers are reused so it is a copy and not an
    // allocation.
    let mut front: Vec<Option<(Vec<u8>, terms::Entry)>> = vec![None; sources.len()];
    for (walk, slot) in walks.iter_mut().zip(&mut front) {
        step(walk, slot)?;
    }

    // One list writer and one term buffer for the whole vocabulary, because a
    // segment has as many terms as it has and most of their lists are a few
    // bytes long.
    let mut list = posting::Writer::new();
    let mut term = Vec::new();
    while smallest(&front, &mut term) {
        let mut docs = 0u32;
        for (index, source) in sources.iter().enumerate() {
            let entry = match &front[index] {
                Some((held, entry)) if *held == term => *entry,
                _ => continue,
            };
            let mut cursor = source.reader.list(entry)?.cursor();
            while let Some(doc) = cursor.advance()? {
                let Some(&new) = mapping[index].get(doc as usize) else {
                    continue;
                };
                if new == GONE {
                    continue;
                }
                let frequency = cursor.frequency();
                list.push(new, frequency)?;
                ceilings.push(frequency, source.reader.length(doc));
                docs += 1;
            }
            step(&mut walks[index], &mut front[index])?;
        }

        if docs == 0 {
            continue;
        }
        let offset = blob.len() as u64;
        list.finish_into(blob);
        ceilings.finish_term(offset);
        dictionary.push(
            &term,
            terms::Entry {
                docs,
                offset,
                len: blob.len() as u64 - offset,
            },
        )?;
        *count += 1;
    }
    Ok(())
}

/// Copies the smallest term at the front of any source into `into`, and says
/// whether there was one.
///
/// It is copied rather than borrowed because the walk that produced it is about
/// to be moved on, and because the caller compares it against every front while
/// it does that.
fn smallest(front: &[Option<(Vec<u8>, terms::Entry)>], into: &mut Vec<u8>) -> bool {
    let Some(least) = front
        .iter()
        .flatten()
        .map(|(held, _)| held.as_slice())
        .min()
    else {
        return false;
    };
    into.clear();
    into.extend_from_slice(least);
    true
}

/// Moves one dictionary walk on, into the buffer it is holding.
fn step(walk: &mut terms::Entries<'_>, slot: &mut Option<(Vec<u8>, terms::Entry)>) -> Result<()> {
    let Some((term, entry)) = walk.next_term()? else {
        *slot = None;
        return Ok(());
    };
    match slot {
        Some((held, at)) => {
            held.clear();
            held.extend_from_slice(term);
            *at = entry;
        }
        None => *slot = Some((term.to_vec(), entry)),
    }
    Ok(())
}

/// Carries the stored fields of the live documents across, or `None` when no
/// source stored any.
///
/// A document is pushed for every live document whether or not its source had a
/// store, because the field section numbers documents by the order they were
/// pushed and a merge of a segment with fields and one without would otherwise
/// hand every document after the join the fields of another one.
fn fields(sources: &[Source<'_>], mapping: &[Vec<DocId>]) -> Result<Option<Vec<u8>>> {
    if !sources.iter().any(|source| source.reader.store().is_some()) {
        return Ok(None);
    }
    let mut writer = store::Writer::new();
    let mut scratch = store::Scratch::new();
    for (source, mapping) in sources.iter().zip(mapping) {
        for doc in 0..source.reader.documents() {
            if mapping[doc as usize] == GONE {
                continue;
            }
            let Some(reader) = source.reader.store() else {
                writer.push(core::iter::empty())?;
                continue;
            };
            let mut document = reader.get(doc, &mut scratch)?;
            let mut held = Vec::new();
            while let Some(field) = document.next_field()? {
                held.push(field);
            }
            writer.push(held)?;
        }
    }
    Ok(Some(writer.finish()?))
}

/// The keys of the live documents, under their new numbers.
///
/// A key naming a document its own segment does not hold is left out rather than
/// refused. It cannot be resolved to anything either way, and a merge is not the
/// pass that decides a segment is damaged.
fn named(sources: &[Source<'_>], mapping: &[Vec<DocId>]) -> Vec<(Box<[u8]>, DocId)> {
    let mut named = Vec::new();
    for (source, mapping) in sources.iter().zip(mapping) {
        let Some(index) = source.reader.keys() else {
            continue;
        };
        named.reserve(index.len());
        for (key, doc) in index.table().entries() {
            let Some(&new) = mapping.get(doc as usize) else {
                continue;
            };
            if new == GONE {
                continue;
            }
            named.push((key.into(), new));
        }
    }
    named
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::Writer;
    use crate::search::Searcher;

    /// A corpus small enough to check by hand, with a term in it that only one
    /// document holds so that deleting that document can be seen.
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

    /// A segment whose documents are named by their key and carry it back as a
    /// stored field, which is what lets a test say which document it is looking
    /// at after the numbering has changed.
    fn keyed(docs: &[(&str, &str)]) -> Vec<u8> {
        let mut writer = Writer::new();
        for (key, text) in docs {
            writer
                .add_keyed_with_fields(key.as_bytes(), text, [("id", key.as_bytes())])
                .expect("adds");
        }
        writer.finish().expect("what was written decodes")
    }

    /// The merged segment of some sources, ready to be read.
    fn merged(sources: &[Source<'_>]) -> Vec<u8> {
        merge(sources).expect("merges").segment.finish()
    }

    /// What a query answers, as documents in order.
    fn answers(index: &Reader<'_>, query: &str) -> Vec<DocId> {
        let mut hits = Searcher::new(index)
            .search(query, 100)
            .expect("the query runs");
        hits.sort_by_key(|hit| hit.doc);
        hits.into_iter().map(|hit| hit.doc).collect()
    }

    /// The `id` field of a document, as a string.
    fn id(index: &Reader<'_>, doc: DocId) -> String {
        let store = index.store().expect("the segment stored its fields");
        let mut scratch = store::Scratch::new();
        let document = store.get(doc, &mut scratch).expect("the document is there");
        let value = document.field("id").expect("decodes").expect("an id");
        String::from_utf8_lossy(value).into_owned()
    }

    #[test]
    fn a_merge_holds_every_live_document_of_every_source() {
        let first = build(&DOCS[..2]);
        let second = build(&DOCS[2..]);
        let sources = [
            Source::new(&first, None).expect("opens"),
            Source::new(&second, None).expect("opens"),
        ];
        let report = merge(&sources).expect("merges");
        assert_eq!(report.documents, 4);
        assert_eq!(report.dropped, 0);

        let bytes = report.segment.finish();
        let segment = Segment::open(&bytes).expect("opens");
        let index = Reader::open(&segment).expect("opens");
        assert_eq!(index.documents(), 4);
        assert_eq!(index.live(), 4);

        // The same questions the sources could answer, answered the same way.
        // The numbering matches because a search across segments numbers a hit
        // by the segments before it, which is the order the merge numbered them
        // in.
        let readers = [
            Reader::open(&Segment::open(&first).expect("opens")).expect("opens"),
            Reader::open(&Segment::open(&second).expect("opens")).expect("opens"),
        ];
        let across = Searcher::over(&readers).expect("a searcher");
        for query in ["the", "dog", "quick", "fox", "barks", "word", "missing"] {
            let mut expected: Vec<DocId> = across
                .search(query, 100)
                .expect("the query runs")
                .into_iter()
                .map(|hit| hit.doc)
                .collect();
            expected.sort_unstable();
            assert_eq!(answers(&index, query), expected, "{query} moved");
        }
    }

    #[test]
    fn a_merge_of_segments_with_nothing_deleted_is_the_segment_the_writer_would_have_built() {
        // Not a comparison of answers but of bytes. A merge that leaves nothing
        // out is doing what the writer does, and if the two disagree then one of
        // them is writing a segment the other would not, which is the kind of
        // difference that shows up much later as a file only one build reads.
        let whole = build(&DOCS);
        let first = build(&DOCS[..1]);
        let second = build(&DOCS[1..]);
        let sources = [
            Source::new(&first, None).expect("opens"),
            Source::new(&second, None).expect("opens"),
        ];
        assert_eq!(merged(&sources), whole);
    }

    #[test]
    fn a_deleted_document_and_its_key_and_its_fields_are_not_in_the_merge() {
        let bytes = keyed(&[
            ("a", DOCS[0]),
            ("b", DOCS[1]),
            ("c", DOCS[2]),
            ("d", DOCS[3]),
        ]);
        let gone = Bitmap::from_sorted(&[1]);
        let source = Source::new(&bytes, Some(gone)).expect("opens");
        let report = merge(&[source]).expect("merges");
        assert_eq!(report.documents, 3);
        assert_eq!(report.dropped, 1);

        let bytes = report.segment.finish();
        let segment = Segment::open(&bytes).expect("opens");
        let index = Reader::open(&segment).expect("opens");
        assert_eq!(index.documents(), 3);
        assert_eq!(index.live(), 3);
        assert!(
            index.deleted().is_none(),
            "a merge starts with nothing gone"
        );

        // The documents that are left are the ones that were left, in order.
        assert_eq!(id(&index, 0), "a");
        assert_eq!(id(&index, 1), "c");
        assert_eq!(id(&index, 2), "d");
        assert_eq!(index.document(b"b"), None);
        assert_eq!(index.document(b"a"), Some(0));
        assert_eq!(index.document(b"c"), Some(1));
        assert_eq!(index.document(b"d"), Some(2));

        // The deleted document held the only "barks" in the corpus, so the term
        // is not in the dictionary at all rather than in it with an empty list.
        assert!(index.postings(b"barks").expect("decodes").is_none());
        assert_eq!(answers(&index, "dog"), vec![0]);
        assert_eq!(answers(&index, "quick"), vec![0, 1]);
    }

    #[test]
    fn the_lengths_and_the_totals_of_a_merge_are_the_ones_of_what_survived() {
        let bytes = keyed(&[("a", "one two three"), ("b", "four"), ("c", "five six")]);
        let source = Source::new(&bytes, Some(Bitmap::from_sorted(&[1]))).expect("opens");
        let bytes = merged(&[source]);
        let segment = Segment::open(&bytes).expect("opens");
        let index = Reader::open(&segment).expect("opens");

        assert_eq!(index.length(0), 3);
        assert_eq!(index.length(1), 2);
        assert_eq!(index.total_length(), 5);
        assert!((index.average_length() - 2.5).abs() < f32::EPSILON);
        assert_eq!(index.rounded().len(), 2);
        for doc in 0..index.documents() {
            assert_eq!(
                index.rounded()[doc as usize],
                length::round(index.length(doc))
            );
        }
    }

    #[test]
    fn a_key_in_two_sources_resolves_to_the_document_of_the_later_one() {
        // What a store looks like when a document was replaced and the deletion
        // has not been written, or when the segments being folded are not the
        // whole store. The rule a lookup follows is that the newest segment
        // wins, and a merge has to leave that answer where it was.
        let first = keyed(&[("a", "the old text"), ("b", "another document")]);
        let second = keyed(&[("a", "the new text")]);
        let sources = [
            Source::new(&first, None).expect("opens"),
            Source::new(&second, None).expect("opens"),
        ];
        let bytes = merged(&sources);
        let segment = Segment::open(&bytes).expect("opens");
        let index = Reader::open(&segment).expect("opens");

        assert_eq!(index.documents(), 3);
        assert_eq!(index.document(b"a"), Some(2));
        assert_eq!(index.document(b"b"), Some(1));
        assert_eq!(answers(&index, "new"), vec![2]);
        // The superseded document is still there, because nothing said it was
        // deleted. It is the key that moved, not the text.
        assert_eq!(answers(&index, "old"), vec![0]);
    }

    #[test]
    fn a_source_without_stored_fields_does_not_shift_the_fields_of_one_with_them() {
        let plain = build(&["a document with no fields at all"]);
        let stored = keyed(&[("a", "one"), ("b", "two")]);
        let sources = [
            Source::new(&plain, None).expect("opens"),
            Source::new(&stored, None).expect("opens"),
        ];
        let bytes = merged(&sources);
        let segment = Segment::open(&bytes).expect("opens");
        let index = Reader::open(&segment).expect("opens");

        assert_eq!(index.documents(), 3);
        let store = index.store().expect("a field section");
        let mut scratch = store::Scratch::new();
        let mut empty = store.get(0, &mut scratch).expect("the document is there");
        assert!(empty.next_field().expect("decodes").is_none());
        assert_eq!(id(&index, 1), "a");
        assert_eq!(id(&index, 2), "b");
    }

    #[test]
    fn a_merge_of_segments_that_stored_nothing_writes_no_field_section() {
        let first = build(&DOCS[..2]);
        let second = build(&DOCS[2..]);
        let sources = [
            Source::new(&first, None).expect("opens"),
            Source::new(&second, None).expect("opens"),
        ];
        let bytes = merged(&sources);
        let segment = Segment::open(&bytes).expect("opens");
        assert!(segment.section(kind::FIELDS).is_none());
        assert!(segment.section(kind::KEYS).is_none());
        assert!(segment.section(kind::KEY_FILTER).is_none());
    }

    #[test]
    fn merging_a_segment_whose_every_document_is_deleted_leaves_an_empty_one() {
        let bytes = keyed(&[("a", DOCS[0]), ("b", DOCS[1])]);
        let source = Source::new(&bytes, Some(Bitmap::from_sorted(&[0, 1]))).expect("opens");
        let report = merge(&[source]).expect("merges");
        assert_eq!(report.documents, 0);
        assert_eq!(report.dropped, 2);
        assert_eq!(report.terms, 0);

        let bytes = report.segment.finish();
        let segment = Segment::open(&bytes).expect("opens");
        let index = Reader::open(&segment).expect("opens");
        assert!(index.is_empty());
        assert_eq!(index.terms(), 0);
        assert_eq!(answers(&index, "dog"), Vec::new());
    }

    #[test]
    fn merging_nothing_at_all_is_an_empty_segment_rather_than_a_refusal() {
        let report = merge(&[]).expect("merges");
        assert_eq!(report.documents, 0);
        let bytes = report.segment.finish();
        let segment = Segment::open(&bytes).expect("opens");
        assert!(Reader::open(&segment).expect("opens").is_empty());
    }

    #[test]
    fn a_segment_holding_a_section_a_merge_cannot_carry_is_refused() {
        // A segment written by a build that knows about something this one does
        // not. Merging it would produce a segment that answers every question
        // the old one did except the ones nothing here can ask yet, and it would
        // do it without saying so.
        let bytes = build(&DOCS);
        let segment = Segment::open(&bytes).expect("opens");
        let mut extended = SegmentWriter::new();
        for kind in segment.kinds() {
            let payload = segment.section(kind).expect("it is listed").to_vec();
            extended.add(kind, payload).expect("adds");
        }
        extended
            .add(kind::VECTORS, b"whatever a vector section holds".to_vec())
            .expect("adds");
        let bytes = extended.finish();

        assert_eq!(
            Source::new(&bytes, None).expect_err("refuses"),
            Error::UncarriedSection {
                kind: kind::VECTORS
            }
        );
    }

    #[test]
    fn deletions_that_do_not_belong_to_the_segment_are_refused() {
        let bytes = build(&DOCS);
        let gone = Bitmap::from_sorted(&[7]);
        assert!(Source::new(&bytes, Some(gone)).is_err());
    }

    #[test]
    fn the_ceilings_of_a_merge_are_scored_against_the_average_of_what_it_holds() {
        // A long document held back by a deletion moves the mean, and a ceiling
        // computed against the mean of the corpus before the deletion is not a
        // ceiling for the corpus after it.
        let long = vec!["word"; 4000].join(" ");
        let docs: Vec<&str> = core::iter::once(long.as_str())
            .chain(core::iter::repeat_n("word word", 300))
            .collect();
        let bytes = build(&docs);
        let source = Source::new(&bytes, Some(Bitmap::from_sorted(&[0]))).expect("opens");
        let bytes = merged(&[source]);
        let segment = Segment::open(&bytes).expect("opens");
        let index = Reader::open(&segment).expect("opens");

        let bounds = index.bounds().expect("lists this long have ceilings");
        assert!(
            (bounds.average_length() - index.average_length()).abs() < f32::EPSILON,
            "the ceilings say {} and the segment says {}",
            bounds.average_length(),
            index.average_length()
        );
        assert_eq!(index.documents(), 300);
    }

    #[test]
    fn a_merge_carries_the_ceilings_at_both_of_the_widths_a_segment_keeps_them() {
        // The ceilings are kept twice, at a block and at a finer grain, and a
        // merge that wrote one of them and not the other would produce a segment
        // that answers correctly and skips less of the work it could have
        // skipped. Nothing about a query would say so, which is why this asks
        // the segment what sections it has rather than asking it a question.
        let docs: Vec<&str> = core::iter::repeat_n("word word", 400).collect();
        let bytes = build(&docs);
        let source = Source::new(&bytes, None).expect("opens");
        let merged = merged(&[source]);

        let written = Segment::open(&bytes).expect("opens");
        let folded = Segment::open(&merged).expect("opens");
        let mut kinds: Vec<u16> = written.kinds().collect();
        kinds.sort_unstable();
        let mut carried: Vec<u16> = folded.kinds().collect();
        carried.sort_unstable();
        assert!(kinds.contains(&kind::BOUNDS));
        assert!(kinds.contains(&kind::WIDE_BOUNDS));
        assert_eq!(carried, kinds);
        assert_eq!(
            merged, bytes,
            "a merge of one whole segment is that segment"
        );
    }

    #[test]
    fn every_section_kind_there_is_is_either_carried_or_refused_on_purpose() {
        // The sections a merge has no way to carry yet. Being in here is a
        // decision that a segment holding one is refused, which is the loud
        // failure. The quiet one is a kind in neither list, which is a section
        // this module has never heard of and would drop, and that is what this
        // test is for.
        const REFUSED: [u16; 5] = [
            kind::VECTORS,
            kind::ACL,
            kind::COLUMNS,
            kind::GRAPH,
            kind::TOMBSTONES,
        ];
        for kind in kind::KNOWN {
            assert!(
                CARRIED.contains(&kind) || REFUSED.contains(&kind),
                "nothing here says what a merge does with a section of kind {kind}"
            );
            assert!(
                !(CARRIED.contains(&kind) && REFUSED.contains(&kind)),
                "a section of kind {kind} is both carried and refused"
            );
        }
    }
}
