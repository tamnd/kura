//! What a block of postings can score, at best.
//!
//! Block max WAND skips a block when the best score anything in it could reach
//! is below the worst hit already in hand. That makes the bound the only thing
//! that decides how much of a posting list gets read, and a bound that is too
//! generous is the same as no bound at all.
//!
//! The bound this crate started with came from the largest *frequency* in a
//! block, with the length normalisation pinned to the shortest document that
//! could exist. That is correct and it is useless. Written out, a term
//! contributes `idf * f * (k1 + 1) / (f + norm)`, and pinning `norm` to its
//! floor of `k1 * (1 - b)` overstates the contribution by a factor of about
//! `(f + norm) / (f + k1 * (1 - b))`. On prose of a few thousand words that is
//! close to four. A bound four times the truth never falls under a threshold,
//! so the block is decoded, and the pruning that the format pays a byte a block
//! for never fires.
//!
//! What is stored here instead is the largest `f / (f + norm)` in the block,
//! which is the whole of the score that varies within a term. The rest,
//! `idf * (k1 + 1)`, is a constant per term that a query works out once. The
//! only loss is the quantisation, and rounding up keeps it a bound.
//!
//! # Why a section of its own
//!
//! The obvious place for this is beside the maximum frequency in the posting
//! list's skip table. It is not there because that would change the posting
//! format, and a reader of an older build would then have to be taught to skip
//! it. A section is additive: the segment already carries a table of sections
//! with a kind on each, and a reader that has never heard of this kind returns
//! the same results and is merely slower.
//!
//! # The parameters are part of the data
//!
//! `norm` depends on `k1`, on `b` and on the average document length, and all
//! three are the caller's at query time. A bound computed under one set is not a
//! bound under another, so the set is written into the section.
//!
//! `k1` and `b` are compared and nothing else happens: a searcher given
//! different ones ignores what is here and falls back to the old bound. Silently
//! using it would not be slower, it would be wrong, and it would be wrong by
//! dropping results rather than by returning bad ones, which is the kind of
//! wrong nobody notices.
//!
//! The average is different, because a store of several segments scores every
//! one of them against the average of the whole store and no segment's own
//! average is that number. Falling back whenever they differ would mean falling
//! back on every multi segment store, which is most of them. Instead the
//! difference is corrected for. See [`Reader::scale`] for the factor and why it
//! holds.
//!
//! # One byte is coarser than it looks
//!
//! A byte carries four tenths of a percent of slack, which sounds like nothing
//! next to the four hundred percent it replaced. It is not nothing, because of
//! what it gets compared against. The score saturates, so the documents at the
//! top of a common term have all saturated and their scores are packed close
//! together: on a corpus of forty seven thousand source files, `the` scores
//! 1.5554 at rank one and 1.5531 at rank ten, which is a spread of 0.15 percent.
//! Rounding a ceiling up by two and a half times that spread puts blocks over
//! the threshold whose true best is well under it, and every one of them gets
//! decoded.
//!
//! So the ceilings are also written a second time at two bytes a block, in a
//! section of their own, and a reader that finds them uses those instead. On the
//! corpus above that takes `the` from thirty three blocks decoded to fourteen
//! and `func` from thirty three to nine.
//!
//! Nine is also what a walk that knew its final threshold in advance would have
//! read for `the`, so the five over that are not the ceiling being loose. They
//! are the threshold arriving late: the walk cannot prune against a bar it has
//! not found yet, and it only finds it once it has scored the ten documents that
//! set it. Narrowing that gap is a different piece of work from this one.
//!
//! The wide section is only the ceilings. It has the same terms in the same
//! order with the same block counts as the narrow one, so a term whose bytes are
//! at `start..end` in the narrow payload is at `2 * start..2 * end` in the wide
//! one, and a directory of its own would be twelve bytes a term for something
//! already on disk. A reader checks the two are in that proportion and ignores
//! the wide one if they are not.
//!
//! # Layout
//!
//! ```text
//! 0..4    terms, u32          how many terms have an entry
//! 4..8    k1, f32
//! 8..12   b, f32
//! 12..16  average, f32        the mean document length the ceilings assume
//! 16..    the directory, `terms` entries of twelve bytes, ascending by offset
//!           0..8    the term's byte offset into the postings section, u64
//!           8..12   where its ceilings start in the payload, u32
//! then    the payload, one byte per block
//! ```
//!
//! and the wide section beside it is the payload again, two bytes per block,
//! little endian, with no header and no directory.
//!
//! The directory is keyed by the offset of the term's posting list rather than
//! by the term itself, because that offset is already in the term dictionary
//! and a lookup there has happened by the time anybody wants a bound. Keying it
//! by the term would mean a second dictionary holding a second copy of the
//! vocabulary.
//!
//! Only terms with at least one whole block get an entry. A term in fewer than
//! a hundred and twenty eight documents has no block to skip, and twelve bytes
//! of directory for each of them would cost more than the vocabulary does,
//! because a real corpus is mostly terms that appear a handful of times.

use crate::codec::{get_u32, get_u64, put_u32, put_u64};
use crate::error::{Error, Result};
use crate::posting::BLOCK_SIZE;

/// What a ceiling of one is stored as.
///
/// The value being quantised is `f / (f + norm)`, which is between zero and one
/// by construction, so a byte holds it with a relative error of one part in two
/// hundred and fifty five. That is four tenths of a percent of slack on a bound
/// that used to have four hundred percent.
pub const SCALE: u8 = u8::MAX;

/// What a ceiling of one is stored as in the wide section.
///
/// Two bytes puts the slack at one part in sixty five thousand, which is under
/// the gap between the top scores of every term this was measured on rather than
/// three times over it.
pub const WIDE_SCALE: u16 = u16::MAX;

/// The bytes a directory entry takes.
const ENTRY: usize = 12;

/// The bytes before the directory starts.
const HEADER: usize = 16;

/// Builds the ceilings for a segment, one term at a time.
///
/// Postings are pushed in the order they are written to the posting list, and
/// [`Writer::finish_term`] closes a term. The block boundaries here have to be
/// the same as the posting list's, which is why the count is kept rather than
/// taken from the caller.
#[derive(Debug)]
pub struct Writer {
    directory: Vec<u8>,
    payload: Vec<u8>,
    /// The same ceilings again at two bytes apiece, which is what a reader that
    /// knows about them prunes with.
    wide: Vec<u8>,
    terms: u32,
    /// The ceilings of the term in progress, including its leftovers.
    pending: Vec<u8>,
    /// The same, wide.
    pending_wide: Vec<u8>,
    /// How many whole blocks the term in progress has produced, which is what
    /// decides whether it is worth an entry at all.
    blocks: usize,
    /// The best ceiling seen in the block in progress.
    best: f32,
    /// How many postings have gone into the block in progress.
    filled: usize,
    k1: f32,
    b: f32,
    /// The mean document length the ceilings were computed against, written into
    /// the section so a searcher working from a different one can correct for it.
    average: f32,
    /// The part of the normalisation that does not vary with the document.
    base: f32,
    /// The part that does, per word of document length.
    per_word: f32,
}

impl Writer {
    /// A writer for a segment scored with these parameters.
    ///
    /// `average` is the mean document length of the segment, which has to be
    /// known before the first posting is pushed. It is the same number
    /// [`Reader::average_length`](crate::index::Reader::average_length) returns
    /// on the way back, and the same clamp is applied to it, because a bound
    /// computed against a different denominator is not a bound.
    #[must_use]
    pub fn new(k1: f32, b: f32, average: f32) -> Self {
        Self {
            directory: Vec::new(),
            payload: Vec::new(),
            wide: Vec::new(),
            terms: 0,
            pending: Vec::new(),
            pending_wide: Vec::new(),
            blocks: 0,
            best: 0.0,
            filled: 0,
            k1,
            b,
            average: average.max(1.0),
            base: k1 * (1.0 - b),
            per_word: k1 * b / average.max(1.0),
        }
    }

    /// Takes one posting of the term in progress.
    #[expect(
        clippy::cast_precision_loss,
        reason = "a frequency or a length past the range of f32 would need a \
                  document of four billion words, and the result is rounded to \
                  one byte anyway"
    )]
    pub fn push(&mut self, frequency: u32, length: u32) {
        let norm = self.per_word.mul_add(length as f32, self.base);
        let frequency = frequency as f32;
        let saturation = frequency / (frequency + norm);
        if saturation > self.best {
            self.best = saturation;
        }
        self.filled += 1;
        if self.filled == BLOCK_SIZE {
            self.close_block();
            self.blocks += 1;
            self.best = 0.0;
            self.filled = 0;
        }
    }

    /// Writes the block in progress out at both widths.
    fn close_block(&mut self) {
        self.pending.push(quantise(self.best));
        self.pending_wide
            .extend_from_slice(&quantise_wide(self.best).to_le_bytes());
    }

    /// Closes the term whose posting list starts at `offset`.
    ///
    /// The leftovers at the end of a list are a block as far as a walk is
    /// concerned, so they get a ceiling of their own at one index past the last
    /// whole block, which is where a cursor reading them reports itself to be.
    pub fn finish_term(&mut self, offset: u64) {
        if self.filled > 0 {
            self.close_block();
        }
        // A term with no whole block has nothing worth skipping and would cost
        // twelve bytes of directory to say so.
        if self.blocks > 0 {
            put_u64(&mut self.directory, offset);
            let start = u32::try_from(self.payload.len()).unwrap_or(u32::MAX);
            put_u32(&mut self.directory, start);
            self.payload.append(&mut self.pending);
            self.wide.append(&mut self.pending_wide);
            self.terms += 1;
        }
        self.pending.clear();
        self.pending_wide.clear();
        self.blocks = 0;
        self.best = 0.0;
        self.filled = 0;
    }

    /// The section, laid out.
    #[must_use]
    pub fn finish(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER + self.directory.len() + self.payload.len());
        put_u32(&mut out, self.terms);
        put_u32(&mut out, self.k1.to_bits());
        put_u32(&mut out, self.b.to_bits());
        put_u32(&mut out, self.average.to_bits());
        out.extend_from_slice(&self.directory);
        out.extend_from_slice(&self.payload);
        out
    }

    /// The wide ceilings, two bytes a block, in the order the payload has them.
    ///
    /// Read after the last [`Writer::finish_term`] and written as a section of
    /// its own. It is not part of [`Writer::finish`] because the two go in
    /// different sections and a caller writing only the narrow one is a caller
    /// writing a segment an older build reads exactly as it reads this one.
    #[must_use]
    pub fn wide(&self) -> &[u8] {
        &self.wide
    }

    /// Whether nothing was worth writing, which is a segment of short lists.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.terms == 0
    }
}

/// Reads the ceilings a segment carries.
#[derive(Debug, Clone, Copy)]
pub struct Reader<'a> {
    directory: &'a [u8],
    payload: &'a [u8],
    /// The same ceilings at two bytes each, or nothing when the segment does not
    /// carry them or carries a set that is not this one's.
    wide: &'a [u8],
    terms: usize,
    k1: f32,
    b: f32,
    average: f32,
}

impl<'a> Reader<'a> {
    /// Opens the section.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Truncated`] if the header or the directory is shorter
    /// than the count at the front says, and [`Error::NotSorted`] if the
    /// directory is not ascending, which is what the binary search relies on.
    pub fn new(input: &'a [u8]) -> Result<Self> {
        let (terms, rest) = get_u32(input)?;
        let (k1, rest) = get_u32(rest)?;
        let (b, rest) = get_u32(rest)?;
        let (average, rest) = get_u32(rest)?;
        let terms = terms as usize;
        let needed = terms.checked_mul(ENTRY).ok_or(Error::Overflow)?;
        if rest.len() < needed {
            return Err(Error::Truncated {
                needed,
                available: rest.len(),
            });
        }
        let (directory, payload) = rest.split_at(needed);
        let reader = Self {
            directory,
            payload,
            wide: &[],
            terms,
            k1: f32::from_bits(k1),
            b: f32::from_bits(b),
            average: f32::from_bits(average),
        };
        // The search below is a binary search, so an out of order directory
        // would not be a wrong answer, it would be a missing one. Refusing it
        // here means a later lookup can be read as a term that has no entry
        // rather than as a term whose entry could not be found.
        let mut last = None;
        for index in 0..terms {
            let (offset, _) = reader.entry(index)?;
            if last.is_some_and(|held| held >= offset) {
                return Err(Error::NotSorted {
                    at: u32::try_from(index).unwrap_or(u32::MAX),
                });
            }
            last = Some(offset);
        }
        Ok(reader)
    }

    /// The same ceilings at two bytes a block, from the section that holds them.
    ///
    /// Anything that is not exactly twice the narrow payload is not this
    /// segment's, and is dropped rather than read into. There is no directory to
    /// check it against, so the length is the whole of what says the two belong
    /// together, and a wrong one would be a bound that does not hold.
    #[must_use]
    pub fn with_wide(mut self, wide: &'a [u8]) -> Self {
        if wide.len() == self.payload.len() * 2 {
            self.wide = wide;
        }
        self
    }

    /// Whether the wide ceilings are there and are this segment's.
    #[must_use]
    pub const fn is_wide(&self) -> bool {
        !self.wide.is_empty()
    }

    /// The parameters the ceilings were computed under.
    ///
    /// A searcher scoring with anything else has to ignore them. See the module
    /// documentation for why that is a correctness matter and not a speed one.
    #[must_use]
    pub const fn parameters(&self) -> (f32, f32) {
        (self.k1, self.b)
    }

    /// The mean document length the ceilings were computed against.
    #[must_use]
    pub const fn average_length(&self) -> f32 {
        self.average
    }

    /// What a ceiling has to be multiplied by to hold at a different average
    /// document length.
    ///
    /// A term contributes `f / (f + c0 + c1 * len / a)` with `c0` and `c1` fixed
    /// by `k1` and `b`. Raising `a` shrinks the denominator, so a ceiling
    /// computed at the segment's own average is not a ceiling at the larger
    /// average a store of several segments scores by. Writing `s` for the stored
    /// average and `q` for the one being scored with, the ratio between the two
    /// contributions is
    ///
    /// ```text
    /// (f + c0 + c1 * len / s) / (f + c0 + c1 * len / q)
    /// ```
    ///
    /// which is at most one when `q <= s`, and at most `q / s` otherwise, since
    /// `s / q * (u + p) <= u + s / q * p` for any non negative `u` and `p` once
    /// `s <= q`. So `max(1, q / s)` scales a ceiling into one that holds, it is
    /// exactly one whenever the averages agree, and it approaches the truth as
    /// documents get long. That is what lets a multi segment store keep the
    /// tight bound instead of falling back on every query.
    #[must_use]
    pub fn scale(&self, average: f32) -> f32 {
        let stored = self.average.max(1.0);
        (average.max(1.0) / stored).max(1.0)
    }

    /// How many terms have ceilings.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.terms
    }

    /// Whether no term does.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.terms == 0
    }

    /// The ceilings of the term whose posting list starts at `offset`, one byte
    /// per block, or nothing when that term has none.
    ///
    /// An absent term is not an error. Short lists are left out on purpose and
    /// a reader of a segment written by a build that stored fewer of them, or
    /// none, has to keep working.
    #[must_use]
    pub fn get(&self, offset: u64) -> &'a [u8] {
        let Some(range) = self.range(offset) else {
            return &[];
        };
        self.payload.get(range).unwrap_or(&[])
    }

    /// The ceilings of that term at the best width the segment carries.
    ///
    /// This is what a walk wants. [`Reader::get`] is the narrow bytes as they
    /// are stored, which is what a test or a dump wants.
    #[must_use]
    pub fn ceilings(&self, offset: u64) -> Ceilings<'a> {
        let Some(range) = self.range(offset) else {
            return Ceilings::default();
        };
        let Some(narrow) = self.payload.get(range.clone()) else {
            return Ceilings::default();
        };
        // Twice the offsets, because the wide payload is the narrow one with two
        // bytes where it has one and nothing else different. An out of range
        // slice leaves the narrow ceilings on their own rather than dropping
        // both, since the narrow ones are still a bound.
        let wide = self
            .wide
            .get(range.start * 2..range.end * 2)
            .unwrap_or_default();
        Ceilings { narrow, wide }
    }

    /// Where in the payload a term's ceilings are, if it has any.
    fn range(&self, offset: u64) -> Option<std::ops::Range<usize>> {
        let index = self.find(offset)?;
        let (_, start) = self.entry(index).ok()?;
        let end = if index + 1 < self.terms {
            self.entry(index + 1).ok()?.1
        } else {
            self.payload.len()
        };
        Some(start..end)
    }

    /// Where in the directory a posting list offset sits, if it is there.
    fn find(&self, offset: u64) -> Option<usize> {
        let (mut low, mut high) = (0usize, self.terms);
        while low < high {
            let middle = low + (high - low) / 2;
            let (found, _) = self.entry(middle).ok()?;
            match found.cmp(&offset) {
                std::cmp::Ordering::Less => low = middle + 1,
                std::cmp::Ordering::Equal => return Some(middle),
                std::cmp::Ordering::Greater => high = middle,
            }
        }
        None
    }

    /// One directory entry: the posting list offset it is for, and where its
    /// ceilings start.
    fn entry(&self, index: usize) -> Result<(u64, usize)> {
        let at = index * ENTRY;
        let bytes = self.directory.get(at..at + ENTRY).ok_or(Error::Truncated {
            needed: at + ENTRY,
            available: self.directory.len(),
        })?;
        let (offset, rest) = get_u64(bytes)?;
        let (start, _) = get_u32(rest)?;
        Ok((offset, start as usize))
    }
}

/// The ceilings of one term's blocks, at whatever width the segment carries.
///
/// A walk asks for a block and gets a fraction of the most the term can
/// contribute, and does not have to know which section the number came out of.
#[derive(Debug, Clone, Copy, Default)]
pub struct Ceilings<'a> {
    narrow: &'a [u8],
    /// The same blocks at two bytes each, or nothing.
    wide: &'a [u8],
}

impl Ceilings<'_> {
    /// How many blocks the term has.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.narrow.len()
    }

    /// Whether the term has no ceilings at all, which is a short list or a
    /// segment written before the section existed.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.narrow.is_empty()
    }

    /// Whether these are the two byte ceilings.
    #[must_use]
    pub const fn is_wide(&self) -> bool {
        !self.wide.is_empty()
    }

    /// What a block reaches, as a fraction of the most the term can contribute.
    #[must_use]
    pub fn get(&self, block: usize) -> Option<f32> {
        if self.wide.is_empty() {
            let quantum = *self.narrow.get(block)?;
            return Some(f32::from(quantum) / f32::from(SCALE));
        }
        let at = block * 2;
        let bytes: [u8; 2] = self.wide.get(at..at + 2)?.try_into().ok()?;
        Some(f32::from(u16::from_le_bytes(bytes)) / f32::from(WIDE_SCALE))
    }

    /// The blocks in descending order of what they reach.
    ///
    /// A walk in this order can stop at the first block under the threshold,
    /// which is the whole point of it, so the order has to be exact and not
    /// nearly right.
    #[must_use]
    pub fn order(&self) -> Vec<u32> {
        let order: Vec<u32> = (0..self.len())
            .map(|block| u32::try_from(block).unwrap_or(u32::MAX))
            .collect();
        if self.wide.is_empty() {
            return sorted_by_byte(&order, self.narrow, 1, 0);
        }
        // Least significant byte first and then the most significant, each pass
        // keeping the order equals arrived in, which is what leaves the whole
        // two byte key descending after the second.
        let order = sorted_by_byte(&order, self.wide, 2, 0);
        sorted_by_byte(&order, self.wide, 2, 1)
    }
}

/// One pass of a counting sort over blocks, by one byte of each block's ceiling,
/// largest first and keeping the order equals arrived in.
fn sorted_by_byte(order: &[u32], bytes: &[u8], stride: usize, at: usize) -> Vec<u32> {
    let key = |block: u32| {
        usize::from(
            bytes
                .get(block as usize * stride + at)
                .copied()
                .unwrap_or_default(),
        )
    };
    let mut counts = [0u32; 256];
    for &block in order {
        counts[key(block)] += 1;
    }
    // Running total from the top down, so the bucket of the largest byte starts
    // at nothing and each one after it starts where the last ended.
    let mut running = 0u32;
    let mut start = [0u32; 256];
    for quantum in (0..256).rev() {
        start[quantum] = running;
        running += counts[quantum];
    }
    let mut out = vec![0u32; order.len()];
    for &block in order {
        let slot = &mut start[key(block)];
        // The counting pass above is what makes every subscript here one the
        // vector has.
        if let Some(place) = out.get_mut(*slot as usize) {
            *place = block;
        }
        *slot += 1;
    }
    out
}

/// A ceiling between zero and one, as a byte, rounded up.
///
/// Rounded up because a bound that is rounded down is not a bound. A block
/// whose ceiling is quantised below its true best would be skipped when it held
/// a document that belonged in the answer, and the result would be a missing
/// hit rather than a wrong one, which is the failure nobody spots.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "clamped into zero to two hundred and fifty five before the cast"
)]
fn quantise(saturation: f32) -> u8 {
    let scale = f32::from(SCALE);
    (saturation * scale).ceil().clamp(0.0, scale) as u8
}

/// What a stored ceiling means, as a fraction of a term's best.
#[must_use]
pub fn ceiling(quantum: u8) -> f32 {
    f32::from(quantum) / f32::from(SCALE)
}

/// The same, in the wide section.
///
/// Rounded up for the same reason, and rounded up from the same number rather
/// than from the byte beside it, so the two sections are two quantisations of
/// one saturation and not one quantisation of the other.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "clamped into zero to sixty five thousand five hundred and thirty \
              five before the cast"
)]
fn quantise_wide(saturation: f32) -> u16 {
    let scale = f32::from(WIDE_SCALE);
    (saturation * scale).ceil().clamp(0.0, scale) as u16
}

#[cfg(test)]
mod tests {
    #![expect(
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        reason = "test fixtures counting to a few hundred"
    )]

    use super::*;

    /// The ceiling a block of these postings should end up with, computed the
    /// long way so the test does not repeat the code it is checking.
    fn best(postings: &[(u32, u32)], k1: f32, b: f32, average: f32) -> f32 {
        postings
            .iter()
            .map(|&(frequency, length)| {
                let norm = k1 * (1.0 - b + b * length as f32 / average.max(1.0));
                frequency as f32 / (frequency as f32 + norm)
            })
            .fold(0.0f32, f32::max)
    }

    fn one_term(postings: &[(u32, u32)], k1: f32, b: f32, average: f32) -> Vec<u8> {
        let mut writer = Writer::new(k1, b, average);
        for &(frequency, length) in postings {
            writer.push(frequency, length);
        }
        writer.finish_term(0);
        writer.finish()
    }

    #[test]
    fn a_term_with_no_whole_block_is_not_worth_an_entry() {
        let postings: Vec<(u32, u32)> = (0..BLOCK_SIZE - 1).map(|_| (3, 500)).collect();
        let bytes = one_term(&postings, 1.2, 0.75, 400.0);
        let reader = Reader::new(&bytes).expect("reads");
        assert!(reader.is_empty());
        assert_eq!(reader.get(0), &[] as &[u8]);
    }

    #[test]
    fn a_whole_block_and_its_leftovers_both_get_a_ceiling() {
        let postings: Vec<(u32, u32)> = (0..BLOCK_SIZE + 5).map(|_| (3, 500)).collect();
        let bytes = one_term(&postings, 1.2, 0.75, 400.0);
        let reader = Reader::new(&bytes).expect("reads");
        assert_eq!(reader.get(0).len(), 2);
    }

    #[test]
    fn a_block_that_ends_exactly_has_no_leftovers() {
        let postings: Vec<(u32, u32)> = (0..BLOCK_SIZE * 2).map(|_| (3, 500)).collect();
        let bytes = one_term(&postings, 1.2, 0.75, 400.0);
        let reader = Reader::new(&bytes).expect("reads");
        assert_eq!(reader.get(0).len(), 2);
    }

    #[test]
    fn the_ceiling_is_never_below_the_best_posting_in_the_block() {
        // The property the whole thing rests on. A ceiling under the truth
        // skips a block that held an answer.
        let (k1, b, average) = (1.2f32, 0.75f32, 400.0f32);
        let mut postings: Vec<(u32, u32)> = Vec::new();
        for at in 0..BLOCK_SIZE {
            postings.push((at as u32 % 17 + 1, (at as u32 % 91) * 40 + 1));
        }
        let bytes = one_term(&postings, k1, b, average);
        let reader = Reader::new(&bytes).expect("reads");
        let stored = ceiling(reader.get(0)[0]);
        let truth = best(&postings, k1, b, average);
        assert!(
            stored >= truth,
            "stored {stored} is under the truth {truth}"
        );
        assert!(
            stored - truth < 1.0 / f32::from(SCALE),
            "stored {stored} is more than a quantum above {truth}"
        );
    }

    #[test]
    fn the_ceiling_is_much_tighter_than_the_frequency_bound_it_replaces() {
        // The number that justifies the section existing. The old bound pinned
        // the normalisation to the shortest document that could exist, so it
        // was loose by whatever the real normalisation came to, and on a
        // document several times the average length that is a factor of four.
        let (k1, b, average) = (1.2f32, 0.75f32, 400.0f32);
        let postings: Vec<(u32, u32)> = (0..BLOCK_SIZE)
            .map(|at| (at as u32 % 3 + 1, 3000))
            .collect();
        let bytes = one_term(&postings, k1, b, average);
        let reader = Reader::new(&bytes).expect("reads");
        let now = ceiling(reader.get(0)[0]);
        let truth = best(&postings, k1, b, average);

        let frequency = postings.iter().map(|p| p.0).max().expect("some") as f32;
        let floor = k1 * (1.0 - b);
        let before = frequency / (frequency + floor);

        assert!(
            before > truth * 3.0,
            "the old bound {before} was meant to be far above the truth {truth}"
        );
        assert!(
            now - truth < 1.0 / f32::from(SCALE),
            "the new bound {now} was meant to be on top of the truth {truth}"
        );
    }

    #[test]
    fn a_block_holding_a_document_past_the_saturating_frequency_still_bounds_it() {
        // The old skip entry is one byte, so every block with a document
        // holding the term two hundred and fifty five times or more reported
        // the same bound whatever else was in it. Two blocks that differ only
        // in how long that document is have to get different ceilings here.
        let (k1, b, average) = (1.2f32, 0.75f32, 400.0f32);
        let short: Vec<(u32, u32)> = (0..BLOCK_SIZE).map(|_| (400, 500)).collect();
        let long: Vec<(u32, u32)> = (0..BLOCK_SIZE).map(|_| (400, 200_000)).collect();
        let short = Reader::new(&one_term(&short, k1, b, average))
            .expect("reads")
            .get(0)[0];
        let long = Reader::new(&one_term(&long, k1, b, average))
            .expect("reads")
            .get(0)[0];
        assert!(
            ceiling(long) < ceiling(short),
            "the long document should bound lower, got {long} against {short}"
        );
    }

    #[test]
    fn terms_are_found_by_the_offset_of_their_posting_list() {
        let mut writer = Writer::new(1.2, 0.75, 400.0);
        for offset in [10u64, 4000, 900_000] {
            for at in 0..BLOCK_SIZE {
                writer.push(at as u32 % 5 + 1, 100);
            }
            writer.finish_term(offset);
        }
        let bytes = writer.finish();
        let reader = Reader::new(&bytes).expect("reads");
        assert_eq!(reader.len(), 3);
        for offset in [10u64, 4000, 900_000] {
            assert_eq!(reader.get(offset).len(), 1, "offset {offset}");
        }
        for offset in [0u64, 11, 3999, 900_001, u64::MAX] {
            assert_eq!(reader.get(offset), &[] as &[u8], "offset {offset}");
        }
    }

    #[test]
    fn the_parameters_come_back_as_they_went_in() {
        let bytes = Writer::new(1.7, 0.3, 400.0).finish();
        let reader = Reader::new(&bytes).expect("reads");
        let (k1, b) = reader.parameters();
        assert!((k1 - 1.7).abs() < 1e-9, "{k1}");
        assert!((b - 0.3).abs() < 1e-9, "{b}");
        assert!((reader.average_length() - 400.0).abs() < 1e-9);
    }

    #[test]
    fn an_average_that_agrees_costs_nothing() {
        let bytes = Writer::new(1.2, 0.75, 400.0).finish();
        let reader = Reader::new(&bytes).expect("reads");
        assert!((reader.scale(400.0) - 1.0).abs() < 1e-9);
        // A shorter average makes every normalisation larger and every
        // contribution smaller, so the stored ceiling already holds.
        assert!((reader.scale(200.0) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn the_scaled_ceiling_still_bounds_the_block_at_a_longer_average() {
        // The property the correction exists for. A segment of short documents
        // inside a store of long ones is scored against an average larger than
        // its own, which makes every contribution larger than what was stored.
        let (k1, b) = (1.2f32, 0.75f32);
        let mine = 200.0f32;
        let postings: Vec<(u32, u32)> = (0..BLOCK_SIZE)
            .map(|at| (at as u32 % 9 + 1, (at as u32 % 40) * 25 + 1))
            .collect();
        let bytes = one_term(&postings, k1, b, mine);
        let reader = Reader::new(&bytes).expect("reads");
        let stored = ceiling(reader.get(0)[0]);
        for theirs in [200.0f32, 400.0, 1000.0, 50_000.0] {
            let held = stored * reader.scale(theirs);
            let truth = best(&postings, k1, b, theirs);
            assert!(
                held >= truth,
                "at average {theirs} the scaled ceiling {held} is under the truth {truth}"
            );
        }
    }

    #[test]
    fn the_scaled_ceiling_is_still_far_tighter_than_the_bound_it_replaces() {
        // The correction is only worth having if what comes out of it is still
        // better than pinning the normalisation at its floor, which is what a
        // segment with no ceilings falls back to.
        let (k1, b) = (1.2f32, 0.75f32);
        let postings: Vec<(u32, u32)> = (0..BLOCK_SIZE)
            .map(|at| (at as u32 % 3 + 1, 3000))
            .collect();
        let bytes = one_term(&postings, k1, b, 300.0);
        let reader = Reader::new(&bytes).expect("reads");
        let held = ceiling(reader.get(0)[0]) * reader.scale(400.0);

        let frequency = postings.iter().map(|p| p.0).max().expect("some") as f32;
        let floor = k1 * (1.0 - b);
        let before = frequency / (frequency + floor);

        assert!(held < before / 2.0, "{held} against {before}");
    }

    #[test]
    fn an_average_length_of_zero_does_not_divide_by_it() {
        let postings: Vec<(u32, u32)> = (0..BLOCK_SIZE).map(|_| (1, 0)).collect();
        let bytes = one_term(&postings, 1.2, 0.75, 0.0);
        let reader = Reader::new(&bytes).expect("reads");
        assert!(ceiling(reader.get(0)[0]) <= 1.0);
    }

    #[test]
    fn a_truncated_section_is_an_error_not_a_panic() {
        let mut writer = Writer::new(1.2, 0.75, 400.0);
        for offset in [0u64, 500] {
            for at in 0..BLOCK_SIZE {
                writer.push(at as u32 % 5 + 1, 100);
            }
            writer.finish_term(offset);
        }
        let bytes = writer.finish();
        for cut in 0..bytes.len() {
            // A prefix that parses has to answer every lookup without reading
            // past what is there, and one that does not parse has to say so
            // rather than panic.
            if let Ok(reader) = Reader::new(&bytes[..cut]) {
                for offset in [0u64, 500, 900_000] {
                    let _ = reader.get(offset);
                }
            }
        }
    }

    #[test]
    fn a_directory_out_of_order_is_refused() {
        let mut bytes = Vec::new();
        put_u32(&mut bytes, 2);
        put_u32(&mut bytes, 1.2f32.to_bits());
        put_u32(&mut bytes, 0.75f32.to_bits());
        put_u32(&mut bytes, 400.0f32.to_bits());
        put_u64(&mut bytes, 900);
        put_u32(&mut bytes, 0);
        put_u64(&mut bytes, 10);
        put_u32(&mut bytes, 1);
        bytes.extend_from_slice(&[200, 200]);
        assert!(matches!(
            Reader::new(&bytes),
            Err(Error::NotSorted { at: 1 })
        ));
    }

    #[test]
    fn a_ceiling_of_one_survives_the_round_trip() {
        // b of one and a document of no length leaves the normalisation at
        // zero, so the saturation is exactly one and the clamp has to hold.
        assert_eq!(quantise(1.0), SCALE);
        assert_eq!(quantise(2.0), SCALE);
        assert!((ceiling(SCALE) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn the_smallest_ceiling_that_is_not_zero_rounds_up_rather_than_away() {
        assert_eq!(quantise(0.0), 0);
        assert_eq!(quantise(0.000_1), 1);
    }

    /// The wide bytes for these ceilings, so a test can build a `Ceilings` at
    /// either width without going through a whole segment.
    fn widen(quanta: &[u16]) -> Vec<u8> {
        quanta
            .iter()
            .flat_map(|quantum| quantum.to_le_bytes())
            .collect()
    }

    #[test]
    fn the_blocks_come_back_largest_ceiling_first_and_ties_in_document_order() {
        let narrow = [3u8, 200, 7, 200, 0, 255];
        let ceilings = Ceilings {
            narrow: &narrow,
            wide: &[],
        };
        assert_eq!(ceilings.order(), [5, 1, 3, 2, 0, 4]);
        assert_eq!(Ceilings::default().order(), Vec::<u32>::new());
        assert_eq!(
            Ceilings {
                narrow: &[9],
                wide: &[]
            }
            .order(),
            [0]
        );

        // Every block once and none of them twice, which is what the walk
        // relies on to score every document in a block it does not prune.
        let narrow: Vec<u8> = (0..1_000u32)
            .map(|i| u8::try_from((i * 37) % 256).expect("a remainder of 256 is a byte"))
            .collect();
        let mut seen = Ceilings {
            narrow: &narrow,
            wide: &[],
        }
        .order();
        assert_eq!(seen.len(), 1_000);
        seen.sort_unstable();
        assert!(seen.iter().copied().eq(0..1_000));
    }

    #[test]
    fn the_wide_blocks_come_back_in_exactly_descending_order() {
        // Values chosen so the low byte disagrees with the high byte, which is
        // what a radix sort that ran only one of its two passes would get wrong.
        let quanta = [0x01_00u16, 0x00_FF, 0x02_00, 0x00_01, 0x02_00];
        let wide = widen(&quanta);
        let ceilings = Ceilings {
            narrow: &[0u8; 5],
            wide: &wide,
        };
        assert_eq!(ceilings.order(), [2, 4, 0, 1, 3]);

        let quanta: Vec<u16> = (0..2_000u32)
            .map(|i| {
                u16::try_from((i * 7_919) % 65_536).expect("a remainder of 65536 is two bytes")
            })
            .collect();
        let wide = widen(&quanta);
        let order = Ceilings {
            narrow: &vec![0u8; quanta.len()],
            wide: &wide,
        }
        .order();
        assert_eq!(order.len(), quanta.len());
        for pair in order.windows(2) {
            let (before, after) = (quanta[pair[0] as usize], quanta[pair[1] as usize]);
            assert!(before >= after, "{before} came before {after}");
        }
        let mut seen = order;
        seen.sort_unstable();
        assert!(seen.iter().copied().eq(0..2_000));
    }

    #[test]
    fn a_wide_section_that_is_not_twice_the_narrow_one_is_ignored() {
        // There is no directory over the wide section, so its length is the
        // whole of what says it belongs to this segment. A section that is not
        // this one's would be read as ceilings that do not bound the blocks.
        let postings: Vec<(u32, u32)> = (0..BLOCK_SIZE * 3)
            .map(|at| (at as u32 % 9 + 1, 500))
            .collect();
        let mut writer = Writer::new(1.2, 0.75, 400.0);
        for &(frequency, length) in &postings {
            writer.push(frequency, length);
        }
        writer.finish_term(0);
        let wide = writer.wide().to_vec();
        let bytes = writer.finish();

        let reader = Reader::new(&bytes).expect("reads");
        assert_eq!(wide.len(), reader.get(0).len() * 2);
        assert!(reader.with_wide(&wide).is_wide());
        assert!(!reader.with_wide(&wide[..wide.len() - 2]).is_wide());
        assert!(!reader.with_wide(&[]).is_wide());
        let mut longer = wide.clone();
        longer.extend_from_slice(&[0, 0]);
        assert!(!reader.with_wide(&longer).is_wide());
    }

    #[test]
    fn the_wide_ceiling_is_the_same_saturation_at_a_finer_grain() {
        let (k1, b, average) = (1.2f32, 0.75f32, 400.0f32);
        let postings: Vec<(u32, u32)> = (0..BLOCK_SIZE)
            .map(|at| (at as u32 % 17 + 1, (at as u32 % 91) * 40 + 1))
            .collect();
        let mut writer = Writer::new(k1, b, average);
        for &(frequency, length) in &postings {
            writer.push(frequency, length);
        }
        writer.finish_term(0);
        let wide = writer.wide().to_vec();
        let bytes = writer.finish();
        let reader = Reader::new(&bytes).expect("reads");

        let truth = best(&postings, k1, b, average);
        let narrow = reader.ceilings(0).get(0).expect("a block");
        let wide = reader.with_wide(&wide).ceilings(0).get(0).expect("a block");
        // Both bound it, and the wide one is the tighter of the two, which is
        // the entire reason the section exists.
        assert!(wide >= truth, "wide {wide} is under the truth {truth}");
        assert!(narrow >= wide, "narrow {narrow} is under the wide {wide}");
        assert!(
            wide - truth < 1.0 / f32::from(WIDE_SCALE),
            "wide {wide} is more than a quantum above {truth}"
        );
    }

    #[test]
    fn a_segment_without_the_wide_section_reads_its_ceilings_from_the_narrow_one() {
        let postings: Vec<(u32, u32)> = (0..=BLOCK_SIZE).map(|_| (3, 500)).collect();
        let bytes = one_term(&postings, 1.2, 0.75, 400.0);
        let reader = Reader::new(&bytes).expect("reads");
        let ceilings = reader.ceilings(0);
        assert!(!ceilings.is_wide());
        assert_eq!(ceilings.len(), 2);
        assert_eq!(ceilings.get(0), Some(ceiling(reader.get(0)[0])));
        assert_eq!(ceilings.get(2), None);
        assert!(reader.ceilings(9_000).is_empty());
    }

    #[test]
    fn a_wide_ceiling_of_one_survives_the_round_trip() {
        assert_eq!(quantise_wide(1.0), WIDE_SCALE);
        assert_eq!(quantise_wide(2.0), WIDE_SCALE);
        assert_eq!(quantise_wide(0.0), 0);
        assert_eq!(quantise_wide(0.000_001), 1);
    }
}
