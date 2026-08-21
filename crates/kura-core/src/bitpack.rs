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
//! arithmetic rather than something to be discovered.
//!
//! # The layout
//!
//! The values are not laid end to end. The block is cut into lanes, lane `l`
//! holding values `l`, `l + LANES`, `l + 2 * LANES` and so on, and the packed
//! words are interleaved so that the words the lanes are reading at any one
//! moment sit next to each other. That is the interleaved layout FastLanes
//! describes, and it is what turns a step of the decoder into one shift and one
//! mask over a register rather than four of each in a row.
//!
//! It comes out even. A block is 128 values over four lanes, so a lane holds 32
//! of them, and 32 values at any width from one bit to thirty two is a whole
//! number of 32 bit words. Nothing hangs over the end of a lane, so the width is
//! the only thing a reader has to be told to find any value in the block.
//!
//! # Why four lanes and not thirty two
//!
//! FastLanes packs against a virtual 1024 bit register, which for 32 bit values
//! is thirty two lanes, so that one piece of scalar source compiles to whatever
//! vector unit the machine turns out to have. This packs against 128 bits
//! instead, and the reason is that here a lane count is not only a register
//! width.
//!
//! The gaps are what compress, so the gaps are what is stored, and turning them
//! back into ids is a running sum. A sum over 128 values in a row is 128
//! dependent adds, which costs more than the unpacking it was meant to help, so
//! there is one sum per lane and value `n` is the difference from value
//! `n - LANES` rather than from value `n - 1`. The lane count is the distance
//! the differencing counts back over, and a longer distance is a larger gap.
//!
//! Four costs about two bits an id against adjacent gaps. Thirty two costs about
//! five. On a run of a million ids with random gaps the packed form is 1.21
//! bytes an id at four lanes and 2.01 at thirty two, which is a posting section
//! two thirds larger. `cargo run --release --example bench` prints both as the
//! `lane count` fact. What the wider register would buy is a decoder that is
//! already faster than the memory it reads from, so there is nothing there to
//! spend two thirds of the section on.
//!
//! Thirty two lanes would also mean 1024 values to a block, because a lane has
//! to hold 32 values for the words to come out whole. The block is the unit the
//! skip table points at and the unit the block-max ceilings prune at, so a block
//! eight times larger is pruning eight times coarser and a membership test
//! decoding eight times as much. That reason is enough on its own.
//!
//! The first of those does not apply to the frequencies, which are packed flat
//! with no differencing at all, so a lane count there is only a register width.
//! Widening that stream on its own is tracked in tamnd/kura#65.
//!
//! # One kernel per width
//!
//! Every shift, every mask and every question about whether a value straddles a
//! word boundary is settled by the width. A decoder handed the width in a
//! variable settles them again for each of the thirty two steps of every block
//! it reads, and cannot hoist any of it, because the next block may be a
//! different width.
//!
//! So there is a kernel per width and a match to choose it, which is the shape
//! the FastLanes reference implementation generates. Inside one of them the
//! width is a constant, the shifts and the masks fold away, the straddling
//! branch is decided at compile time rather than taken, and the loop over the
//! steps unrolls. What it costs is thirty two copies of each of the three small
//! kernels, which is a few kilobytes of text against a decode that is twice as
//! fast over a million ids of mixed widths, and a pack that is a third faster.
//! The two numbers differ because a pack also has to difference the block and
//! find its width, and neither of those got any faster here.
//!
//! # Width and canonical form
//!
//! [`pack`] always chooses the smallest width the block's values fit in, so one
//! block of values has one encoding. [`unpack`] does not re-derive the width and
//! reject a wider one, because that would mean decoding a block to find out
//! whether it should have been decoded. The width is framing rather than data,
//! and the digest over the section the block sits in is what pins the bytes.

// FastLanes is the name of a project and a paper, and the lint that wants a
// word shaped like that in backticks cannot tell one from an identifier.
#![expect(
    clippy::doc_markdown,
    reason = "FastLanes is a name, not an item in this crate"
)]

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

/// Expands to a match that hands `$width` to the kernel written for it.
///
/// Thirty two arms, written once here rather than three times in the functions
/// below. Thirty two is the last arm rather than one of its own so that the
/// match is total without a panic and without an arm that quietly does nothing:
/// a width above thirty two cannot reach here, and if one ever did it would be
/// packed as thirty two, which is wrong but not unsound.
macro_rules! by_width {
    ($width:expr, $kernel:ident($($arg:expr),*)) => {
        match $width {
            1 => $kernel::<1>($($arg),*),
            2 => $kernel::<2>($($arg),*),
            3 => $kernel::<3>($($arg),*),
            4 => $kernel::<4>($($arg),*),
            5 => $kernel::<5>($($arg),*),
            6 => $kernel::<6>($($arg),*),
            7 => $kernel::<7>($($arg),*),
            8 => $kernel::<8>($($arg),*),
            9 => $kernel::<9>($($arg),*),
            10 => $kernel::<10>($($arg),*),
            11 => $kernel::<11>($($arg),*),
            12 => $kernel::<12>($($arg),*),
            13 => $kernel::<13>($($arg),*),
            14 => $kernel::<14>($($arg),*),
            15 => $kernel::<15>($($arg),*),
            16 => $kernel::<16>($($arg),*),
            17 => $kernel::<17>($($arg),*),
            18 => $kernel::<18>($($arg),*),
            19 => $kernel::<19>($($arg),*),
            20 => $kernel::<20>($($arg),*),
            21 => $kernel::<21>($($arg),*),
            22 => $kernel::<22>($($arg),*),
            23 => $kernel::<23>($($arg),*),
            24 => $kernel::<24>($($arg),*),
            25 => $kernel::<25>($($arg),*),
            26 => $kernel::<26>($($arg),*),
            27 => $kernel::<27>($($arg),*),
            28 => $kernel::<28>($($arg),*),
            29 => $kernel::<29>($($arg),*),
            30 => $kernel::<30>($($arg),*),
            31 => $kernel::<31>($($arg),*),
            _ => $kernel::<32>($($arg),*),
        }
    };
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
    by_width!(width, write_at(values, out))
}

/// Writes one block at a width known at compile time, and returns that width.
///
/// A word is worked out and written once rather than accumulated into. The
/// obvious way round is to clear a buffer and fold each value into whichever
/// word or two it lands in, and that costs a read and a write of the buffer per
/// value and a clear of the whole thing per block, at whatever the widest block
/// might have been rather than at the width in hand. Going the other way, from
/// the word to the values that reach it, each output word is written exactly
/// once and nothing has to be cleared first.
fn write_at<const W: u32>(values: &[u32; BLOCK], out: &mut Vec<u8>) -> u32 {
    let width = W as usize;
    out.reserve(width * LANES * 4);
    for word in 0..width {
        // The value the first bit of this word falls inside, and how much of it
        // the word before already took.
        let first = 32 * word / width;
        let taken = u32::try_from(32 * word - first * width).unwrap_or(0);

        let mut bytes = [0u8; LANES * 4];
        for (lane, slot) in bytes.chunks_exact_mut(4).enumerate() {
            let mut packed = values[first * LANES + lane] >> taken;
            let mut bit = W - taken;
            let mut step = first + 1;
            // Whatever runs off the top of the word is what the next word
            // starts with, so the shift dropping it is the point rather than a
            // thing to be guarded against.
            while bit < 32 && step < STEPS {
                packed |= values[step * LANES + lane] << bit;
                bit += W;
                step += 1;
            }
            slot.copy_from_slice(&packed.to_le_bytes());
        }
        out.extend_from_slice(&bytes);
    }
    W
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
    if width == 0 || width > MAX_WIDTH {
        // Nothing here writes a zero width, because the packer spends a bit on
        // an all zero block rather than encode it as nothing. A reader that met
        // one and guessed would be the bug rather than the file.
        return Err(Error::Overflow);
    }
    by_width!(width, delta_at(input, base, out))
}

/// Unpacks one block written by [`pack_flat`], leaving the values as they are.
///
/// # Errors
///
/// Returns [`Error::Overflow`] if `width` is above thirty two, and
/// [`Error::Truncated`] if `input` holds fewer bytes than that width needs.
pub fn unpack_flat(input: &[u8], width: u32, out: &mut [u32; BLOCK]) -> Result<usize> {
    if width == 0 || width > MAX_WIDTH {
        return Err(Error::Overflow);
    }
    by_width!(width, flat_at(input, out))
}

/// Reads one block at a width known at compile time, values as written.
fn flat_at<const W: u32>(input: &[u8], out: &mut [u32; BLOCK]) -> Result<usize> {
    let words = words_of::<W>(input)?;
    for step in 0..STEPS {
        let values = step_at::<W>(words, step);
        for lane in 0..LANES {
            out[step * LANES + lane] = values[lane];
        }
    }
    Ok(W as usize * LANES * 4)
}

/// Reads one block at a width known at compile time and runs the sums back up.
fn delta_at<const W: u32>(input: &[u8], base: u32, out: &mut [u32; BLOCK]) -> Result<usize> {
    let words = words_of::<W>(input)?;
    // The lanes are independent all the way through the read, and this is the
    // only place they meet: four running sums side by side, each one a chain of
    // thirty two adds rather than the one chain of a hundred and twenty eight
    // that storing adjacent gaps would have cost. The sums run inside the same
    // pass as the unpacking rather than after it, so a block is written once.
    let mut running = [base; LANES];
    for step in 0..STEPS {
        let values = step_at::<W>(words, step);
        for lane in 0..LANES {
            running[lane] = running[lane].wrapping_add(values[lane]);
            out[step * LANES + lane] = running[lane];
        }
    }
    Ok(W as usize * LANES * 4)
}

/// The words one block at `W` occupies, or a truncation error.
///
/// Read straight out of the caller's bytes rather than through a buffer. A
/// buffer would have to be sized for the widest block a list can hold, so every
/// narrow block would pay to clear five hundred bytes it never used, and
/// clearing them costs more than the unpacking does.
#[inline]
fn words_of<const W: u32>(input: &[u8]) -> Result<&[[u8; LANES * 4]]> {
    let (head, _) = split_at(input, W as usize * LANES * 4)?;
    let (words, _) = head.as_chunks::<{ LANES * 4 }>();
    Ok(words)
}

/// The four values of one step, unpacked.
///
/// `W` is a constant, so the word index, the shift and the mask are constants,
/// and whether a value at this step straddles a word boundary is answered when
/// this is compiled rather than while it runs.
#[inline]
fn step_at<const W: u32>(words: &[[u8; LANES * 4]], step: usize) -> [u32; LANES] {
    let mask = if W == MAX_WIDTH {
        u32::MAX
    } else {
        (1u32 << W) - 1
    };
    let bit = step * W as usize;
    let word = bit / 32;
    let shift = u32::try_from(bit % 32).unwrap_or(0);

    let low = lanes_of(words, word);
    let mut out = [0u32; LANES];
    if shift + W > 32 {
        let high = lanes_of(words, word + 1);
        for lane in 0..LANES {
            out[lane] = ((low[lane] >> shift) | (high[lane] << (32 - shift))) & mask;
        }
    } else {
        for lane in 0..LANES {
            out[lane] = (low[lane] >> shift) & mask;
        }
    }
    out
}

/// The four lanes of one packed word, as one sixteen byte load.
#[inline]
fn lanes_of(words: &[[u8; LANES * 4]], word: usize) -> [u32; LANES] {
    let (bytes, _) = words[word].as_chunks::<4>();
    let mut out = [0u32; LANES];
    for lane in 0..LANES {
        out[lane] = u32::from_le_bytes(bytes[lane]);
    }
    out
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

    /// The layout written the plain way, with the width in a variable.
    ///
    /// This is what the kernels replaced. It is kept here as the thing they are
    /// checked against, because the whole claim of the rewrite is that it made
    /// the same bytes faster, and a rewrite that quietly changed the format
    /// would pass every round trip test in this file.
    fn reference(values: &[u32; BLOCK], width: u32) -> Vec<u8> {
        let mut words = vec![0u32; width as usize * LANES];
        for (step, chunk) in values.chunks_exact(LANES).enumerate() {
            let bit = step * width as usize;
            let word = bit / 32;
            let shift = u32::try_from(bit % 32).unwrap_or(0);
            for (lane, value) in chunk.iter().enumerate() {
                words[word * LANES + lane] |= value << shift;
                if shift + width > 32 {
                    words[(word + 1) * LANES + lane] |= value >> (32 - shift);
                }
            }
        }
        words.iter().flat_map(|word| word.to_le_bytes()).collect()
    }

    /// Values that fill `width` bits and vary from one another.
    fn spread(width: u32, seed: u32) -> [u32; BLOCK] {
        let mask = if width == MAX_WIDTH {
            u32::MAX
        } else {
            (1u32 << width) - 1
        };
        let mut values = [0u32; BLOCK];
        let mut state = seed | 1;
        for (k, slot) in values.iter_mut().enumerate() {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            // One value is the mask itself, so the block is packed at exactly
            // the width asked for rather than at whatever the noise reached.
            *slot = if k == 0 { mask } else { state & mask };
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
    fn every_kernel_writes_what_the_plain_loop_would_have() {
        for width in 1..=MAX_WIDTH {
            let values = spread(width, width.wrapping_mul(2_654_435_761));
            let mut bytes = Vec::new();
            let used = pack_flat(&values, &mut bytes);
            assert_eq!(used, width, "expected width {width}");
            assert_eq!(bytes, reference(&values, width), "width {width}");
        }
    }

    #[test]
    fn every_kernel_reads_a_full_block_back() {
        for width in 1..=MAX_WIDTH {
            let values = spread(width, width * 40_503);
            let mut bytes = Vec::new();
            let used = pack_flat(&values, &mut bytes);
            assert_eq!(used, width, "expected width {width}");

            let mut out = [0u32; BLOCK];
            let read = unpack_flat(&bytes, used, &mut out).expect("unpack");
            assert_eq!(read, bytes.len(), "width {width}");
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
        assert_eq!(
            unpack_flat(&[0u8; 1024], 33, &mut out),
            Err(Error::Overflow)
        );
        assert_eq!(unpack_flat(&[0u8; 1024], 0, &mut out), Err(Error::Overflow));
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
