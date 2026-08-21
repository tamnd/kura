//! The term dictionary.
//!
//! Everything the index knows about a term other than which documents it is in:
//! how many documents that is, and where in the postings section the list lives.
//! A query starts here, once per term, and what it finds decides how much work
//! the rest of the query does.
//!
//! # Shape
//!
//! Terms are stored in order, in blocks of sixteen, with the shared prefix
//! folded out. A vocabulary is full of words that begin alike, and the sort puts
//! those next to each other, so storing how much of the previous term a term
//! reuses costs one byte and saves the rest. On real text it is the difference
//! between a dictionary that fits in cache and one that does not.
//!
//! In front of the blocks is one entry per block holding that block's first term
//! and where the block starts. That is what is searched. A lookup binary
//! searches the front for the block a term would be in, then walks that block,
//! which is at most sixteen terms and one cache line or two. Two misses for a
//! term lookup, against the twenty a search over every term would take.
//!
//! # Why not a hash table
//!
//! A hash table would answer an exact term faster. It would also lose the order,
//! and the order is what a prefix query, a range query and a merge of two
//! segments are all built on. Those are worth more than the difference between
//! two cache misses and one, on a path a query takes a handful of times rather
//! than a million times.

use crate::codec::{get_u32, get_uvarint, put_u32, put_uvarint, split_at};
use crate::error::{Error, Result};

/// How many terms share one block, and so one full term at the front of it.
///
/// Sixteen is small enough that walking a block is a scan of one or two cache
/// lines and large enough that the block index stays a fifteenth of the terms.
pub const BLOCK_TERMS: usize = 16;

/// What the dictionary holds about one term.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Entry {
    /// How many documents the term appears in.
    ///
    /// It is here rather than only in the posting list because a query needs it
    /// to order the terms it is about to walk, and it needs that before it has
    /// read any postings at all.
    pub docs: u32,

    /// Where the term's posting list starts, as a byte offset into the postings
    /// section.
    pub offset: u64,

    /// How long the posting list is, in bytes.
    pub len: u64,
}

/// Builds a term dictionary from terms in ascending order.
#[derive(Debug, Default)]
pub struct Writer {
    /// The encoded blocks.
    blocks: Vec<u8>,
    /// One entry per block: its first term and where it starts.
    index: Vec<u8>,
    /// Where each index entry starts, so the index can be searched without
    /// being walked.
    index_offsets: Vec<u32>,

    /// The previous term, which the next one folds its prefix against.
    previous: Vec<u8>,
    /// The previous posting offset, which the next one is stored as a gap from.
    previous_offset: u64,
    /// How many terms are in the block being built.
    filled: usize,
    terms: u32,
}

impl Writer {
    /// Returns an empty writer.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends a term.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotSorted`] if `term` is not strictly after the previous
    /// one. The block index, the prefix folding and every lookup all rest on the
    /// order, so it is checked rather than assumed. The identifier in the error
    /// is the position rather than the term, because the error type carries a
    /// number and the caller knows what it just pushed.
    ///
    /// Returns [`Error::Overflow`] if the blocks have grown past what a four
    /// byte offset can address.
    pub fn push(&mut self, term: &[u8], entry: Entry) -> Result<()> {
        if self.terms > 0 && term <= self.previous.as_slice() {
            return Err(Error::NotSorted { at: self.terms });
        }

        if self.filled == 0 {
            // The first term of a block goes into the index whole, and into the
            // block whole, so that a lookup that lands here can read it without
            // the block before it. Its posting offset goes in whole for the same
            // reason: a block that needed the block before it to say where its
            // lists are would not be a block anything could jump into.
            let start = u32::try_from(self.index.len()).map_err(|_| Error::Overflow)?;
            let offset = u32::try_from(self.blocks.len()).map_err(|_| Error::Overflow)?;
            self.index_offsets.push(start);
            put_uvarint(&mut self.index, term.len() as u64);
            self.index.extend_from_slice(term);
            put_u32(&mut self.index, offset);

            put_uvarint(&mut self.blocks, term.len() as u64);
            self.blocks.extend_from_slice(term);
            self.previous_offset = 0;
        } else {
            let shared = shared_prefix(&self.previous, term);
            put_uvarint(&mut self.blocks, shared as u64);
            put_uvarint(&mut self.blocks, (term.len() - shared) as u64);
            self.blocks.extend_from_slice(&term[shared..]);
        }

        put_uvarint(&mut self.blocks, u64::from(entry.docs));
        // Posting lists are written in term order, so the offset only ever goes
        // forward and the gap is a byte or two where the offset would be five.
        let gap = entry
            .offset
            .checked_sub(self.previous_offset)
            .ok_or(Error::NotSorted { at: self.terms })?;
        put_uvarint(&mut self.blocks, gap);
        put_uvarint(&mut self.blocks, entry.len);
        self.previous_offset = entry.offset;

        self.previous.clear();
        self.previous.extend_from_slice(term);
        self.terms += 1;
        self.filled += 1;
        if self.filled == BLOCK_TERMS {
            self.filled = 0;
        }
        Ok(())
    }

    /// Finishes the dictionary and returns the encoded bytes.
    #[must_use]
    pub fn finish(self) -> Vec<u8> {
        let blocks = self.index_offsets.len();
        let mut out = Vec::with_capacity(self.blocks.len() + self.index.len() + blocks * 4 + 24);
        put_uvarint(&mut out, u64::from(self.terms));
        put_uvarint(&mut out, blocks as u64);
        for offset in &self.index_offsets {
            put_u32(&mut out, *offset);
        }
        put_uvarint(&mut out, self.index.len() as u64);
        out.extend_from_slice(&self.index);
        put_uvarint(&mut out, self.blocks.len() as u64);
        out.extend_from_slice(&self.blocks);
        out
    }
}

/// Reads a term dictionary written by [`Writer`].
///
/// Opening one reads the header and nothing else. The terms stay where they are
/// and are compared in place, so a lookup allocates a buffer for the one term it
/// is reconstructing and nothing more.
#[derive(Debug, Clone, Copy)]
pub struct Reader<'a> {
    terms: u32,
    blocks: usize,
    offsets: &'a [u8],
    index: &'a [u8],
    body: &'a [u8],
}

impl<'a> Reader<'a> {
    /// Parses the header of an encoded dictionary.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Truncated`] if the input ends inside the header, the
    /// block index or the blocks, and [`Error::Overflow`] if a length does not
    /// decode.
    pub fn new(input: &'a [u8]) -> Result<Self> {
        let (terms, rest) = get_uvarint(input)?;
        let (blocks, rest) = get_uvarint(rest)?;
        let blocks = usize::try_from(blocks).map_err(|_| Error::Overflow)?;

        let offsets_len = blocks.checked_mul(4).ok_or(Error::Overflow)?;
        let (offsets, rest) = split_at(rest, offsets_len)?;

        let (index_len, rest) = get_uvarint(rest)?;
        let index_len = usize::try_from(index_len).map_err(|_| Error::Overflow)?;
        let (index, rest) = split_at(rest, index_len)?;

        let (body_len, rest) = get_uvarint(rest)?;
        let body_len = usize::try_from(body_len).map_err(|_| Error::Overflow)?;
        let (body, _) = split_at(rest, body_len)?;

        Ok(Self {
            terms: u32::try_from(terms).map_err(|_| Error::Overflow)?,
            blocks,
            offsets,
            index,
            body,
        })
    }

    /// How many terms the dictionary holds.
    #[must_use]
    pub const fn len(&self) -> u32 {
        self.terms
    }

    /// Reports whether the dictionary is empty.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.terms == 0
    }

    /// Looks a term up.
    ///
    /// Returns `None` for a term the dictionary does not hold, which is the
    /// common answer on a real query and is answered without decoding a block
    /// when the term falls outside every block's range.
    ///
    /// # Errors
    ///
    /// Returns an error if the block index or the block the search lands in is
    /// truncated or malformed.
    pub fn get(&self, term: &[u8]) -> Result<Option<Entry>> {
        let Some(block) = self.block_for(term)? else {
            return Ok(None);
        };
        let mut walk = self.walk(block)?;
        while let Some((found, entry)) = walk.next_term()? {
            match found.cmp(term) {
                std::cmp::Ordering::Less => {}
                std::cmp::Ordering::Equal => return Ok(Some(entry)),
                // The block is in order, so the first term past the one being
                // looked for settles it.
                std::cmp::Ordering::Greater => return Ok(None),
            }
        }
        Ok(None)
    }

    /// Walks every term in the dictionary, in order.
    ///
    /// For the tools rather than for a query. Nothing on the query path wants
    /// every term, and anything that did would be a scan where a lookup would
    /// do. What does want them is `verify`, which has to decode each posting
    /// list to find out whether it decodes, and cannot ask for a list without
    /// first knowing which term it belongs to.
    #[must_use]
    pub const fn entries(&self) -> Entries<'a> {
        Entries {
            reader: *self,
            block: 0,
            walk: None,
        }
    }

    /// The index of the last block whose first term is not after `term`, which
    /// is the only block that can hold it.
    fn block_for(&self, term: &[u8]) -> Result<Option<usize>> {
        if self.blocks == 0 {
            return Ok(None);
        }
        let (mut low, mut high) = (0usize, self.blocks);
        while low < high {
            let middle = low + (high - low) / 2;
            let (first, _) = self.index_entry(middle)?;
            if first <= term {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        // Every block starts after the term, so no block can hold it.
        Ok((low > 0).then(|| low - 1))
    }

    /// The first term of a block and the byte offset the block starts at.
    fn index_entry(&self, block: usize) -> Result<(&'a [u8], usize)> {
        let at = block.checked_mul(4).ok_or(Error::Overflow)?;
        let slot = self.offsets.get(at..at + 4).ok_or(Error::Truncated {
            needed: at + 4,
            available: self.offsets.len(),
        })?;
        let (start, _) = get_u32(slot)?;
        let rest = self.index.get(start as usize..).ok_or(Error::Truncated {
            needed: start as usize,
            available: self.index.len(),
        })?;

        let (len, rest) = get_uvarint(rest)?;
        let len = usize::try_from(len).map_err(|_| Error::Overflow)?;
        let (first, rest) = split_at(rest, len)?;
        let (offset, _) = get_u32(rest)?;
        Ok((first, offset as usize))
    }

    /// Starts a walk over one block.
    fn walk(&self, block: usize) -> Result<Walk<'a>> {
        let (_, offset) = self.index_entry(block)?;
        let end = if block + 1 < self.blocks {
            self.index_entry(block + 1)?.1
        } else {
            self.body.len()
        };
        let bytes = self
            .body
            .get(offset..end.max(offset))
            .ok_or(Error::Truncated {
                needed: end,
                available: self.body.len(),
            })?;
        Ok(Walk {
            rest: bytes,
            term: Vec::new(),
            offset: 0,
            first: true,
        })
    }
}

/// A walk over every term in a dictionary, in order.
///
/// Made by [`Reader::entries`]. It is not an [`Iterator`], because a term is
/// handed back as a slice of a buffer the walk owns and rewrites in place, and
/// terms are stored as suffixes onto each other, so a term that outlived the
/// call fetching the next one would be a term that had been overwritten.
#[derive(Debug)]
pub struct Entries<'a> {
    /// The dictionary being walked, which is five words and copies freely.
    reader: Reader<'a>,
    /// The next block to open, so the walk below is over the block before it.
    block: usize,
    /// The block being walked, or nothing before the first and after the last.
    walk: Option<Walk<'a>>,
}

impl Entries<'_> {
    /// Reads the next term and its entry, or `None` past the end of the last
    /// block.
    ///
    /// # Errors
    ///
    /// Returns whatever decoding a block returns, which is the point of having
    /// this at all: a dictionary that has been damaged stops here, at the term
    /// where the damage starts, rather than quietly reporting fewer terms than
    /// it holds.
    pub fn next_term(&mut self) -> Result<Option<(&[u8], Entry)>> {
        // Blocks are opened here and not inside the read below, because a borrow
        // of the walk that outlives one turn of a loop is a borrow the compiler
        // will not grant. This leaves the walk on a block that has something in
        // it, so the read after the loop either answers or is the end.
        while self.walk.as_ref().is_none_or(Walk::is_done) {
            if self.block >= self.reader.blocks {
                return Ok(None);
            }
            self.walk = Some(self.reader.walk(self.block)?);
            self.block += 1;
        }
        match &mut self.walk {
            Some(walk) => walk.next_term(),
            // Unreachable, since the loop above either left a walk in place or
            // returned. Written out rather than unwrapped so that this cannot
            // be the thing that panics on a damaged file.
            None => Ok(None),
        }
    }
}

/// A walk over the terms of one block.
///
/// It carries the term it last produced, because the next one is stored as a
/// suffix onto a prefix of it.
#[derive(Debug)]
struct Walk<'a> {
    rest: &'a [u8],
    term: Vec<u8>,
    offset: u64,
    first: bool,
}

impl Walk<'_> {
    /// Whether this block has nothing left in it.
    const fn is_done(&self) -> bool {
        self.rest.is_empty()
    }

    /// Reads the next term and its entry, or `None` at the end of the block.
    fn next_term(&mut self) -> Result<Option<(&[u8], Entry)>> {
        if self.rest.is_empty() {
            return Ok(None);
        }

        if self.first {
            let (len, rest) = get_uvarint(self.rest)?;
            let len = usize::try_from(len).map_err(|_| Error::Overflow)?;
            let (whole, rest) = split_at(rest, len)?;
            self.term.clear();
            self.term.extend_from_slice(whole);
            self.rest = rest;
            self.first = false;
        } else {
            let (shared, rest) = get_uvarint(self.rest)?;
            let shared = usize::try_from(shared).map_err(|_| Error::Overflow)?;
            if shared > self.term.len() {
                return Err(Error::BadPrefix {
                    shared,
                    available: self.term.len(),
                });
            }
            let (len, rest) = get_uvarint(rest)?;
            let len = usize::try_from(len).map_err(|_| Error::Overflow)?;
            let (suffix, rest) = split_at(rest, len)?;
            self.term.truncate(shared);
            self.term.extend_from_slice(suffix);
            self.rest = rest;
        }

        let (docs, rest) = get_uvarint(self.rest)?;
        let (gap, rest) = get_uvarint(rest)?;
        let (len, rest) = get_uvarint(rest)?;
        self.rest = rest;
        self.offset = self.offset.checked_add(gap).ok_or(Error::Overflow)?;

        Ok(Some((
            self.term.as_slice(),
            Entry {
                docs: u32::try_from(docs).map_err(|_| Error::Overflow)?,
                offset: self.offset,
                len,
            },
        )))
    }
}

/// How many leading bytes two terms have in common.
fn shared_prefix(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b).take_while(|(x, y)| x == y).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a dictionary from terms, giving each one a posting list that
    /// starts where the one before it ended.
    fn build(terms: &[&str]) -> Vec<u8> {
        let mut writer = Writer::new();
        let mut offset = 0u64;
        for (i, term) in terms.iter().enumerate() {
            let len = (i as u64 % 7) + 1;
            writer
                .push(
                    term.as_bytes(),
                    Entry {
                        docs: u32::try_from(i).expect("small") + 1,
                        offset,
                        len,
                    },
                )
                .expect("ascending input");
            offset += len;
        }
        writer.finish()
    }

    fn vocabulary(count: usize) -> Vec<String> {
        // Words that share long prefixes, which is what a real vocabulary looks
        // like and what the prefix folding is for.
        let mut out: Vec<String> = (0..count).map(|i| format!("configuration{i:06}")).collect();
        out.sort();
        out
    }

    /// Every term a walk produces, with its entry.
    fn walked(encoded: &[u8]) -> Vec<(Vec<u8>, Entry)> {
        let reader = Reader::new(encoded).expect("header");
        let mut entries = reader.entries();
        let mut out = Vec::new();
        while let Some((term, entry)) = entries.next_term().expect("walk") {
            out.push((term.to_vec(), entry));
        }
        out
    }

    #[test]
    fn a_walk_produces_every_term_in_order_and_stops() {
        let words = vocabulary(500);
        let refs: Vec<&str> = words.iter().map(String::as_str).collect();
        let walked = walked(&build(&refs));

        // Five hundred terms is several blocks, so this is also the check that
        // the walk carries on across a block boundary rather than stopping at
        // the end of the first one.
        assert_eq!(walked.len(), words.len());
        let mut offset = 0u64;
        for (i, ((term, entry), word)) in walked.iter().zip(&words).enumerate() {
            let len = (i as u64 % 7) + 1;
            assert_eq!(term, word.as_bytes());
            assert_eq!(entry.docs, u32::try_from(i).expect("small") + 1);
            assert_eq!(entry.offset, offset);
            assert_eq!(entry.len, len);
            offset += len;
        }
    }

    #[test]
    fn a_walk_agrees_with_looking_each_term_up() {
        // The two paths share the block decoder and nothing else. A walk that
        // drifted from a lookup would mean `verify` checking a posting list that
        // no query would ever ask for.
        let words = vocabulary(300);
        let refs: Vec<&str> = words.iter().map(String::as_str).collect();
        let encoded = build(&refs);
        let reader = Reader::new(&encoded).expect("header");

        for (term, entry) in walked(&encoded) {
            assert_eq!(reader.get(&term).expect("lookup"), Some(entry));
        }
    }

    #[test]
    fn a_walk_over_an_empty_dictionary_is_empty() {
        assert!(walked(&Writer::new().finish()).is_empty());
    }

    #[test]
    fn a_walk_that_has_ended_stays_ended() {
        // Asking again after the end has to keep saying no. Calling code that
        // loops until `None` would otherwise loop forever the moment a block
        // boundary landed on the last term.
        let encoded = build(&["alpha", "beta"]);
        let reader = Reader::new(&encoded).expect("header");
        let mut entries = reader.entries();
        while entries.next_term().expect("walk").is_some() {}
        assert!(entries.next_term().expect("walk").is_none());
        assert!(entries.next_term().expect("walk").is_none());
    }

    #[test]
    fn finds_every_term_it_was_given() {
        let words = vocabulary(500);
        let refs: Vec<&str> = words.iter().map(String::as_str).collect();
        let encoded = build(&refs);
        let reader = Reader::new(&encoded).expect("header");

        assert_eq!(reader.len(), 500);
        let mut offset = 0u64;
        for (i, word) in words.iter().enumerate() {
            let len = (i as u64 % 7) + 1;
            let found = reader.get(word.as_bytes()).expect("lookup");
            assert_eq!(
                found,
                Some(Entry {
                    docs: u32::try_from(i).expect("small") + 1,
                    offset,
                    len,
                }),
                "term {word}"
            );
            offset += len;
        }
    }

    #[test]
    fn a_term_that_is_not_there_is_not_found() {
        let encoded = build(&["alpha", "beta", "delta", "gamma"]);
        let reader = Reader::new(&encoded).expect("header");

        for missing in ["", "a", "alph", "alphab", "betaa", "cider", "zeta"] {
            assert_eq!(
                reader.get(missing.as_bytes()).expect("lookup"),
                None,
                "found {missing}"
            );
        }
    }

    #[test]
    fn an_empty_dictionary_is_valid() {
        let encoded = build(&[]);
        let reader = Reader::new(&encoded).expect("header");
        assert!(reader.is_empty());
        assert_eq!(reader.get(b"anything").expect("lookup"), None);
    }

    #[test]
    fn terms_out_of_order_are_refused() {
        let mut writer = Writer::new();
        writer.push(b"beta", Entry::default()).expect("first");
        assert_eq!(
            writer.push(b"alpha", Entry::default()),
            Err(Error::NotSorted { at: 1 })
        );
        assert_eq!(
            writer.push(b"beta", Entry::default()),
            Err(Error::NotSorted { at: 1 })
        );
    }

    #[test]
    fn a_posting_offset_that_goes_backwards_is_refused() {
        // Posting lists are written in term order, so an offset behind the one
        // before it means the caller built the two sections out of step.
        let mut writer = Writer::new();
        writer
            .push(
                b"alpha",
                Entry {
                    docs: 1,
                    offset: 100,
                    len: 4,
                },
            )
            .expect("first");
        assert!(matches!(
            writer.push(
                b"beta",
                Entry {
                    docs: 1,
                    offset: 40,
                    len: 4
                }
            ),
            Err(Error::NotSorted { .. })
        ));
    }

    #[test]
    fn works_across_every_block_boundary() {
        // One term short of a block, exactly a block, one over, and on to a few
        // blocks, which is where the index and the walk have to agree.
        for count in 0..BLOCK_TERMS * 3 + 2 {
            let words = vocabulary(count);
            let refs: Vec<&str> = words.iter().map(String::as_str).collect();
            let encoded = build(&refs);
            let reader = Reader::new(&encoded).expect("header");
            assert_eq!(reader.len() as usize, count, "count {count}");
            for word in &words {
                assert!(
                    reader.get(word.as_bytes()).expect("lookup").is_some(),
                    "count {count}, missing {word}"
                );
            }
        }
    }

    #[test]
    fn a_truncated_dictionary_is_an_error_not_a_panic() {
        let words = vocabulary(100);
        let refs: Vec<&str> = words.iter().map(String::as_str).collect();
        let encoded = build(&refs);

        for len in 0..encoded.len() {
            if let Ok(reader) = Reader::new(&encoded[..len]) {
                let _ = reader.get(b"configuration000042");
                let _ = reader.get(b"");
                let _ = reader.get(&[0xff; 64]);
            }
        }
    }

    #[test]
    fn folding_the_prefix_is_what_makes_it_small() {
        // The whole dictionary, including the three numbers it stores per term
        // and the block index, comes out smaller than the terms would be on
        // their own. That is only possible because the shared prefix is stored
        // once per block rather than once per term.
        let words = vocabulary(1_000);
        let encoded = build(&words.iter().map(String::as_str).collect::<Vec<_>>());
        let raw: usize = words.iter().map(String::len).sum();
        assert!(
            encoded.len() < raw,
            "encoded {} bytes for {raw} bytes of terms",
            encoded.len()
        );
    }
}
