//! Taking a file written by an older build and writing today's beside it.
//!
//! The engine refuses a format version it does not know rather than parsing it
//! hopefully, which is the right refusal and is also the whole problem: the
//! build that can read your file is the build you have stopped running. This is
//! the way across, and it exists now, while the step is one array in one
//! section, rather than at the point somebody needs it.
//!
//! # It writes a second file
//!
//! Never in place. The original is not touched, not renamed and not truncated,
//! and the tool refuses to write over anything that is already there. A
//! migration that failed halfway through a file would leave a store that is
//! neither version and that nothing will open, and no amount of care inside the
//! write makes that risk worth taking when the alternative is a second file and
//! a rename by whoever is watching.
//!
//! # What it does to a store
//!
//! A store is a container around segments, and the container has not changed. So
//! the new store is created with the same identifier, the same creation time and
//! the same log size as the old one, every segment is migrated and appended to
//! it in the order the manifest had them, and one manifest is committed at the
//! end carrying the counts the old one carried.
//!
//! The identifier is copied rather than made fresh on purpose. A migrated store
//! is the same store, and anything that recorded which store it was talking to
//! should still recognise it afterwards.
//!
//! # What it refuses
//!
//! A store with records in its log, because those are documents that are not in
//! a segment yet and a migration that replayed them would be doing two things at
//! once, and only one of them is reversible by deleting the file it wrote. Open
//! the store with something that flushes it first.
//!
//! A store with tombstones, because a tombstone bitmap lives outside the segment
//! it belongs to and moving segments moves it. Nothing writes tombstones yet, so
//! this is a guard against a later build rather than a case anybody will hit,
//! and a guard that refuses is better than one that quietly drops deletions.

use std::io::Write;
use std::path::Path;

use kura_core::file::{Result, Store, Trouble};
use kura_core::manifest::Segment;
use kura_core::mapping::Map;
use kura_core::{Error, FORMAT_VERSION, manifest, migrate};

/// What a migration did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Everything in the file is already at this build's version, so nothing was
    /// written.
    Current,
    /// A file was written, holding this many segments.
    Wrote { segments: usize },
    /// Something about the file means this will not touch it.
    Refused,
}

impl Outcome {
    /// Whether the caller has a file it can use.
    ///
    /// A file that was already current and a file that was just migrated both
    /// count. A refusal does not, which is what makes a refusal a failing exit:
    /// a script that carried on from here would be carrying on with a file the
    /// engine still will not open.
    #[must_use]
    pub const fn settled(&self) -> bool {
        !matches!(self, Self::Refused)
    }
}

/// Reads `from`, writes today's format to `into`, and says what it did.
///
/// `now` is written into the new store's manifest as the time of the commit, and
/// is passed in rather than read here so that the caller holds the one clock.
///
/// # Errors
///
/// Returns [`Trouble::Io`] if either file cannot be read or written, and
/// [`Trouble::Format`] if the input is not an index, is damaged, or is a version
/// outside what this build knows how to migrate. A file that needs no migration
/// is a result and not an error.
pub fn migrate(from: &Path, into: &Path, now: u64, out: &mut impl Write) -> Result<Outcome> {
    if from == into {
        writeln!(out, "  the file to write is the file to read")?;
        return Ok(Outcome::Refused);
    }
    // Checked here so the refusal names the file and comes before any work, and
    // still checked again by the open that does the writing, which is the one
    // that holds against two of these running at once.
    if into.exists() {
        writeln!(
            out,
            "  {} is already there, and this never writes over a file",
            into.display()
        )?;
        return Ok(Outcome::Refused);
    }
    let bytes = Map::open(from)?;

    writeln!(out, "{}", from.display())?;
    writeln!(out)?;

    if manifest::looks_like_a_store(&bytes) {
        store(from, into, now, out)
    } else {
        bare(&bytes, into, out)
    }
}

/// A single segment, which is a read, a migration and a write.
fn bare(bytes: &[u8], into: &Path, out: &mut impl Write) -> Result<Outcome> {
    let Some(migrated) = migrate::segment(bytes)? else {
        writeln!(
            out,
            "  this segment is already at version {FORMAT_VERSION}, so nothing was written"
        )?;
        return Ok(Outcome::Current);
    };
    let at = "segment 1 of 1";
    writeln!(
        out,
        "  {at:<22} version {} to {FORMAT_VERSION}, {} bytes to {}",
        version_of(bytes)?,
        bytes.len(),
        migrated.len()
    )?;
    write_new(into, &migrated)?;
    writeln!(out)?;
    writeln!(out, "  wrote {}", into.display())?;
    Ok(Outcome::Wrote { segments: 1 })
}

/// A store, which is the same segment by segment with a container around it.
fn store(from: &Path, into: &Path, now: u64, out: &mut impl Write) -> Result<Outcome> {
    let old = Store::open(from)?;

    if !old.log().is_empty() {
        writeln!(
            out,
            "  this store has {} bytes of records in its log that are not in a segment yet",
            old.log().tail() - old.log().head()
        )?;
        writeln!(
            out,
            "  open it with something that flushes them into a segment first, then migrate that"
        )?;
        return Ok(Outcome::Refused);
    }
    if let Some(at) = old
        .manifest()
        .segments
        .iter()
        .position(|segment| segment.tombstones_len != 0)
    {
        writeln!(
            out,
            "  segment {} of this store has a tombstone bitmap, which lives outside the segment",
            at + 1
        )?;
        writeln!(
            out,
            "  moving the segments would move it, and nothing here knows how to move it with them"
        )?;
        return Ok(Outcome::Refused);
    }

    let view = old.view()?;
    let held = view.len();
    let mut work = Vec::with_capacity(held);
    let mut moved = 0usize;
    for (n, (described, bytes)) in view.described().iter().zip(view.all()).enumerate() {
        let at = format!("segment {} of {held}", n + 1);
        if let Some(migrated) = migrate::segment(bytes)? {
            writeln!(
                out,
                "  {at:<22} version {} to {FORMAT_VERSION}, {} bytes to {}",
                version_of(bytes)?,
                bytes.len(),
                migrated.len()
            )?;
            moved += 1;
            work.push((*described, migrated));
        } else {
            writeln!(out, "  {at:<22} already version {FORMAT_VERSION}")?;
            work.push((*described, bytes.to_vec()));
        }
    }

    writeln!(out)?;
    if moved == 0 {
        // Including an empty store, which has nothing in it that could be an
        // older version. Rewriting it would be a new file that differs from this
        // one in nothing a person asked for.
        writeln!(
            out,
            "  every segment in this store is already at version {FORMAT_VERSION}, so nothing was written"
        )?;
        return Ok(Outcome::Current);
    }

    // The superblock rather than a fresh one, so that the store that comes out
    // is the same store: the identifier anything holding a reference to it wrote
    // down, and the log the store was sized for.
    let front = old.superblock();
    let mut new = Store::create_with_log(into, front.store, front.created, front.wal_len)?;
    let mut manifest = old.manifest().clone();
    manifest.segments.clear();
    for (described, bytes) in &work {
        let written = new.append_segment(bytes, described.docs, described.created)?;
        // Everything the append cannot know, which is everything that is about
        // the segment rather than about where it landed.
        manifest.segments.push(Segment {
            first_live: described.first_live,
            generation: described.generation,
            level: described.level,
            flags: described.flags,
            ..written
        });
    }
    let epoch = new.commit(manifest, now)?;

    writeln!(out, "  wrote {}", into.display())?;
    writeln!(
        out,
        "  {held} segments, {} documents, at epoch {epoch}",
        old.manifest().live
    )?;
    Ok(Outcome::Wrote { segments: held })
}

/// The format version in a segment's header.
///
/// Read here rather than taken from the opened segment because this is for the
/// line that says what was migrated, and by the time there is something to say
/// the migrated segment is the one in hand.
fn version_of(bytes: &[u8]) -> Result<u16> {
    let found = bytes.get(8..10).ok_or(Trouble::Format(Error::Truncated {
        needed: 10,
        available: bytes.len(),
    }))?;
    Ok(u16::from_le_bytes([found[0], found[1]]))
}

/// Writes a file that was not there, and refuses one that was.
fn write_new(into: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(into)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use kura_core::index::Reader;
    use kura_core::segment::Segment as Opened;

    /// A fixed time, so that nothing here depends on the clock.
    const WHEN: u64 = 1_700_000_000;

    /// Where the format fixtures live.
    fn testdata() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/format")
    }

    /// A path of this test's own, under a directory this process shares.
    fn a_path(name: &str) -> std::path::PathBuf {
        let directory = std::env::temp_dir().join(format!("kura-migrate-{}", std::process::id()));
        std::fs::create_dir_all(&directory).expect("a temporary directory");
        let path = directory.join(format!("{name}.kura"));
        std::fs::remove_file(&path).ok();
        path
    }

    /// Copies a fixture somewhere this test can point at it.
    fn a_copy(name: &str, as_name: &str) -> std::path::PathBuf {
        let path = a_path(as_name);
        std::fs::copy(testdata().join(name), &path).expect("the fixture is there");
        path
    }

    /// Runs a migration and hands back the report and the outcome.
    fn run(from: &Path, into: &Path) -> (String, Outcome) {
        let mut out = Vec::new();
        let outcome = migrate(from, into, WHEN, &mut out).expect("the file is readable");
        (String::from_utf8(out).expect("the report is text"), outcome)
    }

    #[test]
    fn a_version_one_segment_becomes_the_segment_this_build_writes() {
        let from = a_copy("v1/segment.kura", "one-segment-in");
        let into = a_path("one-segment-out");
        let (report, outcome) = run(&from, &into);

        assert_eq!(outcome, Outcome::Wrote { segments: 1 });
        assert!(report.contains("version 1 to 2"), "{report}");
        let written = std::fs::read(&into).expect("the migration wrote it");
        let expected =
            std::fs::read(testdata().join("segment.kura")).expect("the fixture is there");
        assert_eq!(written, expected);
    }

    #[test]
    fn a_segment_already_current_is_left_where_it_is() {
        let from = a_copy("segment.kura", "current-segment-in");
        let into = a_path("current-segment-out");
        let (report, outcome) = run(&from, &into);

        assert_eq!(outcome, Outcome::Current);
        assert!(report.contains("already at version 2"), "{report}");
        assert!(
            !into.exists(),
            "a migration with nothing to do wrote a file anyway"
        );
    }

    #[test]
    fn a_version_one_store_becomes_a_store_of_migrated_segments() {
        let from = a_copy("v1/store.kura", "one-store-in");
        let into = a_path("one-store-out");
        let (report, outcome) = run(&from, &into);

        assert_eq!(outcome, Outcome::Wrote { segments: 2 });
        assert!(report.contains("version 1 to 2"), "{report}");

        let expected =
            std::fs::read(testdata().join("segment.kura")).expect("the fixture is there");
        let old = Store::open(&from).expect("the fixture opens");
        let new = Store::open(&into).expect("the migration wrote a store");

        // The same store, not a new one that happens to hold the same documents.
        assert_eq!(new.superblock().store, old.superblock().store);
        assert_eq!(new.superblock().created, old.superblock().created);
        assert_eq!(new.superblock().wal_len, old.superblock().wal_len);
        assert_eq!(new.manifest().live, old.manifest().live);
        assert_eq!(new.manifest().total, old.manifest().total);

        let view = new.view().expect("the segments are there");
        assert_eq!(view.len(), 2);
        for bytes in view.all() {
            assert_eq!(bytes, expected.as_slice());
            let segment = Opened::open(bytes).expect("each segment verifies");
            let index = Reader::open(&segment).expect("each segment is an index");
            assert_eq!(index.documents(), 400);
        }
    }

    #[test]
    fn a_store_already_current_is_left_where_it_is() {
        let from = a_copy("store.kura", "current-store-in");
        let into = a_path("current-store-out");
        let (_, outcome) = run(&from, &into);

        assert_eq!(outcome, Outcome::Current);
        assert!(
            !into.exists(),
            "a migration with nothing to do wrote a store anyway"
        );
    }

    #[test]
    fn a_store_with_records_in_its_log_is_refused() {
        let path = a_path("log-not-empty");
        let mut store = Store::create_with_log(&path, 7, WHEN, 1 << 20).expect("a new store");
        store
            .append(1, b"a document nobody has flushed")
            .expect("room");
        let mut next = store.manifest().clone();
        next.live = 1;
        next.total = 1;
        store.commit(next, WHEN).expect("the commit lands");
        drop(store);

        let into = a_path("log-not-empty-out");
        let (report, outcome) = run(&path, &into);
        assert_eq!(outcome, Outcome::Refused);
        assert!(report.contains("not in a segment yet"), "{report}");
        assert!(!into.exists(), "a refused migration wrote a file anyway");
    }

    #[test]
    fn a_file_that_is_already_there_is_not_written_over() {
        let from = a_copy("v1/segment.kura", "no-clobber-in");
        let into = a_path("no-clobber-out");
        std::fs::write(&into, b"something somebody wanted").expect("a writable path");

        let (report, outcome) = run(&from, &into);
        assert_eq!(outcome, Outcome::Refused);
        assert!(report.contains("never writes over a file"), "{report}");
        assert_eq!(
            std::fs::read(&into).expect("still there"),
            b"something somebody wanted"
        );
    }

    #[test]
    fn writing_back_over_the_input_is_refused() {
        let from = a_copy("v1/segment.kura", "in-place");
        let before = std::fs::read(&from).expect("the fixture is there");
        let (report, outcome) = run(&from, &from);
        assert_eq!(outcome, Outcome::Refused);
        assert!(report.contains("the file to read"), "{report}");
        assert_eq!(std::fs::read(&from).expect("still there"), before);
    }

    #[test]
    fn a_damaged_segment_is_refused_rather_than_migrated() {
        let path = a_path("damaged-in");
        let mut bytes = std::fs::read(testdata().join("v1/segment.kura")).expect("the fixture");
        let at = bytes.len() / 2;
        bytes[at] ^= 0x01;
        std::fs::write(&path, &bytes).expect("a writable path");

        let into = a_path("damaged-out");
        let mut sink = Vec::new();
        assert!(
            migrate(&path, &into, WHEN, &mut sink).is_err(),
            "a migration ran over a segment whose checksum did not match"
        );
        assert!(!into.exists(), "a failed migration left a file behind");
    }
}
