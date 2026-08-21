//! How long a document is, in one byte.
//!
//! Scoring divides by `k1 * (1 - b + b * len / average)`, so the length of the
//! document is read once for every document that gets a score. The lengths are
//! four bytes each, which means a walk over a block of a hundred and twenty
//! eight postings reaches into half a kilobyte of a section it touches nothing
//! else in, and the documents in a block are spread out rather than next to
//! each other, so it is half a kilobyte spread over more.
//!
//! Four bytes is far more than the number deserves. The length only matters
//! through that division, and a length that is a few percent out moves a score
//! by a fraction of a percent, which is well inside the ordering noise that
//! floating point arithmetic has anyway. So a byte, and a quarter of the bytes
//! to reach through.
//!
//! Whether that is faster depends on whether the four byte lengths were going
//! to be in cache anyway. On a corpus small enough that they are, the byte is a
//! shade slower, because reading a byte and then reading its normalisation out
//! of a table is two loads where reading a length was one. On a corpus large
//! enough that they are not, and for a term whose documents are spread out
//! rather than consecutive, it is faster by a lot, because a quarter of the
//! bytes is a quarter of the lines and the lengths of a block stop being a
//! cache miss each. The measurement that says where the crossover is lives in
//! the commit that added this module.
//!
//! # What the byte holds
//!
//! Small documents are exact. A length under thirty two is stored as itself,
//! which covers titles, log lines, chat messages and anything else short enough
//! that a percentage error would be a whole word.
//!
//! Everything else is a five bit mantissa and a four bit exponent, which is a
//! floating point number with no sign and no room for a fraction. The value is
//! `m << e` for `m` in sixteen to thirty one and `e` in one to fourteen, so
//! consecutive codes are six percent apart at worst. The one pairing that would
//! have come out as the top byte is given up to [`ESCAPE`], so the largest
//! length that fits is four hundred and ninety one thousand five hundred and
//! twenty words.
//!
//! # Rounding up
//!
//! [`round`] rounds up, never down, so the length that comes back is at least
//! the length that went in. That direction is deliberate. A longer document is
//! a smaller score, so a length that is rounded up can only lower a score, and a
//! ceiling worked out from exact lengths still bounds a score worked out from
//! rounded ones. Rounding down would break that, quietly, by letting a document
//! score above a ceiling that a walk had already decided to skip.
//!
//! # The document that does not fit
//!
//! There is no code above the top one, so a document longer than the longest
//! length any code holds has nowhere to round up to. Clamping it to the top code
//! would be rounding down, and rounding down is the direction that inflates a
//! score. It is not a theoretical worry: the first corpus this was measured on
//! held a novel concatenated with itself as compression test data, and clamping
//! its length lifted its score by nearly a quarter, which put it on pages it had
//! no business being on.
//!
//! So the top code, [`ESCAPE`], is not a length at all. It means the document is
//! longer than a byte can say and the four byte length beside it is the one to
//! read. That costs a branch on a corpus that has such a document and nothing on
//! one that does not, and it makes the rounding upward with no exception:
//! `of(round(n)) >= n` for every `n` there is.

/// The largest length that has a code of its own.
///
/// Four hundred and ninety one thousand five hundred and twenty words is around
/// three megabytes of prose, which is longer than most books and shorter than
/// some. A document past it gets [`ESCAPE`] instead.
pub const LONGEST: u32 = 30 << 14;

/// The code that means the length did not fit and the exact one is the one to
/// read.
///
/// [`of`] gives it back as [`u32::MAX`], which is the largest thing it could
/// mean, so a caller that never looks at the code still rounds upward and still
/// gets a bound. A caller that ranks by the answer should look.
pub const ESCAPE: u8 = u8::MAX;

/// Lengths below this are stored exactly.
const EXACT: u32 = 32;

/// Rounds a length to the byte that holds it.
///
/// Never down. A length past [`LONGEST`] comes back as [`ESCAPE`], which is not
/// a length and is not to be read as one.
#[must_use]
pub const fn round(length: u32) -> u8 {
    if length < EXACT {
        #[expect(
            clippy::cast_possible_truncation,
            reason = "under thirty two, which is a byte"
        )]
        return length as u8;
    }
    if length > LONGEST {
        return ESCAPE;
    }
    // Five bits of mantissa, so the exponent is however far the top bit is
    // above the fifth one.
    let mut exponent = length.ilog2() - 4;
    // Rounding up, which is what keeps this a bound rather than an estimate.
    let step = 1u32 << exponent;
    let mut mantissa = length.div_ceil(step);
    // A length just under a power of two rounds up into the next exponent.
    if mantissa == EXACT {
        mantissa = 16;
        exponent += 1;
    }
    #[expect(
        clippy::cast_possible_truncation,
        reason = "the exponent is one to fourteen and the mantissa sixteen to \
                  thirty one, so this is thirty two to two hundred and fifty five"
    )]
    let code = (EXACT + (exponent - 1) * 16 + (mantissa - 16)) as u8;
    code
}

/// The length a byte stands for.
///
/// [`u32::MAX`] for [`ESCAPE`], which stands for no length at all. See the
/// module documentation for what to do about it.
#[must_use]
pub const fn of(code: u8) -> u32 {
    if code == ESCAPE {
        return u32::MAX;
    }
    let code = code as u32;
    if code < EXACT {
        return code;
    }
    let above = code - EXACT;
    let exponent = above / 16 + 1;
    let mantissa = above % 16 + 16;
    mantissa << exponent
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_document_keeps_its_length_to_the_word() {
        for length in 0..EXACT {
            assert_eq!(of(round(length)), length, "at {length}");
        }
    }

    #[test]
    fn rounding_a_length_never_shortens_it() {
        // Every length up to a hundred thousand, then the powers of two and
        // their neighbours out to the top, which is where the carry from a full
        // mantissa into the next exponent lives, and then the far end, where
        // the escape is the only honest answer.
        for length in 0..100_000u32 {
            let back = of(round(length));
            assert!(back >= length, "{length} came back as {back}");
        }
        for bit in 5..32u32 {
            for length in [(1u32 << bit) - 1, 1u32 << bit, (1u32 << bit) + 1] {
                let back = of(round(length));
                assert!(back >= length, "{length} came back as {back}");
            }
        }
        for length in [LONGEST, LONGEST + 1, u32::MAX - 1, u32::MAX] {
            let back = of(round(length));
            assert!(back >= length, "{length} came back as {back}");
        }
    }

    #[test]
    fn rounding_a_length_never_adds_more_than_a_sixteenth() {
        for length in EXACT..200_000u32 {
            let back = of(round(length));
            assert!(
                back <= length + length / 16 + 1,
                "{length} came back as {back}"
            );
        }
    }

    #[test]
    fn the_codes_read_back_in_the_order_they_were_written() {
        // The walk that reads these compares them to each other in places, so a
        // larger code has to mean a longer document with no exceptions.
        let mut last = 0;
        for code in 0..=u8::MAX {
            let length = of(code);
            assert!(
                code == 0 || length > last,
                "code {code} is {length} after {last}"
            );
            last = length;
        }
    }

    #[test]
    fn a_document_longer_than_the_top_code_escapes_rather_than_clamps() {
        // The bug this exists to stop is the clamp. A length that came back as
        // LONGEST would be a length rounded down, and a length rounded down is
        // a score rounded up, on the very documents the length norm is there to
        // hold back.
        assert_eq!(round(LONGEST), 254);
        assert_eq!(of(254), LONGEST);
        assert_eq!(round(LONGEST + 1), ESCAPE);
        assert_eq!(round(u32::MAX), ESCAPE);
        assert_eq!(of(ESCAPE), u32::MAX);
    }

    #[test]
    fn every_code_is_the_code_its_own_length_rounds_to() {
        // A round trip the other way round, which is what says the two
        // functions are inverses rather than merely close.
        for code in 0..=u8::MAX {
            assert_eq!(round(of(code)), code, "at code {code}");
        }
        // And every code below the escape is a length, so none of them is the
        // escape by accident.
        for code in 0..u8::MAX {
            assert!(of(code) <= LONGEST, "code {code} is past the top length");
        }
    }
}
