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
//! which is at most sixteen terms and one cache line or two.
//!
//! The binary search is over the first four bytes of each block's first term,
//! kept as a separate array of one word each rather than read out of the entries
//! themselves. Four bytes padded with zeros sort the same way the whole terms do,
//! so the search is exact wherever the four bytes differ, and a probe that finds
//! them equal is the only one that goes and reads a term. On an English
//! vocabulary that is most probes answered out of an array sixteen blocks to a
//! cache line, and it is what turns a search of two misses a step into one.
//!
//! It costs a quarter of a byte per term, since there is one of them per block
//! of sixteen. A segment of 17.6 megabytes built from 206 megabytes of markdown
//! carries 274973 terms and so 68.7 kilobytes of keys, which is four tenths of
//! one percent of the file.
//!
//! # Searching a block without rebuilding its terms
//!
//! A term in a block is stored as how much of the previous term it reuses and
//! what is left, so the obvious way to look one up is to rebuild each term in
//! turn and compare it. The scan does not do that, and rebuilding is left to the
//! walk in [`Entries`], which is for the tools and does need the terms.
//!
//! Instead the scan carries how many bytes of the term being looked for the
//! current term matches, and every term in a block settles against it by
//! arithmetic on that number. A term that reuses less than that many bytes is
//! already past the one being looked for, so the answer is no and the block
//! stops. A term that reuses more is still behind it and is skipped without
//! looking at a byte of it. Only a term that reuses exactly that many is
//! compared, and then only the part of it that is new. So a lookup reads a
//! handful of bytes rather than rebuilding sixteen terms, and it does it in
//! borrowed memory rather than in a buffer it had to allocate first.
//!
//! # Why not a hash table
//!
//! A hash table would answer an exact term faster. It would also lose the order,
//! and the order is what a prefix query, a range query and a merge of two
//! segments are all built on. Those are worth more than the difference between
//! two cache misses and one, on a path a query takes a handful of times rather
//! than a million times.
//!
//! # Why not a finite state transducer
//!
//! Because it is worth about one percent of a segment, and it costs five times
//! the build and three times the walk, and the walk is what merging two segments
//! and verifying a file are both made of.
//!
//! That was measured rather than argued, against the `fst` crate, which is a
//! general transducer of the kind a term index is usually built on. It is
//! genuinely better at holding terms. On an English word list of 234456 words it
//! stores them in 5.11 bytes each against 6.99 here, and on the vocabulary of
//! ten megabytes of markdown 4.02 against 6.01. That is the shape doing what it
//! is for: a shared suffix is stored once, and front coding only ever folds out a
//! shared prefix.
//!
//! Most of it goes away once the transducer has to carry what the dictionary
//! carries. A term here comes with a document count, a posting offset and a
//! posting length, and a transducer's output is one integer, so two of the three
//! end up in a side table that the output indexes into. On the word list that is
//! 11.83 bytes a term against 12.52 here, which is five percent, because the
//! values are nearly half the dictionary either way and the ordinal the
//! transducer has to emit to find them costs 1.19 bytes a term on top.
//!
//! Five percent of the dictionary is not five percent of a file. Indexing 206
//! megabytes of markdown gives a segment of 17.6 megabytes with the terms at
//! 16.6 percent of it, so what is on offer is nine tenths of one percent of the
//! segment.
//!
//! The speed went the other way once the lookup here stopped allocating and the
//! block search stopped reading a term at every step. A hit on the word list is
//! 123 nanoseconds through the transducer and 147 here, a miss 146 against 172,
//! and a walk of every term 52 nanoseconds against 18. Building the dictionary
//! is 28 nanoseconds a term here and 158 there, which is the minimisation pass,
//! and it is paid on every segment written.
//!
//! So the shape stays. What the measurement bought is the two changes above,
//! which took between a third and a half of a lookup and would not have been
//! found by reading the code: the old one allocated a buffer and rebuilt up to
//! sixteen terms to answer a question about one, and its block search read a
//! term at every step of the binary search.

use crate::codec::{get_u32, get_uvarint, put_u32, put_uvarint, split_at};
use crate::error::{Error, Result};

/// How many terms share one block, and so one full term at the front of it.
///
/// Sixteen is small enough that walking a block is a scan of one or two cache
/// lines and large enough that the block index stays a fifteenth of the terms.
///
/// The trade is the dictionary's size against the length of the scan at the end
/// of a lookup, and it was measured again once the scan stopped rebuilding
/// terms. On an English word list of 234456 words, blocks of eight cost 14.41
/// bytes a term and answer a hit in 112 nanoseconds, sixteen 12.52 and 149,
/// thirty two 11.57 and 230, and sixty four 11.10 and 390. Doubling from here
/// buys eight percent of the section for half again the lookup, and halving
/// spends fifteen percent of it to save a quarter of the lookup, so sixteen is
/// where neither direction is clearly worth taking.
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
    /// The first four bytes of each block's first term, which is what the
    /// binary search compares before it reads anything.
    index_keys: Vec<u32>,

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
            self.index_keys.push(key(term));
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
        let mut out = Vec::with_capacity(self.blocks.len() + self.index.len() + blocks * 8 + 24);
        put_uvarint(&mut out, u64::from(self.terms));
        put_uvarint(&mut out, blocks as u64);
        for key in &self.index_keys {
            put_u32(&mut out, *key);
        }
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
    keys: &'a [u8],
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
        let (keys, rest) = split_at(rest, offsets_len)?;
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
            keys,
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
        self.scan(block, term)
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
        let wanted = key(term);
        let (mut low, mut high) = (0usize, self.blocks);
        while low < high {
            let middle = low + (high - low) / 2;
            let found = self.key(middle)?;
            // The four byte keys sort the way the whole terms do, so a step
            // that finds them different is settled and the term stays on disk.
            // Only a step that finds them equal has to go and read one.
            let after = match found.cmp(&wanted) {
                std::cmp::Ordering::Less => false,
                std::cmp::Ordering::Greater => true,
                std::cmp::Ordering::Equal => self.index_entry(middle)?.0 > term,
            };
            if after {
                high = middle;
            } else {
                low = middle + 1;
            }
        }
        // Every block starts after the term, so no block can hold it.
        Ok((low > 0).then(|| low - 1))
    }

    /// The four byte key of a block's first term.
    fn key(&self, block: usize) -> Result<u32> {
        let at = block.checked_mul(4).ok_or(Error::Overflow)?;
        let slot = self.keys.get(at..at + 4).ok_or(Error::Truncated {
            needed: at + 4,
            available: self.keys.len(),
        })?;
        Ok(get_u32(slot)?.0)
    }

    /// Looks `term` up inside one block.
    ///
    /// The terms are never rebuilt. What moves is `matched`, the number of
    /// leading bytes of `term` that the term the scan is on agrees with, and
    /// every term after the first settles against it without a comparison
    /// unless it reuses exactly that many bytes.
    fn scan(&self, block: usize, term: &[u8]) -> Result<Option<Entry>> {
        let mut rest = self.block_bytes(block)?;

        let (len, tail) = get_uvarint(rest)?;
        let len = usize::try_from(len).map_err(|_| Error::Overflow)?;
        let (first, tail) = split_at(tail, len)?;
        rest = tail;

        let (mut matched, ordering) = common(first, term);
        if ordering == std::cmp::Ordering::Greater {
            // The block index picked this block because its first term is not
            // after the one being looked for, so this is a dictionary that has
            // been damaged rather than a term that is missing.
            return Ok(None);
        }
        let mut offset = 0u64;
        let (entry, tail) = payload(rest, &mut offset)?;
        if ordering == std::cmp::Ordering::Equal {
            return Ok(Some(entry));
        }
        rest = tail;

        while !rest.is_empty() {
            let (shared, tail) = get_uvarint(rest)?;
            let shared = usize::try_from(shared).map_err(|_| Error::Overflow)?;
            let (len, tail) = get_uvarint(tail)?;
            let len = usize::try_from(len).map_err(|_| Error::Overflow)?;
            let (suffix, tail) = split_at(tail, len)?;

            // A term that reuses fewer bytes than the scan has matched differs
            // from the one being looked for inside that prefix, and it is after
            // it, so every term left in the block is too.
            if shared < matched {
                return Ok(None);
            }
            // A term that reuses more is still inside the prefix the term
            // before it was already behind on, so it is behind too.
            let mut found = false;
            if shared == matched {
                let (further, ordering) = common(suffix, &term[matched..]);
                match ordering {
                    std::cmp::Ordering::Less => matched += further,
                    std::cmp::Ordering::Equal => found = true,
                    std::cmp::Ordering::Greater => return Ok(None),
                }
            }

            let (entry, tail) = payload(tail, &mut offset)?;
            if found {
                return Ok(Some(entry));
            }
            rest = tail;
        }
        Ok(None)
    }

    /// The encoded bytes of one block.
    fn block_bytes(&self, block: usize) -> Result<&'a [u8]> {
        let (_, offset) = self.index_entry(block)?;
        let end = if block + 1 < self.blocks {
            self.index_entry(block + 1)?.1
        } else {
            self.body.len()
        };
        self.body
            .get(offset..end.max(offset))
            .ok_or(Error::Truncated {
                needed: end,
                available: self.body.len(),
            })
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
        Ok(Walk {
            rest: self.block_bytes(block)?,
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

        let (entry, rest) = payload(self.rest, &mut self.offset)?;
        self.rest = rest;
        Ok(Some((self.term.as_slice(), entry)))
    }
}

/// How many leading bytes two terms have in common.
fn shared_prefix(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b).take_while(|(x, y)| x == y).count()
}

/// How many leading bytes two terms have in common, and which one sorts first.
///
/// The two answers come from the same walk because the scan needs both and
/// walking twice would be walking the same bytes twice.
fn common(a: &[u8], b: &[u8]) -> (usize, std::cmp::Ordering) {
    let shared = shared_prefix(a, b);
    let ordering = match (a.get(shared), b.get(shared)) {
        (Some(x), Some(y)) => x.cmp(y),
        // One ran out inside the other, so the shorter one sorts first.
        (left, right) => left.is_some().cmp(&right.is_some()),
    };
    (shared, ordering)
}

/// The first four bytes of a term, as a number that sorts the way it does.
///
/// A term shorter than four bytes is padded with zeros, which keeps the order:
/// a term that runs out where another carries on sorts before it, and zero is
/// below every byte the other one could have there.
///
/// [`crate::migrate`] needs it too, since a version 1 dictionary has no keys and
/// the migration is largely working them out, and the two of them agreeing is
/// the whole point of there being one of these rather than two.
pub(crate) fn key(term: &[u8]) -> u32 {
    let mut bytes = [0u8; 4];
    let take = term.len().min(4);
    bytes[..take].copy_from_slice(&term[..take]);
    u32::from_be_bytes(bytes)
}

/// Reads the three numbers stored after a term, carrying the posting offset.
fn payload<'a>(input: &'a [u8], offset: &mut u64) -> Result<(Entry, &'a [u8])> {
    let (docs, rest) = get_uvarint(input)?;
    let (gap, rest) = get_uvarint(rest)?;
    let (len, rest) = get_uvarint(rest)?;
    *offset = offset.checked_add(gap).ok_or(Error::Overflow)?;
    Ok((
        Entry {
            docs: u32::try_from(docs).map_err(|_| Error::Overflow)?,
            offset: *offset,
            len,
        },
        rest,
    ))
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

    /// Terms of every shape the scan and the block key have to survive: short
    /// ones the key has to pad, ones that extend each other, ones holding the
    /// byte the padding uses, and ones that agree on far more than the four
    /// bytes the key looks at.
    fn awkward() -> Vec<Vec<u8>> {
        let mut terms: Vec<Vec<u8>> = Vec::new();
        for base in ["", "a", "ab", "abc", "abcd", "abcde", "zzzz"] {
            terms.push(base.as_bytes().to_vec());
            for tail in [0u8, 1, b'a', 0xff] {
                let mut t = base.as_bytes().to_vec();
                t.push(tail);
                terms.push(t.clone());
                t.push(b'q');
                terms.push(t);
            }
        }
        // Enough terms sharing a long prefix to fill several blocks, which is
        // where every step of the binary search finds the keys equal and has to
        // fall back on comparing whole terms.
        for i in 0..200 {
            terms.push(format!("configuration{i:06}").into_bytes());
        }
        terms.sort();
        terms.dedup();
        terms
    }

    /// A dictionary of arbitrary byte terms, with the same entries `build` uses.
    fn build_bytes(terms: &[Vec<u8>]) -> Vec<u8> {
        let mut writer = Writer::new();
        let mut offset = 0u64;
        for (i, term) in terms.iter().enumerate() {
            let len = (i as u64 % 7) + 1;
            writer
                .push(
                    term,
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

    #[test]
    fn a_lookup_agrees_with_a_walk_on_every_probe() {
        // The scan settles most terms by arithmetic on how much of the probe it
        // has matched so far, without looking at them, so the only way to know
        // it settles them the way a comparison would is to ask it about every
        // term and every near miss and check it against the walk, which does
        // rebuild the terms.
        let terms = awkward();
        let encoded = build_bytes(&terms);
        let reader = Reader::new(&encoded).expect("header");
        let table = walked(&encoded);
        assert_eq!(table.len(), terms.len());

        let mut probes: Vec<Vec<u8>> = Vec::new();
        for term in &terms {
            probes.push(term.clone());
            for extra in [0u8, b'a', 0xff] {
                let mut longer = term.clone();
                longer.push(extra);
                probes.push(longer);
            }
            for cut in 0..term.len() {
                probes.push(term[..cut].to_vec());
                let mut bent = term.clone();
                bent[cut] = bent[cut].wrapping_add(1);
                probes.push(bent);
            }
        }

        for probe in probes {
            let expected = table
                .iter()
                .find(|(term, _)| *term == probe)
                .map(|(_, entry)| *entry);
            assert_eq!(
                reader.get(&probe).expect("lookup"),
                expected,
                "probe {:?}",
                String::from_utf8_lossy(&probe)
            );
        }
    }

    #[test]
    fn the_block_key_only_looks_at_four_bytes_and_the_search_still_lands() {
        // Every one of these shares the four bytes the block key is made of, so
        // the binary search finds them equal at every step and the whole answer
        // comes from the comparison it falls back on. There are enough of them
        // to be several blocks.
        let terms: Vec<Vec<u8>> = (0..500)
            .map(|i| format!("term{i:08}").into_bytes())
            .collect();
        let encoded = build_bytes(&terms);
        let reader = Reader::new(&encoded).expect("header");
        for (i, term) in terms.iter().enumerate() {
            let found = reader.get(term).expect("lookup").expect("term is there");
            assert_eq!(found.docs, u32::try_from(i).expect("small") + 1);
        }
        assert_eq!(reader.get(b"term").expect("lookup"), None);
        assert_eq!(reader.get(b"terma").expect("lookup"), None);
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
