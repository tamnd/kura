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
//! Lookup is a direct index into an offset array rather than a walk. A hit list
//! is a scattered set of document numbers by the time it reaches here, so the
//! access pattern is random and any layout that has to scan to find a document
//! is the wrong one.
//!
//! The offsets are four bytes each unless the payload needs more, which is the
//! difference between four and eight bytes per document for every store under
//! four gigabytes, and those are almost all of them. A store of short documents
//! is mostly its offset array, so this is not a small saving.

use crate::DocId;
use crate::codec::{get_uvarint, put_uvarint, split_at};
use crate::error::{Error, Result};

/// Builds a store.
///
/// Documents are pushed in order and numbered from zero, which is the same
/// numbering the index gives them, because the whole point is that a hit from
/// one is a lookup in the other.
#[derive(Debug, Default)]
pub struct Writer {
    names: Vec<String>,
    offsets: Vec<u64>,
    payload: Vec<u8>,
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
    /// identifier can hold.
    pub fn push<'a>(
        &mut self,
        fields: impl IntoIterator<Item = (&'a str, &'a [u8])>,
    ) -> Result<DocId> {
        let doc =
            u32::try_from(self.offsets.len()).map_err(|_| Error::NotSorted { at: u32::MAX })?;
        let start = self.payload.len();
        // The count goes in first and is not known yet, so a placeholder is
        // written and rewritten. It is one byte for anything under 128 fields,
        // and a document with more fields than that is not a document.
        self.payload.push(0);
        let mut count = 0u64;
        for (name, value) in fields {
            let index = self.name(name);
            put_uvarint(&mut self.payload, index);
            put_uvarint(&mut self.payload, value.len() as u64);
            self.payload.extend_from_slice(value);
            count += 1;
        }
        self.values += count;
        if let (true, Some(slot)) = (count < 128, self.payload.get_mut(start)) {
            *slot = u8::try_from(count).unwrap_or(0);
        } else {
            // Rare enough not to be worth a second pass over the common case.
            // The placeholder is dropped and the real count spliced in.
            let mut header = Vec::with_capacity(crate::codec::MAX_VARINT_LEN64);
            put_uvarint(&mut header, count);
            self.payload.splice(start..=start, header);
        }
        self.offsets.push(start as u64);
        Ok(doc)
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

    /// Writes the section.
    #[must_use]
    pub fn finish(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.payload.len() + self.offsets.len() * 8 + 64);
        put_uvarint(&mut out, self.offsets.len() as u64);
        put_uvarint(&mut out, self.names.len() as u64);
        for name in &self.names {
            put_uvarint(&mut out, name.len() as u64);
            out.extend_from_slice(name.as_bytes());
        }
        // Four bytes covers every payload under four gigabytes, which is the
        // size a segment is split at anyway, so the wide form is a fallback
        // rather than a case worth optimising.
        let width: u8 = if self.payload.len() > u32::MAX as usize {
            8
        } else {
            4
        };
        out.push(width);
        put_uvarint(&mut out, self.offsets.len() as u64 * u64::from(width));
        for offset in &self.offsets {
            // The low four bytes of a little endian u64 are the little endian
            // u32, so the narrow form is a prefix of the wide one and neither
            // needs a cast.
            out.extend_from_slice(
                offset
                    .to_le_bytes()
                    .get(..usize::from(width))
                    .unwrap_or_default(),
            );
        }
        put_uvarint(&mut out, self.payload.len() as u64);
        out.extend_from_slice(&self.payload);
        out
    }
}

/// Reads a store.
#[derive(Debug, Clone)]
pub struct Reader<'a> {
    names: Vec<&'a str>,
    offsets: &'a [u8],
    payload: &'a [u8],
    count: u32,
    width: usize,
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
        let (width, rest) = split_at(rest, 1)?;
        let width = match width.first() {
            Some(4) => 4usize,
            Some(8) => 8usize,
            _ => return Err(Error::Overflow),
        };
        let (len, rest) = get_uvarint(rest)?;
        let (offsets, rest) = split_at(rest, usize::try_from(len).map_err(|_| Error::Overflow)?)?;
        if offsets.len() != count as usize * width {
            return Err(Error::Truncated {
                needed: count as usize * width,
                available: offsets.len(),
            });
        }
        let (len, rest) = get_uvarint(rest)?;
        let (payload, _) = split_at(rest, usize::try_from(len).map_err(|_| Error::Overflow)?)?;
        Ok(Self {
            names: resolved,
            offsets,
            payload,
            count,
            width,
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

    /// The values stored for a document.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotSorted`] if there is no such document, and a
    /// decoding error if the record for it is not what it claims to be.
    pub fn get(&self, doc: DocId) -> Result<Document<'a, '_>> {
        if doc >= self.count {
            return Err(Error::NotSorted { at: doc });
        }
        let at = doc as usize * self.width;
        let bytes = self
            .offsets
            .get(at..at + self.width)
            .ok_or(Error::NotSorted { at: doc })?;
        let mut raw = [0u8; 8];
        for (slot, byte) in raw.iter_mut().zip(bytes) {
            *slot = *byte;
        }
        let offset = usize::try_from(u64::from_le_bytes(raw)).map_err(|_| Error::Overflow)?;
        let rest = self.payload.get(offset..).ok_or(Error::Truncated {
            needed: offset,
            available: self.payload.len(),
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
/// back as a slice of the segment rather than a copy of it. A result page asks
/// for two fields out of six, so decoding all six on the way past would be five
/// sixths wasted.
#[derive(Debug, Clone)]
pub struct Document<'a, 'n> {
    names: &'n [&'a str],
    rest: &'a [u8],
    left: u64,
}

impl<'a> Document<'a, '_> {
    /// The next field and its value.
    ///
    /// # Errors
    ///
    /// Returns a decoding error if the record ends in the middle of a value.
    pub fn next_field(&mut self) -> Result<Option<(&'a str, &'a [u8])>> {
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
    pub fn field(&self, name: &str) -> Result<Option<&'a [u8]>> {
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
        writer.finish()
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
        for (doc, fields) in corpus().iter().enumerate() {
            let record = store.get(doc as DocId).expect("the document is there");
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
        let record = store.get(1).expect("is there");
        assert_eq!(record.field("body").expect("decodes"), None);
        assert_eq!(record.field("nothing").expect("decodes"), None);
    }

    #[test]
    fn an_empty_value_is_not_a_missing_value() {
        let bytes = build(&corpus());
        let store = Reader::new(&bytes).expect("reads");
        let record = store.get(2).expect("is there");
        assert_eq!(record.field("body").expect("decodes"), Some(&b""[..]));
    }

    #[test]
    fn the_fields_come_back_in_the_order_they_were_written() {
        let bytes = build(&corpus());
        let store = Reader::new(&bytes).expect("reads");
        let mut record = store.get(2).expect("is there");
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
        let bytes = writer.finish();
        let store = Reader::new(&bytes).expect("reads");
        let mut record = store.get(0).expect("is there");
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
        let bytes = writer.finish();
        let store = Reader::new(&bytes).expect("reads");
        assert_eq!(store.len(), 2);
        let mut record = store.get(0).expect("is there");
        assert!(record.next_field().expect("decodes").is_none());
        assert_eq!(
            store
                .get(1)
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
        let bytes = writer.finish();
        let store = Reader::new(&bytes).expect("reads");
        let record = store.get(0).expect("is there");
        for name in &names {
            assert_eq!(
                record.field(name).expect("decodes"),
                Some(name.as_bytes()),
                "{name}"
            );
        }
        assert_eq!(
            store
                .get(1)
                .expect("is there")
                .field("after")
                .expect("decodes"),
            Some(&b"still fine"[..])
        );
    }

    #[test]
    fn asking_for_a_document_that_is_not_there_is_an_error() {
        let bytes = build(&corpus());
        let store = Reader::new(&bytes).expect("reads");
        assert!(store.get(3).is_err());
        assert!(store.get(DocId::MAX).is_err());
    }

    #[test]
    fn an_empty_store_is_valid() {
        let bytes = Writer::new().finish();
        let store = Reader::new(&bytes).expect("reads");
        assert!(store.is_empty());
        assert_eq!(store.len(), 0);
        assert!(store.names().is_empty());
        assert!(store.get(0).is_err());
    }

    #[test]
    fn a_truncated_store_is_an_error_not_a_panic() {
        let bytes = build(&corpus());
        for cut in 0..bytes.len() {
            let Ok(store) = Reader::new(&bytes[..cut]) else {
                continue;
            };
            for doc in 0..4 {
                let Ok(record) = store.get(doc) else { continue };
                let _ = record.field("title");
                let mut walk = record;
                while let Ok(Some(_)) = walk.next_field() {}
            }
        }
    }
}
