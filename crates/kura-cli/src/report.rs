//! Turning counters into something a person can read at a glance.
//!
//! The layout is chosen so the answer to "did the pruning fire" is one line and
//! not a subtraction the reader has to do. Every count that has a denominator is
//! printed against it, because a number like four hundred thousand means nothing
//! until you know whether the list held four hundred thousand or four million.

use std::io::{self, Write};
use std::time::Duration;

use kura_core::explain::Counters;
use kura_core::index::Reader;
use kura_core::posting::BLOCK_SIZE;
use kura_core::residency::Residency;

use crate::analyse;

/// Prints the query's terms and what each of them costs to open.
///
/// This is the plan. There is no operator tree to show yet: every query is the
/// same shape, a block-max WAND over one list per term, so what varies between a
/// fast query and a slow one is entirely in this table.
///
/// A term gets one row however many segments there are, with its documents and
/// its blocks summed over the lot, because that is the work the query does and
/// because a table that grew a column per segment would be unreadable by the
/// tenth one. What the row cannot show, and what the line under it says instead,
/// is that the weight each term is scored with is worked out over every segment
/// together rather than segment by segment. That is the reason the same document
/// gets the same score wherever it happens to sit, and it is worth being able to
/// see, because it is the thing that would break first if the merge were wrong.
///
/// # Errors
///
/// Returns an error if the output cannot be written, which is a closed pipe.
pub fn plan(query: &str, segments: &[Reader<'_>], out: &mut impl Write) -> io::Result<()> {
    let words = analyse(query);
    writeln!(out, "query    {query}")?;

    let mut found = 0;
    let mut rows = Vec::with_capacity(words.len());
    for word in &words {
        let name = String::from_utf8_lossy(word).into_owned();
        let mut term = Term::Absent;
        for index in segments {
            match index.postings(word) {
                Ok(Some(list)) => {
                    let full = list.blocks();
                    let leftovers =
                        usize::try_from(list.len()).unwrap_or(usize::MAX) > full * BLOCK_SIZE;
                    term = term.and(list.len(), full + usize::from(leftovers));
                }
                // A term that is not in this segment says nothing about the
                // others, so it leaves the row where it is. A term absent from
                // every one of them ends up absent, which is the single most
                // useful thing this report can say, because it explains a thin
                // result page in one line rather than in an afternoon.
                Ok(None) => {}
                // Unreachable in practice, because the search opened the same
                // lists a moment ago and would have failed then. Saying so beats
                // saying the term was absent, which is a different problem with
                // a different fix.
                Err(_) => term = Term::Unreadable,
            }
        }
        if matches!(term, Term::Open(..)) {
            found += 1;
        }
        rows.push((name, term));
    }

    writeln!(out, "terms    {found} of {} in the index", words.len())?;
    writeln!(
        out,
        "segments {} searched together, weighted over all of them",
        segments.len()
    )?;
    writeln!(out)?;
    writeln!(out, "  {:<24} {:>12} {:>10}", "term", "documents", "blocks")?;
    for (name, term) in &rows {
        match term {
            Term::Open(documents, blocks) => {
                writeln!(out, "  {name:<24} {documents:>12} {blocks:>10}")?;
            }
            Term::Absent => writeln!(out, "  {name:<24} {:>12}", "absent")?,
            Term::Unreadable => writeln!(out, "  {name:<24} {:>12}", "unreadable")?,
        }
    }
    writeln!(out)?;
    Ok(())
}

/// What opening one of the query's terms found, across every segment.
enum Term {
    /// The list is there, with this many documents in this many blocks.
    Open(u64, usize),
    /// The term is not in any of the segments.
    Absent,
    /// The term is in a dictionary somewhere but its list did not decode.
    Unreadable,
}

impl Term {
    /// The row with one more segment's list added to it.
    ///
    /// A list that did not decode stays the answer whatever the segments after
    /// it hold, because a number summed over some of the segments and not the
    /// rest is worse than no number: it looks like an answer.
    fn and(self, documents: u32, blocks: usize) -> Self {
        match self {
            Self::Open(had, blocks_had) => {
                Self::Open(had + u64::from(documents), blocks_had + blocks)
            }
            Self::Absent => Self::Open(u64::from(documents), blocks),
            Self::Unreadable => Self::Unreadable,
        }
    }
}

/// Which walk answered the query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Walk {
    /// The best `k` documents, and nothing else.
    Page,
    /// The best `k` documents and how many there are in all, in one pass.
    PageAndTotal,
}

/// Prints what the walk did.
///
/// # Errors
///
/// Returns an error if the output cannot be written, which is a closed pipe.
pub fn counters(
    counters: &Counters,
    took: Duration,
    walk: Walk,
    out: &mut impl Write,
) -> io::Result<()> {
    match walk {
        Walk::Page => writeln!(out, "walk     page, block-max WAND")?,
        Walk::PageAndTotal => {
            writeln!(out, "walk     page and total, one pass")?;
            // Without this line the zeroes below read as a broken optimiser
            // rather than as the price of the total, and that is a whole
            // afternoon of looking in the wrong place.
            writeln!(
                out,
                "         the total needs every match visited, so the skip counts here are"
            )?;
            writeln!(
                out,
                "         the price of asking for it and not a measure of the pruning"
            )?;
        }
    }
    writeln!(out)?;
    line(
        out,
        "postings decoded",
        counters.postings_decoded,
        Some(counters.postings),
    )?;
    line(
        out,
        "blocks decoded",
        counters.blocks_decoded,
        Some(counters.blocks),
    )?;
    line(
        out,
        "blocks skipped",
        counters.blocks_skipped,
        Some(counters.blocks),
    )?;
    line(out, "documents scored", counters.documents_scored, None)?;
    line(out, "cursor seeks", counters.seeks, None)?;
    line(out, "cursor advances", counters.advances, None)?;
    writeln!(out)?;
    if let Some(residency) = counters.residency {
        memory(&residency, out)?;
    }
    writeln!(
        out,
        "took     {took:.3?}, and {:.1}% of the postings were never read",
        f64::from(counters.skipped()) * 100.0
    )?;
    writeln!(out)?;
    Ok(())
}

/// Prints what the query cost in memory.
///
/// Separate from the walk counters above and printed under its own heading,
/// because it is measured differently. Those are counted by the walk as it goes
/// and are exact. These are read from the operating system before and after, and
/// what they include depends on the platform and on what else the process was
/// doing. Running them together in one table would make them look equally
/// trustworthy, and they are not.
fn memory(residency: &Residency, out: &mut impl Write) -> io::Result<()> {
    writeln!(out, "  memory")?;
    match residency.resident_before {
        Some(resident) => {
            let warm = residency.warm().unwrap_or(0.0);
            writeln!(
                out,
                "  {:<24} {:>12}  of {:<12} {:>5.1}%",
                "index resident before",
                bytes(resident),
                bytes(residency.total),
                f64::from(warm) * 100.0
            )?;
        }
        None => writeln!(out, "  {:<24} {:>12}", "index resident before", "unknown")?,
    }
    fault_line(out, "page faults", residency.faults, residency.faulted())?;
    fault_line(
        out,
        "of those, from disk",
        residency.faults_from_disk,
        residency.faulted_from_disk(),
    )?;
    if let Some(note) = residency.note {
        writeln!(out, "         {note}")?;
    }
    // Nobody should read these as belonging to the query alone without being
    // told where they came from, and the place to tell them is here.
    writeln!(
        out,
        "         faults are counted by the operating system around the query rather"
    )?;
    writeln!(
        out,
        "         than inside it, so anything else this process did lands here too"
    )?;
    writeln!(out)?;
    Ok(())
}

/// One fault count, and the size it works out to.
///
/// Both, because the count is exact and the size is a floor. A fault can hand
/// over more than one page where the kernel uses huge pages, and printing only
/// the size would quietly turn that into a wrong answer instead of a small one.
fn fault_line(
    out: &mut impl Write,
    name: &str,
    count: Option<u64>,
    size: Option<u64>,
) -> io::Result<()> {
    match (count, size) {
        (Some(count), Some(size)) => {
            writeln!(out, "  {name:<24} {count:>12}  {:>12}", bytes(size))
        }
        _ => writeln!(out, "  {name:<24} {:>12}", "unknown"),
    }
}

/// One counter, against its denominator when it has one.
fn line(out: &mut impl Write, name: &str, value: u64, of: Option<u64>) -> io::Result<()> {
    match of {
        Some(total) if total > 0 => {
            writeln!(
                out,
                "  {name:<24} {value:>12}  of {total:<12} {:>5.1}%",
                percent(value, total)
            )
        }
        _ => writeln!(out, "  {name:<24} {value:>12}"),
    }
}

#[expect(
    clippy::cast_precision_loss,
    reason = "a percentage of two corpus sized counts, printed to one decimal place"
)]
fn percent(value: u64, total: u64) -> f64 {
    value as f64 / total as f64 * 100.0
}

/// A byte count in the largest unit that keeps it above one.
#[expect(
    clippy::cast_precision_loss,
    reason = "a size printed to one decimal place, where the last bits do not show"
)]
#[must_use]
pub fn bytes(count: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = count as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        return format!("{count} B");
    }
    format!("{size:.1} {}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_size_is_printed_in_the_unit_that_keeps_it_readable() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(1023), "1023 B");
        assert_eq!(bytes(1024), "1.0 KB");
        assert_eq!(bytes(1_048_576), "1.0 MB");
        assert_eq!(bytes(898 * 1_048_576), "898.0 MB");
    }

    #[test]
    fn a_counter_with_no_denominator_does_not_invent_one() {
        let mut out = Vec::new();
        line(&mut out, "documents scored", 12, None).expect("writes");
        let text = String::from_utf8(out).expect("ascii");
        assert!(text.contains("12"));
        assert!(!text.contains('%'));
    }

    #[test]
    fn a_denominator_of_zero_is_not_divided_by() {
        let mut out = Vec::new();
        line(&mut out, "blocks decoded", 0, Some(0)).expect("writes");
        let text = String::from_utf8(out).expect("ascii");
        assert!(!text.contains("NaN"), "{text}");
        assert!(!text.contains('%'), "{text}");
    }

    #[test]
    fn the_report_says_how_much_was_skipped_without_the_reader_subtracting() {
        let counters = Counters {
            terms: 1,
            postings: 1_000,
            blocks: 8,
            blocks_decoded: 2,
            blocks_skipped: 6,
            postings_decoded: 256,
            documents_scored: 256,
            seeks: 6,
            advances: 256,
            residency: None,
        };
        let mut out = Vec::new();
        super::counters(
            &counters,
            Duration::from_micros(1_500),
            Walk::Page,
            &mut out,
        )
        .expect("writes");
        let text = String::from_utf8(out).expect("ascii");
        assert!(text.contains("blocks skipped"), "{text}");
        assert!(
            text.contains("74.4% of the postings were never read"),
            "{text}"
        );
    }

    #[test]
    fn a_memory_reading_is_printed_against_the_size_of_the_index() {
        let residency = Residency {
            faults: Some(512),
            faults_from_disk: Some(256),
            resident_before: Some(8 * 1_048_576),
            total: 32 * 1_048_576,
            page: 4096,
            note: None,
        };
        let mut out = Vec::new();
        memory(&residency, &mut out).expect("writes");
        let text = String::from_utf8(out).expect("ascii");

        assert!(text.contains("8.0 MB"), "{text}");
        assert!(text.contains("32.0 MB"), "{text}");
        assert!(text.contains("25.0%"), "{text}");
        // The count and the size it works out to, for both kinds of fault.
        assert!(text.contains("512"), "{text}");
        assert!(text.contains("2.0 MB"), "{text}");
        assert!(text.contains("256"), "{text}");
        assert!(text.contains("1.0 MB"), "{text}");
        assert!(!text.contains("unknown"), "{text}");
    }

    #[test]
    fn a_platform_that_cannot_answer_is_printed_as_unknown_and_not_as_zero() {
        // The whole reason these are options. A zero here would read as a cold
        // index that faulted nothing, which is the opposite of not knowing.
        let residency = Residency {
            faults: None,
            faults_from_disk: None,
            resident_before: None,
            total: 32 * 1_048_576,
            page: 4096,
            note: Some("this platform does not account for page faults"),
        };
        let mut out = Vec::new();
        memory(&residency, &mut out).expect("writes");
        let text = String::from_utf8(out).expect("ascii");

        assert_eq!(text.matches("unknown").count(), 3, "{text}");
        assert!(text.contains("does not account for page faults"), "{text}");
        assert!(!text.contains(" 0 B"), "{text}");
    }

    #[test]
    fn the_walk_that_counts_says_why_it_skipped_nothing() {
        // A reader who sees zeroes without the reason spends the afternoon
        // looking for a bug in the pruning that is not there.
        let counters = Counters {
            terms: 2,
            postings: 10_710,
            blocks: 86,
            blocks_decoded: 86,
            blocks_skipped: 0,
            postings_decoded: 10_710,
            documents_scored: 247,
            seeks: 41,
            advances: 10_567,
            residency: None,
        };
        let mut out = Vec::new();
        super::counters(
            &counters,
            Duration::from_micros(137),
            Walk::PageAndTotal,
            &mut out,
        )
        .expect("writes");
        let text = String::from_utf8(out).expect("ascii");
        assert!(
            text.contains("the total needs every match visited"),
            "{text}"
        );
    }
}
