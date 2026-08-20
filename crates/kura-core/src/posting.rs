//! Compressed posting lists.
//!
//! A posting list is the set of documents one term appears in, together with how
//! often it appears in each, and it is the single largest thing an inverted index
//! stores. Whatever it costs to hold and to walk is what a query costs,
//! multiplied by the number of terms in it.
//!
//! It is written as fixed size blocks of a hundred and twenty eight postings,
//! packed at a width chosen per block, with a skip entry per block. The packing
//! is in [`crate::bitpack`], along with the reason it beats a varint by more than
//! the width alone would suggest.
//!
//! Whatever is left over at the end, fewer postings than a block holds, is
//! written as varints instead. Uniformity would be nicer, but the vocabulary of a
//! real corpus is mostly terms that appear in a handful of documents, and
//! rounding every one of those up to a whole block would cost more in the term
//! dictionary than the packing saves in the lists that are actually long.
//!
//! # Two streams
//!
//! The ids and the frequencies are packed separately rather than interleaved. A
//! query that is only testing membership, or that is skipping a block because the
//! best score in it cannot reach the top of the heap, never touches the
//! frequencies at all, and interleaving would drag them through cache anyway.
//!
//! The ids are stored as gaps and the frequencies as they are. Frequencies are
//! not ascending, so differencing them would make them larger, and they are
//! almost all one, so a block of them usually packs at a single bit.
//!
//! # Skipping
//!
//! The blocks are what make a long list usable. Without them, finding whether
//! document nine million is in a list means decoding everything before it. With
//! them, the skip table says which block could contain it and only that block is
//! decoded. That is the difference between an intersection that scales with the
//! size of the rarest term and one that scales with the size of the commonest.
//!
//! Each skip entry also carries the largest frequency in its block, which is what
//! lets a scorer bound the best score that block could produce and decide not to
//! decode it. That is the whole of block max pruning, and it is a byte a block.

use crate::DocId;
use crate::bitpack::{self, BLOCK};
use crate::codec::{get_u32, get_uvarint, put_u32, put_uvarint, split_at};
use crate::error::{Error, Result};

/// How many postings go into one packed block.
pub const BLOCK_SIZE: usize = BLOCK;

/// How wide one entry in each of the three skip columns is.
///
/// Fixed width rather than varint on purpose. The skip table is the one part of
/// the format that is searched rather than read, and a search needs to be able
/// to land on entry `n` without decoding the `n - 1` before it.
///
/// The three columns are stored one after another rather than interleaved. A
/// binary search reads only the last id of each block, so an interleaved table
/// would make it stride over the two offsets it does not want and touch three
/// times the cache lines to find the same thing. Splitting them took a thousand
/// seeks into a list of a million postings from 126 ns each to 106 ns, on the
/// same data and the same machine, and the layout is the only thing that
/// changed.
const SKIP_WIDTH: usize = 4;

/// The largest frequency a skip entry records exactly.
///
/// Above this the entry saturates and the reader falls back to the bound the
/// block's packed width gives, which is looser but still correct. A term that
/// appears more than two hundred and fifty five times in one document is a
/// boilerplate footer or a bug, and it is not worth widening every skip entry in
/// the index to hold it exactly.
const MAX_EXACT_FREQUENCY: u32 = u8::MAX as u32;

/// Builds a posting list from ascending document ids.
#[derive(Debug)]
pub struct Writer {
    /// The packed ids.
    docs: Vec<u8>,
    /// The packed frequencies.
    freqs: Vec<u8>,
    /// The last id of each block, which is the skip table.
    skips: Vec<DocId>,
    /// The byte offset of each block's ids inside `docs`.
    offsets: Vec<u32>,
    /// The byte offset of each block's frequencies inside `freqs`.
    freq_offsets: Vec<u32>,
    /// The width each block's ids were packed at, one byte each.
    widths: Vec<u8>,
    /// The width each block's frequencies were packed at, one byte each.
    freq_widths: Vec<u8>,
    /// The largest frequency in each block, saturating.
    maxima: Vec<u8>,
    /// The postings that did not fill a block, as varints.
    tail: Vec<u8>,
    /// Their frequencies, as varints.
    tail_freqs: Vec<u8>,

    pending: [DocId; BLOCK],
    pending_freqs: [u32; BLOCK],
    filled: usize,
    /// The last id written into a block, which is the base the next block counts
    /// from and the base the tail counts from.
    packed_last: DocId,
    last: Option<DocId>,
    count: u32,
}

impl Default for Writer {
    fn default() -> Self {
        Self {
            docs: Vec::new(),
            freqs: Vec::new(),
            skips: Vec::new(),
            offsets: Vec::new(),
            freq_offsets: Vec::new(),
            widths: Vec::new(),
            freq_widths: Vec::new(),
            maxima: Vec::new(),
            tail: Vec::new(),
            tail_freqs: Vec::new(),
            pending: [0; BLOCK],
            pending_freqs: [0; BLOCK],
            filled: 0,
            packed_last: 0,
            last: None,
            count: 0,
        }
    }
}

impl Writer {
    /// Returns an empty writer.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends a posting: a document id and how often the term occurs in it.
    ///
    /// A frequency of zero is stored as written. The list says which documents a
    /// term is in, and a caller that wants to say "in this document, zero times"
    /// is describing something the caller understands and this does not.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotSorted`] if `id` is not strictly greater than the
    /// previous one. The whole format rests on the order, so this is checked
    /// rather than assumed.
    ///
    /// Returns [`Error::Overflow`] if the packed blocks have grown past what a
    /// four byte offset can address, which is a list of roughly two hundred
    /// million ids in one term. A list that long belongs in more than one
    /// segment, and saying so is better than writing a skip table that points at
    /// the wrong bytes.
    pub fn push(&mut self, id: DocId, frequency: u32) -> Result<()> {
        if let Some(last) = self.last
            && id <= last
        {
            return Err(Error::NotSorted { at: id });
        }
        self.last = Some(id);
        self.count += 1;
        self.pending[self.filled] = id;
        self.pending_freqs[self.filled] = frequency;
        self.filled += 1;
        if self.filled == BLOCK {
            self.flush_block()?;
        }
        Ok(())
    }

    /// Finishes the list and returns the encoded bytes.
    #[must_use]
    pub fn finish(mut self) -> Vec<u8> {
        let mut out = Vec::new();
        self.finish_into(&mut out);
        out
    }

    /// Appends the encoded list to `out` and empties the writer for the next
    /// one.
    ///
    /// This is what a caller writing a whole term dictionary uses. A segment has
    /// as many lists as it has terms, most of them a handful of bytes long, and
    /// a writer per term would spend more time in the allocator than in the
    /// codec. Reusing one writer and appending straight into the postings
    /// section costs neither the allocation nor the copy.
    pub fn finish_into(&mut self, out: &mut Vec<u8>) {
        // The leftover postings go out as varints rather than as a short block,
        // so a term that appears in three documents costs a handful of bytes.
        self.tail.clear();
        self.tail_freqs.clear();
        let mut previous = self.packed_last;
        for at in 0..self.filled {
            let id = self.pending[at];
            put_uvarint(&mut self.tail, u64::from(id - previous));
            put_uvarint(&mut self.tail_freqs, u64::from(self.pending_freqs[at]));
            previous = id;
        }

        let blocks = self.skips.len();
        out.reserve(
            self.docs.len()
                + self.freqs.len()
                + blocks * (SKIP_WIDTH * 3 + 3)
                + self.tail.len()
                + self.tail_freqs.len()
                + 32,
        );
        put_uvarint(out, u64::from(self.count));
        put_uvarint(out, blocks as u64);
        for skip in &self.skips {
            put_u32(out, *skip);
        }
        for offset in &self.offsets {
            put_u32(out, *offset);
        }
        for offset in &self.freq_offsets {
            put_u32(out, *offset);
        }
        out.extend_from_slice(&self.widths);
        out.extend_from_slice(&self.freq_widths);
        out.extend_from_slice(&self.maxima);
        put_uvarint(out, self.docs.len() as u64);
        out.extend_from_slice(&self.docs);
        put_uvarint(out, self.freqs.len() as u64);
        out.extend_from_slice(&self.freqs);
        put_uvarint(out, self.tail.len() as u64);
        out.extend_from_slice(&self.tail);
        put_uvarint(out, self.tail_freqs.len() as u64);
        out.extend_from_slice(&self.tail_freqs);

        self.docs.clear();
        self.freqs.clear();
        self.skips.clear();
        self.offsets.clear();
        self.freq_offsets.clear();
        self.widths.clear();
        self.freq_widths.clear();
        self.maxima.clear();
        self.filled = 0;
        self.packed_last = 0;
        self.last = None;
        self.count = 0;
    }

    fn flush_block(&mut self) -> Result<()> {
        let offset = u32::try_from(self.docs.len()).map_err(|_| Error::Overflow)?;
        let freq_offset = u32::try_from(self.freqs.len()).map_err(|_| Error::Overflow)?;
        let width = bitpack::pack(&self.pending, self.packed_last, &mut self.docs);
        let freq_width = bitpack::pack_flat(&self.pending_freqs, &mut self.freqs);

        let mut max = 0u32;
        for frequency in &self.pending_freqs {
            max = max.max(*frequency);
        }

        self.offsets.push(offset);
        self.freq_offsets.push(freq_offset);
        self.skips.push(self.pending[BLOCK - 1]);
        // Neither width ever exceeds thirty two, so the byte is the whole of it.
        self.widths.push(u8::try_from(width).unwrap_or(u8::MAX));
        self.freq_widths
            .push(u8::try_from(freq_width).unwrap_or(u8::MAX));
        self.maxima
            .push(u8::try_from(max.min(MAX_EXACT_FREQUENCY)).unwrap_or(u8::MAX));
        self.packed_last = self.pending[BLOCK - 1];
        self.filled = 0;
        Ok(())
    }
}

/// Reads a posting list written by [`Writer`].
///
/// Opening a list costs the same whether it holds ten postings or ten million:
/// the skip table is left where it is and read in place. A reader that copied the
/// table out would turn every term lookup in a query into an allocation, and a
/// query touches a lot of terms.
#[derive(Debug, Clone, Copy)]
pub struct Reader<'a> {
    count: u32,
    block_count: usize,
    /// The last id of each block, which is what a seek binary searches.
    lasts: &'a [u8],
    /// Where each block's ids start.
    offsets: &'a [u8],
    /// Where each block's frequencies start.
    freq_offsets: &'a [u8],
    widths: &'a [u8],
    freq_widths: &'a [u8],
    maxima: &'a [u8],
    docs: &'a [u8],
    freqs: &'a [u8],
    tail: &'a [u8],
    tail_freqs: &'a [u8],
}

impl<'a> Reader<'a> {
    /// Parses the header of an encoded list.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Truncated`] if the input ends inside the header, the
    /// skip table, the widths, either stream of blocks or either tail, and
    /// [`Error::Overflow`] if a length does not decode.
    pub fn new(input: &'a [u8]) -> Result<Self> {
        let (count, rest) = get_uvarint(input)?;
        let (block_count, rest) = get_uvarint(rest)?;

        let block_count = usize::try_from(block_count).map_err(|_| Error::Overflow)?;
        let column = block_count.checked_mul(SKIP_WIDTH).ok_or(Error::Overflow)?;
        let (lasts, rest) = split_at(rest, column)?;
        let (offsets, rest) = split_at(rest, column)?;
        let (freq_offsets, rest) = split_at(rest, column)?;
        let (widths, rest) = split_at(rest, block_count)?;
        let (freq_widths, rest) = split_at(rest, block_count)?;
        let (maxima, rest) = split_at(rest, block_count)?;

        let (docs, rest) = section(rest)?;
        let (freqs, rest) = section(rest)?;
        let (tail, rest) = section(rest)?;
        let (tail_freqs, _) = section(rest)?;

        Ok(Self {
            count: u32::try_from(count).map_err(|_| Error::Overflow)?,
            block_count,
            lasts,
            offsets,
            freq_offsets,
            widths,
            freq_widths,
            maxima,
            docs,
            freqs,
            tail,
            tail_freqs,
        })
    }

    /// Reads entry `index` out of one of the three skip columns.
    fn column(column: &[u8], index: usize) -> Option<u32> {
        let start = index.checked_mul(SKIP_WIDTH)?;
        let entry = column.get(start..start.checked_add(SKIP_WIDTH)?)?;
        get_u32(entry).ok().map(|(value, _)| value)
    }

    /// Returns the last id of one block.
    fn last_of(&self, index: usize) -> Option<DocId> {
        Self::column(self.lasts, index)
    }

    /// Returns the index of the first block whose last id is not below `target`,
    /// which is the only block that can hold it.
    ///
    /// This reads the last id column and nothing else, which is why the three
    /// columns are stored apart.
    fn seek(&self, target: DocId) -> Option<usize> {
        let (mut low, mut high) = (0usize, self.block_count);
        while low < high {
            let middle = low + (high - low) / 2;
            if self.last_of(middle)? < target {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        (low < self.block_count).then_some(low)
    }

    /// Returns how many postings the list holds.
    #[must_use]
    pub const fn len(&self) -> u32 {
        self.count
    }

    /// Reports whether the list is empty.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// How many full blocks the list holds, not counting the leftovers.
    #[must_use]
    pub const fn blocks(&self) -> usize {
        self.block_count
    }

    /// An upper bound on the frequencies in one block, without decoding it.
    ///
    /// Exact for every frequency a real corpus produces. Above what one byte
    /// holds it falls back to the bound the block's packed width gives, which is
    /// within a factor of two and still an upper bound, which is all a scorer
    /// deciding whether to skip the block needs it to be.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Truncated`] if `index` is past the last block.
    pub fn block_max_frequency(&self, index: usize) -> Result<u32> {
        let stored = u32::from(*self.maxima.get(index).ok_or(Error::Truncated {
            needed: index,
            available: self.maxima.len(),
        })?);
        if stored < MAX_EXACT_FREQUENCY {
            return Ok(stored);
        }
        let width = u32::from(*self.freq_widths.get(index).ok_or(Error::Truncated {
            needed: index,
            available: self.freq_widths.len(),
        })?);
        if width >= 32 {
            return Ok(u32::MAX);
        }
        Ok((1u32 << width) - 1)
    }

    /// Decodes every document id.
    ///
    /// # Errors
    ///
    /// Returns an error if any block is truncated or encodes a gap that runs
    /// past the end of the identifier space.
    pub fn to_vec(&self) -> Result<Vec<DocId>> {
        let mut out = Vec::with_capacity(self.count as usize);
        let mut block = [0u32; BLOCK];
        for index in 0..self.block_count {
            self.decode_block(index, &mut block)?;
            out.extend_from_slice(&block);
        }
        self.walk_tail(|id, _| out.push(id))?;
        Ok(out)
    }

    /// Decodes every posting, as an id and a frequency.
    ///
    /// # Errors
    ///
    /// Returns an error if any block is truncated, or if the two streams
    /// disagree about how many postings the list holds.
    pub fn to_postings(&self) -> Result<Vec<(DocId, u32)>> {
        let mut ids = Vec::with_capacity(self.count as usize);
        let mut freqs = Vec::with_capacity(self.count as usize);
        let mut block = [0u32; BLOCK];
        for index in 0..self.block_count {
            self.decode_block(index, &mut block)?;
            ids.extend_from_slice(&block);
            self.decode_freqs(index, &mut block)?;
            freqs.extend_from_slice(&block);
        }
        self.decode_tail(&mut ids, &mut freqs)?;
        if ids.len() != freqs.len() {
            return Err(Error::Truncated {
                needed: ids.len(),
                available: freqs.len(),
            });
        }
        Ok(ids.into_iter().zip(freqs).collect())
    }

    /// Reports whether `target` is in the list, decoding at most one block.
    ///
    /// # Errors
    ///
    /// Returns an error if the block the skip table points at is truncated.
    pub fn contains(&self, target: DocId) -> Result<bool> {
        // The skip table holds the last id of each block and is ascending, so a
        // binary search over it finds the one block that could hold the target.
        if let Some(index) = self.seek(target) {
            let mut block = [0u32; BLOCK];
            self.decode_block(index, &mut block)?;
            return Ok(block.binary_search(&target).is_ok());
        }
        // Past the last packed block, so it is in the leftovers or nowhere. The
        // leftovers are shorter than one block by construction, which is why
        // walking them is not worth a second index.
        let mut found = false;
        self.walk_tail(|id, _| found |= id == target)?;
        Ok(found)
    }

    /// Starts a walk over the postings.
    #[must_use]
    pub fn cursor(&self) -> Cursor<'a> {
        Cursor {
            list: *self,
            docs: [0; BLOCK],
            freqs: [0; BLOCK],
            block: 0,
            len: 0,
            at: 0,
            started: false,
            done: false,
        }
    }

    /// Decodes one block of packed ids into `out`.
    fn decode_block(&self, index: usize, out: &mut [DocId; BLOCK]) -> Result<()> {
        let offset = Self::column(self.offsets, index).ok_or(Error::Truncated {
            needed: index,
            available: self.block_count,
        })?;
        let width = u32::from(*self.widths.get(index).ok_or(Error::Truncated {
            needed: index,
            available: self.widths.len(),
        })?);
        // The block before this one ends where this one starts counting from.
        // Block zero counts from zero, which is what makes the first id absolute
        // without storing it as one.
        let base = if index == 0 {
            0
        } else {
            self.last_of(index - 1).unwrap_or(0)
        };

        let start = offset as usize;
        let bytes = self.docs.get(start..).ok_or(Error::Truncated {
            needed: start,
            available: self.docs.len(),
        })?;
        bitpack::unpack(bytes, width, base, out)?;
        Ok(())
    }

    /// Decodes one block of packed frequencies into `out`.
    fn decode_freqs(&self, index: usize, out: &mut [u32; BLOCK]) -> Result<()> {
        let offset = Self::column(self.freq_offsets, index).ok_or(Error::Truncated {
            needed: index,
            available: self.block_count,
        })?;
        let width = u32::from(*self.freq_widths.get(index).ok_or(Error::Truncated {
            needed: index,
            available: self.freq_widths.len(),
        })?);
        let start = offset as usize;
        let bytes = self.freqs.get(start..).ok_or(Error::Truncated {
            needed: start,
            available: self.freqs.len(),
        })?;
        bitpack::unpack_flat(bytes, width, out)?;
        Ok(())
    }

    /// Decodes the varints after the last packed block onto the end of `ids`,
    /// and their frequencies onto the end of `freqs`.
    fn decode_tail(&self, ids: &mut Vec<DocId>, freqs: &mut Vec<u32>) -> Result<()> {
        self.walk_tail(|id, frequency| {
            ids.push(id);
            freqs.push(frequency);
        })
    }

    /// Hands every leftover posting to `f`, in order.
    ///
    /// Callers that already have somewhere to put the postings use this rather
    /// than [`Reader::decode_tail`], which needs two vectors to fill.
    fn walk_tail(&self, mut f: impl FnMut(DocId, u32)) -> Result<()> {
        let mut current = if self.block_count == 0 {
            0
        } else {
            self.last_of(self.block_count - 1).unwrap_or(0)
        };

        let mut rest = self.tail;
        let mut rest_freqs = self.tail_freqs;
        while !rest.is_empty() {
            let (gap, tail) = get_uvarint(rest)?;
            let gap = u32::try_from(gap).map_err(|_| Error::Overflow)?;
            current = current.checked_add(gap).ok_or(Error::Overflow)?;
            rest = tail;

            let (frequency, tail) = get_uvarint(rest_freqs)?;
            let frequency = u32::try_from(frequency).map_err(|_| Error::Overflow)?;
            rest_freqs = tail;

            f(current, frequency);
        }
        Ok(())
    }
}

/// A walk over the postings of one list.
///
/// This is what a query holds, one per term. It decodes a block at a time and
/// hands out the postings in it, and [`Cursor::seek`] uses the skip table to jump
/// blocks rather than walking through them, which is what makes an intersection
/// cost the size of the rarest term rather than the commonest.
///
/// The decoded block lives in the cursor rather than on the heap, so a query with
/// ten terms allocates nothing at all once its cursors exist.
#[derive(Debug, Clone)]
pub struct Cursor<'a> {
    list: Reader<'a>,
    docs: [DocId; BLOCK],
    freqs: [u32; BLOCK],
    /// Which block is decoded. Equal to the list's block count once the cursor
    /// has moved into the leftovers.
    block: usize,
    /// How many entries of `docs` and `freqs` are valid.
    len: usize,
    at: usize,
    started: bool,
    done: bool,
}

impl Cursor<'_> {
    /// The document the cursor is on, or `None` before the first [`Cursor::advance`]
    /// and after the end.
    #[must_use]
    pub fn doc(&self) -> Option<DocId> {
        if !self.started || self.done {
            return None;
        }
        self.docs.get(self.at).copied()
    }

    /// How often the term occurs in the document the cursor is on.
    ///
    /// Zero when the cursor is not on a document, which a caller that checked
    /// [`Cursor::doc`] first will never see.
    #[must_use]
    pub fn frequency(&self) -> u32 {
        if !self.started || self.done {
            return 0;
        }
        self.freqs.get(self.at).copied().unwrap_or(0)
    }

    /// Moves to the next document and returns it.
    ///
    /// # Errors
    ///
    /// Returns an error if the block it moves into is truncated.
    pub fn advance(&mut self) -> Result<Option<DocId>> {
        if self.done {
            return Ok(None);
        }
        if !self.started {
            self.started = true;
            self.load(0)?;
            return Ok(self.doc());
        }
        self.at += 1;
        if self.at == self.len {
            let next = self.block + 1;
            self.load(next)?;
        }
        Ok(self.doc())
    }

    /// Moves to the first document at or after `target` and returns it.
    ///
    /// Jumps whole blocks using the skip table when the target is far ahead, and
    /// scans within a block when it is near, which is the case that matters
    /// because a query that has already narrowed to a few candidates spends all
    /// its time there.
    ///
    /// # Errors
    ///
    /// Returns an error if a block it lands in is truncated.
    pub fn seek(&mut self, target: DocId) -> Result<Option<DocId>> {
        if self.done {
            return Ok(None);
        }
        if self.started && self.doc().is_some_and(|doc| doc >= target) {
            return Ok(self.doc());
        }

        // The block the cursor is in may already hold the target, in which case
        // the skip table is a slower way to find out than looking.
        let inside =
            self.started && self.block < self.list.block_count && self.docs[self.len - 1] >= target;
        if !inside {
            let landing = self.list.seek(target).unwrap_or(self.list.block_count);
            if !self.started || landing > self.block {
                self.started = true;
                self.load(landing)?;
                if self.done {
                    return Ok(None);
                }
            }
        }

        // Within the block, and the block is ascending.
        let found = self.docs[self.at..self.len].partition_point(|doc| *doc < target);
        self.at += found;
        if self.at == self.len {
            let next = self.block + 1;
            self.load(next)?;
            // The block the skip table chose ends before the target only when
            // the target is in the leftovers, which the load above moved into.
            if !self.done {
                let found = self.docs[..self.len].partition_point(|doc| *doc < target);
                self.at = found;
                if self.at == self.len {
                    self.done = true;
                }
            }
        }
        Ok(self.doc())
    }

    /// An upper bound on the frequencies in the block the cursor is in.
    ///
    /// Zero once the cursor is past the end, and the real maximum of the
    /// leftovers once it is in them, which are short enough to have been decoded
    /// already.
    #[must_use]
    pub fn block_max_frequency(&self) -> u32 {
        if self.done {
            return 0;
        }
        if self.block < self.list.block_count {
            return self
                .list
                .block_max_frequency(self.block)
                .unwrap_or(u32::MAX);
        }
        self.freqs[..self.len].iter().copied().max().unwrap_or(0)
    }

    /// The last document in the block the cursor is in, which is how far a caller
    /// can skip without decoding anything.
    #[must_use]
    pub fn block_last(&self) -> Option<DocId> {
        if self.done || self.len == 0 {
            return None;
        }
        Some(self.docs[self.len - 1])
    }

    /// Decodes block `index`, or the leftovers when `index` is the one past the
    /// last block, or ends the walk when it is past that.
    ///
    /// The leftovers sit at one index past the last block, so a cursor that has
    /// already read them and is being asked for more is at the end. Loading them
    /// again would be a walk that never finishes.
    fn load(&mut self, index: usize) -> Result<()> {
        self.block = index;
        self.at = 0;
        if index > self.list.block_count {
            self.len = 0;
            self.done = true;
            return Ok(());
        }
        if index < self.list.block_count {
            self.list.decode_block(index, &mut self.docs)?;
            self.list.decode_freqs(index, &mut self.freqs)?;
            self.len = BLOCK;
            return Ok(());
        }

        // Straight into the cursor's own arrays. The leftovers are shorter than
        // one block by construction, so they always fit, and a cursor that has
        // been built once never allocates again.
        let mut filled = 0usize;
        let docs = &mut self.docs;
        let freqs = &mut self.freqs;
        self.list.walk_tail(|id, frequency| {
            if let (Some(slot), Some(count)) = (docs.get_mut(filled), freqs.get_mut(filled)) {
                *slot = id;
                *count = frequency;
                filled += 1;
            }
        })?;
        if filled == 0 {
            self.len = 0;
            self.done = true;
            return Ok(());
        }
        self.len = filled;
        Ok(())
    }
}

/// Reads a length prefixed run of bytes.
fn section(input: &[u8]) -> Result<(&[u8], &[u8])> {
    let (len, rest) = get_uvarint(input)?;
    let len = usize::try_from(len).map_err(|_| Error::Overflow)?;
    split_at(rest, len)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode(ids: &[DocId]) -> Vec<u8> {
        encode_with(&ids.iter().map(|id| (*id, 1)).collect::<Vec<_>>())
    }

    fn encode_with(postings: &[(DocId, u32)]) -> Vec<u8> {
        let mut writer = Writer::new();
        for (id, frequency) in postings {
            writer.push(*id, *frequency).expect("ascending input");
        }
        writer.finish()
    }

    fn count(ids: &[DocId]) -> u32 {
        u32::try_from(ids.len()).expect("test fixtures are small")
    }

    /// A frequency that varies the way a real one does: mostly one, sometimes a
    /// handful, occasionally large.
    fn frequency(i: usize) -> u32 {
        match i % 17 {
            0 => 4,
            7 => 11,
            13 => 300,
            _ => 1,
        }
    }

    fn postings(count: usize) -> Vec<(DocId, u32)> {
        (0..count)
            .map(|i| (u32::try_from(i).expect("small") * 3, frequency(i)))
            .collect()
    }

    #[test]
    fn round_trips_a_short_list() {
        let ids = [0u32, 1, 2, 9, 400, 401, 100_000];
        let encoded = encode(&ids);
        let reader = Reader::new(&encoded).expect("header");
        assert_eq!(reader.len(), count(&ids));
        assert_eq!(reader.to_vec().expect("decode"), ids);
    }

    #[test]
    fn round_trips_at_every_length_around_a_block_boundary() {
        for len in 0..BLOCK * 2 + 3 {
            let want = postings(len);
            let encoded = encode_with(&want);
            let reader = Reader::new(&encoded).expect("header");
            assert_eq!(reader.len() as usize, len, "length {len}");
            assert_eq!(reader.to_postings().expect("decode"), want, "length {len}");
        }
    }

    #[test]
    fn frequencies_survive_the_round_trip() {
        let want = postings(1_000);
        let encoded = encode_with(&want);
        let reader = Reader::new(&encoded).expect("header");
        assert_eq!(reader.to_postings().expect("decode"), want);
    }

    #[test]
    fn a_block_maximum_bounds_the_frequencies_in_it() {
        // Every block, checked against the frequencies actually in it, because
        // an upper bound that is not one is a scorer that skips a document it
        // should have scored.
        let want = postings(1_000);
        let encoded = encode_with(&want);
        let reader = Reader::new(&encoded).expect("header");
        assert!(reader.blocks() > 1);
        for index in 0..reader.blocks() {
            let bound = reader.block_max_frequency(index).expect("in range");
            let real = want[index * BLOCK..(index + 1) * BLOCK]
                .iter()
                .map(|(_, f)| *f)
                .max()
                .expect("a full block");
            assert!(bound >= real, "block {index}: bound {bound} under {real}");
        }
    }

    #[test]
    fn a_frequency_too_large_to_store_exactly_still_bounds_the_block() {
        let want: Vec<(DocId, u32)> = (0..BLOCK)
            .map(|i| {
                (
                    u32::try_from(i).expect("small"),
                    if i == 5 { 90_000 } else { 1 },
                )
            })
            .collect();
        let encoded = encode_with(&want);
        let reader = Reader::new(&encoded).expect("header");
        assert!(reader.block_max_frequency(0).expect("in range") >= 90_000);
        assert_eq!(reader.to_postings().expect("decode"), want);
    }

    #[test]
    fn a_cursor_walks_every_posting_in_order() {
        for len in [0, 1, BLOCK - 1, BLOCK, BLOCK + 1, BLOCK * 3 + 7] {
            let want = postings(len);
            let encoded = encode_with(&want);
            let reader = Reader::new(&encoded).expect("header");
            let mut cursor = reader.cursor();
            let mut got = Vec::new();
            while let Some(doc) = cursor.advance().expect("decode") {
                got.push((doc, cursor.frequency()));
            }
            assert_eq!(got, want, "length {len}");
            assert_eq!(cursor.advance().expect("decode"), None, "length {len}");
        }
    }

    #[test]
    fn a_cursor_seeks_to_every_document_in_the_list() {
        let want = postings(BLOCK * 5 + 3);
        let encoded = encode_with(&want);
        let reader = Reader::new(&encoded).expect("header");

        // Seeking forwards to each id in turn, on one cursor, which is what an
        // intersection does.
        let mut cursor = reader.cursor();
        for (id, freq) in &want {
            assert_eq!(cursor.seek(*id).expect("decode"), Some(*id), "id {id}");
            assert_eq!(cursor.frequency(), *freq, "id {id}");
        }
        assert_eq!(cursor.seek(u32::MAX).expect("decode"), None);
    }

    #[test]
    fn a_seek_lands_on_the_next_document_when_the_target_is_missing() {
        // Every id in this list is a multiple of three, so every id plus one is
        // a target that is not there and has to land on the one after it.
        let want = postings(BLOCK * 4 + 5);
        let encoded = encode_with(&want);
        let reader = Reader::new(&encoded).expect("header");

        for (i, (id, _)) in want.iter().enumerate().take(want.len() - 1) {
            let mut cursor = reader.cursor();
            let landed = cursor.seek(id + 1).expect("decode");
            assert_eq!(landed, Some(want[i + 1].0), "target {}", id + 1);
            assert_eq!(cursor.frequency(), want[i + 1].1);
        }
    }

    #[test]
    fn a_seek_past_the_end_is_the_end() {
        let want = postings(BLOCK * 2 + 9);
        let encoded = encode_with(&want);
        let reader = Reader::new(&encoded).expect("header");
        let mut cursor = reader.cursor();
        assert_eq!(cursor.seek(u32::MAX).expect("decode"), None);
        assert_eq!(cursor.doc(), None);
        assert_eq!(cursor.frequency(), 0);
        assert_eq!(cursor.advance().expect("decode"), None);
    }

    #[test]
    fn a_cursor_over_an_empty_list_is_empty() {
        let encoded = encode(&[]);
        let reader = Reader::new(&encoded).expect("header");
        let mut cursor = reader.cursor();
        assert_eq!(cursor.advance().expect("decode"), None);
        assert_eq!(cursor.doc(), None);

        let mut cursor = reader.cursor();
        assert_eq!(cursor.seek(0).expect("decode"), None);
    }

    #[test]
    fn contains_reaches_the_leftovers_as_well_as_the_blocks() {
        let ids: Vec<DocId> = (0..u32::try_from(BLOCK).expect("small") * 2 + 40)
            .map(|i| i * 5)
            .collect();
        let encoded = encode(&ids);
        let reader = Reader::new(&encoded).expect("header");
        for id in &ids {
            assert!(reader.contains(*id).expect("lookup"), "missing {id}");
            assert!(
                !reader.contains(id + 1).expect("lookup"),
                "found {}",
                id + 1
            );
        }
    }

    #[test]
    fn a_dense_list_costs_well_under_a_byte_an_id() {
        let ids: Vec<DocId> = (0..100_000u32).map(|i| i * 3).collect();
        let encoded = encode(&ids);
        let raw = ids.len() * 4;
        assert!(
            encoded.len() * 4 < raw,
            "{} bytes for {} ids",
            encoded.len(),
            ids.len()
        );
    }

    #[test]
    fn a_sparse_term_does_not_pay_for_a_whole_block() {
        let encoded = encode(&[7, 900, 100_000]);
        assert!(encoded.len() < 20, "{} bytes for three ids", encoded.len());
        let reader = Reader::new(&encoded).expect("header");
        assert_eq!(reader.to_vec().expect("decode"), [7, 900, 100_000]);
    }

    #[test]
    fn ids_out_of_order_are_refused() {
        let mut writer = Writer::new();
        writer.push(5, 1).expect("first");
        assert_eq!(writer.push(5, 1), Err(Error::NotSorted { at: 5 }));
        assert_eq!(writer.push(4, 1), Err(Error::NotSorted { at: 4 }));
    }

    #[test]
    fn a_truncated_list_is_an_error_not_a_panic() {
        let encoded = encode_with(&postings(BLOCK * 2 + 5));
        for len in 0..encoded.len() {
            if let Ok(reader) = Reader::new(&encoded[..len]) {
                let _ = reader.to_vec();
                let _ = reader.to_postings();
                let _ = reader.contains(42);
                let _ = reader.block_max_frequency(0);
                let mut cursor = reader.cursor();
                while let Ok(Some(_)) = cursor.advance() {}
                let mut cursor = reader.cursor();
                let _ = cursor.seek(1_000);
            }
        }
    }
}
