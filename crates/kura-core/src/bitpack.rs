//! Fixed width packing for the blocks a posting list is built from.
//!
//! A posting list is a run of ascending document ids, and the usual way to make
//! it small is to store the gaps between them as varints. That works and it is
//! what this engine did first, but it puts a data dependent branch in front of
//! every single value: the decoder cannot know where the next id starts until it
//! has finished the one before it. On a list of a million ids that is a million
//! unpredictable branches, and it is the reason a varint decoder tops out around
//! two nanoseconds an id no matter how fast the machine is.
//!
//! Packing at a fixed width removes the dependency. Every value in a block gets
//! the same number of bits, the width is chosen once for the block as the
//! smallest that fits its largest value, and the position of value `n` is
//! arithmetic rather than something to be discovered. That is what lets the
//! decoder run four values at a time.
//!
//! # Why four lanes
//!
//! The gaps are what compress, so they have to be stored rather than the ids,
//! and turning gaps back into ids is a running sum: value `n` cannot be finished
//! until value `n - 1` is. A running sum over 128 values is 128 dependent adds,
//! which costs more than the unpacking it was meant to help.
//!
//! So the block is four independent lanes. Value `n` is stored as the difference
//! from value `n - 4` rather than from value `n - 1`, and the four lanes are
//! summed side by side. The bit layout matches: the packed words are grouped in
//! fours, so the four values a step needs sit in four words at the same offset
//! and come out with one shift and one mask each.
//!
//! It is not free. A gap of four positions is about four times a gap of one, so
//! the values need about two more bits than they would with adjacent gaps. On a
//! list with an average gap of three that is four bits an id against the eight a
//! varint spends, so the format is smaller as well as faster, and on a list dense
//! enough for one bit gaps it is the one case where adjacent gaps would have won.
//!
//! # Width and canonical form
//!
//! [`pack`] always chooses the smallest width the block's values fit in, so one
//! block of values has one encoding. [`unpack`] does not re-derive the width and
//! reject a wider one, because that would mean decoding a block to find out
//! whether it should have been decoded. The width is framing rather than data,
//! and the checksum over the segment is what pins the bytes.

use crate::codec::split_at;
use crate::error::{Error, Result};

/// How many values one packed block holds.
///
/// It is a multiple of the lane count and small enough that a decoded block and
/// the words it came from both sit in L1, which is what makes decoding one block
/// to answer a membership test cheap enough to be worth doing.
pub const BLOCK: usize = 128;

/// How many values are decoded side by side.
///
/// Four `u32` lanes are sixteen bytes, which is one register on every machine
/// this engine runs on. The loops below are written over arrays of this length
/// rather than over intrinsics, so the same source compiles to the vector
/// instruction on each target and to something correct on a target that has
/// none.
const LANES: usize = 4;

/// How many steps a block takes at four values a step.
const STEPS: usize = BLOCK / LANES;

/// The widest a value can be packed at.
const MAX_WIDTH: u32 = 32;

/// How many bytes [`pack`] writes for a block at `width`.
///
/// Always a whole number of four byte words, which is what lets a block be read
/// without regard to where the one before it ended.
#[must_use]
pub const fn packed_len(width: u32) -> usize {
    width as usize * BLOCK / 8
}

/// Packs one block of ascending values and returns the width it used.
///
/// `base` is the value the block counts from, which is the last value of the
/// block before it, or zero for the first block. Passing the wrong base
/// produces a block that decodes to different values rather than one that fails
/// to decode, so it is the caller's job to keep it.
///
/// # Panics
///
/// Does not panic. `values` must be ascending and above `base`; a caller that
/// breaks that gets meaningless output rather than a crash, because the check
/// belongs at the edge where the ids come in rather than on the write path of
/// every block.
pub fn pack(values: &[u32; BLOCK], base: u32, out: &mut Vec<u8>) -> u32 {
    lay(&deltas(values, base), out)
}

/// Packs one block of values as they are, without differencing, and returns the
/// width it used.
///
/// This is for the streams that run alongside a posting list and are not
/// ascending: term frequencies, positions counts, anything where value `n` says
/// nothing about value `n + 1`. Differencing those would make them bigger rather
/// than smaller, and the running sum on the way back out would cost more than it
/// saved.
///
/// Frequencies pack particularly well without any of that, because almost every
/// term appears once in almost every document it appears in at all, so most
/// blocks come out at one or two bits a value.
pub fn pack_flat(values: &[u32; BLOCK], out: &mut Vec<u8>) -> u32 {
    lay(values, out)
}

/// Writes one block at the smallest width its values fit in.
fn lay(values: &[u32; BLOCK], out: &mut Vec<u8>) -> u32 {
    // A block whose values are all zero is a real block for the flat form and
    // cannot happen for the delta form, and spending a bit on it either way is
    // what lets every block in the format be decoded by the same path, with no
    // special case for a width that means nothing.
    let width = width_of(values).max(1);

    // The widest a block can be is thirty two bits a value, which is 128 words.
    // Sizing for that keeps the write path free of an allocation as well.
    let mut words = [0u32; MAX_WIDTH as usize * LANES];
    for step in 0..STEPS {
        let bit = step * width as usize;
        let word = bit / 32;
        let shift = u32::try_from(bit % 32).unwrap_or(0);
        let spills = shift + width > 32;
        for lane in 0..LANES {
            let value = values[step * LANES + lane];
            words[word * LANES + lane] |= value << shift;
            if spills {
                words[(word + 1) * LANES + lane] |= value >> (32 - shift);
            }
        }
    }

    // Laid out into bytes first and appended once. Extending the output four
    // bytes at a time instead costs a capacity check per word, and a block is
    // up to a hundred and twenty eight of them.
    let count = width as usize * LANES;
    let mut bytes = [0u8; MAX_WIDTH as usize * LANES * 4];
    for (word, slot) in words[..count].iter().zip(bytes.chunks_exact_mut(4)) {
        slot.copy_from_slice(&word.to_le_bytes());
    }
    out.extend_from_slice(&bytes[..count * 4]);
    width
}

/// Unpacks one block, turning it back into ascending values.
///
/// `base` has to be the one [`pack`] was given. The values are written into
/// `out` rather than returned, so that a caller decoding a list of a thousand
/// blocks reuses one buffer instead of allocating a thousand times.
///
/// # Errors
///
/// Returns [`Error::Overflow`] if `width` is above thirty two, and
/// [`Error::Truncated`] if `input` holds fewer bytes than that width needs.
pub fn unpack(input: &[u8], width: u32, base: u32, out: &mut [u32; BLOCK]) -> Result<usize> {
    let read = unpack_flat(input, width, out)?;
    // The lanes are independent all the way through the read above, and this is
    // the only place they meet: four running sums side by side, each one a chain
    // of thirty two adds rather than the one chain of a hundred and twenty eight
    // that storing adjacent gaps would have cost.
    let mut running = [base; LANES];
    for step in 0..STEPS {
        for lane in 0..LANES {
            running[lane] = running[lane].wrapping_add(out[step * LANES + lane]);
            out[step * LANES + lane] = running[lane];
        }
    }
    Ok(read)
}

/// Unpacks one block written by [`pack_flat`], leaving the values as they are.
///
/// # Errors
///
/// Returns [`Error::Overflow`] if `width` is above thirty two, and
/// [`Error::Truncated`] if `input` holds fewer bytes than that width needs.
pub fn unpack_flat(input: &[u8], width: u32, out: &mut [u32; BLOCK]) -> Result<usize> {
    if width > MAX_WIDTH {
        return Err(Error::Overflow);
    }
    if width == 0 {
        // Nothing here writes a zero width, because the packer spends a bit on
        // an all zero block rather than encode it as nothing. A reader that met
        // one and guessed would be the bug rather than the file.
        return Err(Error::Overflow);
    }

    let count = width as usize * LANES;
    let (head, _) = split_at(input, count * 4)?;
    // Read straight out of the caller's bytes rather than through a buffer.
    // A buffer would have to be sized for the widest block a list can hold, so
    // every narrow block would pay to clear five hundred bytes it never used,
    // and clearing them costs more than the unpacking does.
    let (words, _) = head.as_chunks::<4>();

    let mask = if width == MAX_WIDTH {
        u32::MAX
    } else {
        (1u32 << width) - 1
    };

    for step in 0..STEPS {
        let bit = step * width as usize;
        let word = bit / 32;
        let shift = u32::try_from(bit % 32).unwrap_or(0);
        let spills = shift + width > 32;

        for lane in 0..LANES {
            let mut value = u32::from_le_bytes(words[word * LANES + lane]) >> shift;
            if spills {
                value |= u32::from_le_bytes(words[(word + 1) * LANES + lane]) << (32 - shift);
            }
            out[step * LANES + lane] = value & mask;
        }
    }

    Ok(count * 4)
}

/// The difference of each value from the one four positions before it.
///
/// The first four count from `base`, which is what joins a block to the one in
/// front of it without storing an absolute value in every block.
fn deltas(values: &[u32; BLOCK], base: u32) -> [u32; BLOCK] {
    let mut out = [0u32; BLOCK];
    for lane in 0..LANES {
        out[lane] = values[lane].wrapping_sub(base);
    }
    for k in LANES..BLOCK {
        out[k] = values[k].wrapping_sub(values[k - LANES]);
    }
    out
}

/// The smallest number of bits every value fits in.
fn width_of(values: &[u32; BLOCK]) -> u32 {
    let mut all = 0u32;
    for value in values {
        all |= *value;
    }
    MAX_WIDTH - all.leading_zeros()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A block whose gaps are `gap` apart, starting just above `base`.
    fn ascending(base: u32, gap: u32) -> [u32; BLOCK] {
        let mut values = [0u32; BLOCK];
        let mut current = base;
        for slot in &mut values {
            current += gap;
            *slot = current;
        }
        values
    }

    fn round_trip(values: &[u32; BLOCK], base: u32) {
        let mut bytes = Vec::new();
        let width = pack(values, base, &mut bytes);
        assert_eq!(bytes.len(), packed_len(width), "width {width}");

        let mut out = [0u32; BLOCK];
        let read = unpack(&bytes, width, base, &mut out).expect("unpack");
        assert_eq!(read, bytes.len());
        assert_eq!(&out, values, "width {width}");
    }

    #[test]
    fn round_trips_every_gap_size() {
        // One gap on each side of every power of two, which is where a width is
        // most likely to be chosen one bit short. The largest gap is bounded so
        // that a hundred and twenty eight of them still fit in the identifier
        // space, because a block that wrapped would be testing the arithmetic
        // rather than the widths.
        for shift in 0..24 {
            let gap = 1u32 << shift;
            for gap in [gap, gap + 1] {
                round_trip(&ascending(0, gap), 0);
                round_trip(&ascending(1_000_000, gap), 1_000_000);
            }
        }
    }

    #[test]
    fn round_trips_at_every_width() {
        // Drive the width directly by choosing the gaps, so that all thirty two
        // are exercised rather than whichever ones an ascending list happens to
        // produce. The widest gap wraps the identifier space, which is the one
        // case the packing has to carry without the values meaning anything.
        for width in 1..=MAX_WIDTH {
            let top = if width == MAX_WIDTH {
                u32::MAX
            } else {
                (1u32 << width) - 1
            };
            let mut values = [0u32; BLOCK];
            for slot in &mut values[..LANES] {
                *slot = 1;
            }
            for k in LANES..BLOCK {
                let gap = if k == LANES { top } else { 1 };
                values[k] = values[k - LANES].wrapping_add(gap);
            }

            let mut bytes = Vec::new();
            let used = pack(&values, 1, &mut bytes);
            assert_eq!(used, width, "expected width {width}");

            let mut out = [0u32; BLOCK];
            unpack(&bytes, used, 1, &mut out).expect("unpack");
            assert_eq!(out, values, "width {width}");
        }
    }

    #[test]
    fn the_widest_block_is_the_size_it_says() {
        assert_eq!(packed_len(32), BLOCK * 4);
        assert_eq!(packed_len(1), BLOCK / 8);
    }

    #[test]
    fn a_dense_block_costs_a_few_bits_an_id() {
        let mut bytes = Vec::new();
        let width = pack(&ascending(0, 1), 0, &mut bytes);
        // Counting from four positions back turns a gap of one into a gap of
        // four, which is three bits. Two bits more than adjacent gaps would
        // need, and the whole reason the decode runs four at a time.
        assert_eq!(width, 3);
        assert_eq!(bytes.len(), 48);
    }

    #[test]
    fn a_truncated_block_is_an_error_not_a_panic() {
        let mut bytes = Vec::new();
        let width = pack(&ascending(0, 7), 0, &mut bytes);
        let mut out = [0u32; BLOCK];
        for len in 0..bytes.len() {
            let err = unpack(&bytes[..len], width, 0, &mut out).expect_err("short input");
            assert!(matches!(err, Error::Truncated { .. }), "len {len}: {err:?}");
        }
    }

    #[test]
    fn a_width_this_format_cannot_hold_is_refused() {
        let mut out = [0u32; BLOCK];
        assert_eq!(unpack(&[0u8; 1024], 33, 0, &mut out), Err(Error::Overflow));
        assert_eq!(unpack(&[0u8; 1024], 0, 0, &mut out), Err(Error::Overflow));
    }

    #[test]
    fn trailing_bytes_are_left_for_the_next_block() {
        let values = ascending(0, 5);
        let mut bytes = Vec::new();
        let width = pack(&values, 0, &mut bytes);
        let packed = bytes.len();
        bytes.extend_from_slice(b"the next block");

        let mut out = [0u32; BLOCK];
        assert_eq!(unpack(&bytes, width, 0, &mut out).expect("unpack"), packed);
        assert_eq!(out, values);
    }

    #[test]
    fn the_base_is_what_joins_two_blocks() {
        let first = ascending(0, 9);
        let second = ascending(first[BLOCK - 1], 9);

        let mut bytes = Vec::new();
        let one = pack(&first, 0, &mut bytes);
        let two = pack(&second, first[BLOCK - 1], &mut bytes);

        let mut out = [0u32; BLOCK];
        let read = unpack(&bytes, one, 0, &mut out).expect("first");
        assert_eq!(out, first);
        unpack(&bytes[read..], two, out[BLOCK - 1], &mut out).expect("second");
        assert_eq!(out, second);
    }
}
