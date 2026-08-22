//! What a document costs in the log, measured on real text.
//!
//! Run it with `cargo run --release --example record -- <directory>`. It reads
//! every file under that directory, analyses it, writes the record the log would
//! carry, and reads that record back into an index writer, which is what a
//! recovery does. What it prints is what the records cost against what the text
//! cost, and what each of the two passes ran at.
//!
//! The comparison against the text is the number the shape of the record was
//! chosen on. A record holds the tokens the analyser produced rather than the
//! text, which is more bytes than the text in some corpora and fewer in others,
//! and a corpus of source code is the interesting case because it is full of
//! punctuation that analyses away and full of identifiers that do not.
//!
//! It prints counters and sizes only. No path, no key and no token goes to the
//! terminal, because the directories this is pointed at hold somebody's files.

// Every cast here feeds a printed number that is already approximate.
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use std::path::{Path, PathBuf};
use std::time::Instant;

use kura_core::analysis::Analyzer;
use kura_core::index::Writer;
use kura_core::upsert::{Record, Upsert};

fn main() {
    let root = std::env::args().nth(1).unwrap_or_else(|| ".".to_string());
    let mut found = Vec::new();
    walk(Path::new(&root), &mut found);
    found.sort();
    if found.is_empty() {
        println!("{root} holds no files");
        return;
    }

    let mut analyzer = Analyzer::new();
    let mut record = Upsert::new();
    let mut records: Vec<Vec<u8>> = Vec::with_capacity(found.len());
    let mut text_bytes = 0u64;
    let mut key_bytes = 0u64;
    let mut tokens = 0u64;
    let mut documents = 0u64;
    let mut writing = std::time::Duration::ZERO;

    for path in &found {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        let key = path.to_string_lossy().into_owned().into_bytes();
        text_bytes += text.len() as u64;
        key_bytes += key.len() as u64;
        documents += 1;

        let start = Instant::now();
        record.clear();
        record.key(&key);
        analyzer.analyze(&text, |token, _| record.token(token));
        record.field("path", &key);
        let bytes = record.bytes();
        writing += start.elapsed();
        tokens += record.tokens();
        records.push(bytes);
    }

    if documents == 0 {
        println!("{root} holds no text");
        return;
    }

    let record_bytes: u64 = records.iter().map(|bytes| bytes.len() as u64).sum();
    costs(
        documents,
        text_bytes,
        record_bytes,
        tokens,
        key_bytes,
        writing,
    );

    // The other half: what a recovery pays to turn those records back into the
    // index that was in memory when the machine stopped. It is a walk over the
    // tokens and no analysis at all, which is the whole reason the record holds
    // what it holds.
    let start = Instant::now();
    let mut writer = Writer::new();
    for bytes in &records {
        let record = Record::read(bytes).expect("a record this build wrote");
        writer.add_record(&record).expect("it goes in");
    }
    let replay = start.elapsed();
    let segment = writer.finish().expect("what was written decodes");

    println!("replay wall         {:>12.2} s", replay.as_secs_f32());
    println!(
        "  rate              {:>12.0} docs/s",
        documents as f32 / replay.as_secs_f32()
    );
    println!("segment bytes       {:>12}", segment.len());

    // And what the same documents cost through the analyser, which is what a
    // recovery would pay if the log held the text. The files are read again
    // here and the read is not timed, so what is being compared is one pass of
    // analysing against one pass of walking tokens.
    let mut writer = Writer::new();
    let mut analysing = std::time::Duration::ZERO;
    for path in &found {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        let key = path.to_string_lossy().into_owned().into_bytes();
        let start = Instant::now();
        writer
            .add_keyed_with_fields(&key, &text, [("path", key.as_slice())])
            .expect("it goes in");
        analysing += start.elapsed();
    }
    let same = writer.finish().expect("what was written decodes");

    println!("from the text       {:>12.2} s", analysing.as_secs_f32());
    println!(
        "  rate              {:>12.0} docs/s",
        documents as f32 / analysing.as_secs_f32()
    );
    println!(
        "  against a replay  {:>12.2} x",
        analysing.as_secs_f32() / replay.as_secs_f32()
    );
    println!(
        "same segment        {:>12}",
        if same == segment { "yes" } else { "no" }
    );
}

/// What the records cost, against the text they were made from.
fn costs(
    documents: u64,
    text_bytes: u64,
    record_bytes: u64,
    tokens: u64,
    key_bytes: u64,
    writing: std::time::Duration,
) {
    println!("documents           {documents:>12}");
    println!("text bytes          {text_bytes:>12}");
    println!("record bytes        {record_bytes:>12}");
    println!(
        "  of the text       {:>12.1} %",
        100.0 * record_bytes as f32 / text_bytes as f32
    );
    println!(
        "  per document      {:>12.0} bytes",
        record_bytes as f32 / documents as f32
    );
    println!("tokens              {tokens:>12}");
    println!(
        "  per token         {:>12.2} bytes",
        record_bytes as f32 / tokens as f32
    );
    println!("key bytes           {key_bytes:>12}");
    println!("write wall          {:>12.2} s", writing.as_secs_f32());
    println!(
        "  rate              {:>12.0} MB/s of text",
        text_bytes as f32 / writing.as_secs_f32() / 1_000_000.0
    );
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
