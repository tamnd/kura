//! Getting a store that will not open back to one that will.
//!
//! There is exactly one repair a store supports, and it is worth being plain
//! about what it is before anything else, because the word promises more than
//! the format can deliver.
//!
//! A segment is immutable and holds no redundancy. Nothing in it can be worked
//! out from anything else in it, so a segment with a wrong byte in its postings
//! is a segment that stays wrong, and no tool is going to change that. What a
//! store does hold twice is its manifest, and what a manifest holds is the list
//! of which segments count. So the repair is this: commit a manifest that leaves
//! out the segments that no longer read, and keep the rest.
//!
//! That trade is a loss and it should be described as one. The documents in a
//! dropped segment are gone from the store, though they were already gone before
//! this ran, since a segment that does not decode was not answering queries
//! either. What changes is that the rest of the store becomes usable again,
//! which is the difference between losing a tenth of a corpus and losing all of
//! it. It is still a decision a person makes rather than one a tool should make
//! quietly, so nothing is written without `--commit`.
//!
//! # Why this is safe to run
//!
//! It never writes a byte of a segment. It writes one manifest, into the slot
//! that is not the committed one, which is the path every ordinary commit takes
//! and is atomic for the same reason: either the write lands whole or the old
//! slot is still the one with the higher epoch. The manifest it replaced stays
//! in the other slot until something commits again, so a repair that turns out
//! to have been a mistake has not yet destroyed the evidence.

use std::io::Write;
use std::path::Path;

use kura_core::file::{Result, Store};
use kura_core::manifest::{self, Segment};
use kura_core::mapping::Map;

use crate::verify;

/// What a repair found and what it did about it.
#[derive(Debug, Default, Clone, Copy)]
pub struct Outcome {
    /// How many segments no longer read.
    pub damaged: usize,
    /// How many documents were in them.
    pub lost: u64,
    /// Whether a manifest was written.
    pub committed: bool,
}

impl Outcome {
    /// Whether the store is readable as it stands.
    ///
    /// A store with nothing wrong with it, or one that has just had the parts
    /// that do not read committed away. A store with damage that was only
    /// reported is neither, which is what makes a dry run over a damaged store a
    /// failing exit: the report is accurate and nothing was fixed.
    #[must_use]
    pub const fn settled(&self) -> bool {
        self.damaged == 0 || self.committed
    }
}

/// Looks at a store, says which segments no longer read, and with `commit`
/// writes a manifest without them.
///
/// `now` is written into the manifest as the time of the commit, and is passed
/// in rather than read here so that the caller holds the one clock.
///
/// # Errors
///
/// Returns [`kura_core::file::Trouble`] if the file cannot be read, if the
/// report cannot be written, or if the manifest cannot be committed. A damaged
/// store is a result and not an error.
pub fn repair(path: &Path, commit: bool, now: u64, out: &mut impl Write) -> Result<Outcome> {
    let bytes = Map::open(path)?;

    writeln!(out, "{}", path.display())?;
    writeln!(out)?;

    if !manifest::looks_like_a_store(&bytes) {
        return bare(&bytes, out);
    }

    let (superblock, state) = match manifest::front(&bytes) {
        Ok(front) => front,
        Err(error) => {
            writeln!(out, "  neither manifest slot in this store decodes")?;
            writeln!(out, "      {error}")?;
            writeln!(out)?;
            // The manifest is the only part of a store with a spare copy, so a
            // store that has lost both copies of it has lost the one thing this
            // could have repaired from.
            writeln!(
                out,
                "  the segments may well be intact, and nothing here can say which of them are"
            )?;
            return Ok(Outcome::default());
        }
    };

    let mut outcome = Outcome::default();
    let mut keep: Vec<Segment> = Vec::with_capacity(state.segments.len());
    let held = state.segments.len();
    let ranges = manifest::locate_each(&superblock, &state, bytes.len());

    for (n, (described, range)) in state.segments.iter().zip(ranges).enumerate() {
        let at = format!("segment {} of {held}", n + 1);
        let Ok(range) = range else {
            // Nothing to read and so nothing to decide. A descriptor pointing
            // outside the file is the one kind of damage where the segment
            // itself may be perfectly good and still unusable, and this cannot
            // tell the difference.
            writeln!(
                out,
                "  {at:<22} unreachable, the manifest points outside the file"
            )?;
            outcome.damaged += 1;
            outcome.lost += u64::from(described.docs);
            continue;
        };

        // The same checks `verify` prints, run quietly. A repair that decided
        // what to drop on a cheaper check than the one a person had just run
        // would be a tool that threw away segments verify had called good.
        let found = verify::one_segment(&bytes[range], Some(described.docs), &mut std::io::sink())?;
        if found.failures == 0 && found.skipped == 0 {
            writeln!(out, "  {at:<22} reads, {} documents", described.docs)?;
            keep.push(*described);
        } else {
            writeln!(
                out,
                "  {at:<22} does not read, {} checks failed, {} documents",
                found.failures + found.skipped,
                described.docs
            )?;
            outcome.damaged += 1;
            outcome.lost += u64::from(described.docs);
        }
    }

    writeln!(out)?;
    if outcome.damaged == 0 {
        writeln!(out, "  every segment reads, so there is nothing to repair")?;
        return Ok(outcome);
    }

    writeln!(
        out,
        "  dropping {} of {held} segments loses {} of {} documents",
        outcome.damaged, outcome.lost, state.total
    )?;
    if keep.is_empty() {
        writeln!(
            out,
            "  which is every document in this store, so this leaves an empty store behind"
        )?;
    }
    writeln!(out, "  run verify for what is wrong with each of them")?;
    writeln!(out)?;

    if !commit {
        writeln!(out, "  nothing was written, pass --commit to write it")?;
        return Ok(outcome);
    }

    let (into, epoch) = write(path, keep, now)?;
    outcome.committed = true;
    writeln!(out, "  wrote slot {into:?} at epoch {epoch}")?;
    writeln!(
        out,
        "  the manifest this replaced is in the other slot until the next commit"
    )?;
    Ok(outcome)
}

/// Commits a manifest holding only the segments that read.
///
/// Hands back which slot it landed in and the epoch it landed at, which is what
/// somebody reading this against a later `verify` needs.
fn write(path: &Path, keep: Vec<Segment>, now: u64) -> Result<(manifest::Slot, u64)> {
    let mut store = Store::open(path)?;
    let into = store.slot().other();
    let mut next = store.manifest().clone();
    // Counted up from what is left rather than subtracted from what was there,
    // because the totals in a manifest under repair are exactly the numbers that
    // have just been shown not to be trustworthy.
    next.total = keep.iter().map(|segment| u64::from(segment.docs)).sum();
    next.live = next.total;
    next.segments = keep;
    let epoch = store.commit(next, now)?;
    Ok((into, epoch))
}

/// A single segment, which has nothing to be repaired from.
fn bare(bytes: &[u8], out: &mut impl Write) -> Result<Outcome> {
    let found = verify::one_segment(bytes, None, &mut std::io::sink())?;
    if found.failures == 0 && found.skipped == 0 {
        writeln!(out, "  this segment reads, so there is nothing to repair")?;
        return Ok(Outcome::default());
    }

    writeln!(
        out,
        "  this is a single segment and not a store, and {} checks failed",
        found.failures + found.skipped
    )?;
    writeln!(out)?;
    writeln!(
        out,
        "  a segment holds no second copy of anything, so there is nothing here to rebuild it from"
    )?;
    writeln!(
        out,
        "  index the documents again, or take the segment from a backup"
    )?;
    Ok(Outcome {
        damaged: 1,
        lost: 0,
        committed: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use kura_core::index::Writer;
    use kura_core::manifest::Manifest;

    /// How many documents each segment a test builds holds.
    const PER_SEGMENT: u32 = 20;

    /// A fixed time, so that nothing here depends on the clock.
    const WHEN: u64 = 1_700_000_000;

    /// A path of this test's own, under a directory this process shares.
    ///
    /// The name is the test's own because these run in parallel, and two of them
    /// working on the same store would delete the file out from under each
    /// other.
    fn a_path(name: &str) -> std::path::PathBuf {
        let directory = std::env::temp_dir().join(format!("kura-repair-{}", std::process::id()));
        std::fs::create_dir_all(&directory).expect("a temporary directory");
        let path = directory.join(format!("{name}.kura"));
        std::fs::remove_file(&path).ok();
        path
    }

    /// A store on disk holding `count` segments, committed one at a time.
    fn a_store(name: &str, count: usize) -> std::path::PathBuf {
        let path = a_path(name);
        let mut store = Store::create_with_log(&path, 1, WHEN, 1 << 20).expect("a new store");
        for round in 0..count {
            let mut writer = Writer::new();
            for id in 0..PER_SEGMENT {
                writer
                    .add_with_fields(
                        &format!("segment {round} document {id} about storage and retrieval"),
                        [("path", format!("doc{round}-{id}.txt").as_bytes())],
                    )
                    .expect("a small document fits");
            }
            let segment = writer.finish().expect("what was written decodes");
            let described = store
                .append_segment(&segment, PER_SEGMENT, WHEN)
                .expect("the segment is written");
            let mut next = store.manifest().clone();
            next.live += u64::from(PER_SEGMENT);
            next.total += u64::from(PER_SEGMENT);
            next.segments.push(described);
            store.commit(next, WHEN).expect("the commit lands");
        }
        path
    }

    /// Flips a bit in the middle of the nth segment of a store.
    fn damage(path: &std::path::Path, nth: usize) {
        let mut bytes = std::fs::read(path).expect("the store reads");
        let at = {
            let (superblock, state) = manifest::front(&bytes).expect("the front decodes");
            let ranges =
                manifest::locate(&superblock, &state, bytes.len()).expect("the segments are found");
            let range = ranges[nth].clone();
            range.start + range.len() / 2
        };
        bytes[at] ^= 0x01;
        std::fs::write(path, &bytes).expect("the store is written back");
    }

    /// Runs a repair and hands back the report and the outcome.
    fn run(path: &std::path::Path, commit: bool) -> (String, Outcome) {
        let mut out = Vec::new();
        let outcome = repair(path, commit, WHEN, &mut out).expect("the file is readable");
        (String::from_utf8(out).expect("the report is text"), outcome)
    }

    /// How many segments a store's committed manifest names.
    fn segments_in(path: &std::path::Path) -> usize {
        Store::open(path)
            .expect("the store opens")
            .manifest()
            .segments
            .len()
    }

    #[test]
    fn a_store_with_nothing_wrong_is_left_alone() {
        let path = a_store("intact", 2);
        let (report, outcome) = run(&path, true);
        assert_eq!(outcome.damaged, 0, "{report}");
        assert!(!outcome.committed, "{report}");
        assert!(outcome.settled(), "{report}");
        assert!(report.contains("nothing to repair"), "{report}");
        assert_eq!(segments_in(&path), 2, "{report}");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_dry_run_says_what_it_would_cost_and_writes_nothing() {
        // The whole reason the flag exists. What this is about to do is throw
        // documents away, and a tool that did that because somebody typed the
        // name of the command would be the wrong tool.
        let path = a_store("dry", 3);
        damage(&path, 1);

        let (report, outcome) = run(&path, false);
        assert_eq!(outcome.damaged, 1, "{report}");
        assert_eq!(outcome.lost, u64::from(PER_SEGMENT), "{report}");
        assert!(!outcome.committed, "{report}");
        assert!(!outcome.settled(), "{report}");
        assert!(report.contains("nothing was written"), "{report}");
        assert_eq!(segments_in(&path), 3, "{report}");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn committing_drops_the_damaged_segment_and_keeps_the_rest() {
        let path = a_store("commit", 3);
        damage(&path, 1);

        let (report, outcome) = run(&path, true);
        assert_eq!(outcome.damaged, 1, "{report}");
        assert!(outcome.committed, "{report}");
        assert!(outcome.settled(), "{report}");
        assert_eq!(segments_in(&path), 2, "{report}");

        // And the store comes back clean, which is the point of the exercise.
        let (again, second) = run(&path, false);
        assert_eq!(second.damaged, 0, "{again}");
        assert!(again.contains("nothing to repair"), "{again}");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn what_is_left_still_answers_queries() {
        // A manifest that opens is not the same fact as a store that works, and
        // the second one is what somebody running this wanted.
        let path = a_store("queries", 3);
        damage(&path, 0);
        run(&path, true);

        let store = Store::open(&path).expect("the store opens");
        let view = store.view().expect("the surviving segments map");
        assert_eq!(view.len(), 2);
        for bytes in view.all() {
            let segment = kura_core::segment::Segment::open(bytes).expect("each one decodes");
            let reader = kura_core::index::Reader::open(&segment).expect("each one reads");
            assert_eq!(reader.documents(), PER_SEGMENT);
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn the_counts_are_worked_out_again_from_what_is_left() {
        let path = a_store("counts", 3);
        damage(&path, 0);
        run(&path, true);

        let store = Store::open(&path).expect("the store opens");
        assert_eq!(store.manifest().total, u64::from(PER_SEGMENT) * 2);
        assert_eq!(store.manifest().live, u64::from(PER_SEGMENT) * 2);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_segment_the_manifest_points_outside_the_file_is_dropped() {
        let path = a_store("outside", 2);
        {
            let mut store = Store::open(&path).expect("the store opens");
            let mut next = store.manifest().clone();
            next.segments[0].offset = 1 << 40;
            store.commit(next, WHEN).expect("the commit lands");
        }

        let (report, outcome) = run(&path, true);
        assert_eq!(outcome.damaged, 1, "{report}");
        assert!(report.contains("unreachable"), "{report}");
        assert_eq!(segments_in(&path), 1, "{report}");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_store_where_every_segment_is_damaged_says_so_before_it_empties_it() {
        let path = a_store("everything", 2);
        damage(&path, 0);
        damage(&path, 1);

        let (report, outcome) = run(&path, false);
        assert_eq!(outcome.damaged, 2, "{report}");
        assert!(report.contains("every document in this store"), "{report}");
        assert_eq!(segments_in(&path), 2, "{report}");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn the_manifest_that_was_replaced_is_still_in_the_other_slot() {
        // What makes this safe to run at all. A repair that turns out to have
        // been a mistake has not yet destroyed the evidence.
        let path = a_store("evidence", 2);
        damage(&path, 1);
        let before = Store::open(&path).expect("the store opens").slot();
        run(&path, true);

        let after = Store::open(&path).expect("the store opens");
        assert_ne!(after.slot(), before);

        let bytes = std::fs::read(&path).expect("the store reads");
        let at = usize::try_from(before.offset()).expect("an offset fits");
        let old = Manifest::decode(&bytes[at..][..manifest::SLOT_LEN]).expect("the old slot reads");
        assert_eq!(old.segments.len(), 2);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_bare_segment_that_reads_needs_no_repair() {
        let path = a_path("bare-good");
        let mut writer = Writer::new();
        writer.add("storage and retrieval").expect("one fits");
        std::fs::write(&path, writer.finish().expect("it decodes")).expect("it is written");

        let (report, outcome) = run(&path, true);
        assert_eq!(outcome.damaged, 0, "{report}");
        assert!(report.contains("nothing to repair"), "{report}");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_bare_segment_that_does_not_read_says_what_cannot_be_done() {
        // The honest answer, and the one worth saying out loud. There is no
        // second copy of anything in a segment, so there is nothing to rebuild
        // it from and no amount of tooling changes that.
        let path = a_path("bare-bad");
        let mut writer = Writer::new();
        for id in 0..50u32 {
            writer
                .add(&format!("document {id} about storage and retrieval"))
                .expect("one fits");
        }
        let mut bytes = writer.finish().expect("it decodes");
        let at = bytes.len() / 2;
        bytes[at] ^= 0x01;
        std::fs::write(&path, &bytes).expect("it is written");

        let (report, outcome) = run(&path, true);
        assert_eq!(outcome.damaged, 1, "{report}");
        assert!(!outcome.settled(), "{report}");
        assert!(report.contains("index the documents again"), "{report}");
        std::fs::remove_file(&path).ok();
    }
}
