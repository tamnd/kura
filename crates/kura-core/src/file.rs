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
//! # Positioned reads and writes
//!
//! Every read and write here names its own offset rather than seeking first. Two
//! reasons. A seek and a read is two syscalls where one will do, and more to the
//! point a file position is state shared by everything holding the descriptor,
//! which is the kind of thing that works until the day something else opens the
//! same store.
//!
//! # Not mapped
//!
//! The regions this module touches are a page and two 64 KiB slots, which is not
//! enough to be worth a mapping, and a mapping would take the choice of when
//! bytes reach the platter away from the code that has to be sure they have. The
//! segment region is a different question with a different answer, and reading it
//! is not what this module is for.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

use crate::error::Error;
use crate::manifest::{
    Committed, Manifest, SLOT_A_OFFSET, SLOT_B_OFFSET, SLOT_LEN, SUPERBLOCK_LEN, Slot, Superblock,
};

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
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)?;
        let superblock = Superblock::new(store, created);
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
        Ok(Self {
            file,
            superblock,
            manifest,
            slot: Slot::A,
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
        Ok(Self {
            file,
            superblock,
            manifest,
            slot,
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
    /// # Errors
    ///
    /// Returns [`Trouble::Format`] if the manifest is too large for a slot, and
    /// [`Trouble::Io`] if the write or the sync fails. In either case the store
    /// is still at the state it was at, because the slot being written is the
    /// one nothing is reading.
    pub fn commit(&mut self, manifest: Manifest, written: u64) -> Result<u64> {
        let mut next = manifest;
        next.epoch = self.manifest.epoch.saturating_add(1);
        next.written = written;
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
