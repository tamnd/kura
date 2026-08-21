//! A small set that says no quickly and yes with a caveat.
//!
//! A store made of segments has one question it asks constantly and hates the
//! answer to: which segment holds this key. A key was written into exactly one
//! of them, so every other segment is a lookup that goes through a binary
//! search, touches a handful of cache lines a long way apart, and finds nothing.
//! The cost of an update is then the number of segments rather than the size of
//! any of them, which is the wrong thing to be proportional to.
//!
//! This is the usual answer. Each segment carries a bit set that every key it
//! holds was hashed into. A key that is absent from the set is absent from the
//! segment, certainly, and a key that is present in the set is probably in the
//! segment and has to be looked up properly. So the filter turns most of those
//! misses into a single cache line read.
//!
//! # What it costs and what it gives back
//!
//! The trade is bits per key against how often it says yes when it should have
//! said no. Ten bits a key, which is what [`BITS_PER_KEY`] is, gives around one
//! percent, and a million keys then cost 1.2 megabytes. That is small enough to
//! stay resident while the keys themselves do not, which is the property the
//! whole idea rests on: the filter is read on every lookup and the keys are read
//! on almost none.
//!
//! Fewer bits is a smaller filter and more wasted searches, and the curve is
//! steep at the bottom: at four bits a key better than a tenth of lookups pay
//! for a search that finds nothing. More bits flattens out, and past about
//! sixteen the filter stops being the thing that fits in memory.
//!
//! # Why the bits are in blocks
//!
//! A textbook filter scatters a key's bits across the whole set, so testing one
//! key touches as many cache lines as there are probes, and at ten bits a key
//! that is seven. On a filter that is bigger than cache, which is the only case
//! worth having one for, that is seven misses to answer a question that is
//! supposed to be cheap.
//!
//! So a key picks one block of sixty four bytes first, and all of its bits are
//! inside that block. One block is one cache line when the section lands on a
//! boundary and two when it does not, so a probe costs one miss or two rather
//! than seven.
//!
//! It is paid for in accuracy. The blocks do not fill evenly, some take more
//! than their share of keys, and a full block says yes to everything, so a
//! blocked filter is a little worse than a textbook one of the same size. The
//! measured rate is what matters rather than the formula, which is why
//! [`Reader::len`] keeps the number of keys that went in: it is what lets a
//! reader work out afterwards how loaded the filter it is holding actually is.
//!
//! # No false negatives, ever
//!
//! Everything above is about how often the filter is wrong in the direction that
//! costs time. It is never wrong in the direction that loses data. A key that
//! was inserted always tests present, and a reader that decides a segment does
//! not hold a key because the filter said no is right every time. That is what
//! makes it safe to skip a segment on the strength of it, and it is why a
//! decoder that cannot make sense of a filter has to answer yes rather than no.

use crate::codec::{get_u32, put_u32, split_at};
use crate::error::{Error, Result};
use crate::xxh3;

/// How many bits each key gets, unless a caller asks for another number.
///
/// Ten is the usual place to sit. It is where the curve stops falling steeply
/// and before it flattens, and it is what the measurement in
/// `examples/keys.rs` is against.
pub const BITS_PER_KEY: u8 = 10;

/// How many bytes a key's bits are confined to.
const BLOCK: usize = 64;

/// The same number as bits, which is what a position inside a block is taken
/// modulo.
const BLOCK_BITS: u32 = 512;

/// The fixed part in front of the bits.
const HEADER: usize = 12;

/// The most probes a key gets, whatever the bits per key works out to.
///
/// Past eight the accuracy is barely moving and every probe is another
/// dependent load, and a filter asked for a hundred bits a key should not turn
/// into a filter that reads seventy bytes to answer.
const MAX_PROBES: u8 = 8;

/// How many bits to set per key, for a filter of this many bits per key.
///
/// The number that minimises the false positive rate is the bits per key times
/// the natural log of two, and this is that in integers, rounded, and held to at
/// least one so that a filter always tests something.
// The arithmetic is in thirty two bits, which is wide enough for the largest
// bits per key there is, and the result is held under eight before it comes
// back, so the cast cannot truncate. try_from is not usable here because this is
// const.
#[allow(clippy::cast_possible_truncation)]
const fn probes_for(bits: u8) -> u8 {
    let probes = (bits as u32 * 693 + 500) / 1000;
    if probes < 1 {
        1
    } else if probes > MAX_PROBES as u32 {
        MAX_PROBES
    } else {
        probes as u8
    }
}

/// Which block a key lands in, and the two numbers its bits are walked with.
///
/// The block comes from the top half of the hash and the bits from the bottom
/// half, so a key that shares a block with another does not also share its bit
/// pattern. The step is odd because an even one would walk half as many
/// positions as it should.
#[expect(
    clippy::cast_possible_truncation,
    reason = "both halves of the hash are taken deliberately, and the block is a \
              multiply and shift of two thirty two bit numbers so it is smaller \
              than the block count"
)]
fn probe(hash: u64, blocks: u32) -> (usize, u32, u32) {
    let high = (hash >> 32) as u32;
    let block = ((u64::from(high) * u64::from(blocks)) >> 32) as u32;
    let first = hash as u32;
    (block as usize, first, first.rotate_left(15) | 1)
}

/// Builds a filter over a set of keys.
///
/// The size is fixed when the writer is made, from how many keys are coming,
/// because a bit set cannot be grown without rehashing everything in it. A
/// caller that does not know the count yet should count first: a filter built
/// for far fewer keys than it received is a filter that says yes to everything,
/// which is not wrong but is worth nothing.
#[derive(Debug, Clone)]
pub struct Writer {
    /// How many blocks of [`BLOCK`] bytes the bits are.
    blocks: u32,
    /// How many bits each key sets.
    probes: u8,
    /// How many keys went in, which is not the same as how many distinct ones.
    keys: u32,
    /// The bits themselves.
    bits: Vec<u8>,
}

impl Writer {
    /// A filter sized for `keys` keys at [`BITS_PER_KEY`] bits each.
    #[must_use]
    pub fn new(keys: usize) -> Self {
        Self::with_bits(keys, BITS_PER_KEY)
    }

    /// A filter sized for `keys` keys at `bits` bits each.
    ///
    /// A filter for no keys holds no blocks and answers no to everything, which
    /// is the honest answer and costs twelve bytes.
    #[must_use]
    pub fn with_bits(keys: usize, bits: u8) -> Self {
        let wanted = (keys as u64).saturating_mul(u64::from(bits.max(1)));
        let blocks = wanted.div_ceil(u64::from(BLOCK_BITS));
        let blocks = u32::try_from(blocks).unwrap_or(u32::MAX);
        Self {
            blocks,
            probes: probes_for(bits),
            keys: 0,
            bits: vec![0; blocks as usize * BLOCK],
        }
    }

    /// Puts a key in.
    ///
    /// Putting the same key in twice is not an error and sets the same bits
    /// again, so a caller does not have to know whether its keys are distinct.
    pub fn insert(&mut self, key: &[u8]) {
        if self.blocks == 0 {
            return;
        }
        let (block, first, step) = probe(xxh3::hash64(key), self.blocks);
        let base = block * BLOCK;
        let mut at = first;
        for _ in 0..self.probes {
            let bit = at & (BLOCK_BITS - 1);
            self.bits[base + (bit >> 3) as usize] |= 1u8 << (bit & 7);
            at = at.wrapping_add(step);
        }
        self.keys = self.keys.saturating_add(1);
    }

    /// How many keys went in.
    #[must_use]
    pub const fn len(&self) -> u32 {
        self.keys
    }

    /// Reports whether nothing has been put in.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.keys == 0
    }

    /// What the encoded filter will be, in bytes.
    #[must_use]
    pub const fn size(&self) -> usize {
        HEADER + self.blocks as usize * BLOCK
    }

    /// Writes the filter onto the end of `out`.
    pub fn write_to(&self, out: &mut Vec<u8>) {
        out.reserve(self.size());
        put_u32(out, self.blocks);
        put_u32(out, self.keys);
        out.push(self.probes);
        // Three bytes of nothing, so that the bits start on a four byte
        // boundary and a later field has somewhere to go that does not move
        // them.
        out.extend_from_slice(&[0, 0, 0]);
        out.extend_from_slice(&self.bits);
    }

    /// The filter on its own.
    #[must_use]
    pub fn finish(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.size());
        self.write_to(&mut out);
        out
    }
}

/// Asks a written filter about a key.
#[derive(Debug, Clone, Copy)]
pub struct Reader<'a> {
    /// How many blocks the bits are, which is what a key is placed by.
    blocks: u32,
    /// How many bits a key sets, and so how many have to be set for a yes.
    probes: u8,
    /// How many keys went in, kept so that a reader can say how loaded it is.
    keys: u32,
    /// The bits, exactly `blocks` times [`BLOCK`] bytes of them.
    bits: &'a [u8],
}

impl<'a> Reader<'a> {
    /// Reads a filter out of the front of `input`.
    ///
    /// Bytes after the filter are left alone, the same as everywhere else in
    /// this crate, so a section with something else after it reads fine.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Truncated`] if the input is shorter than the header, or
    /// shorter than the number of blocks the header claims.
    pub fn new(input: &'a [u8]) -> Result<Self> {
        let (head, rest) = split_at(input, HEADER)?;
        let (blocks, tail) = get_u32(head)?;
        let (keys, tail) = get_u32(tail)?;
        let probes = *tail.first().ok_or(Error::Truncated {
            needed: HEADER,
            available: input.len(),
        })?;
        let wanted = blocks as usize * BLOCK;
        let (bits, _) = split_at(rest, wanted)?;
        Ok(Self {
            blocks,
            probes,
            keys,
            bits,
        })
    }

    /// Reports whether the key might be in the set.
    ///
    /// A no is certain. A yes means the bits a key would have set are all set,
    /// which every key that was put in satisfies, and some that were not.
    #[must_use]
    pub fn maybe_holds(&self, key: &[u8]) -> bool {
        if self.blocks == 0 {
            return false;
        }
        let (block, first, step) = probe(xxh3::hash64(key), self.blocks);
        let base = block * BLOCK;
        let mut at = first;
        for _ in 0..self.probes {
            let bit = at & (BLOCK_BITS - 1);
            // Indexing rather than getting, because the block came from a
            // multiply and shift against the block count and the bit is masked
            // into the block, so both are inside the slice the reader checked
            // the length of.
            if self.bits[base + (bit >> 3) as usize] & (1u8 << (bit & 7)) == 0 {
                return false;
            }
            at = at.wrapping_add(step);
        }
        true
    }

    /// How many keys went in.
    ///
    /// Not how many the filter can be asked about, and not how many distinct
    /// keys it holds. It is the number a rate is worked out against.
    #[must_use]
    pub const fn len(&self) -> u32 {
        self.keys
    }

    /// Reports whether nothing went in.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.keys == 0
    }

    /// How many bytes the filter is, header included.
    #[must_use]
    pub const fn size(&self) -> usize {
        HEADER + self.bits.len()
    }

    /// How many bits each key got, which is the number the rate follows from.
    ///
    /// Zero for a filter with no keys in it, rather than a division by nothing.
    #[must_use]
    #[expect(
        clippy::cast_precision_loss,
        reason = "a ratio of two counts, printed rather than compared"
    )]
    pub fn bits_per_key(&self) -> f32 {
        if self.keys == 0 {
            return 0.0;
        }
        (self.bits.len() * 8) as f32 / self.keys as f32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Keys that look like the ones a caller would use, which is to say mostly
    /// alike and differing at the end.
    fn keys(count: usize) -> Vec<Vec<u8>> {
        (0..count)
            .map(|n| format!("https://example.com/wiki/page/{n}").into_bytes())
            .collect()
    }

    fn built(keys: &[Vec<u8>]) -> Vec<u8> {
        let mut writer = Writer::new(keys.len());
        for key in keys {
            writer.insert(key);
        }
        writer.finish()
    }

    #[test]
    fn every_key_that_went_in_comes_back_as_present() {
        // The only property that has to hold exactly. Everything else about a
        // filter is a matter of degree.
        let keys = keys(10_000);
        let bytes = built(&keys);
        let reader = Reader::new(&bytes).expect("a filter");
        for key in &keys {
            assert!(
                reader.maybe_holds(key),
                "{:?}",
                String::from_utf8_lossy(key)
            );
        }
        assert_eq!(reader.len(), 10_000);
    }

    #[test]
    fn a_key_that_did_not_go_in_is_usually_absent() {
        let keys = keys(10_000);
        let bytes = built(&keys);
        let reader = Reader::new(&bytes).expect("a filter");
        let missing: Vec<_> = (10_000..110_000)
            .map(|n| format!("https://example.com/wiki/page/{n}").into_bytes())
            .collect();
        let wrong = missing.iter().filter(|key| reader.maybe_holds(key)).count();
        // The formula says about one in a hundred and the blocking costs a
        // little on top of that. Three percent is loose enough that this is a
        // test of the filter working rather than of a particular hash, and
        // tight enough to fail if the bits stop being set where they are looked
        // for.
        assert!(wrong < 3_000, "{wrong} of 100000 were wrong");
    }

    #[test]
    fn more_bits_a_key_is_fewer_wrong_answers() {
        let keys = keys(20_000);
        let missing: Vec<_> = (20_000..40_000)
            .map(|n| format!("https://example.com/wiki/page/{n}").into_bytes())
            .collect();
        let mut rates = Vec::new();
        for bits in [4u8, 8, 16] {
            let mut writer = Writer::with_bits(keys.len(), bits);
            for key in &keys {
                writer.insert(key);
            }
            let bytes = writer.finish();
            let reader = Reader::new(&bytes).expect("a filter");
            rates.push(missing.iter().filter(|key| reader.maybe_holds(key)).count());
        }
        assert!(rates[0] > rates[1], "{rates:?}");
        assert!(rates[1] > rates[2], "{rates:?}");
    }

    #[test]
    fn a_filter_for_no_keys_answers_no_to_everything() {
        let writer = Writer::new(0);
        assert!(writer.is_empty());
        let bytes = writer.finish();
        assert_eq!(bytes.len(), HEADER);
        let reader = Reader::new(&bytes).expect("a filter");
        assert!(reader.is_empty());
        assert!(!reader.maybe_holds(b"anything"));
        assert!((reader.bits_per_key() - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn a_filter_sized_for_one_key_still_holds_it() {
        let mut writer = Writer::new(1);
        writer.insert(b"only");
        let bytes = writer.finish();
        let reader = Reader::new(&bytes).expect("a filter");
        assert!(reader.maybe_holds(b"only"));
        assert_eq!(reader.size(), HEADER + BLOCK);
    }

    #[test]
    fn a_filter_given_more_keys_than_it_was_sized_for_still_never_misses_one() {
        // A yes to everything is the failure mode, and it is a failure of value
        // rather than of correctness. What must not happen is a no.
        let keys = keys(4_000);
        let mut writer = Writer::with_bits(100, BITS_PER_KEY);
        for key in &keys {
            writer.insert(key);
        }
        let bytes = writer.finish();
        let reader = Reader::new(&bytes).expect("a filter");
        for key in &keys {
            assert!(reader.maybe_holds(key));
        }
    }

    #[test]
    fn the_bits_a_key_gets_are_all_in_one_block() {
        // What the whole shape is for. Two keys apart in the set do not share a
        // cache line, and one key does not span two of them.
        let mut writer = Writer::new(10_000);
        writer.insert(b"https://example.com/wiki/page/one");
        let bytes = writer.finish();
        let touched: Vec<_> = bytes[HEADER..]
            .chunks(BLOCK)
            .enumerate()
            .filter(|(_, block)| block.iter().any(|&byte| byte != 0))
            .map(|(at, _)| at)
            .collect();
        assert_eq!(touched.len(), 1, "{touched:?}");
    }

    #[test]
    fn a_filter_reads_back_as_the_filter_that_was_written() {
        let keys = keys(500);
        let bytes = built(&keys);
        let reader = Reader::new(&bytes).expect("a filter");
        assert_eq!(reader.len(), 500);
        assert_eq!(reader.size(), bytes.len());
        assert!(reader.bits_per_key() >= f32::from(BITS_PER_KEY));
        // And with something after it, which is what a section that carries
        // more than the filter looks like.
        let mut more = bytes.clone();
        more.extend_from_slice(b"and then some");
        let reader = Reader::new(&more).expect("a filter");
        assert_eq!(reader.size(), bytes.len());
        for key in &keys {
            assert!(reader.maybe_holds(key));
        }
    }

    #[test]
    fn a_filter_that_stops_short_is_refused_rather_than_read() {
        let bytes = built(&keys(500));
        for cut in [0, 1, HEADER - 1, HEADER, HEADER + 1, bytes.len() - 1] {
            assert!(
                matches!(Reader::new(&bytes[..cut]), Err(Error::Truncated { .. })),
                "cut at {cut}"
            );
        }
    }

    #[test]
    fn the_probe_count_follows_the_bits_a_key_gets() {
        assert_eq!(probes_for(0), 1);
        assert_eq!(probes_for(1), 1);
        assert_eq!(probes_for(10), 7);
        assert_eq!(probes_for(16), MAX_PROBES);
        assert_eq!(probes_for(255), MAX_PROBES);
    }
}
