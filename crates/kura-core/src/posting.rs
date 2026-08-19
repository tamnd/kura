//! Compressed posting lists.
//!
//! A posting list is the set of documents one term appears in, and it is the
//! single largest thing an inverted index stores. Whatever it costs to hold and
//! to walk is what a query costs, multiplied by the number of terms in it.
//!
//! It is written as fixed size blocks of a hundred and twenty eight ids, packed
//! at a width chosen per block, with a skip entry per block. The packing is in
//! [`crate::bitpack`], along with the reason it beats a varint by more than the
//! width alone would suggest.
//!
//! Whatever is left over at the end, fewer ids than a block holds, is written as
//! varint gaps instead. Uniformity would be nicer, but the vocabulary of a real
//! corpus is mostly terms that appear in a handful of documents, and rounding
//! every one of those up to a whole block would cost more in the term dictionary
//! than the packing saves in the lists that are actually long.
//!
//! The blocks are what make a long list usable. Without them, finding whether
//! document nine million is in a list means decoding everything before it. With
//! them, the skip table says which block could contain it and only that block is
//! decoded. That is the difference between an intersection that scales with the
//! size of the rarest term and one that scales with the size of the commonest.

use crate::DocId;
use crate::bitpack::{self, BLOCK};
use crate::codec::{get_u32, get_uvarint, put_u32, put_uvarint, split_at};
use crate::error::{Error, Result};

/// How many document ids go into one packed block.
pub const BLOCK_SIZE: usize = BLOCK;

/// The size of one skip entry: the last id of a block and the byte offset of the
/// block, both as fixed width little endian words.
///
/// Fixed width rather than varint on purpose. The skip table is the one part of
/// the format that is searched rather than read, and a search needs to be able
/// to land on entry `n` without decoding the `n - 1` before it.
const SKIP_ENTRY: usize = 8;

/// Builds a posting list from ascending document ids.
#[derive(Debug)]
pub struct Writer {
    blocks: Vec<u8>,
    /// The last id of each block, which is the skip table.
    skips: Vec<DocId>,
    /// The byte offset of each block inside `blocks`.
    offsets: Vec<u32>,
    /// The width each block was packed at, one byte each.
    widths: Vec<u8>,
    pending: [DocId; BLOCK],
    filled: usize,
    /// The last id written into a block, which is the base the next block counts
    /// from and the base the tail counts from.
    packed_last: DocId,
    last: Option<DocId>,
    count: u32,
}

impl Default for Writer {
    fn default() -> Self {
        Self {
            blocks: Vec::new(),
            skips: Vec::new(),
            offsets: Vec::new(),
            widths: Vec::new(),
            pending: [0; BLOCK],
            filled: 0,
            packed_last: 0,
            last: None,
            count: 0,
        }
    }
}

impl Writer {
    /// Returns an empty writer.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends a document id.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotSorted`] if `id` is not strictly greater than the
    /// previous one. The whole format rests on the order, so this is checked
    /// rather than assumed.
    ///
    /// Returns [`Error::Overflow`] if the packed blocks have grown past what a
    /// four byte offset can address, which is a list of roughly two hundred
    /// million ids in one term. A list that long belongs in more than one
    /// segment, and saying so is better than writing a skip table that points at
    /// the wrong bytes.
    pub fn push(&mut self, id: DocId) -> Result<()> {
        if let Some(last) = self.last
            && id <= last
        {
            return Err(Error::NotSorted { at: id });
        }
        self.last = Some(id);
        self.count += 1;
        self.pending[self.filled] = id;
        self.filled += 1;
        if self.filled == BLOCK {
            self.flush_block()?;
        }
        Ok(())
    }

    /// Finishes the list and returns the encoded bytes.
    #[must_use]
    pub fn finish(self) -> Vec<u8> {
        // The leftover ids go out as varint gaps rather than as a short block,
        // so a term that appears in three documents costs three bytes.
        let mut tail = Vec::new();
        let mut previous = self.packed_last;
        for id in &self.pending[..self.filled] {
            put_uvarint(&mut tail, u64::from(*id - previous));
            previous = *id;
        }

        let blocks = self.skips.len();
        let mut out =
            Vec::with_capacity(self.blocks.len() + blocks * (SKIP_ENTRY + 1) + tail.len() + 24);
        put_uvarint(&mut out, u64::from(self.count));
        put_uvarint(&mut out, blocks as u64);
        for (skip, offset) in self.skips.iter().zip(self.offsets.iter()) {
            put_u32(&mut out, *skip);
            put_u32(&mut out, *offset);
        }
        out.extend_from_slice(&self.widths);
        put_uvarint(&mut out, self.blocks.len() as u64);
        out.extend_from_slice(&self.blocks);
        put_uvarint(&mut out, tail.len() as u64);
        out.extend_from_slice(&tail);
        out
    }

    fn flush_block(&mut self) -> Result<()> {
        let offset = u32::try_from(self.blocks.len()).map_err(|_| Error::Overflow)?;
        let width = bitpack::pack(&self.pending, self.packed_last, &mut self.blocks);

        self.offsets.push(offset);
        self.skips.push(self.pending[BLOCK - 1]);
        // The width never exceeds thirty two, so the byte is the whole of it.
        self.widths.push(u8::try_from(width).unwrap_or(u8::MAX));
        self.packed_last = self.pending[BLOCK - 1];
        self.filled = 0;
        Ok(())
    }
}

/// Reads a posting list written by [`Writer`].
///
/// Opening a list costs the same whether it holds ten ids or ten million: the
/// skip table is left where it is and read in place. A reader that copied the
/// table out would turn every term lookup in a query into an allocation, and a
/// query touches a lot of terms.
#[derive(Debug)]
pub struct Reader<'a> {
    count: u32,
    block_count: usize,
    table: &'a [u8],
    widths: &'a [u8],
    blocks: &'a [u8],
    tail: &'a [u8],
}

impl<'a> Reader<'a> {
    /// Parses the header of an encoded list.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Truncated`] if the input ends inside the header, the
    /// skip table, the widths, the blocks or the tail, and [`Error::Overflow`]
    /// if a length does not decode.
    pub fn new(input: &'a [u8]) -> Result<Self> {
        let (count, rest) = get_uvarint(input)?;
        let (block_count, rest) = get_uvarint(rest)?;

        let block_count = usize::try_from(block_count).map_err(|_| Error::Overflow)?;
        let table_len = block_count.checked_mul(SKIP_ENTRY).ok_or(Error::Overflow)?;
        let (table, rest) = split_at(rest, table_len)?;
        let (widths, rest) = split_at(rest, block_count)?;

        let (blocks_len, rest) = get_uvarint(rest)?;
        let blocks_len = usize::try_from(blocks_len).map_err(|_| Error::Overflow)?;
        let (blocks, rest) = split_at(rest, blocks_len)?;

        let (tail_len, rest) = get_uvarint(rest)?;
        let tail_len = usize::try_from(tail_len).map_err(|_| Error::Overflow)?;
        let (tail, _) = split_at(rest, tail_len)?;

        Ok(Self {
            count: u32::try_from(count).map_err(|_| Error::Overflow)?,
            block_count,
            table,
            widths,
            blocks,
            tail,
        })
    }

    /// Returns the last id and the byte offset of one block.
    fn skip(&self, index: usize) -> Option<(DocId, u32)> {
        let start = index.checked_mul(SKIP_ENTRY)?;
        let entry = self.table.get(start..start.checked_add(SKIP_ENTRY)?)?;
        let (last_id, rest) = get_u32(entry).ok()?;
        let (offset, _) = get_u32(rest).ok()?;
        Some((last_id, offset))
    }

    /// Returns the index of the first block whose last id is not below `target`,
    /// which is the only block that can hold it.
    fn seek(&self, target: DocId) -> Option<usize> {
        let (mut low, mut high) = (0usize, self.block_count);
        while low < high {
            let middle = low + (high - low) / 2;
            let (last_id, _) = self.skip(middle)?;
            if last_id < target {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        (low < self.block_count).then_some(low)
    }

    /// Returns how many document ids the list holds.
    #[must_use]
    pub const fn len(&self) -> u32 {
        self.count
    }

    /// Reports whether the list is empty.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Decodes every document id.
    ///
    /// # Errors
    ///
    /// Returns an error if any block is truncated or encodes a gap that runs
    /// past the end of the identifier space.
    pub fn to_vec(&self) -> Result<Vec<DocId>> {
        let mut out = Vec::with_capacity(self.count as usize);
        let mut block = [0u32; BLOCK];
        for index in 0..self.block_count {
            self.decode_block(index, &mut block)?;
            out.extend_from_slice(&block);
        }
        self.decode_tail(&mut out)?;
        Ok(out)
    }

    /// Reports whether `target` is in the list, decoding at most one block.
    ///
    /// # Errors
    ///
    /// Returns an error if the block the skip table points at is truncated.
    pub fn contains(&self, target: DocId) -> Result<bool> {
        // The skip table holds the last id of each block and is ascending, so a
        // binary search over it finds the one block that could hold the target.
        if let Some(index) = self.seek(target) {
            let mut block = [0u32; BLOCK];
            self.decode_block(index, &mut block)?;
            return Ok(block.binary_search(&target).is_ok());
        }
        // Past the last packed block, so it is in the leftovers or nowhere. The
        // leftovers are shorter than one block by construction, which is why
        // walking them is not worth a second index.
        let mut rest = Vec::with_capacity(BLOCK);
        self.decode_tail(&mut rest)?;
        Ok(rest.binary_search(&target).is_ok())
    }

    /// Decodes one packed block into `out`.
    fn decode_block(&self, index: usize, out: &mut [DocId; BLOCK]) -> Result<()> {
        let (_, offset) = self.skip(index).ok_or(Error::Truncated {
            needed: index,
            available: self.block_count,
        })?;
        let width = u32::from(*self.widths.get(index).ok_or(Error::Truncated {
            needed: index,
            available: self.widths.len(),
        })?);
        // The block before this one ends where this one starts counting from.
        // Block zero counts from zero, which is what makes the first id absolute
        // without storing it as one.
        let base = if index == 0 {
            0
        } else {
            self.skip(index - 1).map_or(0, |(last, _)| last)
        };

        let start = offset as usize;
        let bytes = self.blocks.get(start..).ok_or(Error::Truncated {
            needed: start,
            available: self.blocks.len(),
        })?;
        bitpack::unpack(bytes, width, base, out)?;
        Ok(())
    }

    /// Decodes the varint gaps after the last packed block onto the end of
    /// `out`.
    fn decode_tail(&self, out: &mut Vec<DocId>) -> Result<()> {
        let mut current = if self.block_count == 0 {
            0
        } else {
            self.skip(self.block_count - 1).map_or(0, |(last, _)| last)
        };

        let mut rest = self.tail;
        while !rest.is_empty() {
            let (gap, tail) = get_uvarint(rest)?;
            let gap = u32::try_from(gap).map_err(|_| Error::Overflow)?;
            current = current.checked_add(gap).ok_or(Error::Overflow)?;
            out.push(current);
            rest = tail;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode(ids: &[DocId]) -> Vec<u8> {
        let mut writer = Writer::new();
        for id in ids {
            writer.push(*id).expect("ascending input");
        }
        writer.finish()
    }

    fn count(ids: &[DocId]) -> u32 {
        u32::try_from(ids.len()).expect("test fixtures are small")
    }

    #[test]
    fn round_trips_a_short_list() {
        let ids = [0u32, 1, 2, 9, 400, 401, 100_000];
        let encoded = encode(&ids);
        let reader = Reader::new(&encoded).expect("header");

        assert_eq!(reader.len(), count(&ids));
        assert_eq!(reader.to_vec().expect("decode"), ids);
    }

    #[test]
    fn round_trips_across_many_blocks() {
        let block = u32::try_from(BLOCK_SIZE).expect("block size fits");
        let ids: Vec<DocId> = (0..block * 7 + 5).map(|i| i * 3).collect();
        let encoded = encode(&ids);
        let reader = Reader::new(&encoded).expect("header");

        assert_eq!(reader.len(), count(&ids));
        assert_eq!(reader.to_vec().expect("decode"), ids);
    }

    #[test]
    fn round_trips_at_every_length_around_a_block_boundary() {
        // A list one id short of a block, exactly a block, and one over, which
        // is where the split between packed blocks and leftovers is decided.
        for len in 0..BLOCK_SIZE * 2 + 3 {
            let ids: Vec<DocId> = (0..len)
                .map(|i| u32::try_from(i * 7).expect("fits"))
                .collect();
            let encoded = encode(&ids);
            let reader = Reader::new(&encoded).expect("header");
            assert_eq!(reader.len(), count(&ids), "len {len}");
            assert_eq!(reader.to_vec().expect("decode"), ids, "len {len}");
        }
    }

    #[test]
    fn an_empty_list_is_valid() {
        let encoded = encode(&[]);
        let reader = Reader::new(&encoded).expect("header");
        assert!(reader.is_empty());
        assert_eq!(reader.to_vec().expect("decode"), Vec::<DocId>::new());
        assert!(!reader.contains(0).expect("lookup"));
    }

    #[test]
    fn contains_finds_members_and_rejects_the_rest() {
        let ids: Vec<DocId> = (0..2_000u32).map(|i| i * 5).collect();
        let encoded = encode(&ids);
        let reader = Reader::new(&encoded).expect("header");

        for id in [0u32, 5, 4_995, 9_995] {
            assert!(reader.contains(id).expect("lookup"), "missing {id}");
        }
        for id in [1u32, 4, 9_996, u32::MAX] {
            assert!(!reader.contains(id).expect("lookup"), "found {id}");
        }
    }

    #[test]
    fn contains_reaches_the_leftovers_as_well_as_the_blocks() {
        // Two full blocks and a few beyond, so that the answer for the last ids
        // is in the varint tail rather than in any block.
        let ids: Vec<DocId> = (0..BLOCK_SIZE * 2 + 5)
            .map(|i| u32::try_from(i * 3 + 1).expect("fits"))
            .collect();
        let encoded = encode(&ids);
        let reader = Reader::new(&encoded).expect("header");

        for id in &ids {
            assert!(reader.contains(*id).expect("lookup"), "missing {id}");
        }
        for id in [0u32, 2, ids[ids.len() - 1] + 1] {
            assert!(!reader.contains(id).expect("lookup"), "found {id}");
        }
    }

    #[test]
    fn out_of_order_input_is_refused() {
        let mut writer = Writer::new();
        writer.push(10).expect("first");
        assert_eq!(writer.push(9), Err(Error::NotSorted { at: 9 }));
        assert_eq!(writer.push(10), Err(Error::NotSorted { at: 10 }));
    }

    #[test]
    fn a_truncated_list_is_an_error_not_a_panic() {
        let ids: Vec<DocId> = (0..500u32).collect();
        let encoded = encode(&ids);

        for len in 0..encoded.len() {
            match Reader::new(&encoded[..len]) {
                Err(_) => {}
                Ok(reader) => {
                    // A header that happens to parse must still not produce a
                    // panic when the blocks behind it are short.
                    let _ = reader.to_vec();
                    let _ = reader.contains(42);
                }
            }
        }
    }

    #[test]
    fn a_dense_list_costs_well_under_a_byte_an_id() {
        let ids: Vec<DocId> = (0..10_000u32).collect();
        let encoded = encode(&ids);
        let raw = ids.len() * size_of::<DocId>();
        assert!(
            encoded.len() * 8 < raw,
            "encoded {} bytes for {raw} raw bytes",
            encoded.len()
        );
    }

    #[test]
    fn a_sparse_term_does_not_pay_for_a_whole_block() {
        // The shape most of a real vocabulary has: a term in three documents.
        // Rounding it up to a packed block would cost sixteen bytes at the
        // narrowest width, and there are more terms like this than any other
        // kind.
        let encoded = encode(&[4u32, 900, 90_000]);
        assert!(encoded.len() < 12, "encoded {} bytes", encoded.len());
    }
}
