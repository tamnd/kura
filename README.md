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
  The blocks are laid out the way FastLanes describes, four ids to a step, and there is a decoder compiled for each of the thirty two widths so that every shift and mask in it is a constant.
  That makes reading them six times faster than reading the same ids as varints, at half the size.
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
- **Segments.** The on disk container, holding a header, a section table, the section payloads and a footer, with an xxh3-128 per section rather than one over the file, so damage is attributed to the section it is in and every other section stays known good.
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
crates/kura-cli    the `kura-cli` binary, and where `explain` lives
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

## Asking what a query did

```sh
cargo build --release -p kura-cli
./target/release/kura-cli index ./docs -o /tmp/docs.kura
./target/release/kura-cli explain /tmp/docs.kura "block max wand"
```

`explain` runs the query and prints what the engine did to answer it, which is the part a timing cannot tell you.

```
query    block max wand pruning
terms    4 of 4 in the index

  term                        documents     blocks
  block                            4671         37
  max                              5407         43
  pruning                           452          4
  wand                              180          2

walk     page, block-max WAND

  postings decoded                 5624  of 10710         52.5%
  blocks decoded                     45  of 86            52.3%
  blocks skipped                     41  of 86            47.7%
  documents scored                  247
  cursor seeks                      477
  cursor advances                   803

took     86.000µs, and 47.5% of the postings were never read
```

A query where blocks skipped is zero over a long list is a query where the pruning did not fire, and that is a different bug from a query that skipped almost everything and was still slow.
The default explains the walk that produces a page of results.
Pass `--total` to explain the walk that produces a page and the total count instead, which has to visit every match and so skips much less by construction.

## Measuring whether the answers were any good

`explain` says what a query cost. It says nothing about whether the results were the right ones, and a ranking change that nobody measured is a ranking change nobody can defend.

```sh
./target/release/kura-cli topics /tmp/docs.kura topics.tsv -o run.txt
./target/release/kura-cli eval qrels.txt run.txt --per-query
```

`topics` reads a file of queries, one per line, with the identifier and the text separated by a tab, and writes a TREC run file.
`eval` scores that run file against a file of judgments.
They are separate commands because a run written here can be scored by `trec_eval` and a run written by another engine can be scored here.

```
judged   5 queries
answered 4 queries
scored   4 queries

  query                   ndcg@10 recall@100     mrr@10
  q1                       0.9037     1.0000     1.0000
  q2                       0.7724     1.0000     1.0000
  q3                       1.0000     1.0000     1.0000
  q4                       1.0000     1.0000     1.0000

  ndcg_cut_10      0.9190
  recall_100       1.0000
  recip_rank_10    1.0000
```

The measures are named the way `trec_eval` names them and computed the way `trec_eval` computes them, which is not always the way a textbook defines them.
The rank column in a run file is ignored and the score decides the order, ties break by document identifier in reverse order, an unjudged document is not relevant, and the gain is linear rather than exponential.
By default a query the run did not answer is not scored at all, which is `trec_eval` without its `-c` flag.
Pass `--complete` to score it as a zero instead, which is the honest choice when comparing two engines, because an engine that returns nothing for its hard queries should not score as though nobody asked them.

## Looking at the file itself

```sh
./target/release/kura-cli verify /tmp/docs.kura
./target/release/kura-cli dump /tmp/docs.kura | head
```

`verify` reads an index all the way through and reports what is wrong with it, one line per check.
It takes a store or a single segment, it keeps going after a failure so the report says how much of the file is damaged rather than only that it is, and it names the section a bad byte landed in, which is the difference between rebuilding one segment and restoring from backup.

`dump` prints what is in an index, one record to a line, tab separated, with a comment line naming the columns.
The default is the dictionary, `--postings` is a line per posting with its frequency, `--documents` is a line per stored field, and `--term` narrows any of it to one term.
Every line says which segment it came from, including when there is only one, so a script never has to know which kind of file it is reading.

```
# segment	term	document	frequency
1	storage	0	1
1	storage	1	2
```

It is meant to be diffed.
The same corpus indexed by two builds produces two identical dumps, and where they stop being identical is what changed.

A dump prints terms and stored fields, which is to say it prints the corpus back out.
Whatever rules the corpus came with apply to what comes out of here.

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
