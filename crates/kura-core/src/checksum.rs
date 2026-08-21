//! CRC-32, the IEEE polynomial that zlib, gzip and PNG use.
//!
//! The engine needs a checksum for one job: telling a reader that the bytes it
//! is about to trust are the bytes that were written. CRC-32 is the right size
//! for that. It is not a hash, it does not defend against anybody who wants to
//! change a file, and nothing here should ever be used as though it did.
//!
//! It is written out longhand rather than pulled from a crate because the core
//! crate has no dependencies and this is a page of arithmetic. The tables are
//! built at compile time, so nothing here costs anything at startup.
//!
//! # Why there are sixteen tables and not one
//!
//! The textbook version of this is one table and one lookup per byte, and it
//! was what this file did until it turned out to be most of what opening an
//! index costs. Every iteration reads the state the previous iteration wrote,
//! so the loop is a chain of dependent loads with nothing for the processor to
//! overlap, and it settles at about 256 MB/s. On a 700 MB segment that is two
//! and a half seconds spent before the first query is even parsed.
//!
//! Slicing consumes sixteen bytes at a time instead of one. The sixteen lookups
//! within an iteration do not depend on each other, so they issue together, and
//! the dependency chain becomes one exclusive or per sixteen bytes rather than
//! one per byte. The tables are what make that possible: table `n` holds the
//! contribution of a byte that still has `n` more bytes behind it, so the work
//! the serial loop did in sixteen steps is precomputed into sixteen
//! independent lookups.
//!
//! The polynomial does not change and neither do the values. This is the same
//! CRC-32 in a different order, so every file any previous build wrote still
//! verifies.
//!
//! The cost is sixteen kilobytes of static tables instead of one. That is
//! binary size rather than startup work, and it is the whole price.
//!
//! # What is not here
//!
//! Hardware acceleration. Modern 64 bit ARM chips have an instruction for
//! exactly this polynomial and x86-64 does not, because the one it has is for
//! the Castagnoli polynomial instead. Matching ARM on x86-64 means carry-less
//! multiply folding, which is unsafe code, runtime feature detection and a
//! great deal more to get wrong. This is safe, it is the same code on every
//! target we ship, and it closes most of the gap.

/// The reflected form of the IEEE polynomial.
const POLYNOMIAL: u32 = 0xEDB8_8320;

/// How many bytes an iteration of the fast path consumes.
///
/// Also the number of tables, because a byte needs a different table for each
/// number of bytes that follow it within the same iteration.
const SLICE: usize = 16;

/// The classic table, giving the state after one byte.
// The index is bounded by the loop condition, so the cast cannot truncate.
// try_from is not usable here because this runs at compile time.
#[allow(clippy::cast_possible_truncation)]
const fn build_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut index = 0usize;
    while index < 256 {
        let mut value = index as u32;
        let mut bit = 0;
        while bit < 8 {
            value = if value & 1 == 0 {
                value >> 1
            } else {
                POLYNOMIAL ^ (value >> 1)
            };
            bit += 1;
        }
        table[index] = value;
        index += 1;
    }
    table
}

/// The sixteen tables, each derived from the one before it.
///
/// Pushing an entry of table `n` through one more byte of zeroes gives the
/// entry of table `n + 1`, which is one step of the serial loop with the input
/// byte already accounted for. Doing that fifteen times is the whole
/// derivation, and it is why only the first table has to be built from the
/// polynomial.
const fn build_tables() -> [[u32; 256]; SLICE] {
    let mut tables = [[0u32; 256]; SLICE];
    tables[0] = build_table();

    let mut slice = 1;
    while slice < SLICE {
        let mut index = 0usize;
        while index < 256 {
            let previous = tables[slice - 1][index];
            tables[slice][index] = (previous >> 8) ^ tables[0][(previous & 0xff) as usize];
            index += 1;
        }
        slice += 1;
    }
    tables
}

static TABLES: [[u32; 256]; SLICE] = build_tables();

/// The first of the tables, which is the one the leftover bytes use.
static TABLE: &[u32; 256] = &TABLES[0];

/// Computes the checksum of `bytes` in one call.
#[must_use]
pub fn crc32(bytes: &[u8]) -> u32 {
    let mut hasher = Crc32::new();
    hasher.update(bytes);
    hasher.finish()
}

/// An incremental checksum, for the writer that assembles a file from pieces
/// and would rather not concatenate them first.
#[derive(Debug, Clone)]
pub struct Crc32 {
    state: u32,
}

impl Crc32 {
    /// Starts a new checksum.
    #[must_use]
    pub const fn new() -> Self {
        Self { state: !0 }
    }

    /// Feeds more bytes in.
    ///
    /// Sixteen bytes at a time while there are sixteen to be had, and one at a
    /// time for what is left over. A caller that hands the input over in small
    /// pieces never reaches the fast path, which is correct rather than
    /// unfortunate: the pieces have to be joined by the state and there is no
    /// way to run ahead of that.
    pub fn update(&mut self, bytes: &[u8]) {
        let mut state = self.state;

        let (blocks, tail) = bytes.as_chunks::<SLICE>();
        for block in blocks {
            // The state joins the first word and only the first word, which is
            // the whole trick. Everything after it is input the tables can be
            // consulted about without knowing what came before.
            let words = block.as_chunks::<4>().0;
            let a = state ^ u32::from_le_bytes(words[0]);
            let b = u32::from_le_bytes(words[1]);
            let c = u32::from_le_bytes(words[2]);
            let d = u32::from_le_bytes(words[3]);

            // Written out rather than looped so that every table index is a
            // constant. A loop counter here would put a bounds check on the
            // inner dimension and leave the unrolling to the optimiser, in the
            // one place in this file where the point is the speed.
            state = TABLES[15][(a & 0xff) as usize]
                ^ TABLES[14][((a >> 8) & 0xff) as usize]
                ^ TABLES[13][((a >> 16) & 0xff) as usize]
                ^ TABLES[12][(a >> 24) as usize]
                ^ TABLES[11][(b & 0xff) as usize]
                ^ TABLES[10][((b >> 8) & 0xff) as usize]
                ^ TABLES[9][((b >> 16) & 0xff) as usize]
                ^ TABLES[8][(b >> 24) as usize]
                ^ TABLES[7][(c & 0xff) as usize]
                ^ TABLES[6][((c >> 8) & 0xff) as usize]
                ^ TABLES[5][((c >> 16) & 0xff) as usize]
                ^ TABLES[4][(c >> 24) as usize]
                ^ TABLES[3][(d & 0xff) as usize]
                ^ TABLES[2][((d >> 8) & 0xff) as usize]
                ^ TABLES[1][((d >> 16) & 0xff) as usize]
                ^ TABLES[0][(d >> 24) as usize];
        }

        for &byte in tail {
            let index = (state ^ u32::from(byte)) & 0xff;
            state = TABLE[index as usize] ^ (state >> 8);
        }

        self.state = state;
    }

    /// Returns the checksum of everything fed in so far.
    ///
    /// The hasher can keep being used afterwards, which is what makes it
    /// possible to checksum a body and then keep appending to it.
    #[must_use]
    pub const fn finish(&self) -> u32 {
        !self.state
    }
}

impl Default for Crc32 {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_published_check_value() {
        // Every CRC-32 implementation in the world agrees on this one, which is
        // the point of it. If this fails, the parameters are wrong rather than
        // the code, and every file this build writes is unreadable by anything
        // that computes the checksum correctly.
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn the_empty_input_is_zero() {
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn feeding_in_pieces_matches_feeding_all_at_once() {
        let data: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();
        let whole = crc32(&data);

        // The splits matter more than they used to. A piece shorter than
        // sixteen bytes goes down the leftover path entirely, and a split at
        // anything other than a multiple of sixteen leaves the second piece
        // starting out of step with the block the first one was walking. Both
        // have to land on the same answer as one call over the lot.
        for split in [0, 1, 7, 15, 16, 17, 31, 256, 257, 993, 999, 1000] {
            let mut hasher = Crc32::new();
            hasher.update(&data[..split]);
            hasher.update(&data[split..]);
            assert_eq!(hasher.finish(), whole, "split at {split}");
        }
    }

    /// One byte at a time, which is what this file did before it did sixteen.
    ///
    /// Kept as a test fixture because the check value pins down one input and
    /// the fast path has a leftover, a block boundary and four words inside a
    /// block to get wrong, none of which that one input exercises.
    fn a_byte_at_a_time(bytes: &[u8]) -> u32 {
        let mut state = !0u32;
        for &byte in bytes {
            let index = (state ^ u32::from(byte)) & 0xff;
            state = TABLE[index as usize] ^ (state >> 8);
        }
        !state
    }

    #[test]
    fn the_reference_agrees_with_the_published_check_value() {
        // Otherwise the test below compares two wrong things to each other.
        assert_eq!(a_byte_at_a_time(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn sixteen_bytes_at_a_time_agrees_with_one_at_a_time_at_every_length() {
        let data: Vec<u8> = (0..600u32)
            .map(|i| (i.wrapping_mul(37) % 253) as u8)
            .collect();
        for length in 0..data.len() {
            let slice = &data[..length];
            assert_eq!(crc32(slice), a_byte_at_a_time(slice), "length {length}");
        }
    }

    #[test]
    fn the_tables_are_the_serial_loop_run_ahead() {
        // Table n has to be what you get by pushing table n minus one through
        // one more byte of zeroes. If that ever stops holding, the fast path
        // and the leftover path disagree on inputs long enough to reach the
        // fast path and on nothing else, which is a bug that hides in exactly
        // the files too large to check by hand.
        for (step, pair) in TABLES.windows(2).enumerate() {
            for (index, (&previous, &entry)) in pair[0].iter().zip(&pair[1]).enumerate() {
                let expected = (previous >> 8) ^ TABLES[0][(previous & 0xff) as usize];
                assert_eq!(entry, expected, "table {} entry {index}", step + 1);
            }
        }
    }

    #[test]
    fn one_flipped_bit_changes_the_result() {
        let mut data = vec![0u8; 64];
        let clean = crc32(&data);
        for byte in 0..data.len() {
            for bit in 0..8 {
                data[byte] ^= 1 << bit;
                assert_ne!(crc32(&data), clean, "byte {byte} bit {bit}");
                data[byte] ^= 1 << bit;
            }
        }
    }

    #[test]
    fn trailing_zeroes_are_not_invisible() {
        // A checksum that misses appended zeroes is a checksum that misses a
        // truncated write padded back out to length, which is exactly the
        // corruption a power cut produces.
        assert_ne!(crc32(b"kura"), crc32(b"kura\0"));
        assert_ne!(crc32(b"kura\0"), crc32(b"kura\0\0"));
    }
}
