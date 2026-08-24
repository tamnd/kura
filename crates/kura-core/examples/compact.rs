//! What folding every segment of a real store into one costs.
//!
//! Run it with `cargo run --release --example compact -- <store>`. It opens the
//! store, merges every segment in it into a single segment, times that, and
//! checks the result answers what the store answered. Nothing is written back:
//! the store is opened for reading and the merged segment is built in memory and
//! dropped, because swapping it in is the store's job and this is here to say
//! what the fold itself costs.
//!
//! It prints counters and sizes only. No key, no term and no document goes to
//! the terminal, because the stores this is pointed at hold somebody's files.

// Every cast here feeds a printed number that is already approximate.
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use std::time::Instant;

use kura_core::compact::{Source, merge};
use kura_core::file::Store;
use kura_core::search::Searcher;

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        println!("usage: compact <store>");
        return;
    };
    // How many terms of the merged dictionary are checked against the store.
    let checking: usize = args
        .next()
        .and_then(|count| count.parse().ok())
        .unwrap_or(2_000);

    let store = Store::open(std::path::Path::new(&path)).expect("a store");
    let view = store.view().expect("a view");
    let manifest = store.manifest();

    let mut sources = Vec::with_capacity(view.len());
    let mut bytes = 0u64;
    let mut terms = 0u64;
    for at in 0..view.len() {
        let segment = view.bytes(at).expect("the descriptor was checked");
        bytes += segment.len() as u64;
        let deleted = view.deleted(at).expect("the deletions decode");
        let source = Source::new(segment, deleted).expect("a segment this build can merge");
        terms += u64::from(source.reader().terms());
        sources.push(source);
    }

    println!("store               {path}");
    println!("segments            {:>12}", sources.len());
    println!("documents           {:>12}", manifest.total);
    println!("  live              {:>12}", manifest.live);
    println!("segment bytes       {bytes:>12}");
    println!("terms               {terms:>12}");

    let start = Instant::now();
    let merged = merge(&sources).expect("a merge");
    let took = start.elapsed();
    let built = merged.segment.size() as u64;

    println!("merge wall          {:>12.2} s", took.as_secs_f32());
    println!(
        "merge rate          {:>12.0} docs/s",
        f64::from(merged.documents) / f64::from(took.as_secs_f32())
    );
    println!("merged documents    {:>12}", merged.documents);
    println!("  left behind       {:>12}", merged.dropped);
    println!("merged terms        {:>12}", merged.terms);
    println!("merged bytes        {built:>12}");
    println!(
        "  of the sources    {:>11.1} %",
        built as f64 * 100.0 / bytes as f64
    );

    // The merged segment has to answer what the store answered. Terms are taken
    // out of the merged dictionary and asked of both sides, and neither the
    // terms nor the documents are printed.
    let laid_out = merged.segment.finish();
    let segment = kura_core::segment::Segment::open(&laid_out).expect("what was written reads");
    let index = kura_core::index::Reader::open(&segment).expect("an index");
    let readers = view.readers().expect("every segment opens");
    let store_side = Searcher::over(readers).expect("a searcher");
    let merged_side = Searcher::new(&index);

    let mut walk = index.entries();
    let mut checked = 0usize;
    let mut disagreed = 0usize;
    while checked < checking {
        let Some((term, _)) = walk.next_term().expect("the dictionary decodes") else {
            break;
        };
        let term = term.to_vec();
        let before = store_side
            .count_terms(&[term.as_slice()])
            .expect("the store answers");
        let after = merged_side
            .count_terms(&[term.as_slice()])
            .expect("the merge answers");
        if before != after {
            disagreed += 1;
        }
        checked += 1;
    }
    println!("terms checked       {checked:>12}");
    println!("  disagreed         {disagreed:>12}");
    assert_eq!(disagreed, 0, "the merged segment answers differently");

    // Every key the merged segment carries has to name the document it was
    // carried across with, and that document has to be the one whose stored
    // fields hold the key. That is the pair a store resolves a key through, and
    // a merge that renumbered one and not the other would still answer queries.
    if let Some(keys) = index.keys() {
        let mut scratch = kura_core::store::Scratch::new();
        let mut resolved = 0usize;
        let mut stored = 0usize;
        for (key, doc) in keys.table().entries() {
            if index.document(key) == Some(doc) {
                resolved += 1;
            }
            let Some(fields) = index.store() else {
                continue;
            };
            let mut document = fields
                .get(doc, &mut scratch)
                .expect("the document is there");
            while let Some((_, value)) = document.next_field().expect("the fields decode") {
                if value == key {
                    stored += 1;
                    break;
                }
            }
        }
        println!("merged keys         {:>12}", keys.len());
        println!("  resolved          {resolved:>12}");
        println!("  found in fields   {stored:>12}");
        assert_eq!(resolved, keys.len(), "a key stopped resolving");
    }
}
