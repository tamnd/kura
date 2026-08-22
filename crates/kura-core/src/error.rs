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

    /// A log record is a kind the replay has no way to apply.
    ///
    /// Skipping it is not an option. The record is there because something
    /// promised a caller it would be applied, and a replay that walks past it
    /// leaves a store missing a write it said it had taken, with nothing to say
    /// so. A store written by a later build and opened by an earlier one is the
    /// way this happens, and the earlier one refusing to open is the only honest
    /// answer it has.
    UnknownRecord {
        /// The kind field of the record.
        kind: u32,
        /// Where in the log it was found, in ring positions rather than in file
        /// offsets.
        position: u64,
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

    /// A commit names an older set of deletions for a segment than the one that
    /// is already committed.
    ///
    /// Deletions only ever accumulate until a compaction rewrites the segment,
    /// so a generation that goes backwards means two writers are working from
    /// different ideas of what is deleted and one of them is about to bring
    /// documents back.
    StaleGeneration {
        /// Where the segment is in the file, which is what identifies it.
        offset: u64,
        /// The generation the committed manifest has.
        committed: u32,
        /// The generation the commit was asked to write.
        given: u32,
    },

    /// One commit was handed two sets of deletions for the same segment.
    ///
    /// A set of deletions is the whole answer for a segment rather than a change
    /// to it, so two of them for one segment is a caller that has not decided
    /// which documents are deleted. Taking the last would lose the other set
    /// quietly, and there is no order between them to take the union in.
    RepeatedSegment {
        /// Which segment of the manifest was named twice.
        at: usize,
    },

    /// A batch worked out what to delete from a view of the store that is no
    /// longer the committed one.
    ///
    /// A batch replacing documents holds a set of deletions per segment, and a
    /// set is the whole answer for its segment, so it is only right about the
    /// store it read. Committing it against a store that has moved since would
    /// undo whatever the commit in between deleted. The batch has to be built
    /// again on a view of where the store is now.
    StaleView {
        /// The epoch the batch read.
        read: u64,
        /// The epoch the store is at.
        committed: u64,
    },

    /// A set of deletions names a document the segment it belongs to does not
    /// have, so the two came from different builds of the same store.
    NoSuchDocument {
        /// The largest document the deletions name.
        doc: u32,
        /// How many documents the segment holds.
        documents: u32,
    },

    /// Two vectors of different lengths were compared, which is a caller bug
    /// rather than a data problem.
    DimensionMismatch {
        /// The length of the left operand.
        left: usize,
        /// The length of the right operand.
        right: usize,
    },

    /// A container said it holds a number of members and holds another, so the
    /// bytes and the header that describes them disagree.
    BadCardinality {
        /// What the header said.
        stated: usize,
        /// What the container actually holds.
        found: usize,
    },

    /// A container's offset in the header is not where that container is, so
    /// one of the two is wrong and there is no way to tell which.
    BadOffset {
        /// Where the header said the container starts.
        stated: usize,
        /// Where reading the containers in order arrived.
        found: usize,
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

    /// A segment does not end in a footer.
    ///
    /// Either the file is not a segment or the bytes that say how to check the
    /// rest of it are gone, and there is no way to tell those apart from here.
    BadFooter,

    /// The two ends of a segment disagree about how long its body is.
    ///
    /// The length is written twice because damage to one copy is invisible to a
    /// reader that only looks at the other, and a body that is longer or shorter
    /// than it should be is the one kind of damage that moves everything after
    /// it rather than changing it.
    BodyLengthMismatch {
        /// The length the header claims.
        header: u64,
        /// The length the footer claims.
        footer: u64,
    },

    /// A section's bytes are not the bytes its table entry was written for.
    ///
    /// Named rather than anonymous, because a digest per section exists to say
    /// which part of a file is damaged. Every other section of the same segment
    /// is still known good.
    SectionChecksumMismatch {
        /// The kind of the section the damage is in.
        kind: u16,
        /// The digest the section table holds.
        stored: u128,
        /// The digest of the bytes that are there.
        computed: u128,
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

    /// A segment holds a section that the pass rewriting it has no way to carry
    /// across. That is a segment written by a build which knows about something
    /// this one does not, and stopping is the point: the alternative is writing
    /// a replacement without it and calling the two the same data.
    UncarriedSection {
        /// The kind that has nowhere to go.
        kind: u16,
    },
}

impl fmt::Display for Error {
    #[expect(
        clippy::too_many_lines,
        reason = "one arm per variant, and splitting the match would hide which \
                  variants have a message from anyone adding one"
    )]
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
            Self::UnknownRecord { kind, position } => {
                write!(
                    f,
                    "the log holds a record of kind {kind} at {position} that this build \
                     cannot apply"
                )
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
            Self::StaleGeneration {
                offset,
                committed,
                given,
            } => {
                write!(
                    f,
                    "a commit puts the segment at {offset} back to tombstone \
                     generation {given} from {committed}"
                )
            }
            Self::RepeatedSegment { at } => {
                write!(f, "one commit names segment {at} twice")
            }
            Self::StaleView { read, committed } => {
                write!(
                    f,
                    "this batch was built on epoch {read} and the store is at {committed}"
                )
            }
            Self::NoSuchDocument { doc, documents } => {
                write!(
                    f,
                    "a deletion names document {doc} in a segment of {documents} documents"
                )
            }
            Self::DimensionMismatch { left, right } => {
                write!(f, "vectors of different lengths: {left} and {right}")
            }
            Self::NotSorted { at } => write!(f, "input is not ascending at {at}"),
            Self::BadCardinality { stated, found } => {
                write!(f, "a container of {stated} members holds {found}")
            }
            Self::BadOffset { stated, found } => {
                write!(
                    f,
                    "a container said it starts at {stated} and starts at {found}"
                )
            }
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
            Self::BadFooter => f.write_str("this segment does not end in a footer"),
            Self::BodyLengthMismatch { header, footer } => {
                write!(
                    f,
                    "the header says the body is {header} bytes and the footer says {footer}"
                )
            }
            Self::SectionChecksumMismatch {
                kind,
                stored,
                computed,
            } => {
                write!(
                    f,
                    "section kind {kind} does not match its checksum: stored {stored:#034x}, \
                     computed {computed:#034x}"
                )
            }
            Self::BadBlock => f.write_str("a compressed block does not decode to what it should"),
            Self::MissingSection { kind } => {
                write!(f, "the segment has no section of kind {kind}")
            }
            Self::UncarriedSection { kind } => {
                write!(f, "a section of kind {kind} cannot be carried across")
            }
        }
    }
}

impl core::error::Error for Error {}
