//! Printing what is inside an index, one record to a line.
//!
//! `verify` answers whether a file is intact and `explain` answers what one
//! query did. Neither of them answers the question that comes up in between,
//! which is what is actually in there. That question is asked when a query
//! returns a document that makes no sense, when two engines disagree on a hit
//! count, and when a format change has to be checked against the file it was
//! supposed to produce, and answering it any other way means writing a throwaway
//! program against the reader.
//!
//! # The shape of the output
//!
//! Tab separated, one record to a line, with a single comment line at the top
//! naming the columns. That is a format `cut` and `sort` and `diff` all already
//! understand, and diffing two dumps is most of what this gets used for: the
//! same corpus indexed by two builds should produce two identical dumps, and
//! where they stop being identical is the answer.
//!
//! Every line carries the segment it came from, including when there is only one
//! of them. A column that is always 1 costs nothing and a script that has to
//! know whether it is reading a store before it can parse a line costs plenty.
//!
//! # Terms and documents are somebody's data
//!
//! This is the one tool here that prints the corpus back out. Everything else
//! reports positions and lengths and counts, and that is deliberate, because a
//! term came from a document and a document belonged to somebody. What comes out
//! of here is the corpus, so it belongs wherever the corpus belongs and nowhere
//! else: not in an issue, not in a pull request, not in a bug report, not in a
//! screenshot.

use std::io::{self, Write};

use kura_core::index;
use kura_core::store::Scratch;

use crate::Failure;

/// Which part of an index to print.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum What {
    /// One line per term, out of the dictionary alone.
    Terms,
    /// One line per posting, for every term or for one of them.
    Postings,
    /// One line per stored field.
    Documents,
}

/// What to print, and how much of it.
#[derive(Clone, Copy)]
pub struct Request<'a> {
    /// Which part of the index.
    pub what: What,
    /// The one term to print, if the request named one.
    pub term: Option<&'a [u8]>,
    /// How many records to print, or every one of them.
    pub limit: Option<u64>,
}

/// Prints the requested part of every segment.
///
/// # Errors
///
/// Returns [`Failure::Engine`] if the index does not decode, and
/// [`Failure::Stdout`] if the output cannot be written, which in practice means
/// the reader closed the pipe.
pub fn dump(
    readers: &[index::Reader<'_>],
    request: Request<'_>,
    out: &mut impl Write,
) -> Result<(), Failure> {
    let header = match request.what {
        What::Terms => "# segment\tterm\tdocuments\toffset\tbytes",
        What::Postings => "# segment\tterm\tdocument\tfrequency",
        What::Documents => "# segment\tdocument\tfield\tvalue",
    };
    writeln!(out, "{header}").map_err(Failure::Stdout)?;

    // One budget across every segment rather than one per segment, because a
    // limit is what a person types when they want to see the shape of the output
    // rather than the output, and a limit that multiplied by the number of
    // segments would not do that.
    let mut left = Budget::of(request.limit);
    for (n, reader) in readers.iter().enumerate() {
        let segment = n + 1;
        match request.what {
            What::Terms => terms(reader, segment, request.term, &mut left, out)?,
            What::Postings => postings(reader, segment, request.term, &mut left, out)?,
            What::Documents => documents(reader, segment, &mut left, out)?,
        }
        if left.spent() {
            break;
        }
    }
    Ok(())
}

/// How many more records may be printed.
///
/// A limit nobody asked for is not a limit of zero and not a limit of `u64::MAX`
/// either, and making it a type rather than an `Option` threaded through five
/// functions is what keeps the two cases from being confused at a call site.
struct Budget(Option<u64>);

impl Budget {
    /// A budget from what the command line asked for.
    const fn of(limit: Option<u64>) -> Self {
        Self(limit)
    }

    /// Whether there is nothing left to spend.
    const fn spent(&self) -> bool {
        matches!(self.0, Some(0))
    }

    /// Spends one record, and says whether there was one to spend.
    const fn take(&mut self) -> bool {
        match &mut self.0 {
            Some(0) => false,
            Some(left) => {
                *left -= 1;
                true
            }
            None => true,
        }
    }
}

/// Prints the dictionary of one segment.
///
/// Out of the dictionary alone and without touching the postings, which is what
/// makes this the cheap mode. It is also why there is no frequency column here:
/// the largest frequency in a term's list is what a block bound is built out of
/// and it is a genuinely useful number, and getting it means decoding every
/// posting in the index, which is the other mode.
fn terms(
    reader: &index::Reader<'_>,
    segment: usize,
    only: Option<&[u8]>,
    left: &mut Budget,
    out: &mut impl Write,
) -> Result<(), Failure> {
    let mut entries = reader.entries();
    while let Some((term, entry)) = entries.next_term()? {
        if only.is_some_and(|wanted| wanted != term) {
            continue;
        }
        if !left.take() {
            return Ok(());
        }
        writeln!(
            out,
            "{segment}\t{}\t{}\t{}\t{}",
            escaped(term),
            entry.docs,
            entry.offset,
            entry.len
        )
        .map_err(Failure::Stdout)?;
    }
    Ok(())
}

/// Prints every posting of one segment, or of one term in it.
///
/// A term the request named and this segment does not hold prints nothing, and
/// that is the answer rather than an error. A store puts a term in whichever
/// segments happened to receive a document containing it, so a term missing from
/// one of them is the ordinary case and not a fault.
fn postings(
    reader: &index::Reader<'_>,
    segment: usize,
    only: Option<&[u8]>,
    left: &mut Budget,
    out: &mut impl Write,
) -> Result<(), Failure> {
    if let Some(wanted) = only {
        let Some(list) = reader.postings(wanted)? else {
            return Ok(());
        };
        return list_of(&list, segment, wanted, left, out);
    }

    let mut entries = reader.entries();
    while let Some((term, entry)) = entries.next_term()? {
        // Copied because the list is read through the reader and the term is
        // borrowed from the walk over the dictionary, and the two cannot both be
        // held at once.
        let term = term.to_vec();
        let list = reader.list(entry)?;
        list_of(&list, segment, &term, left, out)?;
        if left.spent() {
            return Ok(());
        }
    }
    Ok(())
}

/// Prints one posting list.
fn list_of(
    list: &kura_core::posting::Reader<'_>,
    segment: usize,
    term: &[u8],
    left: &mut Budget,
    out: &mut impl Write,
) -> Result<(), Failure> {
    let term = escaped(term);
    let mut cursor = list.cursor();
    while let Some(doc) = cursor.advance()? {
        if !left.take() {
            return Ok(());
        }
        writeln!(out, "{segment}\t{term}\t{doc}\t{}", cursor.frequency())
            .map_err(Failure::Stdout)?;
    }
    Ok(())
}

/// Prints every stored field of one segment.
fn documents(
    reader: &index::Reader<'_>,
    segment: usize,
    left: &mut Budget,
    out: &mut impl Write,
) -> Result<(), Failure> {
    let Some(store) = reader.store() else {
        // Stored fields are optional and an index without them is not an index
        // missing them, so this is a dump of nothing rather than a failure.
        return Ok(());
    };

    let mut scratch = Scratch::new();
    for doc in 0..reader.documents() {
        let mut document = store.get(doc, &mut scratch)?;
        while let Some((name, value)) = document.next_field()? {
            if !left.take() {
                return Ok(());
            }
            writeln!(out, "{segment}\t{doc}\t{name}\t{}", escaped(value))
                .map_err(Failure::Stdout)?;
        }
    }
    Ok(())
}

/// A field or a term as one line of a tab separated file.
///
/// Text stays text, because a dump of a Japanese corpus that came back as four
/// escapes per character would be useless to the person who indexed it. What
/// gets escaped is the three characters that would otherwise end the field or
/// the line, the backslash that makes the escaping reversible, and the control
/// characters, which are invisible and therefore worse than useless in a column
/// somebody is comparing by eye.
///
/// Bytes that are not text at all are printed byte by byte. Stored fields hold
/// whatever was put in them and terms come out of an analyser that has seen
/// whatever was in the corpus, so this has to have an answer for a value that is
/// not UTF-8, and the answer that loses nothing is hexadecimal.
fn escaped(value: &[u8]) -> String {
    let Ok(text) = std::str::from_utf8(value) else {
        let mut out = String::with_capacity(value.len() * 4);
        for byte in value {
            hex(&mut out, *byte);
        }
        return out;
    };

    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            // As the bytes it is stored as, so that an escape always names bytes
            // whether it came from here or from the branch above, and a person
            // reading one against a hex view of the file sees the same numbers.
            other if other.is_control() => {
                let mut buffer = [0u8; 4];
                for byte in other.encode_utf8(&mut buffer).as_bytes() {
                    hex(&mut out, *byte);
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// One byte as an escape, appended.
fn hex(out: &mut String, value: u8) {
    // Written out rather than gone through a formatter, because this runs once
    // per byte and a dump is the whole index by definition.
    const DIGITS: [u8; 16] = *b"0123456789abcdef";
    out.push_str("\\x");
    out.push(char::from(DIGITS[usize::from(value >> 4)]));
    out.push(char::from(DIGITS[usize::from(value & 0xf)]));
}

/// Writes a report and turns a closed pipe into a quiet exit.
///
/// A dump is a thing people pipe into `head`, and `head` closes the pipe when it
/// has what it wants. The process on the writing end gets a broken pipe, and
/// printing an error about it would mean every ordinary use of this command ends
/// in a complaint.
///
/// # Errors
///
/// As [`dump`], except that a broken pipe is not one.
pub fn to_stdout(
    readers: &[index::Reader<'_>],
    request: Request<'_>,
    out: &mut impl Write,
) -> Result<(), Failure> {
    match dump(readers, request, out) {
        Err(Failure::Stdout(error)) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kura_core::index::Writer;
    use kura_core::segment::Segment;

    /// Two small documents with a field each, as a segment.
    fn a_segment() -> Vec<u8> {
        let mut writer = Writer::new();
        writer
            .add_with_fields("storage and retrieval", [("path", b"a.txt".as_slice())])
            .expect("a small document fits");
        writer
            .add_with_fields("storage and storage", [("path", b"b.txt".as_slice())])
            .expect("a small document fits");
        writer.finish().expect("what was written decodes")
    }

    /// Runs a request over one segment and hands back what it printed.
    fn dumped(bytes: &[u8], request: Request<'_>) -> String {
        let segment = Segment::open_without_checksum(bytes).expect("the segment opens");
        let reader = index::Reader::open(&segment).expect("the sections open");
        let mut out = Vec::new();
        dump(&[reader], request, &mut out).expect("the dump runs");
        String::from_utf8(out).expect("the dump is text")
    }

    /// A request with nothing narrowed down.
    const fn all(what: What) -> Request<'static> {
        Request {
            what,
            term: None,
            limit: None,
        }
    }

    /// The lines of a dump with the column header taken off.
    fn records(dump: &str) -> Vec<&str> {
        dump.lines().filter(|line| !line.starts_with('#')).collect()
    }

    #[test]
    fn every_term_in_the_dictionary_gets_a_line() {
        let dump = dumped(&a_segment(), all(What::Terms));
        let lines = records(&dump);
        // and, retrieval, storage
        assert_eq!(lines.len(), 3, "{dump}");
        assert!(dump.starts_with("# segment\t"), "{dump}");
        assert!(lines.iter().any(|line| line.starts_with("1\tstorage\t2\t")));
    }

    #[test]
    fn a_posting_carries_the_frequency_and_not_only_the_document() {
        // The second document says storage twice, and a dump that dropped the
        // frequency would look identical to one that said it once, which is the
        // difference every ranking decision is made out of.
        let dump = dumped(&a_segment(), all(What::Postings));
        assert!(dump.contains("1\tstorage\t0\t1"), "{dump}");
        assert!(dump.contains("1\tstorage\t1\t2"), "{dump}");
    }

    #[test]
    fn one_term_can_be_asked_for_on_its_own() {
        let segment = a_segment();
        let dump = dumped(
            &segment,
            Request {
                what: What::Postings,
                term: Some(b"retrieval"),
                limit: None,
            },
        );
        let lines = records(&dump);
        assert_eq!(lines.len(), 1, "{dump}");
        assert!(lines[0].starts_with("1\tretrieval\t0\t"), "{dump}");
    }

    #[test]
    fn a_term_the_segment_does_not_hold_prints_nothing_and_is_not_an_error() {
        // The ordinary case in a store, where a term lives in whichever segments
        // happened to receive a document containing it.
        let segment = a_segment();
        let dump = dumped(
            &segment,
            Request {
                what: What::Postings,
                term: Some(b"absent"),
                limit: None,
            },
        );
        assert!(records(&dump).is_empty(), "{dump}");
    }

    #[test]
    fn the_stored_fields_come_back_with_the_document_they_belong_to() {
        let dump = dumped(&a_segment(), all(What::Documents));
        assert!(dump.contains("1\t0\tpath\ta.txt"), "{dump}");
        assert!(dump.contains("1\t1\tpath\tb.txt"), "{dump}");
    }

    #[test]
    fn an_index_with_no_stored_fields_dumps_nothing_rather_than_failing() {
        let mut writer = Writer::new();
        writer.add("no fields on this one").expect("one fits");
        let bytes = writer.finish().expect("it decodes");
        let dump = dumped(&bytes, all(What::Documents));
        assert!(records(&dump).is_empty(), "{dump}");
    }

    #[test]
    fn a_limit_is_spent_across_the_segments_and_not_once_per_segment() {
        let bytes = a_segment();
        let segment = Segment::open_without_checksum(&bytes).expect("the segment opens");
        let readers = vec![
            index::Reader::open(&segment).expect("the sections open"),
            index::Reader::open(&segment).expect("the sections open"),
        ];
        let mut out = Vec::new();
        dump(
            &readers,
            Request {
                what: What::Terms,
                term: None,
                limit: Some(4),
            },
            &mut out,
        )
        .expect("the dump runs");
        let dump = String::from_utf8(out).expect("the dump is text");
        assert_eq!(records(&dump).len(), 4, "{dump}");
        // Three terms in the first segment and one in the second, which is what
        // says the budget crossed the boundary rather than resetting at it.
        assert!(dump.contains("\n2\t"), "{dump}");
    }

    #[test]
    fn a_limit_of_zero_prints_the_columns_and_no_records() {
        let dump = dumped(
            &a_segment(),
            Request {
                what: What::Terms,
                term: None,
                limit: Some(0),
            },
        );
        assert!(dump.starts_with("# segment"), "{dump}");
        assert!(records(&dump).is_empty(), "{dump}");
    }

    #[test]
    fn a_tab_in_a_field_does_not_become_a_column() {
        assert_eq!(escaped(b"one\ttwo"), "one\\ttwo");
        assert_eq!(escaped(b"one\ntwo"), "one\\ntwo");
        assert_eq!(escaped(b"back\\slash"), "back\\\\slash");
    }

    #[test]
    fn text_that_is_not_ascii_stays_text() {
        // A dump of a Japanese corpus that came back as four escapes per
        // character would be useless to the person who indexed it.
        assert_eq!(escaped("倉".as_bytes()), "倉");
        assert_eq!(escaped("café".as_bytes()), "café");
    }

    #[test]
    fn bytes_that_are_not_text_are_printed_and_not_lost() {
        assert_eq!(escaped(&[0xff, 0xfe]), "\\xff\\xfe");
        assert_eq!(escaped(&[0x00]), "\\x00");
    }
}
