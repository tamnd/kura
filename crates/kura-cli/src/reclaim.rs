//! Writing a store again beside itself so it is only as big as what it holds.
//!
//! A fold gives back the segment count and not the space. It appends the segment
//! it made and leaves the ones it replaced exactly where they are, because a
//! query that started before the commit is still reading them through offsets it
//! took at the time. That is the correct thing for a commit to do and it means a
//! store that is folded often is a store that is mostly bytes nothing will read
//! again. This is the command that takes them back.
//!
//! # It writes a second file
//!
//! Never in place, and never over a file that is already there. What comes out
//! is a store carrying the same identifier, creation time and log size as the
//! one that went in, holding the same documents in the same order with the same
//! deletions applied, and nothing else. The original is not touched, not renamed
//! and not truncated, and what to do with the two of them afterwards belongs to
//! whoever ran this. A run that dies halfway leaves a partial file nothing points
//! at and a store nobody has written to.
//!
//! # The log is the one thing it will change on purpose
//!
//! `--log` gives the new store a log region of a size somebody chose, and this
//! is the only command that can, because a log is fixed when a store is made and
//! the segments start where it ends.
//!
//! It is here because the log is often most of a small store. A run picks a size
//! for the largest corpus it might index and then indexes one that fits in a
//! fraction of it, and what comes out is a file that is nine parts log. That
//! makes the reclaim look like it has failed when it has done everything it
//! could, so the report names the log and the segments separately and a `--log`
//! is how the other nine tenths come off.
//!
//! A log the run is shrinking is a log with nothing in it, because a rewrite is
//! refused unless the log is already consumed. A log it is growing costs the
//! difference in file length and nothing else.
//!
//! # It says what it will save before it does anything
//!
//! The report is printed from the manifest first: how much of the file the
//! segments account for, how much of it is in front of them, and how much is
//! neither, which is what a rewrite gives back. Only then is the file written,
//! and the figure it actually came to is printed under the one that was
//! predicted. A store with nothing stranded in it is told so and nothing is
//! written, because the second file would differ from the first in nothing
//! anybody asked for.
//!
//! # What it refuses
//!
//! A store with records in its log that no commit has consumed. Those are
//! documents somebody was promised and they name their segments by position, so
//! a file whose segments have moved is a file they no longer describe. Open the
//! store with something that flushes them into a segment first, then reclaim
//! that.
//!
//! A file that is a single segment rather than a store, which has no manifest,
//! no stranded segments and so nothing to give back.

use std::io::Write;
use std::path::Path;
use std::time::Instant;

use kura_core::file::{Result, Store};
use kura_core::mapping::Map;
use kura_core::{manifest, reclaim};

/// What a run of this came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// A file was written, and this is what it saved.
    Wrote(reclaim::Reclaimed),
    /// There was nothing stranded, so nothing was written.
    Nothing,
    /// Something about the file or the target means this will not touch it.
    Refused,
}

impl Outcome {
    /// Whether the caller has an answer it can act on.
    ///
    /// Having written a smaller file and having been told there was nothing to
    /// save both count, because both leave the person who asked knowing where
    /// the space went. A refusal does not, which is what makes it a failing
    /// exit: the store is in whatever state made this refuse, and a script that
    /// carried on would be carrying on from a rewrite that did not happen.
    #[must_use]
    pub const fn settled(&self) -> bool {
        !matches!(self, Self::Refused)
    }
}

/// Reads the store at `from`, writes one holding only what it names to `into`.
///
/// `now` goes into the new store's manifest as the time of the commit, and is
/// passed in rather than read here so that the caller holds the one clock.
///
/// `log` of `None` carries the source's log length across, and `Some` gives the
/// new store that one instead.
///
/// # Errors
///
/// Returns [`kura_core::file::Trouble`] if the source cannot be read or is not a
/// file this build opens, if a tombstone set does not belong to the segment it
/// is filed under, or if the write, the sync or the commit fails. A store that
/// is refused is a result and not an error.
pub fn reclaim(
    from: &Path,
    into: &Path,
    now: u64,
    log: Option<u64>,
    out: &mut impl Write,
) -> Result<Outcome> {
    if from == into {
        writeln!(out, "  the file to write is the file to read")?;
        return Ok(Outcome::Refused);
    }
    // Checked here so the refusal names the file and comes before any reading,
    // and checked again by the create that does the writing, which is the one
    // that holds against two of these running at once.
    if into.exists() {
        writeln!(
            out,
            "  {} is already there, and this never writes over a file",
            into.display()
        )?;
        return Ok(Outcome::Refused);
    }

    writeln!(out, "{}", from.display())?;
    writeln!(out)?;

    {
        let bytes = Map::open(from)?;
        if !manifest::looks_like_a_store(&bytes) {
            writeln!(
                out,
                "  this is a single segment and not a store, so there is nothing stranded in it"
            )?;
            return Ok(Outcome::Refused);
        }
    }

    let store = Store::open(from)?;
    let committed = store.manifest();
    if committed.wal_head != committed.wal_tail {
        writeln!(
            out,
            "  this store holds records from {} to {} in its log that no commit has consumed",
            committed.wal_head, committed.wal_tail
        )?;
        writeln!(
            out,
            "  they name their segments by position and a rewrite moves the segments, so open it"
        )?;
        writeln!(
            out,
            "  with something that flushes them into a segment first, then reclaim that"
        )?;
        return Ok(Outcome::Refused);
    }

    let was = length(from);
    let front = store.superblock().segments_offset;
    let page = u64::from(store.superblock().page);
    // What the segments account for, padded the way the region pads them, so
    // that the prediction is the length of a file rather than the sum of what is
    // in it. A segment and its tombstone set each start on a page.
    let named: u64 = committed
        .segments
        .iter()
        .map(|segment| round(segment.len, page) + round(u64::from(segment.tombstones_len), page))
        .sum();
    let stranded = was.saturating_sub(front).saturating_sub(named);
    // Where the segments will start in the file this is about to write, which is
    // the same place unless a log was asked for. Rounded here the way a store
    // creation rounds it, so the prediction is the length the file will have
    // rather than the number that was typed.
    let ahead = log.map_or(front, |asked| manifest::WAL_OFFSET + round(asked, page));

    said(out, "segments", committed.segments.len() as u64)?;
    said(out, "documents", committed.total)?;
    said(out, "  live", committed.live)?;
    writeln!(out)?;
    said(out, "file bytes", was)?;
    said(out, "  superblock and log", front)?;
    said(out, "  segments", named)?;
    said(out, "  stranded", stranded)?;
    if ahead != front {
        said(out, "  log becomes", ahead)?;
    }
    drop(store);

    // Two reasons to write, and either one on its own is enough. A store with
    // nothing stranded still shrinks if its log does, and a store with plenty
    // stranded shrinks whether the log moves or not.
    if stranded == 0 && ahead == front {
        writeln!(out)?;
        writeln!(
            out,
            "  every byte past the log is a byte a segment names, so a rewrite would give"
        )?;
        writeln!(out, "  back nothing and nothing was written")?;
        return Ok(Outcome::Nothing);
    }

    // The file that comes out is its front, then what the segments account for,
    // and the segments are the same either way. So what the two files differ by
    // is the old front and what was stranded past it against the new front, and
    // that is the whole of the prediction whichever way the log went.
    let losing = front + stranded;
    writeln!(out)?;
    if losing >= ahead {
        writeln!(
            out,
            "  writing {} and giving back about {} bytes",
            into.display(),
            losing - ahead
        )?;
    } else {
        writeln!(
            out,
            "  writing {} with a longer log, so it comes out about {} bytes larger",
            into.display(),
            ahead - losing
        )?;
    }

    let start = Instant::now();
    let done = reclaim::rewrite_with_log(from, into, now, log)?;
    came_to(out, start.elapsed(), &done)?;
    Ok(Outcome::Wrote(done))
}

/// The half of the report that is written after the file is.
///
/// Under the prediction rather than instead of it, so that the two are read
/// together and a run whose arithmetic was wrong says so on its own output.
fn came_to(
    out: &mut impl Write,
    took: std::time::Duration,
    done: &reclaim::Reclaimed,
) -> std::io::Result<()> {
    writeln!(out)?;
    writeln!(
        out,
        "  {:<20} {:>12.2} s",
        "rewrite wall",
        took.as_secs_f32()
    )?;
    said(out, "segments", done.segments as u64)?;
    said(out, "documents", done.documents)?;
    said(out, "was", done.was)?;
    said(out, "now", done.now)?;
    said(out, "saved", done.saved())?;
    writeln!(out)?;
    writeln!(
        out,
        "  nothing was renamed and nothing was deleted, because the rename is the step"
    )?;
    writeln!(
        out,
        "  that destroys something and it belongs to whoever decided to do this"
    )
}

/// `n` taken up to the next multiple of `page`, or `n` if the page is nothing.
///
/// A page of zero is a superblock that has already failed to make sense, and a
/// division by it here would be a panic in the middle of a report rather than
/// the refusal it deserves somewhere else.
const fn round(n: u64, page: u64) -> u64 {
    if page == 0 {
        return n;
    }
    n.div_ceil(page) * page
}

/// One line of the report, a name and a number.
fn said(out: &mut impl Write, what: &str, number: u64) -> std::io::Result<()> {
    writeln!(out, "  {what:<20} {number:>12}")
}

/// How long the file is, or zero if that cannot be asked.
fn length(path: &Path) -> u64 {
    std::fs::metadata(path).map_or(0, |about| about.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use kura_core::bitmap::Bitmap;
    use kura_core::index::Writer;

    /// A fixed time, so that nothing here depends on the clock.
    const WHEN: u64 = 1_700_000_000;

    /// A path of this test's own, under a directory this process shares.
    fn a_path(name: &str) -> std::path::PathBuf {
        let directory =
            std::env::temp_dir().join(format!("kura-reclaim-cli-{}", std::process::id()));
        std::fs::create_dir_all(&directory).expect("a temporary directory");
        let path = directory.join(format!("{name}.kura"));
        std::fs::remove_file(&path).ok();
        path
    }

    /// A store holding `count` segments of five keyed documents each.
    fn a_store(name: &str, count: usize) -> std::path::PathBuf {
        let path = a_path(name);
        let mut store = Store::create_with_log(&path, 1, WHEN, 1 << 20).expect("a new store");
        for round in 0..count {
            let mut writer = Writer::new();
            for id in 0..5u32 {
                let key = format!("segment-{round}-{id}");
                writer
                    .add_keyed_with_fields(
                        key.as_bytes(),
                        "storage and retrieval and the words they share",
                        [("path", key.as_bytes())],
                    )
                    .expect("a document");
            }
            let docs = u32::try_from(writer.len()).expect("five documents fit");
            let bytes = writer.finish().expect("a segment");
            let described = store.append_segment(&bytes, docs, WHEN).expect("appended");
            let mut manifest = store.manifest().clone();
            manifest.segments.push(described);
            manifest.total += u64::from(docs);
            manifest.live += u64::from(docs);
            store.commit(manifest, WHEN).expect("committed");
        }
        path
    }

    #[test]
    fn a_folded_store_gives_back_what_the_fold_stranded() {
        let path = a_store("folded", 6);
        {
            let mut store = Store::open(&path).expect("a store");
            store.compact(0..6, WHEN, 1).expect("a fold");
        }
        let into = a_path("folded-out");

        let mut out = Vec::new();
        let done = reclaim(&path, &into, WHEN, None, &mut out).expect("reclaims");
        let report = String::from_utf8(out).expect("the report is text");
        let Outcome::Wrote(done) = done else {
            panic!("nothing was written: {report}");
        };
        assert_eq!(done.segments, 1);
        assert!(done.saved() > 0, "{report}");
        assert!(done.now < done.was, "{report}");
        // The prediction and the outcome are both in the report, and a run where
        // they disagree by more than a page is a run whose arithmetic is wrong.
        assert!(report.contains("giving back about"), "{report}");
        assert!(report.contains("nothing was renamed"), "{report}");

        // Every key survived, which is what the whole thing is trusted with.
        let store = Store::open(&into).expect("the store that came out");
        let view = store.view().expect("a view");
        for round in 0..6 {
            for id in 0..5 {
                let key = format!("segment-{round}-{id}");
                assert!(
                    view.document(key.as_bytes()).expect("a lookup").is_some(),
                    "{key} went missing"
                );
            }
        }
    }

    #[test]
    fn what_it_predicted_and_what_it_saved_agree_to_within_a_page() {
        let path = a_store("predicted", 8);
        {
            let mut store = Store::open(&path).expect("a store");
            store.compact(0..8, WHEN, 1).expect("a fold");
        }
        let into = a_path("predicted-out");

        let mut out = Vec::new();
        let done = reclaim(&path, &into, WHEN, None, &mut out).expect("reclaims");
        let report = String::from_utf8(out).expect("the report is text");
        let Outcome::Wrote(done) = done else {
            panic!("nothing was written: {report}");
        };
        let predicted: u64 = report
            .lines()
            .find_map(|line| line.trim().strip_prefix("stranded"))
            .and_then(|rest| rest.trim().parse().ok())
            .unwrap_or_else(|| panic!("no stranded line in {report}"));
        let apart = predicted.abs_diff(done.saved());
        assert!(
            apart <= 4096,
            "predicted {predicted} and saved {}, {apart} apart",
            done.saved()
        );
    }

    #[test]
    fn a_store_with_nothing_stranded_is_left_alone() {
        let path = a_store("tight", 3);
        let into = a_path("tight-out");
        let mut out = Vec::new();
        let done = reclaim(&path, &into, WHEN, None, &mut out).expect("reclaims");
        let report = String::from_utf8(out).expect("the report is text");
        assert_eq!(done, Outcome::Nothing, "{report}");
        assert!(done.settled(), "{report}");
        assert!(report.contains("give"), "{report}");
        assert!(!into.exists(), "a file was written anyway");
    }

    #[test]
    fn deletions_come_across_and_stay_deleted() {
        let path = a_store("deleted", 4);
        {
            let mut store = Store::open(&path).expect("a store");
            store.compact(0..4, WHEN, 1).expect("a fold");
            let mut gone = Bitmap::new();
            for ordinal in [1, 4, 7] {
                gone.insert(ordinal);
            }
            store.delete(0, &gone, WHEN).expect("deleted");
        }
        let before = {
            let store = Store::open(&path).expect("a store");
            let manifest = store.manifest();
            (manifest.live, manifest.total)
        };
        let into = a_path("deleted-out");

        let mut out = Vec::new();
        let done = reclaim(&path, &into, WHEN, None, &mut out).expect("reclaims");
        let report = String::from_utf8(out).expect("the report is text");
        assert!(matches!(done, Outcome::Wrote(_)), "{report}");

        let store = Store::open(&into).expect("the store that came out");
        assert_eq!((store.manifest().live, store.manifest().total), before);
        let view = store.view().expect("a view");
        assert_eq!(
            view.deleted(0).expect("the deletions").map(|set| set.len()),
            Some(3)
        );
    }

    #[test]
    fn a_store_with_records_in_its_log_is_refused() {
        let path = a_store("logged", 2);
        {
            let mut store = Store::open(&path).expect("a store");
            store
                .append(1, b"a record nothing has consumed")
                .expect("appended");
            store.sync().expect("synced");
            let manifest = store.manifest().clone();
            store.commit(manifest, WHEN).expect("committed");
        }
        let into = a_path("logged-out");

        let mut out = Vec::new();
        let done = reclaim(&path, &into, WHEN, None, &mut out).expect("reclaims");
        let report = String::from_utf8(out).expect("the report is text");
        assert_eq!(done, Outcome::Refused, "{report}");
        assert!(!done.settled(), "{report}");
        assert!(report.contains("no commit has consumed"), "{report}");
        assert!(!into.exists(), "a file was written anyway");
    }

    #[test]
    fn it_will_not_write_over_a_file_that_is_there() {
        let path = a_store("clobber", 2);
        let into = a_store("clobber-target", 1);
        let mut out = Vec::new();
        let done = reclaim(&path, &into, WHEN, None, &mut out).expect("reclaims");
        let report = String::from_utf8(out).expect("the report is text");
        assert_eq!(done, Outcome::Refused, "{report}");
        assert!(report.contains("never writes over a file"), "{report}");
    }

    /// How long the log of the store at `path` is.
    fn log_of(path: &std::path::Path) -> u64 {
        Store::open(path).expect("a store").superblock().wal_len
    }

    #[test]
    fn a_log_nobody_asked_about_is_the_one_the_store_came_with() {
        let path = a_store("log-kept", 6);
        {
            let mut store = Store::open(&path).expect("a store");
            store.compact(0..6, WHEN, 1).expect("a fold");
        }
        let into = a_path("log-kept-out");
        let mut out = Vec::new();
        let done = reclaim(&path, &into, WHEN, None, &mut out).expect("reclaims");
        let report = String::from_utf8(out).expect("the report is text");
        assert!(matches!(done, Outcome::Wrote(_)), "{report}");
        assert_eq!(log_of(&into), 1 << 20);
        // A run that changed nothing about the log should not be reporting on it.
        assert!(!report.contains("log becomes"), "{report}");
    }

    #[test]
    fn a_shorter_log_is_reason_enough_to_write_when_nothing_is_stranded() {
        // The whole of the case this exists for. A store with three segments and
        // no fold behind it has nothing stranded, so the run before --log would
        // have said so and stopped, and the file would have stayed nine parts
        // log.
        let path = a_store("log-only", 3);
        let into = a_path("log-only-out");
        let was = length(&path);

        let mut out = Vec::new();
        let done = reclaim(&path, &into, WHEN, Some(64 << 10), &mut out).expect("reclaims");
        let report = String::from_utf8(out).expect("the report is text");
        assert!(matches!(done, Outcome::Wrote(_)), "{report}");
        assert!(report.contains("log becomes"), "{report}");
        assert!(report.contains("giving back about"), "{report}");
        assert_eq!(log_of(&into), 64 << 10);
        assert!(length(&into) < was, "{was} became {}", length(&into));

        // And it is still a store holding what the one it came from held.
        let store = Store::open(&into).expect("the store that came out");
        let view = store.view().expect("a view");
        for round in 0..3 {
            for id in 0..5 {
                let key = format!("segment-{round}-{id}");
                assert!(
                    view.document(key.as_bytes()).expect("a lookup").is_some(),
                    "{key} went missing"
                );
            }
        }
    }

    #[test]
    fn a_longer_log_says_the_file_will_come_out_larger_and_it_does() {
        let path = a_store("log-grown", 2);
        let into = a_path("log-grown-out");
        let was = length(&path);

        let mut out = Vec::new();
        let done = reclaim(&path, &into, WHEN, Some(8 << 20), &mut out).expect("reclaims");
        let report = String::from_utf8(out).expect("the report is text");
        assert!(matches!(done, Outcome::Wrote(_)), "{report}");
        assert!(report.contains("comes out about"), "{report}");
        assert_eq!(log_of(&into), 8 << 20);
        assert!(length(&into) > was, "{was} became {}", length(&into));
    }

    #[test]
    fn it_will_not_write_the_file_it_is_reading() {
        let path = a_store("itself", 2);
        let mut out = Vec::new();
        let done = reclaim(&path, &path, WHEN, None, &mut out).expect("reclaims");
        let report = String::from_utf8(out).expect("the report is text");
        assert_eq!(done, Outcome::Refused, "{report}");
        assert!(report.contains("the file to read"), "{report}");
    }
}
