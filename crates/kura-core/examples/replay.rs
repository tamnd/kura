//! What putting a real log back into a real store costs.
//!
//! Run it with `cargo run --release --example replay -- <store>`. It opens the
//! store, walks whatever the log holds, turns the records back into documents
//! and commits them, and times the two halves separately, because the walk is
//! a read of the log and the replay is the whole write path minus the analyser.
//!
//! This writes to the store it is given. That is the point of it: a replay that
//! did not commit would not be the thing worth timing, since the commit is where
//! the ordering that makes any of it safe actually happens. Point it at a copy.
//!
//! The way to make a store worth pointing this at is to interrupt a run of
//! `kura-cli index --store`, which leaves every document it had taken in the log
//! and nothing in a segment.
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

use kura_core::file::Store;
use kura_core::ingest::replay;
use kura_core::residency;

fn main() {
    let Some(path) = std::env::args().nth(1) else {
        println!("usage: replay <store>");
        return;
    };
    let path = std::path::PathBuf::from(path);

    // The walk first, on a store opened for it and dropped again, so that the
    // number below is a replay and not a replay plus the reading it needs.
    let mut store = Store::open(&path).expect("a store");
    let started = Instant::now();
    let walked = store.recover(|_| ()).expect("the log walks");
    let walking = started.elapsed();
    drop(store);

    let before = store_length(&path);
    let mut store = Store::open(&path).expect("a store");
    let live = store.manifest().live;
    let now = 1_700_000_000;
    let started = Instant::now();
    let put_back = replay(&mut store, now, now).expect("the replay finishes");
    let replaying = started.elapsed();
    let after = store.manifest().live;
    let peak = residency::peak_resident().unwrap_or(0);
    drop(store);

    println!("store               {}", path.display());
    println!("file bytes          {before:>12}");
    println!("documents before    {live:>12}");
    println!();
    println!("records             {:>12}", walked.records);
    println!("record bytes        {:>12}", walked.bytes);
    println!(
        "walk wall           {:>12.2} s, {:.0} records/s",
        walking.as_secs_f64(),
        walked.records as f64 / walking.as_secs_f64().max(f64::MIN_POSITIVE)
    );
    println!();
    println!("documents put back  {:>12}", put_back.documents);
    println!("of them replaced    {:>12}", put_back.replacements);
    println!("documents after     {after:>12}");
    println!(
        "replay wall         {:>12.2} s, {:.0} docs/s",
        replaying.as_secs_f64(),
        f64::from(put_back.documents) / replaying.as_secs_f64().max(f64::MIN_POSITIVE)
    );
    println!("peak resident       {peak:>12}");
    println!("file bytes after    {:>12}", store_length(&path));
    println!("epoch               {:>12}", put_back.epoch.unwrap_or(0));
}

/// How long the file is, or zero if it cannot be asked.
fn store_length(path: &std::path::Path) -> u64 {
    std::fs::metadata(path).map_or(0, |found| found.len())
}
