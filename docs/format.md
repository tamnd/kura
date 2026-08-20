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
  12..16  checksum of the body, u32
  16..24  body length, u64
  24..32  reserved, written as zero

body
  section table, 24 bytes per entry, section count entries
    0..2    kind, u16
    2..4    flags, u16
    4..8    padding, u32
    8..16   offset from the start of the body, u64
    16..24  length, u64
  section payloads, in the order the writer added them
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

**Section count, two bytes.**
It bounds the table before a single entry is read, which is what lets the reader check that the table fits inside the body in one comparison rather than discovering it entry by entry.
It also caps a segment at 65535 sections, which is several orders of magnitude more than any real one will hold.

**Checksum of the body, four bytes.**
CRC-32 with the IEEE polynomial, the same one zlib, gzip and PNG use.
It answers one question: are these the bytes that were written.
It is not a hash and it defends against nobody, and the code says so where it is defined.

The checksum covers the body and not the header, because the header holds the checksum.
Checksumming the body alone means a writer can assemble the body, checksum it, and then stamp the header over a reservation it made at the start, in one pass and with no second buffer.

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

**Padding, four bytes.**
It puts the offset and the length on an eight byte boundary within the entry, so reading the table costs no unaligned loads on the architectures that care and none of the compiler's alignment handling on the ones that do not.

**Offset and length, eight bytes each.**
Where the section is and how long it is, with the offset relative to the start of the body rather than the start of the file.
Relative to the body means a writer can build the body before it knows where in a file the segment will land.

## Section kinds

| Kind | Name | What it holds |
| --- | --- | --- |
| 1 | terms | the term dictionary, in prefix folded blocks, and where each term's postings start |
| 2 | postings | posting lists, packed in fixed size blocks, with term frequencies in a stream beside them |
| 3 | fields | stored field values, returned with a hit rather than searched |
| 4 | vectors | quantised vectors, one per passage |
| 5 | acl | the access control lists governing the documents in this segment |
| 6 | columns | columnar values, for filters and facets |
| 7 | graph | entities and edges |
| 8 | tombstones | documents deleted by a later segment |
| 9 | norms | how long each document is, and how long they are on average |

A section may be absent, and an absent section is not the same as an empty one.
A term dictionary with no terms is a fact about the segment.
A missing term dictionary is a fact about the build that wrote it.
The reader keeps them apart, and so should anything built on top.

## What the reader refuses

Opening a segment is where the engine decides whether to trust a file, so it is deliberately unforgiving.
Each of these has its own error rather than one opaque failure, because a caller that is told the file is short can do something different from a caller that is told the checksum did not match.

- Magic that is not this engine's.
- A format version this build does not write.
- A file shorter than the header, or shorter than the body length the header claims.
- A section table that does not fit inside the body.
- A section whose offset and length do not lie inside the body, or that overlaps the table.
- Two sections claiming the same kind.
- A checksum that does not match the body.

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

## Reading without the checksum

`Segment::open` verifies everything.
It is the one to use unless there is a measured reason not to.

`Segment::open_without_checksum` skips the checksum and nothing else.
The structural checks still run, so a section slice it hands back is still inside the input, and a corrupt section table is still refused.

The distinction matters, and it is worth more than an argument from first principles, so here is the measurement.
The segment holds a million document posting list and a megabyte term section, and the machine is an idle Intel i9-13900K running Windows.
It came to 2.1 MB when this was taken.
The posting format has since roughly halved that, which moves the verified figure down with it and leaves the skipped one exactly where it is, because one of them reads the data and the other reads the table.

```
open a segment, checksum verified               3479.9 us
open a segment, checksum skipped                   100 ns
```

Run it yourself with `cargo run --release --example bench`.

The verified figure is stable to within a microsecond across runs.
The skipped figure sits at the resolution of the platform timer, so read it as under a microsecond rather than as exactly 100 ns.
Either way the gap is around four orders of magnitude, and it grows with the size of the segment, because one side is a walk of a two entry table and the other is a pass over every byte.
That is the shape you want: opening should cost what the table costs, and verifying should cost what the data costs.

For a memory mapped file the difference is larger still, since verifying means faulting in every page rather than the one the table lives on.
That trade is worth making for a segment this process wrote and published and has not touched since.
It is not worth making for a file that arrived from somewhere else.

The checksum runs at about 610 MB/s, which is what a byte at a time table driven CRC-32 gets.
A slice by eight implementation would be several times quicker, and it is the obvious thing to do if checksumming ever shows up in a profile.
It has not yet, so it has not been written.
