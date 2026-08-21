//! What a query actually did.
//!
//! A search engine's timings are the easy half of its performance. The hard half
//! is why a query took what it took, and that question is unanswerable from a
//! stopwatch. A query over a term in four hundred thousand documents that runs in
//! twenty nine milliseconds could be decoding every posting because the pruning
//! never fires, or decoding almost none of them and losing the time somewhere
//! else, and those two need opposite fixes.
//!
//! So the walk counts what it does. The numbers here are the ones that separate
//! those cases: how many postings were in the lists the query opened, how many of
//! them it decoded, and how many blocks it stepped over without looking inside.
//!
//! # Not paying for it when nobody asked
//!
//! Counting on the innermost loop of the scorer would change the thing being
//! measured, which is the classic way an instrument lies. The walk is therefore
//! generic over [`Tally`], and the default implementation is [`Off`], whose
//! methods are empty. Monomorphisation removes them, so a plain
//! [`search`](crate::search::Searcher::search) compiles to the same code it did
//! before this module existed.
//!
//! The one exception is block decoding, which is counted inside the cursor
//! whether or not anybody asked. Decoding a block is a hundred and twenty eight
//! postings of unpacking and an increment beside it does not show up in a
//! measurement, and having the number available unconditionally is what lets a
//! caller ask a cursor what it cost after the fact.

use crate::residency::Residency;

/// What one query did, in counts rather than in time.
///
/// Every field is a number a person can check by hand against a small index,
/// which is the property that makes it worth trusting on a large one.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Counters {
    /// How many of the query's terms were in the index at all.
    ///
    /// A query for three words where this says two is a query where one word
    /// was analysed away or is simply absent, and that is worth seeing before
    /// wondering why the results look thin.
    pub terms: u32,
    /// How many postings are in the lists the query opened.
    ///
    /// This is the denominator. Everything else is only interesting against it.
    pub postings: u64,
    /// How many blocks those lists hold, counting the short leftovers at the end
    /// of each as one.
    pub blocks: u64,
    /// How many blocks were unpacked.
    pub blocks_decoded: u64,
    /// How many blocks the walk stepped over without unpacking.
    ///
    /// Worked out at the end as the blocks that were there minus the blocks that
    /// were read, rather than counted where the walk decides to skip. The two
    /// would agree if every skip were of a known number of blocks, and they are
    /// not: a seek past a threshold jumps as far as the skip table takes it and
    /// nobody on the deciding side knows how many blocks that was. Subtracting
    /// is exact and needs nothing on the hot path.
    ///
    /// This is the pruning working, and a query where it is zero over a long
    /// list is a query where the pruning did not fire.
    pub blocks_skipped: u64,
    /// How many postings were unpacked, which is the block count times the block
    /// size except at the end of a list.
    pub postings_decoded: u64,
    /// How many documents had a score computed for them.
    pub documents_scored: u64,
    /// How many documents the walk stepped over because they were deleted.
    ///
    /// A deletion is not a hole in the lists, so the walk meets a deleted
    /// document the way it meets any other and then declines to answer with it.
    /// This is what that cost, and it is also the only way to see from outside
    /// that a segment is carrying deletions at all. A store where it is a large
    /// fraction of the postings decoded is a store that wants compacting.
    pub documents_hidden: u64,
    /// How many times a cursor was moved somewhere the walk named, rather than
    /// to the next thing in front of it.
    ///
    /// A document, for the walks that go in document order, and a block, for the
    /// one that takes them in order of what they could score. Both are the walk
    /// deciding where to look next instead of reading on, which is what this
    /// counts.
    pub seeks: u64,
    /// How many times a cursor was moved to its next document.
    pub advances: u64,
    /// What the query cost in memory, when somebody asked.
    ///
    /// The odd one out. Every other field here is filled in by the walk as it
    /// goes, and this one is filled in by whoever wrapped the walk, because
    /// faults are counted by the operating system around a span of time rather
    /// than by the code inside it. `None` means nobody asked. See
    /// [`residency`](crate::residency) for what it can and cannot say.
    pub residency: Option<Residency>,
}

impl Counters {
    /// What fraction of the postings in the lists were never unpacked.
    ///
    /// One is a query that answered itself from the skip tables, zero is a query
    /// that read everything. This is the single number to look at first.
    #[must_use]
    #[expect(
        clippy::cast_precision_loss,
        reason = "a ratio of two counts, where the last few bits of a corpus sized \
                  integer do not change the answer to two decimal places"
    )]
    pub fn skipped(&self) -> f32 {
        if self.postings == 0 {
            return 0.0;
        }
        let read = self.postings_decoded.min(self.postings);
        1.0 - (read as f32 / self.postings as f32)
    }
}

/// Somewhere for the walk to report what it did.
///
/// Implemented by [`Counters`], which keeps the numbers, and by [`Off`], which
/// throws them away and costs nothing.
pub trait Tally {
    /// The lists the query opened, and what is in them.
    fn opened(&mut self, terms: u32, postings: u64, blocks: u64);
    /// A document was scored.
    fn scored(&mut self);
    /// A document was passed over because it has been deleted.
    fn hidden(&mut self);
    /// A cursor was moved somewhere the walk named.
    fn sought(&mut self);
    /// A cursor was moved to its next document.
    fn advanced(&mut self);
    /// What the cursors decoded, collected once the walk is over.
    fn decoded(&mut self, blocks: u64, postings: u64);
}

/// A tally that keeps nothing.
///
/// The default for every public search call. Its methods are empty and inline,
/// so the walk that uses it is the walk that existed before there was anything
/// to count.
#[derive(Debug, Default, Clone, Copy)]
pub struct Off;

impl Tally for Off {
    #[inline]
    fn opened(&mut self, _terms: u32, _postings: u64, _blocks: u64) {}
    #[inline]
    fn scored(&mut self) {}
    #[inline]
    fn hidden(&mut self) {}
    #[inline]
    fn sought(&mut self) {}
    #[inline]
    fn advanced(&mut self) {}
    #[inline]
    fn decoded(&mut self, _blocks: u64, _postings: u64) {}
}

impl Tally for Counters {
    #[inline]
    fn opened(&mut self, terms: u32, postings: u64, blocks: u64) {
        self.terms = terms;
        self.postings = postings;
        self.blocks = blocks;
    }

    #[inline]
    fn scored(&mut self) {
        self.documents_scored += 1;
    }

    #[inline]
    fn hidden(&mut self) {
        self.documents_hidden += 1;
    }

    #[inline]
    fn sought(&mut self) {
        self.seeks += 1;
    }

    #[inline]
    fn advanced(&mut self) {
        self.advances += 1;
    }

    #[inline]
    fn decoded(&mut self, blocks: u64, postings: u64) {
        self.blocks_decoded += blocks;
        self.postings_decoded += postings;
        self.blocks_skipped = self.blocks.saturating_sub(self.blocks_decoded);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_query_that_read_nothing_skipped_everything() {
        let mut counters = Counters::default();
        counters.opened(1, 1_000, 8);
        assert!((counters.skipped() - 1.0).abs() < 1e-6);
    }

    #[test]
    fn a_query_that_read_everything_skipped_nothing() {
        let mut counters = Counters::default();
        counters.opened(1, 1_000, 8);
        counters.decoded(8, 1_000);
        assert!(counters.skipped().abs() < 1e-6);
    }

    #[test]
    fn a_block_decode_past_the_end_of_a_list_does_not_read_a_negative_fraction() {
        // The last block of a list is padded out to a whole block, so the
        // postings decoded can exceed the postings in the list. The fraction is
        // a fraction either way.
        let mut counters = Counters::default();
        counters.opened(1, 100, 1);
        counters.decoded(1, 128);
        assert!(counters.skipped().abs() < 1e-6);
    }

    #[test]
    fn an_empty_query_reports_nothing_rather_than_dividing_by_zero() {
        assert!(Counters::default().skipped().abs() < 1e-6);
    }
}
