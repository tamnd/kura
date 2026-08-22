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
//! The header, then the section table, then the checksum of the table and of
//! every section in it, then the contents of every section an index needs, then
//! every posting list, then every stored document. The order matters because
//! each stage depends on the one before it: there is no point decoding a posting
//! list out of a section table that points outside the file.
//!
//! A store is that list with three stages in front of it. The superblock, the
//! two manifest slots, and the segment descriptors that say where in the file
//! each segment sits, and then the whole list again for every segment the
//! manifest names. Those three are checked separately because they are the only
//! parts of a store that a segment cannot see, and because they are where an
//! interrupted commit lands. A store also gets one check that a bare segment
//! cannot have at all: the manifest writes down how many documents a segment
//! holds, the segment holds them, and the two are compared.
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
//! Which block of a posting list is damaged. The format checksums each section
//! and nothing smaller, so a mismatch says which section a wrong byte is in, and
//! on a real index the postings are most of the file. Decoding narrows that down
//! to the term whose list stops decoding, which is as far as this goes today,
//! and a byte flipped inside a block of document identifiers usually decodes to
//! different identifiers rather than to an error at all. Catching that needs a
//! checksum per block, which the format does not have yet.
//!
//! It also cannot tell you that an index is the index you meant. Every check
//! here is internal consistency, so an index built from the wrong directory
//! passes all of them.

use std::fmt;
use std::io::{self, Write};
use std::path::Path;

use kura_core::index;
use kura_core::manifest::{self, Manifest, Superblock};
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
/// Takes either shape a path can hold, a store or a bare segment, because both
/// are things the indexer writes and both are things a person ends up holding.
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

    // Which decoder gets the bytes, decided by the magic at the front rather
    // than by handing them to one and then the other and keeping whichever did
    // not complain. Guessing that way answers a question about a damaged store
    // with a complaint about a segment header, which sends the person reading it
    // to the wrong place.
    let outcome = if manifest::looks_like_a_store(&bytes) {
        store(&bytes, out)?
    } else {
        one_segment(&bytes, None, out)?
    };

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

/// Checks the parts of a store that no segment can see, then every segment.
///
/// A commit writes one manifest slot and leaves the other alone, exactly so
/// that a machine which loses power in the middle of it comes back to an older
/// manifest rather than to nothing. So a store where only one slot decodes is a
/// store doing what it was designed to do, and this reports which slot is
/// committed rather than calling the other one damage.
/// Says what the log holds that no segment does, and how many failures that was.
///
/// Counted by walking the records rather than by subtracting the manifest's two
/// positions, because a store that stopped without warning has records past the
/// tail the last commit wrote down and those are exactly the ones worth knowing
/// about. Nothing is applied and nothing is written: a store this reports on is a
/// store still waiting for its replay.
///
/// The records themselves are not read out. This says how many there are and
/// what they come to, and the tool that prints what is inside an index is the
/// one that names things.
fn log(file: &[u8], out: &mut impl Write) -> io::Result<usize> {
    match kura_core::file::walk_log(file, |_| ()) {
        Ok(walked) if walked.records > 0 => {
            writeln!(
                out,
                "    {} records to replay, {}",
                walked.records,
                bytes(walked.bytes)
            )?;
            Ok(0)
        }
        Ok(_) => {
            writeln!(out, "    nothing to replay")?;
            Ok(0)
        }
        Err(error) => {
            writeln!(out, "    the log does not walk, {error}")?;
            Ok(1)
        }
    }
}

fn store(file: &[u8], out: &mut impl Write) -> io::Result<Outcome> {
    let mut outcome = Outcome::default();

    let superblock = match Superblock::decode(file) {
        Ok(superblock) => superblock,
        Err(error) => return fatal(out, "superblock", &error),
    };
    passed(out, "superblock")?;
    writeln!(
        out,
        "      format {}.{}, {} byte pages",
        superblock.major, superblock.minor, superblock.page
    )?;

    // Every slice below is inside the region this proves is present, and they
    // are the only reason it is proved here rather than left to each decode.
    let front = usize::try_from(manifest::WAL_OFFSET).unwrap_or(usize::MAX);
    if file.len() < front {
        return fatal(
            out,
            "manifest",
            &format!(
                "a store keeps its manifest in the first {front} bytes and this file has {}",
                file.len()
            ),
        );
    }

    let a = slot(file, manifest::SLOT_A_OFFSET);
    let b = slot(file, manifest::SLOT_B_OFFSET);
    let committed = match manifest::recover(a, b) {
        Ok(committed) => committed,
        Err(error) => return fatal(out, "manifest", &error),
    };
    let manifest = &committed.manifest;
    passed(out, "manifest")?;
    writeln!(
        out,
        "      committed in slot {:?} at epoch {}",
        committed.slot, manifest.epoch
    )?;
    match Manifest::decode(slot(file, committed.slot.other().offset())) {
        Ok(other) => writeln!(out, "      the other slot holds epoch {}", other.epoch)?,
        // A store starts with one slot written and fills the second on its
        // second commit, so a slot that does not decode is as much what a new
        // store looks like as what an interrupted one does. Neither is damage,
        // and in both cases the slot that did decode is the committed state.
        Err(_) => writeln!(
            out,
            "      the other slot does not decode, which is also what a store looks like before its second commit"
        )?,
    }

    writeln!(out)?;
    writeln!(out, "  documents    {:>12}", manifest.live)?;
    writeln!(out, "  with deleted {:>12}", manifest.total)?;
    writeln!(out, "  log          {:>12}", bytes(superblock.wal_len))?;
    outcome.failures += log(file, out)?;
    writeln!(out, "  segments     {:>12}", manifest.segments.len())?;
    // No share of the file here, unlike the sections inside a segment. Most of a
    // store's length is a log region that is reserved up front and sparse until
    // something writes into it, so a percentage of the file would say that every
    // segment is a rounding error and mean nothing.
    for (n, described) in manifest.segments.iter().enumerate() {
        writeln!(
            out,
            "    {:<4} {:>12}  {:>9} documents  at {}",
            n + 1,
            bytes(described.len),
            described.docs,
            described.offset,
        )?;
    }
    writeln!(out)?;

    outcome.failures += counts(manifest, out)?;

    let ranges = match manifest::locate(&superblock, manifest, file.len()) {
        Ok(ranges) => {
            passed(out, "segment table")?;
            ranges
        }
        Err(error) => {
            failed(out, "segment table", &error)?;
            writeln!(
                out,
                "      the segments are wherever this table says they are, so none of them were read"
            )?;
            outcome.failures += 1;
            outcome.skipped += manifest.segments.len();
            return Ok(outcome);
        }
    };

    if ranges.is_empty() {
        writeln!(out)?;
        writeln!(
            out,
            "  this store holds no segments, which is what one looks like before anything is written into it"
        )?;
        return Ok(outcome);
    }

    let held = ranges.len();
    for (n, range) in ranges.into_iter().enumerate() {
        writeln!(out)?;
        writeln!(out, "  segment {} of {held}", n + 1)?;
        writeln!(out)?;
        let found = one_segment(&file[range], Some(manifest.segments[n].docs), out)?;
        outcome.failures += found.failures;
        outcome.skipped += found.skipped;
    }

    Ok(outcome)
}

/// One manifest slot's bytes.
///
/// The caller has already proved that the file reaches past both slots, which
/// is what makes this a slice rather than an option.
fn slot(file: &[u8], offset: u64) -> &[u8] {
    let at = usize::try_from(offset).unwrap_or(usize::MAX);
    &file[at..][..manifest::SLOT_LEN]
}

/// Checks the totals a manifest carries against the segments it names.
///
/// These counts are written down so that opening a store does not have to read
/// every segment to know how big it is, and anything written down twice can
/// disagree with itself. A manifest that undercounts is the dangerous direction,
/// because a query that trusts it ranks against the wrong collection size and
/// returns plausible answers in the wrong order.
fn counts(manifest: &Manifest, out: &mut impl Write) -> io::Result<usize> {
    let held: u64 = manifest
        .segments
        .iter()
        .map(|described| u64::from(described.docs))
        .sum();

    if manifest.total != held {
        failed(
            out,
            "document counts",
            &format!(
                "the manifest totals {} documents and the segments it names hold {held}",
                manifest.total
            ),
        )?;
        return Ok(1);
    }
    if manifest.live > manifest.total {
        failed(
            out,
            "document counts",
            &format!(
                "the manifest says {} of {} documents are live",
                manifest.live, manifest.total
            ),
        )?;
        return Ok(1);
    }

    passed(out, "document counts")?;
    Ok(0)
}

/// Everything that can be checked from inside one segment.
///
/// `expected` is what a manifest said this segment holds, for a segment that
/// came out of a store, and nothing for a bare one.
///
/// Public to the crate because `repair` decides what to drop out of a store by
/// asking this about each segment in turn, and a repair that made that decision
/// on a cheaper check than the one `verify` prints would be a tool that dropped
/// segments `verify` had just called good.
pub fn one_segment(
    bytes: &[u8],
    expected: Option<u32>,
    out: &mut impl Write,
) -> io::Result<Outcome> {
    let mut outcome = Outcome::default();

    // Structure first, and fatally. Everything below reads through the section
    // table, so a table that does not decode leaves nothing else to try.
    let segment = match Segment::open_without_checksum(bytes) {
        Ok(segment) => segment,
        Err(error) => return fatal(out, "structure", &error),
    };
    passed(out, "structure")?;

    table(&segment, as_u64(bytes.len()), out)?;

    // The checksums are not fatal, which is the whole reason the two open paths
    // are separate. A body that does not match its checksums still decodes often
    // enough to say which term the damage landed in, and that is worth more than
    // refusing to look.
    outcome.failures += checksums(&segment, out)?;

    match index::Reader::open(&segment) {
        Ok(reader) => {
            passed(out, "sections")?;
            contents(&reader, out)?;
            outcome.failures += promised(&reader, expected, out)?;
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

    Ok(outcome)
}

/// Checks a segment against the number of documents the manifest promised.
///
/// Only a store can ask this, because only a store writes the number down in a
/// second place. It is worth asking because the two drifting apart comes back as
/// wrong answers rather than as an error: every document past the count the
/// manifest carries is a document no query will rank, and nothing else in this
/// file would notice them missing.
fn promised(
    reader: &index::Reader<'_>,
    expected: Option<u32>,
    out: &mut impl Write,
) -> io::Result<usize> {
    let Some(expected) = expected else {
        return Ok(0);
    };
    let found = reader.documents();
    if found == expected {
        passed(out, "manifest count")?;
        return Ok(0);
    }
    failed(
        out,
        "manifest count",
        &format!("the manifest says {expected} documents and the segment holds {found}"),
    )?;
    Ok(1)
}

/// A failure that leaves nothing underneath it worth trying.
fn fatal(out: &mut impl Write, what: &str, error: &dyn fmt::Display) -> io::Result<Outcome> {
    failed(out, what, error)?;
    writeln!(out)?;
    writeln!(out, "  nothing else can be read out of this file")?;
    Ok(Outcome {
        failures: 1,
        skipped: 0,
    })
}

/// Prints the section table, which is the map of everything below it.
fn table(segment: &Segment<'_>, total: u64, out: &mut impl Write) -> io::Result<()> {
    writeln!(out)?;
    writeln!(out, "  sections")?;
    for section in segment.sections() {
        writeln!(
            out,
            "    {:<12} {:>12}  {:>6}  at {}",
            label(section.kind),
            bytes(section.length),
            share(section.length, total),
            section.offset,
        )?;
    }
    writeln!(out)
}

/// What to call a section in the report.
///
/// A kind this build has never heard of is not a failure. A reader skips an
/// unknown section and carries on, which is what makes it possible to add one
/// without breaking every older binary, so the honest thing to print is the
/// number rather than a complaint.
fn label(kind: u16) -> String {
    segment::name(kind).map_or_else(|| format!("kind {kind}"), ToString::to_string)
}

/// Checks the section table and then every section against its own digest.
///
/// One line per section rather than one for the file. That is the whole point
/// of a digest per section: a report that says the postings are damaged and the
/// dictionary is intact tells somebody what to do next, and a report that says
/// a byte somewhere is wrong does not.
///
/// The table is checked first and separately. Every digest below it came out of
/// the table, so if the table has been changed then none of the comparisons
/// under it mean anything, and a report that did not say so would be listing
/// sections as good on the word of bytes it had just found to be bad.
fn checksums(segment: &Segment<'_>, out: &mut impl Write) -> io::Result<usize> {
    let mut failures = 0;

    match segment.verify_table() {
        Ok(()) => passed(out, "checksum table")?,
        Err(error) => {
            failed(out, "checksum table", &error)?;
            writeln!(
                out,
                "      the checksums below come out of this table, so treat them as unanswered"
            )?;
            failures += 1;
        }
    }

    for section in segment.sections() {
        let what = format!("checksum {}", label(section.kind));
        match segment.verify_section(section.kind) {
            Ok(()) => passed(out, &what)?,
            Err(error) => {
                failed(out, &what, &error)?;
                failures += 1;
            }
        }
    }

    Ok(failures)
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
    use kura_core::file::Store;
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
    fn the_report_names_the_section_the_damage_is_in() {
        // What a digest per section buys over one digest for the file. The
        // postings fail and everything else is reported as good, so the person
        // reading this knows the dictionary and the stored fields are intact.
        let index = an_index();
        let at = {
            let segment = Segment::open_without_checksum(&index).expect("the index opens");
            let postings = segment
                .sections()
                .find(|section| section.kind == segment::kind::POSTINGS)
                .expect("an index has postings");
            segment::HEADER_LEN + usize::try_from(postings.offset).expect("an offset fits")
        };

        let mut damaged = index.clone();
        damaged[at] ^= 0x01;

        let (report, outcome) = check_bytes("named", &damaged);
        assert!(outcome.failures >= 1, "{report}");
        assert!(report.contains("FAILED   checksum postings"), "{report}");
        assert!(report.contains("ok       checksum table"), "{report}");
        assert!(report.contains("ok       checksum terms"), "{report}");
        assert!(report.contains("ok       checksum fields"), "{report}");
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

    /// How many documents each segment a store test builds holds.
    const PER_SEGMENT: u32 = 20;

    /// A path of this test's own, under a directory this process shares.
    fn a_path(name: &str) -> std::path::PathBuf {
        let directory = std::env::temp_dir().join(format!("kura-verify-{}", std::process::id()));
        std::fs::create_dir_all(&directory).expect("a temporary directory");
        let path = directory.join(format!("{name}.kura"));
        std::fs::remove_file(&path).ok();
        path
    }

    /// A store on disk holding `count` segments, each of [`PER_SEGMENT`] docs.
    ///
    /// Committed once per segment rather than once at the end, so that a store
    /// built here has been through the slot alternation the same number of times
    /// a real one would have.
    fn a_store(name: &str, count: usize) -> std::path::PathBuf {
        let path = a_path(name);
        let mut store =
            Store::create_with_log(&path, 1, 1_700_000_000, 1 << 20).expect("a new store");
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
                .append_segment(&segment, PER_SEGMENT, 1_700_000_000)
                .expect("the segment is written");
            let mut manifest = store.manifest().clone();
            manifest.live += u64::from(PER_SEGMENT);
            manifest.total += u64::from(PER_SEGMENT);
            manifest.segments.push(described);
            store.commit(manifest, 1_700_000_001).expect("committed");
        }
        path
    }

    /// Runs the checks over a file already on disk.
    fn check_file(path: &std::path::Path) -> (String, Outcome) {
        let mut out = Vec::new();
        let outcome = check(path, &mut out).expect("the file is readable");
        (String::from_utf8(out).expect("the report is text"), outcome)
    }

    /// Rewrites the committed manifest of a store, after changing it.
    ///
    /// This is how the tests below build a store that is internally
    /// inconsistent, which is a thing no amount of correct code will produce and
    /// a thing a disk will produce eventually.
    fn recommit(path: &std::path::Path, change: impl FnOnce(&mut kura_core::manifest::Manifest)) {
        let mut store = Store::open(path).expect("the store opens");
        let mut manifest = store.manifest().clone();
        change(&mut manifest);
        store.commit(manifest, 1_700_000_002).expect("committed");
    }

    #[test]
    fn a_store_passes_every_check_and_names_each_segment_it_walked() {
        // The bug this whole path exists for. A store used to come back as not a
        // kura file, which reads as total loss on a file that is completely
        // intact.
        let path = a_store("store-intact", 3);
        let (report, outcome) = check_file(&path);
        assert_eq!(outcome.failures, 0, "{report}");
        assert_eq!(outcome.skipped, 0, "{report}");
        assert!(report.contains("everything checked passed"), "{report}");
        for n in 1..=3 {
            assert!(report.contains(&format!("segment {n} of 3")), "{report}");
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_store_says_which_slot_it_committed_into() {
        // Two commits, so both slots hold something and the report can say which
        // one is current and what the other one is. One commit is the other case
        // and it is covered by the empty store below.
        let path = a_store("store-slots", 2);
        let (report, _) = check_file(&path);
        assert!(report.contains("committed in slot"), "{report}");
        assert!(report.contains("the other slot holds epoch"), "{report}");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_store_with_nothing_in_it_is_not_a_failure() {
        // An empty store is a legal state, not a broken one. It is also the only
        // state where the second manifest slot has never been written, so this
        // covers the report line that says so.
        let path = a_store("store-empty", 0);
        let (report, outcome) = check_file(&path);
        assert_eq!(outcome.failures, 0, "{report}");
        assert!(report.contains("holds no segments"), "{report}");
        assert!(report.contains("before its second commit"), "{report}");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_manifest_that_miscounts_a_segment_is_caught() {
        // The check a bare segment cannot have. Both numbers decode, both are
        // internally consistent, and they disagree, which on the query path would
        // be silently missing documents rather than an error.
        let path = a_store("store-miscount", 1);
        recommit(&path, |manifest| {
            manifest.segments[0].docs = PER_SEGMENT - 5;
            manifest.live = u64::from(PER_SEGMENT - 5);
            manifest.total = u64::from(PER_SEGMENT - 5);
        });

        let (report, outcome) = check_file(&path);
        assert_eq!(outcome.failures, 1, "{report}");
        assert!(report.contains("FAILED   manifest count"), "{report}");
        assert!(report.contains("the segment holds 20"), "{report}");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_manifest_whose_totals_do_not_add_up_is_caught() {
        let path = a_store("store-totals", 2);
        recommit(&path, |manifest| manifest.total += 7);

        let (report, outcome) = check_file(&path);
        assert!(outcome.failures >= 1, "{report}");
        assert!(report.contains("FAILED   document counts"), "{report}");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_descriptor_that_points_past_the_end_of_the_file_reads_no_segments() {
        // The dangerous shape, and the reason the report counts what it did not
        // look at. Two segments are named and neither is reachable, so the answer
        // is two unchecked and not a clean walk.
        let path = a_store("store-outside", 2);
        recommit(&path, |manifest| {
            manifest.segments[1].offset = 1 << 40;
        });

        let (report, outcome) = check_file(&path);
        assert_eq!(outcome.failures, 1, "{report}");
        assert_eq!(outcome.skipped, 2, "{report}");
        assert!(report.contains("FAILED   segment table"), "{report}");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn damage_inside_a_store_is_reported_against_the_segment_it_is_in() {
        // What walking the segments separately buys over one verdict for the
        // file. One of three is damaged, the report says which, and the other two
        // are reported good, which is the difference between rebuilding one
        // segment and restoring the store.
        let path = a_store("store-damaged", 3);
        let mut bytes = std::fs::read(&path).expect("the store reads");
        let at = {
            let (superblock, committed) = manifest::front(&bytes).expect("the front decodes");
            let ranges =
                manifest::locate(&superblock, &committed, bytes.len()).expect("they are located");
            // Into the middle of the second segment, which is past its header and
            // into a section body.
            let second = ranges[1].clone();
            second.start + second.len() / 2
        };
        bytes[at] ^= 0x01;
        std::fs::write(&path, &bytes).expect("the store is rewritten");

        let (report, outcome) = check_file(&path);
        assert!(outcome.failures >= 1, "{report}");
        assert!(report.contains("segment 2 of 3"), "{report}");
        assert!(report.contains("FAILED   checksum"), "{report}");
        let (before, after) = report
            .split_once("segment 2 of 3")
            .expect("the report reached the second segment");
        assert!(!before.contains("FAILED"), "{before}");
        let (_, third) = after
            .split_once("segment 3 of 3")
            .expect("the report reached the third segment");
        assert!(!third.contains("FAILED"), "{third}");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_store_whose_superblock_is_gone_stops_at_the_superblock() {
        let path = a_store("store-superblock", 1);
        let mut bytes = std::fs::read(&path).expect("the store reads");
        // Past the magic, so it still looks like a store and gets as far as the
        // decode. Damaging the magic instead would send it down the segment path,
        // which is a different question and the one the format cannot answer.
        bytes[16] ^= 0xff;
        std::fs::write(&path, &bytes).expect("the store is rewritten");

        let (report, outcome) = check_file(&path);
        assert_eq!(outcome.failures, 1, "{report}");
        assert!(report.contains("FAILED   superblock"), "{report}");
        assert!(report.contains("nothing else"), "{report}");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_store_with_neither_manifest_slot_readable_stops_at_the_manifest() {
        let path = a_store("store-noslots", 1);
        let mut bytes = std::fs::read(&path).expect("the store reads");
        for slot in [manifest::SLOT_A_OFFSET, manifest::SLOT_B_OFFSET] {
            let at = usize::try_from(slot).expect("an offset fits");
            bytes[at..at + manifest::SLOT_LEN].fill(0xff);
        }
        std::fs::write(&path, &bytes).expect("the store is rewritten");

        let (report, outcome) = check_file(&path);
        assert_eq!(outcome.failures, 1, "{report}");
        assert!(report.contains("FAILED   manifest"), "{report}");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_store_truncated_to_its_superblock_says_so_rather_than_slicing() {
        let path = a_path("store-short");
        // A superblock and then the end of the file, which is what a store that
        // was cut off during creation looks like. Every offset in it names a
        // region that is not there.
        let superblock = Superblock::new(1, 1_700_000_000).encode();
        std::fs::write(&path, &superblock).expect("a short store is written");

        let (report, outcome) = check_file(&path);
        assert_eq!(outcome.failures, 1, "{report}");
        assert!(report.contains("FAILED   manifest"), "{report}");
        std::fs::remove_file(&path).ok();
    }
}
