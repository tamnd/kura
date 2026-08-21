//! What a key lookup costs on a real store of several segments.
//!
//! Run it with `cargo run --release --example lookup -- <directory> [segments]`.
//! It indexes the files under the directory, one document each, keyed by path,
//! spreads them over the number of segments asked for, writes the store to a
//! temporary file and then times looking keys up through it.
//!
//! The point is the segment count. A lookup in one segment is a filter probe and
//! a binary search, which `examples/keys.rs` already measures on its own. A
//! lookup in a store is that plus every other segment saying no first, and how
//! much that costs is the number that decides whether a store can be searched by
//! key at all. The model in `examples/keys.rs` says what it ought to come to.
//! This says what it does.
//!
//! The store is written to the system temporary directory and deleted on the way
//! out.

// Every cast here feeds a printed number that is already approximate.
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::time::Instant;

use kura_core::file::Store;
use kura_core::index::Writer;
use kura_core::segment::{Segment, kind};

/// A store identifier, so the file says what wrote it.
const STORE: u128 = 0x006b_7572_612d_6c6f_6f6b_7570_0000_0001;

fn main() {
    let mut args = std::env::args().skip(1);
    let root = args.next().unwrap_or_else(|| ".".to_string());
    let wanted: usize = args
        .next()
        .and_then(|count| count.parse().ok())
        .unwrap_or(10);

    let mut found = Vec::new();
    walk(Path::new(&root), &mut found);
    found.sort();
    let documents: Vec<(Vec<u8>, String)> = found
        .iter()
        .filter_map(|path| {
            let text = std::fs::read_to_string(path).ok()?;
            Some((path.to_string_lossy().into_owned().into_bytes(), text))
        })
        .collect();
    if documents.len() < wanted * 2 {
        println!(
            "{root} holds {} readable files, which is not enough for {wanted} segments",
            documents.len()
        );
        return;
    }

    let path = std::env::temp_dir().join(format!("kura-lookup-{}.kura", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let mut store = Store::create(&path, STORE, 0).expect("a store");
    let mut manifest = store.manifest().clone();

    let each = documents.len().div_ceil(wanted);
    let mut keys = 0usize;
    let mut key_bytes = 0u64;
    let start = Instant::now();
    for (n, chunk) in documents.chunks(each).enumerate() {
        let mut writer = Writer::new();
        for (key, text) in chunk {
            writer.add_keyed(key, text).expect("a document");
            keys += 1;
        }
        let docs = u32::try_from(writer.len()).expect("a segment holds this many");
        let bytes = writer.finish().expect("a segment");
        key_bytes += sections(&bytes);
        let described = store
            .append_segment(&bytes, docs, n as u64)
            .expect("appended");
        manifest.total += u64::from(described.docs);
        manifest.live += u64::from(described.docs);
        manifest.segments.push(described);
    }
    store.commit(manifest, 1).expect("committed");
    let indexed = start.elapsed();

    let view = store.view().expect("a view");
    println!("segments            {:>12}", view.len());
    println!("documents           {keys:>12}");
    println!("indexed in          {:>12.2} s", indexed.as_secs_f32());
    println!("store               {:>12} bytes", size(&path));
    println!("  keys and filter   {key_bytes:>12} bytes");
    println!(
        "  per key           {:>12.1} bytes",
        key_bytes as f32 / keys as f32
    );

    // A hit for every key in the store, so the average is over every segment
    // rather than over whichever one a sample happened to land in. The keys of
    // the oldest segment are the expensive ones, because every newer segment
    // has to say no first.
    let all: Vec<&[u8]> = documents.iter().map(|(key, _)| key.as_slice()).collect();
    let lookup = view.lookup().expect("the key indexes");
    let hit = time(&all, |key| {
        black_box(lookup.document(key).expect("looked up").is_some());
    });
    // A miss that gets past no filter, which is the whole store paying for
    // nothing, and the case a lookup on a key nobody has written yet takes.
    let absent: Vec<Vec<u8>> = documents
        .iter()
        .map(|(key, _)| {
            let mut key = key.clone();
            key.extend_from_slice(b".absent");
            key
        })
        .collect();
    let missing: Vec<&[u8]> = absent.iter().map(Vec::as_slice).collect();
    let miss = time(&missing, |key| {
        black_box(lookup.document(key).expect("looked up").is_some());
    });

    // The oldest and the newest segment on their own, because the average above
    // hides the spread and the spread is what a store with a hundred segments
    // would run into.
    let first = documents.len().min(2000);
    let newest = time(&all[all.len() - first..], |key| {
        black_box(lookup.document(key).expect("looked up").is_some());
    });
    let oldest = time(&all[..first], |key| {
        black_box(lookup.document(key).expect("looked up").is_some());
    });

    // And what the same question costs a caller that takes no handle, which is
    // every segment's key index opened and thrown away again for one key.
    let alone = time(&all, |key| {
        black_box(view.document(key).expect("looked up").is_some());
    });

    println!("lookup, hit         {hit:>12.1} ns");
    println!("  newest segment    {newest:>12.1} ns");
    println!("  oldest segment    {oldest:>12.1} ns");
    println!("lookup, miss        {miss:>12.1} ns");
    println!("one off, hit        {alone:>12.1} ns");

    drop(lookup);
    drop(view);
    drop(store);
    let _ = std::fs::remove_file(&path);
}

/// How many bytes of a segment the two key sections take.
fn sections(bytes: &[u8]) -> u64 {
    let segment = Segment::open(bytes).expect("a segment");
    [kind::KEYS, kind::KEY_FILTER]
        .iter()
        .filter_map(|&want| segment.section(want))
        .map(|section| section.len() as u64)
        .sum()
}

/// How large the store file is.
fn size(path: &Path) -> u64 {
    std::fs::metadata(path).map_or(0, |about| about.len())
}

/// The mean nanoseconds one call takes, over three rounds of every key.
fn time(keys: &[&[u8]], mut each: impl FnMut(&[u8])) -> f32 {
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
