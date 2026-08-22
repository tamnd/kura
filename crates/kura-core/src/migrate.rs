//! Files written by an older build, brought forward to this one.
//!
//! A format that lasts is a format somebody can move off. This is here now,
//! while there is one step to take and it is a small one, rather than when it is
//! first needed, because the build that needs it is the build that cannot write
//! it any more: by then the old layout is only a shape in a file nobody has the
//! code for.
//!
//! # What a step is
//!
//! One version to the next, and no further. [`segment()`] walks from whatever
//! version a file says it is up to [`FORMAT_VERSION`] one step at a time, so the
//! step from 3 to 4 is written once and every older file reaches it by going
//! through the steps before it. The alternative, a function per pair of
//! versions, is a table that grows as the square and where most of the entries
//! are never run.
//!
//! A step works on the sections and not on the file. The container, which is the
//! header, the section table, the digests and the footer, has not changed across
//! any version this build knows, so a migration takes the sections apart, hands
//! the ones that moved to the step, and builds a new segment out of the result.
//! That means the digests and the footer come out of the ordinary writer rather
//! than being patched, and a migrated file is byte for byte what this build
//! would have written if it had indexed the corpus itself.
//!
//! # Version 1 to version 2
//!
//! Version 2 put the first four bytes of each block's first term into an array
//! of its own in front of the block offsets, so that the binary search over the
//! block index compares a word instead of reading a term. Nothing else about the
//! dictionary moved: the same blocks, the same index entries, the same offsets.
//!
//! So the step reads nothing it does not have to. The terms it needs are the
//! first term of each block, and those are already in the index entries, whole,
//! which is where the search would have read them from. It walks the entries,
//! works out a key for each, and writes the dictionary back out with the keys in
//! front. The blocks are copied across untouched.
//!
//! # Sections that are worked out from other sections
//!
//! Not every section is data. The block score ceilings are a function of the
//! postings and the document lengths, which means a segment written before that
//! section existed is not missing anything a migration has to invent, it is
//! missing something a migration can compute. So after the version steps there
//! is a second pass that rebuilds those, and it runs whether or not the old file
//! had them.
//!
//! This is not a version step and it does not move the version. Adding a section
//! is not a format change, because a reader that has never heard of a kind
//! returns the same results without it, so a build that adds one does not get to
//! call the files before it a different version. What it does get is a migration
//! that still keeps its promise: a migrated file is what this build would have
//! written, and this build would have written the ceilings.
//!
//! # What this does not do
//!
//! It does not write in place. A migration reads one file and writes another,
//! and the caller keeps the original until it is satisfied with what came out.
//! Anything else would be a tool that can leave a store as neither version.

use crate::codec::{get_u32, get_u64, get_uvarint, put_u32, put_uvarint, split_at};
use crate::error::{Error, Result};
use crate::index::average;
use crate::search::{B, K1};
use crate::segment::{self, Segment};
use crate::{DocId, FORMAT_VERSION, bound, length, posting, terms};

/// Brings one segment forward to [`FORMAT_VERSION`].
///
/// `Ok(None)` means the segment is already at this build's version and there is
/// nothing to do, which is a different answer from a segment that was migrated
/// and happened to come out the same, and callers want to be able to tell them
/// apart. `Ok(Some(bytes))` is the migrated segment.
///
/// The input is opened with every structural check and every digest, so a
/// migration never starts from a file that has not been proved to be the file
/// that was written.
///
/// # Errors
///
/// Returns whatever [`Segment::open_for_migration`] returns for a segment that
/// is not one, is damaged, or is a version outside what this build knows, and
/// [`Error::Truncated`] or [`Error::Overflow`] if a section is not the shape the
/// version it claims says it should be.
pub fn segment(bytes: &[u8]) -> Result<Option<Vec<u8>>> {
    let old = Segment::open_for_migration(bytes)?;
    let mut version = old.version();
    if version == FORMAT_VERSION {
        return Ok(None);
    }

    // Every section, in the order it was written, so that a section this build
    // has never heard of comes out the other side where it went in. A migration
    // that dropped what it did not recognise would be a worse outcome than one
    // that refused to run.
    let mut sections: Vec<(u16, Vec<u8>)> = old
        .sections()
        .map(|section| {
            let payload = old.section(section.kind).ok_or(Error::Truncated {
                needed: 0,
                available: 0,
            })?;
            Ok((section.kind, payload.to_vec()))
        })
        .collect::<Result<_>>()?;

    while version < FORMAT_VERSION {
        match version {
            1 => one_to_two(&mut sections)?,
            // Unreachable while the versions above are every one between
            // OLDEST_VERSION and FORMAT_VERSION, and a refusal rather than a
            // panic because the thing that makes it reachable is somebody moving
            // FORMAT_VERSION and forgetting the step, which is a mistake that
            // should come back as a file that did not migrate.
            other => {
                return Err(Error::UnsupportedVersion {
                    found: other,
                    expected: FORMAT_VERSION,
                });
            }
        }
        version += 1;
    }
    derive(&mut sections)?;

    let mut writer = segment::Writer::new();
    for (kind, payload) in sections {
        writer.add(kind, payload)?;
    }
    Ok(Some(writer.finish()))
}

/// Rebuilds the sections that are worked out from the others.
///
/// Three of them: the rounded document lengths, which are a function of the four
/// byte lengths, and the block score ceilings at both widths, which are a
/// function of the postings and the rounded lengths. All are rebuilt rather than
/// kept, because
/// a file old enough to be migrated is old enough that whatever it holds was
/// computed by rules this build has moved on from, and recomputing is cheaper to
/// reason about than deciding which old ones are still good.
///
/// A segment missing the postings or the lengths gets nothing and is not an
/// error. That is a segment holding something other than an index, which the
/// container allows and this has no opinion about.
fn derive(sections: &mut Vec<(u16, Vec<u8>)>) -> Result<()> {
    let find = |want: u16| {
        sections
            .iter()
            .find(|(kind, _)| *kind == want)
            .map(|(_, payload)| payload.as_slice())
    };
    let (Some(dictionary), Some(postings), Some(norms)) = (
        find(segment::kind::TERMS),
        find(segment::kind::POSTINGS),
        find(segment::kind::NORMS),
    ) else {
        return Ok(());
    };
    let short = rounded(norms)?;
    let built = ceilings(dictionary, postings, norms)?;

    sections.retain(|(kind, _)| {
        !matches!(
            *kind,
            segment::kind::BOUNDS | segment::kind::WIDE_BOUNDS | segment::kind::ROUNDED
        )
    });
    // Straight after the lengths they were computed from, in the order the
    // indexer adds them. The order of the section table is part of what a byte
    // for byte comparison against a freshly built segment is comparing.
    let mut at = sections
        .iter()
        .position(|(kind, _)| *kind == segment::kind::NORMS)
        .map_or(sections.len(), |at| at + 1);
    sections.insert(at, (segment::kind::ROUNDED, short));
    at += 1;
    if let Some((narrow, wide)) = built {
        sections.insert(at, (segment::kind::BOUNDS, narrow));
        at += 1;
        sections.insert(at, (segment::kind::WIDE_BOUNDS, wide));
    }
    Ok(())
}

/// One byte per document, from the four byte lengths beside them.
fn rounded(norms: &[u8]) -> Result<Vec<u8>> {
    let (documents, rest) = get_u32(norms)?;
    let (_, lengths) = get_u64(rest)?;
    let needed = (documents as usize).checked_mul(4).ok_or(Error::Overflow)?;
    if lengths.len() < needed {
        return Err(Error::Truncated {
            needed,
            available: lengths.len(),
        });
    }
    Ok(lengths[..needed]
        .chunks_exact(4)
        .map(|bytes| {
            let mut four = [0u8; 4];
            four.copy_from_slice(bytes);
            length::round(u32::from_le_bytes(four))
        })
        .collect())
}

/// The block score ceilings of a segment, worked out from its own sections.
///
/// The indexer builds these as it writes the posting lists, where every
/// frequency is already in hand. Here there is no such luck and the lists have
/// to be decoded, which is why this is an offline tool's cost and not a
/// reader's. The two have to agree byte for byte, and the fixture comparison in
/// `tests/format.rs` is what says they do.
fn ceilings(
    dictionary: &[u8],
    postings: &[u8],
    norms: &[u8],
) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
    let (documents, rest) = get_u32(norms)?;
    let (total, lengths) = get_u64(rest)?;
    let needed = (documents as usize).checked_mul(4).ok_or(Error::Overflow)?;
    if lengths.len() < needed {
        return Err(Error::Truncated {
            needed,
            available: lengths.len(),
        });
    }
    let length_of = |doc: DocId| -> u32 {
        lengths
            .get(doc as usize * 4..)
            .and_then(<[u8]>::first_chunk::<4>)
            .map_or(0, |bytes| u32::from_le_bytes(*bytes))
    };

    let mut writer = bound::Writer::new(K1, B, average(total, documents));
    let mut entries = terms::Reader::new(dictionary)?.entries();
    // Ascending by term, which for a dictionary written by this crate is also
    // ascending by the offset of the list, and ascending offsets is what the
    // ceiling directory is searched by.
    while let Some((_, entry)) = entries.next_term()? {
        let start = usize::try_from(entry.offset).map_err(|_| Error::Overflow)?;
        let len = usize::try_from(entry.len).map_err(|_| Error::Overflow)?;
        let end = start.checked_add(len).ok_or(Error::Overflow)?;
        let list = postings.get(start..end).ok_or(Error::SectionOutOfRange {
            kind: segment::kind::POSTINGS,
            offset: entry.offset,
            length: entry.len,
        })?;
        for (doc, frequency) in posting::Reader::new(list)?.to_postings()? {
            writer.push(frequency, length_of(doc));
        }
        writer.finish_term(entry.offset);
    }
    // A segment whose every list is shorter than a block earns no section, which
    // is the same judgement the indexer makes.
    if writer.is_empty() {
        return Ok(None);
    }
    // Read out before the narrow ones consume the writer, which is the same
    // order the indexer does it in and for the same reason.
    let wide = writer.wide().to_vec();
    Ok(Some((writer.finish(), wide)))
}

/// Version 1 to version 2: the term dictionary gains its array of block keys.
fn one_to_two(sections: &mut [(u16, Vec<u8>)]) -> Result<()> {
    // A segment without a dictionary is a fact about the build that wrote it and
    // not a fault, so there being nothing here is an answer.
    let Some((_, payload)) = sections
        .iter_mut()
        .find(|(kind, _)| *kind == segment::kind::TERMS)
    else {
        return Ok(());
    };
    *payload = terms_one_to_two(payload)?;
    Ok(())
}

/// Rewrites a version 1 term dictionary as a version 2 one.
///
/// The version 1 header is the term count, the block count, the block offsets,
/// the index and the blocks. Version 2 is the same with an array of keys between
/// the block count and the offsets, so this is a copy with one array worked out
/// and inserted, and the blocks, which are all but a fortieth of the section,
/// go across as they are.
fn terms_one_to_two(old: &[u8]) -> Result<Vec<u8>> {
    let (count, rest) = get_uvarint(old)?;
    let (blocks, rest) = get_uvarint(rest)?;
    let blocks = usize::try_from(blocks).map_err(|_| Error::Overflow)?;

    let offsets_len = blocks.checked_mul(4).ok_or(Error::Overflow)?;
    let (offsets, rest) = split_at(rest, offsets_len)?;

    let (index_len, rest) = get_uvarint(rest)?;
    let index_len = usize::try_from(index_len).map_err(|_| Error::Overflow)?;
    let (index, rest) = split_at(rest, index_len)?;

    let (body_len, rest) = get_uvarint(rest)?;
    let body_len = usize::try_from(body_len).map_err(|_| Error::Overflow)?;
    let (body, _) = split_at(rest, body_len)?;

    let mut out = Vec::with_capacity(old.len() + offsets_len);
    put_uvarint(&mut out, count);
    put_uvarint(&mut out, blocks as u64);
    for block in 0..blocks {
        let (at, _) = get_u32(&offsets[block * 4..])?;
        let at = usize::try_from(at).map_err(|_| Error::Overflow)?;
        let entry = index.get(at..).ok_or(Error::Truncated {
            needed: at,
            available: index.len(),
        })?;
        let (len, entry) = get_uvarint(entry)?;
        let len = usize::try_from(len).map_err(|_| Error::Overflow)?;
        let (term, _) = split_at(entry, len)?;
        put_u32(&mut out, terms::key(term));
    }
    out.extend_from_slice(offsets);
    put_uvarint(&mut out, index_len as u64);
    out.extend_from_slice(index);
    put_uvarint(&mut out, body_len as u64);
    out.extend_from_slice(body);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terms::Entry;

    /// A vocabulary with the things that make a key interesting in it: terms
    /// shorter than a key, terms that agree on their first four bytes, and a
    /// term with a zero byte where the padding would be.
    fn vocabulary() -> Vec<Vec<u8>> {
        let mut out: Vec<Vec<u8>> = vec![
            b"a".to_vec(),
            b"ab".to_vec(),
            b"abc".to_vec(),
            b"abcd".to_vec(),
            b"abcde".to_vec(),
            b"ab\0cd".to_vec(),
        ];
        for n in 0..500 {
            out.push(format!("prefix{n:04}").into_bytes());
        }
        for n in 0..40 {
            out.push(format!("z{n:02}").into_bytes());
        }
        out.sort();
        out.dedup();
        out
    }

    /// The dictionary this build writes, and the same one without the keys,
    /// which is what version 1 wrote.
    fn both() -> (Vec<u8>, Vec<u8>) {
        let words = vocabulary();
        let mut writer = terms::Writer::new();
        let mut offset = 0u64;
        for (n, word) in words.iter().enumerate() {
            writer
                .push(
                    word,
                    Entry {
                        docs: u32::try_from(n).expect("a short vocabulary") + 1,
                        offset,
                        len: 7 + n as u64 % 11,
                    },
                )
                .expect("sorted");
            offset += 7 + n as u64 % 11;
        }
        let new = writer.finish();
        (strip_keys(&new), new)
    }

    /// Takes the keys back out of a version 2 dictionary, which gives the bytes
    /// version 1 would have written for the same terms.
    ///
    /// Going backwards like this rather than checking in a file is what makes
    /// this a test of the step and not of a fixture. The fixture is in
    /// `tests/format.rs`, where it belongs, and it is what proves this
    /// reconstruction is the layout version 1 actually used.
    fn strip_keys(new: &[u8]) -> Vec<u8> {
        let (count, rest) = get_uvarint(new).expect("header");
        let (blocks, rest) = get_uvarint(rest).expect("header");
        let keys_len = usize::try_from(blocks).expect("a short dictionary") * 4;
        let mut out = Vec::with_capacity(new.len() - keys_len);
        put_uvarint(&mut out, count);
        put_uvarint(&mut out, blocks);
        out.extend_from_slice(&rest[keys_len..]);
        out
    }

    #[test]
    fn a_version_one_dictionary_migrates_to_the_one_this_build_writes() {
        let (old, new) = both();
        assert_eq!(terms_one_to_two(&old).expect("migrates"), new);
    }

    #[test]
    fn a_segment_already_at_this_version_is_left_alone() {
        let mut writer = segment::Writer::new();
        writer
            .add(segment::kind::TERMS, both().1)
            .expect("one section");
        let bytes = writer.finish();
        assert_eq!(segment(&bytes).expect("opens"), None);
    }

    #[test]
    fn a_migrated_segment_is_what_this_build_would_have_written() {
        let (old, new) = both();

        let mut before = segment::Writer::new();
        before
            .add(segment::kind::TERMS, old)
            .expect("a fresh writer");
        before
            .add(segment::kind::POSTINGS, b"postings go here".to_vec())
            .expect("a fresh writer");
        // A kind this build has never heard of, which has to survive.
        before.add(600, b"something else".to_vec()).expect("room");
        let mut bytes = before.finish();
        bytes[8..10].copy_from_slice(&1u16.to_le_bytes());
        // The header is not covered by either digest, so a version written by
        // hand is a file that still verifies. That is the same reason the
        // version is checked before anything is parsed.

        let mut after = segment::Writer::new();
        after
            .add(segment::kind::TERMS, new)
            .expect("a fresh writer");
        after
            .add(segment::kind::POSTINGS, b"postings go here".to_vec())
            .expect("a fresh writer");
        after.add(600, b"something else".to_vec()).expect("room");

        let migrated = segment(&bytes).expect("migrates").expect("was version 1");
        assert_eq!(migrated, after.finish());
    }

    #[test]
    fn a_segment_without_a_dictionary_migrates_to_itself_at_the_new_version() {
        let mut before = segment::Writer::new();
        before
            .add(segment::kind::VECTORS, b"vectors go here".to_vec())
            .expect("a fresh writer");
        let mut bytes = before.finish();
        bytes[8..10].copy_from_slice(&1u16.to_le_bytes());

        let migrated = segment(&bytes).expect("migrates").expect("was version 1");
        let mut after = segment::Writer::new();
        after
            .add(segment::kind::VECTORS, b"vectors go here".to_vec())
            .expect("a fresh writer");
        assert_eq!(migrated, after.finish());
    }

    #[test]
    fn a_version_from_the_future_is_refused_rather_than_migrated() {
        let mut writer = segment::Writer::new();
        writer
            .add(segment::kind::TERMS, both().1)
            .expect("one section");
        let mut bytes = writer.finish();
        bytes[8..10].copy_from_slice(&(FORMAT_VERSION + 1).to_le_bytes());
        assert_eq!(
            segment(&bytes),
            Err(Error::UnsupportedVersion {
                found: FORMAT_VERSION + 1,
                expected: FORMAT_VERSION,
            })
        );
    }

    #[test]
    fn a_damaged_segment_is_refused_before_anything_is_migrated() {
        let mut writer = segment::Writer::new();
        writer
            .add(segment::kind::TERMS, both().0)
            .expect("one section");
        let mut bytes = writer.finish();
        bytes[8..10].copy_from_slice(&1u16.to_le_bytes());
        let at = bytes.len() / 2;
        bytes[at] ^= 0xff;
        assert!(
            segment(&bytes).is_err(),
            "a migration ran over a segment whose checksum did not match"
        );
    }

    #[test]
    fn a_truncated_dictionary_comes_back_as_an_error() {
        let old = both().0;
        for cut in 0..old.len() {
            // Not a panic and not a dictionary invented out of the bytes that
            // are there, which is the rule every decoder in the crate follows
            // and which a migration is no exception to.
            let _ = terms_one_to_two(&old[..cut]);
        }
    }
}
