//! The names a caller knows its documents by.
//!
//! A document inside the engine is a number, and the number is where it happens
//! to sit in the segment holding it. That is the right identifier for everything
//! below this line and useless above it: a caller that indexed a page an hour
//! ago and wants to index it again has a URL, not a document number, and a
//! compaction renumbers every document it touches so the number it was given
//! last time would be wrong anyway.
//!
//! So a segment can carry the key of each of its documents. A key is bytes the
//! caller chooses and it means whatever they need it to mean, usually the
//! identifier the document already has somewhere else.
//!
//! # Sorted rather than hashed
//!
//! A hash table would answer faster and would be smaller, because it would not
//! have to keep the keys at all.
//!
//! It would also be wrong occasionally, and the way it would be wrong is that
//! two keys landing in the same place makes a lookup for one of them answer with
//! the other's document. What the answer is used for is deleting the document
//! being replaced, so a collision deletes somebody else's data and nothing
//! anywhere notices. A primary key is the one place in an engine where being
//! nearly always right is not a trade worth taking, so the keys are kept and
//! compared.
//!
//! Keeping them in order costs nothing over keeping them at all, and it buys a
//! binary search, a merge of two segments that is a walk rather than a sort, and
//! a range of keys for anything later that wants one.
//!
//! # Shape
//!
//! A count, the document each key names, where each key starts, and then the
//! keys end to end in ascending order. The two arrays are fixed width so a
//! binary search can land anywhere without decoding what came before it, which
//! is the whole reason not to varint them.
//!
//! That is eight bytes a key on top of the key itself. Real keys are long and
//! alike, so the keys themselves are most of it, and folding out their shared
//! prefixes the way the term dictionary does would take a good fraction off. It
//! is not done here yet, deliberately: a lookup by key happens once per document
//! written rather than millions of times per query, so the simple layout is
//! worth having first and worth measuring before it is made clever.
//!
//! # What is not here
//!
//! Nothing about which segment a key is in. A key lives in exactly one segment
//! and a store finds it by asking the newest segment first, and that walk needs
//! the filter beside this to be worth anything, so it lives with the store
//! rather than here.

use crate::DocId;
use crate::codec::{get_u32, put_u32, split_at};
use crate::error::{Error, Result};

/// The fixed part in front of the arrays.
const HEADER: usize = 8;

/// What one key costs on top of its own bytes.
const PER_KEY: usize = 8;

/// Builds a key table from keys in ascending order.
///
/// Ascending because the search is a binary search and a merge is a walk, and
/// both of those are the order rather than a consequence of it. A caller adding
/// documents in the order they arrive has its keys in no order at all, so
/// something above has to sort them before they get here, and that is where the
/// decision about what a repeated key means belongs as well.
#[derive(Debug, Default, Clone)]
pub struct Writer {
    /// The document each key names, in key order.
    docs: Vec<DocId>,
    /// Where each key starts in the blob.
    offsets: Vec<u32>,
    /// The keys, end to end.
    blob: Vec<u8>,
}

impl Writer {
    /// A table with nothing in it.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends a key and the document it names.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotSorted`] if `key` is not strictly after the key
    /// before it, which covers a repeat of the same key as well as a key that
    /// goes backwards. A segment holding one key twice would have to answer a
    /// lookup with one of two documents and nothing here can say which.
    ///
    /// Returns [`Error::Overflow`] if the keys have grown past what a four byte
    /// offset can address.
    pub fn push(&mut self, key: &[u8], doc: DocId) -> Result<()> {
        if let Some(last) = self.last()
            && key <= last
        {
            return Err(Error::NotSorted {
                at: u32::try_from(self.docs.len()).unwrap_or(u32::MAX),
            });
        }
        let offset = u32::try_from(self.blob.len()).map_err(|_| Error::Overflow)?;
        self.offsets.push(offset);
        self.docs.push(doc);
        self.blob.extend_from_slice(key);
        Ok(())
    }

    /// The key pushed last, for the order check.
    fn last(&self) -> Option<&[u8]> {
        let start = *self.offsets.last()? as usize;
        self.blob.get(start..)
    }

    /// How many keys are in.
    #[must_use]
    pub fn len(&self) -> usize {
        self.docs.len()
    }

    /// Reports whether no key is in.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.docs.is_empty()
    }

    /// What the encoded table will be, in bytes.
    #[must_use]
    pub fn size(&self) -> usize {
        HEADER + self.docs.len() * PER_KEY + self.blob.len()
    }

    /// Writes the table onto the end of `out`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Overflow`] if there are more keys than a four byte count
    /// can hold, which is the same bound a segment's document count already has.
    pub fn write_to(&self, out: &mut Vec<u8>) -> Result<()> {
        let count = u32::try_from(self.docs.len()).map_err(|_| Error::Overflow)?;
        let bytes = u32::try_from(self.blob.len()).map_err(|_| Error::Overflow)?;
        out.reserve(self.size());
        put_u32(out, count);
        put_u32(out, bytes);
        for &doc in &self.docs {
            put_u32(out, doc);
        }
        for &offset in &self.offsets {
            put_u32(out, offset);
        }
        out.extend_from_slice(&self.blob);
        Ok(())
    }

    /// The table on its own.
    ///
    /// # Errors
    ///
    /// As [`write_to`](Self::write_to).
    pub fn finish(&self) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(self.size());
        self.write_to(&mut out)?;
        Ok(out)
    }
}

/// Looks a key up in a written table.
#[derive(Debug, Clone, Copy)]
pub struct Reader<'a> {
    /// How many keys there are.
    count: usize,
    /// The document each key names, four bytes each, in key order.
    docs: &'a [u8],
    /// Where each key starts in the blob, four bytes each.
    offsets: &'a [u8],
    /// The keys, end to end.
    blob: &'a [u8],
}

impl<'a> Reader<'a> {
    /// Reads a table out of the front of `input`.
    ///
    /// The header is checked and the arrays are not, which is the same bargain
    /// every other reader in this crate makes. Opening a segment is not the
    /// place to walk four bytes per document, and every read below is a checked
    /// slice, so a table whose offsets are nonsense answers wrongly rather than
    /// reading somebody else's memory. Telling a nonsense table from a good one
    /// is what the checksum on the section is for.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Truncated`] if the input is shorter than the header, or
    /// than the arrays and keys the header says are there.
    pub fn new(input: &'a [u8]) -> Result<Self> {
        let (head, rest) = split_at(input, HEADER)?;
        let (count, tail) = get_u32(head)?;
        let (bytes, _) = get_u32(tail)?;
        let count = count as usize;
        let (docs, rest) = split_at(rest, count * 4)?;
        let (offsets, rest) = split_at(rest, count * 4)?;
        let (blob, _) = split_at(rest, bytes as usize)?;
        Ok(Self {
            count,
            docs,
            offsets,
            blob,
        })
    }

    /// How many keys the table holds.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.count
    }

    /// Reports whether the table holds no keys.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// How many bytes the table is, header included.
    #[must_use]
    pub const fn size(&self) -> usize {
        HEADER + self.count * PER_KEY + self.blob.len()
    }

    /// The key at a position, in ascending order.
    ///
    /// `None` past the end, and for a table whose offsets do not make sense,
    /// which is the answer that keeps a damaged section from becoming a wrong
    /// document rather than a missing one.
    #[must_use]
    pub fn key(&self, at: usize) -> Option<&'a [u8]> {
        let start = self.offset(at)?;
        let end = if at + 1 == self.count {
            self.blob.len()
        } else {
            self.offset(at + 1)?
        };
        self.blob.get(start..end)
    }

    /// The document the key at a position names.
    #[must_use]
    pub fn doc(&self, at: usize) -> Option<DocId> {
        let bytes = self.docs.get(at * 4..at * 4 + 4)?;
        Some(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    /// Where the key at a position starts in the blob.
    fn offset(&self, at: usize) -> Option<usize> {
        let bytes = self.offsets.get(at * 4..at * 4 + 4)?;
        Some(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize)
    }

    /// The document a key names, or `None` if the table does not hold it.
    ///
    /// A binary search, so a table of a million keys is twenty probes, and the
    /// last few of them are in the same cache line.
    #[must_use]
    pub fn get(&self, key: &[u8]) -> Option<DocId> {
        let mut low = 0;
        let mut high = self.count;
        while low < high {
            let middle = low + (high - low) / 2;
            match self.key(middle)?.cmp(key) {
                core::cmp::Ordering::Less => low = middle + 1,
                core::cmp::Ordering::Greater => high = middle,
                core::cmp::Ordering::Equal => return self.doc(middle),
            }
        }
        None
    }

    /// Every key and the document it names, in ascending order.
    pub fn entries(&self) -> impl Iterator<Item = (&'a [u8], DocId)> + '_ {
        (0..self.count).filter_map(|at| Some((self.key(at)?, self.doc(at)?)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Keys that look like real ones: long, alike, and differing at the end.
    fn key(n: usize) -> Vec<u8> {
        format!("https://example.com/wiki/page/{n:06}").into_bytes()
    }

    /// A table over `count` keys, each naming the document with the same number.
    fn built(count: usize) -> Vec<u8> {
        let mut keys: Vec<_> = (0..count).map(key).collect();
        keys.sort();
        let mut writer = Writer::new();
        for (doc, key) in keys.iter().enumerate() {
            let doc = DocId::try_from(doc).expect("a test corpus fits in a segment");
            writer.push(key, doc).expect("a key");
        }
        writer.finish().expect("a table")
    }

    #[test]
    fn a_key_that_went_in_names_the_document_it_went_in_with() {
        let bytes = built(1_000);
        let reader = Reader::new(&bytes).expect("a table");
        assert_eq!(reader.len(), 1_000);
        // The keys were sorted before they were pushed and the numbers are
        // zero padded, so the key of document n is the nth key.
        for n in 0..1_000 {
            let doc = DocId::try_from(n).expect("a small number");
            assert_eq!(reader.get(&key(n)), Some(doc));
        }
    }

    #[test]
    fn a_key_that_is_not_there_is_absent_rather_than_the_nearest_one() {
        let bytes = built(1_000);
        let reader = Reader::new(&bytes).expect("a table");
        assert_eq!(reader.get(b""), None);
        assert_eq!(reader.get(b"https://example.com/wiki/page/000000x"), None);
        assert_eq!(reader.get(b"https://example.com/wiki/page/001000"), None);
        assert_eq!(reader.get(b"zzz"), None);
        assert_eq!(reader.get(b"a"), None);
    }

    #[test]
    fn a_table_with_nothing_in_it_answers_nothing() {
        let bytes = Writer::new().finish().expect("a table");
        assert_eq!(bytes.len(), HEADER);
        let reader = Reader::new(&bytes).expect("a table");
        assert!(reader.is_empty());
        assert_eq!(reader.get(b"anything"), None);
        assert_eq!(reader.entries().count(), 0);
    }

    #[test]
    fn a_table_of_one_key_answers_it_and_nothing_else() {
        let mut writer = Writer::new();
        writer.push(b"only", 7).expect("a key");
        let bytes = writer.finish().expect("a table");
        let reader = Reader::new(&bytes).expect("a table");
        assert_eq!(reader.get(b"only"), Some(7));
        assert_eq!(reader.get(b"onl"), None);
        assert_eq!(reader.get(b"onlyy"), None);
    }

    #[test]
    fn a_key_that_is_a_prefix_of_another_is_its_own_key() {
        // The case a naive comparison gets wrong, and the reason the length is
        // part of the key rather than a terminator being part of it.
        let mut writer = Writer::new();
        for (key, doc) in [(&b"doc"[..], 1u32), (b"doc/1", 2), (b"doc/10", 3)] {
            writer.push(key, doc).expect("a key");
        }
        let bytes = writer.finish().expect("a table");
        let reader = Reader::new(&bytes).expect("a table");
        assert_eq!(reader.get(b"doc"), Some(1));
        assert_eq!(reader.get(b"doc/1"), Some(2));
        assert_eq!(reader.get(b"doc/10"), Some(3));
        assert_eq!(reader.get(b"doc/100"), None);
    }

    #[test]
    fn an_empty_key_is_a_key() {
        let mut writer = Writer::new();
        writer.push(b"", 4).expect("a key");
        writer.push(b"after", 5).expect("a key");
        let bytes = writer.finish().expect("a table");
        let reader = Reader::new(&bytes).expect("a table");
        assert_eq!(reader.get(b""), Some(4));
        assert_eq!(reader.get(b"after"), Some(5));
    }

    #[test]
    fn a_key_out_of_order_is_refused_and_so_is_the_same_key_twice() {
        let mut writer = Writer::new();
        writer.push(b"b", 0).expect("a key");
        assert!(matches!(
            writer.push(b"a", 1),
            Err(Error::NotSorted { at: 1 })
        ));
        assert!(matches!(
            writer.push(b"b", 2),
            Err(Error::NotSorted { at: 1 })
        ));
        writer.push(b"c", 3).expect("a key");
        assert_eq!(writer.len(), 2);
    }

    #[test]
    fn every_key_comes_back_in_the_order_it_was_written() {
        let bytes = built(200);
        let reader = Reader::new(&bytes).expect("a table");
        let entries: Vec<_> = reader.entries().collect();
        assert_eq!(entries.len(), 200);
        for pair in entries.windows(2) {
            assert!(pair[0].0 < pair[1].0);
        }
        for (at, (key, doc)) in entries.iter().enumerate() {
            assert_eq!(reader.key(at), Some(*key));
            assert_eq!(reader.doc(at), Some(*doc));
        }
        assert_eq!(reader.key(200), None);
        assert_eq!(reader.doc(200), None);
    }

    #[test]
    fn a_table_reads_back_as_the_table_that_was_written() {
        let bytes = built(64);
        let reader = Reader::new(&bytes).expect("a table");
        assert_eq!(reader.size(), bytes.len());
        // And with something after it, which is what a section holding more
        // than the table looks like.
        let mut more = bytes.clone();
        more.extend_from_slice(b"and then some");
        let reader = Reader::new(&more).expect("a table");
        assert_eq!(reader.size(), bytes.len());
        assert_eq!(reader.get(&key(0)), Some(0));
    }

    #[test]
    fn a_table_that_stops_short_is_refused_rather_than_read() {
        let bytes = built(64);
        for cut in [0, 1, HEADER - 1, HEADER, HEADER + 4, bytes.len() - 1] {
            assert!(
                matches!(Reader::new(&bytes[..cut]), Err(Error::Truncated { .. })),
                "cut at {cut}"
            );
        }
    }

    #[test]
    fn a_table_whose_offsets_are_nonsense_answers_nothing_rather_than_reading_past_the_end() {
        let mut bytes = built(16);
        // The offset of the last key, turned into something past the blob.
        let at = HEADER + 16 * 4 + 15 * 4;
        bytes[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        let reader = Reader::new(&bytes).expect("a table");
        assert_eq!(reader.key(15), None);
        // Every lookup that walks over the damaged key gives up rather than
        // guessing, and the ones that do not are still answerable.
        assert_eq!(reader.get(&key(15)), None);
        // Fourteen rather than fifteen, because a key ends where the next one
        // starts, so a broken offset takes out the key in front of it as well.
        assert_eq!(reader.entries().count(), 14);
    }
}
