//! Reading an index all the way through to find out whether it is intact.
//!
//! A query touches a fraction of an index. It walks the block index of one term
//! and decodes the blocks that survive pruning, and everything it never looked
//! at could be nonsense without the query noticing. That is the right trade for
//! a query and the wrong one for a person holding a file they are not sure
//! about, which is what this is for.
//!
//! # What it checks, in the order it checks it
//!
//! The header, then the section table, then the checksum of the body, then the
//! contents of every section an index needs, then every posting list, then every
//! stored document. The order matters because each stage depends on the one
//! before it: there is no point decoding a posting list out of a section table
//! that points outside the file.
//!
//! # Why it keeps going after the first failure
//!
//! Anything that stops at the first bad byte answers only whether a file is
//! damaged. What a person wants to know next is how much of it is damaged, and
//! whether the damage is in one term nobody searches for or spread across the
//! whole postings section, because those are a rebuild of one segment and a
//! restore from backup respectively. So a failed stage is reported and the walk
//! carries on into the stages that are still possible.
//!
//! Not every failure leaves something possible. A file that is not a kura file
//! has no section table to print and a section table that does not decode has no
//! sections to read, so those two stop. A checksum that does not match does not
//! stop, and neither does a posting list that does not decode, and those are the
//! two cases where carrying on earns the most.
//!
//! # What it cannot tell you
//!
//! Which block of a posting list is damaged. The format checksums the body as a
//! whole and nothing smaller, so a mismatch says a byte somewhere in the file is
//! wrong and cannot say where. Decoding narrows that down to the term whose list
//! stops decoding, which is as far as this goes today, and a byte flipped inside
//! a block of document identifiers usually decodes to different identifiers
//! rather than to an error at all. Catching that needs a checksum per section
//! and then per block, which the format does not have yet.
//!
//! It also cannot tell you that an index is the index you meant. Every check
//! here is internal consistency, so an index built from the wrong directory
//! passes all of them.

use std::fmt;
use std::io::{self, Write};
use std::path::Path;

use kura_core::index;
use kura_core::mapping::Map;
use kura_core::segment::{self, Segment};
use kura_core::store::Scratch;
use kura_core::{DocId, Error};

use crate::report::bytes;

/// What a run of the checks found.
#[derive(Debug, Default, Clone, Copy)]
pub struct Outcome {
    /// How many checks failed.
    pub failures: usize,
    /// How many stages never ran because an earlier one made them impossible.
    pub skipped: usize,
}

impl Outcome {
    /// Whether everything that ran passed and nothing was skipped.
    const fn clean(&self) -> bool {
        self.failures == 0 && self.skipped == 0
    }
}

/// Reads an index all the way through and prints what it found.
///
/// # Errors
///
/// Returns an error only when the file itself cannot be read or the report
/// cannot be written. A damaged index is a result and not an error, and comes
/// back as an [`Outcome`] with a non-zero count.
pub fn check(path: &Path, out: &mut impl Write) -> io::Result<Outcome> {
    let bytes = Map::open(path)?;

    writeln!(out, "{}", path.display())?;
    writeln!(out, "  size {:>26}", self::bytes(as_u64(bytes.len())))?;
    writeln!(out)?;

    let mut outcome = Outcome::default();

    // Structure first, and fatally. Everything below reads through the section
    // table, so a table that does not decode leaves nothing else to try.
    let segment = match Segment::open_without_checksum(&bytes) {
        Ok(segment) => segment,
        Err(error) => {
            failed(out, "structure", &error)?;
            writeln!(out)?;
            writeln!(out, "  nothing else can be read out of this file")?;
            return Ok(Outcome {
                failures: 1,
                skipped: 0,
            });
        }
    };
    passed(out, "structure")?;

    table(&segment, as_u64(bytes.len()), out)?;

    // The checksum is not fatal, which is the whole reason the two open paths
    // are separate. A body that does not match its checksum still decodes often
    // enough to say which term the damage landed in, and that is worth more than
    // refusing to look.
    match Segment::open(&bytes) {
        Ok(_) => passed(out, "checksum")?,
        Err(error) => {
            failed(out, "checksum", &error)?;
            outcome.failures += 1;
        }
    }

    match index::Reader::open(&segment) {
        Ok(reader) => {
            passed(out, "sections")?;
            contents(&reader, out)?;
            outcome.failures += postings(&reader, out)?;
            outcome.failures += documents(&reader, out)?;
        }
        Err(error) => {
            failed(out, "sections", &error)?;
            outcome.failures += 1;
            // The postings and the documents live inside sections this could not
            // open, so they were never looked at. Saying so is the difference
            // between a clean walk and a walk that did not happen.
            outcome.skipped += 2;
        }
    }

    writeln!(out)?;
    if outcome.clean() {
        writeln!(out, "  everything checked passed")?;
    } else {
        writeln!(
            out,
            "  {} failed, {} not checked",
            outcome.failures, outcome.skipped
        )?;
    }
    Ok(outcome)
}

/// Prints the section table, which is the map of everything below it.
fn table(segment: &Segment<'_>, total: u64, out: &mut impl Write) -> io::Result<()> {
    writeln!(out)?;
    writeln!(out, "  sections")?;
    for section in segment.sections() {
        // A kind this build has never heard of is not a failure. A reader skips
        // an unknown section and carries on, which is what makes it possible to
        // add one without breaking every older binary, so the honest thing to
        // print is the number rather than a complaint.
        let name = segment::name(section.kind)
            .map_or_else(|| format!("kind {}", section.kind), ToString::to_string);
        writeln!(
            out,
            "    {name:<12} {:>12}  {:>6}  at {}",
            bytes(section.length),
            share(section.length, total),
            section.offset,
        )?;
    }
    writeln!(out)
}

/// Prints what the index says about itself, which is cheap and often enough.
fn contents(reader: &index::Reader<'_>, out: &mut impl Write) -> io::Result<()> {
    writeln!(out)?;
    writeln!(out, "  documents    {:>12}", reader.documents())?;
    writeln!(out, "  terms        {:>12}", reader.terms())?;
    writeln!(out, "  mean length  {:>12.1}", reader.average_length())?;
    writeln!(out)
}

/// Decodes every posting list, in dictionary order.
///
/// This is the expensive check and the one worth having. It is the only thing
/// here that reads the postings section, which on a real index is most of the
/// file, and the only thing that can name what is broken rather than saying that
/// something is.
fn postings(reader: &index::Reader<'_>, out: &mut impl Write) -> io::Result<usize> {
    /// How many failing terms are named before the report starts counting.
    ///
    /// Damage to the postings section usually takes out a run of lists rather
    /// than one, and a report with four thousand near identical lines in it is a
    /// report nobody reads to the end. The first few say where the damage
    /// starts, which is the part worth acting on.
    const NAMED: usize = 5;

    let mut entries = reader.entries();
    let mut lists = 0u64;
    let mut decoded = 0u64;
    let mut failures = 0usize;
    let mut broken_dictionary = false;

    loop {
        // The dictionary and the lists it points at fail differently. A term
        // that does not decode ends the walk, because the next term is stored as
        // a suffix onto this one and there is no way to carry on past it. A list
        // that does not decode is one term, and the walk moves to the next.
        let (term, entry) = match entries.next_term() {
            Ok(Some(found)) => found,
            Ok(None) => break,
            Err(error) => {
                failed(out, "dictionary", &error)?;
                writeln!(out, "      after {lists} terms")?;
                broken_dictionary = true;
                break;
            }
        };

        lists += 1;
        match walk(reader, entry) {
            Ok(count) => decoded += count,
            Err(error) => {
                if failures == 0 {
                    failed(out, "postings", &error)?;
                }
                if failures < NAMED {
                    // Terms come from a corpus, and a corpus is somebody's
                    // private data. The length goes in the report and the bytes
                    // do not.
                    writeln!(out, "      at term {lists} of {} bytes", term.len())?;
                }
                failures += 1;
            }
        }
    }

    if failures == 0 {
        passed(out, "postings")?;
        writeln!(out, "      {lists} lists, {decoded} postings")?;
    } else {
        if failures > NAMED {
            writeln!(out, "      and {} more", failures - NAMED)?;
        }
        writeln!(out, "      {failures} of {lists} lists did not decode")?;
    }

    // The dictionary failing is its own count, on top of whatever the lists it
    // did produce were worth. A walk that stopped early has not checked the
    // terms past the break, and the count above is over what it reached.
    Ok(failures + usize::from(broken_dictionary))
}

/// Decodes one posting list, and says how many postings were in it.
///
/// Three separate questions, and a list has to answer all of them. Whether it
/// decodes at all, whether what it decodes ascends, and whether there is as much
/// of it as the dictionary and its own header both say. The last two matter
/// because a flipped bit inside a block of identifiers usually decodes to
/// different identifiers rather than to an error, so decoding on its own would
/// let it through.
fn walk(reader: &index::Reader<'_>, entry: kura_core::terms::Entry) -> Result<u64, Trouble> {
    let list = reader.list(entry)?;
    if list.len() != entry.docs {
        return Err(Trouble::Disagreement(format!(
            "the dictionary says {} postings and the list header says {}",
            entry.docs,
            list.len()
        )));
    }

    let mut cursor = list.cursor();
    let mut seen = 0u64;
    let mut previous: Option<DocId> = None;
    while let Some(doc) = cursor.advance()? {
        if previous.is_some_and(|before| doc <= before) {
            return Err(Trouble::Engine(Error::NotSorted { at: doc }));
        }
        previous = Some(doc);
        seen += 1;
    }

    if seen != u64::from(entry.docs) {
        return Err(Trouble::Disagreement(format!(
            "the header says {} postings and {seen} decoded",
            entry.docs
        )));
    }
    Ok(seen)
}

/// What one posting list can be wrong in.
///
/// The engine's own errors, and the ones only a cross-check finds. The second
/// kind is deliberately not in [`Error`]: they are two pieces of the file
/// disagreeing with each other rather than one piece failing to decode, and
/// nothing on the query path is in a position to notice them.
#[derive(Debug)]
enum Trouble {
    /// The engine refused to decode something.
    Engine(Error),
    /// Two parts of the file said different things.
    Disagreement(String),
}

impl From<Error> for Trouble {
    fn from(error: Error) -> Self {
        Self::Engine(error)
    }
}

impl fmt::Display for Trouble {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Engine(error) => write!(f, "{error}"),
            Self::Disagreement(what) => f.write_str(what),
        }
    }
}

/// Reads every stored document, when there are any.
fn documents(reader: &index::Reader<'_>, out: &mut impl Write) -> io::Result<usize> {
    let Some(store) = reader.store() else {
        // Stored fields are optional, and an index built without them is not an
        // index missing them.
        writeln!(out, "  skipped  stored fields, this index has none")?;
        return Ok(0);
    };

    let mut scratch = Scratch::new();
    let mut fields = 0u64;
    for doc in 0..reader.documents() {
        let mut document = match store.get(doc, &mut scratch) {
            Ok(document) => document,
            Err(error) => {
                failed(out, "stored fields", &error)?;
                writeln!(out, "      at document {doc}")?;
                return Ok(1);
            }
        };
        loop {
            // Reading the fields out and not only the document, because the
            // record is a length and then a run of fields and a length that
            // decodes is not the same as a run of fields that does.
            match document.next_field() {
                Ok(Some(_)) => fields += 1,
                Ok(None) => break,
                Err(error) => {
                    failed(out, "stored fields", &error)?;
                    writeln!(out, "      at document {doc}")?;
                    return Ok(1);
                }
            }
        }
    }

    passed(out, "stored fields")?;
    writeln!(
        out,
        "      {} documents, {fields} fields",
        reader.documents()
    )?;
    Ok(0)
}

/// One check that passed.
fn passed(out: &mut impl Write, what: &str) -> io::Result<()> {
    writeln!(out, "  ok       {what}")
}

/// One check that failed, and what went wrong with it.
fn failed(out: &mut impl Write, what: &str, error: &dyn fmt::Display) -> io::Result<()> {
    writeln!(out, "  FAILED   {what}")?;
    writeln!(out, "      {error}")
}

/// A section's share of the file, as a percentage.
fn share(length: u64, total: u64) -> String {
    if total == 0 {
        return "-".to_string();
    }
    #[expect(
        clippy::cast_precision_loss,
        reason = "a percentage in a report is wanted to one decimal place"
    )]
    let share = length as f64 * 100.0 / total as f64;
    format!("{share:.1}%")
}

/// A length as a `u64`, saturating rather than wrapping on a 128 bit future.
fn as_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kura_core::index::Writer;

    /// An index with enough in it that the checks have something to walk.
    fn an_index() -> Vec<u8> {
        let mut writer = Writer::new();
        for id in 0..200u32 {
            writer
                .add_with_fields(
                    &format!("document {id} about storage and retrieval and indexes"),
                    [("path", format!("doc{id}.txt").as_bytes())],
                )
                .expect("two hundred small documents fit");
        }
        writer.finish().expect("what was written decodes")
    }

    /// Runs the checks over some bytes, and hands back the report and the count.
    ///
    /// The name is the test's own, because these run in parallel and two of them
    /// working on the same index would otherwise be handed the same path and
    /// delete the file out from under each other.
    fn check_bytes(name: &str, index: &[u8]) -> (String, Outcome) {
        let directory = std::env::temp_dir().join(format!("kura-verify-{}", std::process::id()));
        std::fs::create_dir_all(&directory).expect("a temporary directory");
        let path = directory.join(format!("{name}.kura"));
        std::fs::write(&path, index).expect("writing the index under test");

        let mut out = Vec::new();
        let outcome = check(&path, &mut out).expect("the file is readable");
        std::fs::remove_file(&path).ok();
        (String::from_utf8(out).expect("the report is text"), outcome)
    }

    #[test]
    fn an_intact_index_passes_every_check() {
        let (report, outcome) = check_bytes("intact", &an_index());
        assert_eq!(outcome.failures, 0, "{report}");
        assert_eq!(outcome.skipped, 0, "{report}");
        assert!(report.contains("everything checked passed"), "{report}");
        assert!(!report.contains("FAILED"), "{report}");
    }

    #[test]
    fn the_report_names_every_section_the_index_holds() {
        let (report, _) = check_bytes("sections", &an_index());
        for name in ["terms", "postings", "norms", "fields"] {
            assert!(report.contains(name), "no {name} in {report}");
        }
    }

    #[test]
    fn a_file_that_is_not_an_index_stops_at_the_structure() {
        let (report, outcome) = check_bytes("rubbish", &vec![0x5a_u8; 4_096]);
        assert_eq!(outcome.failures, 1, "{report}");
        assert!(report.contains("FAILED   structure"), "{report}");
        assert!(report.contains("nothing else"), "{report}");
    }

    #[test]
    fn a_truncated_index_stops_at_the_structure() {
        let index = an_index();
        let (report, outcome) = check_bytes("truncated", &index[..index.len() / 2]);
        assert_eq!(outcome.failures, 1, "{report}");
        assert!(report.contains("FAILED   structure"), "{report}");
    }

    #[test]
    fn a_version_from_the_future_is_refused_by_name() {
        let mut index = an_index();
        index[8] = 0xff;
        index[9] = 0xff;
        let (report, outcome) = check_bytes("version", &index);
        assert_eq!(outcome.failures, 1, "{report}");
        assert!(report.contains("FAILED   structure"), "{report}");
    }

    #[test]
    fn a_flipped_byte_fails_the_checksum_and_the_walk_carries_on() {
        // The point of the whole design. A damaged body still has a section
        // table, so the report says which stage the damage reached rather than
        // stopping at the checksum and leaving the question open.
        let mut index = an_index();
        let at = index.len() / 2;
        index[at] ^= 0x01;

        let (report, outcome) = check_bytes("flipped", &index);
        assert!(outcome.failures >= 1, "{report}");
        assert!(report.contains("FAILED   checksum"), "{report}");
        assert!(report.contains("sections"), "{report}");
    }

    #[test]
    fn a_damaged_index_is_never_a_panic() {
        // Every byte of the header and the section table, one at a time. This is
        // the region where a wrong value is an offset or a length, which is the
        // shape of damage that turns into a slice out of bounds if anything here
        // trusts what it read.
        let index = an_index();
        for at in 0..96.min(index.len()) {
            let mut damaged = index.clone();
            damaged[at] ^= 0xff;
            let mut out = Vec::new();
            // Straight at the pieces rather than through a file, so this stays a
            // hundred cheap calls and not a hundred writes to a disk.
            if let Ok(segment) = Segment::open_without_checksum(&damaged) {
                let _ = table(&segment, as_u64(damaged.len()), &mut out);
                if let Ok(reader) = index::Reader::open(&segment) {
                    let _ = postings(&reader, &mut out);
                    let _ = documents(&reader, &mut out);
                }
            }
        }
    }

    #[test]
    fn an_index_with_no_stored_fields_says_so_and_does_not_fail() {
        let mut writer = Writer::new();
        writer
            .add("no fields on this one")
            .expect("one document fits");
        let (report, outcome) = check_bytes("nofields", &writer.finish().expect("it decodes"));
        assert_eq!(outcome.failures, 0, "{report}");
        assert!(report.contains("this index has none"), "{report}");
    }

    #[test]
    fn an_empty_index_passes() {
        let (report, outcome) =
            check_bytes("empty", &Writer::new().finish().expect("empty is valid"));
        assert_eq!(outcome.failures, 0, "{report}");
    }

    #[test]
    fn a_share_of_nothing_is_not_a_division_by_zero() {
        assert_eq!(share(0, 0), "-");
        assert_eq!(share(50, 100), "50.0%");
    }
}
