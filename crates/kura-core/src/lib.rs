//! Storage engine primitives.
//!
//! This crate holds the pieces every layer above it is built from: the integer
//! codecs that decide how big a segment is, the posting lists that decide how
//! fast a term lookup is, the bitmaps that carry a reader's visibility through a
//! query, and the vector storage that decides how much memory a corpus costs.
//!
//! Two properties run through all of them.
//!
//! Decoding never trusts its input. Every decoder takes a byte slice that could
//! have come from a truncated file, a different version of the format or a
//! corrupted disk, and returns an error rather than panicking or reading past
//! the end. There are no `unwrap` calls on a decode path.
//!
//! Nothing here allocates on a hot loop. Intersection, iteration and scoring all
//! work in buffers the caller owns, because the query path is where the time
//! goes and a growing vector in the middle of it is the usual reason a benchmark
//! stops scaling.

pub mod analysis;
pub mod bitmap;
pub mod bitpack;
pub mod bound;
pub mod checksum;
pub mod codec;
pub mod compact;
#[cfg(feature = "fs")]
pub mod durability;
pub mod error;
pub mod explain;
#[cfg(feature = "fs")]
pub mod file;
pub mod filter;
pub mod index;
#[cfg(feature = "fs")]
pub mod ingest;
pub mod keys;
pub mod length;
pub mod lz;
pub mod manifest;
#[cfg(feature = "fs")]
pub mod mapping;
pub mod migrate;
pub mod posting;
pub mod residency;
pub mod search;
pub mod segment;
pub mod store;
pub mod terms;
pub mod upsert;
pub mod vector;
pub mod wal;
pub mod xxh3;

pub use error::{Error, Result};

/// The internal identifier of a document inside one segment.
///
/// It is a segment local ordinal rather than a global identifier, which is what
/// keeps posting lists dense and delta encoding worthwhile. The mapping back to
/// the caller's own identifier lives one layer up.
pub type DocId = u32;

/// The version of this crate.
///
/// A host that links the engine and reports what it is running needs this, and
/// a version compiled in is the only one that cannot drift from the code beside
/// it. It is not the format version, which moves for its own reasons.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The version of the on disk format this build reads and writes.
///
/// It is checked in the header of every file. A file with a version this build
/// does not know is refused rather than parsed hopefully.
///
/// Version 2 moved the term dictionary. Version 1 searched the block index by
/// reading a term at every step of the binary search, and version 2 puts the
/// first four bytes of each block's first term in an array of their own in front
/// of the offsets and compares those instead. Nothing else about a file changed,
/// and [`migrate`] is what turns one into the other.
pub const FORMAT_VERSION: u16 = 2;

/// The oldest format version this build can still make sense of.
///
/// Nothing opens a file this old for reading. [`migrate`] is the only thing that
/// looks at one, and everything between this and [`FORMAT_VERSION`] is a step it
/// knows how to take. Dropping support for a version is moving this number, and
/// it should be moved on purpose rather than by a decoder quietly forgetting how
/// to read something.
pub const OLDEST_VERSION: u16 = 1;

/// The magic bytes at the start of every file written by this engine.
pub const MAGIC: [u8; 8] = *b"KURA\0\0\0\x01";
