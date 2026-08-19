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

pub mod bitmap;
pub mod codec;
pub mod error;
pub mod posting;
pub mod vector;

pub use error::{Error, Result};

/// The internal identifier of a document inside one segment.
///
/// It is a segment local ordinal rather than a global identifier, which is what
/// keeps posting lists dense and delta encoding worthwhile. The mapping back to
/// the caller's own identifier lives one layer up.
pub type DocId = u32;

/// The version of the on disk format this build reads and writes.
///
/// It is checked in the header of every file. A file with a version this build
/// does not know is refused rather than parsed hopefully.
pub const FORMAT_VERSION: u16 = 1;

/// The magic bytes at the start of every file written by this engine.
pub const MAGIC: [u8; 8] = *b"KURA\0\0\0\x01";
