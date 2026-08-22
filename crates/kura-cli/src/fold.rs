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

    let mut store = Store::open(path)?;
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

    let start = Instant::now();
    let done = store.compact(0..held, now, now)?;
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

    Ok(Some(done))
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
