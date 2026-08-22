//! Folding the segments of a store into one.
//!
//! A store gains a segment every time something is committed into it and never
//! loses one. Index a directory eight times and the file holds sixteen segments,
//! one live copy of each document and fifteen sixteenths of it dead weight, and
//! every lookup by key asks all sixteen. This is the command that turns that
//! back into one segment holding what is live.
//!
//! # What it costs and what it does not
//!
//! It is a rewrite. Every posting list is decoded and encoded again against the
//! new numbering, so the work is proportional to what survives rather than to
//! what is being dropped, and on the corpora this has been run on it goes at
//! about thirty thousand documents a second.
//!
//! It does not make the file smaller. The segments it replaced stay exactly
//! where they are, because a query that started before the commit is still
//! reading them, and the space they hold comes back when the file is rewritten
//! or, if they were at the end of it, the next time the store is opened. What
//! the fold gives back immediately is the segment count, which is what a lookup
//! and a search pay per question.
//!
//! # Why there is no dry run
//!
//! `repair` writes nothing without `--commit` because what it does is throw
//! documents away. This throws nothing away. The documents it leaves behind were
//! deleted, which means they had already stopped answering queries, and
//! everything else comes out the other side under the same key with the same
//! stored fields. The old manifest is in the other slot until the next commit
//! either way.

use std::io::Write;
use std::path::Path;
use std::time::Instant;

use kura_core::file::{Compacted, Result, Store};
use kura_core::manifest;
use kura_core::mapping::Map;
use kura_core::policy::Policy;

/// Folds every segment of a store but the newest `keep` into one.
///
/// `now` is written into the segment and the manifest as the time of the
/// commit, and is passed in rather than read here so that the caller holds the
/// one clock.
///
/// Keeping the newest few is what makes this usable on a store something else is
/// writing to: the segments at the end are the ones a run is about to add to,
/// and folding them in costs the same work again the next time round.
///
/// # Errors
///
/// Returns [`kura_core::file::Trouble`] if the file cannot be read, if a segment
/// holds a section this build cannot carry across, if the report cannot be
/// written, or if the write or the commit fails. Nothing is committed unless all
/// of it is.
pub fn fold(path: &Path, keep: usize, now: u64, out: &mut impl Write) -> Result<Option<Compacted>> {
    let Some(mut store) = opened(path, out)? else {
        return Ok(None);
    };
    let before = store.manifest().segments.len();
    let held = before.saturating_sub(keep);
    said(out, "segments", before as u64)?;
    said(out, "  folding", held as u64)?;
    said(out, "documents", store.manifest().total)?;
    said(out, "  live", store.manifest().live)?;
    said(out, "file bytes", length(path))?;

    if held < 2 {
        writeln!(out)?;
        writeln!(
            out,
            "  a fold of one segment is a copy of it, so there is nothing to do"
        )?;
        return Ok(None);
    }

    perform(&mut store, 0..held, path, now, out).map(Some)
}

/// Folds whichever run the policy says is due, or says that none is.
///
/// The difference from [`fold`] is who chooses. That one folds what it is told
/// to fold, which is what somebody at a terminal wants, and this one asks
/// [`Policy`] and folds what comes back, which is what a store being kept in
/// shape wants. The rule is in the engine rather than here, so this and anything
/// else that keeps a store in shape are folding by the same rule.
///
/// One job per call, and deliberately so. A store that is far behind needs
/// several folds and each of them changes what the next decision should be, so
/// the caller runs this again rather than this looping, and a caller that has to
/// come back is a caller that can stop.
///
/// # Errors
///
/// As [`fold`].
pub fn due(path: &Path, now: u64, out: &mut impl Write) -> Result<Option<Compacted>> {
    let Some(mut store) = opened(path, out)? else {
        return Ok(None);
    };
    let policy = Policy::default();
    said(out, "segments", store.manifest().segments.len() as u64)?;
    said(out, "documents", store.manifest().total)?;
    said(out, "  live", store.manifest().live)?;
    said(out, "file bytes", length(path))?;
    writeln!(out)?;
    levels(&store, policy, out)?;

    let Some(job) = policy.choose(&store.manifest().segments) else {
        writeln!(out)?;
        writeln!(out, "  nothing is due, so nothing was folded")?;
        return Ok(None);
    };
    writeln!(out)?;
    writeln!(
        out,
        "  folding {} segments at level {}, {} bytes, because {}",
        job.run.len(),
        job.level,
        job.bytes,
        job.reason.why()
    )?;
    perform(&mut store, job.run, path, now, out).map(Some)
}

/// Opens a store, or says why the file in front of it is not one.
fn opened(path: &Path, out: &mut impl Write) -> Result<Option<Store>> {
    writeln!(out, "{}", path.display())?;
    writeln!(out)?;

    {
        let bytes = Map::open(path)?;
        if !manifest::looks_like_a_store(&bytes) {
            writeln!(
                out,
                "  this is a single segment and not a store, so there is nothing to fold"
            )?;
            return Ok(None);
        }
    }
    Ok(Some(Store::open(path)?))
}

/// A line per level, what it holds and what it is allowed to hold.
///
/// It is printed whether or not anything is due, because a report that only
/// spoke when it was folding would leave somebody asking why it was not folding
/// with nothing to read.
fn levels(store: &Store, policy: Policy, out: &mut impl Write) -> std::io::Result<()> {
    let segments = &store.manifest().segments;
    let deepest = segments.iter().map(|segment| segment.level).max();
    let Some(deepest) = deepest else {
        writeln!(out, "  no segments, so no levels")?;
        return Ok(());
    };
    for level in 0..=deepest {
        let at: Vec<_> = segments
            .iter()
            .filter(|segment| segment.level == level)
            .collect();
        if at.is_empty() {
            continue;
        }
        let bytes: u64 = at
            .iter()
            .map(|segment| {
                segment
                    .len
                    .saturating_add(u64::from(segment.tombstones_len))
            })
            .sum();
        let counted = if at.len() == 1 { "segment" } else { "segments" };
        if level == 0 {
            writeln!(
                out,
                "  level {level:<4} {:>4} {counted}, {} allowed, {bytes:>14} bytes",
                at.len(),
                policy.level_zero_cap
            )?;
        } else {
            writeln!(
                out,
                "  level {level:<4} {:>4} {counted}, {bytes:>14} bytes of {} allowed",
                at.len(),
                policy.capacity(level)
            )?;
        }
    }
    Ok(())
}

/// Folds a run and reports what it cost, which is the half both callers share.
fn perform(
    store: &mut Store,
    run: core::ops::Range<usize>,
    path: &Path,
    now: u64,
    out: &mut impl Write,
) -> Result<Compacted> {
    let start = Instant::now();
    let done = store.compact(run, now, now)?;
    let took = start.elapsed();
    writeln!(out)?;
    writeln!(out, "  {:<20} {:>12.2} s", "fold wall", took.as_secs_f32())?;
    said(out, "merged documents", u64::from(done.documents))?;
    said(out, "  left behind", done.dropped)?;
    said(out, "merged terms", u64::from(done.terms))?;
    said(out, "merged bytes", done.bytes)?;
    said(out, "stranded bytes", done.stranded)?;
    writeln!(out)?;
    said(out, "segments", store.manifest().segments.len() as u64)?;
    said(out, "documents", store.manifest().total)?;
    said(out, "  live", store.manifest().live)?;
    said(out, "file bytes", length(path))?;
    said(out, "epoch", done.epoch)?;
    writeln!(out)?;
    writeln!(
        out,
        "  the segments this replaced are still in the file, and the space they hold"
    )?;
    writeln!(
        out,
        "  comes back when it is rewritten rather than now, because a query that"
    )?;
    writeln!(out, "  started before the commit is still reading them")?;

    Ok(done)
}

/// One line of the report, a name and a number.
fn said(out: &mut impl Write, what: &str, number: u64) -> std::io::Result<()> {
    writeln!(out, "  {what:<20} {number:>12}")
}

/// How long the file is, or zero if that cannot be asked.
///
/// A size that cannot be read is a line of a report rather than a reason to stop
/// halfway through a compaction.
fn length(path: &Path) -> u64 {
    std::fs::metadata(path).map_or(0, |about| about.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use kura_core::index::Writer;

    /// A fixed time, so that nothing here depends on the clock.
    const WHEN: u64 = 1_700_000_000;

    /// A path of this test's own, under a directory this process shares.
    fn a_path(name: &str) -> std::path::PathBuf {
        let directory = std::env::temp_dir().join(format!("kura-fold-{}", std::process::id()));
        std::fs::create_dir_all(&directory).expect("a temporary directory");
        let path = directory.join(format!("{name}.kura"));
        std::fs::remove_file(&path).ok();
        path
    }

    /// A store holding `count` segments of five documents each, every document
    /// keyed by its own name, committed one segment at a time.
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
    fn a_store_of_several_segments_comes_out_as_one() {
        let path = a_store("several", 4);
        let mut out = Vec::new();
        let done = fold(&path, 0, WHEN, &mut out)
            .expect("folds")
            .expect("a fold");
        assert_eq!(done.folded, 4);
        assert_eq!(done.documents, 20);
        assert_eq!(done.dropped, 0);

        let store = Store::open(&path).expect("a store");
        assert_eq!(store.manifest().segments.len(), 1);
        assert_eq!(store.manifest().live, 20);
        // Every key is still a key, which is the thing a fold is trusted with.
        let view = store.view().expect("a view");
        for round in 0..4 {
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
    fn the_newest_segments_are_left_alone_when_they_are_asked_to_be() {
        let path = a_store("keep", 5);
        let mut out = Vec::new();
        let done = fold(&path, 2, WHEN, &mut out)
            .expect("folds")
            .expect("a fold");
        assert_eq!(done.folded, 3);
        assert_eq!(done.documents, 15);

        let store = Store::open(&path).expect("a store");
        assert_eq!(store.manifest().live, 25);
        assert_eq!(store.manifest().segments[0].docs, 15);
    }

    #[test]
    fn a_store_that_is_already_one_segment_is_left_alone() {
        let path = a_store("single", 1);
        let mut out = Vec::new();
        assert!(
            fold(&path, 0, WHEN, &mut out)
                .expect("nothing to do")
                .is_none()
        );
        let report = String::from_utf8(out).expect("the report is text");
        assert!(report.contains("nothing to do"), "{report}");

        let store = Store::open(&path).expect("a store");
        assert_eq!(store.manifest().epoch, 2);
    }

    #[test]
    fn keeping_more_segments_than_there_are_folds_nothing() {
        let path = a_store("keepall", 3);
        let mut out = Vec::new();
        assert!(
            fold(&path, 9, WHEN, &mut out)
                .expect("nothing to do")
                .is_none()
        );
        let store = Store::open(&path).expect("a store");
        assert_eq!(store.manifest().segments.len(), 3);
    }

    #[test]
    fn a_store_under_the_cap_has_nothing_due() {
        let path = a_store("nothingdue", 7);
        let mut out = Vec::new();
        assert!(due(&path, WHEN, &mut out).expect("asks").is_none());
        let report = String::from_utf8(out).expect("the report is text");
        assert!(report.contains("nothing is due"), "{report}");
        // A report that says nothing is due still says what it looked at.
        assert!(report.contains("level 0"), "{report}");

        let store = Store::open(&path).expect("a store");
        assert_eq!(store.manifest().segments.len(), 7);
    }

    #[test]
    fn a_store_at_the_cap_folds_level_zero_and_says_so() {
        let path = a_store("leveldue", 8);
        let mut out = Vec::new();
        let done = due(&path, WHEN, &mut out).expect("folds").expect("a fold");
        assert_eq!(done.folded, 8);
        assert_eq!(done.documents, 40);
        let report = String::from_utf8(out).expect("the report is text");
        assert!(report.contains("level zero is full"), "{report}");

        let store = Store::open(&path).expect("a store");
        assert_eq!(store.manifest().segments.len(), 1);
        assert_eq!(store.manifest().segments[0].level, 1);
        assert_eq!(store.manifest().live, 40);
        // And the one that came out of it is not due again.
        let mut out = Vec::new();
        assert!(due(&path, WHEN, &mut out).expect("asks").is_none());
    }

    #[test]
    fn a_single_segment_file_is_said_to_be_one_rather_than_opened_as_a_store() {
        let path = a_path("bare");
        let mut writer = Writer::new();
        writer.add("a segment on its own").expect("a document");
        std::fs::write(&path, writer.finish().expect("a segment")).expect("written");

        let mut out = Vec::new();
        assert!(fold(&path, 0, WHEN, &mut out).expect("says so").is_none());
        let report = String::from_utf8(out).expect("the report is text");
        assert!(report.contains("not a store"), "{report}");
    }
}
