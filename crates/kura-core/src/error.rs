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

    /// A posting list was not in ascending order, which every decoder and every
    /// intersection in this crate relies on.
    NotSorted {
        /// The value that broke the order.
        at: u32,
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
            Self::NotSorted { at } => write!(f, "posting list is not ascending at {at}"),
        }
    }
}

impl core::error::Error for Error {}
