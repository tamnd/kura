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

    /// A CRC-32 checksum did not match, so the bytes changed after they were
    /// written.
    ChecksumMismatch {
        /// The checksum stored in the file.
        stored: u32,
        /// The checksum computed over the bytes that were read.
        computed: u32,
    },

    /// An xxh3-128 checksum did not match, so the bytes changed after they were
    /// written.
    ///
    /// Separate from [`Error::ChecksumMismatch`] because the two cover different
    /// parts of the file with different algorithms, and the first question after
    /// a checksum failure is which check failed.
    Xxh3Mismatch {
        /// The checksum stored in the file.
        stored: u128,
        /// The checksum computed over the bytes that were read.
        computed: u128,
    },

    /// Neither manifest slot could be read, so the store has no committed state
    /// to open at.
    ///
    /// A store with one damaged slot opens on the other one, which is the whole
    /// point of there being two. Both failing at once means either the file was
    /// never a store or the damage reaches further than the manifest.
    NoManifest,

    /// A manifest claims more segments than one slot holds.
    TooManySegments {
        /// The count the manifest asked for.
        count: usize,
    },

    /// The superblock declares a page size this build does not use, so every
    /// structural offset in the file would be in the wrong place.
    UnsupportedPageSize {
        /// The page size found in the file.
        found: u32,
        /// The page size this build writes.
        expected: u32,
    },

    /// A log record's length field is not a length a record can have, so the
    /// position it was read from is not the start of a record.
    BadRecord {
        /// The length the record claimed.
        length: u32,
    },

    /// A record does not fit in what is left of the log ring.
    ///
    /// The caller flushes what the log already holds and truncates it, which
    /// frees space and lets the append through. A record larger than the whole
    /// ring never fits, and that is a configuration problem rather than a
    /// transient one.
    LogFull {
        /// How many bytes the record needs, including any padding to the end of
        /// the ring.
        needed: u64,
        /// How many bytes are free.
        free: u64,
    },

    /// The log head and tail from the manifest cannot describe a ring, because
    /// the tail is behind the head or there is more between them than the ring
    /// holds.
    BadPositions {
        /// How far the log has been consumed.
        head: u64,
        /// How far the log has been written.
        tail: u64,
    },

    /// The segments of one search hold more documents between them than a
    /// single numbering can address.
    ///
    /// A segment counts its own documents in 32 bits, and a search across
    /// segments numbers them one after another so that a page of results is one
    /// ordered list. The store that runs out of numbers has upwards of four
    /// billion documents in it and wants splitting rather than a wider integer.
    TooManyDocuments {
        /// How many documents the segments hold between them.
        count: u64,
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
            Self::Xxh3Mismatch { stored, computed } => {
                write!(
                    f,
                    "checksum mismatch: stored {stored:#034x}, computed {computed:#034x}"
                )
            }
            Self::NoManifest => f.write_str("neither manifest slot is readable"),
            Self::TooManySegments { count } => {
                write!(
                    f,
                    "manifest claims {count} segments, which is more than one slot holds"
                )
            }
            Self::UnsupportedPageSize { found, expected } => {
                write!(f, "page size {found} is not the {expected} this build uses")
            }
            Self::BadRecord { length } => {
                write!(f, "a log record cannot be {length} bytes long")
            }
            Self::LogFull { needed, free } => {
                write!(f, "log record needs {needed} bytes and {free} are free")
            }
            Self::BadPositions { head, tail } => {
                write!(f, "log head {head} and tail {tail} do not describe a ring")
            }
            Self::TooManyDocuments { count } => {
                write!(
                    f,
                    "these segments hold {count} documents between them, which is more than one \
                     search can number"
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
