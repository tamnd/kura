//! Compressed posting lists.
//!
//! A posting list is the set of documents one term appears in, and it is the
//! single largest thing an inverted index stores. It is written as fixed size
//! blocks of delta encoded varints, with a skip entry per block.
//!
//! The blocks are what make a large list usable. Without them, finding whether
//! document nine million is in a list means decoding everything before it. With
//! them, the skip table says which block could contain it and only that block is
//! decoded. That is the difference between an intersection that scales with the
//! size of the rarest term and one that scales with the size of the commonest.

use crate::DocId;
use crate::codec::{get_u32, get_uvarint, put_u32, put_uvarint, split_at};
use crate::error::{Error, Result};

/// How many document ids go into one block.
///
/// A larger block compresses slightly better and skips slightly worse. This size
/// keeps a decoded block inside a typical L1 data cache, which matters more than
/// either.
pub const BLOCK_SIZE: usize = 128;

/// The size of one skip entry: the last id of a block and the byte offset of the
/// block, both as fixed width little endian words.
///
/// Fixed width rather than varint on purpose. The skip table is the one part of
/// the format that is searched rather than read, and a search needs to be able
/// to land on entry `n` without decoding the `n - 1` before it.
const SKIP_ENTRY: usize = 8;

/// Builds a posting list from ascending document ids.
#[derive(Debug, Default)]
pub struct Writer {
    blocks: Vec<u8>,
    /// The last id of each block, which is the skip table.
    skips: Vec<DocId>,
    /// The byte offset of each block inside `blocks`.
    offsets: Vec<u32>,
    pending: Vec<DocId>,
    last: Option<DocId>,
    count: u32,
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
    pub fn push(&mut self, id: DocId) -> Result<()> {
        if let Some(last) = self.last
            && id <= last
        {
            return Err(Error::NotSorted { at: id });
        }
        self.last = Some(id);
        self.count += 1;
        self.pending.push(id);
        if self.pending.len() == BLOCK_SIZE {
            self.flush_block();
        }
        Ok(())
    }

    /// Finishes the list and returns the encoded bytes.
    #[must_use]
    pub fn finish(mut self) -> Vec<u8> {
        if !self.pending.is_empty() {
            self.flush_block();
        }

        let mut out = Vec::with_capacity(self.blocks.len() + self.skips.len() * 8 + 16);
        put_uvarint(&mut out, u64::from(self.count));
        put_uvarint(&mut out, self.skips.len() as u64);
        for (skip, offset) in self.skips.iter().zip(self.offsets.iter()) {
            put_u32(&mut out, *skip);
            put_u32(&mut out, *offset);
        }
        put_uvarint(&mut out, self.blocks.len() as u64);
        out.extend_from_slice(&self.blocks);
        out
    }

    fn flush_block(&mut self) {
        let Some(&first) = self.pending.first() else {
            return;
        };
        let offset = u32::try_from(self.blocks.len()).unwrap_or(u32::MAX);
        self.offsets.push(offset);

        // The first id of a block is absolute, so a block can be decoded on its
        // own after a skip. Everything after it is a gap.
        put_uvarint(&mut self.blocks, u64::from(first));
        let mut previous = first;
        for id in self.pending.iter().skip(1) {
            put_uvarint(&mut self.blocks, u64::from(*id - previous));
            previous = *id;
        }

        self.skips.push(previous);
        self.pending.clear();
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
    skip_count: usize,
    table: &'a [u8],
    blocks: &'a [u8],
}

impl<'a> Reader<'a> {
    /// Parses the header of an encoded list.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Truncated`] if the input ends inside the header or the
    /// skip table, and [`Error::Overflow`] if a length does not decode.
    pub fn new(input: &'a [u8]) -> Result<Self> {
        let (count, rest) = get_uvarint(input)?;
        let (skip_count, rest) = get_uvarint(rest)?;

        let skip_count = usize::try_from(skip_count).map_err(|_| Error::Overflow)?;
        let table_len = skip_count.checked_mul(SKIP_ENTRY).ok_or(Error::Overflow)?;
        let (table, rest) = split_at(rest, table_len)?;

        let (blocks_len, rest) = get_uvarint(rest)?;
        let blocks_len = usize::try_from(blocks_len).map_err(|_| Error::Overflow)?;
        let (blocks, _) = split_at(rest, blocks_len)?;

        Ok(Self {
            count: u32::try_from(count).map_err(|_| Error::Overflow)?,
            skip_count,
            table,
            blocks,
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
        let (mut low, mut high) = (0usize, self.skip_count);
        while low < high {
            let middle = low + (high - low) / 2;
            let (last_id, _) = self.skip(middle)?;
            if last_id < target {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        (low < self.skip_count).then_some(low)
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
        for index in 0..self.skip_count {
            self.decode_block(index, &mut out)?;
        }
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
        let Some(index) = self.seek(target) else {
            return Ok(false);
        };
        let mut block = Vec::with_capacity(BLOCK_SIZE);
        self.decode_block(index, &mut block)?;
        Ok(block.binary_search(&target).is_ok())
    }

    /// Decodes one block onto the end of `out`.
    fn decode_block(&self, index: usize, out: &mut Vec<DocId>) -> Result<()> {
        let Some((_, offset)) = self.skip(index) else {
            return Ok(());
        };
        let start = offset as usize;
        let end = self
            .skip(index + 1)
            .map_or(self.blocks.len(), |(_, next)| next as usize);

        let Some(mut block) = self.blocks.get(start..end.max(start)) else {
            return Err(Error::Truncated {
                needed: end,
                available: self.blocks.len(),
            });
        };
        if block.is_empty() {
            return Ok(());
        }

        let (first, rest) = get_uvarint(block)?;
        let mut current = u32::try_from(first).map_err(|_| Error::Overflow)?;
        out.push(current);
        block = rest;

        while !block.is_empty() {
            let (gap, rest) = get_uvarint(block)?;
            let gap = u32::try_from(gap).map_err(|_| Error::Overflow)?;
            current = current.checked_add(gap).ok_or(Error::Overflow)?;
            out.push(current);
            block = rest;
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
    fn a_dense_list_compresses_below_four_bytes_per_document() {
        let ids: Vec<DocId> = (0..10_000u32).collect();
        let encoded = encode(&ids);
        let raw = ids.len() * size_of::<DocId>();
        assert!(
            encoded.len() < raw / 2,
            "encoded {} bytes for {raw} raw bytes",
            encoded.len()
        );
    }
}
