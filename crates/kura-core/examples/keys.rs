//! What a primary key index costs, measured on real keys.
//!
//! Run it with `cargo run --release --example keys -- <directory>`, and it uses
//! the paths of the files under that directory as keys. A directory is not the
//! only source of real keys, but it is one every machine has, and paths have the
//! properties that make key data awkward: they are long, they share prefixes,
//! they are unevenly distributed, and the differences between them are at the
//! end rather than the beginning. Keys made up by a loop have none of that, and
//! a filter measured on made up keys is measuring its hash function rather than
//! its own accuracy.
//!
//! Half of the paths go in and the other half are looked up, so the false
//! positive rate is measured against keys that are as real as the ones that were
//! inserted and, being from the same directory, are as similar to them as
//! anything is going to be.
//!
//! What it prints is the size of the two structures, the rate the filter is
//! actually wrong at, and what a lookup costs on a hit and on a miss.

// Every cast here feeds a printed number that is already approximate.
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::time::Instant;

use kura_core::DocId;
use kura_core::filter;
use kura_core::keys;

fn main() {
    let mut args = std::env::args().skip(1);
    let root = args.next().unwrap_or_else(|| ".".to_string());
    let mut found = Vec::new();
    walk(Path::new(&root), &mut found);
    // Sorted as bytes rather than as paths, because a path sorts by its parts
    // and a key sorts by its bytes, and it is the key order the table is in.
    let mut paths: Vec<Vec<u8>> = found
        .iter()
        .map(|path| path.to_string_lossy().into_owned().into_bytes())
        .collect();
    paths.sort();
    paths.dedup();
    if paths.len() < 4 {
        println!("{root} holds {} files, which is not enough", paths.len());
        return;
    }

    // Every other path, so that the two halves are drawn from the same
    // directories rather than one half being a subtree the other never sees.
    let mut held: Vec<Vec<u8>> = Vec::new();
    let mut absent: Vec<Vec<u8>> = Vec::new();
    for (n, key) in paths.into_iter().enumerate() {
        if n % 2 == 0 { &mut held } else { &mut absent }.push(key);
    }

    let mut table = keys::Writer::new();
    for (doc, key) in held.iter().enumerate() {
        table.push(key, doc as DocId).expect("the paths are sorted");
    }
    let table = table.finish().expect("a table");

    let mut bits = filter::Writer::new(held.len());
    for key in &held {
        bits.insert(key);
    }
    let bits = bits.finish();

    let reader = keys::Reader::new(&table).expect("a table");
    let filter = filter::Reader::new(&bits).expect("a filter");

    let bytes: usize = held.iter().map(Vec::len).sum();
    println!("keys                {:>12}", held.len());
    println!(
        "mean key            {:>12.1} bytes",
        bytes as f32 / held.len() as f32
    );
    println!("table               {:>12} bytes", table.len());
    println!(
        "  per key           {:>12.1} bytes",
        table.len() as f32 / held.len() as f32
    );
    println!("filter              {:>12} bytes", bits.len());
    println!("  per key           {:>12.2} bits", filter.bits_per_key());

    // The rate the filter is wrong at, on keys it has never seen. The formula
    // says about one in a hundred at ten bits a key and a blocked filter is a
    // little worse, so this is the number that decides whether the shape was
    // worth it.
    let wrong = absent.iter().filter(|key| filter.maybe_holds(key)).count();
    println!(
        "false positives     {:>12} of {} ({:.2}%)",
        wrong,
        absent.len(),
        100.0 * wrong as f32 / absent.len() as f32
    );
    // And the check that matters more: it is never wrong the other way.
    let missed = held.iter().filter(|key| !filter.maybe_holds(key)).count();
    println!("false negatives     {missed:>12}");

    let hit = time(&held, |key| {
        black_box(reader.get(key).is_some());
    });
    let miss = time(&absent, |key| {
        black_box(reader.get(key).is_some());
    });
    let probe = time(&absent, |key| {
        black_box(filter.maybe_holds(key));
    });
    println!("lookup, hit         {hit:>12.1} ns");
    println!("lookup, miss        {miss:>12.1} ns");
    println!("filter, miss        {probe:>12.1} ns");
    // What a store of ten segments pays for a key that is in one of them: nine
    // filters that mostly say no, and the searches the ones that say yes cost.
    let rate = wrong as f32 / absent.len() as f32;
    println!(
        "ten segments, hit   {:>12.1} ns",
        hit + 9.0 * (probe + rate * miss)
    );
}

/// The mean nanoseconds one call takes, over three rounds of every key.
fn time(keys: &[Vec<u8>], mut each: impl FnMut(&[u8])) -> f32 {
    let mut best = f32::MAX;
    for _ in 0..3 {
        let start = Instant::now();
        for key in keys {
            each(key);
        }
        let taken = start.elapsed().as_nanos() as f32 / keys.len() as f32;
        best = best.min(taken);
    }
    best
}

/// Every file under `at`, following directories and nothing else.
fn walk(at: &Path, into: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(at) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => walk(&path, into),
            Ok(kind) if kind.is_file() => into.push(path),
            _ => {}
        }
    }
}
