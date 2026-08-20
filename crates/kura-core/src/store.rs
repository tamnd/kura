//! The document values that come back with a hit.
//!
//! A search engine that can only say which documents matched is not much use.
//! Something has to hold the title to put on the result, the path to link to
//! and the identifier the caller knows the document by, and that is this.
//!
//! It is deliberately not the index. Nothing here is searched, and the values
//! are handed back exactly as they went in, because the moment a store starts
//! interpreting what it holds it acquires opinions about encodings that belong
//! to the layer above.
//!
//! Field names are written once, in a dictionary at the front, and each value
//! refers to its name by number. A corpus has a handful of fields and millions
//! of documents, so writing the name with every value would cost more than most
//! of the values do.
//!
//! # Why this is compressed and the index is not
//!
//! The store is the largest thing in a segment, usually by more than it is
//! close. An index of prose is a fraction of the prose; the copy of the prose
//! kept to show a person is all of it. It is also the coldest thing in a
//! segment: a query touches the postings of every matching document and the
//! stored fields of ten. Spending a decompression on those ten to make the
//! whole file half the size is the easiest trade in the format.
//!
//! Records are packed into blocks of about sixteen kilobytes and each block is
//! compressed on its own. Compressing the whole store as one stream would give
//! up random access, and compressing each document on its own would throw away
//! the repetition between documents, which on a real corpus is most of what
//! there is to find. A block is the smallest unit that still has neighbours in
//! it.
//!
//! A document larger than a block gets a block to itself, so reading it means
//! decompressing all of it. That is the price of storing a large document at
//! all, and it is paid by the query that asks for it rather than by the corpus.
//!
//! # Finding a document
//!
//! A hit list is a scattered set of document numbers by the time it reaches
//! here, so lookup is random access and anything that scans is the wrong shape.
//! Each document has a four byte offset into its own decompressed block, and
//! the block it belongs to is found by a binary search over a directory with
//! one entry per block. The search is a few dozen nanoseconds against a
//! decompression measured in microseconds, so it does not show up.
//!
//! Decompressing needs somewhere to put the bytes, and a reader is shared and
//! immutable, so the caller passes a [`Scratch`]. It holds the last block that
//! was decoded, which is what makes reading a page of hits that landed near
//! each other cost one decompression rather than ten.

use crate::DocId;
use crate::codec::{get_uvarint, put_uvarint, split_at};
use crate::error::{Error, Result};
use crate::lz;

/// How much raw record data a block holds before the next document starts a new
/// one.
///
/// The trade is compression against the cost of one lookup, and it is lopsided.
/// Over fifty thousand generated documents of about five kilobytes each, four
/// kilobyte blocks stored the corpus at 0.40 of its size and read a document in
/// 4.1 microseconds, eight at 0.39 and 5.9, sixteen at 0.38 and 9.6, and thirty
/// two at 0.37 and 16.7. Quadrupling the block buys three percent of the size
/// and costs four times the read.
///
/// Eight is the point where the ratio has nearly stopped moving. Documents
/// shorter than this share a block and compress against each other, which is
/// where blocking earns its keep; documents longer than it are on their own
/// either way.
const BLOCK: usize = 8 * 1024;

/// The size of one entry in the block directory.
const ENTRY: usize = 16;

/// The size of one document offset.
const SLOT: usize = 4;

/// Builds a store.
///
/// Documents are pushed in order and numbered from zero, which is the same
/// numbering the index gives them, because the whole point is that a hit from
/// one is a lookup in the other.
#[derive(Debug, Default)]
pub struct Writer {
    names: Vec<String>,
    /// Where each document starts inside its own block, four bytes each.
    offsets: Vec<u32>,
    /// The directory, four `u32` fields per block. See [`Writer::flush`].
    blocks: Vec<u32>,
    /// The compressed blocks, back to back.
    payload: Vec<u8>,
    /// The block being filled, uncompressed.
    block: Vec<u8>,
    /// Somewhere to hold a record while the block it landed in is closed
    /// behind it. See the end of [`Writer::push`].
    spill: Vec<u8>,
    /// The first document in the block being filled.
    first: u32,
    compressor: lz::Compressor,
    values: u64,
}

impl Writer {
    /// Creates an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends a document and returns the number it was given.
    ///
    /// A field name that has not been seen before is added to the dictionary.
    /// Repeating a name within one document is allowed and both values are
    /// kept, because a document with two authors is a document with two
    /// authors.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotSorted`] if more documents are added than a document
    /// identifier can hold, and [`Error::Overflow`] if the compressed payload
    /// passes four gigabytes, which is where a segment is split anyway.
    pub fn push<'a>(
        &mut self,
        fields: impl IntoIterator<Item = (&'a str, &'a [u8])>,
    ) -> Result<DocId> {
        let doc =
            u32::try_from(self.offsets.len()).map_err(|_| Error::NotSorted { at: u32::MAX })?;
        let start = u32::try_from(self.block.len()).map_err(|_| Error::Overflow)?;
        // The count goes in first and is not known yet, so a placeholder is
        // written and rewritten. It is one byte for anything under 128 fields,
        // and a document with more fields than that is not a document.
        self.block.push(0);
        let mut count = 0u64;
        for (name, value) in fields {
            let index = self.name(name);
            put_uvarint(&mut self.block, index);
            put_uvarint(&mut self.block, value.len() as u64);
            self.block.extend_from_slice(value);
            count += 1;
        }
        self.values += count;
        if let (true, Some(slot)) = (count < 128, self.block.get_mut(start as usize)) {
            *slot = u8::try_from(count).unwrap_or(0);
        } else {
            // Rare enough not to be worth a second pass over the common case.
            // The placeholder is dropped and the real count spliced in.
            let mut header = Vec::with_capacity(crate::codec::MAX_VARINT_LEN64);
            put_uvarint(&mut header, count);
            let at = start as usize;
            self.block.splice(at..=at, header);
        }

        // A record that takes the block past its size goes into one of its own
        // rather than staying where it landed. Otherwise a block is a block
        // plus however long the last document happened to be, and a lookup
        // pays to decompress a neighbour it was not asked for. On a corpus of
        // source files, where a document is a few kilobytes, that was most of
        // what a lookup cost.
        let mut offset = start;
        if start > 0 && self.block.len() > BLOCK {
            self.spill.clear();
            self.spill.extend_from_slice(&self.block[start as usize..]);
            self.block.truncate(start as usize);
            self.flush()?;
            self.block.extend_from_slice(&self.spill);
            offset = 0;
        }
        self.offsets.push(offset);
        Ok(doc)
    }

    /// Compresses the block being filled and starts a new one.
    ///
    /// The directory entry is the first document in the block, where the block
    /// starts in the payload, how many bytes it was and how many it became. A
    /// block that did not compress is written as it stands and says so by
    /// giving the same number twice, which is why the compressed form is thrown
    /// away when it is not smaller rather than when it is not strictly smaller.
    fn flush(&mut self) -> Result<()> {
        if self.block.is_empty() {
            return Ok(());
        }
        let start = u32::try_from(self.payload.len()).map_err(|_| Error::Overflow)?;
        let raw = u32::try_from(self.block.len()).map_err(|_| Error::Overflow)?;
        self.compressor.compress(&self.block, &mut self.payload);
        let mut comp =
            u32::try_from(self.payload.len() - start as usize).map_err(|_| Error::Overflow)?;
        if comp >= raw {
            self.payload.truncate(start as usize);
            self.payload.extend_from_slice(&self.block);
            comp = raw;
        }
        self.blocks
            .extend_from_slice(&[self.first, start, raw, comp]);
        self.first = u32::try_from(self.offsets.len()).map_err(|_| Error::Overflow)?;
        self.block.clear();
        Ok(())
    }

    /// How many documents have been pushed.
    #[must_use]
    pub fn len(&self) -> usize {
        self.offsets.len()
    }

    /// How many field values have been written across every document.
    ///
    /// A caller building an index reads this before and after a push to find
    /// out whether the document stored anything, without walking what it
    /// wrote or holding the fields a second time.
    #[must_use]
    pub const fn values(&self) -> u64 {
        self.values
    }

    /// Whether no documents have been pushed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.offsets.is_empty()
    }

    /// The number of a field name, adding it if it is new.
    fn name(&mut self, name: &str) -> u64 {
        if let Some(at) = self.names.iter().position(|known| known == name) {
            return at as u64;
        }
        self.names.push(name.to_string());
        (self.names.len() - 1) as u64
    }

    /// Folds a store over the documents that come after this one's into it.
    ///
    /// The documents of `other` are renumbered to follow the ones already here,
    /// which is the only thing the order of a fold decides. Blocks are moved
    /// across as they stand, still compressed, so folding costs a copy of the
    /// payload and not a pass over the text.
    ///
    /// The exception is a store whose field names disagree with this one's,
    /// which cannot happen when both were filled from the same corpus and is
    /// handled by rewriting the records rather than by refusing. Names are
    /// numbered in the order they were first seen, so agreeing means one list
    /// is the start of the other.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Overflow`] if the two together pass what a store can
    /// hold.
    pub fn merge(&mut self, mut other: Self) -> Result<()> {
        self.flush()?;
        other.flush()?;
        if self.names.iter().zip(&other.names).all(|(a, b)| a == b) {
            if other.names.len() > self.names.len() {
                self.names
                    .extend_from_slice(&other.names[self.names.len()..]);
            }
            let base = u32::try_from(self.offsets.len()).map_err(|_| Error::Overflow)?;
            let start = u32::try_from(self.payload.len()).map_err(|_| Error::Overflow)?;
            self.blocks.reserve(other.blocks.len());
            for entry in other.blocks.chunks_exact(4) {
                self.blocks.extend_from_slice(&[
                    entry[0].checked_add(base).ok_or(Error::Overflow)?,
                    entry[1].checked_add(start).ok_or(Error::Overflow)?,
                    entry[2],
                    entry[3],
                ]);
            }
            self.offsets.extend_from_slice(&other.offsets);
            self.payload.extend_from_slice(&other.payload);
            self.values += other.values;
            self.first = u32::try_from(self.offsets.len()).map_err(|_| Error::Overflow)?;
            return Ok(());
        }

        // The names disagree, so every record has to be read and written again
        // with the numbers it will have here. This is the path that is not
        // taken by a corpus where every document has the same fields.
        let bytes = other.finish()?;
        let read = Reader::new(&bytes)?;
        let mut scratch = Scratch::new();
        for doc in 0..read.len() {
            // The values are copied out rather than borrowed, because they
            // point into the buffer the next document is about to be
            // decompressed into.
            let mut fields: Vec<(&str, Vec<u8>)> = Vec::new();
            let mut record = read.get(doc, &mut scratch)?;
            while let Some((name, value)) = record.next_field()? {
                fields.push((name, value.to_vec()));
            }
            self.push(fields.iter().map(|(name, value)| (*name, value.as_slice())))?;
        }
        Ok(())
    }

    /// Writes the section.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Overflow`] if the compressed payload passes four
    /// gigabytes.
    pub fn finish(mut self) -> Result<Vec<u8>> {
        self.flush()?;
        let mut out = Vec::with_capacity(self.payload.len() + self.offsets.len() * SLOT + 64);
        put_uvarint(&mut out, self.offsets.len() as u64);
        put_uvarint(&mut out, self.names.len() as u64);
        for name in &self.names {
            put_uvarint(&mut out, name.len() as u64);
            out.extend_from_slice(name.as_bytes());
        }
        put_uvarint(&mut out, self.blocks.len() as u64 * 4);
        for field in &self.blocks {
            out.extend_from_slice(&field.to_le_bytes());
        }
        put_uvarint(&mut out, self.offsets.len() as u64 * SLOT as u64);
        for offset in &self.offsets {
            out.extend_from_slice(&offset.to_le_bytes());
        }
        put_uvarint(&mut out, self.payload.len() as u64);
        out.extend_from_slice(&self.payload);
        Ok(out)
    }
}

/// Somewhere to put a decompressed block.
///
/// A reader is shared and holds nothing that changes, so the buffer a block is
/// decoded into belongs to whoever is reading. One of these per thread is the
/// intended shape. It caches the last block, so a page of hits that landed near
/// each other in the corpus costs one decompression rather than one each.
#[derive(Debug, Default)]
pub struct Scratch {
    block: Vec<u8>,
    /// Which block is in the buffer, and which reader it came from. Two readers
    /// number their blocks the same way, so the block number alone would let a
    /// scratch used against one reader answer for the other.
    held: Option<(usize, usize)>,
}

impl Scratch {
    /// Creates an empty buffer.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Forgets whatever block is held, without releasing the memory.
    pub fn clear(&mut self) {
        self.block.clear();
        self.held = None;
    }
}

/// Reads a store.
#[derive(Debug, Clone)]
pub struct Reader<'a> {
    names: Vec<&'a str>,
    blocks: &'a [u8],
    offsets: &'a [u8],
    payload: &'a [u8],
    count: u32,
}

impl<'a> Reader<'a> {
    /// Opens a store.
    ///
    /// # Errors
    ///
    /// Returns an error if the input is not a store this build wrote, which
    /// includes every truncation of one.
    pub fn new(input: &'a [u8]) -> Result<Self> {
        let (count, rest) = get_uvarint(input)?;
        let count = u32::try_from(count).map_err(|_| Error::Overflow)?;
        let (names, mut rest) = get_uvarint(rest)?;
        let names = usize::try_from(names).map_err(|_| Error::Overflow)?;
        // The dictionary is small and is read once, so the names are resolved
        // here rather than on every lookup.
        let mut resolved = Vec::with_capacity(names.min(64));
        for _ in 0..names {
            let (len, tail) = get_uvarint(rest)?;
            let (bytes, tail) = split_at(tail, usize::try_from(len).map_err(|_| Error::Overflow)?)?;
            resolved.push(core::str::from_utf8(bytes).map_err(|_| Error::Overflow)?);
            rest = tail;
        }
        let (len, rest) = get_uvarint(rest)?;
        let (blocks, rest) = split_at(rest, usize::try_from(len).map_err(|_| Error::Overflow)?)?;
        if blocks.len() % ENTRY != 0 {
            return Err(Error::Truncated {
                needed: blocks.len().next_multiple_of(ENTRY),
                available: blocks.len(),
            });
        }
        let (len, rest) = get_uvarint(rest)?;
        let (offsets, rest) = split_at(rest, usize::try_from(len).map_err(|_| Error::Overflow)?)?;
        if offsets.len() != count as usize * SLOT {
            return Err(Error::Truncated {
                needed: count as usize * SLOT,
                available: offsets.len(),
            });
        }
        let (len, rest) = get_uvarint(rest)?;
        let (payload, _) = split_at(rest, usize::try_from(len).map_err(|_| Error::Overflow)?)?;
        Ok(Self {
            names: resolved,
            blocks,
            offsets,
            payload,
            count,
        })
    }

    /// How many documents the store holds.
    #[must_use]
    pub const fn len(&self) -> u32 {
        self.count
    }

    /// Whether the store holds no documents.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// The field names, in the order they were first written.
    #[must_use]
    pub fn names(&self) -> &[&'a str] {
        &self.names
    }

    /// How many blocks the store is split into.
    #[must_use]
    pub const fn blocks(&self) -> usize {
        self.blocks.len() / ENTRY
    }

    /// One directory entry, as first document, start, raw length, stored
    /// length.
    fn entry(&self, at: usize) -> Option<(u32, usize, usize, usize)> {
        let bytes = self.blocks.get(at * ENTRY..at * ENTRY + ENTRY)?;
        let mut field = [0u32; 4];
        for (slot, raw) in field.iter_mut().zip(bytes.chunks_exact(4)) {
            *slot = u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]);
        }
        Some((
            field[0],
            field[1] as usize,
            field[2] as usize,
            field[3] as usize,
        ))
    }

    /// The block a document lives in, which is the last one that starts at or
    /// before it.
    fn locate(&self, doc: DocId) -> Option<usize> {
        let (mut lo, mut hi) = (0usize, self.blocks());
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.entry(mid)?.0 <= doc {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo.checked_sub(1)
    }

    /// The values stored for a document.
    ///
    /// The scratch buffer is where the document's block is decompressed, and it
    /// keeps whatever it decoded last, so asking for documents that were
    /// written near each other costs one decompression between them.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotSorted`] if there is no such document, and a
    /// decoding error if the record for it is not what it claims to be.
    pub fn get<'s>(&'s self, doc: DocId, scratch: &'s mut Scratch) -> Result<Document<'s>> {
        if doc >= self.count {
            return Err(Error::NotSorted { at: doc });
        }
        let at = doc as usize * SLOT;
        let slot = self
            .offsets
            .get(at..at + SLOT)
            .ok_or(Error::NotSorted { at: doc })?;
        let offset = u32::from_le_bytes([slot[0], slot[1], slot[2], slot[3]]) as usize;

        let which = self.locate(doc).ok_or(Error::NotSorted { at: doc })?;
        let (_, start, raw, stored) = self.entry(which).ok_or(Error::NotSorted { at: doc })?;
        let bytes = self
            .payload
            .get(start..start + stored)
            .ok_or(Error::Truncated {
                needed: start + stored,
                available: self.payload.len(),
            })?;
        // A block that did not compress is read where it lies. That is not only
        // faster, it is the whole of the read for a store of values that have
        // nothing in common with each other.
        let block: &'s [u8] = if stored == raw {
            bytes
        } else {
            let held = (self.payload.as_ptr() as usize, which);
            if scratch.held != Some(held) {
                scratch.clear();
                lz::decompress(bytes, raw, &mut scratch.block)?;
                scratch.held = Some(held);
            }
            &scratch.block
        };

        let rest = block.get(offset..).ok_or(Error::Truncated {
            needed: offset,
            available: block.len(),
        })?;
        let (count, rest) = get_uvarint(rest)?;
        Ok(Document {
            names: &self.names,
            rest,
            left: count,
        })
    }
}

/// The values of one document, walked as they are asked for.
///
/// Nothing is decoded until a field is read, and a value that is read comes
/// back as a slice of the block it was decompressed into rather than a copy of
/// it. A result page asks for two fields out of six, so decoding all six on the
/// way past would be five sixths wasted.
#[derive(Debug, Clone)]
pub struct Document<'s> {
    names: &'s [&'s str],
    rest: &'s [u8],
    left: u64,
}

impl<'s> Document<'s> {
    /// The next field and its value.
    ///
    /// # Errors
    ///
    /// Returns a decoding error if the record ends in the middle of a value.
    pub fn next_field(&mut self) -> Result<Option<(&'s str, &'s [u8])>> {
        if self.left == 0 {
            return Ok(None);
        }
        self.left -= 1;
        let (index, rest) = get_uvarint(self.rest)?;
        let (len, rest) = get_uvarint(rest)?;
        let (value, rest) = split_at(rest, usize::try_from(len).map_err(|_| Error::Overflow)?)?;
        self.rest = rest;
        let index = usize::try_from(index).map_err(|_| Error::Overflow)?;
        let name = self.names.get(index).copied().ok_or(Error::Overflow)?;
        Ok(Some((name, value)))
    }

    /// The first value stored under a name, or nothing if there is none.
    ///
    /// # Errors
    ///
    /// Returns a decoding error if the record ends in the middle of a value.
    pub fn field(&self, name: &str) -> Result<Option<&'s [u8]>> {
        let mut walk = self.clone();
        while let Some((found, value)) = walk.next_field()? {
            if found == name {
                return Ok(Some(value));
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
#[expect(
    clippy::cast_possible_truncation,
    reason = "the counts in these tests are literals well under any limit"
)]
mod tests {
    use super::*;

    fn build(docs: &[Vec<(&str, &str)>]) -> Vec<u8> {
        let mut writer = Writer::new();
        for doc in docs {
            writer
                .push(doc.iter().map(|(n, v)| (*n, v.as_bytes())))
                .expect("a handful of documents fit");
        }
        writer.finish().expect("a handful of documents fit")
    }

    fn corpus() -> Vec<Vec<(&'static str, &'static str)>> {
        vec![
            vec![("id", "a"), ("title", "first"), ("body", "the body of it")],
            vec![("id", "b"), ("title", "second")],
            vec![
                ("id", "c"),
                ("body", ""),
                ("title", "third"),
                ("extra", "x"),
            ],
        ]
    }

    #[test]
    fn every_value_comes_back_as_it_went_in() {
        let bytes = build(&corpus());
        let store = Reader::new(&bytes).expect("what was written reads");
        assert_eq!(store.len(), 3);
        let mut scratch = Scratch::new();
        for (doc, fields) in corpus().iter().enumerate() {
            let record = store
                .get(doc as DocId, &mut scratch)
                .expect("the document is there");
            for (name, value) in fields {
                assert_eq!(
                    record.field(name).expect("decodes"),
                    Some(value.as_bytes()),
                    "{name} of document {doc}"
                );
            }
        }
    }

    #[test]
    fn a_field_a_document_does_not_have_is_not_there() {
        let bytes = build(&corpus());
        let store = Reader::new(&bytes).expect("reads");
        let mut scratch = Scratch::new();
        let record = store.get(1, &mut scratch).expect("is there");
        assert_eq!(record.field("body").expect("decodes"), None);
        assert_eq!(record.field("nothing").expect("decodes"), None);
    }

    #[test]
    fn an_empty_value_is_not_a_missing_value() {
        let bytes = build(&corpus());
        let store = Reader::new(&bytes).expect("reads");
        let mut scratch = Scratch::new();
        let record = store.get(2, &mut scratch).expect("is there");
        assert_eq!(record.field("body").expect("decodes"), Some(&b""[..]));
    }

    #[test]
    fn the_fields_come_back_in_the_order_they_were_written() {
        let bytes = build(&corpus());
        let store = Reader::new(&bytes).expect("reads");
        let mut scratch = Scratch::new();
        let mut record = store.get(2, &mut scratch).expect("is there");
        let mut seen = Vec::new();
        while let Some((name, _)) = record.next_field().expect("decodes") {
            seen.push(name);
        }
        assert_eq!(seen, ["id", "body", "title", "extra"]);
    }

    #[test]
    fn a_name_is_written_once_however_many_documents_use_it() {
        // The dictionary is the reason a store of small documents is not mostly
        // field names, so this checks the size rather than the shape.
        let docs: Vec<Vec<(&str, &str)>> = (0..1_000)
            .map(|_| {
                vec![
                    ("a_rather_long_field_name", "1"),
                    ("another_rather_long_one", "2"),
                ]
            })
            .collect();
        let bytes = build(&docs);
        let names = 1_000 * ("a_rather_long_field_name".len() + "another_rather_long_one".len());
        // It comes to about 11 bytes a document, which is the four byte offset
        // and the seven byte record, against the 47 bytes the names alone would
        // cost if they were written with every value.
        assert!(
            bytes.len() < names / 3,
            "{} bytes for a thousand documents",
            bytes.len()
        );
        let store = Reader::new(&bytes).expect("reads");
        assert_eq!(
            store.names(),
            ["a_rather_long_field_name", "another_rather_long_one"]
        );
    }

    #[test]
    fn the_same_name_twice_in_one_document_keeps_both() {
        let mut writer = Writer::new();
        writer
            .push([("author", &b"ada"[..]), ("author", &b"grace"[..])])
            .expect("fits");
        let bytes = writer.finish().expect("fits");
        let store = Reader::new(&bytes).expect("reads");
        let mut scratch = Scratch::new();
        let mut record = store.get(0, &mut scratch).expect("is there");
        let mut seen = Vec::new();
        while let Some((name, value)) = record.next_field().expect("decodes") {
            seen.push((name, value.to_vec()));
        }
        assert_eq!(
            seen,
            [("author", b"ada".to_vec()), ("author", b"grace".to_vec())]
        );
    }

    #[test]
    fn a_document_with_no_fields_is_a_document() {
        let mut writer = Writer::new();
        writer.push(core::iter::empty()).expect("fits");
        writer.push([("id", &b"x"[..])]).expect("fits");
        let bytes = writer.finish().expect("fits");
        let store = Reader::new(&bytes).expect("reads");
        assert_eq!(store.len(), 2);
        let mut scratch = Scratch::new();
        let mut record = store.get(0, &mut scratch).expect("is there");
        assert!(record.next_field().expect("decodes").is_none());
        let mut scratch = Scratch::new();
        assert_eq!(
            store
                .get(1, &mut scratch)
                .expect("is there")
                .field("id")
                .expect("decodes"),
            Some(&b"x"[..])
        );
    }

    #[test]
    fn a_document_with_more_fields_than_the_placeholder_holds() {
        // Past 127 fields the count needs a second byte, which is the one path
        // in the writer that rewrites what it already emitted.
        let names: Vec<String> = (0..300).map(|i| format!("field{i}")).collect();
        let mut writer = Writer::new();
        writer
            .push(names.iter().map(|n| (n.as_str(), n.as_bytes())))
            .expect("fits");
        writer.push([("after", &b"still fine"[..])]).expect("fits");
        let bytes = writer.finish().expect("fits");
        let store = Reader::new(&bytes).expect("reads");
        let mut scratch = Scratch::new();
        let record = store.get(0, &mut scratch).expect("is there");
        for name in &names {
            assert_eq!(
                record.field(name).expect("decodes"),
                Some(name.as_bytes()),
                "{name}"
            );
        }
        let mut scratch = Scratch::new();
        assert_eq!(
            store
                .get(1, &mut scratch)
                .expect("is there")
                .field("after")
                .expect("decodes"),
            Some(&b"still fine"[..])
        );
    }

    #[test]
    fn a_store_that_spans_many_blocks_reads_back_in_any_order() {
        // Every document has to be findable through the directory, and the
        // order they are asked for is the order a hit list arrives in, which is
        // not the order they were written.
        let bodies: Vec<String> = (0..2_000)
            .map(|i| {
                format!(
                    "document {i} {}",
                    "some fairly ordinary prose ".repeat(i % 40)
                )
            })
            .collect();
        let mut writer = Writer::new();
        for body in &bodies {
            writer
                .push([("body", body.as_bytes())])
                .expect("a couple of thousand documents fit");
        }
        let bytes = writer.finish().expect("fits");
        let store = Reader::new(&bytes).expect("reads");
        assert!(
            store.blocks() > 20,
            "{} blocks, which is not enough to be testing blocks",
            store.blocks()
        );

        let mut scratch = Scratch::new();
        // A shuffle without a random number generator: step through by a stride
        // that shares no factor with the count, which visits every document.
        let mut doc = 0usize;
        for _ in 0..bodies.len() {
            doc = (doc + 997) % bodies.len();
            let record = store.get(doc as DocId, &mut scratch).expect("is there");
            assert_eq!(
                record.field("body").expect("decodes"),
                Some(bodies[doc].as_bytes()),
                "document {doc}"
            );
        }
    }

    #[test]
    fn prose_is_smaller_in_the_store_than_it_was_outside_it() {
        // The reason the store is blocked at all. If this stops holding, the
        // compression is not earning the indirection.
        let body = "the quick brown fox jumps over the lazy dog while the dog sleeps ";
        let mut writer = Writer::new();
        let mut raw = 0;
        for i in 0..500 {
            let text = format!("{body}{i}");
            raw += text.len();
            writer.push([("body", text.as_bytes())]).expect("fits");
        }
        let bytes = writer.finish().expect("fits");
        assert!(
            bytes.len() < raw / 4,
            "{} bytes for {raw} bytes of text",
            bytes.len()
        );
    }

    #[test]
    fn no_block_holds_more_than_a_block_of_documents_that_fit_in_one() {
        // The property the lookup cost rests on. A block that overshot by the
        // length of whatever document closed it would be decompressed in full
        // to read any one document in it.
        let mut writer = Writer::new();
        for i in 0..400 {
            let body = "x".repeat(BLOCK / 3 + i % 97);
            writer.push([("body", body.as_bytes())]).expect("fits");
        }
        let bytes = writer.finish().expect("fits");
        let store = Reader::new(&bytes).expect("reads");
        let mut widest = 0;
        for at in 0..store.blocks() {
            let (_, _, raw, _) = store.entry(at).expect("the directory is there");
            widest = widest.max(raw);
        }
        assert!(
            widest <= BLOCK,
            "a block of {widest} bytes against a block size of {BLOCK}"
        );
    }

    #[test]
    fn a_document_larger_than_a_block_is_a_block() {
        let big = "x".repeat(BLOCK * 3);
        let mut writer = Writer::new();
        writer.push([("small", &b"before"[..])]).expect("fits");
        writer.push([("big", big.as_bytes())]).expect("fits");
        writer.push([("small", &b"after"[..])]).expect("fits");
        let bytes = writer.finish().expect("fits");
        let store = Reader::new(&bytes).expect("reads");
        let mut scratch = Scratch::new();
        assert_eq!(
            store
                .get(1, &mut scratch)
                .expect("is there")
                .field("big")
                .expect("decodes"),
            Some(big.as_bytes())
        );
        let mut scratch = Scratch::new();
        assert_eq!(
            store
                .get(2, &mut scratch)
                .expect("is there")
                .field("small")
                .expect("decodes"),
            Some(&b"after"[..])
        );
    }

    #[test]
    fn a_scratch_holds_the_last_block_and_not_the_wrong_one() {
        // Two stores number their blocks the same way, so a scratch that
        // remembers only the number would answer the second store from the
        // first store's bytes.
        let first = build(&corpus());
        let second = build(&[vec![("id", "z"), ("title", "elsewhere")]]);
        let one = Reader::new(&first).expect("reads");
        let two = Reader::new(&second).expect("reads");
        let mut scratch = Scratch::new();
        assert_eq!(
            one.get(0, &mut scratch)
                .expect("is there")
                .field("id")
                .expect("decodes"),
            Some(&b"a"[..])
        );
        assert_eq!(
            two.get(0, &mut scratch)
                .expect("is there")
                .field("id")
                .expect("decodes"),
            Some(&b"z"[..])
        );
    }

    #[test]
    fn asking_for_a_document_that_is_not_there_is_an_error() {
        let bytes = build(&corpus());
        let store = Reader::new(&bytes).expect("reads");
        let mut scratch = Scratch::new();
        assert!(store.get(3, &mut scratch).is_err());
        assert!(store.get(DocId::MAX, &mut scratch).is_err());
    }

    #[test]
    fn an_empty_store_is_valid() {
        let bytes = Writer::new().finish().expect("fits");
        let store = Reader::new(&bytes).expect("reads");
        assert!(store.is_empty());
        assert_eq!(store.len(), 0);
        assert!(store.names().is_empty());
        assert_eq!(store.blocks(), 0);
        assert!(store.get(0, &mut Scratch::new()).is_err());
    }

    #[test]
    fn a_truncated_store_is_an_error_not_a_panic() {
        let bytes = build(&corpus());
        for cut in 0..bytes.len() {
            let Ok(store) = Reader::new(&bytes[..cut]) else {
                continue;
            };
            let mut scratch = Scratch::new();
            for doc in 0..4 {
                let Ok(record) = store.get(doc, &mut scratch) else {
                    continue;
                };
                let _ = record.field("title");
                let mut walk = record;
                while let Ok(Some(_)) = walk.next_field() {}
                scratch = Scratch::new();
            }
        }
    }
}
