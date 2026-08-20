# kura

[![ci](https://github.com/tamnd/kura/actions/workflows/ci.yml/badge.svg)](https://github.com/tamnd/kura/actions/workflows/ci.yml)
[![docs](https://img.shields.io/badge/docs-kura__core-blue)](https://docs.rs/kura-core)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

kura is a storage engine for search and retrieval, written in Rust and built to be linked into a host process rather than run as a server.

The name is 蔵, the Japanese word for a storehouse.
A storehouse is not where the work happens, it is where the things the work needs are kept, in order, so they can be found again.
That is the whole job of this crate.

## Why it exists

Most retrieval systems end up with three storage layers that disagree with each other.
A column store for the structured fields, a separate inverted index for the text, and a vector database for the embeddings.
Every query then has to fan out to all three, and every write has to land in all three before an answer is consistent.
The seams between them are where the wrong results come from.

kura is the primitives for holding all of it in one place.
Posting lists, bitmaps, integer codecs and quantised vectors, sharing one format, one set of identifiers and one definition of what a truncated file is.

## What is here today

This is early.
The crate holds the pieces the rest is built out of, and each one is tested and measured on its own before anything is layered on top.

- **Integer codecs.** LEB128 varints with zigzag mapping for signed values, which is what carries the lengths, the offsets and the runs too short to be worth packing.
- **Posting lists.** Fixed size blocks packed at a width chosen per block, with a skip table, so a membership test on a list of millions decodes one block rather than all of them.
  The blocks decode four ids at a time, which is what makes reading them three times faster than reading the same ids as varints, at a little over half the size.
  Term frequencies ride alongside the ids in a second packed stream that a caller only pays for when it reads it, and each block carries the largest frequency in it so a scorer can skip a block whose best possible contribution cannot reach the current cutoff.
- **Term dictionary.** Terms in order, in blocks of sixteen, with the shared prefix folded out and one index entry per block.
  A lookup binary searches the index and then walks a block, so it touches two cache lines rather than the whole vocabulary, and the folding makes the dictionary smaller than the terms it holds even though it also stores three numbers for each of them.
- **Text analysis.** One tokeniser, used for documents and for queries, because two that differ by anything at all produce an index that cannot find the words in it.
  It folds case, keeps an apostrophe that has a word on both sides of it, and splits Han and kana per character so that text without spaces is findable at all.
- **An index writer.** Text in, a segment out, in one pass.
  Each term's postings go straight into that term's own delta coded chain in an arena, so nothing is buffered or sorted per posting and the only sort at the end is over the vocabulary.
- **Ranked search.** BM25 with block-max WAND.
  The per block frequency ceilings the posting format stores let the scorer decide that a whole block of 128 documents cannot beat what it already has, and skip it without decoding a frequency.
  A total is a separate call from a page, because a total cannot be pruned and most callers do not need one.
- **Stored fields.** The values that come back with a hit, held beside the index rather than in it.
  Field names go in a dictionary at the front and each value refers to its name by number, and the offset array narrows to four bytes per document when the payload fits, which for a corpus of short documents is most of what the section costs.
- **Bitmaps.** A set of document ids that switches between a sorted list and a dense word array depending on how full it is, with intersection, union and difference.
  This is what a permission filter runs on.
- **Vectors.** Cosine similarity, unit normalisation and eight bit quantisation with a per vector scale, which cuts the memory a corpus costs by four with a bounded error.
  Scoring keeps eight partial sums rather than one so the reduction vectorises, and at that point it is bandwidth bound, which is why the quantised form is another four times faster than the full width one.
- **A C ABI.** A static and shared library with a hand written header, so a host in another language can use the engine without a socket and without copying the data.
- **Segments.** The on disk container, holding a header, a section table and the section payloads, checksummed and verified on open.
  It is documented in [docs/format.md](docs/format.md), field by field and with the reason for each one.

The columnar layer and the graph layer are next.
The version number says 0.1.0 for a reason.

## Design rules

**Decoding never trusts its input.** Every decoder takes a byte slice that could have come from a truncated file, a different version of the format or a bad disk, and returns an error instead of panicking or reading past the end.
There are no unwrap calls on a decode path, and the tests feed every decoder its own output cut short at every possible length.

**Nothing allocates on a hot loop.** Intersection, iteration and scoring work in buffers the caller owns.
A growing vector in the middle of a query is the usual reason a benchmark stops scaling.

**One encoding per value.** An input that decodes to a value the encoder would never have produced is rejected.
A format with two spellings for one value has a canonicalisation bug waiting in it.

**No panic crosses the FFI boundary.** Unwinding into foreign frames is undefined behaviour, so every entry point catches first and returns a status code.

**Dependencies are a decision.** The core crate has none, and the FFI crate depends only on the core crate.
Anything linked into this engine is linked into every host that uses it.

## Layout

```
crates/kura-core   the engine, no dependencies, no unsafe on the decode paths
crates/kura-ffi    the C ABI, built as a static and shared library
include/kura.h     the header, written by hand and checked in
examples/c         a C caller, compiled and run in CI on every platform
docs/format.md     the on disk format
```

## Building

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo build --release -p kura-ffi
```

The release build produces `target/release/libkura.a` and a shared library beside it.

## Calling it from C

```c
#include "kura.h"

uint32_t ids[] = {1, 5, 9, 400};
KuraBuffer encoded = {NULL, 0, 0};

if (kura_postings_encode(ids, NULL, 4, &encoded) != KURA_OK) {
  return 1;
}

int32_t found = 0;
kura_postings_contains(encoded.data, encoded.len, 400, &found);
kura_buffer_free(encoded);
```

The null in that call is the term frequency array, and passing null means each id occurs once, which is what a caller that only wants membership wants.
Every call returns a status code and writes its result through an out parameter, so a caller that ignores the status gets a zeroed value rather than a plausible wrong answer.
Memory the engine allocates is freed by the engine.

Build the example with the library:

```sh
cargo build --release -p kura-ffi
cc -std=c11 -Iinclude examples/c/smoke.c target/release/libkura.a -o smoke && ./smoke
```

## Platforms

The engine builds and is tested on macOS, Linux and Windows, on x86-64 and arm64.
There is no platform specific code in the core crate, and no dependency on a system library beyond the C runtime.

## Versioning

The crate version and the ABI version move independently.
`KURA_ABI_VERSION` changes whenever a signature, a status code or a struct layout changes, and a host that links a prebuilt library should compare it against `kura_abi_version()` before calling anything else.
Until 1.0 the on disk format may change between minor versions, and when it does the reader will refuse the old file rather than misread it.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).
The short version is that a change to a decoder needs a test that feeds it bad bytes, and a change to a hot path needs a benchmark.

## License

MIT.
See [LICENSE](LICENSE).
