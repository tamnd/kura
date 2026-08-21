//! XXH3, the 64 bit and 128 bit variants, one shot and unseeded.
//!
//! The engine needs a checksum for one job: telling a reader that the bytes it
//! is about to trust are the bytes that were written. [`crate::checksum`] does
//! that today with CRC-32, and this is what replaces it.
//!
//! # Why replace a checksum that works
//!
//! Speed, and only speed. CRC-32 sliced sixteen bytes at a time is fast enough
//! that verifying an index on open is affordable, and not fast enough that
//! verifying every block on every touch is. XXH3 over the same thirty two
//! megabytes on the same machine in the same run is six times quicker, which
//! moves it from one category to the other.
//!
//! Six times is the ratio and not the rate. The machine this was measured on
//! was at a load average of 28 at the time, so the absolute figures from that
//! run describe the machine's mood rather than the code, and are not comparable
//! with the ones written down in [`crate::checksum`]. The ratio held across
//! every run and every buffer size, which is why it is the number quoted.
//!
//! That difference decides the design of the file format rather than merely
//! improving it. A checksum you can only afford on demand gives you one number
//! over the whole file, which tells a reader that something is wrong and cannot
//! tell it what. A checksum you can afford on every touch gives you one per
//! block, which localises damage to a few kilobytes and makes repairing a store
//! possible instead of restoring it.
//!
//! # Why it is written out longhand
//!
//! The core crate has no dependencies, and this is arithmetic. There is nothing
//! here that is not a multiply, a shift or an exclusive or.
//!
//! # What is not here
//!
//! Seeds and custom secrets. XXH3 takes both, and the file format wants
//! neither, and an untested branch inside a checksum is worse than a branch
//! that does not exist. The seed is folded to zero throughout, which is what
//! removes several terms from the short input paths below.
//!
//! Streaming. Everything the format checksums is a block or a header that is
//! already contiguous in memory at the moment it is checked, so a one shot call
//! is the whole requirement. Adding a streaming form later is additive.
//!
//! Hand written vector code. The reference implementation has AVX2, SSE2 and
//! NEON versions of the one loop that matters. What is here is the scalar form
//! the reference calls the fallback, written so the optimiser can see the shape,
//! and it is a long way from the ceiling. That is measured rather than assumed:
//! see the module tests and `cargo run --release --example bench`.
//!
//! # Trusting it
//!
//! A checksum that is subtly wrong is worse than no checksum, because it is
//! trusted. Every branch of this is checked against values produced by the
//! reference implementation, embedded in the tests, and the lengths chosen are
//! the ones on each side of every branch boundary.

/// The primes the algorithm is built out of.
///
/// Named as the reference names them, because anybody comparing this against it
/// is going to be reading both at once.
mod primes {
    /// Used to scramble the accumulators and to seed one of them.
    pub const PRIME32_1: u64 = 0x9E37_79B1;
    /// Seeds one of the accumulators.
    pub const PRIME32_2: u64 = 0x85EB_CA77;
    /// Seeds one of the accumulators, and appears in the nine to sixteen path.
    pub const PRIME32_3: u64 = 0xC2B2_AE3D;
    /// The main multiplier, and the length multiplier for every long input.
    pub const PRIME64_1: u64 = 0x9E37_79B1_85EB_CA87;
    /// The second multiplier, and the one the high half of a 128 bit result
    /// folds the length in with.
    pub const PRIME64_2: u64 = 0xC2B2_AE3D_27D4_EB4F;
    /// Seeds an accumulator, and finishes the XXH64 avalanche.
    pub const PRIME64_3: u64 = 0x1656_67B1_9E37_79F9;
    /// Seeds an accumulator, and weights the high half of a 128 bit result.
    pub const PRIME64_4: u64 = 0x85EB_CA77_C2B2_AE63;
    /// Seeds an accumulator.
    pub const PRIME64_5: u64 = 0x27D4_EB2F_1656_67C5;
    /// The multiplier in the short avalanche.
    pub const PRIME_MX1: u64 = 0x1656_6791_9E37_79F9;
    /// The multiplier in the four to eight byte path.
    pub const PRIME_MX2: u64 = 0x9FB2_1C65_1E98_DF25;
}

use primes::{
    PRIME_MX1, PRIME_MX2, PRIME32_1, PRIME32_2, PRIME32_3, PRIME64_1, PRIME64_2, PRIME64_3,
    PRIME64_4, PRIME64_5,
};

/// The default secret, 192 bytes of it.
///
/// Not derived from anything. The reference generates it once and writes it
/// down, and every implementation carries the same bytes, because a hash is only
/// useful if two programs agree on it.
static SECRET: [u8; 192] = [
    0xb8, 0xfe, 0x6c, 0x39, 0x23, 0xa4, 0x4b, 0xbe, 0x7c, 0x01, 0x81, 0x2c, 0xf7, 0x21, 0xad, 0x1c,
    0xde, 0xd4, 0x6d, 0xe9, 0x83, 0x90, 0x97, 0xdb, 0x72, 0x40, 0xa4, 0xa4, 0xb7, 0xb3, 0x67, 0x1f,
    0xcb, 0x79, 0xe6, 0x4e, 0xcc, 0xc0, 0xe5, 0x78, 0x82, 0x5a, 0xd0, 0x7d, 0xcc, 0xff, 0x72, 0x21,
    0xb8, 0x08, 0x46, 0x74, 0xf7, 0x43, 0x24, 0x8e, 0xe0, 0x35, 0x90, 0xe6, 0x81, 0x3a, 0x26, 0x4c,
    0x3c, 0x28, 0x52, 0xbb, 0x91, 0xc3, 0x00, 0xcb, 0x88, 0xd0, 0x65, 0x8b, 0x1b, 0x53, 0x2e, 0xa3,
    0x71, 0x64, 0x48, 0x97, 0xa2, 0x0d, 0xf9, 0x4e, 0x38, 0x19, 0xef, 0x46, 0xa9, 0xde, 0xac, 0xd8,
    0xa8, 0xfa, 0x76, 0x3f, 0xe3, 0x9c, 0x34, 0x3f, 0xf9, 0xdc, 0xbb, 0xc7, 0xc7, 0x0b, 0x4f, 0x1d,
    0x8a, 0x51, 0xe0, 0x4b, 0xcd, 0xb4, 0x59, 0x31, 0xc8, 0x9f, 0x7e, 0xc9, 0xd9, 0x78, 0x73, 0x64,
    0xea, 0xc5, 0xac, 0x83, 0x34, 0xd3, 0xeb, 0xc3, 0xc5, 0x81, 0xa0, 0xff, 0xfa, 0x13, 0x63, 0xeb,
    0x17, 0x0d, 0xdd, 0x51, 0xb7, 0xf0, 0xda, 0x49, 0xd3, 0x16, 0x55, 0x26, 0x29, 0xd4, 0x68, 0x9e,
    0x2b, 0x16, 0xbe, 0x58, 0x7d, 0x47, 0xa1, 0xfc, 0x8f, 0xf8, 0xb8, 0xd1, 0x7a, 0xd0, 0x31, 0xce,
    0x45, 0xcb, 0x3a, 0x8f, 0x95, 0x16, 0x04, 0x28, 0xaf, 0xd7, 0xfb, 0xca, 0xbb, 0x4b, 0x40, 0x7e,
];

/// How many bytes one pass of the accumulator loop eats.
const STRIPE: usize = 64;

/// How far along the secret each stripe moves.
const CONSUME: usize = 8;

/// How many stripes go by before the accumulators are scrambled.
///
/// Sixteen, because the secret is 192 bytes and a stripe reads 64 of them from
/// a position that advances by 8, so the last position that still has 64 bytes
/// behind it is 128.
const STRIPES_PER_BLOCK: usize = (SECRET.len() - STRIPE) / CONSUME;

/// How many bytes go by before the accumulators are scrambled.
const BLOCK: usize = STRIPE * STRIPES_PER_BLOCK;

/// Where in the secret the final fold starts reading.
const MERGE_START: usize = 11;

/// How far back from the end of the secret the last stripe reads.
const LAST_ACC_START: usize = 7;

/// Where the second half of the 129 to 240 byte path starts reading.
const MID_START: usize = 3;

/// How far back from the shortest legal secret the last mix reads.
const MID_LAST: usize = 17;

/// The shortest secret the algorithm accepts, which is a constant in the
/// reference and shows up here only as a position to count back from.
const SECRET_MIN: usize = 136;

/// The eight accumulators, before anything has been accumulated into them.
const INIT: [u64; 8] = [
    PRIME32_3, PRIME64_1, PRIME64_2, PRIME64_3, PRIME64_4, PRIME32_2, PRIME64_5, PRIME32_1,
];

/// Hashes `bytes` to 64 bits.
///
/// The same value the reference implementation produces for the same input with
/// no seed and the default secret.
#[must_use]
pub fn hash64(bytes: &[u8]) -> u64 {
    match bytes.len() {
        0 => avalanche64(word(&SECRET, 56) ^ word(&SECRET, 64)),
        1..=3 => {
            let combined = u64::from(fold_short(bytes));
            avalanche64(combined ^ (u64::from(half(&SECRET, 0)) ^ u64::from(half(&SECRET, 4))))
        }
        4..=8 => {
            let low = u64::from(half(bytes, 0));
            let high = u64::from(half(bytes, bytes.len() - 4));
            let joined = high + (low << 32);
            rrmxmx(joined ^ (word(&SECRET, 8) ^ word(&SECRET, 16)), bytes.len())
        }
        9..=16 => {
            let low = word(bytes, 0) ^ (word(&SECRET, 24) ^ word(&SECRET, 32));
            let high = word(bytes, bytes.len() - 8) ^ (word(&SECRET, 40) ^ word(&SECRET, 48));
            let mixed = as_u64(bytes.len())
                .wrapping_add(low.swap_bytes())
                .wrapping_add(high)
                .wrapping_add(fold(low, high));
            avalanche(mixed)
        }
        17..=128 => avalanche(short64(bytes)),
        129..=240 => avalanche(mid64(bytes)),
        _ => {
            let acc = accumulate(bytes);
            merge(
                &acc,
                MERGE_START,
                as_u64(bytes.len()).wrapping_mul(PRIME64_1),
            )
        }
    }
}

/// Hashes `bytes` to 128 bits.
///
/// The same value the reference implementation produces for the same input with
/// no seed and the default secret. The low 64 bits are not the 64 bit hash for
/// short inputs and are for long ones, which is a property of the algorithm
/// rather than of this code and is worth knowing before somebody notices it and
/// assumes one is derived from the other.
#[must_use]
pub fn hash128(bytes: &[u8]) -> u128 {
    let (low, high) = match bytes.len() {
        0 => (
            avalanche64(word(&SECRET, 64) ^ word(&SECRET, 72)),
            avalanche64(word(&SECRET, 80) ^ word(&SECRET, 88)),
        ),
        1..=3 => {
            let low = fold_short(bytes);
            // The high half sees the same four bytes in a different order, which
            // is what stops the two halves from being the same function.
            let high = low.swap_bytes().rotate_left(13);
            (
                avalanche64(
                    u64::from(low) ^ (u64::from(half(&SECRET, 0)) ^ u64::from(half(&SECRET, 4))),
                ),
                avalanche64(
                    u64::from(high) ^ (u64::from(half(&SECRET, 8)) ^ u64::from(half(&SECRET, 12))),
                ),
            )
        }
        4..=8 => {
            let low = u64::from(half(bytes, 0));
            let high = u64::from(half(bytes, bytes.len() - 4));
            let joined = low + (high << 32);
            let keyed = joined ^ (word(&SECRET, 16) ^ word(&SECRET, 24));

            let product = u128::from(keyed)
                .wrapping_mul(u128::from(PRIME64_1 + ((as_u64(bytes.len())) << 2)));
            let mut lower = low_half(product);
            let upper = high_half(product).wrapping_add(lower << 1);
            lower ^= upper >> 3;
            lower = xorshift(lower, 35).wrapping_mul(PRIME_MX2);
            (xorshift(lower, 28), avalanche(upper))
        }
        9..=16 => {
            let low = word(bytes, 0);
            let high = word(bytes, bytes.len() - 8);

            let product = u128::from(low ^ high ^ (word(&SECRET, 32) ^ word(&SECRET, 40)))
                .wrapping_mul(u128::from(PRIME64_1));
            let mut lower = low_half(product).wrapping_add(as_u64(bytes.len() - 1) << 54);
            let keyed = high ^ (word(&SECRET, 48) ^ word(&SECRET, 56));
            let upper = high_half(product)
                .wrapping_add(keyed)
                .wrapping_add(mul32(keyed, PRIME32_2 - 1));
            lower ^= upper.swap_bytes();

            let folded = u128::from(lower).wrapping_mul(u128::from(PRIME64_2));
            (
                avalanche(low_half(folded)),
                avalanche(high_half(folded).wrapping_add(upper.wrapping_mul(PRIME64_2))),
            )
        }
        17..=128 => finish128(short128(bytes), bytes.len()),
        129..=240 => finish128(mid128(bytes), bytes.len()),
        _ => {
            let acc = accumulate(bytes);
            let length = as_u64(bytes.len());
            (
                merge(&acc, MERGE_START, length.wrapping_mul(PRIME64_1)),
                merge(
                    &acc,
                    SECRET.len() - size_of_val(&acc) - MERGE_START,
                    !length.wrapping_mul(PRIME64_2),
                ),
            )
        }
    };
    u128::from(low) | (u128::from(high) << 64)
}

/// The seventeen to one hundred and twenty eight byte path, before the finish.
///
/// Between one and four pairs of sixteen byte mixes, one pair from each end
/// walking inwards, so every byte of the input is read at least once and the
/// ones near the ends are read twice. The nesting is the reference's, and it is
/// written this way rather than as a loop because the count is decided by three
/// comparisons and a loop would have to recompute it.
fn short64(bytes: &[u8]) -> u64 {
    let length = bytes.len();
    let mut acc = as_u64(length).wrapping_mul(PRIME64_1);
    if length > 32 {
        if length > 64 {
            if length > 96 {
                acc = acc.wrapping_add(mix16(bytes, 48, 96)).wrapping_add(mix16(
                    bytes,
                    length - 64,
                    112,
                ));
            }
            acc =
                acc.wrapping_add(mix16(bytes, 32, 64))
                    .wrapping_add(mix16(bytes, length - 48, 80));
        }
        acc = acc
            .wrapping_add(mix16(bytes, 16, 32))
            .wrapping_add(mix16(bytes, length - 32, 48));
    }
    acc.wrapping_add(mix16(bytes, 0, 0))
        .wrapping_add(mix16(bytes, length - 16, 16))
}

/// The seventeen to one hundred and twenty eight byte path for 128 bits.
fn short128(bytes: &[u8]) -> (u64, u64) {
    let length = bytes.len();
    let mut acc = (as_u64(length).wrapping_mul(PRIME64_1), 0);
    if length > 32 {
        if length > 64 {
            if length > 96 {
                acc = mix32(acc, bytes, 48, length - 64, 96);
            }
            acc = mix32(acc, bytes, 32, length - 48, 64);
        }
        acc = mix32(acc, bytes, 16, length - 32, 32);
    }
    mix32(acc, bytes, 0, length - 16, 0)
}

/// The one hundred and twenty nine to two hundred and forty byte path.
///
/// Eight mixes, an avalanche, then as many more mixes as the length allows,
/// then one over the last sixteen bytes. The avalanche in the middle is what
/// stops the second group from being able to cancel the first.
fn mid64(bytes: &[u8]) -> u64 {
    let length = bytes.len();
    let rounds = length / 16;

    let mut acc = as_u64(length).wrapping_mul(PRIME64_1);
    for i in 0..8 {
        acc = acc.wrapping_add(mix16(bytes, 16 * i, 16 * i));
    }
    acc = avalanche(acc);

    for i in 8..rounds {
        acc = acc.wrapping_add(mix16(bytes, 16 * i, 16 * (i - 8) + MID_START));
    }
    acc.wrapping_add(mix16(bytes, length - 16, SECRET_MIN - MID_LAST))
}

/// The one hundred and twenty nine to two hundred and forty byte path for 128
/// bits.
fn mid128(bytes: &[u8]) -> (u64, u64) {
    let length = bytes.len();
    let rounds = length / 32;

    let mut acc = (as_u64(length).wrapping_mul(PRIME64_1), 0);
    for i in 0..4 {
        acc = mix32(acc, bytes, 32 * i, 32 * i + 16, 32 * i);
    }
    acc = (avalanche(acc.0), avalanche(acc.1));

    for i in 4..rounds {
        acc = mix32(acc, bytes, 32 * i, 32 * i + 16, MID_START + 32 * (i - 4));
    }
    // The two inputs are the other way round here, last sixteen bytes first,
    // which is the reference's and is load bearing: it is what makes the tail
    // contribute differently from every mix before it.
    mix32(
        acc,
        bytes,
        length - 16,
        length - 32,
        SECRET_MIN - MID_LAST - 16,
    )
}

/// Turns a pair of accumulators into a 128 bit result.
///
/// Shared by the two middle paths, which differ in how they fill the pair and
/// not at all in what they do with it.
fn finish128((low, high): (u64, u64), length: usize) -> (u64, u64) {
    let length = as_u64(length);
    let folded = low
        .wrapping_mul(PRIME64_1)
        .wrapping_add(high.wrapping_mul(PRIME64_4))
        .wrapping_add(length.wrapping_mul(PRIME64_2));
    // Negated, not merely avalanched. Without it the high half of a short input
    // and the high half of a long one would sit in the same part of the range.
    (
        avalanche(low.wrapping_add(high)),
        0u64.wrapping_sub(avalanche(folded)),
    )
}

/// The loop that does the work on anything over two hundred and forty bytes.
///
/// Eight accumulators, sixty four bytes read into them at a time, and a scramble
/// every sixteen stripes so that the accumulators cannot drift into a state the
/// input controls. The last stripe is read again from the end of the input,
/// which is how the tail is covered without a separate path for it.
fn accumulate(bytes: &[u8]) -> [u64; 8] {
    let mut acc = INIT;

    let blocks = (bytes.len() - 1) / BLOCK;
    for block in 0..blocks {
        let at = block * BLOCK;
        for stripe in 0..STRIPES_PER_BLOCK {
            round(&mut acc, &bytes[at + stripe * STRIPE..], stripe * CONSUME);
        }
        scramble(&mut acc);
    }

    let at = blocks * BLOCK;
    let stripes = (bytes.len() - 1 - at) / STRIPE;
    for stripe in 0..stripes {
        round(&mut acc, &bytes[at + stripe * STRIPE..], stripe * CONSUME);
    }

    round(
        &mut acc,
        &bytes[bytes.len() - STRIPE..],
        SECRET.len() - STRIPE - LAST_ACC_START,
    );
    acc
}

/// Reads sixty four bytes into the eight accumulators.
///
/// Each lane takes a thirty two by thirty two bit product of its own keyed word,
/// and the raw word goes into its neighbour. The neighbour swap is what stops
/// the eight lanes from being eight independent hashes of eight independent
/// streams.
fn round(acc: &mut [u64; 8], stripe: &[u8], at: usize) {
    for i in 0..8 {
        let value = word(stripe, i * 8);
        let keyed = value ^ word(&SECRET, at + i * 8);
        acc[i ^ 1] = acc[i ^ 1].wrapping_add(value);
        acc[i] = acc[i].wrapping_add(mul32(keyed, keyed >> 32));
    }
}

/// Stirs the accumulators between blocks.
fn scramble(acc: &mut [u64; 8]) {
    for (i, lane) in acc.iter_mut().enumerate() {
        *lane = (xorshift(*lane, 47) ^ word(&SECRET, SECRET.len() - STRIPE + i * 8))
            .wrapping_mul(PRIME32_1);
    }
}

/// Folds the eight accumulators down to one value.
fn merge(acc: &[u64; 8], at: usize, start: u64) -> u64 {
    let mut result = start;
    for i in 0..4 {
        result = result.wrapping_add(fold(
            acc[2 * i] ^ word(&SECRET, at + 16 * i),
            acc[2 * i + 1] ^ word(&SECRET, at + 16 * i + 8),
        ));
    }
    avalanche(result)
}

/// Mixes sixteen bytes of input against sixteen bytes of secret.
fn mix16(bytes: &[u8], at: usize, secret: usize) -> u64 {
    fold(
        word(bytes, at) ^ word(&SECRET, secret),
        word(bytes, at + 8) ^ word(&SECRET, secret + 8),
    )
}

/// Mixes two sixteen byte pieces into a pair of accumulators, crosswise.
///
/// Each half takes a mix of one piece and then an exclusive or with the raw
/// words of the other, so neither half can be computed without both pieces.
fn mix32(
    (low, high): (u64, u64),
    bytes: &[u8],
    first: usize,
    second: usize,
    secret: usize,
) -> (u64, u64) {
    (
        low.wrapping_add(mix16(bytes, first, secret))
            ^ word(bytes, second).wrapping_add(word(bytes, second + 8)),
        high.wrapping_add(mix16(bytes, second, secret + 16))
            ^ word(bytes, first).wrapping_add(word(bytes, first + 8)),
    )
}

/// The four to eight byte finaliser, which folds the length in twice.
fn rrmxmx(mut value: u64, length: usize) -> u64 {
    value ^= value.rotate_left(49) ^ value.rotate_left(24);
    value = value.wrapping_mul(PRIME_MX2);
    value ^= (value >> 35).wrapping_add(as_u64(length));
    value = value.wrapping_mul(PRIME_MX2);
    xorshift(value, 28)
}

/// The three bytes of a one to three byte input, packed into four.
///
/// The length goes in as one of the four, which is what keeps a one byte input
/// from colliding with the two byte input that starts and ends with it.
fn fold_short(bytes: &[u8]) -> u32 {
    u32::from(bytes[bytes.len() - 1])
        | (as_u32(bytes.len()) << 8)
        | (u32::from(bytes[0]) << 16)
        | (u32::from(bytes[bytes.len() >> 1]) << 24)
}

/// The 128 bit product of two words, folded down to one.
fn fold(left: u64, right: u64) -> u64 {
    let product = u128::from(left).wrapping_mul(u128::from(right));
    low_half(product) ^ high_half(product)
}

/// The low sixty four bits of a 128 bit value.
///
/// Truncating is the operation here rather than a risk to be handled. The
/// algorithm is written throughout in terms of the two halves of a widening
/// multiply, and a checked conversion at these call sites could only fail by the
/// algorithm itself being wrong, which the reference vectors are what catch.
#[expect(
    clippy::cast_possible_truncation,
    reason = "taking the low half of a widening multiply is what the algorithm asks for"
)]
const fn low_half(value: u128) -> u64 {
    value as u64
}

/// The high sixty four bits of a 128 bit value.
///
/// The shift leaves sixty four significant bits, so this one is a narrowing the
/// compiler can see is exact.
const fn high_half(value: u128) -> u64 {
    low_half(value >> 64)
}

/// The product of the low halves of two words, kept at full width.
fn mul32(left: u64, right: u64) -> u64 {
    (left & 0xFFFF_FFFF).wrapping_mul(right & 0xFFFF_FFFF)
}

/// The short avalanche, used everywhere except on the shortest inputs.
fn avalanche(mut value: u64) -> u64 {
    value ^= value >> 37;
    value = value.wrapping_mul(PRIME_MX1);
    value ^ (value >> 32)
}

/// The XXH64 avalanche, used on inputs of three bytes or fewer.
fn avalanche64(mut value: u64) -> u64 {
    value ^= value >> 33;
    value = value.wrapping_mul(PRIME64_2);
    value ^= value >> 29;
    value = value.wrapping_mul(PRIME64_3);
    value ^ (value >> 32)
}

/// One exclusive or with a shift of itself.
const fn xorshift(value: u64, shift: u32) -> u64 {
    value ^ (value >> shift)
}

/// Eight bytes read little endian.
///
/// Every caller has already established that the bytes are there, either from
/// the length arm it is inside or from the loop bound it is under, so this
/// indexes rather than checking. A panic here would be this module's bug and
/// not the caller's, which is why it is not a `Result`.
fn word(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().expect("eight bytes"))
}

/// Four bytes read little endian.
fn half(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("four bytes"))
}

/// A length as a `u64`.
///
/// Every call site is a length that has already been range checked by the arm
/// it is inside, so the fallback never happens on any input this crate can be
/// handed.
fn as_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

/// A length as a `u32`, for the one to three byte path where it is at most 3.
fn as_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Input with no structure a hash could accidentally like.
    ///
    /// A linear congruential generator rather than anything from the standard
    /// library, so that the bytes these tests run against are the same bytes on
    /// every machine and in every version, which is what makes the values below
    /// mean anything.
    fn data(len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(len);
        let mut state = 0x9E37_79B1_u32;
        for _ in 0..len {
            state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            out.push(((state >> 16) & 0xFF) as u8);
        }
        out
    }

    /// Length, 64 bit hash, 128 bit hash, from the reference implementation.
    ///
    /// The lengths are the ones on each side of every branch boundary in the
    /// algorithm: 0, 1, 3, 4, 8, 9, 16, 17, 128, 129, 240, 241 are where the
    /// paths change, 32, 64, 96 are where the middle path adds a pair of mixes,
    /// 1024 is a block, and 2048 and above run the block loop more than once.
    ///
    /// Produced by the reference implementation and pasted in. Nothing in this
    /// crate could produce them, which is the point: a test that checks this
    /// code against itself would pass on a hash that agrees with nothing else in
    /// the world.
    const VECTORS: &[(usize, u64, u128)] = &[
        (
            0,
            0x2d06_8005_38d3_94c2,
            0x99aa_06d3_0147_98d8_6001_c324_468d_497f,
        ),
        (
            1,
            0x937c_7798_9a8a_94df,
            0xba55_8222_dedb_c2c8_937c_7798_9a8a_94df,
        ),
        (
            2,
            0x76f8_74f3_cf19_462c,
            0x006f_ff3e_098f_ed9f_76f8_74f3_cf19_462c,
        ),
        (
            3,
            0xb085_ec0a_6127_cc24,
            0xe00f_0d29_7a30_9e2c_b085_ec0a_6127_cc24,
        ),
        (
            4,
            0x00fe_6a93_599c_a9d3,
            0x5326_88f1_ddd5_28a2_4b4c_22a7_653b_bb59,
        ),
        (
            5,
            0x0f68_95e8_2b9b_2796,
            0xcfa0_12e8_1f9a_3ab0_c7d2_3c50_05b7_97b4,
        ),
        (
            7,
            0x344d_91ad_e18a_50f0,
            0xa4b8_3d67_89c3_cc8b_90a1_1393_7794_1bdf,
        ),
        (
            8,
            0xeac9_61c1_520d_887b,
            0x4cf0_3af0_bbbf_b0c9_089d_319a_6670_ccf2,
        ),
        (
            9,
            0xae00_5b6e_e477_f3d9,
            0xe5b1_21ae_8bc9_eeb9_6d69_d2ee_f34e_57b2,
        ),
        (
            12,
            0x726f_c083_d41d_bb25,
            0xc995_4f13_442a_35e1_7186_265b_f8dd_57da,
        ),
        (
            15,
            0xa1d3_8390_244b_cb97,
            0x544b_58a1_1043_672c_f80f_9824_fdcf_972c,
        ),
        (
            16,
            0x209a_b5cc_a7ce_fbba,
            0x47ca_9273_a4ee_e5ba_248f_79d4_38c9_b9f2,
        ),
        (
            17,
            0xd62d_c04f_4f52_001a,
            0xdb4c_2e83_9ef5_61c8_12ba_0f72_1ebf_c7de,
        ),
        (
            31,
            0xbf1f_6e27_897b_b382,
            0xdd9b_dbbb_80d4_cda5_8e28_bdcc_c9d8_1539,
        ),
        (
            32,
            0x4ef1_b7fd_a44b_8ad9,
            0xf7bf_8d42_4a1e_506d_0f5a_288f_ff59_f740,
        ),
        (
            33,
            0x0a16_28d7_810c_95ef,
            0xd8b2_7ca8_d4e6_4a49_80b7_2746_503a_a6c0,
        ),
        (
            63,
            0x930e_02df_8f6a_f6ab,
            0xc842_afae_05f6_5af6_794d_c28d_4bdb_fdd9,
        ),
        (
            64,
            0xeade_99f7_0455_fa42,
            0x9ed2_fe26_1fa2_45cf_073a_2dca_d642_da79,
        ),
        (
            65,
            0x950e_da12_d455_8246,
            0x706f_01ec_08dc_301f_ee63_f055_48d4_1352,
        ),
        (
            95,
            0x0161_5223_3d0d_6a4d,
            0xa49c_ef49_bc46_a544_61c0_4360_3435_b53c,
        ),
        (
            96,
            0x2408_e383_bf71_b457,
            0x2365_9c2f_f74d_5c14_57ef_97ee_9365_b7cf,
        ),
        (
            97,
            0x6555_8d6c_f19a_151f,
            0x9316_2b9a_d826_c15e_6d0a_7cf8_2e96_342d,
        ),
        (
            127,
            0x818c_1836_7692_3b8e,
            0x736a_3aee_0ee3_59b3_e13a_021b_de35_11a1,
        ),
        (
            128,
            0xb962_3caa_e4c9_1e0f,
            0xafbd_b0ea_a4bb_b02a_e772_9e35_2afe_04b4,
        ),
        (
            129,
            0x460c_4159_a9cd_e7dc,
            0xad93_97e2_e18e_46f5_64f7_a842_c4db_9888,
        ),
        (
            160,
            0xb972_2092_c4fb_0bec,
            0xac17_b6f9_a6b8_3726_702b_3c8d_55a7_da93,
        ),
        (
            199,
            0xe5f6_f550_778e_fa80,
            0xb785_2d94_796d_a444_04d5_af21_d86b_28f4,
        ),
        (
            200,
            0xb794_8960_eeac_3a77,
            0x3930_1c5d_0166_d5d1_2a42_803a_a5fe_de39,
        ),
        (
            239,
            0x1549_2345_b59f_5c8f,
            0xa848_24a4_6575_0afa_b818_9eea_f465_9e2a,
        ),
        (
            240,
            0xd88a_f41b_708e_65f1,
            0x21a1_496a_437e_e264_2fc9_cfc8_85b2_d40b,
        ),
        (
            241,
            0xc9c1_91d2_988d_ef03,
            0x064e_fce9_9293_124e_c9c1_91d2_988d_ef03,
        ),
        (
            255,
            0x4de3_28d5_fb17_892d,
            0x8c42_4ddd_2d49_9d17_4de3_28d5_fb17_892d,
        ),
        (
            256,
            0x5f51_a8a7_a3d6_008d,
            0x0ebd_bc54_ee08_bc91_5f51_a8a7_a3d6_008d,
        ),
        (
            512,
            0xfe9f_245a_7435_29d5,
            0x25fc_e0e7_b352_0938_fe9f_245a_7435_29d5,
        ),
        (
            1023,
            0x4483_980a_9d8f_6e38,
            0x32a9_bbe3_4b04_8ba5_4483_980a_9d8f_6e38,
        ),
        (
            1024,
            0x70ca_e9e7_a04d_3027,
            0x5016_1e5a_25ff_88b0_70ca_e9e7_a04d_3027,
        ),
        (
            1025,
            0x9558_9abf_c51e_02d8,
            0x9b53_27bb_c2ff_07fe_9558_9abf_c51e_02d8,
        ),
        (
            2047,
            0x7489_ba6b_273e_83db,
            0x97aa_56e1_f9a0_af90_7489_ba6b_273e_83db,
        ),
        (
            2048,
            0xfbc0_1229_eeeb_45cc,
            0xf626_4a9e_a1fc_2bdd_fbc0_1229_eeeb_45cc,
        ),
        (
            4096,
            0x9708_cd90_1bda_18e1,
            0x7997_8be9_b46b_6950_9708_cd90_1bda_18e1,
        ),
        (
            10_000,
            0xb3cb_262a_cf17_f66b,
            0x980f_07c5_59fe_663d_b3cb_262a_cf17_f66b,
        ),
    ];

    #[test]
    fn agrees_with_the_reference_at_sixty_four_bits() {
        for &(len, expected, _) in VECTORS {
            assert_eq!(hash64(&data(len)), expected, "length {len}");
        }
    }

    #[test]
    fn agrees_with_the_reference_at_one_hundred_and_twenty_eight_bits() {
        for &(len, _, expected) in VECTORS {
            assert_eq!(hash128(&data(len)), expected, "length {len}");
        }
    }

    #[test]
    fn the_empty_input_is_not_zero() {
        // A hash that returns zero for nothing makes a zeroed page look like a
        // correctly checksummed empty one, which is the exact state a partly
        // written file is in.
        assert_ne!(hash64(b""), 0);
        assert_ne!(hash128(b""), 0);
    }

    #[test]
    fn every_length_up_to_a_few_blocks_is_reachable_without_panicking() {
        // The paths are chosen by length and several of them index the input at
        // fixed offsets from both ends. This is the check that no length lands
        // on an arm whose offsets do not fit.
        for len in 0..=3_000 {
            let bytes = data(len);
            let _ = hash64(&bytes);
            let _ = hash128(&bytes);
        }
    }

    #[test]
    fn one_flipped_bit_changes_both_hashes() {
        for len in [1_usize, 4, 9, 17, 100, 200, 300, 2_000] {
            let clean = data(len);
            let (was64, was128) = (hash64(&clean), hash128(&clean));
            for byte in 0..len {
                for bit in [0_u8, 3, 7] {
                    let mut damaged = clean.clone();
                    damaged[byte] ^= 1 << bit;
                    assert_ne!(
                        hash64(&damaged),
                        was64,
                        "64 at {len}, byte {byte}, bit {bit}"
                    );
                    assert_ne!(
                        hash128(&damaged),
                        was128,
                        "128 at {len}, byte {byte}, bit {bit}"
                    );
                }
            }
        }
    }

    #[test]
    fn trailing_zeroes_are_not_invisible() {
        // A checksum that misses appended zeroes misses a truncated write padded
        // back out to length, which is what a power cut produces.
        assert_ne!(hash128(b"kura"), hash128(b"kura\0"));
        assert_ne!(hash128(b"kura\0"), hash128(b"kura\0\0"));
        assert_ne!(hash64(b"kura"), hash64(b"kura\0"));
    }

    #[test]
    fn the_two_halves_of_a_long_hash_are_different_functions() {
        // For inputs over 240 bytes the low half is the 64 bit hash, which is a
        // property of the algorithm rather than an accident, and the high half
        // has to not be, or the second sixty four bits would be free of
        // information.
        for len in [241_usize, 1_000, 5_000] {
            let bytes = data(len);
            let whole = hash128(&bytes);
            assert_eq!(low_half(whole), hash64(&bytes), "length {len}");
            assert_ne!(high_half(whole), low_half(whole), "length {len}");
        }
    }

    #[test]
    fn the_secret_is_the_length_the_constants_assume() {
        // Several offsets here are written as distances from the end of the
        // secret, so a secret of the wrong length would read the wrong bytes and
        // still produce a plausible looking number.
        assert_eq!(SECRET.len(), 192);
        assert_eq!(STRIPES_PER_BLOCK, 16);
        assert_eq!(BLOCK, 1_024);
    }
}
