//! A bitmap that changes representation with its density.
//!
//! Visibility is the thing this engine carries through every query: the set of
//! documents one reader is allowed to see. Those sets are wildly different
//! sizes. A contractor with access to two folders produces a set of a few
//! hundred. Somebody in a company wide group produces a set of millions. A
//! single representation is wrong for one of them.
//!
//! So the bitmap holds a sorted list of ordinals while it is small and switches
//! to a word array once the list would cost more than the words do. The switch
//! is one way on purpose: a set that grew past the threshold is usually about to
//! be unioned with another one, and flapping between representations would cost
//! more than the memory saved.

use crate::DocId;

/// The number of ordinals above which the sparse form stops paying for itself.
///
/// A sparse entry costs four bytes. A dense word covers sixty four ordinals for
/// eight bytes. Below this many members the list is smaller for any realistic
/// universe, and above it the words win.
pub const DENSITY_THRESHOLD: usize = 4096;

/// A set of document ordinals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bitmap {
    inner: Repr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Repr {
    /// Ascending, deduplicated ordinals.
    Sparse(Vec<DocId>),
    /// One bit per ordinal, little endian within each word.
    Dense(Vec<u64>),
}

impl Default for Bitmap {
    fn default() -> Self {
        Self::new()
    }
}

impl Bitmap {
    /// Returns an empty bitmap.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            inner: Repr::Sparse(Vec::new()),
        }
    }

    /// Returns an empty bitmap that will hold ordinals up to `capacity` without
    /// growing again. Use it when the size of the universe is already known,
    /// which on a scan it always is.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        if capacity > DENSITY_THRESHOLD {
            return Self {
                inner: Repr::Dense(vec![0; words_for(capacity)]),
            };
        }
        Self {
            inner: Repr::Sparse(Vec::with_capacity(capacity)),
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

        if owned.len() > DENSITY_THRESHOLD {
            let highest = owned.last().copied().unwrap_or(0);
            let mut words = vec![0u64; words_for(highest as usize + 1)];
            for ordinal in &owned {
                set_bit(&mut words, *ordinal);
            }
            return Self {
                inner: Repr::Dense(words),
            };
        }
        Self {
            inner: Repr::Sparse(owned),
        }
    }

    /// Adds an ordinal and reports whether it was not already there.
    pub fn insert(&mut self, ordinal: DocId) -> bool {
        match &mut self.inner {
            Repr::Sparse(list) => match list.binary_search(&ordinal) {
                Ok(_) => false,
                Err(at) => {
                    list.insert(at, ordinal);
                    if list.len() > DENSITY_THRESHOLD {
                        self.densify();
                    }
                    true
                }
            },
            Repr::Dense(words) => {
                let index = word_index(ordinal);
                if index >= words.len() {
                    words.resize(index + 1, 0);
                }
                let mask = bit_mask(ordinal);
                let Some(word) = words.get_mut(index) else {
                    return false;
                };
                let was_set = *word & mask != 0;
                *word |= mask;
                !was_set
            }
        }
    }

    /// Removes an ordinal and reports whether it was there.
    pub fn remove(&mut self, ordinal: DocId) -> bool {
        match &mut self.inner {
            Repr::Sparse(list) => match list.binary_search(&ordinal) {
                Ok(at) => {
                    list.remove(at);
                    true
                }
                Err(_) => false,
            },
            Repr::Dense(words) => {
                let index = word_index(ordinal);
                let Some(word) = words.get_mut(index) else {
                    return false;
                };
                let mask = bit_mask(ordinal);
                let was_set = *word & mask != 0;
                *word &= !mask;
                was_set
            }
        }
    }

    /// Reports whether the ordinal is in the set.
    #[must_use]
    pub fn contains(&self, ordinal: DocId) -> bool {
        match &self.inner {
            Repr::Sparse(list) => list.binary_search(&ordinal).is_ok(),
            Repr::Dense(words) => words
                .get(word_index(ordinal))
                .is_some_and(|word| word & bit_mask(ordinal) != 0),
        }
    }

    /// Returns how many ordinals are in the set.
    #[must_use]
    pub fn len(&self) -> usize {
        match &self.inner {
            Repr::Sparse(list) => list.len(),
            Repr::Dense(words) => words
                .iter()
                .map(|word| word.count_ones() as usize)
                .sum::<usize>(),
        }
    }

    /// Reports whether the set is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        match &self.inner {
            Repr::Sparse(list) => list.is_empty(),
            Repr::Dense(words) => words.iter().all(|word| *word == 0),
        }
    }

    /// Reports whether the set is stored as words rather than as a list. It is
    /// exposed for tests and for the metrics an operator watches, not because a
    /// caller should branch on it.
    #[must_use]
    pub const fn is_dense(&self) -> bool {
        matches!(self.inner, Repr::Dense(_))
    }

    /// Keeps only the ordinals that are also in `other`.
    ///
    /// This is the operation the query path runs most: candidates from a term,
    /// intersected with what the reader may see. It is in place because
    /// allocating a result set per query per term is exactly the cost that stops
    /// a search engine scaling.
    pub fn intersect_with(&mut self, other: &Self) {
        match (&mut self.inner, &other.inner) {
            (Repr::Dense(words), Repr::Dense(theirs)) => {
                for (index, word) in words.iter_mut().enumerate() {
                    *word &= theirs.get(index).copied().unwrap_or(0);
                }
                return;
            }
            (Repr::Sparse(list), _) => {
                list.retain(|ordinal| other.contains(*ordinal));
                return;
            }
            (Repr::Dense(_), Repr::Sparse(_)) => {}
        }
        // A dense set meeting a small one collapses back to a list, because the
        // result cannot be larger than the small side. Walking the small side
        // and probing the dense one costs one lookup per member of the small
        // side, where walking the dense side would cost one per member of the
        // large one. That is the difference between a reader with access to a
        // handful of folders being cheap to filter and being the slowest query
        // on the box.
        let kept: Vec<DocId> = other
            .to_vec()
            .into_iter()
            .filter(|ordinal| self.contains(*ordinal))
            .collect();
        *self = Self::from_sorted(&kept);
    }

    /// Adds every ordinal of `other`.
    pub fn union_with(&mut self, other: &Self) {
        if let (Repr::Dense(words), Repr::Dense(theirs)) = (&mut self.inner, &other.inner) {
            if words.len() < theirs.len() {
                words.resize(theirs.len(), 0);
            }
            for (word, their_word) in words.iter_mut().zip(theirs.iter()) {
                *word |= *their_word;
            }
            return;
        }
        for ordinal in other.to_vec() {
            self.insert(ordinal);
        }
    }

    /// Removes every ordinal of `other`.
    ///
    /// A deny list is applied with this, which is why it exists as its own
    /// operation rather than as a union of complements: a complement needs a
    /// universe size, and the universe here is whatever the segment happens to
    /// hold.
    pub fn difference_with(&mut self, other: &Self) {
        match (&mut self.inner, &other.inner) {
            (Repr::Dense(words), Repr::Dense(theirs)) => {
                for (index, word) in words.iter_mut().enumerate() {
                    *word &= !theirs.get(index).copied().unwrap_or(0);
                }
                return;
            }
            (Repr::Sparse(list), _) => {
                list.retain(|ordinal| !other.contains(*ordinal));
                return;
            }
            (Repr::Dense(_), Repr::Sparse(_)) => {}
        }
        for ordinal in other.to_vec() {
            self.remove(ordinal);
        }
    }

    /// Iterates the ordinals in ascending order.
    pub fn iter(&self) -> impl Iterator<Item = DocId> + '_ {
        Iter {
            bitmap: self,
            position: 0,
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

    fn densify(&mut self) {
        let Repr::Sparse(list) = &self.inner else {
            return;
        };
        let highest = list.last().copied().unwrap_or(0);
        let mut words = vec![0u64; words_for(highest as usize + 1)];
        for ordinal in list {
            set_bit(&mut words, *ordinal);
        }
        self.inner = Repr::Dense(words);
    }
}

impl FromIterator<DocId> for Bitmap {
    fn from_iter<I: IntoIterator<Item = DocId>>(iter: I) -> Self {
        let mut collected: Vec<DocId> = iter.into_iter().collect();
        collected.sort_unstable();
        Self::from_sorted(&collected)
    }
}

struct Iter<'a> {
    bitmap: &'a Bitmap,
    /// Index into the sparse list.
    position: usize,
    /// The remaining bits of the dense word being drained.
    word: u64,
    word_index: usize,
    primed: bool,
}

impl Iterator for Iter<'_> {
    type Item = DocId;

    fn next(&mut self) -> Option<DocId> {
        match &self.bitmap.inner {
            Repr::Sparse(list) => {
                let value = list.get(self.position).copied()?;
                self.position += 1;
                Some(value)
            }
            Repr::Dense(words) => {
                if !self.primed {
                    self.word = words.first().copied().unwrap_or(0);
                    self.primed = true;
                }
                loop {
                    if self.word != 0 {
                        let bit = self.word.trailing_zeros();
                        // Clearing the lowest set bit is one instruction and
                        // avoids re scanning the word from the start.
                        self.word &= self.word - 1;
                        let base = u32::try_from(self.word_index * 64).ok()?;
                        return Some(base + bit);
                    }
                    self.word_index += 1;
                    self.word = *words.get(self.word_index)?;
                }
            }
        }
    }
}

const fn words_for(ordinals: usize) -> usize {
    ordinals.div_ceil(64)
}

const fn word_index(ordinal: DocId) -> usize {
    (ordinal as usize) / 64
}

const fn bit_mask(ordinal: DocId) -> u64 {
    1u64 << (ordinal % 64)
}

fn set_bit(words: &mut Vec<u64>, ordinal: DocId) {
    let index = word_index(ordinal);
    if index >= words.len() {
        words.resize(index + 1, 0);
    }
    if let Some(word) = words.get_mut(index) {
        *word |= bit_mask(ordinal);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sparse(ordinals: &[DocId]) -> Bitmap {
        let map = Bitmap::from_sorted(ordinals);
        assert!(!map.is_dense(), "this fixture is meant to stay sparse");
        map
    }

    fn dense(count: DocId) -> Bitmap {
        let map: Bitmap = (0..count).collect();
        assert!(map.is_dense(), "this fixture is meant to be dense");
        map
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
    fn iteration_is_ascending_in_both_representations() {
        let ordinals = [9u32, 1, 64, 63, 4096, 0];
        let map = Bitmap::from_sorted(&ordinals);
        assert_eq!(map.to_vec(), vec![0, 1, 9, 63, 64, 4096]);

        let big = dense(10_000);
        let collected = big.to_vec();
        assert_eq!(collected.len(), 10_000);
        assert!(collected.is_sorted());
        assert_eq!(collected.first(), Some(&0));
        assert_eq!(collected.last(), Some(&9_999));
    }

    #[test]
    fn it_switches_to_words_once_the_list_stops_paying() {
        let mut map = Bitmap::new();
        for ordinal in 0..=u32::try_from(DENSITY_THRESHOLD).unwrap() {
            map.insert(ordinal);
        }
        assert!(map.is_dense());
        assert_eq!(map.len(), DENSITY_THRESHOLD + 1);
        assert!(map.contains(0));
        assert!(map.contains(u32::try_from(DENSITY_THRESHOLD).unwrap()));
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
    }

    #[test]
    fn intersecting_with_a_shorter_dense_map_clears_the_tail() {
        let mut a: Bitmap = (0..5_000u32).chain(core::iter::once(500_000)).collect();
        a.intersect_with(&dense(5_000));
        assert!(
            !a.contains(500_000),
            "the tail past the other map must clear"
        );
        assert_eq!(a.len(), 5_000);
    }

    #[test]
    fn union_adds_everything_from_the_other_side() {
        let mut a = sparse(&[1, 2]);
        a.union_with(&sparse(&[2, 3]));
        assert_eq!(a.to_vec(), vec![1, 2, 3]);

        let mut wide = dense(5_000);
        wide.union_with(&dense(10_000));
        assert_eq!(wide.len(), 10_000);
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
}
