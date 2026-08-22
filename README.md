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
  A lookup binary searches the index and then walks a block, so it touches a handful of cache lines rather than the whole vocabulary, and the folding makes the dictionary smaller than the terms it holds even though it also stores three numbers for each of them.
  The binary search compares the first four bytes of each block's first term out of an array of its own, and only reads a term when it finds them equal, and the walk of the block never rebuilds a term at all: it carries how much of the term it is looking for it has matched, and every term in the block settles against that number or is skipped without being looked at.
  Together those are half of what a lookup used to cost, which is a measurement that came out of weighing the whole thing against a general finite state transducer and deciding to keep the shape.
- **Text analysis.** One tokeniser, used for documents and for queries, because two that differ by anything at all produce an index that cannot find the words in it.
  It folds case, keeps an apostrophe that has a word on both sides of it, and splits Han and kana per character so that text without spaces is findable at all.
- **An index writer.** Text in, a segment out, in one pass.
  Each term's postings go straight into that term's own delta coded chain in an arena, so nothing is buffered or sorted per posting and the only sort at the end is over the vocabulary.
- **Ranked search.** BM25 with block-max WAND.
  The per block frequency ceilings the posting format stores let the scorer decide that a whole block of 128 documents cannot beat what it already has, and skip it without decoding a frequency.
  A total is a separate call from a page, because a total cannot be pruned and most callers do not need one.
- **Stored fields.** The values that come back with a hit, held beside the index rather than in it.
  Field names go in a dictionary at the front and each value refers to its name by number, and the offset array narrows to four bytes per document when the payload fits, which for a corpus of short documents is most of what the section costs.
  The text itself is compressed in blocks of eight kilobytes in the LZ4 format, so a lookup decompresses one block rather than the file, and any other LZ4 implementation reads a block written here.
  The match finder keeps a chain per hash rather than one candidate, which puts a quarter of a megabyte in the writer and takes a fifth off the largest section in a segment.
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

## Indexing the same directory again

```sh
./target/release/kura-cli index ./corpus -o /tmp/docs.kura --store
./target/release/kura-cli index ./corpus -o /tmp/docs.kura --store
```

A run with `--store` keys every file by its path, and a document written under a key the store already holds replaces it.
The second command above leaves one live copy of every file rather than two, and a file that changed between the two runs is in the index as it is now rather than as it was.
The new documents and the deletions of the ones they replace land in one commit, so a query running at the same time sees one or the other and never a corpus with both copies of a file in it or neither.

A run says which it did:

```
indexed 10888 documents, 106.4 MB of text into /tmp/docs.kura, 16.6 MB in 1.4s
0 of them were new and 10888 replaced a document already in the store
10888 of them went through the log first, 84.3 MB
```

The third line is the write ahead log.
Every document goes into it as it is taken, holding the tokens the analyser produced rather than the text it produced them from, so a store that comes back after a machine stopped mid run rebuilds what it had without running the analyser again and without depending on the analyser still being what it was.
The records are freed when the segment they went into is committed, and the freeing and the commit are one write, so there is no window where a document is in both and none where it is in neither.

It costs what writing the same corpus twice costs.
The Go source tree indexed into a store took 1.08 s before the log and 1.30 s with it, and the log took 84.3 MB for 106.4 MB of text.
The bytes come back as the ring wraps, and the ring is 128 MB in a store this tool makes.

A run into a store puts back whatever the run before it left behind, before it indexes anything of its own, and says so:

```
put back 8871 documents out of the log, 68.9 MB, left by a run that did not finish
```

Killing an indexing run of the Go source tree with a signal it cannot catch, then running the same command again, is where that number came from.
Walking the log to find those 8,871 records took 0.01 s and putting them back took 0.39 s, which is 23,038 documents a second against the 9,000 a second the same corpus indexes at from text, because a record holds the analysis and the text does not.
The example beside it times the two halves separately on a store of your own, and writes to the store it is given, so point it at a copy:

```sh
cargo run --release --example replay -- /tmp/docs.kura
```

The commit that publishes them frees the log in the same write, so a machine that stops in the middle of a replay replays the same records again and lands in the same place, and one that stops after it has nothing left to replay.

The order it happens in is the order that makes it safe rather than the order that reads well.
The log is freed first and the segment published second, because the freed position only reaches the platter inside the manifest that the publish writes.
Freeing it afterwards would leave a committed manifest naming records that are already in a segment, and the next machine to open the store would put those documents in twice.

A log torn anywhere gives back what was promised and no more.
The test for it copies a store, zeroes the log from every 4 KiB boundary in the window written since the last commit through to the end, and opens each copy: the documents whose commit returned are there every time, and the documents that were only logged come back as far as the tear and no further, in the order they were written.

`kura-cli verify` says what a store is holding without touching it:

```
  log              128.0 MB
    8871 records to replay, 68.9 MB
  durability    F_FULLFSYNC, survives the power going
```

That last line is the one to read before believing any of the others.

### Which call, and what it does not promise

Saying a returned commit survives a power cut is a claim about a platform call, and the obvious call does not mean the same thing everywhere.
On macOS an `fsync` returns once the write has been handed to the drive, and the drive is entitled to hold it in a volatile cache and acknowledge it anyway, so a power cut at that moment loses it.
`F_FULLFSYNC` is the call that asks the drive to empty that cache.
On Linux and on Windows the ordinary call already asks for the flush and there is nothing stronger to ask for.

A commit latency measured with the weaker call on one platform and compared against the stronger call on another is not a comparison, so the call is named beside the number everywhere it is reported.
The engine takes the strongest of the three by default, and the other two are there to be asked for out loud:

```sh
./target/release/kura-cli index ./corpus -o /tmp/docs.kura --store --durability device
```

What the three cost, 500 syncs each after a 4 KiB write, on an idle M4 on APFS:

| reach | call | median | p99 | max | syncs/s | survives |
| --- | --- | --- | --- | --- | --- | --- |
| platter | `F_FULLFSYNC` | 3.86 ms | 4.90 ms | 11.94 ms | 259 | the power going |
| device | `fsync` | 3.89 ms | 6.01 ms | 7.16 ms | 257 | the process dying, not the power going |
| ordered | `F_BARRIERFSYNC` | 0.81 ms | 1.24 ms | 1.84 ms | 1,233 | nothing, but nothing written after it lands first |

And on a four core Linux box on ext4 over SATA, where all three are one call:

| reach | call | median | p99 | max | syncs/s |
| --- | --- | --- | --- | --- | --- |
| any | `fdatasync` | 13.71 ms | 115.26 ms | 239.62 ms | 73 |

The honest call is free on the laptop, inside the noise of the weaker one, which is what makes it the default.
It is not free everywhere and it is not free at any moment.
The same laptop measured with three indexing runs' writes still going down gave the strongest reach a median of 5.47 ms, a p99 of 452 ms and a worst of 1.97 s, against a p99 of 6.31 ms for the weaker call on the same run, because asking the drive to empty its cache means waiting for whatever else is in it.
That is an argument for making commits fewer rather than for making them weaker, which is the next piece of work.

Measure your own with the example, pointed at the filesystem the store will live on, since the answer belongs to the device and not to the machine:

```sh
cargo run --release --example sync -- /var/lib/kura
```

A run into a store says what its commits cost and which call made them durable:

```
2 commits, median 37.6ms and worst 37.6ms, synced with F_FULLFSYNC which survives the power going
```

Those are flushes rather than one document commits.
Each of them wrote about 8 MB of segment before it synced, so the four milliseconds of sync are lost in it, and the choice of reach made no measurable difference across three runs of each on the corpus below.
The reach starts to matter when the commit is small, which is what group commit is for and what has not been built yet.

The Go source tree at go1.26.6, which is 10,888 text files and 106.4 MB, on an M4 with the files already in page cache, `--memory 32m`, eight runs of the same command one after another:

| run | new | replaced | segments after | wall |
| --- | --- | --- | --- | --- |
| 1 | 10,888 | 0 | 2 | 1.9 s |
| 2 | 0 | 10,888 | 4 | 1.2 s |
| 3 | 0 | 10,888 | 6 | 1.2 s |
| 4 | 0 | 10,888 | 8 | 1.3 s |
| 5 | 0 | 10,888 | 10 | 1.3 s |
| 6 | 0 | 10,888 | 12 | 1.4 s |
| 7 | 0 | 10,888 | 14 | 1.2 s |
| 8 | 0 | 10,888 | 16 | 1.3 s |

Every document in a run after the first is a lookup, an index and a deletion rather than an append, and it costs about two thirds of what the first run cost, because the first run is the one that reads the files off disk rather than out of the page cache.
A lookup asks the key index of every segment in turn, so the cost of one grows with the segment count, and the run that ended with sixteen segments was not slower than the run that ended with four.

What it does cost is the file.
The documents that were replaced are still in the segments they were written into, so the store grows by a segment a run: 69.8 MB of segments after four runs and 139.4 MB after eight, with 10,888 live documents and 87,104 written throughout.
The file itself is 134.2 MB longer than that, which is the log region, and it is sparse, so a store of a handful of documents is a long file that occupies almost nothing.
Reclaiming that is compaction.
Half of it is here: `kura_core::compact::merge` folds segments into one holding their live documents, and the example beside it does that to a whole store and checks the result answers what the store answered.
On the sixteen segment store the eight runs above left, 139.4 MB of segments holding 87,104 documents of which 10,888 are live:

```
cargo run --release --example compact -- /tmp/docs.kura
```

```
merge wall                  0.36 s
merge rate                 30076 docs/s
merged documents           10888
  left behind              76216
merged terms              400588
merged bytes            16522592
  of the sources           11.9 %
```

The dictionary is the part that shrinks furthest, from 3,679,160 entries across the sixteen segments to 400,588, because fifteen of every sixteen copies of a term were held only by documents somebody had already deleted.
The merge held 37.5 MB of its own beyond the mapped file, which is the segment it was building.

The other half is the commit that swaps the merged segment into the store in place of the segments it came from, and that is what `kura-cli compact` does:

```sh
./target/release/kura-cli compact /tmp/docs.kura
```

```
  segments                       16
    folding                      16
  documents                   87104
    live                      10888
  file bytes              273637391

  fold wall                    0.75 s
  merged documents            10888
    left behind               76216
  merged terms               400588
  merged bytes             16522592
  stranded bytes          139214498

  segments                        1
  documents                   10888
    live                      10888
  file bytes              290164064
  epoch                          18
```

The file gets bigger, which is the part worth saying out loud.
A commit appends, so the merged segment goes on the end and the sixteen it replaced stay where they are, holding 139.2 MB that nothing reads any more.
That space comes back when the file is rewritten and not before, because a query that started before the commit is still reading out of the segments the commit replaced.
Reclaiming it is a separate piece of work from choosing what to fold.

What changes immediately is what a query touches.
The same three queries over the same store, both files warm, nine runs of each, the median of the search itself:

| query | matches | postings decoded before | after | median before | median after |
| --- | --- | --- | --- | --- | --- |
| goroutine channel | 656 | 5,792 | 724 | 156 µs | 29 µs |
| context deadline exceeded | 890 | 7,920 | 990 | 153 µs | 31 µs |
| garbage collector mark | 531 | 5,272 | 659 | 166 µs | 28 µs |

The match counts are the same on both sides, which is the check that matters: a fold that answered a different number would be a fold that lost or duplicated a document.
What the fold takes away is the work of walking sixteen dictionaries and then throwing away seven postings in every eight, and the queries come out about five times faster for it.
The part of the file a query reads goes from 132.7 MB to 15.8 MB at the same time, which is the number that decides how much of an index has to stay in memory.

Choosing when to fold and what to fold is policy and is not here yet, so this is a command somebody runs rather than something the store does on its own.

A run without `--store` writes a single segment and keys nothing, because a file is not a store and there is nothing in it to replace.

## Bounding what an index run holds at once

```sh
./target/release/kura-cli index ./corpus -o /tmp/docs.kura --store --memory 32m
```

Without a budget an index run keeps every posting in memory until the last file has been read, so the memory it needs is set by the size of what it was pointed at rather than by anything the machine can promise.
`--memory` finishes a segment once the writer is holding that much and starts a new one, so the memory is set by the budget instead.

The budget is in the same units the run reports, which is what the writer holds, and the writer is asked after every document.
That makes it a floor rather than a ceiling: the writer stops when it has crossed the budget, not before, so what it holds at its largest is the budget plus whatever the document that crossed it added.
A document cannot be split across two segments, so that last part is not something a budget can take away.

The Go source tree at go1.26.6, which is 10,888 text files and 106.4 MB, on an M4 with the files already in page cache, a warm up run and then three timed runs each:

| `--memory` | segments | held at most | peak RSS | wall |
| --- | --- | --- | --- | --- |
| none | 1 | 53.2 MB | 93 to 99 MB | 0.96 to 0.97 s |
| 128m | 1 | 53.2 MB | 99 to 103 MB | 0.96 to 0.99 s |
| 64m | 1 | 53.2 MB | 91 to 92 MB | 0.97 to 1.0 s |
| 32m | 2 | 36.1 MB | 66 to 89 MB | 0.97 to 1.0 s |
| 16m | 4 | 16.9 MB | 62 to 88 MB | 0.99 to 1.0 s |
| 8m | 9 | 12.2 MB | 56 to 59 MB | 1.0 to 1.1 s |

Two things to read out of that.

What a run holds is the budget plus one document, and one document on this tree is worth up to 6.0 MB.
Every array with an entry per term grows by adding a block rather than by doubling, so nothing a term is counted in can take a step that depends on how much has been indexed already, and the largest step one document takes is the same at the end of a large corpus as at the start.
A run with a budget says which document that was and how much it added, because it is the only part of what a run holds that cannot be read off the rest of the report.

```
held at most 12.2 MB at once, 8.3 MB postings, 3.5 MB vocabulary, 365.3 KB stored fields, 4.0 KB lengths
the most one document added was 6.0 MB, ./corpus/debug/buildinfo/testdata/go117/go117.base64, so a budget of 8.0 MB held 12.2 MB
```

That file is 1.3 MB of base64 in which almost every token is a term nothing else in the tree holds, and it costs that wherever in the run it lands.
It is why the 8m row lands at 12.2 MB and the 32m row at 36.1 MB.
The arrays used to double, and then the same run took an 8.9 MB step, an ordinary 100 KB source file could cost 4.7 MB on its own, and the step grew with the corpus rather than staying put.

The process holds about 40 MB more than the writer says it holds, at every budget.
That is the allocator's slack, the file being read, and the segment going down, and it is additive rather than proportional, so a machine with 200 MB to spare can be told 150m and be believed.
Both of those numbers are this corpus on this machine, and neither is a promise about a corpus with a different vocabulary.

There is also `--flush-every`, which counts the text that has gone in rather than what the writer holds.
It is the older option and the worse one for this, because the ratio between text read and memory held depends on the vocabulary and on how repetitive the corpus is, so a number that bounds one does not bound the other.
It stays because segments of a size is a reasonable thing to want for its own sake.
The same tree, five runs each and the range across them:

| `--flush-every` | segments | wall | peak RSS |
| --- | --- | --- | --- |
| none | 1 | 0.96 to 1.0 s | 92 to 98 MB |
| 32m | 4 | 0.98 to 1.0 s | 62 to 82 MB |
| 8m | 14 | 1.1 s | 47 to 62 MB |
| 2m | 49 | 1.4 s | 37 to 50 MB |

It is a trade and the table is the shape of it.
A budget near the corpus size costs nothing measurable and takes off about a third of the memory.
A budget far below it holds the memory near the budget and starts costing real time, because every flush pays for a term dictionary and the segments the query has to merge across get more numerous.

The segments cost disk as well.
The same corpus is 14.4 MB in one segment and 17.6 MB across fourteen, which is 22 percent more, and it is the term dictionary being written fourteen times.

Ranking does not change.
Ten queries at depth 100 against the same corpus in one segment and in fourteen produced the same 1000 lines, the same documents in the same order with the same scores at every rank.
Querying is not slower either, and on this corpus the fourteen segment version was slightly faster, 262 to 312 microseconds a query against 298 to 568.

Every index run also says how much the writer held and what it was holding.

```
indexed 10888 documents, 106.4 MB of text into /tmp/docs.kura, 14.4 MB in 0.9s
held at most 53.2 MB at once, 39.7 MB postings, 12.8 MB vocabulary, 653.3 KB stored fields, 64.0 KB lengths
```

That is the largest a single writer got, so on a run with `--flush-every` it is the peak of one segment rather than of the corpus.
The same tree at four budgets, three runs each, and the held numbers were identical on every run because they depend on what was indexed and not on the machine:

| budget | segments | held | postings | vocabulary | stored fields | peak RSS |
| --- | --- | --- | --- | --- | --- | --- |
| none | 1 | 53.2 MB | 39.7 MB | 12.8 MB | 653.3 KB | 92 to 98 MB |
| 32m | 4 | 23.5 MB | 17.5 MB | 5.6 MB | 415.1 KB | 62 to 82 MB |
| 8m | 14 | 10.5 MB | 7.3 MB | 2.9 MB | 354.3 KB | 47 to 62 MB |
| 2m | 49 | 8.3 MB | 5.3 MB | 2.6 MB | 353.1 KB | 37 to 50 MB |

The postings are about three quarters of what a writer holds and the vocabulary is most of the rest, so a change that made the stored fields cheaper would be a change nobody could measure.

What the writer holds is also well under what the process holds, and a third line says where the difference is.

```
peak resident 97.8 MB, of which 69.2 MB by the last document, 22.3 MB more merging the postings, 6.3 MB more writing the segment
```

That is the high water mark of the whole process, the same number `/usr/bin/time` reports, read at three points and reported as what each step added.
A high water mark never falls, so the difference between two readings is how much further the work between them pushed the worst the process had been.
Five runs of the tree above with no budget, so one segment:

| | total | by the last document | merging the postings | writing the segment |
| --- | --- | --- | --- | --- |
| run 1 | 97.8 MB | 69.2 MB | 22.3 MB | 6.3 MB |
| run 2 | 93.4 MB | 69.7 MB | 17.3 MB | 6.4 MB |
| run 3 | 95.6 MB | 68.6 MB | 20.7 MB | 6.3 MB |
| run 4 | 94.2 MB | 64.7 MB | 23.1 MB | 6.4 MB |
| run 5 | 92.2 MB | 71.0 MB | 15.0 MB | 6.3 MB |

Most of it is there by the last document, which is the writer plus the allocator plus the buffer each file is read into.
Merging the postings costs 15 to 24 MB, which is the encoded lists and the term dictionary being built while the writer that fed them is still holding everything, and it varies that much because a run that had already been pushed high by the reading pays less for the merge.
Writing the segment costs 6.3 MB and costs it on every run, because it is not the allocator being lucky, it is the pages of a file whose size the corpus decided.

That last number used to be 20.6 MB higher.
The segment was built into a vector and then handed to the store to copy in, so the largest thing this program makes existed twice for the length of one call.
It is now written where it is going as it is made, which is what the three readings are for: the step that should have gone away is the step that went away.

Unlike the held numbers the total moves from run to run, because it is the whole process and the whole process is at the mercy of what else the machine is doing.
Read it as a shape and not as a measurement.
On a run with `--flush-every` the last two steps read as nothing, because by then the segments before it have already pushed the mark past anything the last one does.

The last line of an index run says how many files were not text, and the files it counts used to be the most expensive thing about the run.
A tree of anything real holds archives and libraries and images, and the tool decided whether a file was text by reading all of it and decoding all of it first, so a 77 MB library cost 77 MB to read and up to three times that to decode, because a lossy decode turns every bad byte into three bytes of replacement character.
The decision is now taken on the opening bytes, before anything is decoded and before the rest of the file is read, and a file that survives that is asked the same question again on the whole of it once it has been read.
The rule itself has not changed, and neither has the answer for any file whose opening reads the way the rest of it does.
A tree of 57,363 files, 1.0 GB of text in the 55,884 of them that are text, indexed into a store at four budgets:

| budget | segments | held | peak RSS before | peak RSS now |
| --- | --- | --- | --- | --- |
| none, one file rather than a store | 1 | 244.6 MB | 434.8 MB | 405.7 MB |
| none | 7 | 76.0 MB | 541.3 MB | 219.3 MB |
| 64m | 7 | 65.0 MB | 502.1 MB | 228.4 MB |
| 32m | 13 | 35.1 MB | 531.2 MB | 183.6 MB |

Before the change the budget did not reach the peak at all, and the tightest budget had the highest peak of the three, because the peak was being set by the files the run was throwing away rather than by the ones it was keeping.

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
# segment	term	document	live	frequency
1	storage	0	yes	1
1	storage	1	no	2
```

Every other command hides what a store has deleted, because a document that was replaced is not an answer to a query.
`dump` prints it and says so in the `live` column, because the file is the question this command is being asked: a replaced document stays in the segment it was written into until a merge gets to it, and a tool that dropped it could not be used to find out what a merge is about to do.
Filtering a dump down to the live records is what makes it comparable with what a search returns.

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

## Moving an index off an older format

```sh
./target/release/kura-cli migrate /tmp/docs.kura -o /tmp/docs-2.kura
```

A build refuses a format version it does not recognise rather than parsing it hopefully, which is the right refusal and is also the whole problem, because the build that can read your file is the build you have stopped running.
`migrate` is the way across.

```
  segment 1 of 2         version 1 to 2, 62853 bytes to 63465
  segment 2 of 2         version 1 to 2, 62853 bytes to 63465

  wrote /tmp/docs-2.kura
  2 segments, 800 documents, at epoch 2
```

It never writes in place and never writes over a file that is there.
A migration that failed halfway through would leave a store that is neither version and that nothing will open, and a second file plus a rename by whoever is watching is cheaper than any amount of care inside the write.
An index already in today's format is left alone and nothing is written.

A migrated store is the same store, with the same identifier, so anything that recorded which store it was talking to still recognises it.
Segments are migrated one version at a time, so the step from one version to the next is written once and an older file reaches today by going through the steps between.
What comes out is byte for byte what this build would have written had it indexed the corpus itself, and that is a test rather than a claim: `testdata/format/v1` holds a segment as it came off the build before the term dictionary changed, and the test beside it migrates that segment and compares the result against the one this build writes.

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
