//! What replacing documents that are already in a store costs.
//!
//! Run it with `cargo run --release --example upsert -- <directory> [replaced]
//! [segments]`. It indexes the files under the directory, one document each,
//! keyed by path, spread over the number of segments asked for, and then
//! replaces that many of them in one batch and times it.
//!
//! This is the shape the update column of a benchmark table is measured in:
//! documents that are already there, given again under the keys they are already
//! under, so every one of them is a lookup, an index and a deletion rather than
//! an append. The replaced documents are spread across every segment on purpose,
//! because a batch that only touches the newest segment is the easy case and it
//! is not the case a running store is in.
//!
//! The store is written to the system temporary directory and deleted on the way
//! out.

// Every cast here feeds a printed number that is already approximate.
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use std::path::{Path, PathBuf};
use std::time::Instant;

use kura_core::file::Store;
use kura_core::index::Writer;
use kura_core::ingest::Batch;

/// A store identifier, so the file says what wrote it.
const STORE: u128 = 0x006b_7572_612d_7570_7365_7274_0000_0001;

fn main() {
    let mut args = std::env::args().skip(1);
    let root = args.next().unwrap_or_else(|| ".".to_string());
    let wanted: usize = args
        .next()
        .and_then(|count| count.parse().ok())
        .unwrap_or(5_000);
    let segments: usize = args
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
    if documents.len() < segments * 2 {
        println!(
            "{root} holds {} readable files, which is not enough for {segments} segments",
            documents.len()
        );
        return;
    }
    let replacing = wanted.min(documents.len());

    let path = std::env::temp_dir().join(format!("kura-upsert-{}.kura", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let mut store = Store::create(&path, STORE, 0).expect("a store");
    let mut manifest = store.manifest().clone();

    let each = documents.len().div_ceil(segments);
    let mut bytes = 0u64;
    let start = Instant::now();
    for (n, chunk) in documents.chunks(each).enumerate() {
        let mut writer = Writer::new();
        for (key, text) in chunk {
            writer.add_keyed(key, text).expect("a document");
        }
        let docs = u32::try_from(writer.len()).expect("a segment holds this many");
        let built = writer.finish().expect("a segment");
        bytes += built.len() as u64;
        let described = store
            .append_segment(&built, docs, n as u64)
            .expect("appended");
        manifest.total += u64::from(described.docs);
        manifest.live += u64::from(described.docs);
        manifest.segments.push(described);
    }
    store.commit(manifest, 1).expect("committed");
    let indexed = start.elapsed();
    let corpus: u64 = documents.iter().map(|(_, text)| text.len() as u64).sum();

    println!("corpus              {corpus:>12} bytes");
    println!("documents           {:>12}", documents.len());
    println!("segments            {segments:>12}");
    println!("index wall          {:>12.2} s", indexed.as_secs_f32());
    println!(
        "index rate          {:>12.0} docs/s",
        documents.len() as f32 / indexed.as_secs_f32()
    );
    println!("segment bytes       {bytes:>12}");

    // Every nth document, so the batch touches every segment rather than the
    // tail of the store, which is what makes the deletions span segments.
    let step = documents.len() / replacing;
    let chosen: Vec<&(Vec<u8>, String)> = documents
        .iter()
        .step_by(step.max(1))
        .take(replacing)
        .collect();

    let start = Instant::now();
    let view = store.view().expect("a view");
    let mut batch = Batch::over(&view).expect("a batch");
    for (key, text) in &chosen {
        batch.add_keyed(key, text).expect("a document");
    }
    let replacements = batch.replacements();
    let held = batch.held().total();
    let epoch = batch.commit(&mut store, 2, 2).expect("committed");
    let replaced = start.elapsed();
    drop(view);

    println!("replaced            {:>12}", chosen.len());
    println!("  found in store    {replacements:>12}");
    println!("  batch held        {held:>12} bytes");
    println!("update wall         {:>12.2} s", replaced.as_secs_f32());
    println!(
        "update rate         {:>12.0} docs/s",
        chosen.len() as f32 / replaced.as_secs_f32()
    );
    println!("epoch               {epoch:>12}");
    println!("live                {:>12}", store.manifest().live);
    println!("total               {:>12}", store.manifest().total);
    println!("store               {:>12} bytes", size(&path));

    // The whole point of the exercise: one live copy of every document, and the
    // store is one commit later rather than several.
    assert_eq!(store.manifest().live, documents.len() as u64);

    drop(store);
    let _ = std::fs::remove_file(&path);
}

/// How large the store file is.
fn size(path: &Path) -> u64 {
    std::fs::metadata(path).map_or(0, |about| about.len())
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
