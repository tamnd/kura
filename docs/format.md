# The segment format

A segment is the unit the engine writes, publishes and deletes.
Everything that ends up on disk ends up inside one.

A segment is immutable once written.
A change is a new segment and a delete is a tombstone in a later one.
That is what lets a reader hold an open segment while a writer is still working, with no lock between them, because the bytes a reader is looking at are bytes nobody will ever write to again.

This document is the format itself.
The code that implements it is `crates/kura-core/src/segment.rs`, and every rule below has a test next to it there.

## What a segment is made of

The container knows nothing about what it carries.
It holds a header, a table of sections, and the section payloads, and it hands a caller a byte slice per section.
The term dictionary, the posting lists, the stored fields, the vectors and the access control lists each decode their own section.

That separation is the point.
Changing how a posting list is encoded is not a change to this format, and a segment written before that change is still readable by the container that opens it.

## Layout

```
header, 32 bytes
  0..8    magic
  8..10   format version, u16
  10..12  section count, u16
  12..16  reserved, written as zero
  16..24  body length, u64
  24..32  reserved, written as zero

body
  section table, 40 bytes per entry, section count entries
    0..2    kind, u16
    2..4    flags, u16
    4..8    padding, u32
    8..16   offset from the start of the body, u64
    16..24  length, u64
    24..40  xxh3-128 of the payload, u128
  section payloads, in the order the writer added them

footer, 32 bytes
  0..16   xxh3-128 of the section table, u128
  16..24  body length, u64, the same number the header carries
  24..32  footer magic, the eight bytes KURAFOOT
```

Every integer is little endian.
That is the byte order of every machine this engine targets, so a reader on any of them gets the field without swapping it.

## Why each field is there

**Magic, eight bytes.**
The first question about a file is whether it belongs to this engine at all, and a file that answers that question in its first eight bytes is a file that can be identified by anything, including `file` and a person with a hex editor.
Eight bytes rather than four because four byte magics collide across formats often enough to be a real nuisance.

**Format version, two bytes.**
A build that does not recognise the version refuses the file instead of parsing it hopefully.
Two bytes is far more than this format will ever need, and it keeps the fields that follow it aligned.

The version is 2.
Version 1 searched the term dictionary's block index by reading a term at every step of the binary search, and version 2 keeps the first four bytes of each block's first term in an array of its own in front of the block offsets and compares those instead.
That is the whole difference between them, and `kura-cli migrate` is the way across.
See [what holds the format still](#what-holds-the-format-still) for what a version change costs and what has to happen at one.

**Section count, two bytes.**
It bounds the table before a single entry is read, which is what lets the reader check that the table fits inside the body in one comparison rather than discovering it entry by entry.
It also caps a segment at 65535 sections, which is several orders of magnitude more than any real one will hold.

**Body length, eight bytes.**
This is what makes a segment a record rather than a file.
A reader takes the body length as the boundary and ignores whatever follows, so a segment can sit inside a larger file next to other segments without any of them needing to know.
Eight bytes because four would cap a segment at four gigabytes, and a segment holding an entire shard of a large corpus will pass that.

**Reserved, eight bytes.**
Written as zero and ignored on read.
It rounds the header to 32 bytes, which means the body starts at an offset that is friendly to every alignment the payloads might want, and it leaves somewhere to put the field this format has not needed yet without moving anything.

**Kind, two bytes.**
What a section holds.
See the list below.

**Flags, two bytes.**
Reserved, written as zero.
Per section compression will go here, which is the reason it is per section rather than per segment: a term dictionary and a block of quantised vectors do not want the same answer.
The stored fields already compress, but they do it inside the section, because a store has to stay randomly addressable and that means the compression has to know where the record boundaries are.
A section that can be decoded as a whole is the case this field is for.

**Padding, four bytes.**
It puts the offset and the length on an eight byte boundary within the entry, so reading the table costs no unaligned loads on the architectures that care and none of the compiler's alignment handling on the ones that do not.

**Offset and length, eight bytes each.**
Where the section is and how long it is, with the offset relative to the start of the body rather than the start of the file.
Relative to the body means a writer can build the body before it knows where in a file the segment will land.

**Digest, sixteen bytes.**
The xxh3-128 of that section's payload and nothing else.
See below for why it is per section, and `docs/` has nothing to add about xxh3 itself beyond that it is a hash for detecting damage and not a defence against anybody who wants to change a file.

**Footer, thirty two bytes.**
The digest of the section table, the body length again, and eight magic bytes that are not the header's.

The table digest cannot live in the header, because the header is written first and the table is not finished until every payload has been hashed.
The previous format worked its whole body checksum out in a second pass to get around exactly this.
A footer is written when the number is already in hand, which costs nothing and needs no seek back over a file that may not be seekable.

The body length is repeated because a file whose two ends disagree about how long it is has been damaged in a way that neither end can detect alone.
The footer magic differs from the header magic so that a tool scanning a file for the start of a segment and a tool scanning for the end of one are looking for different needles, and so that a truncated file does not look like the beginning of another segment.

## Why the checksums are where they are

There is no checksum over the segment as a whole, and that is the point.

One digest over everything answers "was this file ever damaged" and cannot answer "which part of it".
Measured on a 68.7 KB index, a byte flipped at two of five offsets was caught by the whole file digest and by nothing else, because the bytes went on to decode into a well formed posting list of the right length in ascending order.
The report for those two could only say that something somewhere was wrong.

A digest per section plus a digest over the table says the same thing and says where.
The table digest covers the offsets and the lengths, so a table that has been edited is caught before any of them is used to slice anything.
Each section digest covers that section's bytes, so damage is attributed to the section it is in and every other section is still known good.

Together they cover every byte of the body exactly once, so nothing is lost by there being no digest over the lot.
The composition is also what lets a reader check one section without reading the rest, which is what `Segment::verify_section` is for and what a repair would need.

This is what `kura-cli verify` prints for an index with a byte flipped inside the postings.

```
  ok       checksum table
  ok       checksum terms
  FAILED   checksum postings
      section kind 2 does not match its checksum: stored 0xaad0..., computed 0xaab4...
  ok       checksum norms
  ok       checksum fields
```

The dictionary and the stored fields are intact, the postings are not, and that is the difference between rebuilding one section and restoring the file from backup.

## Section kinds

| Kind | Name | What it holds |
| --- | --- | --- |
| 1 | terms | the term dictionary, in prefix folded blocks, and where each term's postings start |
| 2 | postings | posting lists, packed in fixed size blocks, with term frequencies in a stream beside them |
| 3 | fields | stored field values, returned with a hit rather than searched, with the names in a dictionary and the records packed into compressed blocks |
| 4 | vectors | quantised vectors, one per passage |
| 5 | acl | the access control lists governing the documents in this segment |
| 6 | columns | columnar values, for filters and facets |
| 7 | graph | entities and edges |
| 8 | tombstones | documents deleted by a later segment |
| 9 | norms | how long each document is, and how long they are on average |
| 10 | bounds | the best score each block of postings can reach, which is what block max pruning compares against |
| 11 | keys | the primary key of each document that has one, sorted, with the document number beside it |
| 12 | key filter | a bloom filter over those keys, so a segment that does not hold a key can say so without the table being read |
| 13 | rounded | how long each document is again, in one byte apiece, which is what scoring divides by when the segment carries it |
| 14 | wide bounds | the same ceilings as kind 10 at two bytes a block instead of one, which is what pruning compares against when the segment carries it |

A section may be absent, and an absent section is not the same as an empty one.
A term dictionary with no terms is a fact about the segment.
A missing term dictionary is a fact about the build that wrote it.
The reader keeps them apart, and so should anything built on top.

The keys and the key filter are the one pair that has to be absent or present together.
A filter without a table can say a key is probably here and then produce nothing, and a table without a filter is searched by every lookup for every key it does not hold, which is the case the filter exists for.
A segment with one and not the other is refused rather than read.

## Sections that are worked out from other sections

Most sections hold something only the writer knew.
The bounds, the wide bounds and the rounded lengths do not.
Every byte in any of them is a function of the postings and the lengths that are already in the file, so a segment written before those kinds existed is not missing data, it is missing a calculation.

That distinction is what lets a kind be added without moving the version.
A reader that has never heard of kind 10 returns the same hits as one that has, because the section only ever says which work can be skipped and never which document wins.
A reader that has never heard of kind 14 prunes from the byte in kind 10, which is the same ceiling rounded up further, so it reads more blocks and returns the same page.
A reader that has never heard of kind 13 divides by the four byte length instead of the byte, which moves a score by well under a percent and can in principle reorder two documents that were already scoring within that of each other.
None of them is a refusal.

Kind 14 is the only section with no directory of its own.
It is the payload of kind 10 again with two bytes where that has one, in the same order over the same terms, so a term whose ceilings are at `start..end` in kind 10 is at `2 * start..2 * end` in kind 14 and a directory would be twelve bytes a term for something already on disk.
What stands in for one is the length: a reader takes kind 14 only when it is exactly twice the payload of kind 10, and ignores it otherwise.
That matters more here than it would elsewhere, because a ceiling read out of a section belonging to some other segment is a bound that does not hold, and a bound that does not hold drops results rather than returning wrong ones.

The bounds and the rounded lengths have to stay sound together, and the rule that keeps them so is that the bounds are worked out from the exact lengths even though the scorer divides by the rounded ones.
Rounding a length goes upwards and a longer document scores lower, so a ceiling from the exact length bounds the rounded score as well as the exact one.
Rounding on the way into the bounds would give a slightly tighter ceiling that only holds for a reader that rounds too, and a build that has never heard of kind 13 still reads kind 10.

It does change what a migration owes.
`kura_core::migrate` promises that a migrated file is what this build would have written, and this build writes both, so the migration runs a second pass after the version steps that rebuilds every derived section from the sections beside it.
That pass runs whether or not the old file had them, and it writes each section in the same place in the table that the indexer would have.
`crates/kura-core/tests/format.rs` is what holds the two paths together: the fixture written by the indexer and the fixture produced by migrating the version 1 file have to come out byte for byte identical, which means the inline calculation in the writer and the decode and recompute in the migration cannot drift apart without a test going red.

## What the reader refuses

Opening a segment is where the engine decides whether to trust a file, so it is deliberately unforgiving.
Each of these has its own error rather than one opaque failure, because a caller that is told the file is short can do something different from a caller that is told the checksum did not match.

- Magic that is not this engine's.
- A format version this build does not write, which includes an older one. `kura_core::migrate` is the only thing that reads one of those, and it does not read a section until it has been told which version it is looking at.
- A file shorter than the header, or shorter than the body length the header claims, or with no room for a footer after the body.
- A footer that does not end in the footer magic.
- A header and a footer that disagree about the body length.
- A section table that does not fit inside the body.
- A section whose offset and length do not lie inside the body, or that overlaps the table.
- Two sections claiming the same kind.
- A section table that does not match the digest in the footer.
- A section whose bytes do not match the digest in its table entry.

None of these allocate anything sized from a number in the file, which is the rule that makes a hostile 32 byte header stay a hostile 32 byte header instead of becoming a request for four exabytes of memory.

Duplicate kinds are worth a word.
The writer already refuses to add one, so a duplicate can only come from a file written by something else.
A reader that silently took the first of two sections would be a reader whose answer depends on the order the writer happened to use, which is the kind of bug that only shows up once two builds disagree.

## What the reader forgives

An unknown section kind is not an error.
A reader skips a section whose kind it does not recognise, and a caller asking for a kind that is not present gets nothing back rather than a failure.

That is the whole forward compatibility story.
A later build can add a section kind, and every file it writes stays readable by every build that came before.
The version is a refusal and the kind is a shrug, and keeping those two apart is what makes it possible to add to the format without a migration.

## What holds the format still

Everything above is a description, and a description drifts.
The thing that keeps it honest is `crates/kura-core/tests/format.rs`, which holds two files written by an earlier build and checked into `testdata/format`: a bare segment and a store with two segments in it, a manifest and a log ring.

It runs them both ways.
Writing the fixture again and comparing it byte for byte catches a change to what this build produces, including one a reader would happily accept, such as a field that grew or an ordering that moved.
Reading the checked in bytes and asking them the same questions catches this build no longer understanding a file it wrote before, which is the failure that matters to somebody with data on disk and the one a change to a decoder alone can cause.

The byte for byte half also pins the writer to being deterministic.
A writer that walked a hash map would pass every other test in the crate and fail this one on somebody else's machine, and the fixtures are checked by CI on five platforms, so a number that came out of the host rather than the input has nowhere to hide.

A failure here is not a broken test.
It is a format change, and the answer to it is to move the version, teach `kura-cli migrate` the step from the old version to the new one, keep the old fixture as the input to a test of that step, and write a new fixture beside it.
`KURA_BLESS=1` writes the fixtures rather than checking them, which is how a new one is made, and it is deliberately awkward to reach for.

That is not a description of a policy, it is a description of what happened.
`testdata/format/v1` holds the same segment and the same store as they came off the build before the dictionary changed, and the test beside them migrates the version 1 segment and compares the result against the version 2 one byte for byte.
The old fixtures are never blessed, because nothing in the tree can write them any more, and that is exactly what makes them worth keeping.

## Reading without the checksums

`Segment::open` verifies everything.
It is the one to use unless there is a measured reason not to.

`Segment::open_without_checksum` skips the digests and nothing else.
Every structural check still runs, the footer included, so a section slice it hands back is still inside the input and a corrupt section table is still refused.

`Segment::verify` is the digests on their own, for a caller that opened without them and wants to pay later.
`Segment::verify_table` is the footer digest by itself, which is thirty two bytes per section and no payload at all.
`Segment::verify_section` is one section, for a reader that wants the dictionary checked and does not want to read the postings to get it.

The distinction matters, and it is worth more than an argument from first principles, so here is the measurement.
The segment holds a million document posting list and a megabyte term section, and the machine is an idle Apple M4 running macOS 15.7.

```
open a segment, checksum verified                 91.8 us
open a segment, checksum skipped                    83 ns
```

Run it yourself with `cargo run --release --example bench`.

The verified figure was 425.6 us on the same machine when the body was checksummed as a whole with CRC-32.
It is a third of a millisecond quicker per segment for a strictly better answer, because xxh3 is around six times the rate and the pass over the payloads is the same pass.

The skipped figure sits near the resolution of the platform timer, so read it as under a microsecond rather than as exactly 83 ns.
Either way the gap is around three orders of magnitude, and it grows with the size of the segment, because one side is a walk of a two entry table and the other is a pass over every byte.
That is the shape you want: opening should cost what the table costs, and verifying should cost what the data costs.

For a memory mapped file the difference is larger still, since verifying means faulting in every page rather than the one the table lives on.
That trade is worth making for a segment this process wrote and published and has not touched since.
It is not worth making for a file that arrived from somewhere else.

The key lookup path is where that trade was taken next.
A lookup by primary key opens every segment in the store, asks its filter and moves on, so opening is the whole of what it does apart from the probe.
On a ten segment store of eleven thousand documents keyed by path, verifying the digests on the way in made a lookup 511 microseconds and skipping them made it 615 nanoseconds, on the same machine and the same store.
The segments were written by this process and published, and a store that wants its bytes proved has `kura-cli verify` for that, which reads every segment once instead of once per key.

Verifying runs at about 19 GB/s on that machine, which is what xxh3-128 gets on thirty two megabytes that do not fit in cache.
The CRC-32 it replaced gets about 3 GB/s, and got 538 MB/s before it was rewritten to consume sixteen bytes at a time.
Those two numbers are why the format hashes each section rather than picking one digest and hoping it is never in the way.
