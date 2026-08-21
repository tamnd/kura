//! A bitmap that picks a representation per chunk of the id space.
//!
//! Visibility is the thing this engine carries through every query: the set of
//! documents one reader is allowed to see. Those sets are wildly different
//! sizes and wildly different shapes. A contractor with access to two folders
//! produces a few hundred ids scattered across a corpus of a hundred million. A
//! member of a company wide group produces a set of millions that is very
//! nearly contiguous. One representation is wrong for at least one of them.
//!
//! # Chunks
//!
//! The id space is cut into chunks of 65536, so the high sixteen bits of an
//! ordinal select a chunk and the low sixteen index inside it. Only the chunks
//! that hold something exist, which is what makes the scattered set cheap: the
//! contractor's few hundred ids cost a few hundred entries no matter how far
//! apart they are, where a flat word array over the same corpus would cost
//! twelve megabytes of zeroes.
//!
//! This is the Roaring layout, from Chambi, Lemire, Kaser and Godin, *Better
//! bitmap performance with Roaring bitmaps*, and the run container from the
//! follow up, *Consistently faster and smaller compressed bitmaps with
//! Roaring*. It is written here rather than taken from the crate of the same
//! name because the core crate has no dependencies and that rule is worth more
//! than the week this took.
//!
//! # Three ways to hold a chunk
//!
//! Within a chunk there are three, and the one in use is whichever is smallest:
//!
//! - An **array** of the low sixteen bits, sorted. Two bytes a member, so it
//!   wins while the chunk is sparse.
//! - A **word block**, one bit per ordinal. A flat 8 KiB whatever is in it, so
//!   it wins once a chunk holds more than four thousand members and nothing
//!   about them is contiguous.
//! - A **run list**, a start and an inclusive end each. Four bytes a run, so a
//!   chunk that is one solid stretch costs four bytes rather than 8 KiB. This
//!   is the company wide group, and it is the case the two container version of
//!   Roaring handles worst.
//!
//! The choice is made by comparing the three costs rather than against a
//! threshold that would have to be kept in step with them, and it is remade
//! after every bulk build and every set operation.
//!
//! It is not remade on a single insert, beyond promoting an array that has
//! outgrown the word block, and it is not remade on a remove at all. A deny
//! list applied one id at a time would otherwise rebuild the chunk on every
//! call, and the bytes that would save are worth less than the rebuilds. So the
//! same set has more than one spelling here, and equality has to compare
//! members rather than representations.
//!
//! # Set operations
//!
//! Intersection, union and difference walk the two chunk lists by key and meet
//! only where both sides have something. Inside a chunk, the pairs that carry
//! the query path have a kernel of their own: two arrays merge, two word blocks
//! run a word loop, two run lists merge as intervals, an array against anything
//! else is a probe per member, and a word block meeting a run list works on the
//! gaps rather than the runs, because a permission filter is one long stretch
//! and its gaps are what there are few of.
//!
//! The pairs left over go through the word block form, which costs one 8 KiB
//! buffer for the duration of the operation and no more, because the result is
//! shrunk back before it is stored. That is a deliberate floor on how much code
//! this module is worth: nine pairs times three operations is twenty seven
//! kernels, and most of them would be answering a question no query asks.
//!
//! Everything is in place. Allocating a result set per query per term is
//! exactly the cost that stops a search engine scaling.
//!
//! # Written down
//!
//! [`Bitmap::write_to`] and [`Bitmap::read`] are the portable serialisation
//! format from the Roaring specification, not a private encoding that resembles
//! it. A tombstone bitmap has to survive a commit, and a permission set has to
//! cross into the host application, and the second of those only works if what
//! comes out is what another implementation reads.
//!
//! The three containers here are the three the specification names, so the
//! layout is a header of a cookie, a key and a cardinality per container, an
//! offset per container where the specification calls for one, and then the
//! containers in the order their keys are in.
//!
//! Which of the three a container is written as is decided from what it holds
//! and not from what it is being held in, because the reader decides the same
//! way and has nothing else to go on. That also means the bytes do not depend
//! on how the set was reached. A set inserted an id at a time and the same set
//! built from a sorted slice are held differently in memory, by the rule above,
//! and they come out as the same file.
//!
//! Reading is checked rather than trusted. Keys ascending, cardinalities
//! matching what the container holds, runs in order and not overlapping, and
//! every read inside the bytes it was given. A permission set that decodes into
//! somebody else's documents is the worst thing this file could do, so a bitmap
//! that is wrong is an error and never a set.

use crate::DocId;
use crate::codec::{get_u16, get_u32, put_u16, put_u32, split_at};
use crate::error::{Error, Result};

/// Ordinals in a chunk.
const CHUNK: usize = 1 << 16;

/// Words in a chunk held as bits.
const WORDS: usize = CHUNK / 64;

/// The most members a container holds as an array rather than as words.
///
/// Eight kilobytes of words against two bytes a member, so this is where the
/// two cost the same. It is a constant rather than a comparison because the
/// serialisation format says which container a count means, and a reader that
/// worked it out differently would read somebody else's bitmap wrongly.
const ARRAY_MAX: usize = BITS_BYTES / 2;

/// What a chunk held as bits costs, in bytes.
///
/// Every other representation is chosen by being cheaper than this, which is
/// why it is the only size constant here. An array holding this many bytes is
/// four thousand and ninety six members and a run list holding it is two
/// thousand and forty eight runs, and neither of those numbers has to be
/// written down to be enforced.
const BITS_BYTES: usize = WORDS * 8;

/// A set of document ordinals.
#[derive(Debug, Clone, Default)]
pub struct Bitmap {
    /// Ascending by key, and never holding an empty chunk.
    chunks: Vec<Chunk>,
}

#[derive(Debug, Clone)]
struct Chunk {
    key: u16,
    store: Store,
}

#[derive(Debug, Clone)]
enum Store {
    /// The low sixteen bits of each member, ascending and deduplicated.
    Array(Vec<u16>),
    /// One bit per ordinal, little endian within each word.
    Bits(Box<Words>),
    /// Ascending, non overlapping, non adjacent stretches.
    Runs(Vec<Run>),
}

#[derive(Debug, Clone)]
struct Words {
    bits: [u64; WORDS],
    /// Kept alongside the words so that [`Bitmap::len`] does not have to add up
    /// a thousand population counts every time a scorer asks how many documents
    /// a reader can see.
    count: u32,
}

/// A stretch of ordinals from `start` to `last`, both included.
///
/// The end is inclusive rather than a length because a run covering a whole
/// chunk is 65536 long, which does not fit in the two bytes the start takes,
/// and that run is the one this representation exists for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Run {
    start: u16,
    last: u16,
}

impl Bitmap {
    /// Returns an empty bitmap.
    #[must_use]
    pub const fn new() -> Self {
        Self { chunks: Vec::new() }
    }

    /// Returns an empty bitmap ready to hold ordinals below `capacity` without
    /// growing its chunk list again.
    ///
    /// It reserves chunk slots rather than members, because how much a member
    /// costs is not known until it is known which chunk it lands in.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            chunks: Vec::with_capacity(capacity.div_ceil(CHUNK)),
        }
    }

    /// Builds a bitmap from ordinals that are already ascending and unique.
    ///
    /// This is the fast path off disk, where the posting list decoder has
    /// already produced them in order. Input that is not ascending is accepted
    /// and normalised at the cost of a sort, because refusing it would push the
    /// check onto every caller.
    #[must_use]
    pub fn from_sorted(ordinals: &[DocId]) -> Self {
        let mut owned = ordinals.to_vec();
        if !owned.is_sorted() {
            owned.sort_unstable();
        }
        owned.dedup();

        let mut chunks: Vec<Chunk> = Vec::new();
        for ordinal in owned {
            let (key, low) = split(ordinal);
            match chunks.last_mut() {
                Some(chunk) if chunk.key == key => match &mut chunk.store {
                    Store::Array(list) => list.push(low),
                    _ => unreachable!("a chunk under construction is always an array"),
                },
                _ => chunks.push(Chunk {
                    key,
                    store: Store::Array(vec![low]),
                }),
            }
        }
        for chunk in &mut chunks {
            chunk.store.shrink();
        }
        Self { chunks }
    }

    /// Adds an ordinal and reports whether it was not already there.
    pub fn insert(&mut self, ordinal: DocId) -> bool {
        let (key, low) = split(ordinal);
        match self.seek(key) {
            Ok(at) => self.chunks[at].store.insert(low),
            Err(at) => {
                self.chunks.insert(
                    at,
                    Chunk {
                        key,
                        store: Store::Array(vec![low]),
                    },
                );
                true
            }
        }
    }

    /// Removes an ordinal and reports whether it was there.
    pub fn remove(&mut self, ordinal: DocId) -> bool {
        let (key, low) = split(ordinal);
        let Ok(at) = self.seek(key) else {
            return false;
        };
        let removed = self.chunks[at].store.remove(low);
        if self.chunks[at].store.is_empty() {
            self.chunks.remove(at);
        }
        removed
    }

    /// Reports whether the ordinal is in the set.
    #[must_use]
    pub fn contains(&self, ordinal: DocId) -> bool {
        let (key, low) = split(ordinal);
        self.seek(key)
            .is_ok_and(|at| self.chunks[at].store.contains(low))
    }

    /// Returns how many ordinals are in the set.
    #[must_use]
    pub fn len(&self) -> usize {
        self.chunks.iter().map(|chunk| chunk.store.len()).sum()
    }

    /// Reports whether the set is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }

    /// The largest ordinal in the set, or nothing if it holds none.
    ///
    /// Cheap whatever shape the last chunk is in, which is what makes it worth
    /// having: a set of deletions can be checked against the size of what it
    /// deletes from without walking it.
    #[must_use]
    pub fn max(&self) -> Option<DocId> {
        let chunk = self.chunks.last()?;
        let low = chunk.store.last()?;
        Some(DocId::from(chunk.key) << 16 | DocId::from(low))
    }

    /// The smallest ordinal the set does not hold.
    ///
    /// Zero for a set that does not start at zero, which includes the empty set.
    ///
    /// This is how a set of deletions says where the documents that are still
    /// worth reading begin. A store that deletes from the front, which is what
    /// deleting the oldest of anything looks like, ends up with a long prefix of
    /// gone documents in its first segment, and a compaction that knows where
    /// the prefix ends can skip it without asking about each one.
    ///
    /// The walk costs the length of that prefix and stops at the first gap, so
    /// it is as long as the answer is and no longer.
    #[must_use]
    pub fn first_absent(&self) -> DocId {
        let mut next = 0;
        for ordinal in self {
            if ordinal != next {
                break;
            }
            next = ordinal.saturating_add(1);
        }
        next
    }

    /// Returns roughly what the set costs in memory, in bytes.
    ///
    /// This is the number the whole module is about, so it is readable rather
    /// than inferred from a heap profiler. It counts the chunk list and each
    /// chunk's payload, and not the capacity a vector may be holding beyond its
    /// length, so two sets built differently and holding the same ordinals
    /// report the same figure.
    #[must_use]
    pub fn memory(&self) -> usize {
        let list = self.chunks.len() * size_of::<Chunk>();
        list + self
            .chunks
            .iter()
            .map(|chunk| chunk.store.memory())
            .sum::<usize>()
    }

    /// Keeps only the ordinals that are also in `other`.
    ///
    /// This is the operation the query path runs most: candidates from a term,
    /// intersected with what the reader may see.
    pub fn intersect_with(&mut self, other: &Self) {
        let mut theirs = 0;
        self.chunks.retain_mut(|chunk| {
            while theirs < other.chunks.len() && other.chunks[theirs].key < chunk.key {
                theirs += 1;
            }
            let Some(found) = other.chunks.get(theirs) else {
                return false;
            };
            if found.key != chunk.key {
                return false;
            }
            chunk.store.intersect(&found.store);
            !chunk.store.is_empty()
        });
    }

    /// Adds every ordinal of `other`.
    pub fn union_with(&mut self, other: &Self) {
        let mut merged = Vec::with_capacity(self.chunks.len() + other.chunks.len());
        let mut mine = core::mem::take(&mut self.chunks).into_iter().peekable();
        let mut theirs = other.chunks.iter().peekable();

        loop {
            match (mine.peek(), theirs.peek()) {
                (Some(ours), Some(found)) if ours.key == found.key => {
                    let mut chunk = mine.next().unwrap_or_else(|| unreachable!());
                    chunk.store.union(&found.store);
                    merged.push(chunk);
                    theirs.next();
                }
                (Some(ours), Some(found)) if ours.key < found.key => {
                    merged.extend(mine.next());
                }
                (Some(_) | None, Some(_)) => {
                    merged.extend(theirs.next().cloned());
                }
                (Some(_), None) => merged.extend(mine.next()),
                (None, None) => break,
            }
        }
        self.chunks = merged;
    }

    /// Removes every ordinal of `other`.
    ///
    /// A deny list is applied with this, which is why it exists as its own
    /// operation rather than as a union of complements: a complement needs a
    /// universe size, and the universe here is whatever the segment happens to
    /// hold.
    pub fn difference_with(&mut self, other: &Self) {
        let mut theirs = 0;
        self.chunks.retain_mut(|chunk| {
            while theirs < other.chunks.len() && other.chunks[theirs].key < chunk.key {
                theirs += 1;
            }
            match other.chunks.get(theirs) {
                Some(found) if found.key == chunk.key => {
                    chunk.store.difference(&found.store);
                    !chunk.store.is_empty()
                }
                _ => true,
            }
        });
    }

    /// Iterates the ordinals in ascending order.
    #[must_use]
    pub fn iter(&self) -> Iter<'_> {
        Iter {
            chunks: &self.chunks,
            chunk: 0,
            at: 0,
            offset: 0,
            word: 0,
            word_index: 0,
            primed: false,
        }
    }

    /// Collects the ordinals into a vector, in ascending order.
    #[must_use]
    pub fn to_vec(&self) -> Vec<DocId> {
        self.iter().collect()
    }

    /// The index of the chunk holding `key`, or where one would go.
    fn seek(&self, key: u16) -> core::result::Result<usize, usize> {
        self.chunks.binary_search_by_key(&key, |chunk| chunk.key)
    }

    /// The shape each container goes down as, in order.
    ///
    /// Asked once and carried through the write, because deciding it walks the
    /// container and because everything else about the layout follows from it:
    /// which cookie the header carries, whether the offsets are written, what
    /// they are, and how long the whole thing is.
    fn shapes(&self) -> Vec<Shape> {
        self.chunks
            .iter()
            .map(|chunk| chunk.store.shape())
            .collect()
    }

    /// How many bytes come before the first container.
    fn header(shapes: &[Shape]) -> usize {
        let containers = shapes.len();
        let runs = shapes.iter().any(|shape| matches!(shape, Shape::Runs(_)));
        let cookie = if runs {
            COOKIE_LEN + containers.div_ceil(8)
        } else {
            COOKIE_LEN + COUNT_LEN
        };
        cookie + DESCRIPTOR_LEN * containers + offsets_len(containers, runs)
    }

    /// How many bytes [`Bitmap::write_to`] appends.
    ///
    /// This is what the set costs written down, which is not what it costs in
    /// memory. [`Bitmap::memory`] is the other number.
    #[must_use]
    pub fn size(&self) -> usize {
        let shapes = self.shapes();
        Self::header(&shapes) + shapes.iter().map(|shape| shape.size()).sum::<usize>()
    }

    /// Writes the set in the portable Roaring format.
    pub fn write_to(&self, out: &mut Vec<u8>) {
        let containers = self.chunks.len();
        let shapes = self.shapes();
        let runs = shapes.iter().any(|shape| matches!(shape, Shape::Runs(_)));

        if runs {
            // The count goes in the top half of the cookie, less one, which is
            // why a bitmap of no containers is never written this way.
            let above = u32::try_from(containers - 1).unwrap_or(u32::MAX);
            put_u32(out, COOKIE_RUNS | (above << 16));
            let mut flags = vec![0u8; containers.div_ceil(8)];
            for (at, shape) in shapes.iter().enumerate() {
                if matches!(shape, Shape::Runs(_)) {
                    flags[at / 8] |= 1 << (at % 8);
                }
            }
            out.extend_from_slice(&flags);
        } else {
            put_u32(out, COOKIE_PLAIN);
            put_u32(out, u32::try_from(containers).unwrap_or(u32::MAX));
        }

        for chunk in &self.chunks {
            put_u16(out, chunk.key);
            // One less than the count, so that a container holding a whole
            // chunk fits in the two bytes it is given. A container is never
            // empty, so nothing is lost by it.
            let last = u16::try_from(chunk.store.len() - 1).unwrap_or(u16::MAX);
            put_u16(out, last);
        }

        if offsets_len(containers, runs) > 0 {
            // Measured from the first byte of the bitmap, and the offsets are
            // themselves part of what comes before the first container.
            let mut at = Self::header(&shapes);
            for shape in &shapes {
                put_u32(out, u32::try_from(at).unwrap_or(u32::MAX));
                at += shape.size();
            }
        }

        for (chunk, shape) in self.chunks.iter().zip(&shapes) {
            chunk.store.write_to(*shape, out);
        }
    }

    /// Reads a set written in the portable Roaring format.
    ///
    /// # Errors
    ///
    /// Returns [`Error::BadMagic`] if the cookie is not one the format defines,
    /// [`Error::Truncated`] if the bytes end inside anything, [`Error::NotSorted`]
    /// if the keys or the members of a container are not ascending, and
    /// [`Error::BadCardinality`] if a container does not hold what the header
    /// said it holds.
    pub fn read(bytes: &[u8]) -> Result<Self> {
        let (cookie, mut rest) = get_u32(bytes)?;
        let runs = cookie & 0xffff == COOKIE_RUNS;
        let containers = if runs {
            // The count is the top half of the cookie, less one.
            (cookie >> 16) as usize + 1
        } else if cookie == COOKIE_PLAIN {
            let count;
            (count, rest) = get_u32(rest)?;
            usize::try_from(count).map_err(|_| Error::Overflow)?
        } else {
            return Err(Error::BadMagic);
        };

        // One bit per container saying which are held as runs, and nothing at
        // all when the cookie already said none of them are.
        let mut flags: &[u8] = &[];
        if runs {
            (flags, rest) = split_at(rest, containers.div_ceil(8))?;
        }

        // The count comes out of the bytes, so it is only believed as far as
        // the reserve. Keys are two bytes and ascending, so a bitmap can hold
        // no more chunks than there are keys, and any larger count is going to
        // run out of bytes before it runs out of chunks.
        let reserve = containers.min(CHUNK);
        let mut keys: Vec<(u16, usize)> = Vec::with_capacity(reserve);
        for _ in 0..containers {
            let key;
            let last;
            (key, rest) = get_u16(rest)?;
            (last, rest) = get_u16(rest)?;
            if keys.last().is_some_and(|&(before, _)| key <= before) {
                return Err(Error::NotSorted { at: u32::from(key) });
            }
            // The cardinality is written one less than it is, so a container
            // holding a whole chunk fits in two bytes and an empty one cannot
            // be spelled at all.
            keys.push((key, usize::from(last) + 1));
        }

        let mut offsets: Vec<usize> = Vec::new();
        if offsets_len(containers, runs) > 0 {
            for _ in 0..containers {
                let offset;
                (offset, rest) = get_u32(rest)?;
                offsets.push(usize::try_from(offset).map_err(|_| Error::Overflow)?);
            }
        }

        let mut chunks = Vec::with_capacity(reserve);
        for (at, &(key, count)) in keys.iter().enumerate() {
            let reached = bytes.len() - rest.len();
            if offsets.get(at).is_some_and(|&offset| offset != reached) {
                // The offsets say the same thing as the order the containers
                // are in, which is what makes them worth checking rather than
                // skipping: a file where the two disagree is a file where one
                // of them is wrong.
                return Err(Error::BadOffset {
                    stated: offsets[at],
                    found: reached,
                });
            }
            let held_as_runs = flags
                .get(at / 8)
                .is_some_and(|byte| byte >> (at % 8) & 1 == 1);
            let store;
            (store, rest) = Store::read(rest, count, held_as_runs)?;
            chunks.push(Chunk { key, store });
        }
        Ok(Self { chunks })
    }
}

/// The cookie of a bitmap where no container is held as runs.
const COOKIE_PLAIN: u32 = 12_346;

/// The cookie of a bitmap where at least one is, which also carries the
/// container count in its top half.
const COOKIE_RUNS: u32 = 12_347;

/// The bytes the cookie takes.
const COOKIE_LEN: usize = 4;

/// The bytes the container count takes, when it is not in the cookie.
const COUNT_LEN: usize = 4;

/// The bytes a key and a cardinality take, per container.
const DESCRIPTOR_LEN: usize = 4;

/// The container count at which a bitmap holding runs still carries offsets.
///
/// A bitmap with no runs always carries them. One with runs carries them only
/// once there are enough containers for skipping to be worth the four bytes
/// each, and the specification puts that at four.
const OFFSETS_FROM: usize = 4;

/// How many bytes the offsets take, which is none for a small bitmap of runs.
const fn offsets_len(containers: usize, runs: bool) -> usize {
    if !runs || containers >= OFFSETS_FROM {
        4 * containers
    } else {
        0
    }
}

/// Two bitmaps are equal when they hold the same ordinals.
///
/// It cannot be derived. The same set has more than one spelling here, and a
/// set that reached a chunk by inserting is allowed to be holding it as words
/// where a set that was built from a sorted slice is holding it as runs.
impl PartialEq for Bitmap {
    fn eq(&self, other: &Self) -> bool {
        self.chunks.len() == other.chunks.len()
            && self
                .chunks
                .iter()
                .zip(other.chunks.iter())
                .all(|(ours, theirs)| ours.key == theirs.key && ours.store == theirs.store)
    }
}

impl Eq for Bitmap {}

impl PartialEq for Store {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Array(mine), Self::Array(theirs)) => mine == theirs,
            (Self::Runs(mine), Self::Runs(theirs)) => mine == theirs,
            (Self::Bits(mine), Self::Bits(theirs)) => mine.bits == theirs.bits,
            _ => self.len() == other.len() && self.iter().eq(other.iter()),
        }
    }
}

impl Eq for Store {}

impl FromIterator<DocId> for Bitmap {
    fn from_iter<I: IntoIterator<Item = DocId>>(iter: I) -> Self {
        let mut collected: Vec<DocId> = iter.into_iter().collect();
        collected.sort_unstable();
        Self::from_sorted(&collected)
    }
}

impl<'a> IntoIterator for &'a Bitmap {
    type Item = DocId;
    type IntoIter = Iter<'a>;

    fn into_iter(self) -> Iter<'a> {
        self.iter()
    }
}

/// Ascending ordinals, across every chunk and every representation.
pub struct Iter<'a> {
    chunks: &'a [Chunk],
    chunk: usize,
    /// The next entry of an array, or the run being walked.
    at: usize,
    /// How far into that run the walk has got.
    offset: u32,
    /// The remaining bits of the word block word being drained.
    word: u64,
    word_index: usize,
    primed: bool,
}

impl Iterator for Iter<'_> {
    type Item = DocId;

    fn next(&mut self) -> Option<DocId> {
        loop {
            let chunk = self.chunks.get(self.chunk)?;
            let base = u32::from(chunk.key) << 16;
            match &chunk.store {
                Store::Array(list) => {
                    if let Some(low) = list.get(self.at) {
                        self.at += 1;
                        return Some(base | u32::from(*low));
                    }
                }
                Store::Runs(runs) => {
                    if let Some(run) = runs.get(self.at) {
                        let span = u32::from(run.last) - u32::from(run.start);
                        if self.offset <= span {
                            let low = u32::from(run.start) + self.offset;
                            self.offset += 1;
                            return Some(base | low);
                        }
                        self.at += 1;
                        self.offset = 0;
                        continue;
                    }
                }
                Store::Bits(words) => {
                    if !self.primed {
                        self.word = words.bits[0];
                        self.primed = true;
                    }
                    loop {
                        if self.word != 0 {
                            let bit = self.word.trailing_zeros();
                            // Clearing the lowest set bit is one instruction and
                            // avoids re scanning the word from the start.
                            self.word &= self.word - 1;
                            let low = u32::try_from(self.word_index * 64).ok()? + bit;
                            return Some(base | low);
                        }
                        self.word_index += 1;
                        match words.bits.get(self.word_index) {
                            Some(next) => self.word = *next,
                            None => break,
                        }
                    }
                }
            }

            self.chunk += 1;
            self.at = 0;
            self.offset = 0;
            self.word = 0;
            self.word_index = 0;
            self.primed = false;
        }
    }
}

/// Which of the three a container goes down as, and how much of it there is.
///
/// Not the same question as which of the three it is being held in. A reader
/// takes the shape from the run flag and the cardinality and not from anything
/// the writer says about it, so the writer picks the shape from what the
/// container holds rather than from the variant it happens to be in. A chunk
/// filled past the point where words are cheapest and then emptied again is
/// still words in memory, and a chunk reached by inserting a member at a time
/// is still an array however long a stretch of them it ended up with, and
/// neither of those is what either one should cost on disk.
#[derive(Debug, Clone, Copy)]
enum Shape {
    /// Two bytes per member, up to [`ARRAY_MAX`] of them.
    Array(usize),
    /// One bit per ordinal in the chunk, whatever it holds.
    Words,
    /// Two bytes of count and four bytes per run.
    Runs(usize),
}

impl Shape {
    /// How many bytes the container takes, without the key and the count.
    const fn size(self) -> usize {
        match self {
            Self::Array(members) => 2 * members,
            Self::Words => BITS_BYTES,
            Self::Runs(runs) => 2 + 4 * runs,
        }
    }
}

impl Store {
    /// Which shape this container is cheapest written down as.
    fn shape(&self) -> Shape {
        let members = self.len();
        let as_array = 2 * members;
        // Runs only have to be counted until they cost more than the cheaper of
        // the other two, the same as in [`Store::shrink`], and the two agree on
        // purpose: a container already held in its cheapest shape should not
        // have to be walked again to find that out.
        let runs = self.runs_within(as_array.min(BITS_BYTES) / 4);
        if 4 * runs < as_array && 4 * runs < BITS_BYTES {
            return Shape::Runs(runs);
        }
        if members <= ARRAY_MAX {
            Shape::Array(members)
        } else {
            Shape::Words
        }
    }

    /// Writes the container, without the key and the count that name it.
    fn write_to(&self, shape: Shape, out: &mut Vec<u8>) {
        match shape {
            Shape::Array(members) => {
                debug_assert_eq!(members, self.len());
                for low in self.iter() {
                    put_u16(out, low);
                }
            }
            Shape::Words => match self {
                Self::Bits(words) => {
                    for word in words.bits {
                        out.extend_from_slice(&word.to_le_bytes());
                    }
                }
                other => {
                    for word in other.to_words().bits {
                        out.extend_from_slice(&word.to_le_bytes());
                    }
                }
            },
            Shape::Runs(count) => {
                put_u16(out, u16::try_from(count).unwrap_or(u16::MAX));
                for run in self.collect_runs(count) {
                    put_u16(out, run.start);
                    // A length rather than an end, and one less than the length
                    // at that, for the same reason the cardinality is: a run
                    // covering a whole chunk has to fit in two bytes.
                    put_u16(out, run.last - run.start);
                }
            }
        }
    }

    /// Reads a container holding `count` members, and says what is left.
    ///
    /// Which of the three it is has already been decided by the caller: the run
    /// flag says whether it is runs, and the specification says a container of
    /// more than four thousand and ninety six members is words and anything
    /// else is an array.
    fn read(bytes: &[u8], count: usize, held_as_runs: bool) -> Result<(Self, &[u8])> {
        if held_as_runs {
            let (len, mut rest) = get_u16(bytes)?;
            let mut runs: Vec<Run> = Vec::with_capacity(usize::from(len));
            let mut held = 0usize;
            for _ in 0..len {
                let start;
                let length;
                (start, rest) = get_u16(rest)?;
                (length, rest) = get_u16(rest)?;
                let last = start.checked_add(length).ok_or(Error::Overflow)?;
                match runs.last_mut() {
                    Some(before) if start <= before.last => {
                        return Err(Error::NotSorted {
                            at: u32::from(start),
                        });
                    }
                    // Adjacent runs are one run here, because every operation
                    // in this module takes that for granted. Merging them costs
                    // nothing and refusing them would refuse a bitmap that is
                    // not wrong, only written by somebody who did not merge.
                    Some(before) if start == before.last + 1 => before.last = last,
                    _ => runs.push(Run { start, last }),
                }
                held += usize::from(length) + 1;
            }
            if held != count {
                return Err(Error::BadCardinality {
                    stated: count,
                    found: held,
                });
            }
            return Ok((Self::Runs(runs), rest));
        }

        if count > ARRAY_MAX {
            let (bits, rest) = split_at(bytes, BITS_BYTES)?;
            let mut words = Box::new(Words {
                bits: [0; WORDS],
                count: 0,
            });
            let mut held = 0u32;
            for (word, eight) in words.bits.iter_mut().zip(bits.chunks_exact(8)) {
                let mut eight_bytes = [0u8; 8];
                eight_bytes.copy_from_slice(eight);
                *word = u64::from_le_bytes(eight_bytes);
                held += word.count_ones();
            }
            words.count = held;
            if held as usize != count {
                return Err(Error::BadCardinality {
                    stated: count,
                    found: held as usize,
                });
            }
            return Ok((Self::Bits(words), rest));
        }

        let (mut rest, mut list) = (bytes, Vec::with_capacity(count));
        for _ in 0..count {
            let low;
            (low, rest) = get_u16(rest)?;
            if list.last().is_some_and(|&before| low <= before) {
                return Err(Error::NotSorted { at: u32::from(low) });
            }
            list.push(low);
        }
        Ok((Self::Array(list), rest))
    }

    fn len(&self) -> usize {
        match self {
            Self::Array(list) => list.len(),
            Self::Bits(words) => words.count as usize,
            Self::Runs(runs) => runs
                .iter()
                .map(|run| usize::from(run.last) - usize::from(run.start) + 1)
                .sum(),
        }
    }

    /// The largest member of the chunk, or nothing if it holds none.
    fn last(&self) -> Option<u16> {
        match self {
            Self::Array(list) => list.last().copied(),
            Self::Bits(words) => words
                .bits
                .iter()
                .rposition(|&word| word != 0)
                .and_then(|at| {
                    let highest = words.bits[at].ilog2();
                    let at = u32::try_from(at).ok()?;
                    u16::try_from(at * 64 + highest).ok()
                }),
            Self::Runs(runs) => runs.last().map(|run| run.last),
        }
    }

    fn is_empty(&self) -> bool {
        match self {
            Self::Array(list) => list.is_empty(),
            Self::Bits(words) => words.count == 0,
            Self::Runs(runs) => runs.is_empty(),
        }
    }

    fn contains(&self, low: u16) -> bool {
        match self {
            Self::Array(list) => list.binary_search(&low).is_ok(),
            Self::Bits(words) => words.contains(low),
            Self::Runs(runs) => covered(runs, low).is_ok(),
        }
    }

    fn memory(&self) -> usize {
        match self {
            Self::Array(list) => list.len() * 2,
            Self::Bits(_) => BITS_BYTES + size_of::<u32>(),
            Self::Runs(runs) => runs.len() * 4,
        }
    }

    fn iter(&self) -> WalkStore<'_> {
        WalkStore::new(self)
    }

    /// Adds `low` and reports whether it was not already there.
    fn insert(&mut self, low: u16) -> bool {
        match self {
            Self::Array(list) => {
                let Err(at) = list.binary_search(&low) else {
                    return false;
                };
                list.insert(at, low);
                if list.len() * 2 > BITS_BYTES {
                    self.shrink();
                }
                true
            }
            Self::Bits(words) => words.insert(low),
            Self::Runs(runs) => runs_insert(runs, low),
        }
    }

    /// Removes `low` and reports whether it was there.
    fn remove(&mut self, low: u16) -> bool {
        match self {
            Self::Array(list) => {
                let Ok(at) = list.binary_search(&low) else {
                    return false;
                };
                list.remove(at);
                true
            }
            Self::Bits(words) => words.remove(low),
            Self::Runs(runs) => runs_remove(runs, low),
        }
    }

    /// Renders the chunk as words, whatever it is holding now.
    fn to_words(&self) -> Words {
        match self {
            Self::Bits(words) => (**words).clone(),
            Self::Array(list) => {
                let mut words = Words::empty();
                for low in list {
                    words.insert(*low);
                }
                words
            }
            Self::Runs(runs) => {
                let mut words = Words::empty();
                for run in runs {
                    words.fill(*run);
                }
                words
            }
        }
    }

    /// How many stretches of consecutive ordinals the chunk holds.
    ///
    /// Nothing outside a test wants the exact number, because everything that
    /// asks is deciding between representations and stops as soon as the answer
    /// cannot change.
    #[cfg(test)]
    fn runs(&self) -> usize {
        self.runs_within(usize::MAX)
    }

    /// The same count, given up on once it passes `cap`.
    ///
    /// The answer is exact while it is at or below `cap` and merely larger than
    /// `cap` above it, which is all [`Store::shrink`] needs and is the
    /// difference between reading a word or two of a scattered chunk and
    /// reading all thousand of them.
    fn runs_within(&self, cap: usize) -> usize {
        match self {
            Self::Runs(runs) => runs.len(),
            Self::Array(list) => {
                let mut count = 0;
                let mut previous = None;
                for low in list {
                    if previous.map(|before| before + 1) != Some(u32::from(*low)) {
                        count += 1;
                        if count > cap {
                            return count;
                        }
                    }
                    previous = Some(u32::from(*low));
                }
                count
            }
            Self::Bits(words) => words.runs_within(cap),
        }
    }

    /// Switches to whichever of the three is smallest.
    fn shrink(&mut self) {
        let len = self.len();
        let as_array = len * 2;
        // Runs only have to be counted until they cost more than the cheaper of
        // the other two, and on a chunk they cannot describe that happens in
        // the first word or so.
        let runs = self.runs_within(as_array.min(BITS_BYTES) / 4);
        let as_runs = runs * 4;

        if as_runs < as_array && as_runs < BITS_BYTES {
            if !matches!(self, Self::Runs(_)) {
                *self = Self::Runs(self.collect_runs(runs));
            }
            return;
        }
        if as_array < BITS_BYTES {
            if !matches!(self, Self::Array(_)) {
                *self = Self::Array(self.collect_array(len));
            }
            return;
        }
        if !matches!(self, Self::Bits(_)) {
            *self = Self::Bits(Box::new(self.to_words()));
        }
    }

    fn collect_array(&self, len: usize) -> Vec<u16> {
        let mut out = Vec::with_capacity(len);
        out.extend(WalkStore::new(self));
        out
    }

    fn collect_runs(&self, count: usize) -> Vec<Run> {
        let mut out: Vec<Run> = Vec::with_capacity(count);
        for low in WalkStore::new(self) {
            match out.last_mut() {
                Some(run) if u32::from(run.last) + 1 == u32::from(low) => run.last = low,
                _ => out.push(Run {
                    start: low,
                    last: low,
                }),
            }
        }
        out
    }

    fn intersect(&mut self, other: &Self) {
        let replacement = match (&mut *self, other) {
            (Self::Array(mine), Self::Array(theirs)) => {
                keep_common(mine, theirs);
                None
            }
            (Self::Array(mine), _) => {
                mine.retain(|low| other.contains(*low));
                None
            }
            (Self::Runs(mine), Self::Runs(theirs)) => {
                *mine = runs_intersect(mine, theirs);
                None
            }
            (Self::Bits(mine), Self::Bits(theirs)) => {
                for (word, their) in mine.bits.iter_mut().zip(theirs.bits.iter()) {
                    *word &= *their;
                }
                mine.recount();
                None
            }
            // A group wide permission filter meeting a term's candidates is
            // this pair, so it does not go the long way round. Clearing the
            // gaps between the runs touches a word per gap rather than
            // rendering the runs as words and then reading them back.
            (Self::Bits(mine), Self::Runs(theirs)) => {
                keep_within(mine, theirs);
                None
            }
            (Self::Runs(mine), Self::Bits(theirs)) => {
                let mut words = (**theirs).clone();
                keep_within(&mut words, mine);
                Some(Self::Bits(Box::new(words)))
            }
            // Whatever is left, the result cannot be larger than the other side
            // when the other side is an array, so build one rather than a word
            // block that would be shrunk right back.
            (_, Self::Array(theirs)) => Some(Self::Array(
                theirs
                    .iter()
                    .copied()
                    .filter(|low| self.contains(*low))
                    .collect(),
            )),
        };
        if let Some(store) = replacement {
            *self = store;
        }
        self.shrink();
    }

    fn union(&mut self, other: &Self) {
        let replacement = match (&mut *self, other) {
            (Self::Array(mine), Self::Array(theirs)) => {
                add_all(mine, theirs);
                None
            }
            (Self::Runs(mine), Self::Runs(theirs)) => {
                *mine = runs_union(mine, theirs);
                None
            }
            (Self::Bits(mine), Self::Bits(theirs)) => {
                for (word, their) in mine.bits.iter_mut().zip(theirs.bits.iter()) {
                    *word |= *their;
                }
                mine.recount();
                None
            }
            (Self::Bits(mine), Self::Array(theirs)) => {
                for low in theirs {
                    mine.insert(*low);
                }
                None
            }
            (Self::Bits(mine), Self::Runs(theirs)) => {
                for run in theirs {
                    mine.fill(*run);
                }
                None
            }
            _ => {
                let mut mine = self.to_words();
                let theirs = other.to_words();
                for (word, their) in mine.bits.iter_mut().zip(theirs.bits.iter()) {
                    *word |= *their;
                }
                mine.recount();
                Some(Self::Bits(Box::new(mine)))
            }
        };
        if let Some(store) = replacement {
            *self = store;
        }
        self.shrink();
    }

    fn difference(&mut self, other: &Self) {
        let replacement = match (&mut *self, other) {
            (Self::Array(mine), _) => {
                mine.retain(|low| !other.contains(*low));
                None
            }
            (Self::Runs(mine), Self::Runs(theirs)) => {
                *mine = runs_difference(mine, theirs);
                None
            }
            (Self::Bits(mine), Self::Bits(theirs)) => {
                for (word, their) in mine.bits.iter_mut().zip(theirs.bits.iter()) {
                    *word &= !*their;
                }
                mine.recount();
                None
            }
            (Self::Bits(mine), Self::Array(theirs)) => {
                for low in theirs {
                    mine.remove(*low);
                }
                None
            }
            (Self::Bits(mine), Self::Runs(theirs)) => {
                for run in theirs {
                    mine.clear(*run);
                }
                None
            }
            _ => {
                let mut mine = self.to_words();
                let theirs = other.to_words();
                for (word, their) in mine.bits.iter_mut().zip(theirs.bits.iter()) {
                    *word &= !*their;
                }
                mine.recount();
                Some(Self::Bits(Box::new(mine)))
            }
        };
        if let Some(store) = replacement {
            *self = store;
        }
        self.shrink();
    }
}

impl Words {
    fn empty() -> Self {
        Self {
            bits: [0; WORDS],
            count: 0,
        }
    }

    fn contains(&self, low: u16) -> bool {
        let (word, mask) = at(low);
        self.bits[word] & mask != 0
    }

    fn insert(&mut self, low: u16) -> bool {
        let (word, mask) = at(low);
        let was = self.bits[word] & mask != 0;
        self.bits[word] |= mask;
        if !was {
            self.count += 1;
        }
        !was
    }

    fn remove(&mut self, low: u16) -> bool {
        let (word, mask) = at(low);
        let was = self.bits[word] & mask != 0;
        self.bits[word] &= !mask;
        if was {
            self.count -= 1;
        }
        was
    }

    fn fill(&mut self, run: Run) {
        self.span(run, true);
    }

    fn clear(&mut self, run: Run) {
        self.span(run, false);
    }

    /// Sets or clears every bit from `run.start` to `run.last`.
    ///
    /// The two ends are masked and everything between them is a whole word, so
    /// a run covering the chunk is a thousand word writes rather than sixty
    /// five thousand bit writes.
    fn span(&mut self, run: Run, set: bool) {
        let (first, low_bit) = (usize::from(run.start) / 64, u32::from(run.start) % 64);
        let (last, high_bit) = (usize::from(run.last) / 64, u32::from(run.last) % 64);
        let low_mask = u64::MAX << low_bit;
        let high_mask = if high_bit == 63 {
            u64::MAX
        } else {
            (1u64 << (high_bit + 1)) - 1
        };

        if first == last {
            self.apply(first, low_mask & high_mask, set);
            return;
        }
        self.apply(first, low_mask, set);
        for word in first + 1..last {
            self.apply(word, u64::MAX, set);
        }
        self.apply(last, high_mask, set);
    }

    fn apply(&mut self, word: usize, mask: u64, set: bool) {
        let before = self.bits[word];
        self.bits[word] = if set { before | mask } else { before & !mask };
        let after = self.bits[word];
        self.count = self.count.wrapping_add(after.count_ones());
        self.count = self.count.wrapping_sub(before.count_ones());
    }

    fn recount(&mut self) {
        self.count = self.bits.iter().map(|word| word.count_ones()).sum();
    }

    /// How many stretches of consecutive set bits there are, given up on once
    /// the count passes `cap`.
    ///
    /// A bit starts a run when it is set and the bit below it is not, which
    /// across a word boundary means the top bit of the word below.
    fn runs_within(&self, cap: usize) -> usize {
        let mut carry = 0u64;
        let mut count = 0usize;
        for word in &self.bits {
            count += (word & !((word << 1) | carry)).count_ones() as usize;
            if count > cap {
                return count;
            }
            carry = word >> 63;
        }
        count
    }
}

/// A walk over one chunk's low sixteen bits, whatever it is holding.
struct WalkStore<'a> {
    store: &'a Store,
    at: usize,
    offset: u32,
    word: u64,
    word_index: usize,
    primed: bool,
}

impl<'a> WalkStore<'a> {
    fn new(store: &'a Store) -> Self {
        Self {
            store,
            at: 0,
            offset: 0,
            word: 0,
            word_index: 0,
            primed: false,
        }
    }
}

impl Iterator for WalkStore<'_> {
    type Item = u16;

    fn next(&mut self) -> Option<u16> {
        match self.store {
            Store::Array(list) => {
                let low = list.get(self.at).copied()?;
                self.at += 1;
                Some(low)
            }
            Store::Runs(runs) => loop {
                let run = runs.get(self.at)?;
                let span = u32::from(run.last) - u32::from(run.start);
                if self.offset <= span {
                    let low = u32::from(run.start) + self.offset;
                    self.offset += 1;
                    return u16::try_from(low).ok();
                }
                self.at += 1;
                self.offset = 0;
            },
            Store::Bits(words) => {
                if !self.primed {
                    self.word = words.bits[0];
                    self.primed = true;
                }
                loop {
                    if self.word != 0 {
                        let bit = self.word.trailing_zeros();
                        self.word &= self.word - 1;
                        let low = u32::try_from(self.word_index * 64).ok()? + bit;
                        return u16::try_from(low).ok();
                    }
                    self.word_index += 1;
                    self.word = *words.bits.get(self.word_index)?;
                }
            }
        }
    }
}

/// Splits an ordinal into the chunk that holds it and its place inside it.
const fn split(ordinal: DocId) -> (u16, u16) {
    #[expect(
        clippy::cast_possible_truncation,
        reason = "the two halves of a u32 are what this returns"
    )]
    ((ordinal >> 16) as u16, ordinal as u16)
}

/// The word holding `low` and the bit within it.
fn at(low: u16) -> (usize, u64) {
    (usize::from(low) / 64, 1u64 << (low % 64))
}

/// Where `low` sits among the runs, or where a run holding it would go.
fn covered(runs: &[Run], low: u16) -> core::result::Result<usize, usize> {
    runs.binary_search_by(|run| {
        if run.last < low {
            core::cmp::Ordering::Less
        } else if run.start > low {
            core::cmp::Ordering::Greater
        } else {
            core::cmp::Ordering::Equal
        }
    })
}

/// Clears every bit the runs do not cover.
///
/// It walks the gaps rather than the runs, because a permission filter is one
/// long stretch and its gaps are what there are few of.
fn keep_within(words: &mut Words, runs: &[Run]) {
    let mut next = 0u32;
    for run in runs {
        if u32::from(run.start) > next {
            clear_between(words, next, u32::from(run.start) - 1);
        }
        next = u32::from(run.last) + 1;
    }
    clear_between(words, next, u32::from(u16::MAX));
}

/// Clears the bits from `start` to `last`, given the two ends as the wider type
/// the arithmetic above used. A start past the end of the chunk clears nothing.
fn clear_between(words: &mut Words, start: u32, last: u32) {
    if let (Ok(start), Ok(last)) = (u16::try_from(start), u16::try_from(last))
        && start <= last
    {
        words.clear(Run { start, last });
    }
}

/// Keeps only what both sorted lists hold, in one pass over each.
fn keep_common(mine: &mut Vec<u16>, theirs: &[u16]) {
    let mut at = 0;
    mine.retain(|low| {
        while theirs.get(at).is_some_and(|their| their < low) {
            at += 1;
        }
        theirs.get(at) == Some(low)
    });
}

/// Merges `theirs` into `mine`, both sorted, without duplicates.
fn add_all(mine: &mut Vec<u16>, theirs: &[u16]) {
    let mut merged = Vec::with_capacity(mine.len() + theirs.len());
    let mut ours = mine.iter().copied().peekable();
    let mut them = theirs.iter().copied().peekable();
    loop {
        match (ours.peek().copied(), them.peek().copied()) {
            (Some(a), Some(b)) if a == b => {
                merged.push(a);
                ours.next();
                them.next();
            }
            (Some(a), Some(b)) if a < b => {
                merged.push(a);
                ours.next();
            }
            (Some(_) | None, Some(b)) => {
                merged.push(b);
                them.next();
            }
            (Some(a), None) => {
                merged.push(a);
                ours.next();
            }
            (None, None) => break,
        }
    }
    *mine = merged;
}

/// Adds `low` to a run list, joining the runs on either side if it closes a gap.
fn runs_insert(runs: &mut Vec<Run>, low: u16) -> bool {
    let Err(at) = covered(runs, low) else {
        return false;
    };
    let joins_below = at > 0 && u32::from(runs[at - 1].last) + 1 == u32::from(low);
    let joins_above = runs
        .get(at)
        .is_some_and(|run| u32::from(low) + 1 == u32::from(run.start));

    match (joins_below, joins_above) {
        (true, true) => {
            runs[at - 1].last = runs[at].last;
            runs.remove(at);
        }
        (true, false) => runs[at - 1].last = low,
        (false, true) => runs[at].start = low,
        (false, false) => runs.insert(
            at,
            Run {
                start: low,
                last: low,
            },
        ),
    }
    true
}

/// Removes `low` from a run list, splitting the run it lands in if it is inside.
fn runs_remove(runs: &mut Vec<Run>, low: u16) -> bool {
    let Ok(at) = covered(runs, low) else {
        return false;
    };
    let run = runs[at];
    match (run.start == low, run.last == low) {
        (true, true) => {
            runs.remove(at);
        }
        (true, false) => runs[at].start = low.saturating_add(1),
        (false, true) => runs[at].last = low.saturating_sub(1),
        (false, false) => {
            runs[at].last = low.saturating_sub(1);
            runs.insert(
                at + 1,
                Run {
                    start: low.saturating_add(1),
                    last: run.last,
                },
            );
        }
    }
    true
}

/// The stretches both run lists cover.
fn runs_intersect(mine: &[Run], theirs: &[Run]) -> Vec<Run> {
    let mut out = Vec::new();
    let (mut ours, mut them) = (0, 0);
    while let (Some(a), Some(b)) = (mine.get(ours), theirs.get(them)) {
        let start = a.start.max(b.start);
        let last = a.last.min(b.last);
        if start <= last {
            out.push(Run { start, last });
        }
        if a.last < b.last {
            ours += 1;
        } else {
            them += 1;
        }
    }
    out
}

/// The stretches either run list covers, joined where they touch.
fn runs_union(mine: &[Run], theirs: &[Run]) -> Vec<Run> {
    let mut out: Vec<Run> = Vec::with_capacity(mine.len() + theirs.len());
    let (mut ours, mut them) = (0, 0);
    loop {
        let next = match (mine.get(ours), theirs.get(them)) {
            (Some(a), Some(b)) => {
                if a.start <= b.start {
                    ours += 1;
                    *a
                } else {
                    them += 1;
                    *b
                }
            }
            (Some(a), None) => {
                ours += 1;
                *a
            }
            (None, Some(b)) => {
                them += 1;
                *b
            }
            (None, None) => break,
        };
        match out.last_mut() {
            // Touching counts as overlapping, because two runs with nothing
            // between them are one run and a list that kept them apart would
            // cost twice what it should.
            Some(run) if u32::from(run.last) + 1 >= u32::from(next.start) => {
                run.last = run.last.max(next.last);
            }
            _ => out.push(next),
        }
    }
    out
}

/// The stretches `mine` covers and `theirs` does not.
fn runs_difference(mine: &[Run], theirs: &[Run]) -> Vec<Run> {
    let mut out = Vec::new();
    let mut them = 0;
    for run in mine {
        let mut start = u32::from(run.start);
        let last = u32::from(run.last);
        while them < theirs.len() && u32::from(theirs[them].last) < start {
            them += 1;
        }
        let mut cut = them;
        while start <= last {
            let Some(blocker) = theirs.get(cut) else {
                break;
            };
            let (from, to) = (u32::from(blocker.start), u32::from(blocker.last));
            if from > last {
                break;
            }
            if from > start {
                push(&mut out, start, from - 1);
            }
            start = start.max(to + 1);
            cut += 1;
        }
        if start <= last {
            push(&mut out, start, last);
        }
    }
    out
}

/// Appends a run, given the two ends as the wider type the arithmetic used.
fn push(out: &mut Vec<Run>, start: u32, last: u32) {
    if let (Ok(start), Ok(last)) = (u16::try_from(start), u16::try_from(last)) {
        out.push(Run { start, last });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape(map: &Bitmap, chunk: usize) -> &'static str {
        match map.chunks[chunk].store {
            Store::Array(_) => "array",
            Store::Bits(_) => "bits",
            Store::Runs(_) => "runs",
        }
    }

    /// A fixture small enough that no chunk of it turns into a word block.
    fn sparse(ordinals: &[DocId]) -> Bitmap {
        let map = Bitmap::from_sorted(ordinals);
        for chunk in 0..map.chunks.len() {
            assert_ne!(
                shape(&map, chunk),
                "bits",
                "this fixture is meant to be small"
            );
        }
        map
    }

    fn dense(count: DocId) -> Bitmap {
        (0..count).collect()
    }

    #[test]
    fn insert_contains_remove() {
        let mut map = Bitmap::new();
        assert!(map.is_empty());
        assert!(map.insert(7));
        assert!(!map.insert(7), "inserting twice should report no change");
        assert!(map.contains(7));
        assert!(!map.contains(8));
        assert!(map.remove(7));
        assert!(!map.remove(7));
        assert!(map.is_empty());
    }

    #[test]
    fn iteration_is_ascending_in_every_representation() {
        let ordinals = [9u32, 1, 64, 63, 4096, 0];
        let map = Bitmap::from_sorted(&ordinals);
        assert_eq!(map.to_vec(), vec![0, 1, 9, 63, 64, 4096]);

        let solid = dense(10_000);
        assert_eq!(shape(&solid, 0), "runs");
        let collected = solid.to_vec();
        assert_eq!(collected.len(), 10_000);
        assert!(collected.is_sorted());
        assert_eq!(collected.first(), Some(&0));
        assert_eq!(collected.last(), Some(&9_999));

        let scattered: Bitmap = (0..20_000u32).map(|i| i * 3).collect();
        assert_eq!(shape(&scattered, 0), "bits");
        assert_eq!(
            scattered.to_vec(),
            (0..20_000u32).map(|i| i * 3).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_chunk_is_held_whichever_way_is_smallest() {
        // A handful of ordinals is a list.
        let few: Bitmap = (0..100u32).map(|i| i * 700).collect();
        assert_eq!(shape(&few, 0), "array");

        // One solid stretch is a pair of numbers however long it is.
        let solid: Bitmap = (0..60_000u32).collect();
        assert_eq!(shape(&solid, 0), "runs");
        assert!(solid.memory() < 200, "memory was {}", solid.memory());

        // Scattered and crowded is words, because neither of the other two can
        // describe it in less than eight kilobytes.
        let scattered: Bitmap = (0..30_000u32).map(|i| i * 2).collect();
        assert_eq!(shape(&scattered, 0), "bits");
    }

    #[test]
    fn a_scattered_set_costs_what_it_holds_and_not_what_it_spans() {
        let scattered: Bitmap = (0..5_000u32).map(|i| i * 20_000).collect();
        assert_eq!(scattered.len(), 5_000);
        // A flat word array over the same span would be twelve megabytes.
        assert!(
            scattered.memory() < 100_000,
            "memory was {}",
            scattered.memory()
        );
    }

    #[test]
    fn ordinals_land_in_the_chunk_that_holds_them() {
        let mut map = Bitmap::new();
        for ordinal in [0u32, 65_535, 65_536, 131_072, u32::MAX] {
            assert!(map.insert(ordinal));
        }
        assert_eq!(map.chunks.len(), 4);
        assert_eq!(map.to_vec(), vec![0, 65_535, 65_536, 131_072, u32::MAX]);
        assert!(map.contains(u32::MAX));
        assert!(!map.contains(u32::MAX - 1));
    }

    #[test]
    fn intersection_keeps_only_what_is_in_both() {
        let mut a = sparse(&[1, 5, 9, 70]);
        a.intersect_with(&sparse(&[5, 70, 99]));
        assert_eq!(a.to_vec(), vec![5, 70]);

        let mut wide = dense(10_000);
        wide.intersect_with(&dense(5_000));
        assert_eq!(wide.len(), 5_000);
        assert!(wide.contains(4_999));
        assert!(!wide.contains(5_000));
    }

    #[test]
    fn intersection_works_across_representations() {
        let mut big = dense(10_000);
        big.intersect_with(&sparse(&[3, 9_999, 20_000]));
        assert_eq!(big.to_vec(), vec![3, 9_999]);

        let mut small = sparse(&[3, 9_999, 20_000]);
        small.intersect_with(&dense(10_000));
        assert_eq!(small.to_vec(), vec![3, 9_999]);

        let mut words: Bitmap = (0..30_000u32).map(|i| i * 2).collect();
        words.intersect_with(&dense(1_000));
        assert_eq!(
            words.to_vec(),
            (0..500u32).map(|i| i * 2).collect::<Vec<_>>()
        );
    }

    #[test]
    fn an_intersection_drops_the_chunks_the_other_side_never_had() {
        let mut a: Bitmap = (0..5_000u32).chain(core::iter::once(500_000)).collect();
        a.intersect_with(&dense(5_000));
        assert!(!a.contains(500_000), "the chunk past the other map must go");
        assert_eq!(a.len(), 5_000);
        assert_eq!(a.chunks.len(), 1);
    }

    #[test]
    fn union_adds_everything_from_the_other_side() {
        let mut a = sparse(&[1, 2]);
        a.union_with(&sparse(&[2, 3]));
        assert_eq!(a.to_vec(), vec![1, 2, 3]);

        let mut wide = dense(5_000);
        wide.union_with(&dense(10_000));
        assert_eq!(wide.len(), 10_000);

        let mut far = sparse(&[1]);
        far.union_with(&sparse(&[1_000_000]));
        assert_eq!(far.to_vec(), vec![1, 1_000_000]);
        assert_eq!(far.chunks.len(), 2);
    }

    #[test]
    fn two_runs_that_touch_become_one() {
        let mut left: Bitmap = (0..1_000u32).collect();
        left.union_with(&(1_000..2_000u32).collect());
        assert_eq!(shape(&left, 0), "runs");
        assert_eq!(left.chunks[0].store.runs(), 1);
        assert_eq!(left.len(), 2_000);
    }

    #[test]
    fn difference_is_how_a_deny_list_is_applied() {
        let mut allowed = sparse(&[1, 2, 3, 4]);
        allowed.difference_with(&sparse(&[2, 4]));
        assert_eq!(allowed.to_vec(), vec![1, 3]);

        let mut wide = dense(10_000);
        wide.difference_with(&dense(9_999));
        assert_eq!(wide.to_vec(), vec![9_999]);

        let mut wide = dense(10_000);
        wide.difference_with(&sparse(&[0, 9_999]));
        assert_eq!(wide.len(), 9_998);
        assert!(!wide.contains(0));
        assert!(!wide.contains(9_999));

        let mut middle = dense(1_000);
        middle.difference_with(&Bitmap::from_sorted(&(400..600).collect::<Vec<_>>()));
        assert_eq!(middle.len(), 800);
        assert!(middle.contains(399));
        assert!(!middle.contains(400));
        assert!(!middle.contains(599));
        assert!(middle.contains(600));
    }

    #[test]
    fn an_empty_intersection_leaves_nothing_behind() {
        let mut a = dense(10_000);
        a.intersect_with(&Bitmap::new());
        assert!(a.is_empty(), "len was {}", a.len());
        assert_eq!(a.to_vec(), Vec::<DocId>::new());
    }

    #[test]
    fn duplicates_and_disorder_in_the_input_are_normalised() {
        let map = Bitmap::from_sorted(&[5, 1, 5, 1, 3]);
        assert_eq!(map.to_vec(), vec![1, 3, 5]);
    }

    #[test]
    fn with_capacity_does_not_invent_members() {
        assert!(Bitmap::with_capacity(0).is_empty());
        assert!(Bitmap::with_capacity(100_000).is_empty());
        assert_eq!(Bitmap::with_capacity(100_000).len(), 0);
    }

    #[test]
    fn a_run_survives_being_poked_a_hole_in_and_filled_again() {
        let mut map: Bitmap = (0..1_000u32).collect();
        assert_eq!(shape(&map, 0), "runs");
        assert!(map.remove(500));
        assert!(!map.contains(500));
        assert_eq!(map.len(), 999);
        assert_eq!(map.chunks[0].store.runs(), 2);
        assert!(map.insert(500));
        assert_eq!(map.chunks[0].store.runs(), 1);
        assert_eq!(map.to_vec(), (0..1_000u32).collect::<Vec<_>>());

        assert!(map.remove(0));
        assert!(map.remove(999));
        assert_eq!(map.to_vec(), (1..999u32).collect::<Vec<_>>());
    }

    #[test]
    fn equality_does_not_depend_on_how_a_set_was_built() {
        // Removing does not go looking for a cheaper representation, because a
        // deny list applied one id at a time would then rebuild the chunk on
        // every call. So a set worn down to ten members is still words, and it
        // has to compare equal to the same ten members held as a list.
        let mut worn: Bitmap = (0..30_000u32).map(|i| i * 2).collect();
        assert_eq!(shape(&worn, 0), "bits");
        for ordinal in (0..29_990u32).map(|i| i * 2) {
            worn.remove(ordinal);
        }
        assert_eq!(shape(&worn, 0), "bits");

        let tail: Vec<DocId> = (29_990..30_000u32).map(|i| i * 2).collect();
        let built = Bitmap::from_sorted(&tail);
        assert_eq!(shape(&built, 0), "array");
        assert_eq!(worn, built);
        assert_eq!(worn.len(), built.len());

        let mut short = built.clone();
        short.remove(tail[0]);
        assert_ne!(worn, short);
    }

    /// The three representations have to agree about every operation, so they
    /// are all run against a plain sorted vector doing the same thing.
    #[test]
    fn every_representation_agrees_with_a_sorted_vector() {
        let shapes: [Vec<DocId>; 5] = [
            // A list.
            (0..100u32).map(|i| i * 37).collect(),
            // A run.
            (0..50_000u32).collect(),
            // Words.
            (0..30_000u32).map(|i| i * 2).collect(),
            // A few runs with gaps, across two chunks.
            (0..1_000u32)
                .chain(70_000..71_000)
                .chain(80_000..80_010)
                .collect(),
            // Nothing.
            Vec::new(),
        ];

        for left in &shapes {
            for right in &shapes {
                let mut both: Vec<DocId> = left
                    .iter()
                    .copied()
                    .filter(|id| right.binary_search(id).is_ok())
                    .collect();
                both.dedup();
                let mut map = Bitmap::from_sorted(left);
                map.intersect_with(&Bitmap::from_sorted(right));
                assert_eq!(map.to_vec(), both, "intersect");
                assert_eq!(map.len(), both.len(), "intersect len");

                let mut either: Vec<DocId> = left.iter().chain(right.iter()).copied().collect();
                either.sort_unstable();
                either.dedup();
                let mut map = Bitmap::from_sorted(left);
                map.union_with(&Bitmap::from_sorted(right));
                assert_eq!(map.to_vec(), either, "union");
                assert_eq!(map.len(), either.len(), "union len");

                let without: Vec<DocId> = left
                    .iter()
                    .copied()
                    .filter(|id| right.binary_search(id).is_err())
                    .collect();
                let mut map = Bitmap::from_sorted(left);
                map.difference_with(&Bitmap::from_sorted(right));
                assert_eq!(map.to_vec(), without, "difference");
                assert_eq!(map.len(), without.len(), "difference len");
            }
        }
    }

    /// A bitmap holding all three shapes at once, and more containers than the
    /// count at which the offsets come back.
    fn every_shape() -> Bitmap {
        let mut map = Bitmap::new();
        for low in [0, 1, 9, 63, 64, 4096] {
            map.insert(low);
        }
        for low in 0..1000 {
            map.insert((1 << 16) + low);
        }
        for low in (0..30_000).step_by(3) {
            map.insert((2 << 16) + low);
        }
        for low in [0, 5, 6, 7, 65_535] {
            map.insert((9 << 16) + low);
        }
        map
    }

    fn written(map: &Bitmap) -> Vec<u8> {
        let mut bytes = Vec::new();
        map.write_to(&mut bytes);
        assert_eq!(bytes.len(), map.size(), "size disagrees with write_to");
        bytes
    }

    #[test]
    fn a_set_comes_back_from_its_bytes() {
        let cases = [
            Bitmap::new(),
            Bitmap::from_sorted(&[7]),
            sparse(&[0, 1, 9, 63, 64, 4096]),
            dense(1000),
            (0..30_000).step_by(3).collect(),
            every_shape(),
        ];
        for map in cases {
            let back = Bitmap::read(&written(&map)).expect("reads what it wrote");
            assert_eq!(back.to_vec(), map.to_vec());
            assert_eq!(back.len(), map.len());
        }
    }

    #[test]
    fn writing_a_bitmap_leaves_what_was_in_the_vector_already() {
        let map = every_shape();
        let mut bytes = vec![0xab; 7];
        map.write_to(&mut bytes);
        assert_eq!(&bytes[..7], &[0xab; 7], "it wrote over what was there");
        assert_eq!(bytes.len(), 7 + map.size());
        assert_eq!(
            Bitmap::read(&bytes[7..]).expect("reads").to_vec(),
            map.to_vec()
        );
    }

    #[test]
    fn a_chunk_emptied_below_the_threshold_is_written_as_an_array() {
        // The shape on disk follows from the cardinality, so a container the
        // reader is going to take as an array has to be written as one however
        // it is being held here.
        let mut map: Bitmap = (0..12_000).step_by(2).collect();
        assert_eq!(shape(&map, 0), "bits");
        for low in (0..8000).step_by(4) {
            map.remove(low);
        }
        assert_eq!(map.len(), 4000);
        assert_eq!(shape(&map, 0), "bits", "removing does not change the shape");
        assert_eq!(
            map.size(),
            COOKIE_LEN + COUNT_LEN + DESCRIPTOR_LEN + 4 + 2 * 4000
        );
        let back = Bitmap::read(&written(&map)).expect("reads");
        assert_eq!(back.to_vec(), map.to_vec());
    }

    #[test]
    fn a_cookie_from_another_format_is_refused() {
        let mut bytes = written(&every_shape());
        bytes[0] = 0;
        assert!(matches!(Bitmap::read(&bytes), Err(Error::BadMagic)));
        assert!(matches!(Bitmap::read(&[]), Err(Error::Truncated { .. })));
    }

    #[test]
    fn bytes_that_stop_early_are_refused_rather_than_read() {
        for map in [every_shape(), dense(1000), sparse(&[1, 5, 70_000])] {
            let bytes = written(&map);
            for end in 0..bytes.len() {
                assert!(
                    Bitmap::read(&bytes[..end]).is_err(),
                    "{end} of {} bytes was accepted",
                    bytes.len()
                );
            }
            assert!(Bitmap::read(&bytes).is_ok(), "the whole of it was refused");
        }
    }

    #[test]
    fn a_container_count_larger_than_the_bytes_is_refused() {
        // The count is four bytes and comes before anything that could bound
        // it, so a reader that believed it would reserve room for four billion
        // containers before finding out there are two.
        let mut bytes = written(&sparse(&[1, 5, 70_000]));
        bytes[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(Bitmap::read(&bytes).is_err());
    }

    #[test]
    fn keys_that_do_not_ascend_are_refused() {
        let map = sparse(&[1, 5, 70_000]);
        let mut bytes = written(&map);
        // The first descriptor is the key of chunk zero, and the second is the
        // key of chunk one, four bytes further on.
        let (first, second) = (COOKIE_LEN + COUNT_LEN, COOKIE_LEN + COUNT_LEN + 4);
        bytes[second..second + 2].copy_from_slice(&0u16.to_le_bytes());
        assert!(matches!(Bitmap::read(&bytes), Err(Error::NotSorted { .. })));
        let mut bytes = written(&map);
        bytes[first..first + 2].copy_from_slice(&2u16.to_le_bytes());
        assert!(matches!(Bitmap::read(&bytes), Err(Error::NotSorted { .. })));
    }

    #[test]
    fn members_that_do_not_ascend_are_refused() {
        let mut bytes = written(&sparse(&[1, 5, 9]));
        // One container, so the body starts after the cookie, the count, the
        // one descriptor and the one offset.
        let body = COOKIE_LEN + COUNT_LEN + DESCRIPTOR_LEN + 4;
        bytes[body..body + 2].copy_from_slice(&9u16.to_le_bytes());
        assert!(matches!(
            Bitmap::read(&bytes),
            Err(Error::NotSorted { at: 5 })
        ));
    }

    #[test]
    fn a_container_that_does_not_hold_what_it_said_is_refused() {
        let map: Bitmap = (0..30_000).step_by(3).collect();
        let mut bytes = written(&map);
        let body = COOKIE_LEN + COUNT_LEN + DESCRIPTOR_LEN + 4;
        bytes[body] = 0;
        let Err(Error::BadCardinality { stated, found }) = Bitmap::read(&bytes) else {
            panic!("a word block short of three members was accepted");
        };
        assert_eq!(stated, 10_000);
        assert_eq!(found, 9997);
    }

    #[test]
    fn an_offset_that_points_somewhere_else_is_refused() {
        let mut bytes = written(&sparse(&[1, 5, 70_000]));
        // Two containers, so two offsets, and the second one is where the first
        // container ends.
        let second = COOKIE_LEN + COUNT_LEN + 2 * DESCRIPTOR_LEN + 4;
        bytes[second..second + 4].copy_from_slice(&0u32.to_le_bytes());
        let Err(Error::BadOffset { stated, found }) = Bitmap::read(&bytes) else {
            panic!("an offset pointing at the header was accepted");
        };
        assert_eq!(stated, 0);
        assert_eq!(found, 24 + 4);
    }

    /// A bitmap of one chunk held as runs, spelled out rather than written by
    /// this build, so that runs this build would never produce can be read.
    fn one_chunk_of_runs(members: u16, runs: &[(u16, u16)]) -> Vec<u8> {
        let mut bytes = Vec::new();
        put_u32(&mut bytes, COOKIE_RUNS);
        bytes.push(1);
        put_u16(&mut bytes, 0);
        put_u16(&mut bytes, members - 1);
        put_u16(&mut bytes, u16::try_from(runs.len()).expect("a few runs"));
        for &(start, length) in runs {
            put_u16(&mut bytes, start);
            put_u16(&mut bytes, length);
        }
        bytes
    }

    #[test]
    fn runs_that_touch_are_read_as_one_run() {
        let bytes = one_chunk_of_runs(10, &[(0, 4), (5, 4)]);
        let map = Bitmap::read(&bytes).expect("touching runs are not wrong, only untidy");
        assert_eq!(map.to_vec(), (0..10).collect::<Vec<DocId>>());
        assert_eq!(map.chunks[0].store.runs(), 1, "they were not merged");
    }

    #[test]
    fn runs_that_overlap_or_go_backwards_are_refused() {
        for runs in [
            &[(0u16, 4u16), (3, 4)][..],
            &[(10, 0), (0, 0)][..],
            &[(0, 4), (0, 4)][..],
        ] {
            let held: u16 = runs.iter().map(|&(_, length)| length + 1).sum();
            let bytes = one_chunk_of_runs(held, runs);
            assert!(
                matches!(Bitmap::read(&bytes), Err(Error::NotSorted { .. })),
                "{runs:?} was accepted"
            );
        }
    }

    #[test]
    fn runs_that_do_not_add_up_to_what_was_stated_are_refused() {
        let bytes = one_chunk_of_runs(11, &[(0, 4), (5, 4)]);
        let Err(Error::BadCardinality { stated, found }) = Bitmap::read(&bytes) else {
            panic!("a chunk of runs holding one fewer than it said was accepted");
        };
        assert_eq!(stated, 11);
        assert_eq!(found, 10);
    }

    #[test]
    fn a_run_covering_a_whole_chunk_comes_back() {
        let map: Bitmap = (0..1 << 16).collect();
        let bytes = written(&map);
        assert_eq!(bytes.len(), COOKIE_LEN + 1 + DESCRIPTOR_LEN + 2 + 4);
        let back = Bitmap::read(&bytes).expect("reads");
        assert_eq!(back.len(), 1 << 16);
        assert!(back.contains(0) && back.contains(65_535));
    }

    #[test]
    fn bytes_that_are_not_a_bitmap_at_all_are_refused_rather_than_trusted() {
        // Not a fuzzer, but enough to say that the reader decides from the
        // bytes rather than from what it hopes they are. Anything it accepts
        // has to hold together, which is the property that matters here.
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut bytes = written(&every_shape());
        for _ in 0..2000 {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            let at = usize::try_from(seed >> 33).unwrap_or(0) % bytes.len();
            let was = bytes[at];
            bytes[at] ^= (seed >> 11).to_le_bytes()[0] | 1;
            if let Ok(map) = Bitmap::read(&bytes) {
                let again = written(&map);
                assert_eq!(
                    Bitmap::read(&again)
                        .expect("what it accepted it writes")
                        .to_vec(),
                    map.to_vec()
                );
            }
            bytes[at] = was;
        }
    }

    #[test]
    fn the_largest_member_comes_back_from_every_shape() {
        assert_eq!(Bitmap::new().max(), None);
        assert_eq!(sparse(&[3]).max(), Some(3));
        assert_eq!(sparse(&[0, 1, 9, 63, 64, 4096]).max(), Some(4096));
        // A word block, where the answer is in the last word that is not zero
        // rather than in the last word.
        let scattered = Bitmap::from_sorted(&(0..30_000).step_by(3).collect::<Vec<_>>());
        assert_eq!(shape(&scattered, 0), "bits");
        assert_eq!(scattered.max(), Some(29_999 - 29_999 % 3));
        // Runs, and a run that ends at the top of its chunk.
        let chunk = DocId::try_from(CHUNK).expect("a chunk is sixty five thousand wide");
        let full = dense(chunk);
        assert_eq!(shape(&full, 0), "runs");
        assert_eq!(full.max(), Some(chunk - 1));
        // Several chunks, where the answer is in the last of them and not in
        // the largest of them.
        let mut many = Bitmap::from_sorted(&(0..1_000).collect::<Vec<_>>());
        many.insert(u32::MAX);
        assert_eq!(many.max(), Some(u32::MAX));
    }

    #[test]
    fn the_first_ordinal_the_set_does_not_hold_is_where_the_prefix_ends() {
        assert_eq!(Bitmap::new().first_absent(), 0);
        // A set that does not start at zero has no prefix, so the answer is
        // zero and not the first member.
        assert_eq!(sparse(&[3, 4, 5]).first_absent(), 0);
        assert_eq!(sparse(&[0]).first_absent(), 1);
        assert_eq!(sparse(&[0, 1, 2, 7]).first_absent(), 3);
        // Across a chunk boundary, where the prefix is one whole container and
        // the first member of the next.
        let chunk = DocId::try_from(CHUNK).expect("a chunk is sixty five thousand wide");
        let mut over = dense(chunk);
        over.insert(chunk);
        over.insert(chunk + 2);
        assert_eq!(over.first_absent(), chunk + 1);
    }

    #[test]
    fn the_first_absent_ordinal_moves_back_when_the_prefix_is_broken() {
        let mut map = Bitmap::from_sorted(&(0..1_000).collect::<Vec<_>>());
        assert_eq!(map.first_absent(), 1_000);
        map.remove(500);
        assert_eq!(map.first_absent(), 500);
        map.remove(0);
        assert_eq!(map.first_absent(), 0);
    }

    #[test]
    fn the_largest_member_survives_what_was_taken_out() {
        let mut map = Bitmap::from_sorted(&[1, 2, 3, 900, 70_000]);
        assert_eq!(map.max(), Some(70_000));
        map.remove(70_000);
        assert_eq!(map.max(), Some(900));
        map.remove(900);
        map.remove(3);
        assert_eq!(map.max(), Some(2));
        map.remove(2);
        map.remove(1);
        assert_eq!(map.max(), None);
    }
}
