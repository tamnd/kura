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
//!   12..16  reserved, written as zero and ignored on read
//!   16..24  body length, u64
//!   24..32  reserved, written as zero and ignored on read
//!
//! body
//!   section table, 40 bytes per entry
//!     0..2    kind, u16
//!     2..4    flags, u16, reserved
//!     4..8    padding, u32
//!     8..16   offset from the start of the body, u64
//!     16..24  length, u64
//!     24..40  xxh3-128 of the payload, u128
//!   section payloads, in the order the writer added them
//!
//! footer, 32 bytes
//!   0..16   xxh3-128 of the section table, u128
//!   16..24  body length, u64, the same number the header carries
//!   24..32  footer magic
//! ```
//!
//! # Why the checksums are where they are
//!
//! There is no checksum over the segment as a whole, and that is the point.
//!
//! One digest over everything answers "was this file ever damaged" and cannot
//! answer "which part of it". Measured on a 68.7 KB index, a byte flipped at two
//! of five offsets was caught by the whole file digest and by nothing else,
//! because the bytes went on to decode into a well formed posting list of the
//! right length in ascending order. The report for those two could only say that
//! something somewhere was wrong.
//!
//! A digest per section plus a digest over the table says the same thing and
//! says where. The table digest covers the offsets and lengths, so a table that
//! has been edited is caught before any of them is used to slice anything. Each
//! section digest covers that section's bytes, so damage is attributed to the
//! section it is in and every other section is still known good.
//!
//! Together they cover every byte of the body exactly once, so nothing is lost
//! by there being no digest over the lot. The composition is also what lets a
//! reader check one section without reading the rest, which is what
//! [`Segment::verify_section`] is for and what a repair would need.
//!
//! # Why there is a footer
//!
//! The table digest cannot go in the header, because the header is written
//! first and the table is not known until every payload has been hashed. It
//! could be worked out in an extra pass, which is what the previous format did
//! for its whole body checksum, and a footer is the cheaper answer: it is
//! written last, when the number is already in hand.
//!
//! The footer also repeats the body length. A file whose two ends disagree
//! about how long it is has been damaged in a way that neither end can detect
//! alone, and a manifest entry carries a hash of the footer, so a store can tell
//! whether a segment is the one it committed without reading the segment
//! through.
//!
//! # What the reader refuses
//!
//! Opening a segment is where the engine decides whether to trust a file, so it
//! is deliberately unforgiving. Wrong magic, a version this build does not
//! write, a body that is shorter than the header says, a missing or wrong
//! footer, two ends that disagree about the body length, a section table that
//! runs past the end, a section whose bytes lie outside the body, two sections
//! of the same kind, or a digest that does not match, are all errors with their
//! own variant rather than one opaque failure.
//!
//! An unknown *section kind* is the one thing that is not an error. A reader
//! skips a section it does not recognise, which is what lets a later build add a
//! section without making every file it writes unreadable by this one. Version
//! is a refusal, kind is a shrug, and the difference is the whole forward
//! compatibility story.

use crate::codec::{
    get_u16, get_u32, get_u64, get_u128, put_u16, put_u32, put_u64, put_u128, split_at,
};
use crate::error::{Error, Result};
use crate::xxh3;
use crate::{FORMAT_VERSION, MAGIC};

/// The fixed size of a segment header.
pub const HEADER_LEN: usize = 32;

/// The fixed size of a segment footer.
pub const FOOTER_LEN: usize = 32;

/// The eight bytes that end every segment.
///
/// Different from the header magic on purpose. A tool scanning a file for
/// segments finds the start of one and the end of one with different needles,
/// and a header magic at the tail would make a truncated file look like the
/// beginning of another segment.
pub const FOOTER_MAGIC: [u8; 8] = *b"KURAFOOT";

/// The size of one entry in the section table.
const ENTRY_LEN: usize = 40;

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
    /// How long each document is, and how long they are on average, which is
    /// what a ranking function divides by.
    pub const NORMS: u16 = 9;
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

    /// Writes the segment out into a vector.
    ///
    /// [`Writer::write_to`] is the one to use when there is a file to write to,
    /// because this holds the segment and the sections it was made of at the
    /// same time.
    ///
    /// # Panics
    ///
    /// If the section count does not fit the header field, which [`Writer::add`]
    /// makes impossible. The alternative is writing a header that disagrees with
    /// the body and still passes its own checksum.
    #[must_use]
    pub fn finish(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.size());
        // Writing into a vector cannot fail, and a vector that cannot grow has
        // already aborted rather than returned.
        self.write_to(&mut out)
            .expect("a vector always takes what it is given");
        out
    }

    /// How many bytes the segment will be once it is written.
    ///
    /// It is not called `len` because [`Writer::is_empty`] answers a different
    /// question, which is whether any section has been added at all. A segment
    /// with nothing in it is still a header.
    #[must_use]
    pub fn size(&self) -> usize {
        let payload: usize = self.sections.iter().map(|(_, bytes)| bytes.len()).sum();
        HEADER_LEN + self.sections.len() * ENTRY_LEN + payload + FOOTER_LEN
    }

    /// Writes the segment to `out`.
    ///
    /// This is what a caller with somewhere to put it should use. A segment is
    /// the largest thing this crate builds, and handing back a vector of it
    /// means the sections and the copy of them exist at the same time, which on
    /// a real corpus is a few hundred megabytes of resident memory spent to say
    /// what is already said.
    ///
    /// Each section is hashed once, on the way into the table entry that
    /// describes it, so the only pass over a payload is the one that was going
    /// to happen anyway. The table digest goes in the footer rather than the
    /// header because the table is not finished until every payload has been
    /// hashed, and a footer is written when that number is already in hand
    /// instead of after a second walk or a seek back over a file that may not be
    /// seekable.
    ///
    /// # Errors
    ///
    /// Whatever `out` returns.
    ///
    /// # Panics
    ///
    /// If the section count does not fit the header field, which [`Writer::add`]
    /// makes impossible. The alternative is writing a header that disagrees with
    /// the body and still passes its own checks.
    pub fn write_to(self, out: &mut impl std::io::Write) -> std::io::Result<()> {
        let table_len = self.sections.len() * ENTRY_LEN;

        // The table has to be written before the payloads and needs the offsets
        // the payloads will land at, so walk them once to work the offsets out.
        let mut table = Vec::with_capacity(table_len);
        let mut offset = table_len as u64;
        for (kind, payload) in &self.sections {
            put_u16(&mut table, *kind);
            put_u16(&mut table, 0); // flags
            put_u32(&mut table, 0); // padding, keeps the entry eight byte aligned
            put_u64(&mut table, offset);
            put_u64(&mut table, payload.len() as u64);
            put_u128(&mut table, xxh3::hash128(payload));
            offset += payload.len() as u64;
        }

        // The count fits because add refuses to go past MAX_SECTIONS, which is
        // exactly the range of the field. Writing a wrong count here would give
        // a file that passes its own checks and still decodes to the wrong
        // thing, so this is one of the few places worth failing loudly.
        let count = u16::try_from(self.sections.len()).expect("add bounds the section count");

        let mut header = Vec::with_capacity(HEADER_LEN);
        header.extend_from_slice(&MAGIC);
        put_u16(&mut header, FORMAT_VERSION);
        put_u16(&mut header, count);
        put_u32(&mut header, 0); // reserved
        put_u64(&mut header, offset);
        header.resize(HEADER_LEN, 0); // the reserved tail

        out.write_all(&header)?;
        out.write_all(&table)?;
        for (_, payload) in &self.sections {
            out.write_all(payload)?;
        }
        out.write_all(&footer_bytes(&table, offset))?;
        Ok(())
    }
}

/// The footer for a body of `body_len` bytes whose table is `table`.
fn footer_bytes(table: &[u8], body_len: u64) -> Vec<u8> {
    let mut footer = Vec::with_capacity(FOOTER_LEN);
    put_u128(&mut footer, xxh3::hash128(table));
    put_u64(&mut footer, body_len);
    footer.extend_from_slice(&FOOTER_MAGIC);
    footer
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
    /// The digest of the table, out of the footer.
    table_digest: u128,
}

impl<'a> Segment<'a> {
    /// Opens a segment and verifies it completely, checksums included.
    ///
    /// This is the one to use unless there is a measured reason not to.
    ///
    /// # Errors
    ///
    /// Every way a segment can be wrong has its own variant. See the module
    /// documentation for the list.
    pub fn open(bytes: &'a [u8]) -> Result<Self> {
        let segment = Self::open_without_checksum(bytes)?;
        segment.verify()?;
        Ok(segment)
    }

    /// Opens a segment and verifies its structure but not its contents.
    ///
    /// The digests are the only checks skipped, and they are what costs a read
    /// of the whole file at open time. That is worth skipping for a large
    /// segment that was verified when it was published and has not been touched
    /// since, and it is not worth skipping for a file that arrived from
    /// somewhere else. Everything structural still holds, the footer included,
    /// so a section slice returned by this segment is still inside the input.
    ///
    /// # Errors
    ///
    /// As [`Segment::open`], minus what [`Segment::verify`] returns.
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
        let (_reserved, header) = get_u32(header)?;
        let (declared, _) = get_u64(header)?;

        // The body length is a claim by the file about itself, so it is checked
        // against the bytes actually present rather than used to size anything.
        let body_len = usize::try_from(declared).map_err(|_| Error::Truncated {
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

        // The footer sits immediately after the body, which is what lets a
        // segment be one record inside a larger file: everything past the footer
        // belongs to whoever put it there.
        let (footer, _) = split_at(&rest[body_len..], FOOTER_LEN)?;
        let (table_digest, footer) = get_u128(footer)?;
        let (footer_len, magic) = get_u64(footer)?;
        if magic != FOOTER_MAGIC {
            return Err(Error::BadFooter);
        }
        if footer_len != declared {
            return Err(Error::BodyLengthMismatch {
                header: declared,
                footer: footer_len,
            });
        }

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
            table_digest,
        };
        segment.validate_table()?;
        Ok(segment)
    }

    /// Checks every digest the segment carries.
    ///
    /// The table first and then each section, which is the order they have to be
    /// checked in: the digests the sections are compared against are in the
    /// table, so a table nobody has checked is a set of comparisons against
    /// numbers that could themselves be wrong.
    ///
    /// This is what [`Segment::open`] does once the structure holds. It is
    /// public so that a caller who opened without it can pay for it later,
    /// during a compaction or a scrub rather than in front of somebody waiting
    /// for a query.
    ///
    /// # Errors
    ///
    /// [`Error::Xxh3Mismatch`] if the section table has been changed, and
    /// [`Error::SectionChecksumMismatch`] naming the first section whose bytes
    /// are not the bytes that were written.
    pub fn verify(&self) -> Result<()> {
        self.verify_table()?;
        for index in 0..self.count {
            self.verify_at(index)?;
        }
        Ok(())
    }

    /// Checks the section table against the digest in the footer.
    ///
    /// Thirty two bytes per section and nothing else, so this is the one check
    /// whose cost does not depend on how large the segment is. A tool that walks
    /// a directory of segments to see which ones are worth reading starts here.
    ///
    /// # Errors
    ///
    /// [`Error::Xxh3Mismatch`] if the offsets, the lengths or the digests in the
    /// table are not the ones that were written.
    pub fn verify_table(&self) -> Result<()> {
        let computed = xxh3::hash128(self.table);
        if computed == self.table_digest {
            return Ok(());
        }
        Err(Error::Xxh3Mismatch {
            stored: self.table_digest,
            computed,
        })
    }

    /// Checks one section against its own digest, reading no other section.
    ///
    /// A reader that only wants the term dictionary can pay for the term
    /// dictionary, which on a segment whose postings are most of the file is a
    /// different order of cost. It is also the question a repair asks: the first
    /// thing worth knowing about a damaged file is which parts of it are still
    /// good.
    ///
    /// # Errors
    ///
    /// [`Error::MissingSection`] if the segment does not carry that kind, and
    /// [`Error::SectionChecksumMismatch`] if the bytes are not the bytes that
    /// were written. The table digest is not checked here, because a caller
    /// asking about one section has usually already opened the segment, which
    /// checks it.
    pub fn verify_section(&self, kind: u16) -> Result<()> {
        for index in 0..self.count {
            if self
                .entry(index)
                .is_some_and(|section| section.kind == kind)
            {
                return self.verify_at(index);
            }
        }
        Err(Error::MissingSection { kind })
    }

    /// Checks the section at one position in the table.
    fn verify_at(&self, index: usize) -> Result<()> {
        // Neither of these can happen for a segment that came out of open,
        // because validate_table proved every entry decodes and every slice
        // fits. They are errors rather than expects because this is a decode
        // path and nothing on a decode path panics.
        let section = self.entry(index).ok_or(Error::Truncated {
            needed: self.count * ENTRY_LEN,
            available: self.table.len(),
        })?;
        let bytes = self.slice(section).ok_or(Error::SectionOutOfRange {
            kind: section.kind,
            offset: section.offset,
            length: section.length,
        })?;
        let computed = xxh3::hash128(bytes);
        if computed != section.digest {
            return Err(Error::SectionChecksumMismatch {
                kind: section.kind,
                stored: section.digest,
                computed,
            });
        }
        Ok(())
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
            let section = self.entry(index).ok_or(Error::Truncated {
                needed: table_len,
                available: self.table.len(),
            })?;

            let start = usize::try_from(section.offset).ok();
            let end = start.and_then(|s| {
                usize::try_from(section.length)
                    .ok()
                    .and_then(|l| s.checked_add(l))
            });
            let fits = match (start, end) {
                (Some(start), Some(end)) => start >= table_len && end <= self.body.len(),
                _ => false,
            };
            if !fits {
                return Err(Error::SectionOutOfRange {
                    kind: section.kind,
                    offset: section.offset,
                    length: section.length,
                });
            }

            // Duplicates are rejected on write, but a file can arrive from
            // anywhere, and a reader that silently picks the first of two
            // sections is a reader whose answer depends on write order.
            let word = usize::from(section.kind) / 64;
            let bit = 1u64 << (u32::from(section.kind) % 64);
            if seen[word] & bit != 0 {
                return Err(Error::DuplicateSection { kind: section.kind });
            }
            seen[word] |= bit;
        }
        Ok(())
    }

    /// Reads one table entry in place.
    fn entry(&self, index: usize) -> Option<Section> {
        let start = index.checked_mul(ENTRY_LEN)?;
        let bytes = self.table.get(start..start.checked_add(ENTRY_LEN)?)?;
        let (kind, rest) = get_u16(bytes).ok()?;
        let (_flags, rest) = get_u16(rest).ok()?;
        let (_padding, rest) = get_u32(rest).ok()?;
        let (offset, rest) = get_u64(rest).ok()?;
        let (length, rest) = get_u64(rest).ok()?;
        let (digest, _) = get_u128(rest).ok()?;
        Some(Section {
            kind,
            offset,
            length,
            digest,
        })
    }

    /// The bytes one table entry describes.
    ///
    /// `None` only for an entry that has not been through `validate_table`,
    /// which is no entry a caller can reach.
    fn slice(&self, section: Section) -> Option<&'a [u8]> {
        let start = usize::try_from(section.offset).ok()?;
        let end = start.checked_add(usize::try_from(section.length).ok()?)?;
        self.body.get(start..end)
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
            let section = self.entry(index)?;
            if section.kind != kind {
                continue;
            }
            // validate_table already proved these convert and fit.
            return self.slice(section);
        }
        None
    }

    /// Iterates the section kinds present, in the order they were written.
    pub fn kinds(&self) -> impl Iterator<Item = u16> + '_ {
        self.sections().map(|section| section.kind)
    }

    /// Iterates the section table, in the order it was written.
    ///
    /// The whole entry rather than only the kind, which is what a tool that
    /// prints a file's layout needs. Every entry was checked at open, so an
    /// offset and a length here are known to fit inside the body.
    pub fn sections(&self) -> impl Iterator<Item = Section> + '_ {
        (0..self.count).filter_map(move |index| self.entry(index))
    }
}

/// One row of the section table.
///
/// The offset is from the start of the body and not from the start of the file,
/// which is the same thing the table itself stores. Anything printing these for
/// a person to compare against a hex dump has to add [`HEADER_LEN`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Section {
    /// Which kind of section this is, from [`kind`].
    pub kind: u16,
    /// Where it starts, from the start of the body.
    pub offset: u64,
    /// How many bytes it holds.
    pub length: u64,
    /// The xxh3-128 of those bytes, as the writer computed it.
    ///
    /// [`Segment::verify_section`] is what compares this against the bytes that
    /// are there. It is public because a tool that copies a section out, or a
    /// store deciding whether two segments hold the same dictionary, can use the
    /// number without reading the payload at all.
    pub digest: u128,
}

/// The name a person knows a section kind by.
///
/// Returns `None` for a kind this build has never heard of, which is not an
/// error: a reader skips an unknown section, and a tool that prints the table
/// should say the number rather than pretend the section is not there.
#[must_use]
pub const fn name(kind: u16) -> Option<&'static str> {
    match kind {
        kind::TERMS => Some("terms"),
        kind::POSTINGS => Some("postings"),
        kind::FIELDS => Some("fields"),
        kind::VECTORS => Some("vectors"),
        kind::ACL => Some("acl"),
        kind::COLUMNS => Some("columns"),
        kind::GRAPH => Some("graph"),
        kind::TOMBSTONES => Some("tombstones"),
        kind::NORMS => Some("norms"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_section_table_reads_back_as_it_was_written() {
        let mut writer = Writer::new();
        writer.add(kind::TERMS, vec![1; 40]).expect("adds");
        writer.add(kind::POSTINGS, vec![2; 400]).expect("adds");
        let bytes = writer.finish();
        let segment = Segment::open(&bytes).expect("opens");

        let sections: Vec<Section> = segment.sections().collect();
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].kind, kind::TERMS);
        assert_eq!(sections[0].length, 40);
        assert_eq!(sections[1].kind, kind::POSTINGS);
        assert_eq!(sections[1].length, 400);
        // In the order written, and each one where the section itself is.
        assert!(sections[0].offset < sections[1].offset);
        for section in &sections {
            let start = usize::try_from(section.offset).expect("fits");
            let end = start + usize::try_from(section.length).expect("fits");
            assert!(end <= segment.body.len());
        }
    }

    #[test]
    fn a_kind_has_a_name_and_an_unknown_kind_does_not() {
        assert_eq!(name(kind::POSTINGS), Some("postings"));
        assert_eq!(name(kind::NORMS), Some("norms"));
        // Not an error, because a reader skips a section it has never heard of
        // and a tool printing the table should say so rather than hide it.
        assert_eq!(name(4_242), None);
    }

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
            row(&mut body, kind::TERMS, (2 * ENTRY_LEN) as u64, 1);
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
                    Error::SectionChecksumMismatch { .. }
                        | Error::Xxh3Mismatch { .. }
                        | Error::SectionOutOfRange { .. }
                        | Error::DuplicateSection { .. }
                        | Error::BodyLengthMismatch { .. }
                        | Error::BadFooter
                        | Error::Truncated { .. }
                ),
                "byte {index}: {err:?}"
            );
        }
    }

    #[test]
    fn damage_is_attributed_to_the_section_it_landed_in() {
        // The whole reason there is a digest per section. One digest over the
        // file can say that a byte somewhere is wrong and stop there, and a
        // person holding a damaged index wants to know whether it is the term
        // dictionary or a section they can rebuild.
        let clean = sample();
        let acl = Segment::open(&clean)
            .expect("open")
            .sections()
            .find(|section| section.kind == kind::ACL)
            .expect("the sample carries an acl");

        let mut bytes = clean.clone();
        bytes[HEADER_LEN + usize::try_from(acl.offset).expect("fits")] ^= 0xff;

        assert!(matches!(
            Segment::open(&bytes),
            Err(Error::SectionChecksumMismatch {
                kind: kind::ACL,
                ..
            })
        ));

        // And every other section is still known good, which is the half of the
        // answer a single digest cannot give at all.
        let segment = Segment::open_without_checksum(&bytes).expect("the structure is untouched");
        assert_eq!(segment.verify_section(kind::TERMS), Ok(()));
        assert_eq!(segment.verify_section(kind::POSTINGS), Ok(()));
        assert!(segment.verify_section(kind::ACL).is_err());
        assert_eq!(
            segment.verify_section(kind::VECTORS),
            Err(Error::MissingSection {
                kind: kind::VECTORS
            })
        );
    }

    #[test]
    fn an_edited_table_is_caught_before_the_sections_it_describes() {
        // The digest one entry holds, changed. The structure still checks out
        // and the section it points at is untouched, so nothing but the digest
        // over the table can catch this, and it has to catch it before any
        // section is compared against a number that is now made up.
        let mut bytes = sample();
        bytes[HEADER_LEN + 24] ^= 0xff;

        assert!(matches!(
            Segment::open(&bytes),
            Err(Error::Xxh3Mismatch { .. })
        ));
        assert!(Segment::open_without_checksum(&bytes).is_ok());
    }

    #[test]
    fn a_segment_ends_in_a_footer() {
        let bytes = sample();
        assert_eq!(
            &bytes[bytes.len() - FOOTER_MAGIC.len()..],
            &FOOTER_MAGIC[..]
        );
        // Not the header magic, so that a tool scanning a file for the start of
        // a segment cannot find one at the end of the segment before it.
        assert_ne!(FOOTER_MAGIC, MAGIC);
    }

    #[test]
    fn a_missing_footer_is_refused() {
        let mut bytes = sample();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        assert_eq!(Segment::open(&bytes), Err(Error::BadFooter));
        // Structural rather than a digest, so skipping the digests does not skip
        // this: a file that does not end in a footer is not a segment.
        assert_eq!(
            Segment::open_without_checksum(&bytes),
            Err(Error::BadFooter)
        );
    }

    #[test]
    fn two_ends_that_disagree_about_the_length_are_refused() {
        // The header says one thing and the footer says another. Damage that
        // changed only one of them would be invisible to a reader that only
        // reads the other, which is why the number is written twice.
        let mut bytes = sample();
        let at = bytes.len() - FOOTER_LEN + 16;
        bytes[at..at + 8].copy_from_slice(&7u64.to_le_bytes());

        let body_len = (bytes.len() - HEADER_LEN - FOOTER_LEN) as u64;
        assert_eq!(
            Segment::open(&bytes),
            Err(Error::BodyLengthMismatch {
                header: body_len,
                footer: 7,
            })
        );
    }

    #[test]
    fn the_size_a_writer_promises_is_the_size_it_writes() {
        let mut writer = Writer::new();
        writer.add(kind::TERMS, vec![1; 40]).expect("adds");
        writer.add(kind::POSTINGS, vec![2; 4_000]).expect("adds");
        let size = writer.size();
        assert_eq!(writer.finish().len(), size);
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
        // A length no allocation should be sized from.
        row(&mut body, kind::TERMS, ENTRY_LEN as u64, u64::MAX);

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
        row(&mut body, kind::TERMS, 0, 4); // inside the table itself
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
        row(&mut body, kind::TERMS, 1 << 40, 8);

        let bytes = wrap(&body, 1);
        assert!(matches!(
            Segment::open_without_checksum(&bytes),
            Err(Error::SectionOutOfRange { .. })
        ));
    }

    #[test]
    fn skipping_the_checksum_accepts_a_body_the_checksum_would_reject() {
        let mut bytes = sample();
        let postings = Segment::open(&bytes)
            .expect("open")
            .sections()
            .find(|section| section.kind == kind::POSTINGS)
            .expect("the sample carries postings");
        bytes[HEADER_LEN + usize::try_from(postings.offset).expect("fits")] ^= 0xff;

        assert!(matches!(
            Segment::open(&bytes),
            Err(Error::SectionChecksumMismatch { .. })
        ));
        // The damage is inside a payload, and a payload is exactly what the
        // structural checks never look at.
        assert!(Segment::open_without_checksum(&bytes).is_ok());
    }

    /// One hand built table entry, for the tables a writer would refuse to
    /// produce. The digest is left at zero because every test that builds a
    /// table this way is about a structural check, which happens first.
    fn row(out: &mut Vec<u8>, kind: u16, offset: u64, length: u64) {
        put_u16(out, kind);
        put_u16(out, 0);
        put_u32(out, 0);
        put_u64(out, offset);
        put_u64(out, length);
        put_u128(out, 0);
    }

    /// Puts a valid header and footer around a hand built body, so that a test
    /// about a corrupt section table is not also a test about the two ends.
    fn wrap(body: &[u8], sections: u16) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_LEN + body.len() + FOOTER_LEN);
        out.extend_from_slice(&MAGIC);
        put_u16(&mut out, FORMAT_VERSION);
        put_u16(&mut out, sections);
        put_u32(&mut out, 0); // reserved
        put_u64(&mut out, body.len() as u64);
        out.resize(HEADER_LEN, 0);
        out.extend_from_slice(body);

        let table = &body[..usize::from(sections) * ENTRY_LEN];
        out.extend_from_slice(&footer_bytes(table, body.len() as u64));
        out
    }
}
