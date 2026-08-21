//! A dependency free benchmark for the primitives on the query path.
//!
//! Run it with `cargo run --release --example bench`. It is deliberately plain:
//! a timing loop and a printed table, no harness, no statistics beyond a median.
//! The crate has no dependencies and a benchmark framework is not a good enough
//! reason to acquire the first one, and the numbers that matter here are the
//! order of magnitude ones, such as whether a membership test decodes one block
//! or the whole list.
//!
//! The results are not a promise. They are a way to notice when a change makes
//! something ten times slower, which is the only kind of regression a benchmark
//! of this shape can honestly catch.
//!
//! # Machine readable results
//!
//! `cargo run --release --example bench -- --json` prints a JSON document
//! instead of the table, so a runner can diff two commits without parsing
//! columns. Every timing row carries the same numbers the table shows, and the
//! query rows carry what the query did as well, from [`kura_core::explain`].
//!
//! A time on its own cannot tell a slower query from a query that did more
//! work, and those are opposite problems. A row that got slower while its
//! postings decoded stayed flat is a scoring or a memory problem. A row that got
//! slower because it decoded twice as many postings is a pruning problem. Both
//! of those look like one number going up until the counters are beside it.

// Every cast here feeds a printed number that is already approximate, so losing
// a digit of precision on the way to a rate costs nothing.
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use std::hint::black_box;
use std::io::{self, Write};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use kura_core::DocId;
use kura_core::bitmap::Bitmap;
use kura_core::bitpack;
use kura_core::codec::{get_uvarint, put_uvarint};
use kura_core::explain::Counters;
use kura_core::index;
use kura_core::lz;
use kura_core::posting::{Reader, Writer};
use kura_core::search::Searcher;
use kura_core::segment::{self, Segment};
use kura_core::store;
use kura_core::terms;
use kura_core::vector::{Quantised, cosine, dot, normalise};

/// How many times each measurement is repeated.
///
/// Enough that the fastest round has a fair chance of being one where nothing
/// else on the machine got in the way, which is what makes the number below
/// comparable between two runs on a laptop that is doing other things.
const ROUNDS: usize = 25;

/// Everything measured so far, in the order it was measured.
///
/// A global rather than a value threaded through every measurement, because the
/// alternative is an extra parameter on forty call sites that exists only to
/// carry a result nobody reads until the end.
static REPORT: Mutex<Report> = Mutex::new(Report::new());

/// Whether the table is being printed, which it is not when JSON was asked for.
static TABLE: AtomicBool = AtomicBool::new(true);

fn main() {
    let json = std::env::args().any(|arg| arg == "--json");
    TABLE.store(!json, Ordering::Relaxed);

    if !json {
        println!(
            "{:<44} {:>12} {:>12} {:>10}",
            "case", "best", "median", "per item"
        );
    }

    let encoded = postings();
    engine();
    parallel();
    stores();
    blocks();
    dictionary();
    bitmaps();
    varints();
    segments(&encoded);
    vectors();

    if json {
        let report = REPORT.lock().expect("nothing panicked holding the report");
        let mut out = io::stdout().lock();
        report
            .write(&mut out)
            .expect("stdout takes a few kilobytes");
    }
}

/// Whether measurements are being printed as they are taken.
fn printing() -> bool {
    TABLE.load(Ordering::Relaxed)
}

/// Everything one run of this benchmark produced.
struct Report {
    /// One entry per timing, in the order they were taken.
    cases: Vec<Case>,
    /// The sizes and ratios that are facts about the data rather than timings.
    facts: Vec<Fact>,
}

impl Report {
    const fn new() -> Self {
        Self {
            cases: Vec::new(),
            facts: Vec::new(),
        }
    }

    /// Writes the whole run as JSON.
    ///
    /// Hand written, because a serialiser would be the crate's first dependency
    /// and this document is numbers and names from string literals.
    fn write(&self, out: &mut impl Write) -> io::Result<()> {
        writeln!(out, "{{")?;
        writeln!(out, "  \"rounds\": {ROUNDS},")?;
        writeln!(out, "  \"cases\": [")?;
        for (at, case) in self.cases.iter().enumerate() {
            let comma = if at + 1 == self.cases.len() { "" } else { "," };
            write!(out, "    ")?;
            case.write(out)?;
            writeln!(out, "{comma}")?;
        }
        writeln!(out, "  ],")?;
        writeln!(out, "  \"facts\": {{")?;
        for (at, fact) in self.facts.iter().enumerate() {
            let comma = if at + 1 == self.facts.len() { "" } else { "," };
            write!(out, "    \"{}\": {{", escape(&fact.name))?;
            for (field, (name, value)) in fact.fields.iter().enumerate() {
                let between = if field == 0 { "" } else { ", " };
                write!(out, "{between}\"{}\": {}", escape(name), number(*value))?;
            }
            writeln!(out, "}}{comma}")?;
        }
        writeln!(out, "  }}")?;
        writeln!(out, "}}")
    }
}

/// One timing, and what the work it timed did.
struct Case {
    name: String,
    rounds: usize,
    items: usize,
    best: Duration,
    median: Duration,
    /// What the queries in this case did, for the cases that run queries.
    counted: Option<Counted>,
}

impl Case {
    fn write(&self, out: &mut impl Write) -> io::Result<()> {
        write!(
            out,
            "{{\"name\": \"{}\", \"rounds\": {}, \"items\": {}, \"best_ns\": {}, \"median_ns\": {}",
            escape(&self.name),
            self.rounds,
            self.items,
            self.best.as_nanos(),
            self.median.as_nanos()
        )?;
        if self.items > 1 {
            let per = self.best.as_nanos() as f64 / self.items as f64;
            write!(out, ", \"ns_per_item\": {}", number(per))?;
        }
        if let Some(counted) = &self.counted {
            write!(out, ", \"counters\": ")?;
            counted.write(out)?;
        }
        write!(out, "}}")
    }
}

/// What a set of queries did, summed over the set.
///
/// Summed rather than averaged because the sums are what the ratios are made
/// of. Postings decoded against postings is the skip fraction of the whole set,
/// and an average of per query fractions would weight a query over one short
/// list the same as a query over four long ones.
struct Counted {
    counters: Counters,
    queries: usize,
}

impl Counted {
    fn write(&self, out: &mut impl Write) -> io::Result<()> {
        let c = &self.counters;
        write!(
            out,
            "{{\"queries\": {}, \"terms\": {}, \"postings\": {}, \"postings_decoded\": {}, \
             \"blocks\": {}, \"blocks_decoded\": {}, \"blocks_skipped\": {}, \
             \"documents_scored\": {}, \"seeks\": {}, \"advances\": {}, \"skipped\": {}}}",
            self.queries,
            c.terms,
            c.postings,
            c.postings_decoded,
            c.blocks,
            c.blocks_decoded,
            c.blocks_skipped,
            c.documents_scored,
            c.seeks,
            c.advances,
            number(f64::from(c.skipped()))
        )
    }
}

/// A size or a ratio that describes the data rather than how long it took.
struct Fact {
    name: String,
    fields: Vec<(&'static str, f64)>,
}

/// Records a fact, and prints the sentence version of it unless JSON was asked
/// for.
fn fact(name: &str, line: &str, fields: Vec<(&'static str, f64)>) {
    if printing() {
        println!("{line}");
    }
    REPORT
        .lock()
        .expect("nothing panicked holding the report")
        .facts
        .push(Fact {
            name: name.to_string(),
            fields,
        });
}

/// Attaches to the measurement just taken what its queries did.
///
/// Called straight after the [`bench`] whose queries these are, which is what
/// makes attaching to the last case rather than by name correct.
fn counted(queries: usize, counters: Counters) {
    if printing() {
        let scored = counters.documents_scored / queries.max(1) as u64;
        println!(
            "{:<44} skipped {:.1}% of the postings, scored {scored} documents per query",
            "",
            f64::from(counters.skipped()) * 100.0
        );
    }
    let mut report = REPORT.lock().expect("nothing panicked holding the report");
    if let Some(case) = report.cases.last_mut() {
        case.counted = Some(Counted { counters, queries });
    }
}

/// Runs every query with counting on and adds up what they all did.
///
/// A second pass rather than counting during the timed one, so that the timing
/// measures the search the engine actually ships.
fn tally<S: AsRef<str>>(queries: &[S], mut run: impl FnMut(&str) -> Counters) -> Counters {
    let mut total = Counters::default();
    for query in queries {
        let one = run(query.as_ref());
        total.terms += one.terms;
        total.postings += one.postings;
        total.blocks += one.blocks;
        total.blocks_decoded += one.blocks_decoded;
        total.blocks_skipped += one.blocks_skipped;
        total.postings_decoded += one.postings_decoded;
        total.documents_scored += one.documents_scored;
        total.seeks += one.seeks;
        total.advances += one.advances;
    }
    total
}

/// A number that is always valid JSON.
///
/// A ratio over an empty input would be a division by zero, and `NaN` is not a
/// JSON value. Nothing here divides by zero today, and a document that silently
/// stopped parsing later would be a bad way to find out that changed.
fn number(value: f64) -> String {
    if value.is_finite() {
        format!("{value}")
    } else {
        "0".to_string()
    }
}

/// A JSON string body.
///
/// Every name here is an ASCII literal, so this exists to keep that true rather
/// than because anything currently needs it.
fn escape(text: &str) -> String {
    use std::fmt::Write as _;

    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

/// The stored fields, which are most of what a segment weighs.
///
/// Three numbers. What the text costs on disk once it is blocked and
/// compressed, because that is the whole reason the store is not a flat array
/// of records. What it costs to get one document back, which is the price the
/// query path pays for that. And the raw compression rate, which is what the
/// first two are made of.
///
/// The corpus is generated, and generated text compresses better than real text
/// because the vocabulary is smaller. The ratio here is a ceiling. The one that
/// counts is measured against real documents in the benchmark suite.
fn stores() {
    let corpus = corpus(50_000);
    let raw: usize = corpus.iter().map(String::len).sum();

    let mut size = 0;
    bench_rounds("store fifty thousand documents", 3, corpus.len(), || {
        let mut writer = store::Writer::new();
        for text in &corpus {
            writer
                .push([("body", text.as_bytes())])
                .expect("fifty thousand documents fit");
        }
        let bytes = writer.finish().expect("what was written fits");
        size = bytes.len();
        black_box(&bytes);
    });

    let mut writer = store::Writer::new();
    for text in &corpus {
        writer.push([("body", text.as_bytes())]).expect("fits");
    }
    let bytes = writer.finish().expect("fits");
    let reader = store::Reader::new(&bytes).expect("what was written reads");
    fact(
        "store",
        &format!(
            "store: {:.1} MB of text in {:.1} MB, {:.2} of the input, {} blocks",
            raw as f64 / 1e6,
            size as f64 / 1e6,
            size as f64 / raw as f64,
            reader.blocks()
        ),
        vec![
            ("text_bytes", raw as f64),
            ("store_bytes", size as f64),
            ("ratio", size as f64 / raw as f64),
            ("blocks", reader.blocks() as f64),
        ],
    );

    // The order is the one a hit list arrives in, which is scattered, so every
    // lookup pays for a block that is not the one already in hand. Reading a
    // page of ten hits that landed near each other is cheaper than this.
    let mut scratch = store::Scratch::new();
    let mut doc = 0usize;
    bench("read one stored document at random", corpus.len(), || {
        for _ in 0..corpus.len() {
            doc = (doc + 30_011) % corpus.len();
            let record = reader
                .get(doc as DocId, &mut scratch)
                .expect("the document is there");
            black_box(record.field("body").expect("decodes"));
        }
    });

    let text: String = corpus[..2_000].concat();
    let mut compressed = Vec::new();
    let mut compressor = lz::Compressor::new();
    bench("compress a megabyte of text", text.len(), || {
        compressed.clear();
        compressor.compress(text.as_bytes(), &mut compressed);
        black_box(&compressed);
    });
    let mut back = Vec::new();
    bench("decompress it again", text.len(), || {
        back.clear();
        lz::decompress(&compressed, text.len(), &mut back).expect("what was written decodes");
        black_box(&back);
    });
    fact(
        "compress",
        &format!(
            "compress: {:.1} MB into {:.1} MB, {:.2} of the input",
            text.len() as f64 / 1e6,
            compressed.len() as f64 / 1e6,
            compressed.len() as f64 / text.len() as f64,
        ),
        vec![
            ("input_bytes", text.len() as f64),
            ("output_bytes", compressed.len() as f64),
            ("ratio", compressed.len() as f64 / text.len() as f64),
        ],
    );
}

/// The term dictionary, which every query walks once per term before it reads a
/// single posting.
///
/// Two numbers matter. How many bytes a term costs, because the dictionary is
/// the part of a segment that a query touches all of and so the part that wants
/// to be resident. And what a lookup costs, including the misses, which is why
/// the lookups below are spread across the vocabulary rather than hammering one
/// term that would sit in cache after the first round.
fn dictionary() {
    let words = vocabulary(200_000);
    let mut writer = terms::Writer::new();
    let mut offset = 0u64;
    for (i, word) in words.iter().enumerate() {
        let len = (i as u64 % 64) + 8;
        writer
            .push(
                word.as_bytes(),
                terms::Entry {
                    docs: u32::try_from(i % 5_000).expect("under five thousand") + 1,
                    offset,
                    len,
                },
            )
            .expect("ascending input");
        offset += len;
    }
    let encoded = writer.finish();
    let raw: usize = words.iter().map(String::len).sum();
    fact(
        "term_dictionary",
        &format!(
            "term dictionary: {} terms in {} bytes, {:.2} bytes per term, {:.2} raw",
            words.len(),
            encoded.len(),
            encoded.len() as f64 / words.len() as f64,
            raw as f64 / words.len() as f64
        ),
        vec![
            ("terms", words.len() as f64),
            ("bytes", encoded.len() as f64),
            ("bytes_per_term", encoded.len() as f64 / words.len() as f64),
            ("raw_bytes_per_term", raw as f64 / words.len() as f64),
        ],
    );

    // Every sixteenth term, so the walk lands in a different block each time and
    // the run covers the whole dictionary rather than one warm corner of it.
    let probes: Vec<&str> = words.iter().step_by(16).map(String::as_str).collect();
    let reader = terms::Reader::new(&encoded).expect("header");
    bench("look up terms across the dictionary", probes.len(), || {
        let mut found = 0usize;
        for probe in &probes {
            if reader
                .get(black_box(probe.as_bytes()))
                .expect("lookup")
                .is_some()
            {
                found += 1;
            }
        }
        black_box(found);
    });

    let absent: Vec<String> = probes.iter().map(|p| format!("{p}x")).collect();
    bench("look up terms that are not there", absent.len(), || {
        let mut found = 0usize;
        for probe in &absent {
            if reader
                .get(black_box(probe.as_bytes()))
                .expect("lookup")
                .is_some()
            {
                found += 1;
            }
        }
        black_box(found);
    });
}

/// A vocabulary shaped like a real one: a few thousand stems, each carried
/// through a set of endings, so consecutive terms share a long prefix the way
/// "configure", "configured" and "configuration" do.
fn vocabulary(count: usize) -> Vec<String> {
    const STEMS: [&str; 16] = [
        "config",
        "index",
        "search",
        "storage",
        "document",
        "cluster",
        "process",
        "connect",
        "transact",
        "compress",
        "distribute",
        "authent",
        "replicat",
        "aggregat",
        "normalis",
        "serialis",
    ];
    const ENDINGS: [&str; 8] = ["", "ed", "es", "ing", "ion", "or", "able", "ility"];

    let mut out = Vec::with_capacity(count);
    let mut n = 0usize;
    while out.len() < count {
        for stem in STEMS {
            for ending in ENDINGS {
                if out.len() == count {
                    break;
                }
                out.push(format!("{stem}{ending}{n:05}"));
            }
        }
        n += 1;
    }
    out.sort();
    out
}

/// Posting lists: how small they get and what encoding and decoding cost.
fn postings() -> Vec<u8> {
    let ids: Vec<DocId> = (0..1_000_000u32).map(|i| i * 3).collect();
    let encoded = encode(&ids);
    fact(
        "posting_list",
        &format!(
            "posting list: {} ids in {} bytes, {:.2} bytes per id",
            ids.len(),
            encoded.len(),
            encoded.len() as f64 / ids.len() as f64
        ),
        vec![
            ("ids", ids.len() as f64),
            ("bytes", encoded.len() as f64),
            ("bytes_per_id", encoded.len() as f64 / ids.len() as f64),
        ],
    );

    bench("encode a million ids", ids.len(), || {
        black_box(encode(black_box(&ids)));
    });

    bench("decode a million ids", ids.len(), || {
        let reader = Reader::new(&encoded).expect("header");
        black_box(reader.to_vec().expect("decode"));
    });

    // A thousand of them, spread across the list, rather than one. A single
    // lookup on a cold cache is mostly a measurement of what the benchmark
    // before it left in L2, and it moves by a factor of two between runs for
    // that reason alone.
    let probes: Vec<DocId> = (0..1_000u32).map(|i| i * 2_999).collect();
    bench("membership tests, one block each", probes.len(), || {
        let reader = Reader::new(&encoded).expect("header");
        let mut found = 0usize;
        for probe in &probes {
            if reader.contains(black_box(*probe)).expect("lookup") {
                found += 1;
            }
        }
        black_box(found);
    });

    // What a scorer actually does: walk the list a posting at a time, reading
    // the frequency of each. It costs more than `to_vec` because it decodes both
    // streams and hands the values out one at a time rather than in blocks.
    bench(
        "walk a million postings with frequencies",
        ids.len(),
        || {
            let reader = Reader::new(&encoded).expect("header");
            let mut cursor = reader.cursor();
            let mut total = 0u64;
            while let Some(doc) = cursor.advance().expect("decode") {
                total += u64::from(doc) + u64::from(cursor.frequency());
            }
            black_box(total);
        },
    );

    // What an intersection does: a cursor on the long list, driven by ids from
    // a short one, jumping blocks rather than walking them. A thousand seeks
    // spread over a million postings, so almost every one crosses blocks.
    let targets: Vec<DocId> = (0..1_000u32).map(|i| i * 2_999).collect();
    bench(
        "seek a thousand times into a million",
        targets.len(),
        || {
            let reader = Reader::new(&encoded).expect("header");
            let mut cursor = reader.cursor();
            let mut found = 0u64;
            for target in &targets {
                if let Some(doc) = cursor.seek(black_box(*target)).expect("decode") {
                    found += u64::from(doc);
                }
            }
            black_box(found);
        },
    );

    encoded
}

/// The permission filter, which is a bitmap intersection and nothing else.
fn bitmaps() {
    let dense: Bitmap = (0..1_000_000u32).collect();
    let every_third: Bitmap = (0..1_000_000u32).filter(|i| i % 3 == 0).collect();
    bench("intersect two dense bitmaps", 1_000_000, || {
        let mut left = dense.clone();
        left.intersect_with(black_box(&every_third));
        black_box(left.len());
    });

    let sparse: Bitmap = (0..1_000u32).map(|i| i * 977).collect();
    bench(
        "intersect a sparse bitmap into a dense one",
        1_000_000,
        || {
            let mut left = dense.clone();
            left.intersect_with(black_box(&sparse));
            black_box(left.len());
        },
    );
}

/// The two block codecs side by side, on the same ids.
///
/// This is the comparison the posting format rests on, so it is measured rather
/// than argued about. Both do the same job: turn a run of ascending ids into
/// bytes and back. One reads a value at a time and has to finish each before it
/// knows where the next one starts, the other reads four at a time at a width it
/// was told in advance.
fn blocks() {
    let ids: Vec<DocId> = (0..1_000_000u32).map(|i| i * 3).collect();

    // One continuous run of gaps, which is the best shape a varint decoder can
    // be given. Restarting the run every block would make the comparison below
    // flattering for no reason.
    let mut varint = Vec::new();
    let mut previous = 0u32;
    for id in &ids {
        put_uvarint(&mut varint, u64::from(*id - previous));
        previous = *id;
    }

    let mut packed = Vec::new();
    let mut widths = Vec::new();
    let mut base = 0u32;
    for chunk in ids.chunks(bitpack::BLOCK) {
        let mut block = [0u32; bitpack::BLOCK];
        block[..chunk.len()].copy_from_slice(chunk);
        for slot in &mut block[chunk.len()..] {
            *slot = chunk[chunk.len() - 1];
        }
        widths.push(bitpack::pack(&block, base, &mut packed));
        base = block[bitpack::BLOCK - 1];
    }
    fact(
        "block_codecs",
        &format!(
            "block codecs: varint {} bytes, packed {} bytes, {:.2} against {:.2} bytes per id",
            varint.len(),
            packed.len(),
            varint.len() as f64 / ids.len() as f64,
            packed.len() as f64 / ids.len() as f64
        ),
        vec![
            ("varint_bytes", varint.len() as f64),
            ("packed_bytes", packed.len() as f64),
            (
                "varint_bytes_per_id",
                varint.len() as f64 / ids.len() as f64,
            ),
            (
                "packed_bytes_per_id",
                packed.len() as f64 / ids.len() as f64,
            ),
        ],
    );

    bench("decode a million gaps, varints", ids.len(), || {
        let mut out = Vec::with_capacity(ids.len());
        let mut rest = black_box(varint.as_slice());
        let mut current = 0u32;
        while !rest.is_empty() {
            let (gap, tail) = get_uvarint(rest).expect("decode");
            current += u32::try_from(gap).expect("gap fits");
            out.push(current);
            rest = tail;
        }
        black_box(out.len());
    });

    bench("decode a million ids, packed blocks", ids.len(), || {
        let mut out = Vec::with_capacity(ids.len());
        let mut block = [0u32; bitpack::BLOCK];
        let mut rest = black_box(packed.as_slice());
        let mut base = 0u32;
        for width in &widths {
            let read = bitpack::unpack(rest, *width, base, &mut block).expect("unpack");
            out.extend_from_slice(&block);
            base = block[bitpack::BLOCK - 1];
            rest = &rest[read..];
        }
        black_box(out.len());
    });
}

/// The codec everything else is built out of.
fn varints() {
    let values: Vec<u64> = (0..1_000_000u64).map(|i| i * 7).collect();
    bench("varint round trip", values.len(), || {
        let mut buffer = Vec::with_capacity(values.len() * 2);
        for value in &values {
            put_uvarint(&mut buffer, *value);
        }
        let mut rest = buffer.as_slice();
        while !rest.is_empty() {
            let (value, tail) = get_uvarint(rest).expect("decode");
            black_box(value);
            rest = tail;
        }
    });
}

/// The two numbers that decide whether checksumming on open is affordable.
///
/// Opening is meant to be a walk of the section table and nothing else, so it
/// should not move with the size of the segment, and verifying should move
/// exactly linearly with it. If those two ever converge, the fast path has
/// stopped being a fast path.
fn segments(postings: &[u8]) {
    let mut writer = segment::Writer::new();
    writer
        .add(segment::kind::POSTINGS, postings.to_vec())
        .expect("one of a kind");
    writer
        .add(segment::kind::TERMS, vec![0x5a; 1 << 20])
        .expect("one of a kind");
    let bytes = writer.finish();
    fact(
        "segment",
        &format!("segment: {} bytes over 2 sections", bytes.len()),
        vec![("bytes", bytes.len() as f64), ("sections", 2.0)],
    );

    bench("open a segment, checksum verified", bytes.len(), || {
        let opened = Segment::open(black_box(&bytes)).expect("open");
        black_box(opened.section(segment::kind::TERMS));
    });

    bench("open a segment, checksum skipped", 1, || {
        let opened = Segment::open_without_checksum(black_box(&bytes)).expect("open");
        black_box(opened.section(segment::kind::TERMS));
    });
}

/// Similarity, at full width and quantised, which is the memory for time trade
/// the vector layer exists to make.
fn vectors() {
    let query: Vec<f32> = (0..768).map(|i| ((i % 17) as f32 / 17.0) - 0.5).collect();
    let corpus: Vec<Vec<f32>> = (0..10_000)
        .map(|n| {
            (0..768)
                .map(|i| (((i + n) % 23) as f32 / 23.0) - 0.5)
                .collect()
        })
        .collect();

    bench("cosine over ten thousand f32 vectors", corpus.len(), || {
        let mut best = f32::MIN;
        for candidate in &corpus {
            let score = cosine(black_box(&query), candidate).expect("same length");
            if score > best {
                best = score;
            }
        }
        black_box(best);
    });

    let mut unit_query = query.clone();
    normalise(&mut unit_query);
    let unit: Vec<Vec<f32>> = corpus
        .iter()
        .map(|v| {
            let mut v = v.clone();
            normalise(&mut v);
            v
        })
        .collect();
    bench(
        "dot over ten thousand normalised f32 vectors",
        unit.len(),
        || {
            let mut best = f32::MIN;
            for candidate in &unit {
                let score = dot(black_box(&unit_query), candidate).expect("same length");
                if score > best {
                    best = score;
                }
            }
            black_box(best);
        },
    );

    let quantised_query = Quantised::from_f32(&query);
    let quantised: Vec<Quantised> = corpus.iter().map(|v| Quantised::from_f32(v)).collect();
    bench(
        "dot over ten thousand quantised vectors",
        quantised.len(),
        || {
            let mut best = f32::MIN;
            for candidate in &quantised {
                let score = quantised_query
                    .dot(black_box(candidate))
                    .expect("same length");
                if score > best {
                    best = score;
                }
            }
            black_box(best);
        },
    );
}

fn encode(ids: &[DocId]) -> Vec<u8> {
    let mut writer = Writer::new();
    for id in ids {
        writer.push(*id, 1).expect("ascending input");
    }
    writer.finish()
}

/// Runs `f` a few times and prints the median, plus the cost per item.
/// Runs `f` a few times and prints the fastest round and the median, plus the
/// cost per item at the fastest.
///
/// Both, because they answer different questions. The median is what the work
/// costs on the machine as it actually is, sharing it with everything else that
/// is running. The fastest round is the closest this can get to what the work
/// costs on its own, and it is the one to compare against another build, because
/// a change that made something slower should not be able to hide behind a busy
/// afternoon.
fn bench(name: &str, items: usize, f: impl FnMut()) {
    bench_rounds(name, ROUNDS, items, f);
}

/// The same, with a round count of the caller's choosing, for work that is too
/// slow to repeat twenty five times.
fn bench_rounds(name: &str, rounds: usize, items: usize, mut f: impl FnMut()) {
    let mut timings = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        let start = Instant::now();
        f();
        timings.push(start.elapsed());
    }
    timings.sort_unstable();
    let best = timings[0];
    let median = timings[rounds / 2];

    if printing() {
        let per_item = if items > 1 {
            format!("{:.2} ns", best.as_nanos() as f64 / items as f64)
        } else {
            String::new()
        };
        println!(
            "{name:<44} {:>12} {:>12} {per_item:>10}",
            format_duration(best),
            format_duration(median)
        );
    }

    REPORT
        .lock()
        .expect("nothing panicked holding the report")
        .cases
        .push(Case {
            name: name.to_string(),
            rounds,
            items,
            best,
            median,
            counted: None,
        });
}

fn format_duration(d: Duration) -> String {
    let nanos = d.as_nanos();
    if nanos < 10_000 {
        format!("{nanos} ns")
    } else if nanos < 10_000_000 {
        format!("{:.1} us", nanos as f64 / 1_000.0)
    } else {
        format!("{:.1} ms", nanos as f64 / 1_000_000.0)
    }
}

/// The whole engine, end to end: text in, a segment out, and queries against it.
///
/// The two numbers this exists for are how fast text turns into an index and how
/// long a query takes once it is one. Everything above measures a piece in
/// isolation, which is useful for catching a regression and useless for knowing
/// whether the thing works, because a query spends its time in the interaction
/// between the dictionary, the skip table and the scorer rather than in any one
/// of them.
///
/// The corpus is generated rather than real, and generated text is kinder than
/// real text: the vocabulary is smaller and the term distribution is smoother.
/// Read these as a floor for regressions rather than as a claim about a corpus.
/// The numbers against real documents and against other engines live in the
/// benchmark suite, which is a separate repository because it has to depend on
/// the engines it compares against.
fn engine() {
    let corpus = corpus(50_000);
    let raw: usize = corpus.iter().map(String::len).sum();
    let mut size = 0;
    bench_rounds("index fifty thousand documents", 3, corpus.len(), || {
        let mut writer = index::Writer::new();
        for text in &corpus {
            writer.add(text).expect("fifty thousand documents fit");
        }
        let bytes = writer.finish().expect("what was written decodes");
        size = bytes.len();
        black_box(&bytes);
    });

    let mut writer = index::Writer::new();
    for text in &corpus {
        writer.add(text).expect("fifty thousand documents fit");
    }
    let bytes = writer.finish().expect("what was written decodes");
    let segment = Segment::open(&bytes).expect("a segment this writer wrote opens");
    let reader = index::Reader::open(&segment).expect("the sections are all there");
    fact(
        "index",
        &format!(
            "index: {} documents, {:.1} MB of text in {:.1} MB, {:.2} of the input, {} terms",
            corpus.len(),
            raw as f64 / 1e6,
            size as f64 / 1e6,
            size as f64 / raw as f64,
            reader.terms()
        ),
        vec![
            ("documents", corpus.len() as f64),
            ("text_bytes", raw as f64),
            ("index_bytes", size as f64),
            ("ratio", size as f64 / raw as f64),
            ("terms", f64::from(reader.terms())),
        ],
    );

    queries(&Searcher::new(&reader));
}

/// The query side of [`engine`], which is where the pruning either works or does
/// not.
///
/// Separate from the indexing side because they are read separately: a change to
/// the writer moves the rows above and a change to the walk moves the rows here.
fn queries(searcher: &Searcher<'_, '_>) {
    // A query is only interesting if it has to choose. One term is a walk down
    // one list, and three terms with one of them common is where the pruning
    // either works or does not.
    let words = vocabulary(4_000);
    let one: Vec<&str> = words.iter().step_by(37).map(String::as_str).collect();
    let two: Vec<String> = one
        .chunks(2)
        .filter(|pair| pair.len() == 2)
        .map(|pair| format!("{} {}", pair[0], pair[1]))
        .collect();
    let three: Vec<String> = one
        .chunks(2)
        .filter(|pair| pair.len() == 2)
        .map(|pair| format!("{} {} {}", pair[0], pair[1], words[0]))
        .collect();

    // Every query row is timed and then run once more with counting on, so the
    // row carries what the work was as well as what it cost. A row that gets
    // slower with its counters unchanged and a row that gets slower because it
    // decoded twice as much are different bugs.
    bench("query one term, top ten", one.len(), || {
        for query in &one {
            black_box(searcher.search(query, 10).expect("searches"));
        }
    });
    counted(
        one.len(),
        tally(&one, |query| {
            searcher.search_explained(query, 10).expect("searches").1
        }),
    );

    bench("query two terms, top ten", two.len(), || {
        for query in &two {
            black_box(searcher.search(query, 10).expect("searches"));
        }
    });
    counted(
        two.len(),
        tally(&two, |query| {
            searcher.search_explained(query, 10).expect("searches").1
        }),
    );

    bench("query three terms, top ten", three.len(), || {
        for query in &three {
            black_box(searcher.search(query, 10).expect("searches"));
        }
    });
    counted(
        three.len(),
        tally(&three, |query| {
            searcher.search_explained(query, 10).expect("searches").1
        }),
    );

    bench("query three terms, top hundred", three.len(), || {
        for query in &three {
            black_box(searcher.search(query, 100).expect("searches"));
        }
    });
    counted(
        three.len(),
        tally(&three, |query| {
            searcher.search_explained(query, 100).expect("searches").1
        }),
    );

    // What a search box actually asks for, which is a page and a count of what
    // it is a page of. The two calls are what the one replaces, so they are
    // here beside it rather than in a note.
    bench("page and total, one walk", three.len(), || {
        for query in &three {
            black_box(searcher.search_and_count(query, 10).expect("searches"));
        }
    });
    counted(
        three.len(),
        tally(&three, |query| {
            searcher
                .search_and_count_explained(query, 10)
                .expect("searches")
                .2
        }),
    );

    bench("page and total, two walks", three.len(), || {
        for query in &three {
            black_box(searcher.count(query).expect("counts"));
            black_box(searcher.search(query, 10).expect("searches"));
        }
    });
    counted(
        three.len(),
        tally(&three, |query| {
            let counting = searcher.count_explained(query).expect("counts").1;
            let searching = searcher.search_explained(query, 10).expect("searches").1;
            // Two walks over the same lists, so the denominators add up the
            // same way the work does.
            Counters {
                terms: counting.terms + searching.terms,
                postings: counting.postings + searching.postings,
                blocks: counting.blocks + searching.blocks,
                blocks_decoded: counting.blocks_decoded + searching.blocks_decoded,
                blocks_skipped: counting.blocks_skipped + searching.blocks_skipped,
                postings_decoded: counting.postings_decoded + searching.postings_decoded,
                documents_scored: counting.documents_scored + searching.documents_scored,
                seeks: counting.seeks + searching.seeks,
                advances: counting.advances + searching.advances,
            }
        }),
    );
}

/// What the fold buys, which is the whole reason it exists.
///
/// The same corpus is indexed on one thread and then on several, and the
/// segments are compared byte for byte, because a build that goes wide and
/// comes out different is not a faster build of the same thing.
///
/// The speedup is under the thread count and always will be. The fold itself is
/// serial, and so is the write of the term dictionary, so the parts that go wide
/// are the analysis and the posting chains. Those are most of the time, which is
/// why this is worth doing at all.
fn parallel() {
    let corpus = corpus(50_000);
    let threads = std::thread::available_parallelism().map_or(4, std::num::NonZero::get);

    let mut one = Vec::new();
    let mut serial = Duration::MAX;
    for _ in 0..3 {
        let start = Instant::now();
        let mut writer = index::Writer::new();
        for text in &corpus {
            writer.add(text).expect("fifty thousand documents fit");
        }
        one = writer.finish().expect("what was written decodes");
        serial = serial.min(start.elapsed());
    }
    fact(
        "index_on_one_thread",
        &format!(
            "index on one thread: {} in {}",
            corpus.len(),
            format_duration(serial)
        ),
        vec![
            ("documents", corpus.len() as f64),
            ("nanos", serial.as_nanos() as f64),
        ],
    );

    let mut widths = vec![2, 4, threads];
    widths.retain(|width| *width <= threads);
    widths.sort_unstable();
    widths.dedup();
    for width in widths {
        // Ceiling division, so the last slice is the short one rather than
        // there being a slice more than there are threads.
        let slice = corpus.len().div_ceil(width);
        let mut best = Duration::MAX;
        let mut many = Vec::new();
        for _ in 0..3 {
            let start = Instant::now();
            let parts = std::thread::scope(|scope| {
                let handles: Vec<_> = corpus
                    .chunks(slice)
                    .map(|slice| {
                        scope.spawn(move || {
                            let mut part = index::Writer::new();
                            for text in slice {
                                part.add(text).expect("a slice of the corpus fits");
                            }
                            part
                        })
                    })
                    .collect();
                handles
                    .into_iter()
                    .map(|handle| handle.join().expect("a writer thread does not panic"))
                    .collect()
            });
            many = index::Writer::concat(parts).expect("the parts fold");
            best = best.min(start.elapsed());
        }
        assert_eq!(one, many, "a segment folded on {width} threads differs");
        fact(
            &format!("index_on_{width}_threads"),
            &format!(
                "index on {width} threads: {} in {}, {:.2}x",
                corpus.len(),
                format_duration(best),
                serial.as_secs_f64() / best.as_secs_f64()
            ),
            vec![
                ("documents", corpus.len() as f64),
                ("nanos", best.as_nanos() as f64),
                ("speedup", serial.as_secs_f64() / best.as_secs_f64()),
            ],
        );
    }
}

/// Documents made of words drawn from a heavy tailed distribution.
///
/// Uniform words would make every posting list the same length, and a query
/// planner that skips has nothing to skip when every term is equally common.
/// Drawing the rank log uniformly gets the shape real text has, where a handful
/// of words are in nearly every document and most words are in almost none.
fn corpus(count: usize) -> Vec<String> {
    let words = vocabulary(30_000);
    let mut state = 0x2545_f491_4f6c_dd1d_u64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let mut out = Vec::with_capacity(count);
    let mut text = String::with_capacity(8_192);
    for _ in 0..count {
        text.clear();
        let length = 60 + (next() % 540) as usize;
        for _ in 0..length {
            let u = (next() >> 11) as f64 / (1_u64 << 53) as f64;
            let rank = ((words.len() as f64).powf(u) - 1.0) as usize;
            text.push_str(&words[rank.min(words.len() - 1)]);
            text.push(' ');
        }
        out.push(text.clone());
    }
    out
}
