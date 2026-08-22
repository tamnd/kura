//! What one document puts into the log.
//!
//! The log ring carries records and does not care what is in them, so this is
//! the other half: the bytes one document is worth on its way into the log, and
//! the reading of them that a recovery turns back into postings.
//!
//! # The analysed form rather than the text
//!
//! Recovery has to rebuild what was in memory when the machine stopped, and
//! there are two ways to do that. Keep the text and analyse it again, or keep
//! what the analyser produced.
//!
//! Keeping the text is fewer bytes and it makes recovery cost an analysis pass
//! over everything the log holds, which is the most expensive thing in the
//! ingest path and the last thing worth paying for while a store is coming back
//! up. It also makes recovery depend on the analyser being exactly what it was
//! when the record was written, so a change to the tokeniser quietly rebuilds a
//! different index out of the same log, and nothing about that failure looks
//! like a failure.
//!
//! Keeping the analysed form costs more bytes and takes both of those away.
//!
//! # A stream of tokens rather than terms with counts
//!
//! The same document as a list of distinct terms with a frequency against each
//! would be smaller, because a term said twenty times would be written once.
//!
//! What it would throw away is the order, and the order is what a phrase query
//! is, so a record in that shape could not be replayed into an index that knows
//! where in a document a word was. The index here does not store positions yet
//! and the log is the one part of the file that has to be written before it is
//! read, so it is the part where guessing wrong is dearest.
//!
//! The stream is also free to write. The analyser hands each token to a
//! callback, the callback appends it, and there is no map and no sort anywhere
//! on the write path. A frequency list needs one of each per document.
//!
//! # Shape
//!
//! ```text
//! flags        1 byte, bit 0 for a key, bit 1 for stored fields
//! key          varint length, then the bytes             (if bit 0)
//! tokens       varint count, varint bytes, then the tokens
//! token        varint length, then the bytes
//! fields       varint count, then the fields             (if bit 1)
//! field        varint name length, name, varint value length, value
//! ```
//!
//! The token region carries its byte length as well as its count so a reader
//! can step over it and reach the fields without decoding a token, and so that a
//! replay can check what it decoded against what was meant to be there. The
//! count is what the document's length is scored against, and a count that
//! disagreed with the tokens present would be an index whose norms are wrong
//! rather than an index that fails to build, which is the kind of damage nobody
//! finds for a month.
//!
//! # What is not here
//!
//! A deletion that is not part of a replacement. Replaying a keyed record is a
//! replacement already, because the ingest path looks the key up and deletes
//! what it finds, so the only case missing is a caller deleting a document
//! without writing another one, and that record can be added beside this one
//! when the delete path exists.

use crate::codec::{get_uvarint, put_uvarint, split_at};
use crate::error::{Error, Result};

/// The kind an upsert record is appended under.
///
/// The log stores a kind per record so a replay can tell what it is reading
/// before it reads it, and a store that comes back holding records of a kind it
/// does not know about is a store written by something newer.
pub const KIND: u32 = 1;

/// Bit zero of the flags, set when the document has a key.
const HAS_KEY: u8 = 1;

/// Bit one of the flags, set when the document has stored fields.
const HAS_FIELDS: u8 = 2;

/// A document being built into the bytes that go in the log.
///
/// Held across documents and cleared between them, because an ingest run writes
/// one of these per document and an allocation per document is a cost nobody
/// asked for.
#[derive(Debug, Default)]
pub struct Upsert {
    /// The key, if the document has one.
    key: Vec<u8>,
    /// Whether there is a key, since a key can legitimately be empty bytes.
    keyed: bool,
    /// The tokens, each as a length and its bytes.
    tokens: Vec<u8>,
    /// How many of them there are.
    count: u64,
    /// The stored fields, each as a name and a value.
    fields: Vec<u8>,
    /// How many of those there are.
    values: u64,
}

impl Upsert {
    /// An empty record.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Forgets what was in it, keeping the memory it has.
    pub fn clear(&mut self) {
        self.key.clear();
        self.keyed = false;
        self.tokens.clear();
        self.count = 0;
        self.fields.clear();
        self.values = 0;
    }

    /// Names the document.
    ///
    /// Called once. A second call replaces the key rather than adding one,
    /// because a document has one key or none and the writer that takes these
    /// makes the same rule.
    pub fn key(&mut self, key: &[u8]) {
        self.key.clear();
        self.key.extend_from_slice(key);
        self.keyed = true;
    }

    /// Appends one token, in the order the analyser produced it.
    pub fn token(&mut self, token: &[u8]) {
        put_uvarint(&mut self.tokens, token.len() as u64);
        self.tokens.extend_from_slice(token);
        self.count += 1;
    }

    /// Appends one stored field.
    pub fn field(&mut self, name: &str, value: &[u8]) {
        put_uvarint(&mut self.fields, name.len() as u64);
        self.fields.extend_from_slice(name.as_bytes());
        put_uvarint(&mut self.fields, value.len() as u64);
        self.fields.extend_from_slice(value);
        self.values += 1;
    }

    /// How many tokens have been appended.
    #[must_use]
    pub const fn tokens(&self) -> u64 {
        self.count
    }

    /// Writes the record into `out`, which is what gets appended to the log.
    pub fn write_to(&self, out: &mut Vec<u8>) {
        let mut flags = 0;
        if self.keyed {
            flags |= HAS_KEY;
        }
        if self.values > 0 {
            flags |= HAS_FIELDS;
        }
        out.push(flags);
        if self.keyed {
            put_uvarint(out, self.key.len() as u64);
            out.extend_from_slice(&self.key);
        }
        put_uvarint(out, self.count);
        put_uvarint(out, self.tokens.len() as u64);
        out.extend_from_slice(&self.tokens);
        if self.values > 0 {
            put_uvarint(out, self.values);
            out.extend_from_slice(&self.fields);
        }
    }

    /// The record on its own, for a caller with nowhere to put it.
    #[must_use]
    pub fn bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.tokens.len() + self.fields.len() + 32);
        self.write_to(&mut out);
        out
    }
}

/// One document as it was written into the log.
///
/// Borrowed from the bytes rather than copied out of them, because a replay
/// walks the ring in place and the only thing it does with a record is feed it
/// to a writer.
#[derive(Debug, Clone, Copy)]
pub struct Record<'a> {
    /// The key, if the document had one.
    key: Option<&'a [u8]>,
    /// How many tokens the record says it holds.
    count: u64,
    /// The tokens, end to end.
    tokens: &'a [u8],
    /// How many stored fields it says it holds.
    values: u64,
    /// The fields, end to end.
    fields: &'a [u8],
}

impl<'a> Record<'a> {
    /// Reads a record.
    ///
    /// The regions are checked here and the contents are not: a token is
    /// decoded when it is walked to, so a record whose tokens run past the
    /// region they were given fails on the token rather than on the header.
    /// What this does establish is that the three regions are inside the bytes
    /// and do not overlap, which is what makes walking them separately safe.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Truncated`] if a length runs past the end of the bytes,
    /// and [`Error::BadRecord`] if the flags name something this does not know
    /// about.
    pub fn read(bytes: &'a [u8]) -> Result<Self> {
        let (&flags, rest) = bytes.split_first().ok_or(Error::Truncated {
            needed: 1,
            available: 0,
        })?;
        if flags & !(HAS_KEY | HAS_FIELDS) != 0 {
            return Err(Error::BadRecord {
                length: u32::from(flags),
            });
        }

        let (key, rest) = if flags & HAS_KEY == 0 {
            (None, rest)
        } else {
            let (len, rest) = get_uvarint(rest)?;
            let (key, rest) = split_at(rest, length(len)?)?;
            (Some(key), rest)
        };

        let (count, rest) = get_uvarint(rest)?;
        let (bytes, rest) = get_uvarint(rest)?;
        let (tokens, rest) = split_at(rest, length(bytes)?)?;

        let (values, fields) = if flags & HAS_FIELDS == 0 {
            (0, &rest[..0])
        } else {
            let (values, rest) = get_uvarint(rest)?;
            (values, rest)
        };

        Ok(Self {
            key,
            count,
            tokens,
            values,
            fields,
        })
    }

    /// The key the document was written under, if it had one.
    #[must_use]
    pub const fn key(&self) -> Option<&'a [u8]> {
        self.key
    }

    /// How many tokens the document held, which is its length for scoring.
    #[must_use]
    pub const fn len(&self) -> u64 {
        self.count
    }

    /// Whether the document held no tokens at all.
    ///
    /// A real case rather than a defensive one: a file of punctuation analyses
    /// to nothing, and it is still a document with a key and stored fields.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// How many stored fields it held.
    #[must_use]
    pub const fn values(&self) -> u64 {
        self.values
    }

    /// A walk over the tokens, in the order the analyser produced them.
    #[must_use]
    pub const fn tokens(&self) -> Tokens<'a> {
        Tokens {
            left: self.tokens,
            seen: 0,
            count: self.count,
        }
    }

    /// A walk over the stored fields.
    #[must_use]
    pub const fn fields(&self) -> Fields<'a> {
        Fields {
            left: self.fields,
            seen: 0,
            count: self.values,
        }
    }
}

/// The tokens of one record.
#[derive(Debug, Clone, Copy)]
pub struct Tokens<'a> {
    /// What is left of the region.
    left: &'a [u8],
    /// How many have been handed back.
    seen: u64,
    /// How many there should be.
    count: u64,
}

impl<'a> Tokens<'a> {
    /// The next token, or `None` at the end of them.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Truncated`] if a token runs past the region it is in,
    /// and [`Error::BadRecord`] if the region ends before the count the record
    /// declared is reached or holds bytes after it.
    pub fn next_token(&mut self) -> Result<Option<&'a [u8]>> {
        if self.left.is_empty() {
            if self.seen != self.count {
                return Err(short(self.count));
            }
            return Ok(None);
        }
        if self.seen == self.count {
            return Err(short(self.count));
        }
        let (len, rest) = get_uvarint(self.left)?;
        let (token, rest) = split_at(rest, length(len)?)?;
        self.left = rest;
        self.seen += 1;
        Ok(Some(token))
    }
}

/// The stored fields of one record.
#[derive(Debug, Clone, Copy)]
pub struct Fields<'a> {
    /// What is left of the region.
    left: &'a [u8],
    /// How many have been handed back.
    seen: u64,
    /// How many there should be.
    count: u64,
}

impl<'a> Fields<'a> {
    /// The next field as a name and a value, or `None` at the end of them.
    ///
    /// # Errors
    ///
    /// As [`Tokens::next_token`], and [`Error::BadRecord`] for a name that is
    /// not text, since a field name is a string everywhere else in the engine
    /// and a replay that handed back bytes here would have nowhere to put them.
    pub fn next_field(&mut self) -> Result<Option<(&'a str, &'a [u8])>> {
        if self.left.is_empty() {
            if self.seen != self.count {
                return Err(short(self.count));
            }
            return Ok(None);
        }
        if self.seen == self.count {
            return Err(short(self.count));
        }
        let (len, rest) = get_uvarint(self.left)?;
        let (name, rest) = split_at(rest, length(len)?)?;
        let (len, rest) = get_uvarint(rest)?;
        let (value, rest) = split_at(rest, length(len)?)?;
        let name = core::str::from_utf8(name).map_err(|_| short(self.count))?;
        self.left = rest;
        self.seen += 1;
        Ok(Some((name, value)))
    }
}

/// A length as it is used, refused rather than truncated on a machine where a
/// pointer is narrower than the number in the file.
fn length(value: u64) -> Result<usize> {
    usize::try_from(value).map_err(|_| Error::Overflow)
}

/// The error a region that does not hold what it said it held comes back as.
fn short(count: u64) -> Error {
    Error::BadRecord {
        length: u32::try_from(count).unwrap_or(u32::MAX),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A record with one of everything in it.
    fn whole() -> Vec<u8> {
        let mut record = Upsert::new();
        record.key(b"docs/index.md");
        for token in ["storage", "and", "retrieval", "storage"] {
            record.token(token.as_bytes());
        }
        record.field("path", b"docs/index.md");
        record.field("title", b"Storage and retrieval");
        record.bytes()
    }

    /// The tokens of a record, as text.
    fn tokens(record: &Record<'_>) -> Vec<String> {
        let mut walk = record.tokens();
        let mut out = Vec::new();
        while let Some(token) = walk.next_token().expect("the tokens decode") {
            out.push(String::from_utf8(token.to_vec()).expect("a token is text here"));
        }
        out
    }

    /// The fields of a record, as text.
    fn fields(record: &Record<'_>) -> Vec<(String, String)> {
        let mut walk = record.fields();
        let mut out = Vec::new();
        while let Some((name, value)) = walk.next_field().expect("the fields decode") {
            out.push((
                name.to_string(),
                String::from_utf8(value.to_vec()).expect("a value is text here"),
            ));
        }
        out
    }

    #[test]
    fn a_record_comes_back_as_it_went_in() {
        let bytes = whole();
        let record = Record::read(&bytes).expect("it reads");
        assert_eq!(record.key(), Some(b"docs/index.md".as_slice()));
        assert_eq!(record.len(), 4);
        assert_eq!(tokens(&record), ["storage", "and", "retrieval", "storage"]);
        assert_eq!(record.values(), 2);
        assert_eq!(
            fields(&record),
            [
                ("path".to_string(), "docs/index.md".to_string()),
                ("title".to_string(), "Storage and retrieval".to_string()),
            ]
        );
    }

    #[test]
    fn a_token_said_twice_is_written_twice_and_in_the_order_it_was_said() {
        // The whole reason this is a stream and not a list of terms with counts
        // against them. A record that folded the repeat away could not be
        // replayed into an index that knows where in a document a word was.
        let bytes = whole();
        let record = Record::read(&bytes).expect("it reads");
        let tokens = tokens(&record);
        assert_eq!(tokens.first().map(String::as_str), Some("storage"));
        assert_eq!(tokens.last().map(String::as_str), Some("storage"));
    }

    #[test]
    fn a_document_with_no_key_and_no_fields_is_a_record_of_its_tokens() {
        let mut record = Upsert::new();
        record.token(b"alone");
        let bytes = record.bytes();
        let record = Record::read(&bytes).expect("it reads");
        assert_eq!(record.key(), None);
        assert_eq!(record.values(), 0);
        assert!(
            record
                .fields()
                .next_field()
                .expect("nothing to read")
                .is_none()
        );
        assert_eq!(tokens(&record), ["alone"]);
    }

    #[test]
    fn a_document_that_analysed_to_nothing_is_still_a_document() {
        // A file of punctuation. It has a key and something to show, and the
        // store has to be able to say later that it holds it.
        let mut record = Upsert::new();
        record.key(b"symbols.txt");
        record.field("path", b"symbols.txt");
        let bytes = record.bytes();
        let record = Record::read(&bytes).expect("it reads");
        assert!(record.is_empty());
        assert_eq!(record.len(), 0);
        assert_eq!(record.key(), Some(b"symbols.txt".as_slice()));
        assert_eq!(record.values(), 1);
        assert!(tokens(&record).is_empty());
    }

    #[test]
    fn an_empty_key_is_a_key_and_not_the_absence_of_one() {
        // Bytes the caller chooses, and a caller that chose no bytes chose a
        // key. Reading it back as no key at all would make a document nothing
        // could replace.
        let mut record = Upsert::new();
        record.key(b"");
        record.token(b"nameless");
        let bytes = record.bytes();
        let record = Record::read(&bytes).expect("it reads");
        assert_eq!(record.key(), Some(b"".as_slice()));
    }

    #[test]
    fn a_record_can_be_used_again_without_carrying_the_last_one() {
        let mut record = Upsert::new();
        record.key(b"first");
        record.token(b"one");
        record.field("path", b"first");
        record.clear();
        record.token(b"two");
        let bytes = record.bytes();
        let record = Record::read(&bytes).expect("it reads");
        assert_eq!(record.key(), None);
        assert_eq!(record.values(), 0);
        assert_eq!(tokens(&record), ["two"]);
    }

    #[test]
    fn a_record_cut_short_anywhere_is_refused_rather_than_half_read() {
        let bytes = whole();
        for at in 0..bytes.len() {
            let cut = &bytes[..at];
            let refused = match Record::read(cut) {
                Err(_) => true,
                Ok(record) => {
                    let mut walk = record.tokens();
                    let mut bad = false;
                    loop {
                        match walk.next_token() {
                            Err(_) => {
                                bad = true;
                                break;
                            }
                            Ok(None) => break,
                            Ok(Some(_)) => {}
                        }
                    }
                    let mut walk = record.fields();
                    loop {
                        match walk.next_field() {
                            Err(_) => {
                                bad = true;
                                break;
                            }
                            Ok(None) => break,
                            Ok(Some(_)) => {}
                        }
                    }
                    bad
                }
            };
            assert!(refused, "the record cut at {at} was read as whole");
        }
    }

    #[test]
    fn a_record_that_claims_more_tokens_than_it_holds_is_refused() {
        let mut record = Upsert::new();
        record.token(b"one");
        record.token(b"two");
        let mut bytes = record.bytes();
        // The count sits after the flags byte, and both counts here are one
        // byte, so this is the count and not something else.
        bytes[1] = 3;
        let record = Record::read(&bytes).expect("the regions are still fine");
        let mut walk = record.tokens();
        assert!(walk.next_token().is_ok());
        assert!(walk.next_token().is_ok());
        assert!(
            walk.next_token().is_err(),
            "a region that ran out early was read as the end of the tokens"
        );
    }

    #[test]
    fn a_record_that_claims_fewer_tokens_than_it_holds_is_refused() {
        let mut record = Upsert::new();
        record.token(b"one");
        record.token(b"two");
        let mut bytes = record.bytes();
        bytes[1] = 1;
        let record = Record::read(&bytes).expect("the regions are still fine");
        let mut walk = record.tokens();
        assert!(walk.next_token().is_ok());
        assert!(
            walk.next_token().is_err(),
            "a region with bytes left over was read as the end of the tokens"
        );
    }

    #[test]
    fn a_flag_this_does_not_know_about_is_refused() {
        // A record written by something newer, which is a store that cannot be
        // replayed here rather than a record to read as much of as possible.
        let mut bytes = whole();
        bytes[0] |= 0x80;
        assert!(Record::read(&bytes).is_err());
    }

    #[test]
    fn a_field_name_that_is_not_text_is_refused() {
        let mut record = Upsert::new();
        record.token(b"one");
        record.field("ok", b"value");
        let mut bytes = record.bytes();
        // The name is the four bytes before the value length, and the value is
        // the last five. Bending the first byte of the name is enough.
        let at = bytes.len() - 5 - 1 - 2;
        bytes[at] = 0xff;
        let record = Record::read(&bytes).expect("the regions are still fine");
        assert!(record.fields().next_field().is_err());
    }
}
