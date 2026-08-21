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
- **Bitmaps.** A set of document ids held the Roaring way, cut into chunks of 65536, with each chunk kept as a sorted list, a block of words or a list of runs, whichever of the three is smallest for what is in it.
  This is what a permission filter runs on, which is why it matters that a reader in a company wide group costs three and a half kilobytes rather than six hundred, and a contractor with a few thousand ids scattered across a hundred million documents costs sixty nine kilobytes rather than twelve megabytes.
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

## Bounding what an index run holds at once

```sh
./target/release/kura-cli index ./corpus -o /tmp/docs.kura --store --flush-every 32m
```

Without that option an index run keeps every posting in memory until the last file has been read, so the memory it needs is set by the size of what it was pointed at rather than by anything the machine can promise.
`--flush-every` finishes a segment once that much text has gone in and starts a new one, so the memory is set by the budget instead.

The Go source tree at go1.26.6, which is 10,888 text files and 106.4 MB, on an M4 with the files already in page cache, five runs each and the range across them:

| budget | segments | wall | peak RSS |
| --- | --- | --- | --- |
| none | 1 | 1.0 s | 120 to 122 MB |
| 32m | 4 | 1.0 to 1.1 s | 77 to 101 MB |
| 8m | 14 | 1.2 to 1.5 s | 58 to 66 MB |
| 2m | 49 | 2.1 to 3.2 s | 46 to 48 MB |

It is a trade and the table is the shape of it.
A budget near the corpus size costs nothing measurable and takes off about a third of the memory.
A budget far below it holds the memory near the budget and starts costing real time, because every flush pays for a term dictionary and the segments the query has to merge across get more numerous.

The segments cost disk as well.
The same corpus is 14.3 MB in one segment and 17.4 MB across fourteen, which is 22 percent more, and it is the term dictionary being written fourteen times.

Ranking does not change.
Ten queries at depth 100 against the same corpus in one segment and in fourteen produced the same 1000 lines, the same documents in the same order with the same scores at every rank.
Querying is not slower either, and on this corpus the fourteen segment version was slightly faster, 262 to 312 microseconds a query against 298 to 568.

Every index run also says how much the writer held and what it was holding.

```
indexed 10888 documents, 106.4 MB of text into /tmp/docs.kura, 14.3 MB in 1.9s
held at most 63.7 MB at once, 46.3 MB postings, 17.0 MB vocabulary, 397.3 KB stored fields, 64.0 KB lengths
```

That is the largest a single writer got, so on a run with `--flush-every` it is the peak of one segment rather than of the corpus.
The same tree at four budgets, three runs each, and the numbers were identical on every run because they depend on what was indexed and not on the machine:

| budget | segments | held | postings | vocabulary | stored fields | peak RSS |
| --- | --- | --- | --- | --- | --- | --- |
| none | 1 | 63.7 MB | 46.3 MB | 17.0 MB | 397.3 KB | 109 to 130 MB |
| 32m | 4 | 30.9 MB | 23.3 MB | 7.5 MB | 159.1 KB | 73 to 86 MB |
| 8m | 14 | 15.6 MB | 11.8 MB | 3.8 MB | 114.3 KB | 60 to 67 MB |
| 2m | 49 | 9.1 MB | 6.0 MB | 3.0 MB | 97.1 KB | 43 to 45 MB |

Two things fall out of that table.
The postings are about three quarters of what a writer holds and the vocabulary is most of the rest, so a change that made the stored fields cheaper would be a change nobody could measure.
And what the writer holds is only about half of what the process holds, at every budget, so the other half is somewhere else entirely and `--flush-every` does not bound it.

A third line says where that other half is.

```
peak resident 93.4 MB by the last document, 111.7 MB once the segment was built, 111.7 MB once it was written
```

That is the high water mark of the whole process, the same number `/usr/bin/time` reports, read at three points.
A high water mark never falls, so each reading says how much further the work since the one before it pushed the worst the process had been.
Three runs of the tree above with no budget, so one segment:

| | by the last document | once the segment was built | once it was written |
| --- | --- | --- | --- |
| run 1 | 96.6 MB | 108.4 MB | 108.4 MB |
| run 2 | 93.4 MB | 111.7 MB | 111.7 MB |
| run 3 | 109.2 MB | 118.1 MB | 118.1 MB |

Writing the file costs nothing, on every run.
Turning the writer into a segment costs 9 to 18 MB, which is about the size of the segment, and it is a real cost that no budget bounds because it happens once per segment however many there are.
Everything else, and it is the largest part, is already there by the last document: the writer says it is holding 63.7 MB and the process has 93 to 109 MB, so 30 to 45 MB of it is the allocator rather than anything the engine knows about.

Unlike the held numbers these move from run to run, because they are the whole process and the whole process is at the mercy of what else the machine is doing.
Read them as a shape and not as a measurement.

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

## Getting a store back after a bad byte

```sh
./target/release/kura-cli repair /tmp/docs.kura
./target/release/kura-cli repair /tmp/docs.kura --commit
```

A segment is immutable and holds no redundancy, so a segment with a wrong byte in it stays wrong and no tool is going to change that.
What a store holds twice is its manifest, and a manifest is the list of which segments count, so there is exactly one repair available: commit a manifest that leaves out the segments that no longer read and keep the rest.

```
  segment 1 of 3         reads, 22 documents
  segment 2 of 3         does not read, 1 checks failed, 6 documents
  segment 3 of 3         reads, 1 documents

  dropping 1 of 3 segments loses 6 of 29 documents
```

That trade is a loss, and the documents it costs were already unreachable before it ran, since a segment that does not decode was not answering queries either.
What changes is that the rest of the store becomes usable again, which is the difference between losing part of a corpus and losing all of it.
It prints what it would do and writes nothing until it is given `--commit`, and the segments it decides about are the ones `verify` calls damaged, using the same checks, so it never drops a segment that `verify` had just called good.

It writes one manifest into the slot that is not the committed one, which is the path every ordinary commit takes, so the repair is itself recoverable: the manifest it replaced stays in the store until the commit after it.

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
