//! What a file written by this build looks like, pinned to a file on disk.
//!
//! Every other test in the crate checks that this build reads what this build
//! writes, which is a property that holds just as well after both sides of the
//! format change together. That is the failure this is here for. The fixtures
//! in `testdata/format` were written by an earlier build and are checked in, so
//! a change to the layout of a section shows up as a file that no longer
//! matches rather than as nothing at all.
//!
//! There are two halves to it, and they fail for different reasons.
//!
//! Writing the fixture again and comparing it byte for byte catches a change to
//! what this build produces, including one that a reader would still accept,
//! such as a field that grew or an ordering that moved. It also pins the writer
//! to being deterministic, since a writer whose output depended on the order a
//! hash map handed it something would fail here on some runs and not others.
//!
//! Reading the checked in bytes and asking them the same questions catches the
//! other direction: this build no longer understanding a file it wrote before.
//! That is the one that matters to somebody with data on disk, and it is the
//! one that a change to a decoder alone can cause.
//!
//! # When it fails
//!
//! A failure here is a format change, and the answer to it is not to rewrite
//! the fixture. It is to decide whether the change is meant, and if it is, to
//! move [`kura_core::FORMAT_VERSION`], teach `kura-cli migrate` the step from
//! the old version to the new one, keep the old fixture as the input to a test
//! of that step, and write a new fixture beside it.
//!
//! Setting `KURA_BLESS=1` writes the fixtures rather than checking them, which
//! is how a new one is made. It is deliberately awkward to reach for.

#![cfg(feature = "fs")]

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use kura_core::file::Store;
use kura_core::index::{Reader, Writer};
use kura_core::search::Searcher;
use kura_core::segment::{self, Segment};
use kura_core::store::Scratch;
use kura_core::terms;

/// Where the fixtures live, from wherever the test binary is run.
fn testdata() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/format")
}

/// The corpus the segment fixture is built from.
///
/// Small enough to read, and shaped to reach the parts of the format that only
/// appear at some size: more terms than fit in one dictionary block, more
/// documents than fit in one posting block, a term in every document and a term
/// in one, and stored fields both shorter and longer than a store block.
fn corpus() -> Vec<(String, String)> {
    const SUBJECTS: [&str; 8] = [
        "the storehouse",
        "a segment",
        "the manifest",
        "a posting list",
        "the dictionary",
        "a bitmap",
        "the compressor",
        "a query",
    ];
    const VERBS: [&str; 6] = [
        "holds",
        "verifies",
        "decodes",
        "refuses",
        "compresses",
        "answers",
    ];
    const OBJECTS: [&str; 7] = [
        "every document in order",
        "a truncated file",
        "the bytes it was given",
        "what a reader asks for",
        "one block at a time",
        "the terms of a query",
        "a corrupted section",
    ];

    let mut out = Vec::new();
    for i in 0..400usize {
        let subject = SUBJECTS[i % SUBJECTS.len()];
        let verb = VERBS[(i / 8) % VERBS.len()];
        let object = OBJECTS[(i / 48) % OBJECTS.len()];
        // A term that is in every document and a term that is in one, so the
        // fixture holds both ends of the posting list length distribution.
        let mut text = format!("kura {subject} {verb} {object} number{i:04}");
        if i % 97 == 0 {
            text.push_str(" rare");
        }
        // A document longer than a store block, so the fixture covers a stored
        // record that gets a block to itself.
        if i == 13 {
            for word in 0..2_000 {
                write!(text, " filler{word:04}").expect("a string never fails");
            }
        }
        let path = format!("doc/{:03}/{}.txt", i / 100, i);
        out.push((path, text));
    }
    out
}

/// Builds the segment fixture from the corpus.
fn build() -> Vec<u8> {
    let mut writer = Writer::new();
    for (path, text) in corpus() {
        writer
            .add_with_fields(
                &text,
                [("path", path.as_bytes()), ("body", text.as_bytes())],
            )
            .expect("the corpus is small");
    }
    writer.finish().expect("the corpus is small")
}

/// Compares `built` against the fixture at `name`, or writes it when blessing.
fn pin(name: &str, built: &[u8]) -> Vec<u8> {
    let path = testdata().join(name);
    if std::env::var_os("KURA_BLESS").is_some() {
        std::fs::create_dir_all(testdata()).expect("testdata is writable");
        std::fs::write(&path, built).expect("testdata is writable");
        return built.to_vec();
    }
    std::fs::read(&path)
        .unwrap_or_else(|error| panic!("{}: {error}, and KURA_BLESS is not set", path.display()))
}

#[test]
fn this_build_writes_the_fixture_byte_for_byte() {
    let built = build();
    let fixture = pin("segment.kura", &built);

    if built != fixture {
        let at = built
            .iter()
            .zip(&fixture)
            .position(|(a, b)| a != b)
            .unwrap_or(built.len().min(fixture.len()));
        panic!(
            "the segment this build writes is {} bytes against the fixture's {}, first differing \
             at byte {at}. This is a format change. See the comment at the top of this file.",
            built.len(),
            fixture.len()
        );
    }
}

/// Builds the store fixture: two of the same segment, in one file.
///
/// The identifier and the timestamps are fixed rather than taken from the clock
/// and the process, which is what makes a store file something that can be
/// pinned at all. A short log ring keeps the fixture small, since the ring is
/// sized when the store is made and is written out whether or not it holds
/// anything.
fn build_store(at: &Path) -> Vec<u8> {
    const IDENTITY: u128 = 0x6b75_7261_0000_0000_0000_0000_0000_0001;
    const CREATED: u64 = 1_700_000_000_000_000_000;
    const LOG: u64 = 64 * 1024;

    let _ = std::fs::remove_file(at);
    let segment = build();
    let mut store = Store::create_with_log(at, IDENTITY, CREATED, LOG).expect("a writable path");
    for round in 0..2u64 {
        let mut manifest = store.manifest().clone();
        let written = store
            .append_segment(&segment, 400, CREATED + round)
            .expect("a writable path");
        manifest.live += 400;
        manifest.total += 400;
        manifest.segments.push(written);
        store
            .commit(manifest, CREATED + round)
            .expect("a writable path");
    }
    drop(store);
    let bytes = std::fs::read(at).expect("the store was just written");
    let _ = std::fs::remove_file(at);
    bytes
}

#[test]
fn this_build_writes_the_store_fixture_byte_for_byte() {
    let scratch = std::env::temp_dir().join("kura-format-fixture-write.kura");
    let built = build_store(&scratch);
    let fixture = pin("store.kura", &built);
    assert_eq!(
        built.len(),
        fixture.len(),
        "the store this build writes is a different size than the fixture. This is a format \
         change. See the comment at the top of this file."
    );
    assert!(
        built == fixture,
        "the store this build writes differs from the fixture at byte {}. This is a format \
         change. See the comment at the top of this file.",
        built
            .iter()
            .zip(&fixture)
            .position(|(a, b)| a != b)
            .unwrap_or(0)
    );
}

#[test]
fn this_build_reads_the_store_fixture() {
    let scratch = std::env::temp_dir().join("kura-format-fixture-read.kura");
    let fixture = pin("store.kura", &build_store(&scratch));
    let at = std::env::temp_dir().join("kura-format-fixture-open.kura");
    std::fs::write(&at, &fixture).expect("a writable path");

    let store = Store::open(&at).expect("the fixture opens");
    assert_eq!(store.manifest().segments.len(), 2);
    assert_eq!(store.manifest().live, 800);

    let view = store.view().expect("the segments are there");
    assert_eq!(view.len(), 2);
    for bytes in view.all() {
        let segment = Segment::open(bytes).expect("each segment verifies");
        let index = Reader::open(&segment).expect("each segment is an index");
        assert_eq!(index.documents(), 400);
    }
    drop(view);
    drop(store);
    let _ = std::fs::remove_file(&at);
}

#[test]
fn the_writer_gives_the_same_bytes_every_time() {
    // The half of the pinning above that does not need a file: a writer that
    // walked a hash map somewhere would fail here on some runs and not others,
    // and would fail the comparison against the fixture on every machine but
    // the one that wrote it.
    assert_eq!(build(), build());
}

#[test]
fn this_build_reads_the_fixture_and_gets_the_same_answers() {
    let fixture = pin("segment.kura", &build());
    let segment = Segment::open(&fixture).expect("the fixture verifies");
    assert_eq!(segment.version(), kura_core::FORMAT_VERSION);

    let index = Reader::open(&segment).expect("the fixture is an index");
    assert_eq!(index.documents(), 400);

    // A term in every document, a term in one, and a term in none.
    let all = index
        .postings(b"kura")
        .expect("decodes")
        .expect("kura is in every document");
    assert_eq!(all.len(), 400);
    let rare = index
        .postings(b"number0013")
        .expect("decodes")
        .expect("the number of one document");
    assert_eq!(rare.len(), 1);
    assert!(
        index.postings(b"absent").expect("decodes").is_none(),
        "a term the corpus does not hold"
    );

    // The dictionary, walked the way `verify` walks it.
    let terms = terms::Reader::new(
        segment
            .section(segment::kind::TERMS)
            .expect("the fixture has a dictionary"),
    )
    .expect("header");
    let mut walked = 0usize;
    let mut entries = terms.entries();
    while entries.next_term().expect("walk").is_some() {
        walked += 1;
    }
    assert_eq!(walked, terms.len() as usize);

    // The stored fields, including the document that is longer than a block.
    let stored = index.store().expect("the fixture stores fields");
    let mut scratch = Scratch::new();
    let record = stored.get(13, &mut scratch).expect("decodes");
    assert_eq!(
        record.field("path").expect("decodes"),
        Some(&b"doc/000/13.txt"[..])
    );
    let long = record
        .field("body")
        .expect("decodes")
        .expect("every document has a body")
        .len();
    assert!(
        long > 16 * 1024,
        "the long document came back short: {long}"
    );

    // And a query, end to end, against the file on disk.
    let searcher = Searcher::new(&index);
    let hits = searcher.search("rare storehouse", 5).expect("decodes");
    assert_eq!(hits.len(), 5);
    for pair in hits.windows(2) {
        assert!(
            pair[0].score >= pair[1].score,
            "hits came back out of order"
        );
    }
    // The five documents holding the rare term, in the order the scores put
    // them, which is a pin on the lengths in the norms section as much as on
    // the postings: two of them score exactly the same and the order between
    // them is the one the walk produced.
    let best: Vec<(u32, String)> = hits
        .iter()
        .map(|hit| (hit.doc, format!("{:.4}", hit.score)))
        .collect();
    let expected: Vec<(u32, String)> = [
        (0u32, "7.2597"),
        (388, "5.0598"),
        (291, "4.8950"),
        (97, "4.7407"),
        (194, "4.7407"),
    ]
    .into_iter()
    .map(|(doc, score)| (doc, score.to_owned()))
    .collect();
    assert_eq!(best, expected, "the fixture ranks differently than it did");
}
