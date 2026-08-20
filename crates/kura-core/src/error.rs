//! The error type shared by every decoder in the crate.

use core::fmt;

/// The result of an operation that reads or writes engine data.
pub type Result<T> = core::result::Result<T, Error>;

/// What went wrong.
///
/// The variants are deliberately about the shape of the data rather than about
/// the caller's intent. A caller that gets [`Error::Truncated`] knows the file
/// is short, which is actionable, where a single opaque "invalid data" is not.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// The input ended in the middle of a value.
    Truncated {
        /// How many bytes the decoder still needed.
        needed: usize,
        /// How many bytes were left.
        available: usize,
    },

    /// A variable length integer did not terminate within the bytes its width
    /// allows, which means the input is not what it claims to be.
    Overflow,

    /// The magic bytes at the start of the input are not this engine's.
    BadMagic,

    /// The format version in the header is one this build does not read.
    UnsupportedVersion {
        /// The version found in the file.
        found: u16,
        /// The version this build writes.
        expected: u16,
    },

    /// A checksum did not match, so the bytes changed after they were written.
    ChecksumMismatch {
        /// The checksum stored in the file.
        stored: u32,
        /// The checksum computed over the bytes that were read.
        computed: u32,
    },

    /// Two vectors of different lengths were compared, which is a caller bug
    /// rather than a data problem.
    DimensionMismatch {
        /// The length of the left operand.
        left: usize,
        /// The length of the right operand.
        right: usize,
    },

    /// Input that has to be ascending was not, which every decoder, every
    /// intersection and every binary search in this crate relies on.
    NotSorted {
        /// Where the order broke, as a value for a posting list and as a
        /// position for anything the caller pushes in sequence.
        at: u32,
    },

    /// A term claimed to share more of the term before it than that term has,
    /// so the block it is in cannot be reconstructed.
    BadPrefix {
        /// How many bytes the entry said it shares.
        shared: usize,
        /// How many bytes the term before it has.
        available: usize,
    },

    /// Two sections of a segment claimed the same kind, so a reader asking for
    /// that kind would have to guess which one was meant.
    DuplicateSection {
        /// The kind that appeared twice.
        kind: u16,
    },

    /// A segment's section table claims more sections than the format allows.
    TooManySections {
        /// The count found in the header.
        count: usize,
    },

    /// A section's offset and length do not lie inside the segment, which means
    /// the table is corrupt however plausible the individual numbers look.
    SectionOutOfRange {
        /// The kind of the offending section.
        kind: u16,
        /// Where the table said the section starts.
        offset: u64,
        /// How long the table said the section is.
        length: u64,
    },

    /// A compressed block did not decode to what its container said it holds,
    /// which means the bytes are not the bytes that were written however well
    /// formed the sequences in them look.
    BadBlock,

    /// A segment is missing a section that the thing being read out of it
    /// cannot do without, which is a different fact from the segment being
    /// short or corrupt: the file is intact and holds something else.
    MissingSection {
        /// The kind that was needed and is not there.
        kind: u16,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated { needed, available } => {
                write!(
                    f,
                    "input ended early: needed {needed} bytes, {available} left"
                )
            }
            Self::Overflow => f.write_str("variable length integer did not terminate"),
            Self::BadMagic => f.write_str("not a kura file"),
            Self::UnsupportedVersion { found, expected } => {
                write!(
                    f,
                    "format version {found} is not readable by this build, which writes {expected}"
                )
            }
            Self::ChecksumMismatch { stored, computed } => {
                write!(
                    f,
                    "checksum mismatch: stored {stored:#010x}, computed {computed:#010x}"
                )
            }
            Self::DimensionMismatch { left, right } => {
                write!(f, "vectors of different lengths: {left} and {right}")
            }
            Self::NotSorted { at } => write!(f, "input is not ascending at {at}"),
            Self::BadPrefix { shared, available } => {
                write!(
                    f,
                    "term shares {shared} bytes with a term that is only {available} long"
                )
            }
            Self::DuplicateSection { kind } => {
                write!(f, "section kind {kind} appears more than once")
            }
            Self::TooManySections { count } => {
                write!(
                    f,
                    "segment claims {count} sections, which is more than the format allows"
                )
            }
            Self::SectionOutOfRange {
                kind,
                offset,
                length,
            } => {
                write!(
                    f,
                    "section kind {kind} claims bytes {offset}..{} which are not in the segment",
                    offset.saturating_add(*length)
                )
            }
            Self::BadBlock => f.write_str("a compressed block does not decode to what it should"),
            Self::MissingSection { kind } => {
                write!(f, "the segment has no section of kind {kind}")
            }
        }
    }
}

impl core::error::Error for Error {}
