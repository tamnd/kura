//! A byte oriented compressor for the sections that hold text.
//!
//! The stored fields are the largest thing in a segment by a wide margin. An
//! index of a corpus of prose is a small fraction of the prose, and the copy of
//! the prose kept to show a person is the whole of it, so a segment that keeps
//! the text raw is a segment that is mostly text. Compressing it is the single
//! largest saving available on disk, and it is available for the price of one
//! pass over data the writer is already touching.
//!
//! The format is the LZ4 block format. It was not invented here on purpose.
//! It decompresses at gigabytes a second, the decoder is a hundred lines, and
//! a block written by this code is a block any other LZ4 implementation reads,
//! which matters the first time somebody has to look inside a file without
//! this crate to hand.
//!
//! What is here is the format and not the reference implementation. There is no
//! unsafe code, no unaligned wide loads and no output overrun to be undone
//! afterwards, which costs some speed on the compression side and buys a
//! decoder that cannot be talked into reading past the end of its input by a
//! file that arrived from somewhere else. That trade is the whole point of the
//! crate.
//!
//! # The format
//!
//! A block is a series of sequences. Each is a token byte, then any literals,
//! then a two byte little endian offset backwards into the output, then the
//! length of the match to copy from there. The token packs the two lengths into
//! a nibble each, and a nibble of 15 means the rest of the length follows as a
//! chain of bytes that ends at the first one under 255.
//!
//! The last sequence in a block is literals with no match after it, which is
//! how the decoder knows a block has ended without a terminator.
//!
//! The compatibility claim is checked both ways against the reference lz4
//! binary rather than assumed, and rechecked whenever the encoder changes:
//! nine hundred kilobytes of this crate's own source compressed here and
//! decompressed there, and the same source compressed there at both ends of its
//! level range and decompressed here. A block is wrapped in the legacy frame
//! for that, which is a magic number and a length and carries no checksum, so
//! it can be written without a second hash function. The test that stays behind
//! is a block written out by hand, which is the part of that check that does not
//! need a second implementation installed to run.
//!
//! # Finding the matches
//!
//! The format says what a block looks like and says nothing about which matches
//! go in it, and that second part is where the ratio comes from. Two encoders
//! that both write this format can differ by a third on the same input.
//!
//! Every position goes into a table under the hash of its four bytes, and every
//! table entry points at the position before it that hashed the same way, so a
//! search can walk the sixteen nearest candidates and keep the longest rather
//! than take the first one it is handed. It then looks one byte further on, and
//! when there is a longer match there it spends a literal to reach it, because
//! a greedy encoder that takes the first match it finds pays for that choice for
//! the rest of the block: the match it took is where the next search has to
//! start.
//!
//! In blocks of eight kilobytes, which is the size the stored fields are written
//! in, that is worth eight percent of the section on real prose and twenty two
//! on generated text. Ten megabytes of markdown compressed to 0.603 of itself
//! taking the first candidate and 0.557 this way, and the benchmark's corpus to
//! 0.389 and 0.301. On disk the store went from 0.399 of the corpus to 0.314.
//!
//! It costs three to seven times the compression speed in blocks that size, and
//! thirteen when a whole file is one block and the chains are as long as the
//! file. That is paid once, when a segment is written. On the other side, which
//! is the one a query is on, nothing is given up: the blocks are made of fewer
//! and longer sequences, so eight kilobytes comes back a few percent faster than
//! it did, and reading one stored document at random got five percent quicker.
//!
//! # Why not zstd
//!
//! Because it would be the core crate's first dependency, and because writing
//! one here is a much larger surface than this is.
//!
//! It would be smaller. Measured on the same three corpora in the same eight
//! kilobyte blocks, `zstd -3` reaches 0.423 on the markdown against 0.557 here,
//! and 0.348 on a directory of Rust source against 0.458. That is a fifth to a
//! quarter off the largest section in a segment and it is not a rounding error.
//! Almost all of it is the entropy stage, which is the part no encoder for this
//! format can have, so it is not something a better match finder closes.
//!
//! What it costs is decompression speed, which is the side a query is on.
//! Blocks of eight kilobytes decompress here at between 1.8 and 2.2 gigabytes a
//! second and through `zstd -3` at between 0.3 and 0.7, so the section that is a
//! quarter smaller takes three to five times as long to read back.
//!
//! The dependency is the part that settles it rather than the ratio. `zstd-sys`
//! is a C library, and this crate cross builds for `wasm32-unknown-unknown` in
//! CI, where there is no C toolchain; `ruzstd` decompresses and does not
//! compress. Writing zstd here means FSE, Huffman, the sequence format and the
//! frame, on a decode path that has to refuse every hostile byte sequence
//! without panicking. That is a project and not a change, so it is
//! [issue #76][zstd] rather than a paragraph in this one.
//!
//! [zstd]: https://github.com/tamnd/kura/issues/76

use crate::error::{Error, Result};

/// How many bytes a copy moves at a time, and how much slack the output buffer
/// is given so that it can.
///
/// Sixteen is two machine words on the platforms this runs on, which is the
/// largest piece the compiler will move without being asked twice.
const WIDE: usize = 16;

/// The shortest match worth encoding.
///
/// Three bytes cost three bytes to encode, so a match has to reach four before
/// it saves anything. The format bakes this in: a match length is stored with
/// four already subtracted.
const MIN_MATCH: usize = 4;

/// The bytes at the end of a block that are always literals.
///
/// The reference decoder reads and writes in wide chunks and relies on this
/// margin to do so without checking. This decoder does not need the margin, but
/// it writes it, because a block that leaves it out is a block the reference
/// decoder would refuse.
const LAST_LITERALS: usize = 5;

/// How far from the end a match may start.
const MF_LIMIT: usize = 12;

/// The furthest back a match may point, which is what fits in two bytes.
const MAX_DISTANCE: usize = 65535;

/// How many entries the match finder's table of heads has.
///
/// Sixteen thousand slots for a block of a few tens of kilobytes, which is
/// enough that collisions are rare and small enough that the table stays in
/// cache alongside the block it is indexing.
const HASH_LOG: u32 = 14;

/// How many positions the chain remembers.
///
/// A match cannot point further back than [`MAX_DISTANCE`], so a position more
/// than that many bytes ago is of no use to anybody and its slot is free to be
/// reused. Two positions that land in the same slot are 65536 apart, and the
/// one that is still reachable is the later of them.
const CHAIN: usize = MAX_DISTANCE + 1;

/// How many candidates the search looks at before it takes what it has.
///
/// The chain for a hash is in most recent order, so the search is looking at
/// the nearest matches first and giving up while the ones left are the distant
/// ones. Sixteen was measured rather than guessed. Over ten megabytes of prose
/// in blocks of eight kilobytes, four is 0.9 percent larger and sixty four is
/// 0.4 percent smaller for half again the time, so the curve has flattened out
/// by here. It flattens because a block holds eight kilobytes of history and
/// there is only so much in it to find.
const SEARCH_DEPTH: usize = 16;

/// The match length at which the search stops looking for a better one.
///
/// A match this long already costs about a byte for every sixty four it covers,
/// so the most a longer one can save is under two percent of what is left, and
/// the search is not free. This is what keeps a run of one repeated byte from
/// walking the whole chain at every position: the first candidate covers the
/// rest of the block, and there is nothing further to ask.
const LONG_ENOUGH: usize = 64;

/// The multiplier of a Fibonacci hash, which is the odd integer nearest to two
/// to the thirty second over the golden ratio.
const HASH_MUL: u32 = 2_654_435_761;

/// How quickly the scan gives up on data that is not compressing.
///
/// After enough consecutive misses the scan starts stepping over positions
/// rather than testing every one. On text this never triggers, and on data that
/// is already compressed it is the difference between a pass and a crawl.
const SKIP_TRIGGER: u32 = 6;

/// The most bytes a block of `len` can compress to.
///
/// Compression can make data larger, and a caller sizing a buffer needs to know
/// by how much before it starts. The answer is the input, plus one token per
/// 255 bytes of literals, plus the last token.
#[must_use]
pub const fn bound(len: usize) -> usize {
    len + len / 255 + 16
}

/// A match the search settled on, after it had looked at the alternatives.
struct Found {
    /// Where the match starts in the input, which is at or before the position
    /// the hash was taken at, because a match usually reaches back further than
    /// the four bytes that found it.
    start: usize,
    /// Where the earlier copy of those bytes starts.
    back: usize,
    /// How many bytes the two have in common from `start` and `back` on.
    length: usize,
}

/// Finds matches and writes blocks.
///
/// The tables are owned by the compressor rather than made per block, because a
/// table made per block is either an allocation or a memset on the hot path and
/// this format is fast enough that either one would show.
#[derive(Debug)]
pub struct Compressor {
    /// The most recent position whose four bytes hashed into each slot.
    heads: Vec<u32>,
    /// For each position, the position before it that hashed the same way.
    ///
    /// Following this from a head walks every position with that hash, most
    /// recent first, which is what makes it possible to keep looking after the
    /// first candidate rather than take it.
    chain: Vec<u32>,
    /// What is added to a position before it goes in a table, so that entries
    /// from earlier blocks are recognisably stale without clearing anything.
    base: u32,
}

impl Default for Compressor {
    fn default() -> Self {
        Self::new()
    }
}

impl Compressor {
    /// Creates a compressor.
    ///
    /// The two tables together are three hundred and twenty kilobytes, which is
    /// the price of the chain and is paid once per writer rather than per block.
    #[must_use]
    pub fn new() -> Self {
        Self {
            heads: vec![0; 1 << HASH_LOG],
            chain: vec![0; CHAIN],
            // Positions start at one so that a zeroed table holds nothing that
            // can be mistaken for a position in the first block.
            base: 1,
        }
    }

    /// Compresses `input` and appends the block to `out`.
    ///
    /// The block is self contained. Nothing in it refers to a previous block,
    /// so blocks can be decompressed in any order and one damaged block does
    /// not cost the ones after it.
    pub fn compress(&mut self, input: &[u8], out: &mut Vec<u8>) {
        self.stamp(input.len());
        out.reserve(bound(input.len()));

        if input.len() < MF_LIMIT + 1 {
            // Too short for the end rules to leave anywhere a match could
            // start, so the whole thing is literals.
            literals(out, input);
            return;
        }

        let limit = input.len() - MF_LIMIT;
        let end = input.len() - LAST_LITERALS;
        let mut anchor = 0usize;
        // Position zero is never a candidate, which costs one byte of ratio and
        // keeps the table's empty value unambiguous.
        let mut ip = 1usize;
        let mut misses: u32 = 1 << SKIP_TRIGGER;

        while ip < limit {
            let found = self.longest(input, ip, anchor, end);
            self.insert(input, ip);

            let Some(mut found) = found else {
                ip += (misses >> SKIP_TRIGGER) as usize;
                misses += 1;
                continue;
            };
            misses = 1 << SKIP_TRIGGER;

            // One byte further on there may be a longer match, and when there
            // is, spending a literal to reach it costs a byte and saves more.
            // A greedy encoder takes the first match it finds and pays for that
            // for the rest of the block, because the match it took is what the
            // next search has to start after.
            let mut inserted = ip + 1;
            if found.length < LONG_ENOUGH && ip + 1 < limit {
                let later = self.longest(input, ip + 1, anchor, end);
                self.insert(input, ip + 1);
                inserted = ip + 2;
                if let Some(later) = later
                    && later.length > found.length
                {
                    found = later;
                }
            }

            let stop = found.start + found.length;
            sequence_out(
                out,
                &input[anchor..found.start],
                found.start - found.back,
                found.length,
            );

            // Every position the match covered still goes into the tables. The
            // encoder is done with them, but the next search is not, and a chain
            // with the inside of every match missing from it is a chain that
            // finds the second copy of a phrase and not the third.
            for at in inserted.min(stop)..stop.min(limit) {
                self.insert(input, at);
            }

            ip = stop;
            anchor = ip;
        }

        literals(out, &input[anchor..]);
    }

    /// How many bytes this compressor is holding.
    ///
    /// The two tables, which are the same size from the moment they are made and
    /// never grow. It is here because a caller adding up what it holds would
    /// otherwise have to know that number, and a constant somebody else has to
    /// know is a constant that goes stale.
    #[must_use]
    pub fn held(&self) -> usize {
        (self.heads.capacity() + self.chain.capacity()) * core::mem::size_of::<u32>()
    }

    /// The longest match at `at`, or nothing if there is none worth encoding.
    ///
    /// The chain is walked most recent first and the walk stops at
    /// [`SEARCH_DEPTH`] candidates, or sooner if it reaches one that is out of
    /// range or is not further back than the one before it. That last check is
    /// what keeps a chain that two distant positions have shared a slot in from
    /// being walked in circles.
    fn longest(&self, input: &[u8], at: usize, anchor: usize, end: usize) -> Option<Found> {
        let sequence = word(input, at);
        let mut entry = self.heads[hash(sequence)];
        let mut nearest = at;
        let mut best = 0usize;
        let mut found = None;

        for _ in 0..SEARCH_DEPTH {
            let Some(candidate) = self.candidate(entry, at) else {
                break;
            };
            if candidate >= nearest {
                break;
            }
            nearest = candidate;
            entry = self.chain[candidate % CHAIN];

            if word(input, candidate) != sequence {
                continue;
            }

            let mut length = MIN_MATCH;
            while at + length < end && input[at + length] == input[candidate + length] {
                length += 1;
            }
            if length <= best {
                continue;
            }
            best = length;
            found = Some(candidate);
            if length >= LONG_ENOUGH {
                break;
            }
        }

        let candidate = found?;

        // A match usually starts before the position the hash found, since the
        // hash only looks at four bytes. Walking backwards costs a byte
        // comparison each and takes those bytes out of the literal run. It
        // happens once, on the candidate that won, rather than on every
        // candidate the search looked at, because it is the same walk over the
        // same bytes for most of them and it is not what decides between them.
        let mut start = at;
        let mut back = candidate;
        let mut length = best;
        while start > anchor && back > 0 && input[start - 1] == input[back - 1] {
            start -= 1;
            back -= 1;
            length += 1;
        }

        Some(Found {
            start,
            back,
            length,
        })
    }

    /// Records that a match starting at `at` is now something to be found.
    fn insert(&mut self, input: &[u8], at: usize) {
        let slot = hash(word(input, at));
        // A block is far smaller than four gigabytes, so the position fits, and
        // a block that somehow did not would only cost ratio.
        let stamped = self
            .base
            .wrapping_add(u32::try_from(at).unwrap_or(u32::MAX));
        self.chain[at % CHAIN] = self.heads[slot];
        self.heads[slot] = stamped;
    }

    /// Moves the table's window forward by one block.
    fn stamp(&mut self, len: usize) {
        let next = self
            .base
            .checked_add(u32::try_from(len).unwrap_or(u32::MAX))
            .and_then(|n| n.checked_add(1));
        if let Some(base) = next {
            self.base = base;
        } else {
            // Four gigabytes of input later the stamp has nowhere left to go,
            // so the tables are cleared and the count starts again. This
            // happens once per four gigabytes and costs a memset.
            self.heads.fill(0);
            self.chain.fill(0);
            self.base = 1;
        }
    }

    /// Turns a table entry into a position in this block, or nothing if the
    /// entry is from an earlier block or too far back to encode.
    fn candidate(&self, entry: u32, ip: usize) -> Option<usize> {
        let at = usize::try_from(entry.checked_sub(self.base)?).ok()?;
        if at >= ip || ip - at > MAX_DISTANCE {
            return None;
        }
        Some(at)
    }
}

/// Decompresses a block whose uncompressed length is already known.
///
/// The length is not read out of the block because it is not in the block. It
/// belongs to whatever wrote the block down, which knows it for free, and a
/// decoder that is told how much to expect can refuse a block that produces
/// anything else instead of growing until it runs out of memory.
///
/// # Errors
///
/// Returns [`Error::Truncated`] if the block ends in the middle of a sequence
/// and [`Error::BadBlock`] if it decodes to something other than `expect`
/// bytes or points a match outside what it has already produced.
pub fn decompress(input: &[u8], expect: usize, out: &mut Vec<u8>) -> Result<()> {
    let start = out.len();
    // The output is sized once and then written into as a slice, rather than
    // grown as it goes. A push that might reallocate cannot compile to a fixed
    // size move, and a fixed size move is the whole of why this is fast.
    //
    // The slack on the end is what lets a copy round its length up to a whole
    // number of pieces instead of checking on every piece whether it is allowed
    // to write the next one. It is cut off before this returns, and so is
    // everything else if the block turns out not to decode.
    out.resize(start + expect + WIDE, 0);
    let filled = fill(input, out.get_mut(start..).unwrap_or_default(), expect);
    out.truncate(if filled.is_ok() {
        start + expect
    } else {
        start
    });
    filled
}

/// Decompresses a block into a buffer that is used again for the next one.
///
/// [`decompress`] appends, so a caller decoding one block after another has to
/// empty the buffer first, and emptying it means the sizing below writes the
/// whole block's worth of zeroes again before the decode overwrites every one
/// of them. This replaces the buffer's contents instead, so the zeroing happens
/// once, when the buffer grows, rather than once per block. On a store read,
/// where a page of results is a handful of scattered blocks, that memset was a
/// measurable share of what fetching the page cost.
///
/// # Errors
///
/// The same errors [`decompress`] returns, for the same reasons.
pub fn decompress_into(input: &[u8], expect: usize, out: &mut Vec<u8>) -> Result<()> {
    let need = expect + WIDE;
    if out.len() < need {
        out.resize(need, 0);
    }
    let filled = fill(input, out.get_mut(..need).unwrap_or_default(), expect);
    out.truncate(if filled.is_ok() { expect } else { 0 });
    filled
}

/// The body of a decode, working in a buffer that is already the right size.
///
/// `dst` is `expect + WIDE` bytes long. Everything below relies on that, which
/// is why it is not public.
fn fill(input: &[u8], dst: &mut [u8], expect: usize) -> Result<()> {
    let mut ip = 0usize;
    let mut op = 0usize;

    loop {
        let token = *input.get(ip).ok_or(Error::Truncated {
            needed: 1,
            available: 0,
        })?;
        ip += 1;

        let mut run = usize::from(token >> 4);
        if run == 15 {
            run += length(input, &mut ip)?;
        }
        let to = ip.checked_add(run).ok_or(Error::Overflow)?;
        if to > input.len() {
            return Err(Error::Truncated {
                needed: run,
                available: input.len().saturating_sub(ip),
            });
        }
        if op + run > expect {
            return Err(Error::BadBlock);
        }
        copy_in(dst, op, input, ip, run).ok_or(Error::BadBlock)?;
        ip = to;
        op += run;

        // A block ends on a literal run with no match after it, which is how a
        // decoder knows it is done without a terminator to trust.
        if ip == input.len() {
            break;
        }

        let offset = ip
            .checked_add(2)
            .and_then(|to| input.get(ip..to))
            .and_then(<[u8]>::first_chunk::<2>)
            .map(|bytes| usize::from(u16::from_le_bytes(*bytes)))
            .ok_or(Error::Truncated {
                needed: 2,
                available: input.len().saturating_sub(ip),
            })?;
        ip += 2;
        if offset == 0 || offset > op {
            return Err(Error::BadBlock);
        }

        let mut copy = usize::from(token & 15);
        if copy == 15 {
            copy += length(input, &mut ip)?;
        }
        copy += MIN_MATCH;
        if op + copy > expect {
            return Err(Error::BadBlock);
        }

        let from = op - offset;
        let wide = copy.next_multiple_of(WIDE);
        if offset >= wide {
            // The match is far enough back that reading a whole piece past the
            // end of it still reads bytes that are already written, so it goes
            // out in pieces like a literal run does. This is most matches in
            // text.
            let (before, after) = dst.split_at_mut(op);
            copy_in(after, 0, before, from, copy).ok_or(Error::BadBlock)?;
        } else if offset >= copy {
            dst.copy_within(from..from + copy, op);
        } else {
            // An offset shorter than the length means the match reads bytes it
            // is in the middle of writing, which is how the format spells a
            // repeating pattern. The pattern is laid down once and then
            // doubled, so a run of a thousand bytes is ten copies rather than a
            // thousand.
            dst.copy_within(from..op, op);
            let mut have = offset;
            while have < copy {
                let take = have.min(copy - have);
                dst.copy_within(op..op + take, op + have);
                have += take;
            }
        }
        op += copy;
    }

    if op == expect {
        Ok(())
    } else {
        Err(Error::BadBlock)
    }
}

/// Copies `len` bytes into `dst` at `op`, a whole piece at a time where the
/// ends of both buffers leave room for it.
///
/// The rounding up is the point. A copy of a length the compiler knows is a
/// move instruction, and a copy of a length it does not know is a call into
/// memcpy, and at the four to fifteen bytes a sequence usually carries the call
/// costs more than the copy. Writing a few bytes too many into slack that is
/// about to be overwritten is cheaper than being exact.
#[inline]
fn copy_in(dst: &mut [u8], op: usize, src: &[u8], ip: usize, len: usize) -> Option<()> {
    let wide = len.next_multiple_of(WIDE);
    if let (Some(d), Some(s)) = (
        dst.get_mut(op..op.checked_add(wide)?),
        src.get(ip..ip.checked_add(wide)?),
    ) {
        for (d, s) in d.chunks_exact_mut(WIDE).zip(s.chunks_exact(WIDE)) {
            if let (Some(d), Some(s)) = (d.first_chunk_mut::<WIDE>(), s.first_chunk::<WIDE>()) {
                *d = *s;
            }
        }
        return Some(());
    }
    // The tail of a block, where there is not a whole piece left to read.
    dst.get_mut(op..op.checked_add(len)?)?
        .copy_from_slice(src.get(ip..ip.checked_add(len)?)?);
    Some(())
}

/// Reads the rest of a length that did not fit in its nibble.
///
/// Each byte of 255 says there is more. The chain cannot run away, because
/// every byte of it is a byte of input that has to be there.
fn length(input: &[u8], ip: &mut usize) -> Result<usize> {
    let mut total = 0usize;
    loop {
        let byte = *input.get(*ip).ok_or(Error::Truncated {
            needed: 1,
            available: 0,
        })?;
        *ip += 1;
        total = total
            .checked_add(usize::from(byte))
            .ok_or(Error::Overflow)?;
        if byte != 255 {
            return Ok(total);
        }
    }
}

/// Writes a sequence: the literals before a match, then the match.
fn sequence_out(out: &mut Vec<u8>, run: &[u8], offset: usize, length: usize) {
    let copy = length - MIN_MATCH;
    let token = (u8::try_from(run.len().min(15)).unwrap_or(15) << 4)
        | u8::try_from(copy.min(15)).unwrap_or(15);
    out.push(token);
    if run.len() >= 15 {
        rest(out, run.len() - 15);
    }
    out.extend_from_slice(run);
    out.push(u8::try_from(offset & 0xff).unwrap_or(0));
    out.push(u8::try_from(offset >> 8).unwrap_or(0));
    if copy >= 15 {
        rest(out, copy - 15);
    }
}

/// Writes the last sequence, which is literals and nothing else.
fn literals(out: &mut Vec<u8>, run: &[u8]) {
    out.push(u8::try_from(run.len().min(15)).unwrap_or(15) << 4);
    if run.len() >= 15 {
        rest(out, run.len() - 15);
    }
    out.extend_from_slice(run);
}

/// Writes what a length nibble could not hold.
fn rest(out: &mut Vec<u8>, mut left: usize) {
    while left >= 255 {
        out.push(255);
        left -= 255;
    }
    out.push(u8::try_from(left).unwrap_or(0));
}

/// The four bytes at a position, as one integer to compare and hash.
#[inline]
fn word(input: &[u8], at: usize) -> u32 {
    input
        .get(at..)
        .and_then(<[u8]>::first_chunk::<4>)
        .map_or(0, |bytes| u32::from_le_bytes(*bytes))
}

/// Which slot four bytes belong in.
#[inline]
const fn hash(sequence: u32) -> usize {
    // The shift leaves the value inside the table, and a u32 widens into a
    // usize on every target this builds for.
    (sequence.wrapping_mul(HASH_MUL) >> (32 - HASH_LOG)) as usize
}

#[cfg(test)]
#[expect(
    clippy::cast_possible_truncation,
    reason = "the test data is deliberately the low bits of a mixed counter"
)]
mod tests {
    use super::*;

    fn round_trip(input: &[u8]) -> Vec<u8> {
        let mut block = Vec::new();
        Compressor::new().compress(input, &mut block);
        let mut out = Vec::new();
        decompress(&block, input.len(), &mut out).expect("what was written decodes");
        assert_eq!(out, input);
        block
    }

    #[test]
    fn a_block_written_out_by_hand_decodes_the_way_the_format_says() {
        // Read this against the format and not against the encoder above. It is
        // the one test here that would fail if both sides of this module agreed
        // with each other and disagreed with everybody else.
        //
        // Token 0x64: six literals, then a match of four plus four. The six
        // literals are "hello ". The offset is six, so the match starts at the
        // beginning and copies eight bytes, "hello he". Token 0x50 ends the
        // block with five literals and no match after them.
        let block = [
            0x64, b'h', b'e', b'l', b'l', b'o', b' ', 0x06, 0x00, 0x50, b'l', b'l', b'o', b'!',
            b'!',
        ];
        let mut out = Vec::new();
        decompress(&block, 19, &mut out).expect("decodes");
        assert_eq!(out, b"hello hello hello!!");
    }

    #[test]
    fn nothing_round_trips() {
        round_trip(&[]);
    }

    #[test]
    fn every_length_up_to_a_block_round_trips() {
        // The end rules change what the compressor may emit at several lengths
        // near zero, and the boundaries are exactly where an off by one lives.
        let source: Vec<u8> = (0..300u32).map(|i| (i % 251) as u8).collect();
        for len in 0..source.len() {
            round_trip(&source[..len]);
        }
    }

    #[test]
    fn a_repeated_byte_compresses_to_almost_nothing() {
        let input = vec![b'x'; 64 << 10];
        let block = round_trip(&input);
        assert!(block.len() < input.len() / 200, "{} bytes", block.len());
    }

    #[test]
    fn overlapping_matches_decode_a_byte_at_a_time() {
        // An offset of one and a length of thousands is the format's way of
        // spelling a run, and it is the case a bulk copy gets wrong.
        let mut input = b"the pattern that repeats ".to_vec();
        while input.len() < 40_000 {
            let half = input.len();
            input.extend_from_within(..half);
        }
        round_trip(&input);
    }

    #[test]
    fn prose_compresses_by_more_than_half() {
        let mut input = Vec::new();
        while input.len() < 200_000 {
            input.extend_from_slice(
                b"Most retrieval systems end up with three storage layers that \
                  disagree with each other, and the seams between them are where \
                  the wrong results come from. ",
            );
        }
        let block = round_trip(&input);
        assert!(block.len() < input.len() / 2, "{} bytes", block.len());
    }

    #[test]
    fn looking_past_the_first_candidate_is_what_the_ratio_is_for() {
        // Text made of a small vocabulary in a scrambled order, which is the
        // shape that separates a match finder that takes the first candidate it
        // is handed from one that looks at sixteen and takes the longest. Every
        // word has been seen before, so there is always a match; which one is
        // taken is the whole question.
        //
        // Taking the first candidate compresses this to 0.404 of itself. The
        // bound below is 0.30, so it is not a bound this can drift past without
        // the chain or the step to the next byte having stopped working.
        let words = [
            "storage",
            "index",
            "the",
            "a",
            "segment",
            "posting",
            "block",
            "document",
            "compress",
            "retrieval",
            "of",
            "and",
            "list",
            "term",
        ];
        let mut state = 0x2545_f491_4f6c_dd1d_u64;
        let mut input = String::new();
        while input.len() < 64 << 10 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            input.push_str(words[(state % words.len() as u64) as usize]);
            input.push(if state.is_multiple_of(17) { '\n' } else { ' ' });
        }
        let block = round_trip(input.as_bytes());
        assert!(
            block.len() < input.len() * 3 / 10,
            "{} bytes of {}",
            block.len(),
            input.len()
        );
    }

    #[test]
    fn a_block_longer_than_the_chain_round_trips() {
        // Two positions 65536 apart share a slot in the chain, so a block this
        // long is one where the search follows entries that are not the ones
        // that were put there. The walk has to notice and stop rather than
        // circle, and whatever it settles on has to still be a match.
        let mut input = Vec::with_capacity(1 << 20);
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        while input.len() < 1 << 20 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            // Runs of a repeated byte, so the block is full of long matches and
            // the chains for them are long too.
            let byte = (state >> 40) as u8;
            let run = 1 + (state % 40) as usize;
            input.extend(core::iter::repeat_n(byte, run));
        }
        let block = round_trip(&input);
        assert!(block.len() < input.len() / 3, "{} bytes", block.len());
    }

    #[test]
    fn data_that_does_not_compress_still_round_trips() {
        // A counter mixed hard enough that no four bytes repeat, which is the
        // case where the skip trigger fires and the output grows.
        let input: Vec<u8> = (0..100_000u32)
            .map(|i| {
                let mut x = i.wrapping_mul(HASH_MUL);
                x ^= x >> 15;
                (x >> 3) as u8
            })
            .collect();
        let block = round_trip(&input);
        assert!(block.len() <= bound(input.len()));
    }

    #[test]
    fn a_truncated_block_is_an_error_not_a_panic() {
        let mut input = Vec::new();
        while input.len() < 5_000 {
            input.extend_from_slice(b"a line of text that will certainly repeat\n");
        }
        let mut block = Vec::new();
        Compressor::new().compress(&input, &mut block);
        for cut in 0..block.len() {
            let mut out = Vec::new();
            let _ = decompress(&block[..cut], input.len(), &mut out);
        }
    }

    #[test]
    fn a_block_that_decodes_to_the_wrong_length_is_refused() {
        let input = b"a short run of text that has a match in it, text".to_vec();
        let mut block = Vec::new();
        Compressor::new().compress(&input, &mut block);
        let mut out = Vec::new();
        assert!(decompress(&block, input.len() - 1, &mut out).is_err());
        out.clear();
        assert!(decompress(&block, input.len() + 1, &mut out).is_err());
    }

    #[test]
    fn a_match_pointing_before_the_start_is_refused() {
        // One sequence: no literals, then an offset of one into an output that
        // has nothing in it yet.
        let block = [0x00u8, 0x01, 0x00];
        let mut out = Vec::new();
        assert!(decompress(&block, 4, &mut out).is_err());
    }

    #[test]
    fn a_length_chain_that_never_ends_is_refused() {
        let mut block = vec![0xf0u8];
        block.extend(std::iter::repeat_n(255u8, 32));
        let mut out = Vec::new();
        assert!(decompress(&block, 1 << 20, &mut out).is_err());
    }

    #[test]
    fn blocks_are_independent_of_each_other() {
        // The table is kept between blocks for speed, so this is the test that
        // says keeping it cannot make a block refer to the one before it.
        let mut compressor = Compressor::new();
        let first = b"the first block, which shares a great deal of its text".to_vec();
        let second = b"the second block, which shares a great deal of its text".to_vec();
        let mut a = Vec::new();
        compressor.compress(&first, &mut a);
        let mut b = Vec::new();
        compressor.compress(&second, &mut b);

        let mut out = Vec::new();
        decompress(&b, second.len(), &mut out).expect("decodes on its own");
        assert_eq!(out, second);
    }

    #[test]
    fn appending_leaves_what_was_already_there() {
        let mut out = b"before".to_vec();
        let input = b"a block that follows something else in the same buffer".to_vec();
        let mut block = Vec::new();
        Compressor::new().compress(&input, &mut block);
        decompress(&block, input.len(), &mut out).expect("decodes");
        assert_eq!(&out[..6], b"before");
        assert_eq!(&out[6..], &input[..]);
    }

    #[test]
    fn a_reused_buffer_holds_the_block_it_was_last_given() {
        // The buffer is not emptied between blocks, so the case worth checking
        // is a long block followed by a short one, where whatever the long one
        // left behind is still sitting past the end of the short one.
        let long: Vec<u8> = (0..4_000u32).map(|i| (i % 251) as u8).collect();
        let short = b"short".to_vec();
        let mut buffer = Vec::new();

        for input in [&long, &short, &long, &short] {
            let mut block = Vec::new();
            Compressor::new().compress(input, &mut block);
            decompress_into(&block, input.len(), &mut buffer).expect("decodes");
            assert_eq!(&buffer[..], &input[..]);
        }
    }

    #[test]
    fn a_reused_buffer_is_emptied_when_a_block_does_not_decode() {
        let input = b"something that decoded once".to_vec();
        let mut block = Vec::new();
        Compressor::new().compress(&input, &mut block);
        let mut buffer = Vec::new();
        decompress_into(&block, input.len(), &mut buffer).expect("decodes");
        assert_eq!(&buffer[..], &input[..]);

        // The same block, told to expect a length it does not produce.
        decompress_into(&block, input.len() + 1, &mut buffer)
            .expect_err("a block that decodes to the wrong length is refused");
        assert!(
            buffer.is_empty(),
            "a refused block left {} bytes behind for the next caller to read",
            buffer.len()
        );
    }
}
