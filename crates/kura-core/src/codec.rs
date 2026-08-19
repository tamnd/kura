//! Variable length integer codecs.
//!
//! Everything the engine writes to disk goes through here. The encoding is
//! LEB128: seven payload bits per byte, the top bit set while more bytes follow.
//! It is the same shape protobuf and DWARF use, it costs one byte for the values
//! that dominate a delta encoded posting list, and it needs no alignment.
//!
//! Signed values are zigzag mapped first, so that a small negative number costs
//! one byte instead of ten. That matters for column deltas, which are as often
//! negative as positive.

use crate::error::{Error, Result};

/// The largest number of bytes a `u64` can occupy.
pub const MAX_VARINT_LEN64: usize = 10;

/// The largest number of bytes a `u32` can occupy.
pub const MAX_VARINT_LEN32: usize = 5;

/// Appends `value` to `out` and returns how many bytes were written.
// Every cast below is preceded by a mask down to seven bits, or by the loop
// condition that left fewer than eight bits in the value, so none of them can
// truncate. The lint cannot see that, and rewriting it through `u8::try_from`
// would put a fallible conversion on the hottest encode path in the engine.
#[allow(clippy::cast_possible_truncation)]
pub fn put_uvarint(out: &mut Vec<u8>, mut value: u64) -> usize {
    let start = out.len();
    while value >= 0x80 {
        out.push(((value & 0x7f) as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
    out.len() - start
}

/// Reads a `u64` from the front of `input` and returns it with the rest of the
/// input.
///
/// # Errors
///
/// Returns [`Error::Truncated`] if the input ends inside the value, and
/// [`Error::Overflow`] if the value does not terminate within the ten bytes a
/// `u64` can occupy.
pub fn get_uvarint(input: &[u8]) -> Result<(u64, &[u8])> {
    let mut value: u64 = 0;
    let mut shift: u32 = 0;

    for (i, &byte) in input.iter().enumerate() {
        if i == MAX_VARINT_LEN64 {
            return Err(Error::Overflow);
        }
        // The tenth byte of a u64 can only carry one payload bit. Anything else
        // set there means the input is not a u64 this encoder produced.
        if i == MAX_VARINT_LEN64 - 1 && byte > 1 {
            return Err(Error::Overflow);
        }
        value |= u64::from(byte & 0x7f) << shift;
        if byte < 0x80 {
            let rest = input.get(i + 1..).unwrap_or(&[]);
            return Ok((value, rest));
        }
        shift += 7;
    }

    Err(Error::Truncated {
        needed: 1,
        available: 0,
    })
}

/// Appends a signed `value` to `out` using zigzag mapping.
pub fn put_ivarint(out: &mut Vec<u8>, value: i64) -> usize {
    put_uvarint(out, zigzag_encode(value))
}

/// Reads a zigzag mapped signed integer from the front of `input`.
///
/// # Errors
///
/// The same conditions as [`get_uvarint`].
pub fn get_ivarint(input: &[u8]) -> Result<(i64, &[u8])> {
    let (raw, rest) = get_uvarint(input)?;
    Ok((zigzag_decode(raw), rest))
}

/// Maps a signed integer onto an unsigned one so that values near zero stay
/// small whichever side of zero they are on.
// The two casts here reinterpret the bits rather than convert the value, which
// is the whole point of the mapping. Shifting a negative right by 63 gives all
// ones, which is the sign mask the exclusive or needs.
#[must_use]
#[allow(clippy::cast_sign_loss)]
pub const fn zigzag_encode(value: i64) -> u64 {
    ((value << 1) ^ (value >> 63)) as u64
}

/// The inverse of [`zigzag_encode`].
#[must_use]
#[allow(clippy::cast_possible_wrap)]
pub const fn zigzag_decode(value: u64) -> i64 {
    ((value >> 1) as i64) ^ -((value & 1) as i64)
}

/// Writes a little endian `u32`.
pub fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

/// Reads a little endian `u32` from the front of `input`.
///
/// # Errors
///
/// Returns [`Error::Truncated`] if fewer than four bytes are available.
pub fn get_u32(input: &[u8]) -> Result<(u32, &[u8])> {
    let (head, rest) = split_at(input, 4)?;
    let mut buf = [0u8; 4];
    buf.copy_from_slice(head);
    Ok((u32::from_le_bytes(buf), rest))
}

/// Splits `input` at `n`, or reports how much was missing.
///
/// # Errors
///
/// Returns [`Error::Truncated`] if `input` is shorter than `n`.
pub fn split_at(input: &[u8], n: usize) -> Result<(&[u8], &[u8])> {
    if input.len() < n {
        return Err(Error::Truncated {
            needed: n,
            available: input.len(),
        });
    }
    Ok(input.split_at(n))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_every_boundary() {
        // One value on each side of every byte width boundary, which is where a
        // shift is most likely to be off by one.
        let mut values = vec![0u64, 1, 127, 128, u64::MAX, u64::MAX - 1];
        for shift in 0..64 {
            values.push(1u64 << shift);
        }

        for value in values {
            let mut buf = Vec::new();
            let written = put_uvarint(&mut buf, value);
            assert_eq!(written, buf.len());
            assert!(written <= MAX_VARINT_LEN64);

            let (decoded, rest) = get_uvarint(&buf).expect("decode");
            assert_eq!(decoded, value, "value {value}");
            assert!(rest.is_empty());
        }
    }

    #[test]
    fn small_values_cost_one_byte() {
        for value in 0..128u64 {
            let mut buf = Vec::new();
            assert_eq!(put_uvarint(&mut buf, value), 1);
        }
    }

    #[test]
    fn signed_values_near_zero_stay_small() {
        for value in -63..=63i64 {
            let mut buf = Vec::new();
            assert_eq!(put_ivarint(&mut buf, value), 1, "value {value}");
            let (decoded, _) = get_ivarint(&buf).expect("decode");
            assert_eq!(decoded, value);
        }
    }

    #[test]
    fn signed_round_trips_the_extremes() {
        for value in [i64::MIN, i64::MIN + 1, -1, 0, 1, i64::MAX - 1, i64::MAX] {
            let mut buf = Vec::new();
            put_ivarint(&mut buf, value);
            let (decoded, _) = get_ivarint(&buf).expect("decode");
            assert_eq!(decoded, value);
        }
    }

    #[test]
    fn truncated_input_is_an_error_not_a_panic() {
        let mut buf = Vec::new();
        put_uvarint(&mut buf, u64::MAX);
        for len in 0..buf.len() {
            let err = get_uvarint(&buf[..len]).expect_err("short input should fail");
            assert!(matches!(err, Error::Truncated { .. }), "len {len}: {err:?}");
        }
    }

    #[test]
    fn a_value_that_never_terminates_is_rejected() {
        let never_ends = [0xffu8; 16];
        assert_eq!(get_uvarint(&never_ends), Err(Error::Overflow));
    }

    #[test]
    fn an_overlong_encoding_is_rejected() {
        // Ten bytes where the last carries more than the single bit a u64 has
        // left. Accepting this would give two encodings for one value, and a
        // format with two encodings for one value has a canonicalisation bug
        // waiting in it.
        let mut overlong = [0x80u8; 10];
        overlong[9] = 0x02;
        assert_eq!(get_uvarint(&overlong), Err(Error::Overflow));
    }

    #[test]
    fn decoding_leaves_the_rest_of_the_input_alone() {
        let mut buf = Vec::new();
        put_uvarint(&mut buf, 300);
        buf.extend_from_slice(b"tail");

        let (value, rest) = get_uvarint(&buf).expect("decode");
        assert_eq!(value, 300);
        assert_eq!(rest, b"tail");
    }

    #[test]
    fn u32_round_trips_and_reports_truncation() {
        let mut buf = Vec::new();
        put_u32(&mut buf, 0xdead_beef);
        let (value, rest) = get_u32(&buf).expect("decode");
        assert_eq!(value, 0xdead_beef);
        assert!(rest.is_empty());

        assert!(matches!(
            get_u32(&buf[..3]),
            Err(Error::Truncated {
                needed: 4,
                available: 3
            })
        ));
    }
}
