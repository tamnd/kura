//! The segment container.
//!
//! A segment is the unit the engine writes, publishes and deletes. Everything
//! that ends up on disk ends up inside one, and a segment is immutable once it
//! has been written: a change is a new segment, and a delete is a tombstone in a
//! later one. That is what makes a reader that already opened a segment safe
//! from a writer that is still working, without a lock between them.
//!
//! The container itself knows nothing about what it carries. It holds a header,
//! a table of sections, and the section payloads, and it hands a caller a byte
//! slice per section. The term dictionary, the posting lists, the stored fields,
//! the vectors and the access control lists each decode their own section, so a
//! change to any of those formats is not a change to this one.
//!
//! # Layout
//!
//! ```text
//! header, 32 bytes
//!   0..8    magic, the same eight bytes in every file this engine writes
//!   8..10   format version, u16
//!   10..12  section count, u16
//!   12..16  checksum of the body, u32
//!   16..24  body length, u64
//!   24..32  reserved, written as zero and ignored on read
//!
//! body
//!   section table, 24 bytes per entry
//!     0..2    kind, u16
//!     2..4    flags, u16, reserved
//!     4..8    padding, u32
//!     8..16   offset from the start of the body, u64
//!     16..24  length, u64
//!   section payloads, in the order the writer added them
//! ```
//!
//! # What the reader refuses
//!
//! Opening a segment is where the engine decides whether to trust a file, so it
//! is deliberately unforgiving. Wrong magic, a version this build does not
//! write, a body that is shorter than the header says, a section table that runs
//! past the end, a section whose bytes lie outside the body, two sections of the
//! same kind, or a checksum that does not match, are all errors with their own
//! variant rather than one opaque failure.
//!
//! An unknown *section kind* is the one thing that is not an error. A reader
//! skips a section it does not recognise, which is what lets a later build add a
//! section without making every file it writes unreadable by this one. Version
//! is a refusal, kind is a shrug, and the difference is the whole forward
//! compatibility story.

use crate::checksum::{Crc32, crc32};
use crate::codec::{get_u16, get_u32, get_u64, put_u16, put_u32, put_u64, split_at};
use crate::error::{Error, Result};
use crate::{FORMAT_VERSION, MAGIC};

/// The fixed size of a segment header.
pub const HEADER_LEN: usize = 32;

/// The size of one entry in the section table.
const ENTRY_LEN: usize = 24;

/// The most sections one segment can hold, from the width of the count field.
pub const MAX_SECTIONS: usize = u16::MAX as usize;

/// The section kinds this build knows about.
///
/// They are plain constants rather than an enum because an unknown kind has to
/// survive a round trip through a reader that has never heard of it, and an enum
/// would make that a parse error instead of a skip.
pub mod kind {
    /// The term dictionary: terms and where their posting lists start.
    pub const TERMS: u16 = 1;
    /// Delta encoded posting lists, one per term.
    pub const POSTINGS: u16 = 2;
    /// Stored field values, returned with a hit rather than searched.
    pub const FIELDS: u16 = 3;
    /// Quantised vectors, one per passage.
    pub const VECTORS: u16 = 4;
    /// The access control lists that govern the documents in this segment.
    pub const ACL: u16 = 5;
    /// Columnar values, for filters and facets.
    pub const COLUMNS: u16 = 6;
    /// Entities and edges.
    pub const GRAPH: u16 = 7;
    /// Documents deleted by a later segment.
    pub const TOMBSTONES: u16 = 8;
}

/// Builds a segment.
///
/// Sections are added in any order and are written in the order they were
/// added. The writer owns each payload, because a segment is assembled once and
/// then read many times, and borrowing the pieces would push the lifetime
/// problem onto every caller for no gain.
#[derive(Debug, Default)]
pub struct Writer {
    sections: Vec<(u16, Vec<u8>)>,
}

impl Writer {
    /// Starts an empty segment.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds one section.
    ///
    /// # Errors
    ///
    /// Returns [`Error::DuplicateSection`] if a section of that kind was already
    /// added, and [`Error::TooManySections`] past [`MAX_SECTIONS`].
    pub fn add(&mut self, kind: u16, payload: Vec<u8>) -> Result<()> {
        if self.sections.iter().any(|(existing, _)| *existing == kind) {
            return Err(Error::DuplicateSection { kind });
        }
        if self.sections.len() >= MAX_SECTIONS {
            return Err(Error::TooManySections {
                count: self.sections.len() + 1,
            });
        }
        self.sections.push((kind, payload));
        Ok(())
    }

    /// Reports whether any section has been added.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sections.is_empty()
    }

    /// Writes the segment out.
    ///
    /// The header is written last, over a reservation made at the start, so that
    /// the checksum covers a body that is already complete rather than one that
    /// is still being appended to.
    ///
    /// # Panics
    ///
    /// If the section count does not fit the header field, which [`Writer::add`]
    /// makes impossible. The alternative is writing a header that disagrees with
    /// the body and still passes its own checksum.
    #[must_use]
    pub fn finish(self) -> Vec<u8> {
        let table_len = self.sections.len() * ENTRY_LEN;
        let payload_len: usize = self.sections.iter().map(|(_, bytes)| bytes.len()).sum();

        let mut out = Vec::with_capacity(HEADER_LEN + table_len + payload_len);
        out.resize(HEADER_LEN, 0);

        // The table has to be written before the payloads and needs the offsets
        // the payloads will land at, so walk them once to work the offsets out.
        let mut offset = table_len as u64;
        for (kind, payload) in &self.sections {
            put_u16(&mut out, *kind);
            put_u16(&mut out, 0); // flags
            put_u32(&mut out, 0); // padding, keeps the entry eight byte aligned
            put_u64(&mut out, offset);
            put_u64(&mut out, payload.len() as u64);
            offset += payload.len() as u64;
        }
        for (_, payload) in &self.sections {
            out.extend_from_slice(payload);
        }

        let body_len = out.len() - HEADER_LEN;
        let mut hasher = Crc32::new();
        hasher.update(&out[HEADER_LEN..]);

        // The count fits because add refuses to go past MAX_SECTIONS, which is
        // exactly the range of the field. Writing a wrong count here would give
        // a file that passes its own checksum and still decodes to the wrong
        // thing, so this is one of the few places worth failing loudly.
        let count = u16::try_from(self.sections.len()).expect("add bounds the section count");

        let mut header = Vec::with_capacity(HEADER_LEN);
        header.extend_from_slice(&MAGIC);
        put_u16(&mut header, FORMAT_VERSION);
        put_u16(&mut header, count);
        put_u32(&mut header, hasher.finish());
        put_u64(&mut header, body_len as u64);
        header.resize(HEADER_LEN, 0); // the reserved tail
        out[..HEADER_LEN].copy_from_slice(&header);

        out
    }
}

/// A segment opened for reading.
///
/// Opening borrows the bytes and allocates nothing, so a memory mapped file of
/// any size opens in the time it takes to walk the section table. Every section
/// accessor hands back a sub slice of the same borrow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segment<'a> {
    version: u16,
    count: usize,
    /// The section table, still encoded. Entries are read in place.
    table: &'a [u8],
    /// Everything after the header, which is what the offsets are relative to.
    body: &'a [u8],
}

impl<'a> Segment<'a> {
    /// Opens a segment and verifies it completely, checksum included.
    ///
    /// This is the one to use unless there is a measured reason not to.
    ///
    /// # Errors
    ///
    /// Every way a segment can be wrong has its own variant. See the module
    /// documentation for the list.
    pub fn open(bytes: &'a [u8]) -> Result<Self> {
        let segment = Self::open_without_checksum(bytes)?;
        let stored = stored_checksum(bytes)?;
        let computed = crc32(segment.body);
        if stored != computed {
            return Err(Error::ChecksumMismatch { stored, computed });
        }
        Ok(segment)
    }

    /// Opens a segment and verifies its structure but not its contents.
    ///
    /// The checksum is the only check skipped, and skipping it costs a read of
    /// the whole file at open time. That is worth doing for a large segment that
    /// was verified when it was published and has not been touched since, and it
    /// is not worth doing for a file that arrived from somewhere else. The
    /// structural checks still hold, so a section slice returned by this segment
    /// is still inside the input.
    ///
    /// # Errors
    ///
    /// As [`Segment::open`], minus [`Error::ChecksumMismatch`].
    pub fn open_without_checksum(bytes: &'a [u8]) -> Result<Self> {
        let (header, rest) = split_at(bytes, HEADER_LEN)?;

        let (magic, header) = split_at(header, MAGIC.len())?;
        if magic != MAGIC {
            return Err(Error::BadMagic);
        }
        let (version, header) = get_u16(header)?;
        if version != FORMAT_VERSION {
            return Err(Error::UnsupportedVersion {
                found: version,
                expected: FORMAT_VERSION,
            });
        }
        let (count, header) = get_u16(header)?;
        let (_checksum, header) = get_u32(header)?;
        let (body_len, _) = get_u64(header)?;

        // The body length is a claim by the file about itself, so it is checked
        // against the bytes actually present rather than used to size anything.
        let body_len = usize::try_from(body_len).map_err(|_| Error::Truncated {
            needed: usize::MAX,
            available: rest.len(),
        })?;
        if rest.len() < body_len {
            return Err(Error::Truncated {
                needed: body_len,
                available: rest.len(),
            });
        }
        let body = &rest[..body_len];

        let count = usize::from(count);
        let table_len = count
            .checked_mul(ENTRY_LEN)
            .ok_or(Error::TooManySections { count })?;
        let (table, _) = split_at(body, table_len)?;

        let segment = Self {
            version,
            count,
            table,
            body,
        };
        segment.validate_table()?;
        Ok(segment)
    }

    /// Checks every entry before any caller can ask for a section, so that a
    /// corrupt table fails once at open rather than at whichever access happens
    /// to touch the bad entry.
    fn validate_table(&self) -> Result<()> {
        let table_len = self.count * ENTRY_LEN;

        // One bit per possible kind, on the stack. Comparing every entry against
        // every earlier one would be four billion comparisons for a table this
        // format allows, which is a denial of service anyone could write into a
        // file. Eight kilobytes of stack turns it into a single pass.
        let mut seen = [0u64; (u16::MAX as usize + 1) / 64];

        for index in 0..self.count {
            let (kind, offset, length) = self.entry(index).ok_or(Error::Truncated {
                needed: table_len,
                available: self.table.len(),
            })?;

            let start = usize::try_from(offset).ok();
            let end =
                start.and_then(|s| usize::try_from(length).ok().and_then(|l| s.checked_add(l)));
            let fits = match (start, end) {
                (Some(start), Some(end)) => start >= table_len && end <= self.body.len(),
                _ => false,
            };
            if !fits {
                return Err(Error::SectionOutOfRange {
                    kind,
                    offset,
                    length,
                });
            }

            // Duplicates are rejected on write, but a file can arrive from
            // anywhere, and a reader that silently picks the first of two
            // sections is a reader whose answer depends on write order.
            let word = usize::from(kind) / 64;
            let bit = 1u64 << (u32::from(kind) % 64);
            if seen[word] & bit != 0 {
                return Err(Error::DuplicateSection { kind });
            }
            seen[word] |= bit;
        }
        Ok(())
    }

    /// Reads one table entry in place.
    fn entry(&self, index: usize) -> Option<(u16, u64, u64)> {
        let start = index.checked_mul(ENTRY_LEN)?;
        let bytes = self.table.get(start..start.checked_add(ENTRY_LEN)?)?;
        let (kind, rest) = get_u16(bytes).ok()?;
        let (_flags, rest) = get_u16(rest).ok()?;
        let (_padding, rest) = get_u32(rest).ok()?;
        let (offset, rest) = get_u64(rest).ok()?;
        let (length, _) = get_u64(rest).ok()?;
        Some((kind, offset, length))
    }

    /// The format version in the header, which is always [`FORMAT_VERSION`] for
    /// a segment this build was willing to open.
    #[must_use]
    pub const fn version(&self) -> u16 {
        self.version
    }

    /// How many sections the segment holds, including kinds this build does not
    /// know about.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.count
    }

    /// Reports whether the segment holds no sections at all.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Returns the bytes of one section, or `None` if the segment does not carry
    /// that kind.
    #[must_use]
    pub fn section(&self, kind: u16) -> Option<&'a [u8]> {
        for index in 0..self.count {
            let (found, offset, length) = self.entry(index)?;
            if found != kind {
                continue;
            }
            // validate_table already proved these convert and fit.
            let start = usize::try_from(offset).ok()?;
            let end = start.checked_add(usize::try_from(length).ok()?)?;
            return self.body.get(start..end);
        }
        None
    }

    /// Iterates the section kinds present, in the order they were written.
    pub fn kinds(&self) -> impl Iterator<Item = u16> + '_ {
        (0..self.count).filter_map(move |index| self.entry(index).map(|(kind, _, _)| kind))
    }
}

/// Reads the checksum field out of a header that has already been length
/// checked by the caller.
fn stored_checksum(bytes: &[u8]) -> Result<u32> {
    let (header, _) = split_at(bytes, HEADER_LEN)?;
    let (_, rest) = split_at(header, MAGIC.len() + 2 + 2)?;
    let (checksum, _) = get_u32(rest)?;
    Ok(checksum)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<u8> {
        let mut writer = Writer::new();
        writer
            .add(kind::TERMS, b"the term dictionary".to_vec())
            .expect("add");
        writer.add(kind::POSTINGS, vec![7u8; 300]).expect("add");
        writer
            .add(kind::ACL, b"who may read this".to_vec())
            .expect("add");
        writer.finish()
    }

    #[test]
    fn round_trips_every_section() {
        let bytes = sample();
        let segment = Segment::open(&bytes).expect("open");

        assert_eq!(segment.version(), FORMAT_VERSION);
        assert_eq!(segment.len(), 3);
        assert!(!segment.is_empty());
        assert_eq!(
            segment.section(kind::TERMS),
            Some(&b"the term dictionary"[..])
        );
        assert_eq!(segment.section(kind::POSTINGS), Some(&[7u8; 300][..]));
        assert_eq!(segment.section(kind::ACL), Some(&b"who may read this"[..]));
        assert_eq!(segment.section(kind::VECTORS), None);
        assert_eq!(
            segment.kinds().collect::<Vec<_>>(),
            vec![kind::TERMS, kind::POSTINGS, kind::ACL]
        );
    }

    #[test]
    fn an_empty_segment_is_valid() {
        let bytes = Writer::new().finish();
        let segment = Segment::open(&bytes).expect("open");
        assert!(segment.is_empty());
        assert_eq!(segment.len(), 0);
        assert_eq!(segment.section(kind::TERMS), None);
        assert_eq!(segment.kinds().count(), 0);
    }

    #[test]
    fn an_empty_section_is_not_a_missing_one() {
        // The difference matters: a term dictionary with no terms is a fact
        // about the segment, and a missing dictionary is a fact about the build
        // that wrote it.
        let mut writer = Writer::new();
        writer.add(kind::TERMS, Vec::new()).expect("add");
        let bytes = writer.finish();

        let segment = Segment::open(&bytes).expect("open");
        assert_eq!(segment.section(kind::TERMS), Some(&[][..]));
        assert_eq!(segment.section(kind::POSTINGS), None);
    }

    #[test]
    fn a_section_kind_this_build_does_not_know_is_skipped_not_refused() {
        let mut writer = Writer::new();
        writer.add(kind::TERMS, b"known".to_vec()).expect("add");
        writer
            .add(40_000, b"from a later build".to_vec())
            .expect("add");
        let bytes = writer.finish();

        let segment = Segment::open(&bytes).expect("an unknown kind must still open");
        assert_eq!(segment.section(kind::TERMS), Some(&b"known"[..]));
        assert_eq!(segment.section(40_000), Some(&b"from a later build"[..]));
        assert_eq!(segment.len(), 2);
    }

    #[test]
    fn a_duplicate_section_is_refused_on_write() {
        let mut writer = Writer::new();
        writer.add(kind::TERMS, b"first".to_vec()).expect("add");
        assert_eq!(
            writer.add(kind::TERMS, b"second".to_vec()),
            Err(Error::DuplicateSection { kind: kind::TERMS })
        );
    }

    #[test]
    fn a_duplicate_section_is_refused_on_read() {
        // Hand build a table with the same kind twice, which is what a file from
        // a broken writer somewhere else would look like.
        let mut body = Vec::new();
        for _ in 0..2 {
            put_u16(&mut body, kind::TERMS);
            put_u16(&mut body, 0);
            put_u32(&mut body, 0);
            put_u64(&mut body, (2 * ENTRY_LEN) as u64);
            put_u64(&mut body, 1);
        }
        body.push(b'x');

        let bytes = wrap(&body, 2);
        assert_eq!(
            Segment::open(&bytes),
            Err(Error::DuplicateSection { kind: kind::TERMS })
        );
    }

    #[test]
    fn bad_magic_is_refused() {
        let mut bytes = sample();
        bytes[0] = b'X';
        assert_eq!(Segment::open(&bytes), Err(Error::BadMagic));
    }

    #[test]
    fn an_unknown_version_is_refused() {
        let mut bytes = sample();
        bytes[8..10].copy_from_slice(&(FORMAT_VERSION + 1).to_le_bytes());
        assert_eq!(
            Segment::open(&bytes),
            Err(Error::UnsupportedVersion {
                found: FORMAT_VERSION + 1,
                expected: FORMAT_VERSION,
            })
        );
    }

    #[test]
    fn a_changed_byte_anywhere_in_the_body_is_caught() {
        let clean = sample();
        for index in HEADER_LEN..clean.len() {
            let mut bytes = clean.clone();
            bytes[index] ^= 0xff;
            let err = Segment::open(&bytes).expect_err("a corrupt body must not open");
            assert!(
                matches!(
                    err,
                    Error::ChecksumMismatch { .. }
                        | Error::SectionOutOfRange { .. }
                        | Error::DuplicateSection { .. }
                        | Error::Truncated { .. }
                ),
                "byte {index}: {err:?}"
            );
        }
    }

    #[test]
    fn truncating_at_every_length_is_an_error_rather_than_a_panic() {
        // Short is short at every prefix, whether the cut lands in the header,
        // in the section table or in a payload, and the answer is the same error
        // in all three cases rather than a panic in one of them.
        let clean = sample();
        for len in 0..clean.len() {
            let err = Segment::open(&clean[..len]).expect_err("a short segment must not open");
            assert!(matches!(err, Error::Truncated { .. }), "len {len}: {err:?}");

            let err = Segment::open_without_checksum(&clean[..len])
                .expect_err("a short segment must not open");
            assert!(matches!(err, Error::Truncated { .. }), "len {len}: {err:?}");
        }
    }

    #[test]
    fn a_section_that_points_outside_the_segment_is_refused() {
        let mut body = Vec::new();
        put_u16(&mut body, kind::TERMS);
        put_u16(&mut body, 0);
        put_u32(&mut body, 0);
        put_u64(&mut body, ENTRY_LEN as u64);
        put_u64(&mut body, u64::MAX); // a length no allocation should be sized from

        let bytes = wrap(&body, 1);
        assert_eq!(
            Segment::open(&bytes),
            Err(Error::SectionOutOfRange {
                kind: kind::TERMS,
                offset: ENTRY_LEN as u64,
                length: u64::MAX,
            })
        );
    }

    #[test]
    fn a_section_that_overlaps_the_table_is_refused() {
        let mut body = Vec::new();
        put_u16(&mut body, kind::TERMS);
        put_u16(&mut body, 0);
        put_u32(&mut body, 0);
        put_u64(&mut body, 0); // inside the table itself
        put_u64(&mut body, 4);
        body.extend_from_slice(b"tail");

        let bytes = wrap(&body, 1);
        assert!(matches!(
            Segment::open(&bytes),
            Err(Error::SectionOutOfRange { .. })
        ));
    }

    #[test]
    fn a_body_length_larger_than_the_file_is_refused() {
        let mut bytes = sample();
        bytes[16..24].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(matches!(
            Segment::open(&bytes),
            Err(Error::Truncated { .. })
        ));
    }

    #[test]
    fn trailing_bytes_after_the_body_are_ignored() {
        // A segment can be one record inside a larger file, so the reader takes
        // the body length as the boundary and does not care what follows.
        let mut bytes = sample();
        let clean = Segment::open(&bytes)
            .expect("open")
            .section(kind::TERMS)
            .map(<[u8]>::to_vec);
        bytes.extend_from_slice(b"another segment starts here");

        let segment = Segment::open(&bytes).expect("open with a tail");
        assert_eq!(segment.section(kind::TERMS).map(<[u8]>::to_vec), clean);
    }

    #[test]
    fn skipping_the_checksum_still_refuses_a_corrupt_table() {
        let mut body = Vec::new();
        put_u16(&mut body, kind::TERMS);
        put_u16(&mut body, 0);
        put_u32(&mut body, 0);
        put_u64(&mut body, 1 << 40);
        put_u64(&mut body, 8);

        let bytes = wrap(&body, 1);
        assert!(matches!(
            Segment::open_without_checksum(&bytes),
            Err(Error::SectionOutOfRange { .. })
        ));
    }

    #[test]
    fn skipping_the_checksum_accepts_a_body_the_checksum_would_reject() {
        let mut bytes = sample();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;

        assert!(matches!(
            Segment::open(&bytes),
            Err(Error::ChecksumMismatch { .. })
        ));
        assert!(Segment::open_without_checksum(&bytes).is_ok());
    }

    /// Puts a valid header in front of a hand built body, so that a test about
    /// a corrupt section table is not also a test about the header.
    fn wrap(body: &[u8], sections: u16) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_LEN + body.len());
        out.extend_from_slice(&MAGIC);
        put_u16(&mut out, FORMAT_VERSION);
        put_u16(&mut out, sections);
        put_u32(&mut out, crc32(body));
        put_u64(&mut out, body.len() as u64);
        out.resize(HEADER_LEN, 0);
        out.extend_from_slice(body);
        out
    }
}
