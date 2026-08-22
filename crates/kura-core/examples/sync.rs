//! What each durability reach costs on the machine it is run on.
//!
//! Run it with `cargo run --release --example sync -- <directory>`, or with no
//! argument to use the temporary directory. It writes a file there, appends
//! 4 KiB and syncs, five hundred times per reach, and reports the distribution
//! of the sync alone.
//!
//! The point is the spread rather than the median. A sync is the one operation
//! in the engine whose worst case is two orders of magnitude off its typical
//! case, and a commit latency quoted as a mean has hidden exactly the thing
//! anybody asking about commit latency wanted to know.
//!
//! The file is written where you point it, so point it at the filesystem the
//! store will live on. The answer is a property of that filesystem and that
//! device, not of the machine, and a run against a temporary directory that
//! turns out to be a memory backed mount has measured nothing.

// Every cast here feeds a printed number that is already approximate.
#![allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]

use std::fs::OpenOptions;
use std::io::Write as _;
use std::time::Instant;

use kura_core::durability::{Reach, sync};

/// How many syncs per reach.
const RUNS: usize = 500;

/// How much goes down before each one.
const PAYLOAD: usize = 4096;

fn main() {
    let directory = std::env::args()
        .nth(1)
        .map_or_else(std::env::temp_dir, std::path::PathBuf::from);

    println!("directory     {}", directory.display());
    println!("runs          {RUNS} per reach, {PAYLOAD} bytes written before each");
    println!();
    println!(
        "{:<9} {:<16} {:>10} {:>10} {:>10} {:>10} {:>9}",
        "reach", "call", "median", "p99", "max", "min", "syncs/s"
    );

    for reach in [Reach::Platter, Reach::Device, Reach::Ordered] {
        match measure(&directory, reach) {
            Ok(times) => report(reach, &times),
            Err(problem) => println!(
                "{:<9} {:<16} {problem}",
                format!("{reach:?}").to_lowercase(),
                reach.call()
            ),
        }
    }

    println!();
    for reach in [Reach::Platter, Reach::Device, Reach::Ordered] {
        println!(
            "{:<9} {:<16} survives {}",
            format!("{reach:?}").to_lowercase(),
            reach.call(),
            reach.promise()
        );
    }
}

/// Times `RUNS` syncs at one reach, in milliseconds.
fn measure(directory: &std::path::Path, reach: Reach) -> std::io::Result<Vec<f64>> {
    let path = directory.join(format!("kura-sync-{reach:?}.bin"));
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&path)?;
    // Its full length up front, so that no sync in the loop is carrying a
    // length change and every one of them is measuring the same thing.
    file.write_all(&vec![0u8; RUNS * PAYLOAD])?;
    file.sync_all()?;

    let payload = vec![7u8; PAYLOAD];
    let mut times = Vec::with_capacity(RUNS);
    let outcome = (|| {
        for _ in 0..RUNS {
            file.write_all(&payload)?;
            let started = Instant::now();
            sync(&file, reach)?;
            times.push(started.elapsed().as_secs_f64() * 1000.0);
        }
        Ok(times)
    })();
    drop(file);
    std::fs::remove_file(&path).ok();
    outcome
}

/// Prints the distribution of one reach's times.
fn report(reach: Reach, times: &[f64]) {
    let mut sorted = times.to_vec();
    sorted.sort_by(f64::total_cmp);
    let median = sorted[sorted.len() / 2];
    println!(
        "{:<9} {:<16} {:>9.3}ms {:>9.3}ms {:>9.3}ms {:>9.3}ms {:>9.0}",
        format!("{reach:?}").to_lowercase(),
        reach.call(),
        median,
        sorted[sorted.len() * 99 / 100],
        sorted[sorted.len() - 1],
        sorted[0],
        1000.0 / median.max(f64::MIN_POSITIVE)
    );
}
