//! The store file itself: opening one, creating one, and committing to one.
//!
//! Everything under this module works on byte ranges, on purpose, because a torn
//! write is a slice with a byte changed in it and there is no way to arrange a
//! real one reliably through a filesystem on five platforms. This is where those
//! byte ranges meet a descriptor, and it is deliberately small: read the front of
//! the file, decide which manifest is the committed one, and write a new
//! manifest in the way that makes the write all or nothing.
//!
//! # The order that matters
//!
//! A commit writes the whole manifest into whichever slot is not live, then
//! fsyncs, and that fsync is the commit point. Until it returns, the other slot
//! is still the committed state and a machine that lost power would come back to
//! it. After it returns, the slot just written has the higher epoch and is the
//! committed state.
//!
//! There is no second write to order after the first, because nothing points at
//! the live slot. The epoch decides, and it lives inside the region the slot's
//! own checksum covers. See [`crate::manifest`] for why that is not the design
//! it started as.
//!
//! # The log is a different promise
//!
//! A commit is durable when it returns. An append is not, and that is the point
//! of having both. Appending puts a record in the log region and returns, and
//! syncing puts everything appended so far on the platter, so a batch of writers
//! that arrived together pay for one fsync between them rather than one each.
//! Nothing has been promised to anybody until the sync returns, and what happens
//! between the two is the writer's business rather than this module's.
//!
//! Recovery reads the log forward from the head the last commit named. It does
//! not stop where that commit said the tail was, because the records past it are
//! the ones a store that stopped without warning needs, and it does not read the
//! whole region either, because that region is a quarter of a gigabyte of mostly
//! nothing. It reads a window at a time and follows the records through it.
//!
//! # Positioned reads and writes
//!
//! Every read and write here names its own offset rather than seeking first. Two
//! reasons. A seek and a read is two syscalls where one will do, and more to the
//! point a file position is state shared by everything holding the descriptor,
//! which is the kind of thing that works until the day something else opens the
//! same store.
//!
//! # Written through a descriptor, read through a mapping
//!
//! The superblock and the two manifest slots are a page and 128 KiB between
//! them, which is not enough to be worth a mapping, and a mapping would take the
//! choice of when bytes reach the platter away from the code that has to be sure
//! they have. So everything written here is written positioned and synced.
//!
//! The segment region is the other way round. It is the whole of the store by
//! size, it is only ever appended to and never edited, and a query touches a
//! small fraction of it, so reading it means [`Store::view`] and a mapping. See
//! [`crate::mapping`] for why reading it any other way put a number in the
//! results table that described a different program.
//!
//! # Segments arrive before the manifest that names them
//!
//! [`Store::append_segment`] writes a segment into the region and does not
//! commit anything, and it is durable when it returns. [`Store::commit`] then
//! writes a manifest that names it. That order is not an accident and it is not
//! reversible: a manifest naming a segment that is not on the platter yet is a
//! store that comes back pointing at a hole, where segment bytes that no
//! manifest names are bytes nothing will ever read. One of those is a lost
//! store and the other is wasted space.
//!
//! # Deletions arrive the same way, and never in place
//!
//! A set of deletions is the one thing about a segment that changes after it is
//! written, so it does not live in the segment. It lives in the same region,
//! beside it, and the manifest points at it. A newer set is appended somewhere
//! else and the manifest is pointed at that, which is the same order as a
//! segment and buys the same property: a machine that stops halfway comes back
//! either to the new set or to the old one.
//!
//! It also buys copy on write for free. A [`View`] holds the mapping and the
//! offsets it was made with, so a query already running is reading bytes no
//! writer will touch, and the space the older set is in comes back when a
//! compaction rewrites the segment rather than when the newer set is written.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

use crate::DocId;
use crate::bitmap::Bitmap;
use crate::error::Error;
use crate::index::{Keys, Reader};
use crate::manifest::{
    Committed, Manifest, PAGE, SLOT_A_OFFSET, SLOT_B_OFFSET, SLOT_LEN, SUPERBLOCK_LEN, Segment,
    Slot, Superblock,
};
use crate::mapping::Map;
use crate::wal::{self, MIN_RECORD, Record, Ring};

#[cfg(not(any(unix, windows)))]
compile_error!(
    "the fs feature needs positioned reads and writes, which exist on unix and on windows; \
     build with --no-default-features on anything else"
);

/// What can go wrong reading or writing a store.
///
/// Two kinds of fact, kept apart. The filesystem refusing is one thing and the
/// bytes it handed over not being a store is another, and the first question
/// after a failure is which of those happened, because one of them is a
/// permissions problem and the other is a restore from backup.
#[derive(Debug)]
#[non_exhaustive]
pub enum Trouble {
    /// The filesystem said no.
    Io(io::Error),
    /// The bytes are not the bytes a store is made of.
    Format(Error),
}

impl std::fmt::Display for Trouble {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => error.fmt(f),
            Self::Format(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for Trouble {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Format(error) => Some(error),
        }
    }
}

impl From<io::Error> for Trouble {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<Error> for Trouble {
    fn from(error: Error) -> Self {
        Self::Format(error)
    }
}

/// The result of opening, reading or committing to a store.
pub type Result<T> = std::result::Result<T, Trouble>;

/// An open store file.
///
/// It holds the descriptor, the superblock, and whichever manifest was the
/// committed one when it opened or when it last committed. It does not hold the
/// segments, which are read through their own mappings and outlive nothing.
#[derive(Debug)]
pub struct Store {
    /// The file, opened for reading and writing.
    file: File,
    /// The first page, which does not change while the store is open.
    superblock: Superblock,
    /// The committed state.
    manifest: Manifest,
    /// Which slot the committed state is in, so the next commit knows which one
    /// it is free to overwrite.
    slot: Slot,
    /// Where the log is up to, which runs ahead of the committed manifest
    /// between commits and is written into the next one.
    log: Ring,
    /// Where the next segment goes, which like the log runs ahead of the
    /// committed manifest between commits.
    segments: u64,
}

/// Where the segment region ends, according to a manifest.
///
/// Past the furthest segment it names rather than past the last one, because
/// segments are listed in the order they were added and a compaction takes
/// entries out of the middle, so the end of the list is not the end of the
/// region.
///
/// Rounded up to a page, and the gap that leaves is not reclaimed. It is at most
/// a page against a segment measured in megabytes, and every structural offset
/// in this format is page aligned so that a mapping of one does not begin in the
/// middle of a page it shares with something else.
fn end_of(superblock: &Superblock, manifest: &Manifest) -> u64 {
    let mut end = superblock.segments_offset;
    for segment in &manifest.segments {
        end = end.max(segment.offset.saturating_add(segment.len));
    }
    end.next_multiple_of(u64::from(PAGE))
}

impl Store {
    /// Creates a store where there is not one already.
    ///
    /// The identifier and the timestamp come from the caller, because this crate
    /// has neither a source of randomness nor a clock, and because a creation
    /// path that cannot be made to produce the same bytes twice cannot be
    /// tested.
    ///
    /// Fails if the path exists. Creating over a store that is already there
    /// destroys it, and that is not something to do by accident on the strength
    /// of one wrong argument.
    ///
    /// # Errors
    ///
    /// Returns [`Trouble::Io`] if the file cannot be created, written or synced.
    /// The manifest it writes describes no segments and cannot be too large for
    /// its slot.
    pub fn create(path: &Path, store: u128, created: u64) -> Result<Self> {
        Self::create_with_log(path, store, created, crate::manifest::DEFAULT_WAL_LEN)
    }

    /// Creates a store whose log region is a size of the caller's choosing.
    ///
    /// As [`Store::create`], except that the log is not the default quarter of a
    /// gigabyte. It is fixed for the life of the store, because the segments
    /// start where it ends.
    ///
    /// # Errors
    ///
    /// As [`Store::create`].
    pub fn create_with_log(path: &Path, store: u128, created: u64, wal_len: u64) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)?;
        let superblock = Superblock::with_log(store, created, wal_len);
        let manifest = Manifest::empty(created);
        // The file is given its full length up front so that the regions the
        // superblock names exist before anything claims they do. It is a sparse
        // file until something writes into it, so the log ring costs nothing on
        // disk until it is used.
        file.set_len(superblock.segments_offset)?;
        write_at(&file, &superblock.encode(), 0)?;
        write_at(&file, &manifest.encode()?, SLOT_A_OFFSET)?;
        // Everything, not just the data, because the length of the file is part
        // of what has to survive here.
        file.sync_all()?;
        let log = Ring::new(
            superblock.wal_len,
            manifest.wal_head,
            manifest.wal_tail,
            manifest.wal_sequence,
        )?;
        let segments = end_of(&superblock, &manifest);
        Ok(Self {
            file,
            superblock,
            manifest,
            slot: Slot::A,
            log,
            segments,
        })
    }

    /// Opens a store, at whichever manifest was last committed.
    ///
    /// # Errors
    ///
    /// Returns [`Trouble::Io`] if the file cannot be opened or read, and
    /// [`Trouble::Format`] if it is not a store, is a version this build does
    /// not read, is shorter than the regions it claims, or has no readable
    /// manifest in either slot.
    pub fn open(path: &Path) -> Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let mut page = [0u8; SUPERBLOCK_LEN];
        read_at(&file, &mut page, 0)?;
        let superblock = Superblock::decode(&page)?;
        let len = file.metadata()?.len();
        if len < superblock.segments_offset {
            return Err(Trouble::Format(Error::Truncated {
                needed: as_usize(superblock.segments_offset),
                available: as_usize(len),
            }));
        }
        let mut a = vec![0u8; SLOT_LEN];
        let mut b = vec![0u8; SLOT_LEN];
        read_at(&file, &mut a, SLOT_A_OFFSET)?;
        read_at(&file, &mut b, SLOT_B_OFFSET)?;
        let Committed { slot, manifest } = crate::manifest::recover(&a, &b)?;
        let log = Ring::new(
            superblock.wal_len,
            manifest.wal_head,
            manifest.wal_tail,
            manifest.wal_sequence,
        )?;
        let segments = end_of(&superblock, &manifest);
        Ok(Self {
            file,
            superblock,
            manifest,
            slot,
            log,
            segments,
        })
    }

    /// The first page of the store.
    #[must_use]
    pub const fn superblock(&self) -> &Superblock {
        &self.superblock
    }

    /// The committed state.
    #[must_use]
    pub const fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// Which slot the committed state is in.
    #[must_use]
    pub const fn slot(&self) -> Slot {
        self.slot
    }

    /// Where the log is up to.
    ///
    /// Ahead of the committed manifest whenever something has been appended
    /// since the last commit, which is the ordinary state of a store that is
    /// being written to.
    #[must_use]
    pub const fn log(&self) -> &Ring {
        &self.log
    }

    /// Appends a record to the log and returns the sequence it was given.
    ///
    /// This does not make anything durable. The bytes are with the operating
    /// system when it returns and on the platter when [`Store::sync`] returns,
    /// and the gap between those two is deliberate: it is what lets a batch of
    /// writers that arrived together share one fsync instead of paying for one
    /// each. Deciding when to close that gap is the writer's job and not this
    /// one's.
    ///
    /// # Errors
    ///
    /// Returns [`Trouble::Format`] with [`Error::LogFull`] if the record does
    /// not fit in what the log has free, which is a signal to flush a segment
    /// and truncate rather than a failure of the store, and [`Trouble::Io`] if
    /// the write fails. Either way the log is where it was, because the ring
    /// only moves once the bytes are written.
    pub fn append(&mut self, kind: u32, payload: &[u8]) -> Result<u64> {
        let placement = self.log.place(payload.len())?;
        let base = self.superblock.wal_offset;
        if let Some((at, span)) = placement.pad {
            let mut bytes = Vec::with_capacity(MIN_RECORD);
            wal::encode_pad(span, placement.sequence, at, &mut bytes);
            write_at(&self.file, &bytes, base + self.log.physical(at))?;
        }
        let mut bytes = Vec::with_capacity(as_usize(u64::from(placement.span)));
        wal::encode(kind, placement.sequence, payload, placement.at, &mut bytes)?;
        write_at(&self.file, &bytes, base + self.log.physical(placement.at))?;
        Ok(self.log.taken(&placement))
    }

    /// Puts everything appended since the last one on the platter.
    ///
    /// # Errors
    ///
    /// Returns [`Trouble::Io`] if the sync fails, which is the one failure that
    /// cannot be recovered from by trying again, since the platform is entitled
    /// to have thrown the writes away.
    pub fn sync(&self) -> Result<()> {
        self.file.sync_data()?;
        Ok(())
    }

    /// Frees the log up to `through`.
    ///
    /// Called once the records before that position are in a segment and the
    /// manifest naming that segment has been committed. Nothing is written here:
    /// the head moves in memory and reaches the file with the next commit, which
    /// is the right order, because a head that ran ahead of the commit would
    /// free records the store still needs.
    ///
    /// # Errors
    ///
    /// Returns [`Trouble::Format`] with [`Error::BadPositions`] if the position
    /// is behind the head or past the tail.
    pub fn truncate_log(&mut self, through: u64) -> Result<()> {
        self.log.truncate(through)?;
        Ok(())
    }

    /// Commits a new manifest, and returns the epoch it was given.
    ///
    /// The epoch is assigned here rather than taken from the caller. Everything
    /// about recovery rests on it never going backwards, and a number that
    /// decides which of two states a store comes back as is not a number to let
    /// anybody set by hand.
    ///
    /// When this returns, the new state is on the platter. If the machine had
    /// stopped at any point before it returned, the store would come back at the
    /// state before this call, whole.
    ///
    /// The log positions are taken from the log rather than from the caller, for
    /// the same reason the epoch is. They say which records the store still
    /// needs, and a manifest that disagrees with the ring it describes is a
    /// store that either replays records it has already applied or loses records
    /// it has not.
    ///
    /// # Errors
    ///
    /// Returns [`Trouble::Format`] if the manifest is too large for a slot or if
    /// it puts a segment's deletions back to an older generation than the
    /// committed one, and [`Trouble::Io`] if the write or the sync fails. In
    /// every case the store is still at the state it was at, because the slot
    /// being written is the one nothing is reading.
    pub fn commit(&mut self, manifest: Manifest, written: u64) -> Result<u64> {
        Self::deletions_only_accumulate(&self.manifest, &manifest)?;
        let mut next = manifest;
        next.epoch = self.manifest.epoch.saturating_add(1);
        next.written = written;
        next.wal_head = self.log.head();
        next.wal_tail = self.log.tail();
        next.wal_sequence = self.log.sequence();
        let bytes = next.encode()?;
        let slot = self.slot.other();
        write_at(&self.file, &bytes, slot.offset())?;
        // The data and not the metadata, because the file has not changed length
        // and nothing here depends on its timestamps. On the platforms that tell
        // the difference this is the cheaper of the two.
        self.file.sync_data()?;
        self.manifest = next;
        self.slot = slot;
        Ok(self.manifest.epoch)
    }

    /// Refuses a commit that puts a segment's deletions back to an older set.
    ///
    /// A segment is matched by where it is and by the digest of its own footer,
    /// so the check is about the same segment rather than about the same place
    /// in the file: a compaction that dropped a segment and an append that later
    /// reused the space is a different segment starting again at nothing, which
    /// is not a step backwards. The footer digest is zero on every segment this
    /// engine has written so far, so today the match is by offset alone and the
    /// reuse case would be refused. Nothing reuses an offset yet, because the
    /// region is append only and space comes back by rewriting the file, and the
    /// check reads correctly the moment the digest is filled in.
    ///
    /// Deletions only accumulate until a compaction rewrites the segment, so a
    /// generation that goes backwards means two writers hold different ideas of
    /// what is deleted, and committing the older one brings documents back from
    /// the dead. Refusing costs a walk of two short lists per commit.
    fn deletions_only_accumulate(committed: &Manifest, next: &Manifest) -> Result<()> {
        for segment in &next.segments {
            let Some(was) = committed
                .segments
                .iter()
                .find(|old| old.offset == segment.offset && old.footer == segment.footer)
            else {
                continue;
            };
            if segment.generation < was.generation {
                return Err(Trouble::Format(Error::StaleGeneration {
                    offset: segment.offset,
                    committed: was.generation,
                    given: segment.generation,
                }));
            }
        }
        Ok(())
    }

    /// Where a segment appended now would start.
    ///
    /// Kept here rather than worked out from the manifest each time, and that is
    /// the whole point of it. A store with several segments to write appends
    /// them all and commits once, so between the first append and the commit the
    /// manifest names none of them, and a placement read out of the manifest
    /// would put every one of them at the same offset on top of the last. Which
    /// it did, until a test asked where three segments had gone.
    ///
    /// It only ever moves forward while a store is open. Reopening puts it back
    /// to the end of what the committed manifest names, so the space under
    /// segments that were appended and never committed, or that a compaction has
    /// since dropped, comes back the next time the store is opened rather than
    /// during.
    #[must_use]
    pub const fn segments_end(&self) -> u64 {
        self.segments
    }

    /// Writes a segment into the segment region and says where it went.
    ///
    /// The bytes are on the platter when this returns, and nothing points at
    /// them yet. Naming them is [`Store::commit`], and doing it in that order is
    /// what makes a store that stops in the middle of a write recoverable: the
    /// worst this can leave behind is a stretch of bytes no manifest mentions,
    /// which the next append writes over.
    ///
    /// The count and the timestamp are the caller's because this crate has no
    /// clock, and because a segment's own header already holds both and having
    /// this read them back out would mean opening a segment to find out what the
    /// thing that just built it already knew.
    ///
    /// # Errors
    ///
    /// Returns [`Trouble::Io`] if the write or the sync fails. Nothing has been
    /// committed either way, so a failure here leaves the store at the state it
    /// was at.
    pub fn append_segment(&mut self, bytes: &[u8], docs: u32, created: u64) -> Result<Segment> {
        self.append_segment_with(docs, created, |into| {
            use io::Write as _;
            into.write_all(bytes)
        })
    }

    /// Writes a segment into the segment region as it is produced.
    ///
    /// The same thing [`Store::append_segment`] does, for a caller that has the
    /// parts of a segment rather than the segment. [`crate::segment::Writer`]
    /// can write itself into anything that takes bytes, and this is the thing to
    /// hand it when the bytes are going into a store, because the alternative is
    /// building the whole segment in memory next to the sections it was built
    /// out of and then copying it in. On a real corpus that copy is tens of
    /// megabytes held for no reason other than the shape of the call.
    ///
    /// What the closure writes goes straight into the file at the offset the
    /// segment starts at, in order, with no buffer in between. The descriptor
    /// that comes back covers however much it wrote.
    ///
    /// Everything [`Store::append_segment`] promises holds here too: the bytes
    /// are on the platter when this returns and nothing points at them until a
    /// commit names them.
    ///
    /// # Errors
    ///
    /// Returns [`Trouble::Io`] with whatever the closure failed with, or with
    /// the failure of the sync afterwards. A closure that gave up halfway leaves
    /// the store where it was, since the cursor only moves on success and a
    /// stretch of bytes no manifest mentions is what the next append writes over.
    pub fn append_segment_with(
        &mut self,
        docs: u32,
        created: u64,
        write: impl FnOnce(&mut Appending<'_>) -> io::Result<()>,
    ) -> Result<Segment> {
        let offset = self.segments;
        let mut appending = Appending {
            file: &self.file,
            at: offset,
            written: 0,
        };
        write(&mut appending)?;
        let len = appending.written;
        // Everything and not just the data, because the file has grown and the
        // length is as much a part of what has to survive as the bytes are. A
        // sync that left the size behind would give back a store whose segment
        // is inside a file that ends before it.
        self.file.sync_all()?;
        // Only after the bytes are down. A cursor that moved first and then
        // failed to write would leave a gap that reads as a segment nobody
        // wrote, and the next append would put a real one after it.
        self.segments = offset.saturating_add(len).next_multiple_of(u64::from(PAGE));
        Ok(Segment {
            offset,
            len,
            docs,
            created,
            ..Segment::default()
        })
    }

    /// Writes a set of deletions for a segment and says where it went.
    ///
    /// The set goes into the segment region beside the segment it is about,
    /// because a segment is immutable and a set of deletions is the opposite: it
    /// is the one thing about a segment that changes after it is written. What
    /// comes back is the descriptor to commit, which is the one handed in with
    /// the tombstone fields pointing at the bytes just written and the
    /// generation moved on by one.
    ///
    /// Nothing is overwritten, ever. A newer set is written somewhere else and
    /// the manifest is pointed at it, which is what makes a delete atomic and
    /// what makes it safe for a query that is already running: a view holds the
    /// mapping and the offsets it was made with, so the bytes underneath it are
    /// ones no writer will touch again. The space the older set is in comes back
    /// when a compaction rewrites the segment, and not before.
    ///
    /// An empty set clears the descriptor rather than writing nothing down, so a
    /// segment whose deletions were all undone by a compaction is a segment with
    /// no tombstones rather than one pointing at an empty bitmap.
    ///
    /// # Errors
    ///
    /// Returns [`Trouble::Format`] with [`Error::NoSuchDocument`] if the set
    /// names a document the segment does not have, which means it was built
    /// against a different segment, and [`Trouble::Io`] if the write or the sync
    /// fails. Nothing is committed either way.
    pub fn append_tombstones(&mut self, segment: &Segment, deleted: &Bitmap) -> Result<Segment> {
        if let Some(doc) = deleted.max()
            && doc >= segment.docs
        {
            return Err(Trouble::Format(Error::NoSuchDocument {
                doc,
                documents: segment.docs,
            }));
        }
        let mut next = *segment;
        next.generation = segment.generation.saturating_add(1);
        if deleted.is_empty() {
            next.tombstones_offset = 0;
            next.tombstones_len = 0;
            next.first_live = 0;
            return Ok(next);
        }

        let mut bytes = Vec::with_capacity(deleted.size());
        deleted.write_to(&mut bytes);
        let offset = self.segments;
        write_at(&self.file, &bytes, offset)?;
        // Everything and not just the data, for the reason the segment append
        // gives: the file has grown and its length has to survive with the
        // bytes.
        self.file.sync_all()?;
        let len = bytes.len() as u64;
        self.segments = offset.saturating_add(len).next_multiple_of(u64::from(PAGE));

        next.tombstones_offset = offset;
        next.tombstones_len = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
        next.first_live = deleted.first_absent().min(segment.docs);
        Ok(next)
    }

    /// Deletes documents from one segment and commits it.
    ///
    /// The set is the whole answer for that segment rather than a change to it,
    /// which is what a set of deletions is on disk, so a caller that wants to
    /// delete one more document passes the set it had with one more in it. That
    /// is the only version that is idempotent: replaying the same call twice
    /// leaves the same store, where applying a change twice would not.
    ///
    /// The write and the commit are the two halves of the same promise and this
    /// does both, which is why it is here rather than left to a caller holding
    /// [`append_tombstones`](Self::append_tombstones): the bitmap is on the
    /// platter before the manifest points at it, and the manifest pointing at it
    /// is one fsync of one slot, so a machine that stops anywhere in here comes
    /// back either to the deletions or to the ones before them.
    ///
    /// The live count is kept honest across the call, because that is what a
    /// compaction decides from and what a store reports when asked how many
    /// documents it holds. It is worked out from what the segment had deleted
    /// before, which means reading the older set back, and that is a few
    /// kilobytes against a commit.
    ///
    /// # Errors
    ///
    /// Returns [`Trouble::Format`] with [`Error::MissingSection`] if there is no
    /// such segment, [`Error::NoSuchDocument`] if the set names a document the
    /// segment does not have, a decoding error if the set already committed
    /// cannot be read back, and [`Trouble::Io`] if any of the writes fail.
    pub fn delete(&mut self, at: usize, deleted: &Bitmap, written: u64) -> Result<u64> {
        let segment = self
            .manifest
            .segments
            .get(at)
            .copied()
            .ok_or(Error::MissingSection { kind: 0 })?;
        let before = self.committed_deletions(&segment)?;
        let next = self.append_tombstones(&segment, deleted)?;
        let mut manifest = self.manifest.clone();
        manifest.segments[at] = next;
        // Back out what this segment used to hide and take off what it hides
        // now, rather than counting the whole store again. Saturating on both
        // sides because a manifest that was already wrong about its own count is
        // not worth turning into a panic.
        manifest.live = manifest
            .live
            .saturating_add(before)
            .saturating_sub(deleted.len() as u64);
        self.commit(manifest, written)
    }

    /// Publishes a segment and deletions across any number of segments in one
    /// commit.
    ///
    /// This is what replacing a document takes. The new copy and the deletion of
    /// the old one have to become visible together, because a store that shows
    /// both is a store that answers twice and a store that shows neither is a
    /// store that lost a document, and the old copy is usually not in the same
    /// segment as anything else being deleted in the same batch.
    ///
    /// Each set is the whole answer for its segment rather than a change to it,
    /// the same rule [`delete`](Self::delete) follows and for the same reason: it
    /// is the only form that can be replayed. A caller adding one document to
    /// what a segment already hides passes the set it read back with one more in
    /// it.
    ///
    /// Everything is on the platter before the manifest names any of it. The
    /// segment is written and synced, then each bitmap is written and synced,
    /// and only then does one manifest naming all of them go into the other slot
    /// and get fsynced. A machine that stops anywhere in here comes back to the
    /// store as it was, with some bytes in the segment region that nothing
    /// points at, which is what the region being append only makes harmless.
    ///
    /// Passing `None` for the segment is a commit of deletions alone, which is
    /// what deleting several documents that happen to live in different segments
    /// looks like.
    ///
    /// # Errors
    ///
    /// Returns [`Trouble::Format`] with [`Error::RepeatedSegment`] if two sets
    /// name the same segment, [`Error::MissingSection`] if a set names a segment
    /// that is not there, [`Error::NoSuchDocument`] if a set names a document its
    /// segment does not have, [`Error::TooManySegments`] if the new segment does
    /// not fit the manifest, a decoding error if a set already committed cannot
    /// be read back, and [`Trouble::Io`] if any write, sync or commit fails.
    /// Nothing is committed unless all of it is.
    pub fn publish(
        &mut self,
        segment: Option<(&[u8], u32)>,
        created: u64,
        deletions: &[(usize, Bitmap)],
        written: u64,
    ) -> Result<u64> {
        for (n, (at, _)) in deletions.iter().enumerate() {
            if deletions[..n].iter().any(|(earlier, _)| earlier == at) {
                return Err(Trouble::Format(Error::RepeatedSegment { at: *at }));
            }
            if *at >= self.manifest.segments.len() {
                return Err(Trouble::Format(Error::MissingSection { kind: 0 }));
            }
        }

        // Read what each of them hides now before anything is written, because
        // the live count is worked out from the difference and a half written
        // batch should not have moved it.
        let mut before = Vec::with_capacity(deletions.len());
        for (at, _) in deletions {
            before.push(self.committed_deletions(&self.manifest.segments[*at])?);
        }

        let mut manifest = self.manifest.clone();
        if let Some((bytes, docs)) = segment {
            let described = self.append_segment(bytes, docs, created)?;
            manifest.segments.push(described);
            manifest.total = manifest.total.saturating_add(u64::from(docs));
            manifest.live = manifest.live.saturating_add(u64::from(docs));
        }
        for ((at, deleted), was) in deletions.iter().zip(before) {
            let described = self.manifest.segments[*at];
            let next = self.append_tombstones(&described, deleted)?;
            manifest.segments[*at] = next;
            manifest.live = manifest
                .live
                .saturating_add(was)
                .saturating_sub(deleted.len() as u64);
        }
        self.commit(manifest, written)
    }

    /// How many documents a committed descriptor says are deleted.
    ///
    /// Read back rather than remembered, because the store does not hold the
    /// sets and a process that opened the file a moment ago has no idea what the
    /// last one did.
    fn committed_deletions(&self, segment: &Segment) -> Result<u64> {
        if segment.tombstones_offset == 0 || segment.tombstones_len == 0 {
            return Ok(0);
        }
        let mut bytes = vec![0u8; segment.tombstones_len as usize];
        read_at(&self.file, &mut bytes, segment.tombstones_offset)?;
        Ok(Bitmap::read(&bytes)?.len() as u64)
    }

    /// Maps the store and hands back the bytes of every segment in it.
    ///
    /// This is how a query gets at a store. The mapping is of the whole file,
    /// which costs nothing it does not use, because a mapping is a promise
    /// rather than a read and the log region is a quarter of a gigabyte of
    /// mostly untouched sparse file.
    ///
    /// The segments are checked against the file here rather than as each one is
    /// handed out, so a view that exists is a view where every slice is inside
    /// the mapping and inside the segment region. That is worth the loop: a
    /// descriptor pointing into the manifest slots would otherwise be a query
    /// quietly reading a manifest as though it were postings.
    ///
    /// # Errors
    ///
    /// Returns [`Trouble::Io`] if the file cannot be mapped, and
    /// [`Trouble::Format`] with [`Error::Truncated`] if a segment the manifest
    /// names is not entirely inside the file, or [`Error::SectionOutOfRange`] if
    /// one starts before the segment region does. The same two apply to the
    /// tombstone bitmaps, which are checked here for the same reason and at the
    /// same time: a view that exists is one where every slice it can hand out is
    /// known to be inside the mapping and inside the segment region.
    pub fn view(&self) -> Result<View> {
        let map = Map::of(&self.file)?;
        // The same check a reader with only the bytes has to make, out of the
        // same function, so that a descriptor a view refuses is a descriptor the
        // command line tool refuses too. Two implementations of this would
        // disagree eventually, and the way they would disagree is that one of
        // them lets a bad descriptor through.
        let ranges = crate::manifest::locate(&self.superblock, &self.manifest, map.len())?;
        let deletions = crate::manifest::tombstones(&self.superblock, &self.manifest, map.len())?;
        Ok(View {
            map,
            segments: self.manifest.segments.clone(),
            ranges,
            deletions,
        })
    }

    /// Walks the records the log holds and hands each one to `each`.
    ///
    /// This is what a store does after opening one that was not closed: the
    /// manifest says what is in segments, and the log says what was promised
    /// after that and never got there. Records arrive oldest first, and when it
    /// returns the log is positioned at the end of what was really written, so
    /// the next append carries on from there.
    ///
    /// The walk starts at the committed head and stops at the first record that
    /// is damaged, that runs off the end of the region, or whose sequence does
    /// not continue the one before it. It does not stop at the committed tail,
    /// because a store that stopped without warning has records past it and
    /// those are exactly the ones worth having.
    ///
    /// One case is not covered yet. A record torn in the middle of a run leaves
    /// intact records from that same run behind it, at the positions and with
    /// the numbering they were written with. The walk stops at the torn one, as
    /// it should, but if the next record appended over it happens to be exactly
    /// as long as the record it replaced, then the intact one behind it lines up
    /// again and a later walk reads it. It was a real record, so nothing catches
    /// it, and replaying it applies a write the store never promised. Closing
    /// that means resuming on a fresh lap after an unclean recovery, which needs
    /// the checkpoint path this does not have yet.
    ///
    /// # Errors
    ///
    /// Returns [`Trouble::Io`] if the log cannot be read. Damage in the log is
    /// not an error: it is where the log ends, and a torn record at the end is
    /// the ordinary shape of a machine that lost power midway through a write.
    pub fn recover(&mut self, mut each: impl FnMut(&Record<'_>)) -> Result<u64> {
        let ring = self.superblock.wal_len;
        let base = self.superblock.wal_offset;
        let head = self.manifest.wal_head;
        // One lap and no more. Past that the walk would be reading records it
        // has already read, and a ring that somehow chains all the way round
        // would otherwise never end.
        let stop = head.saturating_add(ring);
        let mut position = head;
        let mut expected: Option<u64> = None;
        let mut window = vec![0u8; WINDOW];
        let mut count = 0u64;
        'walk: while position < stop {
            let physical = position % ring;
            let lap = ring - physical;
            if lap < MIN_RECORD as u64 {
                // Too close to the end of the region for a record to start, so
                // the writer wrapped here and so does this.
                position += lap;
                continue;
            }
            let want = as_usize(lap.min(window.len() as u64));
            read_at(&self.file, &mut window[..want], base + physical)?;
            let mut offset = 0;
            while offset + MIN_RECORD <= want {
                let span = as_usize(u64::from(span_of(&window[offset..])?));
                if span > want - offset {
                    if span as u64 > lap - offset as u64 {
                        // A record cannot claim more than the lap it is in, so
                        // whatever this is, it is not one.
                        break 'walk;
                    }
                    // It runs past the window rather than past the region, so
                    // widen the window and read it again from where it starts.
                    if span > window.len() {
                        window.resize(span, 0);
                    }
                    break;
                }
                let Ok(record) = wal::decode(&window[offset..offset + span], position) else {
                    break 'walk;
                };
                if expected.is_some_and(|expected| record.sequence != expected) {
                    break 'walk;
                }
                offset += span;
                position += span as u64;
                if record.kind == wal::kind::PAD {
                    expected = Some(record.sequence);
                    continue;
                }
                expected = Some(record.sequence.saturating_add(1));
                each(&Record { position, ..record });
                count += 1;
            }
        }
        // The committed sequence is a floor and not a starting point. A replay
        // that ended early because a record was torn would otherwise hand out
        // numbers the ring has already seen, and those are the one thing that
        // tells a stale lap from a live one.
        let sequence = expected
            .unwrap_or(self.manifest.wal_sequence)
            .max(self.manifest.wal_sequence);
        self.log = Ring::new(ring, head, position, sequence)?;
        Ok(count)
    }
}

/// The segments of a store, mapped, ready to be read.
///
/// Made by [`Store::view`]. It holds the mapping, so the slices it hands out
/// live exactly as long as it does, which is the chain a query is built on: the
/// view owns the bytes, a [`Segment`](crate::segment::Segment) borrows a slice
/// of them, a [`Reader`] borrows the segment, and a
/// [`Searcher`](crate::search::Searcher) borrows the readers. None of it is
/// copied and none of it can outlive the mapping underneath.
///
/// It is a snapshot rather than a live handle. The segments are the ones the
/// manifest named when the view was taken, and a commit after that does not
/// reach it. That is the behaviour a query wants: a page of results computed
/// halfway across a set of segments that changed underneath it is a page that
/// never described anything.
#[derive(Debug)]
pub struct View {
    /// The whole store file.
    map: Map,
    /// The segments the manifest named when this was taken.
    segments: Vec<Segment>,
    /// Where each of them sits in the mapping, checked once when the view was
    /// taken so that every slice below is known to be inside it.
    ranges: Vec<core::ops::Range<usize>>,
    /// Where each segment's deletions sit, for the segments that have any,
    /// checked at the same time and against the same rules.
    deletions: Vec<Option<core::ops::Range<usize>>>,
}

impl View {
    /// How many segments there are.
    #[must_use]
    pub fn len(&self) -> usize {
        self.segments.len()
    }

    /// Whether there are none, which is a store nothing has been written to.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.segments.is_empty()
    }

    /// What the manifest said about the segments.
    #[must_use]
    pub fn described(&self) -> &[Segment] {
        &self.segments
    }

    /// The bytes of one segment, or `None` when there is no such segment.
    ///
    /// Infallible for an index that is in range, because [`Store::view`] already
    /// checked every descriptor against the mapping. There is nothing left here
    /// that can fail on bytes.
    #[must_use]
    pub fn bytes(&self, at: usize) -> Option<&[u8]> {
        self.map.get(self.ranges.get(at)?.clone())
    }

    /// Every segment's bytes, oldest first.
    pub fn all(&self) -> impl Iterator<Item = &[u8]> {
        (0..self.len()).filter_map(|at| self.bytes(at))
    }

    /// The bytes of one segment's deletions, or `None` where it has none.
    ///
    /// Infallible for the same reason [`bytes`](Self::bytes) is: the descriptor
    /// was checked against the mapping when the view was taken.
    #[must_use]
    pub fn tombstones(&self, at: usize) -> Option<&[u8]> {
        self.map.get(self.deletions.get(at)?.clone()?)
    }

    /// Which of one segment's documents are deleted, or `None` where none are.
    ///
    /// # Errors
    ///
    /// Returns a decoding error if the bytes the descriptor points at are not a
    /// bitmap. That is a store that has been damaged, and reading it as no
    /// deletions would answer with documents somebody deleted.
    pub fn deleted(&self, at: usize) -> Result<Option<Bitmap>> {
        match self.tombstones(at) {
            Some(bytes) => Ok(Some(Bitmap::read(bytes)?)),
            None => Ok(None),
        }
    }

    /// Opens one segment for searching, with its deletions already applied.
    ///
    /// This is the way to get a reader out of a store, and the reason it exists
    /// rather than leaving the caller to open the bytes is that applying the
    /// deletions is then not a step anybody can forget. A reader that came from
    /// here cannot answer with a document the store says is gone.
    ///
    /// # Errors
    ///
    /// Returns [`Trouble::Format`] if there is no such segment, if the bytes are
    /// not a segment or not an index, if the deletions do not decode, or if they
    /// name a document the segment does not have, which means the manifest is
    /// pointing at a bitmap belonging to something else.
    pub fn reader(&self, at: usize) -> Result<Reader<'_>> {
        let bytes = self.bytes(at).ok_or(Error::MissingSection { kind: 0 })?;
        let segment = crate::segment::Segment::open(bytes)?;
        let reader = Reader::open(&segment)?;
        match self.deleted(at)? {
            Some(deleted) => Ok(reader.hiding(deleted)?),
            None => Ok(reader),
        }
    }

    /// The document a key names, or `None` if nothing live in the store holds
    /// it.
    ///
    /// One lookup, for a caller that has one key to ask about.
    ///
    /// A caller with more than one should take a [`Lookup`] and keep it, because
    /// this opens every segment's key index and throws it away again, and that
    /// is most of what a lookup costs. Measured on a ten segment store of eleven
    /// thousand documents keyed by path, this way is 1.1 microseconds a key and
    /// a handle that is already open is 169 nanoseconds.
    ///
    /// # Errors
    ///
    /// As [`Lookup::document`].
    pub fn document(&self, key: &[u8]) -> Result<Option<(usize, DocId)>> {
        self.lookup()?.document(key)
    }

    /// Opens the key index of every segment, ready to be asked about many keys.
    ///
    /// # Errors
    ///
    /// Returns a decoding error if a segment or one of its key sections is not
    /// what it claims to be, or if a set of deletions does not decode.
    pub fn lookup(&self) -> Result<Lookup<'_>> {
        let mut keys = Vec::with_capacity(self.len());
        let mut deleted = Vec::with_capacity(self.len());
        for at in 0..self.len() {
            let bytes = self.bytes(at).ok_or(Error::MissingSection { kind: 0 })?;
            // Without the digests, which would read every segment through. The
            // structure is still checked, so every slice this hands out is
            // inside the mapping, and a store that wants its contents proved has
            // a verify pass for that rather than a check on the query path.
            let segment = crate::segment::Segment::open_without_checksum(bytes)?;
            keys.push(Keys::open(&segment)?);
            deleted.push(self.deleted(at)?);
        }
        Ok(Lookup { keys, deleted })
    }

    /// Opens every segment, oldest first, each with its deletions applied.
    ///
    /// The order is the order the manifest lists them in, which is the order a
    /// hit's identifier is worked out from, so it is the order a searcher has to
    /// be given them in.
    ///
    /// # Errors
    ///
    /// As [`reader`](Self::reader), for the first segment that has one.
    pub fn readers(&self) -> Result<Vec<Reader<'_>>> {
        (0..self.len()).map(|at| self.reader(at)).collect()
    }
}

/// The key index of every segment in a view, open and ready to be asked.
///
/// Made by [`View::lookup`]. It exists because opening the key index of a
/// segment is most of what a lookup costs, and a caller with a batch of keys to
/// resolve, which is every ingest that has to decide what is new and what is a
/// replacement, would otherwise pay that for every key it asks about.
///
/// It is a snapshot of a snapshot: the view it came from is already the segments
/// the manifest named when the view was taken, and this holds the key sections
/// of exactly those. A commit after that does not reach either.
#[derive(Debug)]
pub struct Lookup<'a> {
    /// One entry per segment, `None` for a segment nobody named a document in.
    keys: Vec<Option<Keys<'a>>>,
    /// Their deletions, decoded once for the same reason the keys are opened
    /// once.
    deleted: Vec<Option<Bitmap>>,
}

impl Lookup<'_> {
    /// How many segments it covers.
    #[must_use]
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Whether it covers none, which is a store nothing has been written to.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// How many of the segments carry keys at all.
    #[must_use]
    pub fn named(&self) -> usize {
        self.keys.iter().filter(|keys| keys.is_some()).count()
    }

    /// The document a key names, newest segment first, or `None` if nothing live
    /// holds it.
    ///
    /// The answer is which segment and which document inside it, because that
    /// pair is what everything else here takes: the deletions are per segment
    /// and so is the numbering.
    ///
    /// Newest first is what makes replacing a document work. A key written twice
    /// is in two segments, both of them true when they were written, and the
    /// later one is the one that means anything now. The walk stops at the
    /// newest segment that names the key at all, so an older copy is never
    /// reached.
    ///
    /// It stops there even when that document has been deleted, and answers
    /// nothing. The alternative is to carry on and answer with the copy an
    /// earlier segment holds, which would bring a document back from the dead
    /// the moment somebody deleted the one that replaced it. A key whose newest
    /// document is gone is a key nothing holds.
    ///
    /// Every segment but the one holding the key answers from its filter, which
    /// is one cache line each and no decoding at all.
    ///
    /// # Errors
    ///
    /// None yet. It returns a result because the deletions it consults are read
    /// when the handle is made, and moving that read to where it is used is a
    /// change this signature should survive.
    pub fn document(&self, key: &[u8]) -> Result<Option<(usize, DocId)>> {
        for at in (0..self.keys.len()).rev() {
            let Some(keys) = self.keys[at].as_ref() else {
                continue;
            };
            let Some(doc) = keys.get(key) else { continue };
            let gone = self.deleted[at]
                .as_ref()
                .is_some_and(|deleted| deleted.contains(doc));
            return Ok((!gone).then_some((at, doc)));
        }
        Ok(None)
    }

    /// The key index of one segment, or `None` where there is none or no such
    /// segment.
    #[must_use]
    pub fn keys(&self, at: usize) -> Option<&Keys<'_>> {
        self.keys.get(at)?.as_ref()
    }
}

/// The segment region of a store, open at the end and taking bytes.
///
/// [`Store::append_segment_with`] hands one of these to whatever is producing a
/// segment. Everything written goes where it is going as it arrives, positioned
/// after what came before it, so nothing is ever held twice.
///
/// There is no buffer under this and there is deliberately not one. A segment is
/// written as a header, a table and then one payload per section, which is a
/// handful of writes of the size the sections already are, and putting a buffer
/// in the way would put back exactly the copy this exists to avoid.
#[derive(Debug)]
pub struct Appending<'a> {
    /// The store's descriptor, borrowed for as long as the write takes.
    file: &'a File,
    /// Where the segment starts.
    at: u64,
    /// How much of it has been written so far.
    written: u64,
}

impl Appending<'_> {
    /// How many bytes have gone in so far.
    #[must_use]
    pub const fn written(&self) -> u64 {
        self.written
    }
}

impl io::Write for Appending<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // All of it or none of it, because a positioned write of the whole
        // slice is what both platforms give and reporting a short write here
        // would only make the caller ask again for bytes already written.
        self.write_all(buf)?;
        Ok(buf.len())
    }

    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        write_at(self.file, buf, self.at.saturating_add(self.written))?;
        self.written = self.written.saturating_add(buf.len() as u64);
        Ok(())
    }

    /// Nothing to do, since there is nothing held back.
    ///
    /// Durability is the store's business and it is an fsync, which happens once
    /// the whole segment is down rather than whenever a caller asks.
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// How much of the log a recovery reads at a time.
///
/// Big enough that one read covers many records, so a recovery is a handful of
/// them rather than one per record, and small enough to be nothing beside the
/// region, which is a quarter of a gigabyte by default and not something to pull
/// into memory to find out that it is empty.
const WINDOW: usize = 1 << 20;

/// The length a record at the front of `bytes` claims.
fn span_of(bytes: &[u8]) -> Result<u32> {
    let (span, _) = crate::codec::get_u32(bytes)?;
    Ok(span)
}

/// Reads exactly `buf.len()` bytes from `offset`.
#[cfg(unix)]
fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(file, buf, offset)
}

/// Writes all of `buf` at `offset`.
#[cfg(unix)]
fn write_at(file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
    std::os::unix::fs::FileExt::write_all_at(file, buf, offset)
}

/// Reads exactly `buf.len()` bytes from `offset`.
///
/// Windows has no exact form of a positioned read, so this is the loop that
/// would be inside one. A short read is not an error on its own, and an
/// interrupted one is not an error at all.
#[cfg(windows)]
fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    let mut buf = buf;
    let mut offset = offset;
    while !buf.is_empty() {
        match file.seek_read(buf, offset) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "the store ended before the region it claims to have",
                ));
            }
            Ok(read) => {
                buf = &mut buf[read..];
                offset += read as u64;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Writes all of `buf` at `offset`.
#[cfg(windows)]
fn write_at(file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    let mut buf = buf;
    let mut offset = offset;
    while !buf.is_empty() {
        match file.seek_write(buf, offset) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "the store would not take the bytes",
                ));
            }
            Ok(written) => {
                buf = &buf[written..];
                offset += written as u64;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// A file offset as a length, for an error that reports both.
fn as_usize(value: u64) -> usize {
    // On a 32 bit machine a store larger than the address space is a file this
    // process could not map anyway, and the number is going into an error
    // message rather than an index.
    usize::try_from(value).unwrap_or(usize::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{MAX_SEGMENTS, Segment};

    /// A store identifier that is recognisable in a hex dump.
    const STORE: u128 = 0x0123_4567_89ab_cdef_fedc_ba98_7654_3210;

    /// A path of this test's own.
    ///
    /// Keyed by the test's name and the process, because the tests run in
    /// parallel and two of them sharing a path is one of them deleting the
    /// other's file halfway through.
    fn path(name: &str) -> std::path::PathBuf {
        let directory = std::env::temp_dir().join(format!("kura-file-{}", std::process::id()));
        std::fs::create_dir_all(&directory).expect("a temporary directory");
        let path = directory.join(format!("{name}.kura"));
        let _ = std::fs::remove_file(&path);
        path
    }

    fn segment(n: u64) -> Segment {
        Segment {
            offset: 135_168 + n * 4096,
            len: 4096,
            docs: 100,
            created: 1_700_000_000 + n,
            ..Segment::default()
        }
    }

    #[test]
    fn a_new_store_opens_at_what_was_written_into_it() {
        let path = path("new");
        let created = {
            let store = Store::create(&path, STORE, 1_700_000_000).expect("a store");
            assert_eq!(store.slot(), Slot::A);
            assert_eq!(store.manifest().epoch, 1);
            assert!(store.manifest().segments.is_empty());
            *store.superblock()
        };
        let store = Store::open(&path).expect("a store");
        assert_eq!(*store.superblock(), created);
        assert_eq!(store.superblock().store, STORE);
        assert_eq!(store.manifest().epoch, 1);
        assert_eq!(store.slot(), Slot::A);
    }

    #[test]
    fn creating_over_a_store_that_is_already_there_is_refused() {
        let path = path("exists");
        Store::create(&path, STORE, 1).expect("a store");
        let error = Store::create(&path, STORE, 1).expect_err("refused");
        assert!(
            matches!(&error, Trouble::Io(io) if io.kind() == io::ErrorKind::AlreadyExists),
            "{error:?}"
        );
    }

    #[test]
    fn a_commit_alternates_slots_and_moves_the_epoch_forward() {
        let path = path("alternate");
        let mut store = Store::create(&path, STORE, 1).expect("a store");
        let mut expected = [Slot::B, Slot::A].into_iter().cycle();
        for n in 1..=8u64 {
            let mut manifest = store.manifest().clone();
            manifest.segments.push(segment(n));
            manifest.live = n * 100;
            let epoch = store
                .commit(manifest, 1_700_000_000 + n)
                .expect("committed");
            assert_eq!(epoch, n + 1);
            assert_eq!(store.slot(), expected.next().expect("a slot"));
            assert_eq!(store.manifest().segments.len() as u64, n);
        }
        let store = Store::open(&path).expect("a store");
        assert_eq!(store.manifest().epoch, 9);
        assert_eq!(store.manifest().segments.len(), 8);
        assert_eq!(store.manifest().live, 800);
        assert_eq!(store.slot(), Slot::A);
    }

    #[test]
    fn a_commit_that_never_finished_leaves_the_store_at_the_state_before_it() {
        let path = path("torn");
        {
            let mut store = Store::create(&path, STORE, 1).expect("a store");
            let mut manifest = store.manifest().clone();
            manifest.segments.push(segment(1));
            store.commit(manifest, 2).expect("committed");
            assert_eq!(store.slot(), Slot::B);
        }
        // What a machine losing power partway through the slot B write leaves
        // behind. Slot B holds the beginning of a manifest and the end of
        // nothing, and its checksum says so.
        damage(&path, SLOT_B_OFFSET + 4096);
        let store = Store::open(&path).expect("a store");
        assert_eq!(store.slot(), Slot::A);
        assert_eq!(store.manifest().epoch, 1);
        assert!(store.manifest().segments.is_empty());
    }

    #[test]
    fn a_store_with_neither_slot_readable_says_so() {
        let path = path("gone");
        Store::create(&path, STORE, 1).expect("a store");
        damage(&path, SLOT_A_OFFSET + 32);
        let error = Store::open(&path).expect_err("no manifest");
        assert!(
            matches!(error, Trouble::Format(Error::NoManifest)),
            "{error:?}"
        );
    }

    #[test]
    fn something_that_is_not_a_store_is_not_opened() {
        let path = path("rubbish");
        std::fs::write(&path, vec![0x5au8; SUPERBLOCK_LEN * 2]).expect("a file");
        let error = Store::open(&path).expect_err("not a store");
        assert!(
            matches!(error, Trouble::Format(Error::BadMagic)),
            "{error:?}"
        );
    }

    #[test]
    fn a_store_shorter_than_the_regions_it_claims_is_not_opened() {
        let path = path("short");
        {
            let store = Store::create(&path, STORE, 1).expect("a store");
            drop(store);
        }
        let file = OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("the file");
        file.set_len(SLOT_B_OFFSET).expect("truncated");
        drop(file);
        let error = Store::open(&path).expect_err("too short");
        assert!(
            matches!(error, Trouble::Format(Error::Truncated { .. })),
            "{error:?}"
        );
    }

    #[test]
    fn a_file_that_is_not_there_is_an_io_error_and_not_a_panic() {
        let path = path("missing");
        let error = Store::open(&path).expect_err("not there");
        assert!(
            matches!(&error, Trouble::Io(io) if io.kind() == io::ErrorKind::NotFound),
            "{error:?}"
        );
    }

    #[test]
    fn a_manifest_too_large_for_a_slot_is_refused_and_changes_nothing() {
        let path = path("toobig");
        let mut store = Store::create(&path, STORE, 1).expect("a store");
        let mut manifest = store.manifest().clone();
        manifest.segments = (0..=MAX_SEGMENTS as u64).map(segment).collect();
        let error = store.commit(manifest, 2).expect_err("too big");
        assert!(
            matches!(error, Trouble::Format(Error::TooManySegments { .. })),
            "{error:?}"
        );
        assert_eq!(store.manifest().epoch, 1);
        assert_eq!(store.slot(), Slot::A);
        let store = Store::open(&path).expect("a store");
        assert_eq!(store.manifest().epoch, 1);
    }

    #[test]
    fn a_store_survives_more_commits_than_it_has_slots() {
        let path = path("many");
        let mut store = Store::create(&path, STORE, 1).expect("a store");
        for n in 1..=64u64 {
            let mut manifest = store.manifest().clone();
            manifest.terms = n * 1000;
            store.commit(manifest, n).expect("committed");
        }
        drop(store);
        let store = Store::open(&path).expect("a store");
        assert_eq!(store.manifest().epoch, 65);
        assert_eq!(store.manifest().terms, 64_000);
    }

    /// A log region small enough that a test can go round it, and a whole page,
    /// because everything structural in a store is page aligned.
    const RING: u64 = 4096;

    /// A payload that says which record it came from.
    fn payload(n: u64, len: usize) -> Vec<u8> {
        (0..len)
            .map(|i| u8::try_from((n + i as u64) % 251).unwrap_or_default())
            .collect()
    }

    /// Every record in the log, as a recovery sees them.
    fn replayed(store: &mut Store) -> Vec<(u32, u64, Vec<u8>)> {
        let mut found = Vec::new();
        store
            .recover(|record| found.push((record.kind, record.sequence, record.payload.to_vec())))
            .expect("recovered");
        found
    }

    #[test]
    fn a_new_store_has_an_empty_log() {
        let path = path("emptylog");
        let mut store = Store::create_with_log(&path, STORE, 1, RING).expect("a store");
        assert!(store.log().is_empty());
        assert_eq!(store.log().len(), RING);
        assert_eq!(store.log().sequence(), 1);
        assert!(replayed(&mut store).is_empty());
    }

    #[test]
    fn what_is_appended_to_the_log_is_there_when_the_store_is_opened_again() {
        let path = path("append");
        {
            let mut store = Store::create_with_log(&path, STORE, 1, RING).expect("a store");
            for n in 0..8 {
                let sequence = store
                    .append(wal::kind::UPSERT, &payload(n, 40))
                    .expect("appended");
                assert_eq!(sequence, n + 1);
            }
            store.sync().expect("synced");
        }
        // Nothing was committed after the appends, so the manifest still says
        // the log is empty. The records are found anyway, which is the whole
        // point: the committed tail is a floor and not a fact.
        let mut store = Store::open(&path).expect("a store");
        assert_eq!(store.manifest().wal_tail, 0);
        let found = replayed(&mut store);
        assert_eq!(found.len(), 8);
        for (n, (kind, sequence, bytes)) in found.iter().enumerate() {
            let n = n as u64;
            assert_eq!(*kind, wal::kind::UPSERT);
            assert_eq!(*sequence, n + 1);
            assert_eq!(*bytes, payload(n, 40));
        }
        assert_eq!(store.log().sequence(), 9);
    }

    #[test]
    fn a_recovery_follows_the_log_round_the_ring_without_reading_the_lap_before() {
        let path = path("lap");
        let tail = {
            let mut store = Store::create_with_log(&path, STORE, 1, RING).expect("a store");
            for n in 0..16 {
                store
                    .append(wal::kind::UPSERT, &payload(n, 200))
                    .expect("appended");
            }
            // Everything so far is in a segment, as far as this test is
            // concerned, so the log is free to move over it.
            let through = store.log().tail();
            store.truncate_log(through).expect("truncated");
            let manifest = store.manifest().clone();
            store.commit(manifest, 2).expect("committed");
            for n in 100..110 {
                store
                    .append(wal::kind::DELETE, &payload(n, 200))
                    .expect("appended");
            }
            store.sync().expect("synced");
            store.log().tail()
        };
        let mut store = Store::open(&path).expect("a store");
        let found = replayed(&mut store);
        assert_eq!(
            found.len(),
            10,
            "the walk did not stop where this lap stopped"
        );
        for (n, (kind, _, bytes)) in found.iter().enumerate() {
            assert_eq!(*kind, wal::kind::DELETE);
            assert_eq!(*bytes, payload(100 + n as u64, 200));
        }
        assert_eq!(store.log().tail(), tail);
        assert_eq!(store.log().sequence(), 27);
    }

    #[test]
    fn a_torn_record_is_where_the_log_ends_and_the_next_append_takes_its_place() {
        let path = path("tornlog");
        let at = {
            let mut store = Store::create_with_log(&path, STORE, 1, RING).expect("a store");
            store
                .append(wal::kind::UPSERT, &payload(1, 40))
                .expect("appended");
            store
                .append(wal::kind::UPSERT, &payload(2, 40))
                .expect("appended");
            let at = store.log().tail();
            for n in 3..5 {
                store
                    .append(wal::kind::UPSERT, &payload(n, 40))
                    .expect("appended");
            }
            store.sync().expect("synced");
            at
        };
        // A record that was being written when the machine stopped.
        damage(&path, crate::manifest::WAL_OFFSET + at + 20);
        {
            let mut store = Store::open(&path).expect("a store");
            let found = replayed(&mut store);
            assert_eq!(found.len(), 2);
            assert_eq!(store.log().tail(), at);
            assert_eq!(store.log().sequence(), 3);
            // And the log carries on from there, into the bytes the torn record
            // and the one after it were in.
            store
                .append(wal::kind::COMMIT, &payload(9, 12))
                .expect("appended");
            store.sync().expect("synced");
        }
        let mut store = Store::open(&path).expect("a store");
        let found = replayed(&mut store);
        assert_eq!(found.len(), 3);
        assert_eq!(found[2].0, wal::kind::COMMIT);
        assert_eq!(found[2].1, 3);
        assert_eq!(found[2].2, payload(9, 12));
    }

    #[test]
    fn a_record_that_does_not_fit_the_log_is_refused_and_the_log_does_not_move() {
        let path = path("full");
        let mut store = Store::create_with_log(&path, STORE, 1, RING).expect("a store");
        store
            .append(wal::kind::UPSERT, &payload(1, 40))
            .expect("appended");
        let (tail, sequence) = (store.log().tail(), store.log().sequence());
        let error = store
            .append(wal::kind::UPSERT, &payload(2, as_usize(RING)))
            .expect_err("too big");
        assert!(
            matches!(error, Trouble::Format(Error::LogFull { .. })),
            "{error:?}"
        );
        assert_eq!(store.log().tail(), tail);
        assert_eq!(store.log().sequence(), sequence);
        assert_eq!(replayed(&mut store).len(), 1);
    }

    #[test]
    fn a_record_larger_than_a_recovery_window_is_read_on_its_own() {
        let path = path("window");
        let big = WINDOW + WINDOW / 2;
        {
            let mut store =
                Store::create_with_log(&path, STORE, 1, 8 * 1024 * 1024).expect("a store");
            store
                .append(wal::kind::UPSERT, &payload(1, 64))
                .expect("appended");
            store
                .append(wal::kind::UPSERT, &payload(2, big))
                .expect("appended");
            store
                .append(wal::kind::UPSERT, &payload(3, 64))
                .expect("appended");
            store.sync().expect("synced");
        }
        let mut store = Store::open(&path).expect("a store");
        let found = replayed(&mut store);
        assert_eq!(found.len(), 3);
        assert_eq!(found[1].2, payload(2, big));
        assert_eq!(found[2].2, payload(3, 64));
    }

    #[test]
    fn a_commit_writes_where_the_log_is_rather_than_what_the_caller_says() {
        let path = path("logcommit");
        let mut store = Store::create_with_log(&path, STORE, 1, RING).expect("a store");
        store
            .append(wal::kind::UPSERT, &payload(1, 40))
            .expect("appended");
        let mut manifest = store.manifest().clone();
        // Numbers a caller has no business setting, and which would cost the
        // store a record each if they were taken at face value.
        manifest.wal_head = 900;
        manifest.wal_tail = 900;
        manifest.wal_sequence = 900;
        store.commit(manifest, 2).expect("committed");
        assert_eq!(store.manifest().wal_head, 0);
        assert_eq!(store.manifest().wal_tail, store.log().tail());
        assert_eq!(store.manifest().wal_sequence, 2);
        let mut store = Store::open(&path).expect("a store");
        assert_eq!(replayed(&mut store).len(), 1);
    }

    /// Words a document is built from, chosen so that every query below hits
    /// some documents and misses others.
    const WORDS: [&str; 6] = ["ledger", "invoice", "quarter", "ledger", "audit", "ledger"];

    /// A document that holds a predictable handful of the words above.
    fn text(n: usize) -> String {
        let mut out = String::new();
        for at in 0..=(n % 5) {
            out.push_str(WORDS[(n + at) % WORDS.len()]);
            out.push(' ');
        }
        out.push_str(WORDS[n % WORDS.len()]);
        out
    }

    /// A segment holding the `count` documents starting at `from`.
    fn built(from: usize, count: usize) -> (Vec<u8>, u32) {
        let mut writer = crate::index::Writer::new();
        for n in from..from + count {
            writer.add(&text(n)).expect("a document");
        }
        let docs = u32::try_from(writer.len()).expect("a test corpus fits in a segment");
        (writer.finish().expect("a segment"), docs)
    }

    /// Writes a store holding one segment per entry of `parts`, each entry
    /// saying how many documents that segment gets.
    ///
    /// Every segment is appended before anything is committed, which is what a
    /// store flushing a batch does and is the case that used to put them all at
    /// the same offset.
    fn stored(path: &Path, parts: &[usize]) -> Store {
        let mut store = Store::create(path, STORE, 1_700_000_000).expect("a store");
        let mut manifest = store.manifest().clone();
        let mut from = 0;
        for (n, &count) in parts.iter().enumerate() {
            let (bytes, docs) = built(from, count);
            let described = store
                .append_segment(&bytes, docs, 1_700_000_000 + n as u64)
                .expect("appended");
            manifest.segments.push(described);
            manifest.total += u64::from(described.docs);
            manifest.live += u64::from(described.docs);
            from += count;
        }
        store.commit(manifest, 1_700_000_001).expect("committed");
        store
    }

    /// The page a query gets out of a store, with the documents named by the
    /// segment they are in rather than by the searcher wide number, so that a
    /// store split three ways and a store split one way are comparable.
    fn page(store: &Store, query: &str, k: usize) -> Vec<(usize, u32, f32)> {
        let view = store.view().expect("a view");
        let segments: Vec<_> = view
            .all()
            .map(|bytes| crate::segment::Segment::open(bytes).expect("a segment"))
            .collect();
        let readers: Vec<_> = segments
            .iter()
            .map(|segment| crate::index::Reader::open(segment).expect("a reader"))
            .collect();
        let searcher = crate::search::Searcher::over(&readers).expect("a searcher");
        searcher
            .search(query, k)
            .expect("searched")
            .into_iter()
            .map(|hit| {
                let (at, doc) = searcher.locate(hit.doc).expect("a segment");
                (at, doc, hit.score)
            })
            .collect()
    }

    #[test]
    fn a_segment_written_into_a_store_comes_back_out_of_it() {
        let path = path("onesegment");
        let store = stored(&path, &[40]);
        drop(store);

        let store = Store::open(&path).expect("a store");
        let view = store.view().expect("a view");
        assert_eq!(view.len(), 1);
        let bytes = view.bytes(0).expect("the segment");
        let segment = crate::segment::Segment::open(bytes).expect("a segment");
        let reader = crate::index::Reader::open(&segment).expect("a reader");
        assert_eq!(reader.documents(), 40);
        assert_eq!(view.described()[0].docs, 40);
    }

    #[test]
    #[expect(
        clippy::float_cmp,
        reason = "the same documents scored by the same code over the same terms \
                  in the same order, so the two sums agree bit for bit or the \
                  change this tests for has not been made"
    )]
    fn a_store_of_several_segments_ranks_the_same_as_a_store_of_one() {
        // The point of the whole exercise, on a real file rather than on
        // buffers. The same hundred and twenty documents, written as one
        // segment and as four, have to give the same page, because a document's
        // score is a fact about the corpus and not about which batch it arrived
        // in.
        let one = stored(&path("wholecorpus"), &[120]);
        let four = stored(&path("splitcorpus"), &[30, 30, 30, 30]);
        assert_eq!(one.view().expect("a view").len(), 1);
        assert_eq!(four.view().expect("a view").len(), 4);

        for query in ["ledger", "invoice quarter", "ledger audit invoice"] {
            let whole = page(&one, query, 20);
            let split = page(&four, query, 20);
            assert!(!whole.is_empty(), "{query} found nothing to compare");
            assert_eq!(whole.len(), split.len(), "{query}");
            for (at, (left, right)) in whole.iter().zip(split.iter()).enumerate() {
                // A document is named by the segment it is in and its ordinal
                // there, so the four segment store's thirtieth document of
                // segment two is the ninetieth of the one segment store. Put
                // back together rather than compared as pairs, because the pairs
                // are different by construction and the documents are not.
                let (whole_at, whole_doc, whole_score) = *left;
                let (split_at, split_doc, split_score) = *right;
                assert_eq!(whole_at, 0);
                assert_eq!(
                    whole_doc,
                    u32::try_from(split_at).expect("four segments") * 30 + split_doc,
                    "{query} put a different document at {at}"
                );
                // To the last bit, not to a tolerance. The terms are summed in
                // the order they were given on both sides, so there is nothing
                // for the two to disagree about.
                assert_eq!(whole_score, split_score, "{query} at {at}");
            }
        }
    }

    #[test]
    fn segments_appended_before_a_commit_do_not_land_on_top_of_each_other() {
        // All three go in before anything is committed, which is what a store
        // with a batch to flush does. While the placement was worked out from
        // the committed manifest, that manifest named none of them and all three
        // went to the same offset, so the first two were bytes under the third.
        let path = path("layout");
        let store = stored(&path, &[20, 20, 20]);
        let described = store.manifest().segments.clone();
        assert_eq!(described.len(), 3);

        let page = u64::from(PAGE);
        let mut previous = store.superblock().segments_offset;
        for segment in &described {
            assert!(segment.len > 0, "{described:?}");
            assert!(segment.offset >= previous, "{described:?}");
            assert_eq!(segment.offset % page, 0, "{described:?}");
            previous = segment.offset + segment.len;
        }
        assert!(store.segments_end() >= previous);

        // And the three are still three different segments when read back,
        // which is the check that a layout assertion on its own would miss if
        // the offsets were right and the bytes were not.
        let view = store.view().expect("a view");
        assert_eq!(view.len(), 3);
        for bytes in view.all() {
            let segment = crate::segment::Segment::open(bytes).expect("a segment");
            let reader = crate::index::Reader::open(&segment).expect("a reader");
            assert_eq!(reader.documents(), 20);
        }

        // Reopening puts the cursor back where the committed manifest leaves it,
        // which for a store whose appends were all committed is where it already
        // was.
        drop(store);
        let store = Store::open(&path).expect("a store");
        assert_eq!(store.manifest().segments, described);
        assert_eq!(store.segments_end(), previous.next_multiple_of(page));
    }

    #[test]
    fn a_view_is_the_store_it_was_taken_from_and_not_the_one_after() {
        let path = path("snapshot");
        let mut store = stored(&path, &[20]);
        let view = store.view().expect("a view");
        assert_eq!(view.len(), 1);

        let (bytes, docs) = built(20, 20);
        let described = store.append_segment(&bytes, docs, 2).expect("appended");
        let mut manifest = store.manifest().clone();
        manifest.segments.push(described);
        store.commit(manifest, 3).expect("committed");

        assert_eq!(view.len(), 1, "a commit reached a view taken before it");
        assert_eq!(store.view().expect("a view").len(), 2);
    }

    #[test]
    fn a_segment_written_as_it_is_made_is_the_segment_written_whole() {
        let (bytes, docs) = built(0, 20);

        let path = path("streamed");
        let mut store = Store::create(&path, STORE, 1).expect("a store");
        let mut writer = crate::index::Writer::new();
        for n in 0..20 {
            writer.add(&text(n)).expect("a document");
        }
        let segment = crate::index::Writer::build(vec![writer]).expect("a segment");
        let described = store
            .append_segment_with(docs, 2, |into| segment.write_to(into))
            .expect("appended");
        assert_eq!(described.len, bytes.len() as u64);

        let mut manifest = store.manifest().clone();
        manifest.segments.push(described);
        store.commit(manifest, 3).expect("committed");

        // Byte for byte, because the whole point of this is that it is the same
        // segment and not a segment that reads the same.
        let view = store.view().expect("a view");
        assert_eq!(view.bytes(0).expect("the segment"), &bytes[..]);
    }

    #[test]
    fn an_append_puts_the_pieces_it_is_given_one_after_another() {
        use io::Write as _;

        let path = path("pieces");
        let mut store = Store::create(&path, STORE, 1).expect("a store");
        let offset = store.segments_end();
        let described = store
            .append_segment_with(1, 2, |into| {
                let mut so_far = 0;
                for piece in [&b"one"[..], b"two", b"three"] {
                    into.write_all(piece)?;
                    so_far += piece.len() as u64;
                    assert_eq!(into.written(), so_far);
                }
                Ok(())
            })
            .expect("appended");

        assert_eq!(described.offset, offset);
        assert_eq!(described.len, 11);
        let mut back = [0u8; 11];
        read_at(&store.file, &mut back, offset).expect("read back");
        assert_eq!(&back, b"onetwothree");
    }

    #[test]
    fn an_append_that_gave_up_halfway_leaves_the_store_where_it_was() {
        let path = path("halfway");
        let mut store = stored(&path, &[20]);
        let end = store.segments_end();
        let manifest = store.manifest().clone();

        let refused = store.append_segment_with(1, 2, |into| {
            use io::Write as _;
            into.write_all(b"the start of a segment nobody finished")?;
            Err(io::Error::other("the thing making the segment gave up"))
        });
        assert!(refused.is_err());

        // The cursor did not move, so the bytes that did land are bytes the next
        // append writes over, and the committed state has not heard about any of
        // it.
        assert_eq!(store.segments_end(), end);
        assert_eq!(store.manifest().segments, manifest.segments);

        let (bytes, docs) = built(20, 20);
        let described = store.append_segment(&bytes, docs, 3).expect("appended");
        assert_eq!(described.offset, end);
    }

    #[test]
    fn an_empty_store_has_a_view_with_nothing_in_it() {
        let path = path("emptyview");
        let store = Store::create(&path, STORE, 1).expect("a store");
        let view = store.view().expect("a view");
        assert!(view.is_empty());
        assert_eq!(view.len(), 0);
        assert!(view.bytes(0).is_none());
        assert_eq!(view.all().count(), 0);
    }

    #[test]
    fn a_manifest_naming_a_segment_that_is_not_in_the_file_is_refused() {
        let path = path("dangling");
        let mut store = stored(&path, &[20]);
        let mut manifest = store.manifest().clone();
        // A segment nothing ever wrote, out past the end of the file. Which is
        // what a manifest committed before the segment it names would leave, and
        // the reason `append_segment` syncs before `commit` is called.
        manifest.segments.push(Segment {
            offset: store.segments_end(),
            len: 1 << 30,
            docs: 1,
            ..Segment::default()
        });
        store.commit(manifest, 2).expect("committed");
        let error = store.view().expect_err("not in the file");
        assert!(
            matches!(error, Trouble::Format(Error::Truncated { .. })),
            "{error:?}"
        );
    }

    #[test]
    fn a_segment_that_starts_before_the_segment_region_is_refused() {
        let path = path("intheslots");
        let mut store = stored(&path, &[20]);
        let mut manifest = store.manifest().clone();
        // Pointing at a manifest slot. Every byte of it is in the file, so the
        // length check passes and this is the one that has to catch it, and what
        // it catches is a query reading a manifest as though it were postings.
        manifest.segments.push(Segment {
            offset: SLOT_A_OFFSET,
            len: SLOT_LEN as u64,
            docs: 1,
            ..Segment::default()
        });
        store.commit(manifest, 2).expect("committed");
        let error = store.view().expect_err("not in the region");
        assert!(
            matches!(error, Trouble::Format(Error::SectionOutOfRange { .. })),
            "{error:?}"
        );
    }

    /// What a query gets out of a view, through the readers the view hands out,
    /// which are the ones with the deletions already applied.
    ///
    /// Named by segment and by the document's place in it, the same as
    /// [`page`], and without the scores because these tests are about which
    /// documents come back rather than in what order.
    fn answered(view: &View, query: &str, k: usize) -> Vec<(usize, u32)> {
        let readers = view.readers().expect("readers");
        let searcher = crate::search::Searcher::over(&readers).expect("a searcher");
        searcher
            .search(query, k)
            .expect("searched")
            .into_iter()
            .map(|hit| searcher.locate(hit.doc).expect("a segment"))
            .collect()
    }

    /// [`answered`], for a caller that has not taken a view yet.
    fn hits(store: &Store, query: &str, k: usize) -> Vec<(usize, u32)> {
        answered(&store.view().expect("a view"), query, k)
    }

    /// The key document `n` of a keyed store is written under.
    fn key(n: usize) -> Vec<u8> {
        format!("record-{n:04}").into_bytes()
    }

    /// [`stored`], with every document written under a key of its own.
    fn keyed(path: &Path, parts: &[usize]) -> Store {
        let mut store = Store::create(path, STORE, 1_700_000_000).expect("a store");
        let mut manifest = store.manifest().clone();
        let mut from = 0;
        for (n, &count) in parts.iter().enumerate() {
            let mut writer = crate::index::Writer::new();
            for at in from..from + count {
                writer.add_keyed(&key(at), &text(at)).expect("a document");
            }
            let docs = u32::try_from(writer.len()).expect("a test corpus fits");
            let bytes = writer.finish().expect("a segment");
            let described = store
                .append_segment(&bytes, docs, 1_700_000_000 + n as u64)
                .expect("appended");
            manifest.segments.push(described);
            manifest.total += u64::from(described.docs);
            manifest.live += u64::from(described.docs);
            from += count;
        }
        store.commit(manifest, 1_700_000_001).expect("committed");
        store
    }

    /// A query matching every document, because every one of them holds at
    /// least the word its number picks.
    const EVERYTHING: &str = "ledger invoice quarter audit";

    /// How many documents a store answers with, counted rather than paged.
    fn counted(store: &Store) -> u64 {
        let view = store.view().expect("a view");
        let readers = view.readers().expect("readers");
        let searcher = crate::search::Searcher::over(&readers).expect("a searcher");
        searcher.count(EVERYTHING).expect("counted")
    }

    #[test]
    fn a_deleted_document_is_still_gone_when_the_store_is_opened_again() {
        let path = path("deletecommit");
        let mut store = stored(&path, &[40]);
        let before = hits(&store, "audit", 8);
        let (at, doc) = before[0];
        store
            .delete(at, &Bitmap::from_sorted(&[doc]), 2)
            .expect("deleted");
        assert_eq!(store.manifest().live, 39);
        drop(store);

        let store = Store::open(&path).expect("a store");
        assert_eq!(store.manifest().live, 39);
        let after = hits(&store, "audit", 8);
        assert!(!after.contains(&(at, doc)), "{after:?}");
        for hit in &before[1..] {
            assert!(after.contains(hit), "{hit:?} went missing with {doc}");
        }
    }

    #[test]
    fn a_view_taken_before_a_delete_still_answers_with_the_document() {
        let path = path("deletesnapshot");
        let mut store = stored(&path, &[40]);
        let older = store.view().expect("a view");
        let (at, doc) = answered(&older, "audit", 1)[0];

        store
            .delete(at, &Bitmap::from_sorted(&[doc]), 2)
            .expect("deleted");
        let newer = store.view().expect("a view");

        // Both alive at once, which is the whole point: the delete is a write
        // somewhere else and a manifest pointed at it, so the older view is
        // reading bytes nothing touched.
        assert_eq!(
            answered(&older, "audit", 1)[0],
            (at, doc),
            "a delete reached a view taken before it"
        );
        assert_ne!(answered(&newer, "audit", 1)[0], (at, doc));
        assert!(older.tombstones(at).is_none());
        assert!(newer.tombstones(at).is_some());
    }

    #[test]
    fn a_newer_set_of_deletions_leaves_the_older_one_where_it_was() {
        let path = path("copyonwrite");
        let mut store = stored(&path, &[40]);
        store
            .delete(0, &Bitmap::from_sorted(&[1]), 2)
            .expect("deleted");
        let older = store.view().expect("a view");
        let first = older.tombstones(0).expect("a bitmap").to_vec();

        store
            .delete(0, &Bitmap::from_sorted(&[1, 2, 3]), 3)
            .expect("deleted");
        let newer = store.view().expect("a view");

        assert_eq!(older.tombstones(0).expect("a bitmap"), &first[..]);
        assert_ne!(newer.tombstones(0).expect("a bitmap"), &first[..]);
        assert_eq!(
            older.deleted(0).expect("a set").expect("some"),
            Bitmap::from_sorted(&[1])
        );
        assert_eq!(
            newer.deleted(0).expect("a set").expect("some"),
            Bitmap::from_sorted(&[1, 2, 3])
        );
        assert_eq!(store.manifest().live, 37);
    }

    #[test]
    fn a_tombstone_bitmap_that_is_not_in_the_segment_region_is_refused_rather_than_read() {
        let path = path("tombstoneslot");
        let mut store = stored(&path, &[20]);
        let mut manifest = store.manifest().clone();
        // Pointing at a manifest slot, which is the dangerous one for a
        // tombstone bitmap the same way it is for a segment: every byte of it
        // is in the file, so it decodes into something and the something is a
        // set of documents nobody deleted.
        manifest.segments[0].tombstones_offset = SLOT_A_OFFSET;
        manifest.segments[0].tombstones_len = 16;
        manifest.segments[0].generation += 1;
        store.commit(manifest, 2).expect("committed");

        let error = store.view().expect_err("not in the region");
        assert!(
            matches!(error, Trouble::Format(Error::SectionOutOfRange { .. })),
            "{error:?}"
        );
    }

    #[test]
    fn a_tombstone_bitmap_that_is_not_in_the_file_is_refused_rather_than_read() {
        let path = path("tombstonepast");
        let mut store = stored(&path, &[20]);
        let mut manifest = store.manifest().clone();
        manifest.segments[0].tombstones_offset = store.segments_end();
        manifest.segments[0].tombstones_len = 1 << 20;
        manifest.segments[0].generation += 1;
        store.commit(manifest, 2).expect("committed");

        let error = store.view().expect_err("not in the file");
        assert!(
            matches!(error, Trouble::Format(Error::Truncated { .. })),
            "{error:?}"
        );
    }

    #[test]
    fn a_commit_that_puts_the_deletions_back_to_an_older_set_is_refused() {
        let path = path("staleset");
        let mut store = stored(&path, &[20]);
        let (at, doc) = hits(&store, "audit", 1)[0];
        store
            .delete(at, &Bitmap::from_sorted(&[doc]), 2)
            .expect("deleted");
        let committed = store.manifest().clone();

        // What a writer working from a manifest it read before the delete would
        // commit, and what it would do is bring the document back.
        let mut older = committed.clone();
        older.segments[at].generation -= 1;
        older.segments[at].tombstones_offset = 0;
        older.segments[at].tombstones_len = 0;
        let error = store.commit(older, 3).expect_err("a step backwards");
        assert!(
            matches!(error, Trouble::Format(Error::StaleGeneration { .. })),
            "{error:?}"
        );

        assert_eq!(store.manifest().epoch, committed.epoch);
        assert_eq!(store.manifest().segments, committed.segments);
        assert!(!hits(&store, "audit", 8).contains(&(at, doc)));
    }

    #[test]
    fn a_segment_that_starts_again_at_an_offset_another_one_used_is_not_a_step_backwards() {
        // A compaction drops a segment and a later append reuses the space, so
        // there is a new segment at an old offset with no deletions and a
        // generation of zero. Matching on the footer as well as on the offset is
        // what tells the two apart.
        let path = path("reusedoffset");
        let mut store = stored(&path, &[20]);
        store
            .delete(0, &Bitmap::from_sorted(&[0, 1]), 2)
            .expect("deleted");

        let mut manifest = store.manifest().clone();
        manifest.segments[0].footer ^= 1;
        manifest.segments[0].generation = 0;
        manifest.segments[0].tombstones_offset = 0;
        manifest.segments[0].tombstones_len = 0;
        store.commit(manifest, 3).expect("committed");
        assert_eq!(store.manifest().segments[0].generation, 0);
    }

    #[test]
    fn a_delete_naming_a_document_the_segment_does_not_have_is_refused() {
        let path = path("nosuchdocument");
        let mut store = stored(&path, &[20]);
        let error = store
            .delete(0, &Bitmap::from_sorted(&[20]), 2)
            .expect_err("no such document");
        assert!(
            matches!(
                error,
                Trouble::Format(Error::NoSuchDocument {
                    doc: 20,
                    documents: 20
                })
            ),
            "{error:?}"
        );
        assert_eq!(store.manifest().live, 20);
        assert!(store.view().expect("a view").tombstones(0).is_none());
    }

    #[test]
    fn a_delete_of_a_segment_that_is_not_there_is_refused() {
        let path = path("nosuchsegment");
        let mut store = stored(&path, &[20]);
        let error = store
            .delete(3, &Bitmap::from_sorted(&[0]), 2)
            .expect_err("no such segment");
        assert!(
            matches!(error, Trouble::Format(Error::MissingSection { .. })),
            "{error:?}"
        );
    }

    #[test]
    fn deleting_nothing_clears_the_descriptor_rather_than_writing_an_empty_set() {
        let path = path("undeleted");
        let mut store = stored(&path, &[20]);
        store
            .delete(0, &Bitmap::from_sorted(&[0, 1, 2]), 2)
            .expect("deleted");
        assert_eq!(store.manifest().live, 17);

        store.delete(0, &Bitmap::new(), 3).expect("deleted");
        assert_eq!(store.manifest().live, 20);
        let segment = store.manifest().segments[0];
        assert_eq!(segment.tombstones_offset, 0);
        assert_eq!(segment.tombstones_len, 0);
        assert_eq!(segment.first_live, 0);
        assert_eq!(segment.generation, 2);
        assert!(store.view().expect("a view").tombstones(0).is_none());
    }

    #[test]
    fn a_deleted_prefix_says_where_the_documents_worth_reading_begin() {
        let path = path("firstlive");
        let mut store = stored(&path, &[20]);
        store
            .delete(0, &Bitmap::from_sorted(&[0, 1, 2, 9]), 2)
            .expect("deleted");
        assert_eq!(store.manifest().segments[0].first_live, 3);
        assert_eq!(store.manifest().segments[0].generation, 1);
    }

    #[test]
    fn the_live_count_is_what_a_search_counts_after_several_rounds_of_deletion() {
        let path = path("livecount");
        let mut store = stored(&path, &[30, 30, 30]);
        assert_eq!(store.manifest().live, 90);
        assert_eq!(counted(&store), 90);

        // A set is the whole answer for its segment rather than a change to it,
        // so each round hands over what it had with one more in it.
        let mut gone = [Bitmap::new(), Bitmap::new(), Bitmap::new()];
        let mut written = 2;
        for round in 0..4 {
            for (at, set) in gone.iter_mut().enumerate() {
                let doc = u32::try_from(round * 3 + at).expect("a small document number");
                set.insert(doc);
                store.delete(at, set, written).expect("deleted");
                written += 1;
                assert_eq!(
                    counted(&store),
                    store.manifest().live,
                    "round {round} of segment {at}"
                );
            }
        }
        assert_eq!(store.manifest().live, 78);

        // And the same store opened from nothing but the file agrees, which is
        // the part that says the sets were written down rather than remembered.
        drop(store);
        let store = Store::open(&path).expect("a store");
        assert_eq!(store.manifest().live, 78);
        assert_eq!(counted(&store), 78);
    }

    #[test]
    fn a_deletion_in_one_segment_does_not_touch_what_the_others_answer_with() {
        let path = path("acrosssegments");
        let mut store = stored(&path, &[30, 30, 30]);
        let before = hits(&store, "audit", 12);
        store
            .delete(1, &Bitmap::from_sorted(&[0, 1, 2, 3, 4]), 2)
            .expect("deleted");

        let after = hits(&store, "audit", 12);
        for hit in &before {
            let deleted = hit.0 == 1 && hit.1 < 5;
            assert_eq!(!deleted, after.contains(hit), "{hit:?}");
        }
    }

    /// Adds one more segment to a store that already has some, holding the
    /// documents named, and commits it.
    fn extend(store: &mut Store, documents: &[(Vec<u8>, String)]) {
        let mut writer = crate::index::Writer::new();
        for (key, text) in documents {
            writer.add_keyed(key, text).expect("a document");
        }
        let docs = u32::try_from(writer.len()).expect("a test corpus fits");
        let bytes = writer.finish().expect("a segment");
        let described = store
            .append_segment(&bytes, docs, 1_700_000_100)
            .expect("appended");
        let mut manifest = store.manifest().clone();
        manifest.segments.push(described);
        manifest.total += u64::from(docs);
        manifest.live += u64::from(docs);
        store.commit(manifest, 1_700_000_101).expect("committed");
    }

    #[test]
    fn a_key_finds_the_document_it_was_written_under() {
        let path = path("keylookup");
        let store = keyed(&path, &[10, 10, 10]);
        let view = store.view().expect("a view");
        // First of the first segment, last of the last, and one in the middle,
        // by the segment they are in and their place inside it.
        assert_eq!(view.document(&key(0)).expect("looked up"), Some((0, 0)));
        assert_eq!(view.document(&key(14)).expect("looked up"), Some((1, 4)));
        assert_eq!(view.document(&key(29)).expect("looked up"), Some((2, 9)));
    }

    #[test]
    fn a_key_nothing_in_the_store_holds_is_absent() {
        let path = path("keyabsent");
        let store = keyed(&path, &[10, 10, 10]);
        let view = store.view().expect("a view");
        // Past the end, before the beginning, and a prefix of a key that is
        // there, which is the one a filter is least likely to save.
        assert_eq!(view.document(&key(30)).expect("looked up"), None);
        assert_eq!(view.document(b"aaaa").expect("looked up"), None);
        assert_eq!(view.document(b"record-").expect("looked up"), None);
        assert_eq!(view.document(b"").expect("looked up"), None);
    }

    #[test]
    fn a_key_written_again_later_answers_with_the_newer_document() {
        let path = path("keyreplace");
        let mut store = keyed(&path, &[10]);
        let view = store.view().expect("a view");
        assert_eq!(view.document(&key(3)).expect("looked up"), Some((0, 3)));
        drop(view);

        // The same key again in a segment of its own, which is what replacing a
        // document is before anything is compacted.
        extend(&mut store, &[(key(3), text(300))]);
        let view = store.view().expect("a view");
        assert_eq!(view.document(&key(3)).expect("looked up"), Some((1, 0)));
        // And the neighbours are where they were.
        assert_eq!(view.document(&key(2)).expect("looked up"), Some((0, 2)));
        assert_eq!(view.document(&key(4)).expect("looked up"), Some((0, 4)));
    }

    #[test]
    fn a_key_naming_a_deleted_document_is_absent() {
        let path = path("keydeleted");
        let mut store = keyed(&path, &[10, 10]);
        assert_eq!(
            store
                .view()
                .expect("a view")
                .document(&key(12))
                .expect("looked up"),
            Some((1, 2))
        );
        store
            .delete(1, &Bitmap::from_sorted(&[2]), 2)
            .expect("deleted");
        let view = store.view().expect("a view");
        assert_eq!(view.document(&key(12)).expect("looked up"), None);
        // Its neighbours in the same segment are untouched, so the answer is
        // about that document rather than about that segment.
        assert_eq!(view.document(&key(11)).expect("looked up"), Some((1, 1)));
        assert_eq!(view.document(&key(13)).expect("looked up"), Some((1, 3)));
    }

    #[test]
    fn deleting_a_replacement_does_not_bring_the_document_it_replaced_back() {
        let path = path("keyresurrect");
        let mut store = keyed(&path, &[4]);
        extend(&mut store, &[(key(1), text(300))]);
        assert_eq!(
            store
                .view()
                .expect("a view")
                .document(&key(1))
                .expect("looked up"),
            Some((1, 0))
        );
        // Delete the newer copy. The older one is still there, still live, and
        // still under the same key, and it is not the answer: the newest
        // segment that names a key is the one that decides what that key means.
        store
            .delete(1, &Bitmap::from_sorted(&[0]), 3)
            .expect("deleted");
        let view = store.view().expect("a view");
        assert_eq!(view.document(&key(1)).expect("looked up"), None);
        // The older copy is genuinely still there, which is what makes the
        // answer above a decision rather than an absence.
        assert_eq!(view.reader(0).expect("a reader").document(&key(1)), Some(1));
    }

    #[test]
    fn a_store_of_segments_without_keys_answers_nothing_rather_than_failing() {
        let (unkeyed, empty) = (path("keynone"), path("keyempty"));
        let store = stored(&unkeyed, &[10, 10]);
        let view = store.view().expect("a view");
        assert_eq!(view.document(&key(3)).expect("looked up"), None);
        // A store with nothing in it at all is the same answer, and is the case
        // a lookup meets before the first segment is written.
        let store = Store::create(&empty, STORE, 1_700_000_000).expect("a store");
        let view = store.view().expect("a view");
        assert_eq!(view.document(&key(0)).expect("looked up"), None);
    }

    #[test]
    fn a_view_taken_before_a_key_was_replaced_still_answers_with_the_old_one() {
        // The same snapshot rule the rest of a view follows. A lookup that
        // walked the segments the manifest has now would answer with a document
        // the rest of the view cannot read.
        let path = path("keysnapshot");
        let mut store = keyed(&path, &[4]);
        let before = store.view().expect("a view");
        extend(&mut store, &[(key(1), text(300))]);
        assert_eq!(before.document(&key(1)).expect("looked up"), Some((0, 1)));
        assert_eq!(
            store
                .view()
                .expect("a view")
                .document(&key(1))
                .expect("looked up"),
            Some((1, 0))
        );
    }

    #[test]
    fn a_segment_and_the_deletions_that_go_with_it_arrive_in_one_commit() {
        let path = path("publish");
        let mut store = stored(&path, &[10, 10]);
        let epoch = store.manifest().epoch;
        let (bytes, docs) = built(100, 4);
        store
            .publish(
                Some((&bytes, docs)),
                1_700_000_100,
                &[
                    (0, Bitmap::from_sorted(&[0, 1])),
                    (1, Bitmap::from_sorted(&[9])),
                ],
                7,
            )
            .expect("published");
        // One commit for all three, which is what makes a replacement a single
        // event rather than a window somebody can read half of.
        assert_eq!(store.manifest().epoch, epoch + 1);
        assert_eq!(store.manifest().segments.len(), 3);
        assert_eq!(store.manifest().total, 24);
        assert_eq!(store.manifest().live, 21);

        let view = store.view().expect("a view");
        assert_eq!(view.deleted(0).expect("read").expect("a set").len(), 2);
        assert_eq!(view.deleted(1).expect("read").expect("a set").len(), 1);
        assert_eq!(view.deleted(2).expect("read"), None);
        assert_eq!(counted(&store), 21);
    }

    #[test]
    fn deletions_alone_are_a_publish_without_a_segment() {
        let path = path("publishnoseg");
        let mut store = stored(&path, &[10, 10]);
        store
            .publish(
                None,
                0,
                &[
                    (0, Bitmap::from_sorted(&[3])),
                    (1, Bitmap::from_sorted(&[4])),
                ],
                7,
            )
            .expect("published");
        assert_eq!(store.manifest().segments.len(), 2);
        assert_eq!(store.manifest().live, 18);
        assert_eq!(counted(&store), 18);
    }

    #[test]
    fn a_publish_naming_one_segment_twice_is_refused() {
        // Two sets for one segment is a caller that has not decided what is
        // deleted, and taking the later one would lose the other quietly.
        let path = path("publishtwice");
        let mut store = stored(&path, &[10, 10]);
        let epoch = store.manifest().epoch;
        let outcome = store.publish(
            None,
            0,
            &[
                (1, Bitmap::from_sorted(&[1])),
                (1, Bitmap::from_sorted(&[2])),
            ],
            7,
        );
        assert!(matches!(
            outcome,
            Err(Trouble::Format(Error::RepeatedSegment { at: 1 }))
        ));
        assert_eq!(store.manifest().epoch, epoch);
        assert_eq!(store.manifest().live, 20);
    }

    #[test]
    fn a_publish_naming_a_segment_that_is_not_there_is_refused() {
        let path = path("publishmissing");
        let mut store = stored(&path, &[10]);
        let outcome = store.publish(None, 0, &[(4, Bitmap::from_sorted(&[0]))], 7);
        assert!(matches!(
            outcome,
            Err(Trouble::Format(Error::MissingSection { kind: 0 }))
        ));
        assert_eq!(store.manifest().live, 10);
    }

    #[test]
    fn a_publish_whose_deletions_do_not_fit_their_segment_commits_nothing() {
        // The segment is written before the bitmaps, so this is the case where
        // half the batch is already on disk when the other half is refused.
        let path = path("publishbaddoc");
        let mut store = stored(&path, &[10]);
        let epoch = store.manifest().epoch;
        let (bytes, docs) = built(100, 4);
        let outcome = store.publish(
            Some((&bytes, docs)),
            1_700_000_100,
            &[(0, Bitmap::from_sorted(&[40]))],
            7,
        );
        assert!(matches!(
            outcome,
            Err(Trouble::Format(Error::NoSuchDocument {
                doc: 40,
                documents: 10
            }))
        ));
        // The bytes of the new segment are in the file and nothing names them,
        // which is what an append only region makes harmless.
        assert_eq!(store.manifest().epoch, epoch);
        assert_eq!(store.manifest().segments.len(), 1);
        assert_eq!(store.manifest().live, 10);
        drop(store);
        let store = Store::open(&path).expect("a store");
        assert_eq!(store.manifest().segments.len(), 1);
        assert_eq!(counted(&store), 10);
    }

    #[test]
    fn a_published_batch_is_still_there_when_the_store_is_opened_again() {
        let path = path("publishreopen");
        let mut store = stored(&path, &[10, 10]);
        let (bytes, docs) = built(100, 4);
        store
            .publish(
                Some((&bytes, docs)),
                1_700_000_100,
                &[(0, Bitmap::from_sorted(&[0, 1, 2]))],
                7,
            )
            .expect("published");
        drop(store);

        let store = Store::open(&path).expect("a store");
        assert_eq!(store.manifest().segments.len(), 3);
        assert_eq!(store.manifest().live, 21);
        assert_eq!(counted(&store), 21);
        let view = store.view().expect("a view");
        assert_eq!(view.deleted(0).expect("read").expect("a set").len(), 3);
    }

    #[test]
    fn publishing_deletions_twice_leaves_the_second_set_and_the_count_that_goes_with_it() {
        // A set is the whole answer for its segment, so a caller that wants one
        // more document gone passes the set it had with one more in it, and the
        // live count follows the difference rather than the size of what it was
        // handed.
        let path = path("publishagain");
        let mut store = stored(&path, &[10]);
        store
            .publish(None, 0, &[(0, Bitmap::from_sorted(&[0, 1]))], 7)
            .expect("published");
        assert_eq!(store.manifest().live, 8);
        store
            .publish(None, 0, &[(0, Bitmap::from_sorted(&[0, 1, 2]))], 8)
            .expect("published");
        assert_eq!(store.manifest().live, 7);
        assert_eq!(counted(&store), 7);
        // And the older set is still where it was, because a newer one is
        // written somewhere else and the manifest is repointed.
        let view = store.view().expect("a view");
        assert_eq!(view.deleted(0).expect("read").expect("a set").len(), 3);
    }

    /// Changes one byte of a file in place, which is what a torn write leaves.
    fn damage(path: &Path, offset: u64) {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .expect("the file");
        let mut byte = [0u8; 1];
        read_at(&file, &mut byte, offset).expect("a byte");
        byte[0] ^= 0x40;
        write_at(&file, &byte, offset).expect("a byte");
    }
}
