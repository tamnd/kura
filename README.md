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
The bytes come back as the ring wraps, and the ring is 128 MB in a store this tool makes unless `--log` says otherwise.

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

### What the keys cost

A document nobody can name is a document nobody can replace, so everything above is paid for by keeping a key beside every document.
The example measures what that comes to by indexing the same documents twice in one process, once through `add` and once through `add_keyed`, alternating which of the two goes first so that a machine drifting under somebody else's work does not land on one case:

```sh
cargo run --release --example keyed -- ./src 64
```

The size it reports is exact, because the same documents through the same writer come to the same bytes every time.
On 5,992 files of Go source, 64.1 MB of text, the segment goes from 9.8 MB to 10.5 MB, which is 8 percent, or 129 bytes for each document.
That is the whole of what a key costs a store at rest: the key itself, and what it takes to find it again.

The time it reports is not exact, and the example prints every round rather than only their middle so that a reader can tell which they are looking at.
Six rounds of each case on a busy laptop ran between 1.03 s and 4.30 s, and the quickest round of each case was 1.05 s plain against 1.03 s keyed.
Two numbers that close, with that spread behind them, say the key table is not where an index run spends its time, and they do not say anything finer than that.

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

The example was run on that box again later, and since the three reaches are one call there it is three measurements of the same thing:

| reach | call | median | p99 | max | syncs/s |
| --- | --- | --- | --- | --- | --- |
| platter | `fdatasync` | 2.715 ms | 27.259 ms | 38.799 ms | 368 |
| device | `fdatasync` | 4.593 ms | 93.927 ms | 274.030 ms | 218 |
| ordered | `fdatasync` | 5.241 ms | 37.612 ms | 47.161 ms | 191 |

Nothing separates those three rows but the machine, and they are a factor of two apart at the median and a factor of ten at the worst.
That is the floor on how finely a single sync measurement can be read, and it is why the reach is chosen by which promise a store wants rather than by which call measured faster on the day.

The honest call is free on the laptop, inside the noise of the weaker one, which is what makes it the default.
It is not free everywhere and it is not free at any moment.
The same laptop measured with three indexing runs' writes still going down gave the strongest reach a median of 5.47 ms, a p99 of 452 ms and a worst of 1.97 s, against a p99 of 6.31 ms for the weaker call on the same run, because asking the drive to empty its cache means waiting for whatever else is in it.
That is an argument for making commits fewer rather than for making them weaker, which is the section below.

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
The reach starts to matter when the commit is small, which is what the next section is about.

### One commit for many writers

A commit is two syncs, whatever it holds.
The data goes down, one sync puts all of it on the platter, the manifest naming it goes into the slot nothing is reading, and a second sync makes that the store.
Everything else a commit writes is covered by one of those two, so the number of times it waits for the drive is the number of orderings it needs rather than the number of writes it makes.

A run says how many times it waited:

```
9 commits, median 13.6ms and worst 20.9ms, synced with F_FULLFSYNC which survives the power going
waited for the drive 18 times, 2.0 per commit
```

That is the second pass over the Go source tree, where every document replaces one already in the store.
Each of those commits writes a segment and a set of deletions for the segment the replaced documents were in, and it used to sync after each of them: 27 waits for the same nine commits, 3.0 per commit.
The nine syncs that went are about a tenth of that run, which is under the spread of the drive on this laptop, so the count is the number to read and not the wall clock.

The same property is what lets several writers share one commit.
Each of them builds a batch, the batches that are ready go into the file together, one manifest names all of them, and the group pays what one of them would have.
`commit_all` is that, and the example measures it, 128 commits of four documents each into a store of its own, on an idle M4 on APFS:

| group | syncs | syncs per 1,000 documents | commits/s | median | p99 | worst |
| --- | --- | --- | --- | --- | --- | --- |
| 1 | 256 | 500.0 | 127 | 7.914 ms | 9.658 ms | 13.829 ms |
| 2 | 128 | 250.0 | 260 | 7.548 ms | 12.731 ms | 12.731 ms |
| 4 | 64 | 125.0 | 443 | 8.637 ms | 14.746 ms | 14.746 ms |
| 8 | 32 | 62.5 | 1,069 | 7.341 ms | 8.935 ms | 8.935 ms |
| 16 | 16 | 31.2 | 1,901 | 9.022 ms | 10.013 ms | 10.013 ms |
| 32 | 8 | 15.6 | 3,465 | 9.324 ms | 9.949 ms | 9.949 ms |

```sh
cargo run --release --example group -- /var/lib/kura
```

Twenty seven times the commits a second for the same documents, the same segments and the same order.
The column that matters as much is the median, which does not move: a group of thirty two costs one writer's latency, because the group is waiting for the same sync a single commit was waiting for anyway.
The same run on the same laptop while it was busy gave 72 commits a second at a group of one and 1,839 at a group of thirty two, which is the same shape a quarter of the way down, since the sync is the whole of the cost and its spread belongs to the machine.

A four core Linux box on ext4 over SATA, where a single fsync costs three times what it costs on the laptop:

| group | syncs | syncs per 1,000 documents | commits/s | median | p99 |
| --- | --- | --- | --- | --- | --- |
| 1 | 256 | 500.0 | 18 | 26.033 ms | 557.732 ms |
| 8 | 32 | 62.5 | 90 | 71.642 ms | 232.347 ms |
| 32 | 8 | 15.6 | 527 | 70.033 ms | 94.644 ms |

Twenty nine times there, and the multiple is larger for the reason the drive is slower: the more of a commit is the wait, the more of it a group takes away.
That box was carrying a load average of 65 across its four cores, so the latencies are an upper bound and not a measurement of the drive.
The sync counts are the same on a busy machine as on an idle one, which is most of why they are the column worth quoting.

A batch does not have to have been prepared against the state it is committed into, which is the part of this that is not about syncs at all.
Building a batch is an analyser pass over however many documents it holds, and a commit by somebody else in the middle of that used to throw the pass away, because a batch names segments by position and holds the whole answer for each one it deletes from.
Now it is joined onto what happened instead.
Its deletions are unioned with what each segment hides now, its own segment's position is remapped, and its keys are looked up again in the segments that arrived while it was being built, so a key two writers used ends up with one live document and it belongs to whoever committed last.
A batch prepared against the current state has nothing above it and pays for none of that.

A compaction moves more than a commit does, because it folds a run of segments into one and every document in that run is either renumbered or dropped.
That used to be the end of a batch that was prepared before it.
Now the fold writes down where everything went and the batch is carried through it: a deletion naming a folded segment becomes a deletion naming the merged one with every identifier put through the mapping, a deletion naming a segment above the run moves down by however many positions the run lost, and a document the merge did not carry is one that was already deleted, so the deletion the batch holds for it has already happened.
The store keeps the record of one fold and the next fold replaces it, so what is still refused is a batch that two folds went past, and the answer to that is to build it again.

The thing that forms the group is `Writer`, which is a store several threads can commit into.
Each of them asks it for a view, builds a batch against that view, and hands it over.
Whoever finds nobody committing takes everything that is waiting, its own batch included, and commits all of it at once, and everybody else waits for the answer.
Nobody lingers hoping for company.
A commit is one sync of latency and nothing else, so a leader that waited for more batches would be making a lone writer slower for nothing, and the window that costs nobody anything is the length of the sync already in flight.
Under load that window fills on its own.

The view is handed out rather than taken from the store, so preparing a batch never waits for the drive, and preparing is the expensive half of ingest.
That view is usually a commit or two behind, which is exactly what the join above makes harmless.

Sixteen threads, 512 batches of four documents between them, on an M4 on APFS:

| threads | documents/s | syncs | syncs per 1,000 documents | average group | median | p99 |
| --- | --- | --- | --- | --- | --- | --- |
| 1 | 501 | 1,024 | 500.0 | 1.0 | 7.795 ms | 14.752 ms |
| 2 | 376 | 1,024 | 500.0 | 1.0 | 15.935 ms | 95.888 ms |
| 4 | 970 | 512 | 250.0 | 2.0 | 15.778 ms | 24.114 ms |
| 8 | 1,811 | 256 | 125.0 | 4.0 | 15.939 ms | 28.926 ms |
| 16 | 3,813 | 128 | 62.5 | 8.0 | 15.998 ms | 21.907 ms |

```sh
cargo run --release --example writers -- /var/lib/kura
```

The group comes out at half the thread count at every step, and that is not an accident of the machine.
The threads a commit releases spend the next commit preparing their next batch and queue for the one after it, so the writers settle into two cohorts that take turns leading.
Half the threads is therefore the number to expect, and the cost per document falls with it while the median wait stays where a single commit put it.

The two thread row is the one worth reading carefully, because half of two is one.
The pair alternate, neither ever joins the other, and each waits behind the other's commit for no gain: the same throughput as one thread for twice the latency.
That is the honest floor of taking the queue as it stands, and it is what a small linger would fix if a real workload ever asks for it.

The leader checks each batch against the store it is about to commit into, carries the ones a fold moved through it, and commits them with the rest.
A batch it cannot carry is answered on its own rather than costing the group its commit.

### Threads that fill batches at once

The commit is two syncs and the read is a syscall, and between them is the analyser, which is where an index run spends its time.
`--threads` gives that part of it to the machine.
Each thread takes files off a shared counter one at a time, fills a batch of its own against the view the writer is handing out, and gives it back, and the batches that are ready when a commit finishes go into the next one together.

```sh
kura-cli index /usr/src -o /var/lib/kura/store.kura --store --memory 32m --threads 8
```

The Go source tree again, 10,884 text files and 104.6 MB, on an M4 of ten cores with the files in page cache:

| threads | wall | documents/s | peak resident | commits | syncs | folding | wall with `--no-fold` | segments after |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 1 | 1.2 s | 9,100 | 94.0 MB | 2 | 6 | 104 ms | 1.1 s | 1 |
| 2 | 627 ms | 17,400 | 96.8 MB | 3 | 8 | 111 ms | 509 ms | 1 |
| 4 | 451 ms | 24,100 | 125.5 MB | 4 | 8 | 122 ms | 289 ms | 1 |
| 8 | 389 ms | 28,000 | 140.8 MB | 8 | 8 | 151 ms | 233 ms | 1 |
| 16 | 397 ms | 27,400 | 158.5 MB | 16 | 10 | 180 ms | 208 ms | 2 |

Five runs of each, medians.
The two last columns are the whole story: every run now ends by folding what it wrote into one segment, and what a run costs above the `--no-fold` ceiling is that fold.
It scales to eight threads, which is where the ceiling stops improving, and it stops scaling for the ordinary reason rather than for a storage one.

The four thread row is slower than it used to be and it is worth saying why.
It was 297 ms and it left four segments; it is 451 ms and it leaves one.
Four segments is a store that answers about twice as slowly as the same documents in one, for as long as it stays that way, so the run was quietly handing its own saving to every query afterwards.
`--threads` now changes how long the run takes and not the shape of the store it leaves, which was the point of the flag.
The old behaviour is still there under `--no-fold`, and `kura-cli compact` is what folds afterwards for anybody who would rather choose the moment.

The fold that happens as the run goes is a thread of its own now.
It asks the policy what is due against the same view the writers are filling batches against, which costs no lock, and takes the store only for the fold itself.
The batches in flight are carried through it rather than refused, which is what makes it possible at all.
At `--memory 32m` on this corpus it barely gets to do anything, because 32m per thread is more than a tenth of the corpus and every thread commits once at the very end, so only the sixteen thread run has anything to fold in the middle of itself.
The case it is for is a smaller budget, where the commits are spread over the run.
Eight threads at `--memory 4m`, five paired runs, medians: folding at the end is 470 ms and leaves one segment, folding beside the run is 368 ms and leaves four.
That is 22 percent off the wall clock and a store that wants a `compact` afterwards, and the segment count sits at the cap over the run instead of climbing to thirty three and dropping in one jump at the end.

One thing is different above one thread and the run says it.
There is no log, because a record goes into the ring as its document arrives and the ring is reached through the store, which one thread at a time holds, so a run that stops loses whatever it had not committed rather than leaving it for the next run to put back.
Part of the gap between the first row and the second is that: the single threaded run wrote 83.8 MB into the log on the way past, and the same corpus into a bare segment with no store and no log takes 873 ms.

`--memory` is per thread, so eight threads at 128m is a gigabyte.
That is the peak resident column growing while the wall clock falls, and it is the thing to set before turning the count up on a machine that has other work on it.

What `--threads` is not allowed to change is the store it leaves.
The same corpus by one thread and by eight gives the same live count, the same answers to the same queries and the same stored fields, and a run over a corpus that is already in the store replaces every document once whichever thread got there first.

The Go source tree at go1.26.6, which is 10,884 text files and 104.6 MB, on an M4 with the files already in page cache, `--memory 32m --no-fold`, eight runs of the same command one after another:

| run | new | replaced | segments after | wall |
| --- | --- | --- | --- | --- |
| 1 | 10,884 | 0 | 2 | 2.2 s |
| 2 | 0 | 10,884 | 4 | 1.3 s |
| 3 | 0 | 10,884 | 6 | 1.8 s |
| 4 | 0 | 10,884 | 8 | 2.0 s |
| 5 | 0 | 10,884 | 10 | 1.4 s |
| 6 | 0 | 10,884 | 12 | 1.5 s |
| 7 | 0 | 10,884 | 14 | 1.3 s |
| 8 | 0 | 10,884 | 16 | 1.2 s |

Folding is off here so that the segments pile up and the cost of a store that nobody is keeping in shape is visible.
The same eight runs without that flag fold at the fourth and end on two segments, and they are the same wall clock.

Every document in a run after the first is a lookup, an index and a deletion rather than an append, and it costs about two thirds of what the first run cost, because the first run is the one that reads the files off disk rather than out of the page cache.
A lookup asks the key index of every segment in turn, so the cost of one grows with the segment count, and the run that ended with sixteen segments was not slower than the run that ended with four.

What it does cost is the file.
The documents that were replaced are still in the segments they were written into, so the store grows by a segment a run: 68.9 MB of segments after four runs and 137.7 MB after eight, with 10,884 live documents and 87,072 written throughout.
The file itself is 134.4 MB longer than that, which is the log region at its default length, and it is sparse, so a store of a handful of documents is a long file that occupies almost nothing.
Reclaiming that is compaction.
Half of it is here: `kura_core::compact::merge` folds segments into one holding their live documents, and the example beside it does that to a whole store and checks the result answers what the store answered.
On the sixteen segment store the eight runs above left, 137.7 MB of segments holding 87,072 documents of which 10,884 are live:

```
cargo run --release --example compact -- /tmp/docs.kura
```

```
merge wall                  0.36 s
merge rate                 29836 docs/s
merged documents           10884
  left behind              76188
merged terms              391413
merged bytes            16339149
  of the sources           11.9 %
```

The dictionary is the part that shrinks furthest, from 3,603,768 entries across the sixteen segments to 391,413, because fifteen of every sixteen copies of a term were held only by documents somebody had already deleted.
The merge held 37.5 MB of its own beyond the mapped file, which is the segment it was building.

The other half is the commit that swaps the merged segment into the store in place of the segments it came from, and that is what `kura-cli compact` does:

```sh
./target/release/kura-cli compact /tmp/docs.kura
```

```
  segments                       16
    folding                      16
  documents                   87072
    live                      10884
  file bytes              272130063

  fold wall                    0.45 s
  merged documents            10884
    left behind               76188
  merged terms               391413
  merged bytes             16339149
  stranded bytes          137705042

  segments                        1
  documents                   10884
    live                      10884
  file bytes              288473293
  epoch                          18
```

The file gets bigger, which is the part worth saying out loud.
A commit appends, so the merged segment goes on the end and the sixteen it replaced stay where they are, holding 137.7 MB that nothing reads any more.
That space comes back when the file is rewritten and not before, because a query that started before the commit is still reading out of the segments the commit replaced.
Reclaiming it is a separate piece of work from choosing what to fold, and it is `kura-cli reclaim` further down.

What changes immediately is what a query touches.
The same three queries over the same store, both files warm, nine runs of each, the median of the search itself:

| query | matches | postings decoded before | after | median before | median after |
| --- | --- | --- | --- | --- | --- |
| goroutine channel | 655 | 5,784 | 723 | 150 µs | 40 µs |
| context deadline exceeded | 887 | 7,888 | 986 | 238 µs | 47 µs |
| garbage collector mark | 529 | 5,256 | 657 | 241 µs | 37 µs |

The match counts are the same on both sides, which is the check that matters: a fold that answered a different number would be a fold that lost or duplicated a document.
What the fold takes away is the work of walking sixteen dictionaries and then throwing away seven postings in every eight, and the queries come out four or five times faster for it.
The part of the file a query reads goes from 131.4 MB to 15.6 MB at the same time, which is the number that decides how much of an index has to stay in memory.

Choosing what to fold is `--due`, which asks the policy in `kura_core::policy` and folds the one run it says is due:

```sh
./target/release/kura-cli compact /tmp/docs.kura --due
```

```
  segments                        9
  documents                   10884
    live                      10884
  file bytes              153060641

  level 0       9 segments, 8 allowed,       18689801 bytes
                        0 of 10884 documents deleted, a rewrite is due at 3266

  folding 9 of them at level 0, 18689801 bytes and 0 of 10884 documents deleted,
  into level 1, because level zero is full
```

The rule is eight segments at level zero and a growth factor of ten above it.
Every commit adds a segment at level zero, eight of them are eight posting lists to walk for a term and eight key filters to ask before a document can be called new, and folding them into one at level one turns that back into one of each.
Past level zero a level is allowed ten times what a level zero segment weighs, so level one holds 1.28 GB and level two holds 12.8 GB, and a level is folded into the next when the segments at it come to more than that.
The factor is what keeps the number of levels logarithmic in the size of the store, and every level is a segment every query opens.

A fold can only take segments that sit next to each other in the manifest, because their order is what decides which copy of a key wins, so the policy reads each level as runs, and neither of the two size rules will fold a run of one.
Asking again after that fold says what it looked at and that there is nothing to do:

```
  level 1       1 segment,       16339149 bytes of 1342177280 allowed
                        0 of 10884 documents deleted, a rewrite is due at 3266

  nothing is due, so nothing was folded
```

The third rule is dead weight, and it is the one rule that will take a run of one.
A deleted document is still in the segment it was written to and still costs what a live one costs, which is a posting to skip in every list it appears in and an entry in the key filter, so once three in ten of the documents in a run are deleted the run is rewritten without them.
Indexing the same tree into the same store twice is the case: the second run replaces every document, and half the store is then documents that have stopped answering.

```sh
./target/release/kura-cli index ./src -o /tmp/dead.kura --store
./target/release/kura-cli index ./src -o /tmp/dead.kura --store
./target/release/kura-cli compact /tmp/dead.kura --due
```

```
  segments                        2
  documents                   21768
    live                      10884

  level 0       2 segments, 8 allowed,       32678313 bytes
                    10884 of 21768 documents deleted, a rewrite is due at 6531

  folding 2 of them at level 0, 32678313 bytes and 10884 of 21768 documents deleted,
  into level 0, because enough of it is deleted to pay for the rewrite

  fold wall                    0.16 s
  merged documents            10884
    left behind               10884
```

That is 10,884 documents rewritten in 0.16 s, and the segment that comes out sits at the level the ones that went in were at rather than a level deeper.
A rewrite that only drops what is dead is not a level of growth, and promoting it would walk a segment down the levels every time somebody deleted from it until it sat at a level whose capacity nothing could ever fill.

What the file does in the meantime is grow, from 167.0 MB to 183.4 MB across that fold, because nothing is reclaimed yet and the segments the fold replaced are still where they were.
The segment count and the live document count are what come back today, and giving the space back is its own piece of work.

One fold per call, deliberately, because a store that is far behind needs several and each of them changes what the next decision should be.
When to run it is still the caller's, and the rate limit that turns this from a rule into a policy is not written yet.

A run without `--store` writes a single segment and keys nothing, because a file is not a store and there is nothing in it to replace.

### The same fan out from a library

What `--threads` does is available to a caller that is not the tool, and it does not take thirty lines of channel and scope to get at.
`ingest::spread` takes a stream of documents and a thread count, analyses them in a batch per thread, and commits all of the batches together, so a reader sees one update rather than one per thread:

```rust
let done = ingest::spread(
    &mut store,
    8,
    documents,
    |document| document.id.as_bytes(),
    |batch, document| batch.add_keyed(document.id.as_bytes(), &document.text).map(|_| ()),
    now,
    now,
)?;
```

There are two closures because the fan out has to know the key before the document is analysed.
A worker is picked by the hash of the key, so every copy of a key lands in the same batch and a key handed over twice in one call is decided by the order it was handed over in, which is what a caller means by handing it over twice.
Anything else would decide it by which batch happened to be earlier in the group.
The queues in front of the workers are bounded, so a producer quicker than the analysers waits instead of pulling the whole input into memory.

It leaves one segment per worker where a single batch leaves one, and it says how many in what it returns, along with how many documents were taken and how many of them replaced a copy the store already held.
Merging them first would cost a pass over everything just written, to save a fold that the compaction policy is going to do anyway on its own schedule and off the caller's thread.

The three callers that had written this by hand are what it came from.
The benchmark runner is the one with a number attached: replacing 5,000 documents in a store of 82,789 took 9.6 s on one thread and 2.3 s across ten, on a machine that was busy for both, so the ratio is what to read there and not either figure.

### The fold at the end of a wide build

A run that analyses on every core still has to make one segment out of what those cores produced, and until recently that last step used one of them.
`Writer::build_on` takes the thread count as well as the parts:

```rust
let segment = index::Writer::build_on(parts, 10)?;
```

Every part already knows its own vocabulary in order, so the merged vocabulary is cut into ranges of terms and each range is merged on its own.
A worker takes the terms in one range out of every part and produces a piece of the posting blob with its dictionary entries and its ceilings beside it, and joining the pieces is a concatenation with a shift, because everything in a piece is numbered against its own blob.
The cuts are taken at even strides through the largest part's vocabulary and found in the others by binary search, and there are several ranges per thread rather than one, because a handful of terms carry most of the postings in any corpus and ranges equal in terms are not equal in work.

The segment it writes is the segment one thread writes, byte for byte.
The merged order comes from the term order and the part order, neither of which depends on where the cuts fall, so the tests compare the bytes rather than the answers.
On the benchmark corpus of 82,789 documents and 464.0 MB in ten parts, the two files came out identical apart from fifty two bytes of timestamp in the superblock and the manifest.

What it saves, on a machine that was busy throughout and alternating between the two so that neither got the quiet half: the fold took 2.3 s, 3.4 s and 4.0 s on one thread against 0.5 s, 1.5 s and 1.2 s on ten.
The ratio is what to read there rather than any of the figures, and it is between two and five.
Before this the fold was around a third of an index phase whose other stages were already wide, which is what held a whole run's parallelism down to twice on a machine with ten cores.

## Keeping the store in shape while a run writes into it

An index run into a store commits a batch at a time and every commit adds a segment, so a run that was bounded at 8 MB of memory over the Go toolchain source used to leave nine segments behind and a run bounded at 4 MB left twenty two.
Nothing folded them, because nothing was asked to.
Now the run folds as it goes, and it does it because there is nobody else there to: at eight segments at level zero a fold is due and the run pays for one, and at twelve it stops taking documents until level zero is back under the cap.

```sh
./target/release/kura-cli index ./src -o /tmp/docs.kura --store --memory 8m
```

```
indexed 10884 documents, 104.6 MB of text into /tmp/docs.kura, 17.8 MB in 2.9s
9 commits, median 15.4ms and worst 36.2ms, synced with F_FULLFSYNC which survives the power going
folded 1 time, 8 segments and 10701 documents, 160.7ms of the run, 6 percent
/tmp/docs.kura now holds 2 segments
```

Two segments instead of nine, for 160.7 ms of a 2.9 s run.
The same corpus with `--no-fold` is the run that was there before, nine segments in 2.4 s, and the difference between the two is what the folding cost.

What it buys is what every question afterwards pays.
The same thirteen queries answered against both stores, warm, four runs of each, came out between 72 and 98 µs a query on the nine segment store and between 49 and 64 µs on the two segment one.
A term costs one posting list walk per segment and a key costs one filter per segment, and that is the whole of the difference.

The hard cap is the other half, and it is for a store that is already far behind rather than one this run is filling.

```
folded 3 times, 39 segments and 18893 documents, 312.1ms of the run, 16 percent
1 of those were waited for at the hard cap of level zero rather than paid for one at a time
/tmp/behind.kura now holds 8 segments
```

That is a run over a store that was left at twenty two segments.
It stopped once before it wrote anything, folded until level zero was under the cap, and paid for two more folds on the way through, which came to 312.1 ms and sixteen percent of the run.
Without the stall the same run would have finished with forty four segments in the file.

`--no-fold` turns all of it off, which is how the numbers above were measured and what to use on a run that would rather fold afterwards.

## Where the time in a fold goes

A fold is worth what it costs, which is settled elsewhere and comes to a few tens of thousands of queries against the store it leaves.
What it costs is a separate question and this is the answer to it.

```sh
cargo run --release --example folding -- ./src /tmp
```

```
 sources         in        out     opening     hashing     merging    the rest       whole
       2     1.2 MB     1.0 MB      0.1 ms      0.1 ms      6.1 ms     10.5 ms     16.6 ms
       4     3.2 MB     2.8 MB      0.2 ms      0.2 ms     16.9 ms     12.8 ms     29.9 ms
       8     6.7 MB     5.7 MB      0.4 ms      0.3 ms     38.4 ms     16.9 ms     55.7 ms
      16    15.3 MB    12.5 MB      0.8 ms      0.8 ms     91.4 ms     23.6 ms    115.8 ms
```

Each row folds that many of the newest segments of the same store, which are the ones a run's own fold folds and are all about the size of the memory budget.
The Go toolchain source at go1.26.6, 10,750 documents in 22 segments, on an M4 of ten cores.

Opening is a source per segment, digests checked.
The read path stopped checking them because it costs a read of the whole segment against a query that wants a few bytes of it, and a fold is the opposite case: it is about to read every byte anyway and it is the last thing that will ever read these bytes before the manifest stops pointing at them.
The hashing column is what that check costs, measured by opening the same segments without it, and it is one percent of the fold.
That is the price of a merge being the place damage is caught, and it is worth paying.

Merging is the whole of the rest of the work and about four fifths of the time.
The vocabularies are walked together, every posting list is decoded, renumbered against the merged document numbering and encoded again, and the block ceilings are recomputed because they are scored against a mean document length that has moved.
Nothing is written to the file in this column.

The rest is the file and the drive: appending the segment, writing the manifest, and waiting for the drive twice.
It is ten milliseconds even for the row that writes a megabyte, because two `F_FULLFSYNC` calls on this machine are about nine of that, and a commit costs two syncs whatever it holds.

The first run of this found a quarter of the merge in `memcmp`.
The walk across the source dictionaries was finding the smallest term at the front of any source by comparing all of them, and then finding which sources held that term by comparing all of them again.
The second pass is now gone: the pass that finds the smallest term records which fronts it compared equal to, because it already knows.
That took the sixteen segment merge from 97.6 ms to 91.4, and the saving grows with the source count because the comparison it removed was one per source per term.

A heap is the usual answer to a merge of sorted runs and it is the wrong one here, which is worth writing down so that nobody has to find out twice.
A heap costs a comparison per entry per level, and the entries are the terms of every source added up.
For segments cut out of one corpus, where nearly every term is in nearly every segment, that is the same count the scan does multiplied by the depth of the heap.

## Getting the space back after a fold

A fold gives back the segment count and leaves the file the size it was.
The segments it replaced stay exactly where they are, because a query that started before the commit is still reading them through offsets it took at the time, so a store that is written to and folded often is a store that is mostly bytes nothing will read again.
`reclaim` writes it out again beside itself holding only what the manifest names.

The store below is the Go toolchain source at go1.26.6, indexed six times over into the same store on an M4 of ten cores, which is what a directory that is watched rather than indexed once looks like.
Every run after the first replaced all 10,884 documents, so 65,304 were written and 10,884 of them are live.
Each run was one commit and so left one segment, which is six against a level zero cap of eight, so nothing folded along the way and the fold below is the one the command asked for.

```sh
kura-cli compact /tmp/go.kura
kura-cli reclaim /tmp/go.kura -o /tmp/go-fresh.kura
```

```
  file bytes              248770765
    superblock and log    134352896
    segments               16343040
    stranded               98074829

  writing /tmp/go-fresh.kura and giving back about 98074829 bytes

  rewrite wall                 0.02 s
  segments                        1
  documents                   10884
  was                     248770765
  now                     150692045
  saved                    98078720
```

248.8 MB to 150.7 MB in twenty milliseconds, and the prediction printed before it wrote anything was 3,891 bytes out, which is less than a page.
The command prints what the file is made of first and refuses a store with anything in its log, so the run that decides whether to do this is a run that wrote nothing.

The 16.3 MB line is the index and the 134.4 MB above it is the superblock, the two manifest slots and the log ring, which `kura-cli` sizes at 128 MB by default.
So the floor a plain rewrite reaches is the log rather than the postings, and on a store this small the log is nine tenths of what comes out.
That is the honest number and it is the whole of the argument for the log being a size somebody picks.

### The log is the other nine tenths

`--log` on `reclaim` gives the new store a log of its own length, and this is the only command that can, because the segments start where the log ends and so the number is fixed for as long as a store exists.

```sh
kura-cli reclaim /tmp/go.kura -o /tmp/go-archive.kura --log 4m
```

```
  file bytes              248770765
    superblock and log    134352896
    segments               16343040
    stranded               98074829
    log becomes             4329472

  writing /tmp/go-archive.kura and giving back about 228098253 bytes

  rewrite wall                 0.02 s
  segments                        1
  documents                   10884
  was                     248770765
  now                      20668621
  saved                   228102144
```

20.7 MB, which is the 16.3 MB of index and the 4.3 MB the log was asked for, and the prediction was again 3,891 bytes out.
The two files answer identically: the same documents in the same order at the same scores for every query tried, and `verify` reads the smaller one through with everything passing.
Shrinking a log is safe here and nowhere else, because a rewrite is refused unless the log is already consumed, so the region it drops is a region with nothing in it, and the new store starts its own ring empty at the length it was given.
A store that came out of this is a store that can still be written to, and a later run that wants a longer log again asks for one the same way.

`--log` is also on the index run that creates a store, which is the other end of the same problem.
Building the store above with `--log 32m` instead leaves a file that reclaims to 50.0 MB rather than 150.7, and it changes the run as well as the file: three commits rather than one, 29.8 MB held at once rather than 53.4, and 81.7 MB peak resident rather than 100.2.
That is the log doing the memtable's job, since every document goes into the log before it is committed and a batch has to end when the ring fills.
The run says so when it opens the store rather than leaving it to be worked out afterwards from the segment count.

```
the log is 32.0 MB and the memory budget is 128.0 MB, so a batch ends when the log fills rather than when the budget does
```

What comes out of either rewrite is the same store.
It carries the same identifier, the same creation time and, unless `--log` said otherwise, the same log size, its segments keep the level a fold gave them and the generation a reader tells deletions apart by, `verify` reads it through with everything passing, and the queries it answers come back with the same documents in the same order at the same scores.
Nothing is renamed and nothing is deleted, because the rename is the step that destroys something and it belongs to whoever decided to do this.

## What a query costs while the store is being written to

Every other measurement here is of one thing at a time.
A run indexes and then it is asked questions, or it is asked questions and nothing is writing.
That is not the case a store is built for, and it hides the question a rate limit would be tuned against: what does a reader pay while an ingest is going on beside it, and how much of what it pays is the folding rather than the writing.

```sh
cargo run --release --example serving -- ./src /var/lib/kura
```

Half the corpus is indexed and folded into one segment, and then the same queries are asked over and over in three conditions: nothing writing, four threads writing the other half with nothing folding, and the same four threads with a keeper folding beside them.
The queries come out of the corpus rather than being made up, taken at ranks 1, 10, 100, 1,000 and 10,000 in what the analyser produced, which is a spread from a term in nearly every document to one in a handful.
What a query costs is the whole of what a reader pays to see the newest commit: taking the view, opening a reader over each of its segments and running the query.
A server holding one searcher open would pay less than this and would be answering out of date.

The Go source tree at go1.26.6, 11,496 files of which 5,748 are indexed first and 5,748 written while the queries run, on an M4 of ten cores, five rounds of each condition with the times pooled, five runs of the whole thing, each cell the middle of the five:

| condition | queries a second | segments at the end | median | p95 | p99 |
| --- | --- | --- | --- | --- | --- |
| quiet | 525,513 | 1 | 2.5 µs | 2.7 µs | 2.8 µs |
| writing | 227,090 | 13 | 2.7 µs | 11.7 µs | 17.2 µs |
| writing and folding | 216,087 | 6 | 2.8 µs | 12.1 µs | 19.0 µs |

An ingest costs a reader 10 percent at the median and about six times at p99, and that is the writing rather than the folding.
Adding a fold beside the writing costs nothing that shows at the median, the two rows landing on the same figure in three runs of the five, and about 11 percent at p99, where the five runs range from 17 percent off to 20 percent on and so say nothing either.

The throughput column says the other half of it.
A saturated reader gets 525,513 queries a second out of a store nothing is writing to and 227,090 out of the same store with four writers on it, so what the ingest really takes from a reader on a ten core machine is cores rather than latency.

There is no worst column, though there used to be.
Now that a query is a few microseconds, the largest number in a million of them is the scheduler taking the core away rather than the store doing anything, and it came out at 1.8 ms, 15.2 ms and 17.5 ms across three runs of the same thing.

That the folding is nearly free to the reader is worth saying plainly, because it is the opposite of what a rate limit is usually for.
The fold takes the store for as long as it runs, but a reader does not take the store, so what a fold costs a reader is the cores it uses and not a wait.
On this machine and this corpus a rate limit tuned to protect the read path would be protecting it from a tenth of a microsecond at the median and from two microseconds in the tail.

What folding costs is the ingest: 147.5 ms with nothing folding against 202.9 ms with a keeper beside it, medians of five rounds, for a store that ends at six segments rather than thirteen.

These numbers replace an earlier set that had the quiet median at 0.48 ms rather than 2.5 µs.
That measurement was not wrong about the store, it was measuring something else: nearly all of it was a reader hashing every byte of the store on the way in, which is the section below.
It also had the folding taking the read tail down rather than leaving it alone, and that was the same artefact, because the reader with the most segments open was the reader with the most hashing to do.

## What a query costs at a load somebody chose

The section above has the reader asking as fast as it can, which is a load nobody runs at.
A latency budget is a promise about what a query costs at a rate the operator picked, so the reader takes one:

```sh
cargo run --release --example serving -- ./src /var/lib/kura 1000 10000
```

Every argument that is a number is queries a second, and the whole measurement is run again at each of them.
The reader holds to the rate rather than to the gap, so the hundredth query is due a hundred gaps after the first whatever the ninety ninth cost, and if the reader is behind it does not wait.
That matters more than it sounds.
A reader that sleeps for a gap, wakes, and starts its clock on waking is reporting the queries it managed and leaving out the ones it could not get to, and the ones it could not get to are the slow ones.
So a run also prints what the client waited, measured from when a query was due rather than from when it started, and it says plainly when a condition did not hold the rate it was offered.

The same corpus and the same machine as the section above, five runs of each, and each cell is the middle of the five:

| offered | condition | segments at the end | median | p95 | p99 |
| --- | --- | --- | --- | --- | --- |
| 1,000 a second | quiet | 1 | 5.7 µs | 19.4 µs | 30.3 µs |
| | writing | 13 | 15.6 µs | 89.1 µs | 358.3 µs |
| | writing and folding | 6 | 13.7 µs | 70.8 µs | 330.9 µs |
| 10,000 a second | quiet | 1 | 2.6 µs | 3.2 µs | 6.3 µs |
| | writing | 13 | 8.9 µs | 30.8 µs | 75.5 µs |
| | writing and folding | 6 | 8.3 µs | 25.0 µs | 54.0 µs |

The first thing in it has nothing to do with writing.
A query costs 5.7 µs when the reader asks a thousand times a second and 2.5 µs when it asks as fast as it can, against the same store with nothing else running.
An idle reader pays twice what a busy one pays, because it comes back to a cold cache and a core it has been taken off, and every number in this file that was measured by a saturated reader is a floor rather than a service level.

The rest of it does not say the same thing as the saturated run, and the difference is the point of running it.
Writing costs the reader 174 percent at the median and close to twelve times at p99 at a thousand a second, and 242 percent and twelve times at ten thousand, where the saturated reader had it at 10 percent and six times.
A reader that never stops asking holds its cache and its core, and one that asks at a rate somebody chose comes back to neither, so an ingest beside it reaches further into what a query costs.

Folding beside the writing costs a reader nothing at either rate, and at a thousand a second it takes something off.
There the folding condition came out below the writing condition at the median in all five runs, at p95 in all five and at p99 in four of five.
At ten thousand the median is a wash, folding above the writing in three runs, below in one and equal in one, while p95 and p99 go to the folding in four of five.

That is the segment count showing up.
The keeper holds the middle query at six segments where the writing alone climbs to thirteen, and at a rate low enough that the reader is not fighting for a core, six segments to walk against thirteen is worth more than the core the keeper takes.
The saturated run does not see it because a saturated reader is short of cores rather than short of anything the keeper could give back.

An earlier set of five runs put the quiet row at 6.1 µs and 3.7, the writing row at 25.9 and 15.1, and had the folding costing 25 to 30 percent at the median rather than saving anything.
Every row of it was measured by a query that opened a reader over each of its segments before it could answer, which #186 took out, and the row it flattered most was the one with the most segments to open.

Above about 10,000 a second one reader thread offering a rate stops being a measurement while the writers run, since going to sleep and being woken costs more than the query does.
So `readers=<n>` puts several threads on it, sharing one schedule rather than each holding their own, which means the offered rate stays the rate the store is asked at rather than the rate times the reader count:

```sh
cargo run --release --example serving -- ./src /var/lib/kura readers=4 50000
```

Four readers, four writers, 50,000 queries a second, five runs, each cell the middle of the five:

| condition | segments at the end | median | p95 | p99 |
| --- | --- | --- | --- | --- |
| quiet | 1 | 2.5 µs | 3.7 µs | 6.1 µs |
| writing | 13 | 6.6 µs | 22.5 µs | 43.6 µs |
| writing and folding | 6 | 7.1 µs | 21.6 µs | 37.8 µs |

Five times the load one reader could offer, and the story is the one the lower loads told.
Writing costs the reader 164 percent at the median and 615 percent at p99.
Folding beside it costs 8 percent more than the writing alone at the median and takes 13 percent off at p99, the folding row landing below the writing row at p99 in four runs of the five, which is the segment count paying for the core the keeper takes and is the same trade the lower rates show.

That is also this repository's number for what a reader pays under a concurrent write at a load worth quoting, and it says the target has not been met.
Read p99 under concurrent write is meant to land within 20 percent of the idle p99 and it is 615 percent over.
The fold is not what does it and no compaction policy will fix it, which leaves the writing itself, and that is where the work is.

The gap is wider than the 390 percent an earlier set of runs gave, and both numbers moved the right way to produce it.
The writing p99 came down from 64.6 µs to 43.6 and the quiet p99 came down from 13.2 to 6.1, and the target is a ratio, so a floor that halves while the loaded number falls by a third makes the ratio worse.
Every row of the earlier set was measured by a query that opened a reader over each of its segments first, which #186 took out, and that was a cost both rows paid, so removing it took more off the small number than the large one.

Four readers is not the ceiling of the harness but it is close to the ceiling of the machine.
At 100,000 a second the quiet condition holds and the two writing conditions come in 1 to 8 percent short, at 200,000 the writing gets through about 124,000, and at 400,000 even the quiet condition tops out around 178,000.
Four readers, four writers and a keeper is nine threads on ten cores, so past this the harness is measuring the scheduler.

The wait is not worth reading too closely either.
On this operating system a thread that asks to be woken in a hundred microseconds is woken about a third late, which is the timer and not the store, and the tail of the waiting moves by more between two runs of the quiet condition than it does between the conditions.

## Where the cost of writing beside a reader goes

The table above says writing costs a reader 164 percent at the median, and it does not say what the reader is paying for.
The writing row differs from the quiet row in four things at once: it holds more documents, it holds them across several segments rather than one, there are four writers on the cores while the query runs, and those writers are committing.
So the same command adds four more rows.
It builds the store the middle query of the writing condition was looking at, by adding batches to the base with one thread and nothing reading until the segment count that query walked is reached, folds a copy of that down to one segment, and asks both of them the same questions with nothing else happening.
Then it puts the writer threads back beside the first of those stores with the commits taken out of them, and finally it runs the writing condition again with the store asking the drive to order its writes rather than to finish them.

Getting the store those rows are read against right took two corrections and neither was the obvious one.
The first is the segment count.
A writing condition that finishes at thirteen segments started at one, so its middle query walked far fewer than thirteen, and a quiet row held at thirteen for a whole round came out slower than the writing row it was there to explain.
Every query now records what its own view held, after its clock has stopped, and the middle of those is the count.
The second is the documents, and it is the same mistake one level down.
The store the middle query saw held what had been added by the middle of the round, so folding the end of the round back down to the middle's count leaves that count with the end's documents, which is a store no query ever saw, and it hands the documents that were not there yet to whichever step of the decomposition comes next.
With that in it the writers came out costing a negative amount three runs out of five.
The store is now built forwards to the count instead of folded backwards to it.

Four readers, four writers, 50,000 queries a second, five runs on a quiet machine.
The middle query of the writing condition walked six segments in three of the five and five in the other two, and every row below it is built to match that count in its own run, so the table is the middle of the three runs that agree on six rather than the middle of all five.
The two runs at five segments are quoted where they say something different.

| condition | segments a query walked | median | p95 | p99 |
| --- | --- | --- | --- | --- |
| quiet | 1 | 2.5 µs | 3.7 µs | 6.1 µs |
| quiet, the documents the middle query saw, one segment | 1 | 2.8 µs | 3.8 µs | 6.7 µs |
| quiet, those documents in six segments | 6 | 6.3 µs | 9.4 µs | 13.6 µs |
| the writer threads beside it, committing nothing | 6 | 7.5 µs | 19.1 µs | 23.8 µs |
| writing | 6 at the middle, 1 to 13 across the round | 6.5 µs | 22.5 µs | 42.2 µs |
| writing, syncs turned into barriers | 6 at the middle, 1 to 13 across the round | 7.0 µs | 23.9 µs | 49.0 µs |
| writing and folding | 6 | 7.1 µs | 21.6 µs | 32.4 µs |

The first four rows are read against the one above, because the costs multiply rather than add.
The documents the writing had added by then cost 10 percent at the median and 10 at p99.
Holding them across six segments rather than one costs 132 percent and 90 more, which is the largest step at both ends by a wide margin.
The writer threads on the cores, doing everything a writer does except the commit, cost 18 percent and 67 more.

The last three rows cannot be read that way, and the reason is worth stating because it is the trap the second correction above was an instance of.
Every row that is not a writing row holds one segment count for a whole round, while a writing row climbs from one segment to thirteen, so half of its queries walked fewer segments than any query in the row above it and a straight subtraction picks up that ramp rather than the thing it was asked about.
It shows in the table as a writing row that is faster than the row above it at the median, 6.5 µs against 7.5, which is not a commit making a query cheaper.
Read only over the queries that walked six segments in both rows, which is what the two have in common, the commits cost 45 percent at the median, 0 at p95 and 33 at p99.

That number is the noisiest thing here and it is quoted as an order of magnitude rather than a figure.
The five runs put it at -6, 24, 45, 81 and 81 percent at the median, because the matched count leaves a few thousand queries in the writing row against forty thousand in the row it is read against.
What the five agree on is that it is large and positive, and the section below takes it apart by changing what a commit holds rather than by measuring it more precisely.

So the last row but one asks which part of a commit does it.
It writes exactly as the writing row does, same batches, same lock, same segment appended, same manifest slot, same view handed over, with the two syncs a commit costs turned into barriers rather than waits.
Read at the six segments both of them walked it comes to -17 percent at the median, 0 at p95 and 8 at p99, so the syncs are not what the reader is paying for.
They are not free to anybody else: the same ingest took 144.6 ms with them and 129.7 ms without, so they are 10 percent of the writing's own wall clock.
What costs the reader is the rest of a commit, and that is a thing this engine controls rather than a thing the drive does.

Folding earns its keep by holding the segment count down.
The writing row ends at thirteen segments and the folding row at six, and at this rate that is worth more to a reader than the core the keeper takes: the folding row is below the writing row at p99 in four runs of the five and 13 percent below at the middle of them.
A rate limit that paused the keeper to buy back half a microsecond at the median would be paying for it in the tail.

Getting inside the read p99 target wants both of the large steps: fewer segments for a query to walk, and a commit that does less to a reader while it is happening.

These numbers replace an earlier set that had the four shares at 13, 104, 22 and 59 percent at the median.
Every row of that set was measured by a query that opened a reader over each of its segments before it could answer, which #186 took out, and the shares moved in a direction worth reading twice.
Opening was a cost every row paid, so removing it took a fixed amount off the top of all of them, and the base the first three are divided into is the smallest number in the table.
The segment count step went up rather than down, from 104 percent to 132, even though opening a set of six readers was part of what it used to be measuring.
The one step that got clearly cheaper as a share is the documents, from 13 percent to 10, which is the only one that was not about the segment count.
So the decomposition now says harder what it said before: the thing to fix is how much a query pays for the segments it walks.

## Which part of a query the writing reaches

A query in these tables is four things and they are counted as one: taking a view of the store, opening a reader over each segment in it, building a searcher over those readers, and running the search.
The same command will time the four separately, which is a different question from the one above and is run separately because it means four more clock reads inside something that takes a few microseconds:

```sh
cargo run --release --example serving -- ./src /var/lib/kura readers=4 parts 50000
```

Same corpus and machine, five runs, each cell the middle of the five.
The run prints two more conditions that the section below reads and this one leaves alone:

| the median of | taking the view | opening the readers | building the searcher | running the query |
| --- | --- | --- | --- | --- |
| quiet | 0.04 µs | 0.00 µs | 0.00 µs | 2.46 µs |
| writing | 0.08 µs | 0.04 µs | 0.04 µs | 6.58 µs |
| writing and folding | 0.08 µs | 0.04 µs | 0.04 µs | 11.12 µs |

| and of p99 | taking the view | opening the readers | building the searcher | running the query |
| --- | --- | --- | --- | --- |
| quiet | 0.12 µs | 0.04 µs | 0.04 µs | 3.88 µs |
| writing | 1.75 µs | 0.04 µs | 0.08 µs | 36.75 µs |
| writing and folding | 0.33 µs | 0.08 µs | 0.08 µs | 44.42 µs |

Three of the four are now nothing.
Taking a view is 40 nanoseconds with nothing writing and 80 with four writers committing as fast as they can fill batches, opening the readers is 0.00 to 0.04 µs in every cell of both tables, and building a searcher over readers that are already open is the same.
Opening used to be the column that multiplied, 0.58 µs quiet against 3.17 under a write, and it is a per view cost spread over the queries that ran against the view rather than a per query one since #186, so what it reads here is a different measurement rather than the same one having got smaller.
"What a view keeps between queries" further down is where that change is read on its own.

That leaves the whole of what the writing costs in one column.
Running the query is 168 percent more under a concurrent write at the median, 6.58 µs against 2.46, and 847 percent more at p99, 36.75 against 3.88.
The four parts add up to the whole query the run reports beside them, 2.50 against a whole query median of 2.5 quiet and 6.74 against 6.8 writing, so nothing large is being spent between them and there is no fifth thing to look for.

The one column that moved the other way is worth naming because it was not visible before.
Taking a view has a p99 of 1.75 µs under a write against 0.12 quiet, and it is the least stable number in the table, 0.33 to 4.62 µs across the five runs where the quiet figure was 0.12 in all five.
In microseconds it is nothing next to a 36.75 µs query, so it is not where the work is, but it is the one part of a query that touches what a committing writer publishes, and a tail that wanders by a factor of fourteen run to run is a tail with a lock in it.

## Whether a commit costs a reader by the commit or by the bytes

Two suspects are gone by now.
The syncs are not what a commit costs a reader, and neither is the view handover.
What is left divides into two kinds of thing.
One kind is paid once per commit whatever the commit held: extending the mapping, taking the lock, publishing the manifest.
The other is paid by the byte, and the candidate there is that a segment written a moment ago is being read for the first time, where a store that has been sitting still has been read a quarter of a million times by the time its median query runs.
The two predict different things, which means a run can tell them apart.
Hold the number of commits a query has lived through the same and change how much each of them wrote.

The same command does it with one more row.
It commits the same way with an eighth of the documents in each batch, over an eighth of the files so that the store still ends the round with the same number of segments, and it reads that row against a control built the way it was built rather than the way the writing row was: the same files, the same batch size, the same pace, with the commits taken out.
Reading the small batch row against the writing row directly would be wrong, because a store of six small segments holds fewer documents than a store of six full ones and is cheaper to query for that reason alone.
So there are two subtractions, each of a row against its own control, and what is read is the two of them against each other.

The five quiet runs behind the section above, each cell the middle of the five:

| a commit holding | what its own control cost | the commits, at the median | at p95 | at p99 |
| --- | --- | --- | --- | --- |
| about 440 documents | 7.5 µs | 45 percent | 0 percent | 41 percent |
| about 55 documents | 4.3 µs | -1 percent | -46 percent | 97 percent |

The top row is the same subtraction the section above reports, read over all five runs rather than over the three whose middle query walked six segments, so it is the same measurement and not a second one.
The row that is new here is the bottom one, and the two are read against each other.
In microseconds rather than percentages the large commit adds 3.4 µs to a query and the small one adds nothing that can be told from zero, which is about 8 nanoseconds a document against none.
The cost follows the bytes.

That rules out the whole per commit family.
Extending the mapping, taking the lock and publishing the manifest all cost the same whether the commit added four hundred documents or fifty, and a commit that added fifty does not show up in a query at all, so the fixed part of a commit is not worth chasing.
What is worth chasing is the segment that was written a moment ago and is being read cold, and that is a thing to do something about rather than a thing to accept: the writer knows exactly which bytes it just wrote, and the reader is about to walk them.

An earlier set of nine runs on a shared machine, taken before a view cached its readers, put the same two rows at 48 percent and 4 percent at the median.
The large row has not moved and the small row has gone from nearly nothing to nothing, so what changed is the floor both of them are measured from rather than the answer.

Three honest limits on this.
The p95 and p99 columns of the bottom row do not agree with each other or with the median, so what is claimed here is the median and not the tail.
The two rows do not start from the same floor, 7.5 µs against 4.3, which is why each is read against its own control and why the two percentages are compared rather than the two rows.
And the control for the bottom row holds about three hundred documents more than the row itself, which is the wrong way round for the claim being made and so does not flatter it, but it does mean a small negative there is a control that is slightly too dear rather than a commit that pays a query back.

## Whether the bytes cost the reader leaving or arriving

By the byte leaves one more fork, and the two sides of it want different fixes.
The bytes are on their way to a file, and the same bytes are about to be walked by a query that has never touched them.
If the cost is the writing then the answer is somewhere in how the store hands bytes to the drive.
If it is the reading then the answer is that the writer knows exactly which bytes it just wrote and the reader is about to walk them, which is a thing to do something about.

One more row separates the two.
It builds the same batches as the no commits row and writes each finished segment to a file of its own beside the store, commits none of them, and asks its questions of the same store that never changes.
The same megabytes go somewhere instead of nowhere and nothing else about the two rows differs, so the two are read against each other whole rather than at a matched count.

The same five quiet runs, each cell the middle of the five:

| | median | p95 | p99 |
| --- | --- | --- | --- |
| threads, no commits | 7.5 µs | 18.9 µs | 23.6 µs |
| threads, segments written aside | 7.5 µs | 19.2 µs | 22.0 µs |

Nothing at the median, and the five runs put the difference at -1, -0, -0, 0 and 1 percent.
In microseconds the same megabytes reaching a file cost a query nothing that can be told from zero where a commit holding them costs it 3.4, so all of it is on the other side.
An earlier ten runs, half of them on a machine busy enough to stretch the ingest from 140 ms to 250, put it at two percent with eight of the ten between minus three and four, which is the same answer read through a larger floor.

So a commit costs a reader by the bytes it wrote, and it is not the bytes leaving, it is the same bytes arriving in a segment no query has touched yet.
Every other candidate is now out.
The syncs are out, the view handover is out, the fixed part of a commit is out because the cost followed the bytes, and the write itself is out because the same bytes written somewhere the reader never looks cost it nothing.

Two honest limits.
The segment written aside goes out unbuffered and is never synced, so what that row pays is the write calls and the dirty pages rather than a flush, and it is only fair to compare it against a commit because the syncs were already measured and were nothing.
And the tails move in both directions between the two rows, so what is claimed here is the median.

## Whether a segment read cold costs page faults or cold bytes

Read for the first time is still two things and they want different fixes.

One is the page faults.
`Store::view` takes a fresh mapping of the whole file every time, and `Writer::catch_up` replaces the view the writer holds whenever the manifest moves, so every commit throws a mapping away and builds another.
A query after a commit faults in pages that were resident a moment ago through a mapping that no longer exists.

The other is that the bytes are cold in the caches whatever the mapping does.
A segment written a few milliseconds ago has been read once, by the thread that wrote it.
Nothing about mapping fixes that.

The run counts faults now, around the same window it times, and reports them per query rather than per round, because a writing round is over when the files are done and a quiet round is a second.
Five runs, each cell the middle of the five:

| condition | faults a query | from a file |
| --- | --- | --- |
| quiet | 0.00 | 0.00 |
| writing | 0.29 | 0.00 |
| writing and folding | 0.25 | 0.00 |
| writing, ordered only | 0.30 | 0.00 |
| writing, smaller batches | 0.25 | 0.01 |
| quiet, all of it | 0.00 | 0.00 |
| quiet, spread out | 0.00 | 0.00 |
| threads, no commits | 0.04 | 0.01 |
| threads, segments written aside | 0.03 | 0.01 |

The quiet rows fault nothing at all, which is the floor and says the counters are reading what they should.
Committing adds about 0.25 faults to a query, 0.29 against the 0.04 of the row that does the same work without committing, and not one of them is a read from a file, which is what pages written a few milliseconds ago should look like.

Then the row that settles it.
Committing eight times as often with an eighth of the bytes in each commit adds about 0.21, which is the same 0.25 within the noise, while its commits cost a reader nothing against 3.4 µs.
So the faults follow the commits and the cost follows the bytes, and those are two different things.

The arithmetic agrees.
Charging the whole 3.4 µs to 0.25 faults puts a fault at 14 µs, and a fault that needs no read from a file is hundreds of nanoseconds.
At a realistic price the faults are a few percent of it, and the write is not in it at all.

What is left is the bytes.
A store sitting still has had its hot postings read a quarter of a million times by the time its median query runs, and a segment a commit just added has been read once.
That is the cost, and remapping on every commit is not worth doing anything about for this reason, whatever else argues for it.

Two limits.
The counters belong to the process and not to the reader, so the writer threads are in them, including whatever their own rebuilt views fault.
That only makes the case stronger, since it means the reader's share of the 0.25 is smaller than 0.25, but it does mean no row here can be read on its own.
And this is an answer by elimination and by a count that moves the wrong way, not a measurement of a cache miss.

## What it costs when there is no core to spare

Everything folding costs a reader on the machine above is the core the keeper takes, and that machine has ten of them for four writers, a keeper and a reader.
The case that would argue for a rate limit is the one with nothing spare, which is `threads=<n>` rather than a smaller machine:

```sh
cargo run --release --example serving -- ./src /var/lib/kura threads=10 10000
```

Ten writers on ten cores, 10,000 queries a second, five runs, each cell the middle of the five:

| condition | segments at the end | median | p95 | p99 |
| --- | --- | --- | --- | --- |
| quiet | 1 | 2.6 µs | 2.9 µs | 4.0 µs |
| writing | 17 | 6.5 µs | 21.1 µs | 100.2 µs |
| writing and folding | 9 | 7.6 µs | 17.5 µs | 42.9 µs |

So the cost of folding to a reader still shows at the median when the cores run out, 17 percent above the writing row, and it is paid back in the tail.
The folding row is below the writing row at p95, 17.5 µs against 21.1, and less than half of it at p99, 42.9 against 100.2.
The segment counts beside them are the reason to believe that is the keeper rather than the run: the writing row ends at 17 segments and the folding row at 9, and what a segment count costs a query is the section below.

That inverts what these two rows said when they were taken with four writers and cores to spare, where folding was above writing everywhere, 11.3 µs against 6.8 at the median and 46.0 against 42.4 at p99.
Both readings are the same trade seen from two sides.
A keeper wants a core, and where there is one to take the reader never notices the segments it saved; where there is not, the core it takes costs a few microseconds at the median and the segments it saved are worth more than that by p95.

The wall is the other half of the price and it is not small.
Ten writers put the first pass through in 89.55 ms with nothing folding and 155.94 ms with a keeper beside them, which is 74 percent more ingest wall for a tail less than half as long.

That is worth stating as a conclusion rather than a table, because it is what a rate limit would have been built against.
A compactor that yields when read p99 goes over a budget would never fire, on this machine, at any of these loads: p99 does not move when the folding starts, and at ten writers it falls.
What moves is the median, by a few microseconds, and what moves it is a thread wanting a core.
A limit that watched the median and paused the keeper would be buying back those microseconds by letting the segment count climb, and the section below is what a segment count costs.

## What a segment count costs a query

The section above says a store of several segments answers more slowly than the same documents in one, and so does most of the rest of this file.
It has been asserted from the shape of the code, which is not a number.

```sh
cargo run --release --example layers -- ./src /var/lib/kura
```

The corpus is indexed once into a store of many segments, and then a copy of that store is folded down to each of a series of counts, so every rung holds exactly the same documents and differs only in how many segments they are spread across.
The queries are picked the same way the serving example picks them.

The Go source tree at go1.26.6, 10,750 documents indexed into 22 segments and folded down from there, on an M4 of ten cores, 200 asks of each query at each rung:

| segments | live | opening | again | unchecked | median | p95 | p99 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 1 | 16 MB | 7.3 µs | 0.00 µs | 0.8 µs | 2.2 µs | 3.0 µs | 3.0 µs |
| 2 | 16 MB | 8.5 µs | 0.00 µs | 1.0 µs | 3.3 µs | 11.0 µs | 13.2 µs |
| 4 | 16 MB | 12.0 µs | 0.00 µs | 1.2 µs | 4.1 µs | 12.6 µs | 13.8 µs |
| 8 | 17 MB | 20.1 µs | 0.00 µs | 2.0 µs | 6.2 µs | 10.0 µs | 15.2 µs |
| 16 | 19 MB | 32.3 µs | 0.00 µs | 3.6 µs | 10.4 µs | 16.9 µs | 17.8 µs |

The query behaves the way the claim says.
Four segments is about twice the median of one and sixteen is about five times it, so what a segment costs is roughly a fixed amount on top of the work the query would do anyway.
Earlier text here put four segments at 20 to 30 percent slower than one, which was too kind to it, and that line has been corrected.

The three opening columns are three different questions and are not to be subtracted from each other carelessly.
Unchecked opens a set of readers of its own on a view that has already answered a query, keeping nothing back for a second caller, so it is what every query paid before a view started keeping what it opened.
Again is the same call on that same warm view now, which is what every query after the first one pays.
Opening builds a view of its own each time, so it pays the first touch of a fresh mapping as well as the opening itself, and it is what the first query after a commit really costs.

The again column read 0.00 µs at every rung in two of three runs and 0.04 at the top rung in the third, which is one tick of the clock.
So the segment count no longer reaches a repeated query at all, where it used to cost it 0.8 µs at one segment and 3.6 at sixteen.
The first query after a commit pays 7.3 µs at one segment and 32.3 at sixteen, and that is paid once for every query that follows it until the next commit.
At sixteen segments the crossover is nine queries: a view asked ten times has already paid for itself, and the serving example gets through about twelve thousand between commits.

The unchecked column is also what says the digests are not being hashed on the query path.
The first run of this found something much larger than the thing it was built to measure: opening a reader over a one segment store cost 873 µs, four hundred times the query it was opened for, and it barely moved as the segment count climbed.
Opening a segment hashed every section in it against the digest in its table before handing back a reader, so an open cost what the store held rather than what the query wanted, and sixteen megabytes at about 19 GB/s is 870 µs.
A reader now opens without checking the digests, which is what the key lookup path had been doing all along.
The structure is still checked, the footer included, so every slice a reader hands out is still inside the mapping, and a byte that changed on disk is still caught by `kura-cli verify`, which is the tool whose job it is to ask.
Every rung holds the same documents, so a column that hashed everything it opened would be flat and enormous, and this one grows with the segment count instead.

## What a view keeps between queries

A view is a snapshot of one commit.
Nothing it describes can change while it is alive, because a commit makes a new view rather than editing this one, so a reader it opened over a segment stays right for as long as anybody holds the view.
It follows that opening those readers once and handing the same ones back is safe, and that is what a view now does.

The decomposition in the serving example says what that was worth, on the same corpus with four writers committing as fast as they can fill batches:

```sh
cargo run --release --example serving -- ./src /var/lib/kura threads=4 parts
```

The median of opening the readers, before this and after it, over three runs of each:

| condition | before | after |
| --- | --- | --- |
| quiet | 0.58 to 0.88 µs | 0.00 µs |
| writing | 1.04 to 1.50 µs | 0.00 µs |
| writing and folding | 1.12 to 2.00 µs | 0.00 µs |
| writing, ordered only | 1.21 to 1.67 µs | 0.00 µs |
| writing, smaller batches | 1.12 to 1.46 µs | 0.00 µs |

The whole query went with it.
On the pair of runs whose running the query column agreed most closely, the quiet median went from 3.2 µs to 2.6 and the writing median from 5.5 to 3.0.
The four parts add up to the whole in both, which is the check that says the opening really left rather than moving somewhere else: 0.04 for the view plus 0.58 for the opening plus 2.62 for the query is 3.24 before, and the same three with nothing in the middle is 2.66 after.
The serving tables above have all been taken again since the change.

The p99 of that column went from 5.96 µs to 0.04, which is the more interesting half.
Opening still happens, once per view, and a view lasts a commit.
The writing run answers 171,424 questions across fourteen commits, so about one query in twelve thousand is the one that opens, and a share that small does not reach a p99 at all.
It reaches the maximum, which is where it belongs: a tail that one query in twelve thousand pays is a tail, and a cost every query paid was not.

## Bounding what an index run holds at once

```sh
./target/release/kura-cli index ./corpus -o /tmp/docs.kura --store --memory 32m
```

Without a budget an index run keeps every posting in memory until the last file has been read, so the memory it needs is set by the size of what it was pointed at rather than by anything the machine can promise.
`--memory` finishes a segment once the writer is holding that much and starts a new one, so the memory is set by the budget instead.
A run into a store is bounded at 128m without being asked, `--memory none` is how to turn that off, and a run without `--store` is unbounded either way because what it writes is one segment and there is nowhere to put a second.

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
