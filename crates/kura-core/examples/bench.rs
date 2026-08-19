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

// Every cast here feeds a printed number that is already approximate, so losing
// a digit of precision on the way to a rate costs nothing.
#![allow(clippy::cast_precision_loss)]

use std::hint::black_box;
use std::time::{Duration, Instant};

use kura_core::DocId;
use kura_core::bitmap::Bitmap;
use kura_core::codec::{get_uvarint, put_uvarint};
use kura_core::posting::{Reader, Writer};
use kura_core::segment::{self, Segment};
use kura_core::vector::{Quantised, cosine};

/// How many times each measurement is repeated before the median is taken.
const ROUNDS: usize = 9;

fn main() {
    println!("{:<44} {:>12} {:>14}", "case", "median", "per item");

    let encoded = postings();
    bitmaps();
    varints();
    segments(&encoded);
    vectors();
}

/// Posting lists: how small they get and what encoding and decoding cost.
fn postings() -> Vec<u8> {
    let ids: Vec<DocId> = (0..1_000_000u32).map(|i| i * 3).collect();
    let encoded = encode(&ids);
    println!(
        "posting list: {} ids in {} bytes, {:.2} bytes per id",
        ids.len(),
        encoded.len(),
        encoded.len() as f64 / ids.len() as f64
    );

    bench("encode a million ids", ids.len(), || {
        black_box(encode(black_box(&ids)));
    });

    bench("decode a million ids", ids.len(), || {
        let reader = Reader::new(&encoded).expect("header");
        black_box(reader.to_vec().expect("decode"));
    });

    bench("membership test, one block decoded", 1, || {
        let reader = Reader::new(&encoded).expect("header");
        black_box(reader.contains(black_box(2_999_997)).expect("lookup"));
    });

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
    println!("segment: {} bytes over 2 sections", bytes.len());

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
        writer.push(*id).expect("ascending input");
    }
    writer.finish()
}

/// Runs `f` a few times and prints the median, plus the cost per item.
fn bench(name: &str, items: usize, mut f: impl FnMut()) {
    let mut timings = Vec::with_capacity(ROUNDS);
    for _ in 0..ROUNDS {
        let start = Instant::now();
        f();
        timings.push(start.elapsed());
    }
    timings.sort_unstable();
    let median = timings[ROUNDS / 2];

    let per_item = if items > 1 {
        format!("{:.1} ns", median.as_nanos() as f64 / items as f64)
    } else {
        String::new()
    };
    println!("{name:<44} {:>12} {per_item:>14}", format_duration(median));
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
