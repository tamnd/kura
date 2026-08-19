//! CRC-32, the IEEE polynomial that zlib, gzip and PNG use.
//!
//! The engine needs a checksum for one job: telling a reader that the bytes it
//! is about to trust are the bytes that were written. CRC-32 is the right size
//! for that. It is not a hash, it does not defend against anybody who wants to
//! change a file, and nothing here should ever be used as though it did.
//!
//! It is written out longhand rather than pulled from a crate because the core
//! crate has no dependencies and this is forty lines. The table is built at
//! compile time, so the cost at runtime is one lookup and one shift per byte.

/// The reflected form of the IEEE polynomial.
const POLYNOMIAL: u32 = 0xEDB8_8320;

/// The lookup table, one entry per byte value.
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

static TABLE: [u32; 256] = build_table();

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
    pub fn update(&mut self, bytes: &[u8]) {
        let mut state = self.state;
        for &byte in bytes {
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

        for split in [0, 1, 7, 256, 999, 1000] {
            let mut hasher = Crc32::new();
            hasher.update(&data[..split]);
            hasher.update(&data[split..]);
            assert_eq!(hasher.finish(), whole, "split at {split}");
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
