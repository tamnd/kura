//! The superblock and the manifest: everything about a store that changes.
//!
//! A segment is immutable, so nothing in one ever needs rewriting. That leaves a
//! small amount of state that does change: which segments exist, where they are,
//! how many live documents they hold between them, and how far the log has been
//! consumed. All of it lives here, in the two fixed regions at the front of the
//! file, and a commit is the act of replacing it.
//!
//! # The front of a store
//!
//! ```text
//! 0        superblock, 4 KiB, written once
//! 4096     manifest slot A, 64 KiB
//! 69632    manifest slot B, 64 KiB
//! 135168   write ahead log ring
//! ...      segment region, append only, page aligned
//! ```
//!
//! The superblock says where the other three begin and is written when the store
//! is created. It carries a store identifier so that a segment recovered from a
//! backup can be told whether it belongs to this store, and a creation
//! timestamp, because every incident investigation starts with when.
//!
//! # Why there are two manifest slots
//!
//! A commit has to be all or nothing. If a manifest were written in place, a
//! machine losing power halfway through would leave a file whose list of
//! segments is half the old list and half the new one, which is worse than
//! either.
//!
//! So a commit writes the whole manifest into whichever slot is not currently
//! live, and fsyncs it. Until that fsync returns, the other slot is still the
//! committed state and nothing has changed. After it returns, the new slot is
//! the one with the higher epoch and it is the committed state. There is no
//! moment in between, which is the property being bought.
//!
//! # Why the epoch decides and not a pointer
//!
//! The obvious design puts a pointer to the live slot in the superblock and
//! flips it on commit. It does not work here. The superblock is covered by a
//! checksum, so changing a byte inside it means recomputing that checksum and
//! writing the page again, and a 4 KiB write is not atomic. Excluding the
//! pointer from the checksum to get the atomicity back leaves a byte in the file
//! that nothing verifies, which is the one byte that decides which state the
//! store opens in.
//!
//! The epoch already answers the question and it is inside the region the
//! checksum covers. Read both slots, discard any that fails its own checksum,
//! and take the higher epoch. A slot caught mid write fails its checksum and is
//! discarded, a slot that has never been written is zeroes and fails its
//! checksum too, and the survivor with the higher epoch is by construction the
//! last commit that completed. This is also one fsync per commit rather than
//! two, since there is no second write to order after the first.
//!
//! # What is not here
//!
//! Nothing in this module touches a file. It encodes and decodes byte ranges,
//! and the layer that owns the file descriptor does the reading, the writing and
//! the fsync, in that order. That is what makes the interesting cases testable:
//! a torn write is a slice with a byte changed in it, and there is no way to
//! arrange one of those reliably through a filesystem.
//!
//! The log is not here either. The manifest records where its head and tail are,
//! because those are part of the committed state, and the records themselves are
//! somebody else's format.

use crate::codec::{
    get_u16, get_u32, get_u64, get_u128, put_u16, put_u32, put_u64, put_u128, split_at,
};
use crate::error::{Error, Result};
use crate::xxh3;

/// The magic bytes at the start of a store.
///
/// Different from the segment magic on purpose. Opening a segment as a store or
/// a store as a segment is a mistake somebody will make, and it should fail on
/// the first eight bytes rather than somewhere deeper in.
pub const MAGIC: [u8; 8] = *b"KURASTOR";

/// The page size every structural offset in a store is aligned to.
///
/// 4 KiB because it is the page size on every platform and architecture this
/// engine supports, so a mapped read of one page is one fault.
pub const PAGE: u32 = 4096;

/// The size of the superblock, which is one page.
pub const SUPERBLOCK_LEN: usize = PAGE as usize;

/// The size of one manifest slot.
pub const SLOT_LEN: usize = 64 * 1024;

/// Where manifest slot A begins.
pub const SLOT_A_OFFSET: u64 = SUPERBLOCK_LEN as u64;

/// Where manifest slot B begins.
pub const SLOT_B_OFFSET: u64 = SLOT_A_OFFSET + SLOT_LEN as u64;

/// Where the write ahead log ring begins, which is directly after slot B.
pub const WAL_OFFSET: u64 = SLOT_B_OFFSET + SLOT_LEN as u64;

/// The default size of the write ahead log ring.
///
/// A ring rather than an append region, so the space it takes is bounded and
/// truncating it is a pointer move rather than a rewrite. The size is fixed when
/// the store is created because moving it afterwards would move everything after
/// it.
pub const DEFAULT_WAL_LEN: u64 = 256 * 1024 * 1024;

/// The store format version this build writes, in its breaking half.
///
/// A store with a major this build does not know is refused. A store with a
/// minor it does not know is opened, and the parts it does not recognise are
/// ignored, which is what makes adding a field an additive change.
pub const MAJOR: u16 = 1;

/// The store format version this build writes, in its additive half.
pub const MINOR: u16 = 0;

/// The number of bytes a checksum takes at the end of a region.
const SUM_LEN: usize = 16;

/// The fixed part of a manifest, before the segment table.
const MANIFEST_HEADER_LEN: usize = 64;

/// The size of one entry in the segment table.
const SEGMENT_LEN: usize = 64;

/// The most segments one manifest slot holds.
///
/// Roughly a thousand, which is far more than the compaction policy allows a
/// store to accumulate. When it stops being enough the manifest spills into a
/// segment of its own and the slot holds a pointer to it, which is an additive
/// change and therefore a minor version bump.
pub const MAX_SEGMENTS: usize = (SLOT_LEN - MANIFEST_HEADER_LEN - SUM_LEN) / SEGMENT_LEN;

// The table has to fit in the slot with the header before it and the checksum
// after it, and one more entry has to not fit. Checked here rather than in a
// test, because it is a property of the arithmetic above and a build that got it
// wrong should not get as far as running.
const _: () = assert!(MANIFEST_HEADER_LEN + MAX_SEGMENTS * SEGMENT_LEN + SUM_LEN <= SLOT_LEN);
const _: () = assert!(MANIFEST_HEADER_LEN + (MAX_SEGMENTS + 1) * SEGMENT_LEN + SUM_LEN > SLOT_LEN);

/// Which of the two manifest slots is meant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Slot {
    /// The first slot, at [`SLOT_A_OFFSET`].
    A,
    /// The second slot, at [`SLOT_B_OFFSET`].
    B,
}

impl Slot {
    /// Where this slot begins in the file.
    #[must_use]
    pub const fn offset(self) -> u64 {
        match self {
            Self::A => SLOT_A_OFFSET,
            Self::B => SLOT_B_OFFSET,
        }
    }

    /// The other slot, which is the one the next commit writes.
    #[must_use]
    pub const fn other(self) -> Self {
        match self {
            Self::A => Self::B,
            Self::B => Self::A,
        }
    }
}

/// The first page of a store.
///
/// Written when the store is created and then only when the format version
/// changes, which is to say almost never. Everything in it is a fact about the
/// file's shape rather than about its contents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Superblock {
    /// The breaking half of the format version.
    pub major: u16,
    /// The additive half of the format version.
    pub minor: u16,
    /// The page size every offset below is aligned to.
    pub page: u32,
    /// Where the write ahead log ring begins.
    pub wal_offset: u64,
    /// How long the write ahead log ring is.
    pub wal_len: u64,
    /// Where the segment region begins, which is where the file grows.
    pub segments_offset: u64,
    /// A random value identifying this store, so that a segment found somewhere
    /// else can be tested for belonging to it.
    pub store: u128,
    /// When the store was created, in unix nanoseconds.
    pub created: u64,
    /// Reserved, written as zero.
    pub flags: u64,
}

impl Superblock {
    /// A superblock for a new store with the default layout.
    ///
    /// The identifier and the timestamp come from the caller because this crate
    /// has neither a clock nor a source of randomness, and because a decoder
    /// that cannot be made to produce the same bytes twice is a decoder that
    /// cannot be tested.
    #[must_use]
    pub const fn new(store: u128, created: u64) -> Self {
        Self {
            major: MAJOR,
            minor: MINOR,
            page: PAGE,
            wal_offset: WAL_OFFSET,
            wal_len: DEFAULT_WAL_LEN,
            segments_offset: WAL_OFFSET + DEFAULT_WAL_LEN,
            store,
            created,
            flags: 0,
        }
    }

    /// Writes the superblock as one page.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(SUPERBLOCK_LEN);
        out.extend_from_slice(&MAGIC);
        put_u16(&mut out, self.major);
        put_u16(&mut out, self.minor);
        put_u32(&mut out, self.page);
        put_u64(&mut out, self.wal_offset);
        put_u64(&mut out, self.wal_len);
        put_u64(&mut out, self.segments_offset);
        put_u128(&mut out, self.store);
        put_u64(&mut out, self.created);
        put_u64(&mut out, self.flags);
        out.resize(SUPERBLOCK_LEN - SUM_LEN, 0);
        let sum = xxh3::hash128(&out);
        put_u128(&mut out, sum);
        out
    }

    /// Reads a superblock from the first page of a store.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Truncated`] if there is less than a page, then
    /// [`Error::BadMagic`], then [`Error::UnsupportedVersion`] for a major this
    /// build does not read, then [`Error::Xxh3Mismatch`], then
    /// [`Error::UnsupportedPageSize`].
    ///
    /// The order is deliberate. A file that is not a store should say so rather
    /// than report a checksum failure, which reads as damage and sends the
    /// reader looking for a backup they do not need.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let (page, _) = split_at(bytes, SUPERBLOCK_LEN)?;
        let (body, tail) = split_at(page, SUPERBLOCK_LEN - SUM_LEN)?;
        let (magic, rest) = split_at(body, MAGIC.len())?;
        if magic != MAGIC {
            return Err(Error::BadMagic);
        }
        let (major, rest) = get_u16(rest)?;
        if major != MAJOR {
            return Err(Error::UnsupportedVersion {
                found: major,
                expected: MAJOR,
            });
        }
        let (stored, _) = get_u128(tail)?;
        let computed = xxh3::hash128(body);
        if stored != computed {
            return Err(Error::Xxh3Mismatch { stored, computed });
        }
        let (minor, rest) = get_u16(rest)?;
        let (page_size, rest) = get_u32(rest)?;
        if page_size != PAGE {
            return Err(Error::UnsupportedPageSize {
                found: page_size,
                expected: PAGE,
            });
        }
        let (wal_offset, rest) = get_u64(rest)?;
        let (wal_len, rest) = get_u64(rest)?;
        let (segments_offset, rest) = get_u64(rest)?;
        let (store, rest) = get_u128(rest)?;
        let (created, rest) = get_u64(rest)?;
        let (flags, _) = get_u64(rest)?;
        Ok(Self {
            major,
            minor,
            page: page_size,
            wal_offset,
            wal_len,
            segments_offset,
            store,
            created,
            flags,
        })
    }
}

/// One segment, as the manifest describes it.
///
/// The segment itself repeats most of this in its own header, and the two are
/// checked against each other on open. The duplication is the point: a manifest
/// that has drifted from the file it names is a bug that shows up as wrong
/// results rather than as a crash, so it is worth being able to detect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Segment {
    /// Where the segment begins in the file.
    pub offset: u64,
    /// How long it is.
    pub len: u64,
    /// How many documents it holds, including tombstoned ones.
    pub docs: u32,
    /// The first ordinal in it that is still live, which is what lets a
    /// compaction skip a prefix of deletions without reading them.
    pub first_live: u32,
    /// Where this segment's tombstone bitmap is, or zero for none.
    ///
    /// Tombstones live outside the segment because a segment is immutable and a
    /// tombstone is not.
    pub tombstones_offset: u64,
    /// How long the tombstone bitmap is.
    pub tombstones_len: u32,
    /// Which generation of tombstones this is, so a reader can tell whether the
    /// bitmap it already has is current.
    pub generation: u32,
    /// Which level of the compaction policy the segment sits at.
    pub level: u32,
    /// Reserved, written as zero.
    pub flags: u32,
    /// When the segment was written, in unix nanoseconds.
    pub created: u64,
    /// The xxh3-64 of the segment's own footer.
    ///
    /// This is how a manifest entry is matched against the file it points at
    /// without reading the segment through.
    pub footer: u64,
}

/// The mutable state of a store.
///
/// One of these is the committed state at any instant, and a commit replaces it
/// with another. Everything in it is either a location, a count that would be
/// expensive to recompute, or a position in the log.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Manifest {
    /// Which commit this is. Monotonic, and what decides between the two slots.
    pub epoch: u64,
    /// When the commit happened, in unix nanoseconds.
    pub written: u64,
    /// How many documents are live across all segments.
    pub live: u64,
    /// How many documents there are including tombstoned ones, which is what a
    /// compaction decision is made from.
    pub total: u64,
    /// The total term count, which is the numerator of the average document
    /// length that BM25 divides by.
    pub terms: u64,
    /// Reserved, written as zero.
    pub flags: u32,
    /// How far the write ahead log has been consumed.
    pub wal_head: u64,
    /// How far the write ahead log has been written.
    pub wal_tail: u64,
    /// Every segment in the store, in the order they were added.
    pub segments: Vec<Segment>,
}

impl Manifest {
    /// The manifest a newly created store commits, describing nothing.
    #[must_use]
    pub fn empty(written: u64) -> Self {
        Self {
            epoch: 1,
            written,
            ..Self::default()
        }
    }

    /// Writes the manifest as one slot, padded to [`SLOT_LEN`].
    ///
    /// The padding is not waste. A slot is a fixed region of the file, so a
    /// manifest that shrinks has to overwrite what the longer one left behind,
    /// and writing the whole slot every time is both simpler and the only
    /// version that cannot leave a stale tail behind.
    ///
    /// # Errors
    ///
    /// Returns [`Error::TooManySegments`] past [`MAX_SEGMENTS`].
    pub fn encode(&self) -> Result<Vec<u8>> {
        let count = u32::try_from(self.segments.len()).map_err(|_| Error::TooManySegments {
            count: self.segments.len(),
        })?;
        if self.segments.len() > MAX_SEGMENTS {
            return Err(Error::TooManySegments {
                count: self.segments.len(),
            });
        }
        let mut out = Vec::with_capacity(SLOT_LEN);
        put_u64(&mut out, self.epoch);
        put_u64(&mut out, self.written);
        put_u64(&mut out, self.live);
        put_u64(&mut out, self.total);
        put_u64(&mut out, self.terms);
        put_u32(&mut out, count);
        put_u32(&mut out, self.flags);
        put_u64(&mut out, self.wal_head);
        put_u64(&mut out, self.wal_tail);
        debug_assert_eq!(out.len(), MANIFEST_HEADER_LEN);
        for segment in &self.segments {
            put_u64(&mut out, segment.offset);
            put_u64(&mut out, segment.len);
            put_u32(&mut out, segment.docs);
            put_u32(&mut out, segment.first_live);
            put_u64(&mut out, segment.tombstones_offset);
            put_u32(&mut out, segment.tombstones_len);
            put_u32(&mut out, segment.generation);
            put_u32(&mut out, segment.level);
            put_u32(&mut out, segment.flags);
            put_u64(&mut out, segment.created);
            put_u64(&mut out, segment.footer);
        }
        out.resize(SLOT_LEN - SUM_LEN, 0);
        let sum = xxh3::hash128(&out);
        put_u128(&mut out, sum);
        Ok(out)
    }

    /// Reads a manifest out of one slot.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Truncated`] for a short slot, [`Error::Xxh3Mismatch`] if
    /// the slot does not checksum, and [`Error::TooManySegments`] if the count
    /// in the header is larger than the slot could hold, which is the case where
    /// a plausible looking number would otherwise send the decoder past the end
    /// of the table.
    ///
    /// A slot that has never been written is zeroes, and zeroes are not a valid
    /// checksum of themselves, so an untouched slot fails here rather than
    /// decoding into an empty manifest at epoch zero. That distinction matters
    /// on a store that has been committed to exactly once.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let (slot, _) = split_at(bytes, SLOT_LEN)?;
        let (body, tail) = split_at(slot, SLOT_LEN - SUM_LEN)?;
        let (stored, _) = get_u128(tail)?;
        let computed = xxh3::hash128(body);
        if stored != computed {
            return Err(Error::Xxh3Mismatch { stored, computed });
        }
        let (epoch, rest) = get_u64(body)?;
        let (written, rest) = get_u64(rest)?;
        let (live, rest) = get_u64(rest)?;
        let (total, rest) = get_u64(rest)?;
        let (terms, rest) = get_u64(rest)?;
        let (count, rest) = get_u32(rest)?;
        let (flags, rest) = get_u32(rest)?;
        let (wal_head, rest) = get_u64(rest)?;
        let (wal_tail, mut rest) = get_u64(rest)?;
        let count = count as usize;
        if count > MAX_SEGMENTS {
            return Err(Error::TooManySegments { count });
        }
        let mut segments = Vec::with_capacity(count);
        for _ in 0..count {
            let mut segment = Segment::default();
            (segment.offset, rest) = get_u64(rest)?;
            (segment.len, rest) = get_u64(rest)?;
            (segment.docs, rest) = get_u32(rest)?;
            (segment.first_live, rest) = get_u32(rest)?;
            (segment.tombstones_offset, rest) = get_u64(rest)?;
            (segment.tombstones_len, rest) = get_u32(rest)?;
            (segment.generation, rest) = get_u32(rest)?;
            (segment.level, rest) = get_u32(rest)?;
            (segment.flags, rest) = get_u32(rest)?;
            (segment.created, rest) = get_u64(rest)?;
            (segment.footer, rest) = get_u64(rest)?;
            segments.push(segment);
        }
        Ok(Self {
            epoch,
            written,
            live,
            total,
            terms,
            flags,
            wal_head,
            wal_tail,
            segments,
        })
    }

    /// The manifest that follows this one, at the next epoch.
    ///
    /// The epoch is the only field a caller must not set by hand, since the
    /// whole recovery story rests on it never going backwards.
    #[must_use]
    pub fn next(&self, written: u64) -> Self {
        Self {
            epoch: self.epoch.saturating_add(1),
            written,
            ..self.clone()
        }
    }
}

/// Which manifest a store opens at, and which slot it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Committed {
    /// The slot the surviving manifest was read from. The next commit goes to
    /// [`Slot::other`] of this one.
    pub slot: Slot,
    /// The state the store opens at.
    pub manifest: Manifest,
}

/// Decides which of the two slots is the committed state.
///
/// Both slots are decoded, whichever fail their checksum are discarded, and the
/// survivor with the higher epoch wins. A tie cannot happen on a store this
/// engine wrote, since a commit only ever increments, and if one somehow appears
/// then slot A is taken so that the choice is at least the same on every machine
/// that reads the file.
///
/// # Errors
///
/// Returns [`Error::NoManifest`] if neither slot decodes. That is not the same
/// fact as a store being empty: an empty store still has one committed manifest
/// describing no segments.
pub fn recover(a: &[u8], b: &[u8]) -> Result<Committed> {
    let first = Manifest::decode(a).ok().map(|manifest| Committed {
        slot: Slot::A,
        manifest,
    });
    let second = Manifest::decode(b).ok().map(|manifest| Committed {
        slot: Slot::B,
        manifest,
    });
    match (first, second) {
        (Some(one), Some(two)) => {
            if two.manifest.epoch > one.manifest.epoch {
                Ok(two)
            } else {
                Ok(one)
            }
        }
        (Some(only), None) | (None, Some(only)) => Ok(only),
        (None, None) => Err(Error::NoManifest),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn superblock() -> Superblock {
        Superblock::new(0x0123_4567_89ab_cdef_fedc_ba98_7654_3210, 1_700_000_000)
    }

    fn segment(n: u32) -> Segment {
        let wide = u64::from(n);
        Segment {
            offset: 135_168 + wide * 4096,
            len: 4096 * (wide + 1),
            docs: 1000 + n,
            first_live: n,
            tombstones_offset: wide * 64,
            tombstones_len: 128,
            generation: 3,
            level: 1,
            flags: 0,
            created: 1_700_000_000 + wide,
            footer: 0xdead_beef_0000_0000 + wide,
        }
    }

    /// The most segments a slot holds, as a count of segments rather than as a
    /// size in bytes.
    fn full() -> u32 {
        u32::try_from(MAX_SEGMENTS).expect("a slot holds fewer segments than a u32 counts")
    }

    fn manifest(epoch: u64, segments: u32) -> Manifest {
        Manifest {
            epoch,
            written: 1_700_000_100,
            live: 900,
            total: 1000,
            terms: 55_000,
            flags: 0,
            wal_head: 4096,
            wal_tail: 8192,
            segments: (0..segments).map(segment).collect(),
        }
    }

    #[test]
    fn a_superblock_is_one_page_and_round_trips() {
        let written = superblock();
        let bytes = written.encode();
        assert_eq!(bytes.len(), SUPERBLOCK_LEN);
        assert_eq!(Superblock::decode(&bytes).expect("a superblock"), written);
    }

    #[test]
    fn a_superblock_names_the_regions_after_it() {
        let block = superblock();
        assert_eq!(block.wal_offset, WAL_OFFSET);
        assert_eq!(block.wal_offset, SLOT_B_OFFSET + SLOT_LEN as u64);
        assert_eq!(block.segments_offset, WAL_OFFSET + DEFAULT_WAL_LEN);
        // Every structural offset is page aligned, which is the property that
        // makes a mapped read of any of them a single fault.
        for offset in [
            SLOT_A_OFFSET,
            SLOT_B_OFFSET,
            block.wal_offset,
            block.segments_offset,
        ] {
            assert_eq!(offset % u64::from(PAGE), 0, "{offset}");
        }
    }

    #[test]
    fn something_that_is_not_a_store_says_so_rather_than_reporting_damage() {
        let bytes = vec![0u8; SUPERBLOCK_LEN];
        assert_eq!(Superblock::decode(&bytes), Err(Error::BadMagic));
    }

    #[test]
    fn a_format_major_from_the_future_is_refused_by_number() {
        let mut block = superblock();
        block.major = MAJOR + 1;
        let bytes = block.encode();
        assert_eq!(
            Superblock::decode(&bytes),
            Err(Error::UnsupportedVersion {
                found: MAJOR + 1,
                expected: MAJOR,
            })
        );
    }

    #[test]
    fn a_format_minor_from_the_future_is_opened() {
        let mut block = superblock();
        block.minor = MINOR + 7;
        let bytes = block.encode();
        let read = Superblock::decode(&bytes).expect("a superblock");
        assert_eq!(read.minor, MINOR + 7);
    }

    #[test]
    fn a_page_size_this_build_does_not_use_is_refused() {
        let mut block = superblock();
        block.page = 8192;
        let bytes = block.encode();
        assert_eq!(
            Superblock::decode(&bytes),
            Err(Error::UnsupportedPageSize {
                found: 8192,
                expected: PAGE,
            })
        );
    }

    #[test]
    fn any_single_bit_changed_in_a_superblock_field_is_caught() {
        let bytes = superblock().encode();
        // The reserved tail is zeroes and is covered by the same checksum, so
        // walking the fields is enough to make the point without eight thousand
        // iterations of the same arithmetic.
        for byte in 8..72 {
            for bit in 0..8 {
                let mut damaged = bytes.clone();
                damaged[byte] ^= 1 << bit;
                assert!(
                    Superblock::decode(&damaged).is_err(),
                    "byte {byte} bit {bit} went unnoticed"
                );
            }
        }
    }

    #[test]
    fn a_manifest_is_one_slot_and_round_trips() {
        for segments in [0, 1, 2, 17, full()] {
            let written = manifest(4, segments);
            let bytes = written.encode().expect("a manifest");
            assert_eq!(bytes.len(), SLOT_LEN, "{segments} segments");
            assert_eq!(
                Manifest::decode(&bytes).expect("a manifest"),
                written,
                "{segments} segments"
            );
        }
    }

    #[test]
    fn a_manifest_with_more_segments_than_a_slot_holds_is_an_error() {
        let written = manifest(4, full() + 1);
        assert_eq!(
            written.encode(),
            Err(Error::TooManySegments {
                count: MAX_SEGMENTS + 1
            })
        );
    }

    #[test]
    fn a_segment_count_larger_than_the_slot_is_an_error_and_not_a_read_past_the_end() {
        let mut bytes = manifest(4, 3).encode().expect("a manifest");
        // The count sits at offset 40. Claiming a million segments is what a
        // flipped byte in the header looks like, and the table it describes
        // would run well past the end of the slot.
        bytes[40..44].copy_from_slice(&1_000_000u32.to_le_bytes());
        let body = SLOT_LEN - SUM_LEN;
        let sum = xxh3::hash128(&bytes[..body]);
        bytes[body..].copy_from_slice(&sum.to_le_bytes());
        assert_eq!(
            Manifest::decode(&bytes),
            Err(Error::TooManySegments { count: 1_000_000 })
        );
    }

    #[test]
    fn a_slot_that_was_never_written_does_not_decode_as_an_empty_manifest() {
        let untouched = vec![0u8; SLOT_LEN];
        assert!(matches!(
            Manifest::decode(&untouched),
            Err(Error::Xxh3Mismatch { .. })
        ));
    }

    #[test]
    fn recovery_takes_the_higher_epoch() {
        let older = manifest(7, 2).encode().expect("a manifest");
        let newer = manifest(8, 3).encode().expect("a manifest");
        let found = recover(&older, &newer).expect("a committed manifest");
        assert_eq!(found.slot, Slot::B);
        assert_eq!(found.manifest.epoch, 8);
        let found = recover(&newer, &older).expect("a committed manifest");
        assert_eq!(found.slot, Slot::A);
        assert_eq!(found.manifest.epoch, 8);
    }

    #[test]
    fn recovery_ignores_a_torn_slot_however_new_it_claims_to_be() {
        let good = manifest(7, 2).encode().expect("a manifest");
        let mut torn = manifest(9, 3).encode().expect("a manifest");
        // What a machine losing power halfway through a slot write leaves
        // behind: the beginning of the new manifest and the end of nothing.
        torn[512] ^= 0x40;
        let found = recover(&good, &torn).expect("a committed manifest");
        assert_eq!(found.slot, Slot::A);
        assert_eq!(found.manifest.epoch, 7);
    }

    #[test]
    fn recovery_with_one_slot_never_written_opens_on_the_other() {
        let only = manifest(1, 0).encode().expect("a manifest");
        let untouched = vec![0u8; SLOT_LEN];
        let found = recover(&only, &untouched).expect("a committed manifest");
        assert_eq!(found.slot, Slot::A);
        assert_eq!(found.manifest.epoch, 1);
    }

    #[test]
    fn recovery_with_neither_slot_readable_is_its_own_error() {
        let untouched = vec![0u8; SLOT_LEN];
        assert_eq!(recover(&untouched, &untouched), Err(Error::NoManifest));
    }

    #[test]
    fn the_next_commit_goes_to_the_slot_that_is_not_live() {
        assert_eq!(Slot::A.other(), Slot::B);
        assert_eq!(Slot::B.other(), Slot::A);
        assert_eq!(Slot::A.other().other(), Slot::A);
        assert_eq!(Slot::A.other().offset(), SLOT_B_OFFSET);
    }

    #[test]
    fn a_commit_moves_the_epoch_forward_and_nothing_else() {
        let before = manifest(4, 2);
        let after = before.next(1_700_000_500);
        assert_eq!(after.epoch, before.epoch + 1);
        assert_eq!(after.written, 1_700_000_500);
        assert_eq!(after.segments, before.segments);
        assert_eq!(after.live, before.live);
    }

    #[test]
    fn truncating_at_every_length_is_an_error_rather_than_a_panic() {
        let block = superblock().encode();
        for len in 0..block.len() {
            assert!(Superblock::decode(&block[..len]).is_err(), "{len}");
        }
        let slot = manifest(3, 5).encode().expect("a manifest");
        for len in (0..slot.len()).step_by(7) {
            assert!(Manifest::decode(&slot[..len]).is_err(), "{len}");
        }
    }

    #[test]
    fn trailing_bytes_after_a_region_are_ignored() {
        // A caller handing over a mapping of the whole file rather than an exact
        // slice is the normal case, not an error.
        let mut block = superblock().encode();
        block.extend_from_slice(&[0xff; 1024]);
        assert!(Superblock::decode(&block).is_ok());
        let mut slot = manifest(3, 5).encode().expect("a manifest");
        slot.extend_from_slice(&[0xff; 1024]);
        assert!(Manifest::decode(&slot).is_ok());
    }

    #[test]
    fn the_slot_holds_the_number_of_segments_the_constant_claims() {
        // The bound itself is asserted at compile time. This one is here so that
        // a change to the entry layout that quietly halves the capacity has to
        // be a deliberate edit to a written down number.
        assert_eq!(MAX_SEGMENTS, 1022);
    }
}
