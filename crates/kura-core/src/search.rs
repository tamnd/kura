//! Ranked retrieval.
//!
//! This is the part of a search engine that people mean when they say search:
//! given a few words, find the handful of documents that best answer them,
//! without looking at the ones that cannot.
//!
//! The ranking is BM25 and the retrieval is block-max WAND. Neither is novel,
//! and that is the point. What decides whether a query takes a millisecond or a
//! hundred is not the formula, it is how much of the posting lists gets decoded,
//! and both of those are the standard answers to that question.
//!
//! The idea behind WAND is that a document cannot score more than the sum of the
//! best each of its terms can ever contribute. Once `k` results are in hand,
//! anything whose ceiling is under the worst of them cannot displace it, so the
//! lists can be skipped forward past it without decoding a frequency. Block-max
//! sharpens that: the posting list stores the largest frequency in each block of
//! 128, so the ceiling can be computed for the block a cursor is sitting in
//! rather than for the term as a whole, and a block whose ceiling is too low is
//! skipped whole.
//!
//! What makes that cheap here is that the ceiling per block is already on disk.
//! It is the byte per block the posting format writes, and reading it costs one
//! indexed load rather than a decode.
//!
//! # More than one segment
//!
//! A store is not one segment and never becomes one. Writes land in a new
//! segment beside the ones already there, so a query runs over all of them and
//! still has to come back with one page, and that page is only right if a
//! document's score does not depend on which segment it happens to sit in.
//!
//! Which rules out searching each segment on its own and merging the answers.
//! Two of the three quantities BM25 needs belong to the corpus rather than to a
//! segment: how many documents there are, and how many of them hold the term. A
//! word in one document of a small segment and in a thousand documents of a
//! large one is one word with one weight, and giving it two weights produces a
//! page that reorders itself the moment those segments are merged. So the counts
//! are taken across every segment before anything is walked, and the walk uses
//! them everywhere.
//!
//! The merge is the heap the single segment walk was already filling. Segments
//! go into it one after another, which means the threshold the first segment
//! reached prunes the second, and by the last one the bar is as high as it is
//! going to get. Filling a heap per segment and merging at the end gives the
//! same page for more work, because every segment would start again from a
//! threshold of zero.

use crate::DocId;
use crate::analysis::Analyzer;
use crate::error::{Error, Result};
use crate::explain::{Counters, Off, Tally};
use crate::index::Reader;
use crate::posting::{self, BLOCK_SIZE, Cursor};

/// How quickly a term's contribution saturates as it repeats.
///
/// The value everybody uses. A document with ten occurrences of a word is more
/// about that word than one with two, but not five times more, and `k1` is the
/// knob that says how much of the difference survives.
pub const K1: f32 = 1.2;

/// How much a document's length is held against it.
///
/// At zero, length is ignored and long documents win everything by having more
/// chances to match. At one, the correction is full. Three quarters is the
/// value the literature settled on and it is a reasonable default for prose.
pub const B: f32 = 0.75;

/// A document and what it scored.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Hit {
    /// Which document, numbered across the segments the searcher was given.
    ///
    /// For a searcher over one segment that is the segment's own identifier.
    /// Over several it is the segment's identifier plus the documents in the
    /// segments before it, which is what lets hits from different segments sit
    /// in one ordered page. [`Searcher::locate`] takes it back apart, and the
    /// numbering only means anything for the searcher that produced it, because
    /// it depends on which segments that searcher was given and in what order.
    pub doc: DocId,
    /// What it scored. Higher is better, and the number is only comparable
    /// against other hits for the same query.
    pub score: f32,
}

/// Runs queries against one segment or across several.
#[derive(Debug)]
pub struct Searcher<'a, 'b> {
    segments: &'a [Reader<'b>],
    /// How many documents the segments hold between them, which is the `N` of
    /// the inverse document frequency.
    documents: u32,
    /// How many terms they hold between them, which over that count is the mean
    /// length BM25 normalises by.
    total: u64,
    k1: f32,
    b: f32,
}

impl<'a, 'b> Searcher<'a, 'b> {
    /// A searcher over one segment, with the usual BM25 parameters.
    #[must_use]
    pub const fn new(index: &'a Reader<'b>) -> Self {
        Self::one(index, K1, B)
    }

    /// A searcher over one segment with parameters of the caller's choosing.
    ///
    /// Worth tuning per corpus and not worth guessing at. Code is short and
    /// title fields want a different `b` from long prose.
    #[must_use]
    pub const fn with_parameters(index: &'a Reader<'b>, k1: f32, b: f32) -> Self {
        Self::one(index, k1, b)
    }

    /// A searcher over several segments, which scores them as one corpus.
    ///
    /// The order matters, because it is what decides the numbering the hits come
    /// back with. Give the same segments in the same order and the same query
    /// gives the same page.
    ///
    /// # Errors
    ///
    /// Returns [`Error::TooManyDocuments`] if the segments hold more documents
    /// between them than one numbering can address. A segment counts its own
    /// documents in 32 bits, so the store that trips this has upwards of four
    /// billion of them and wants sharding rather than a wider integer.
    pub fn over(segments: &'a [Reader<'b>]) -> Result<Self> {
        Self::over_with_parameters(segments, K1, B)
    }

    /// A searcher over several segments with parameters of the caller's
    /// choosing.
    ///
    /// # Errors
    ///
    /// Returns [`Error::TooManyDocuments`] for the same reason
    /// [`over`](Self::over) does.
    pub fn over_with_parameters(segments: &'a [Reader<'b>], k1: f32, b: f32) -> Result<Self> {
        let mut documents = 0u64;
        let mut total = 0u64;
        for index in segments {
            documents = documents.saturating_add(u64::from(index.documents()));
            total = total.saturating_add(index.total_length());
        }
        // Strictly under the largest identifier there is, rather than up to it,
        // because that one means a spent list everywhere else in this module and
        // a hit is not the place to start making it mean two things.
        let Ok(documents) = u32::try_from(documents) else {
            return Err(Error::TooManyDocuments { count: documents });
        };
        if documents == DocId::MAX {
            return Err(Error::TooManyDocuments {
                count: u64::from(documents),
            });
        }
        Ok(Self {
            segments,
            documents,
            total,
            k1,
            b,
        })
    }

    /// The shared part of the single segment constructors, which cannot fail and
    /// so does not say that it might.
    #[must_use]
    const fn one(index: &'a Reader<'b>, k1: f32, b: f32) -> Self {
        Self {
            segments: core::slice::from_ref(index),
            documents: index.documents(),
            total: index.total_length(),
            k1,
            b,
        }
    }

    /// The segments this searcher runs over, in the order it numbers them.
    #[must_use]
    pub const fn segments(&self) -> &'a [Reader<'b>] {
        self.segments
    }

    /// How many documents those segments hold between them.
    #[must_use]
    pub const fn documents(&self) -> u32 {
        self.documents
    }

    /// The mean document length across every segment.
    ///
    /// Not the mean of the segments' own means, which is only the same number
    /// when the segments are the same size, and segments are not the same size.
    #[must_use]
    #[expect(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        reason = "an average over a corpus is wanted to a few significant figures, \
                  and the division runs in f64 so that only the result is narrowed"
    )]
    pub fn average_length(&self) -> f32 {
        if self.documents == 0 {
            return 0.0;
        }
        (self.total as f64 / f64::from(self.documents)) as f32
    }

    /// Which segment a hit came from, and what that segment calls the document.
    ///
    /// This is what a caller needs to go and fetch the document, because stored
    /// fields live in the segment and are addressed the way the segment
    /// addresses them. Returns nothing for an identifier past the end of the
    /// last segment, which is an identifier from some other searcher.
    #[must_use]
    pub fn locate(&self, doc: DocId) -> Option<(usize, DocId)> {
        let mut base: DocId = 0;
        for (at, index) in self.segments.iter().enumerate() {
            let end = base.saturating_add(index.documents());
            if doc < end {
                return Some((at, doc - base));
            }
            base = end;
        }
        None
    }

    /// Analyses `query` and returns the best `k` documents for it.
    ///
    /// The query goes through the same analyser the documents did, which is the
    /// only way the terms on the two sides can be the same terms.
    ///
    /// # Errors
    ///
    /// Returns an error if a posting list in the index does not decode.
    pub fn search(&self, query: &str, k: usize) -> Result<Vec<Hit>> {
        let words = analyse(query);
        let terms: Vec<&[u8]> = words.iter().map(Vec::as_slice).collect();
        self.search_terms(&terms, k)
    }

    /// How many documents hold at least one of the query's terms.
    ///
    /// This is a different question from what [`search`](Self::search) answers
    /// and it costs more, because a total cannot be pruned: every posting has
    /// to be looked at to know whether its document was already counted. A
    /// caller that only needs a page of results should not ask for it.
    ///
    /// # Errors
    ///
    /// Returns an error if a posting list in the index does not decode.
    pub fn count(&self, query: &str) -> Result<u64> {
        let words = analyse(query);
        let terms: Vec<&[u8]> = words.iter().map(Vec::as_slice).collect();
        self.count_terms(&terms)
    }

    /// How many documents hold at least one of a set of analysed terms.
    ///
    /// # Errors
    ///
    /// Returns an error if a posting list in the index does not decode.
    pub fn count_terms(&self, terms: &[&[u8]]) -> Result<u64> {
        self.count_with(terms, &mut Off)
    }

    /// How many documents hold at least one of a query's terms, and what the
    /// walk did to find out.
    ///
    /// # Errors
    ///
    /// Returns an error if a posting list in the index does not decode.
    pub fn count_explained(&self, query: &str) -> Result<(u64, Counters)> {
        let words = analyse(query);
        let terms: Vec<&[u8]> = words.iter().map(Vec::as_slice).collect();
        let mut counters = Counters::default();
        let total = self.count_with(&terms, &mut counters)?;
        Ok((total, counters))
    }

    fn count_with<T: Tally>(&self, terms: &[&[u8]], tally: &mut T) -> Result<u64> {
        let mut shards = self.open(terms, tally)?;
        let mut total = 0;
        // Summed rather than merged, because two segments never hold the same
        // document and so the unions they count are disjoint.
        for shard in &mut shards {
            total += count_lists(&mut shard.lists, tally)?;
            shard.lists.report(tally);
        }
        Ok(total)
    }

    /// The best `k` documents and how many there are in all, in one pass.
    ///
    /// Asking for both separately walks the lists twice, and the walk is most of
    /// what a query costs. This walks them once. The total needs every document
    /// looked at, so there is no pruning to give up, and what would have been
    /// pruned is skipped anyway: a document is only scored when the terms on it
    /// could between them beat the worst hit held so far, which is decided from
    /// bounds that are already in hand rather than from frequencies that would
    /// have to be decoded.
    ///
    /// A result page and a total is what a search box shows, so this is the call
    /// most callers want.
    ///
    /// # Errors
    ///
    /// Returns an error if a posting list in the index does not decode.
    pub fn search_and_count(&self, query: &str, k: usize) -> Result<(Vec<Hit>, u64)> {
        let words = analyse(query);
        let terms: Vec<&[u8]> = words.iter().map(Vec::as_slice).collect();
        self.search_and_count_terms(&terms, k)
    }

    /// The best `k` documents and how many there are in all, for terms that are
    /// already analysed.
    ///
    /// # Errors
    ///
    /// Returns an error if a posting list in the index does not decode.
    pub fn search_and_count_terms(&self, terms: &[&[u8]], k: usize) -> Result<(Vec<Hit>, u64)> {
        self.search_and_count_with(terms, k, &mut Off)
    }

    /// The best `k` documents, the total, and a count of what the walk did.
    ///
    /// The same answer [`search_and_count`](Self::search_and_count) gives, with
    /// the numbers that say what it cost. See [`crate::explain`].
    ///
    /// # Errors
    ///
    /// Returns an error if a posting list in the index does not decode.
    pub fn search_and_count_explained(
        &self,
        query: &str,
        k: usize,
    ) -> Result<(Vec<Hit>, u64, Counters)> {
        let words = analyse(query);
        let terms: Vec<&[u8]> = words.iter().map(Vec::as_slice).collect();
        let mut counters = Counters::default();
        let (hits, total) = self.search_and_count_with(&terms, k, &mut counters)?;
        Ok((hits, total, counters))
    }

    fn search_and_count_with<T: Tally>(
        &self,
        terms: &[&[u8]],
        k: usize,
        tally: &mut T,
    ) -> Result<(Vec<Hit>, u64)> {
        if k == 0 {
            return Ok((Vec::new(), self.count_terms(terms)?));
        }
        let mut shards = self.open(terms, tally)?;
        let norm = Norm::new(self.k1, self.b, self.average_length());
        let mut top = TopK::new(k);
        let mut total = 0u64;
        for shard in &mut shards {
            total += if shard.lists.len() == 1 {
                // One term, and its total is in the header of its own list.
                // Nothing has to be walked to know it, so the search can prune
                // the way it does when no total was asked for.
                let total = u64::from(shard.lists.counts[0]);
                self.page_of_one(shard, norm, &mut top, tally)?;
                total
            } else {
                self.page_and_total_of(shard, norm, &mut top, tally)?
            };
            shard.lists.report(tally);
        }
        Ok((top.into_sorted(), total))
    }

    /// The page and the total for one segment, in one walk over its lists.
    fn page_and_total_of<T: Tally>(
        &self,
        shard: &mut Shard<'a, 'b>,
        norm: Norm,
        top: &mut TopK,
        tally: &mut T,
    ) -> Result<u64> {
        let lists = &mut shard.lists;
        let floor = self.k1 * (1.0 - self.b);
        let mut total = 0u64;
        loop {
            let (which, doc, second) = lists.front_two();
            if doc == DocId::MAX {
                return Ok(total);
            }

            // A block of one list that no other list reaches into, whose best
            // posting cannot beat the worst hit in hand, contributes documents
            // to the total and nothing to the page. Those are counted in one
            // step instead of being walked and rejected one at a time.
            if let Some(last) = lists.cursors[which].block_last()
                && last < second
                && lists.block_bound(which, self.k1, floor) <= top.threshold()
            {
                total += lists.take_block(which, last, tally)?;
                continue;
            }

            total += 1;

            // What the terms sitting on this document could add up to at their
            // best. This costs an addition per term and saves the frequency
            // decode, the length lookup and the division that scoring would.
            let ceiling: f32 = lists
                .heads
                .iter()
                .filter(|head| head.doc == doc)
                .map(|head| head.bound)
                .sum();
            if ceiling > top.threshold() {
                let norm = norm.of(shard.index, doc);
                let mut score = 0.0;
                for at in 0..lists.len() {
                    if lists.heads[at].doc == doc {
                        score += lists.score(at, self.k1, norm);
                    }
                }
                tally.scored();
                top.push(Hit {
                    doc: shard.base.saturating_add(doc),
                    score,
                });
            }

            for at in 0..lists.len() {
                if lists.heads[at].doc == doc {
                    lists.advance(at, tally)?;
                }
            }
        }
    }

    /// Returns the best `k` documents for a set of terms that are already
    /// analysed.
    ///
    /// A repeated term is counted once. Weighting a term by how often it appears
    /// in the query only pays off for queries far longer than anybody types, and
    /// it costs a multiply on the innermost loop of the scorer.
    ///
    /// # Errors
    ///
    /// Returns an error if a posting list in the index does not decode.
    pub fn search_terms(&self, terms: &[&[u8]], k: usize) -> Result<Vec<Hit>> {
        self.search_with(terms, k, &mut Off)
    }

    /// The best `k` documents, and a count of what the walk did to find them.
    ///
    /// The same answer [`search`](Self::search) gives, with the numbers that say
    /// what it cost. See [`crate::explain`] for what they mean and for why
    /// asking for them does not change them.
    ///
    /// # Errors
    ///
    /// Returns an error if a posting list in the index does not decode.
    pub fn search_explained(&self, query: &str, k: usize) -> Result<(Vec<Hit>, Counters)> {
        let words = analyse(query);
        let terms: Vec<&[u8]> = words.iter().map(Vec::as_slice).collect();
        self.search_terms_explained(&terms, k)
    }

    /// The best `k` documents for analysed terms, and what the walk did.
    ///
    /// # Errors
    ///
    /// Returns an error if a posting list in the index does not decode.
    pub fn search_terms_explained(
        &self,
        terms: &[&[u8]],
        k: usize,
    ) -> Result<(Vec<Hit>, Counters)> {
        let mut counters = Counters::default();
        let hits = self.search_with(terms, k, &mut counters)?;
        Ok((hits, counters))
    }

    fn search_with<T: Tally>(&self, terms: &[&[u8]], k: usize, tally: &mut T) -> Result<Vec<Hit>> {
        if k == 0 || self.documents == 0 {
            return Ok(Vec::new());
        }
        let mut shards = self.open(terms, tally)?;
        let norm = Norm::new(self.k1, self.b, self.average_length());
        let mut top = TopK::new(k);
        // In order, into the one heap. What the segments before this one found
        // is what this one has to beat, so the pruning gets stronger as the
        // query goes along rather than starting again at each segment.
        for shard in &mut shards {
            if shard.lists.len() == 1 {
                self.page_of_one(shard, norm, &mut top, tally)?;
            } else {
                self.page_of(shard, norm, &mut top, tally)?;
            }
            shard.lists.report(tally);
        }
        Ok(top.into_sorted())
    }

    /// The best documents in one segment, into a heap that may already hold
    /// better ones from another.
    fn page_of<T: Tally>(
        &self,
        shard: &mut Shard<'a, 'b>,
        norm: Norm,
        top: &mut TopK,
        tally: &mut T,
    ) -> Result<()> {
        let lists = &mut shard.lists;
        // The smallest the denominator of the tf factor can get, which is what
        // turns a frequency into an upper bound on a score.
        let floor = self.k1 * (1.0 - self.b);
        let mut threshold = top.threshold();
        // The lists in order of where their cursors are. This is a list of
        // subscripts rather than the lists themselves because it is sorted on
        // every iteration and a cursor is a kilobyte, so sorting the lists would
        // copy kilobytes to move a term one place.
        let mut order: Vec<usize> = (0..lists.len()).collect();

        loop {
            order.sort_unstable_by_key(|&at| lists.heads[at].doc);
            // A spent list carries the largest identifier there is, so the sort
            // leaves the live ones in front and this is where they stop.
            let live = order.partition_point(|&at| lists.heads[at].doc != DocId::MAX);
            let Some(pivot) = pivot(lists, &order[..live], threshold) else {
                break;
            };
            let candidate = lists.heads[order[pivot]].doc;

            if lists.heads[order[0]].doc != candidate {
                // The lists before the pivot are behind it, and nothing between
                // where they are and the pivot can reach the threshold, so there
                // is no reason to decode any of it.
                for &at in &order[..pivot] {
                    lists.seek(at, candidate, tally)?;
                }
                continue;
            }

            // Every list on the candidate, which is not the same thing as every
            // list up to the pivot. The pivot is where the running total of the
            // terms' own bounds first passes the threshold, and lists after it
            // can be sitting on the candidate too, because the sort puts equal
            // documents next to each other and the pivot lands wherever the
            // arithmetic lands. Those lists score the candidate, so leaving them
            // out of the bound below is a bound that is too low, and a bound
            // that is too low skips a document that belonged on the page.
            let mut moved = pivot;
            while moved + 1 < live && lists.heads[order[moved + 1]].doc == candidate {
                moved += 1;
            }

            // Before paying for the frequencies, ask what the blocks these
            // cursors are sitting in could possibly add up to.
            let ceiling: f32 = order[..=moved]
                .iter()
                .map(|&at| lists.block_bound(at, self.k1, floor))
                .sum();
            if ceiling <= threshold {
                // As far as the shortest of those blocks reaches, and no
                // further than the next list along. The bound just computed
                // covers the lists on the candidate and nothing else, so it says
                // nothing about a document that a list further back would also
                // score, and that list starts at the document its cursor is on.
                let mut next = order[..=moved]
                    .iter()
                    .filter_map(|&at| lists.cursors[at].block_last())
                    .min()
                    .unwrap_or(candidate)
                    .saturating_add(1)
                    .max(candidate.saturating_add(1));
                if moved + 1 < live {
                    next = next.min(lists.heads[order[moved + 1]].doc);
                }
                for at in 0..lists.len() {
                    if lists.heads[at].doc < next {
                        lists.seek(at, next, tally)?;
                    }
                }
                continue;
            }

            let moved = moved + 1;
            // Summed in the order the terms were given rather than in the order
            // the cursors happen to be sitting in. Addition of floats is not
            // associative, so the second order would give a score that depends
            // on where the cursors are, and two documents a hundredth of a
            // millionth apart would swap places according to which segment they
            // were in. Which they do: this is not theoretical.
            let norm = norm.of(shard.index, candidate);
            let mut score = 0.0;
            for at in 0..lists.len() {
                if lists.heads[at].doc == candidate {
                    score += lists.score(at, self.k1, norm);
                }
            }
            tally.scored();
            top.push(Hit {
                doc: shard.base.saturating_add(candidate),
                score,
            });
            threshold = top.threshold();
            for &at in &order[..moved] {
                lists.advance(at, tally)?;
            }
        }

        Ok(())
    }

    /// The best documents in one segment for a query that came down to one list.
    ///
    /// One term is the commonest query there is and it has no pivot to find:
    /// every document in the list is a candidate and the only question is which
    /// of them score. What is left of the pruning is the block bound, which
    /// still steps over a whole block whose best posting cannot displace the
    /// worst hit in hand.
    fn page_of_one<T: Tally>(
        &self,
        shard: &mut Shard<'a, 'b>,
        norm: Norm,
        top: &mut TopK,
        tally: &mut T,
    ) -> Result<()> {
        let lists = &mut shard.lists;
        let floor = self.k1 * (1.0 - self.b);
        // The block bound only changes when the block does, and this is the one
        // walk that asks for it on every document, so it is worked out once per
        // block rather than once per posting.
        let mut cached = (usize::MAX, 0.0f32);

        while lists.heads[0].doc != DocId::MAX {
            let doc = lists.heads[0].doc;
            let block = lists.cursors[0].block();
            if cached.0 != block {
                cached = (block, lists.block_bound(0, self.k1, floor));
            }
            if cached.1 <= top.threshold() {
                let next = lists.cursors[0]
                    .block_last()
                    .unwrap_or(doc)
                    .saturating_add(1)
                    .max(doc.saturating_add(1));
                lists.seek(0, next, tally)?;
                continue;
            }
            tally.scored();
            top.push(Hit {
                doc: shard.base.saturating_add(doc),
                score: lists.score(0, self.k1, norm.of(shard.index, doc)),
            });
            lists.advance(0, tally)?;
        }

        Ok(())
    }

    /// Opens a cursor in every segment for every term that is in one.
    ///
    /// Two passes over the segments, because how surprising a term is depends on
    /// how many documents in the store hold it and that is not known until the
    /// last segment has been asked. The lists found on the way are kept rather
    /// than looked up again: a posting list here is a borrowed view of bytes
    /// that are already mapped, so holding one costs a pointer and a length,
    /// where finding it again costs a walk through a dictionary.
    fn open<T: Tally>(&self, terms: &[&[u8]], tally: &mut T) -> Result<Vec<Shard<'a, 'b>>> {
        let mut found: Vec<Vec<Option<posting::Reader<'b>>>> =
            Vec::with_capacity(self.segments.len());
        let mut holding = vec![0u32; terms.len()];
        for index in self.segments {
            let mut row = Vec::with_capacity(terms.len());
            for (at, term) in terms.iter().enumerate() {
                let list = index.postings(term)?.filter(|list| !list.is_empty());
                if let Some(list) = &list {
                    // A term cannot be in more documents than the store holds,
                    // and that count is a `u32`, so this cannot really
                    // saturate.
                    holding[at] = holding[at].saturating_add(list.len());
                }
                row.push(list);
            }
            found.push(row);
        }
        // The two numbers a term contributes with, worked out once for the whole
        // query rather than once per segment. This is what makes a document's
        // score independent of the segment it landed in.
        let weights: Vec<(f32, f32)> = holding
            .iter()
            .map(|&holding| {
                let idf = idf(self.documents, holding);
                (idf, idf * (self.k1 + 1.0))
            })
            .collect();

        let mut shards = Vec::with_capacity(self.segments.len());
        let mut base: DocId = 0;
        let mut opened = 0u32;
        let mut postings = 0u64;
        let mut blocks = 0u64;
        for (index, row) in self.segments.iter().zip(found) {
            let mut lists = Lists {
                heads: Vec::with_capacity(terms.len()),
                cursors: Vec::with_capacity(terms.len()),
                counts: Vec::with_capacity(terms.len()),
            };
            for (at, list) in row.into_iter().enumerate() {
                let Some(list) = list else {
                    continue;
                };
                let mut cursor = list.cursor();
                let Some(doc) = cursor.advance()? else {
                    continue;
                };
                postings += u64::from(list.len());
                // The leftovers at the end of a list are a block as far as a
                // walk is concerned, so they count as one here.
                blocks += list.blocks() as u64;
                if list.len() as usize > list.blocks() * BLOCK_SIZE {
                    blocks += 1;
                }
                let (idf, bound) = weights[at];
                lists.heads.push(Head { doc, idf, bound });
                lists.cursors.push(cursor);
                lists.counts.push(list.len());
            }
            if !lists.is_empty() {
                shards.push(Shard { index, base, lists });
            }
            base = base.saturating_add(index.documents());
        }
        // How many of the query's terms were found somewhere, which is a
        // question about the store rather than about a segment, so a term in
        // eight segments is one term and not eight. A query with four billion
        // terms in it is not a query, so the saturation is a formality rather
        // than a case.
        for held in &holding {
            if *held > 0 {
                opened = opened.saturating_add(1);
            }
        }
        tally.opened(opened, postings, blocks);
        Ok(shards)
    }
}

/// One segment's share of a query.
///
/// The lists it opened, the reader they came out of, because scoring needs
/// document lengths and those live in the segment, and where this segment's
/// documents start in the numbering the hits come back with.
#[derive(Debug)]
struct Shard<'a, 'b> {
    index: &'a Reader<'b>,
    base: DocId,
    lists: Lists<'b>,
}

/// How many documents in one segment hold at least one of the query's terms.
fn count_lists<T: Tally>(lists: &mut Lists<'_>, tally: &mut T) -> Result<u64> {
    // One term is the common case and its answer is already in the header of its
    // posting list, so nothing needs decoding at all.
    if lists.len() <= 1 {
        return Ok(lists.counts.first().map_or(0, |&count| u64::from(count)));
    }
    let mut total = 0;
    loop {
        let (which, doc, second) = lists.front_two();
        if doc == DocId::MAX {
            return Ok(total);
        }

        // Where the block this list is in ends before any other list starts,
        // every document left in it belongs to the union and belongs to nobody
        // else, so the whole block is one step rather than a hundred and twenty
        // eight. On the query shape that costs the most, one common term and one
        // rare one, this is nearly all of the walk.
        if let Some(last) = lists.cursors[which].block_last()
            && last < second
        {
            total += lists.take_block(which, last, tally)?;
            continue;
        }

        total += 1;
        for at in 0..lists.len() {
            if lists.heads[at].doc == doc {
                lists.advance(at, tally)?;
            }
        }
    }
}

/// The hot half of a query term: where its cursor is and what it could add.
///
/// This is kept away from the cursor because a cursor holds a decoded block and
/// so is a kilobyte wide, and the walk reads these three numbers once per term
/// for every document it steps over. Held inside the cursors, that would be a
/// fresh cache line per term per step; held here it is a few words that stay in
/// cache for the whole query.
#[derive(Debug, Clone, Copy)]
struct Head {
    /// Where the cursor is, or [`DocId::MAX`] once the list is spent. Nothing
    /// else can carry that identifier, because a corpus with four billion
    /// documents in one segment does not get written in the first place.
    doc: DocId,
    idf: f32,
    /// The most this term can add to any document's score, anywhere.
    bound: f32,
}

/// The posting lists one query is walking.
#[derive(Debug)]
struct Lists<'a> {
    heads: Vec<Head>,
    cursors: Vec<Cursor<'a>>,
    /// How many documents each list holds, which a total wants and the scorer
    /// has already spent to get the inverse document frequency.
    counts: Vec<u32>,
}

impl Lists<'_> {
    fn len(&self) -> usize {
        self.heads.len()
    }

    fn is_empty(&self) -> bool {
        self.heads.is_empty()
    }

    /// Which list is on the lowest document, that document, and the lowest
    /// document any of the others is on.
    ///
    /// The second one is what says how far the first can run on its own. Where
    /// two lists are on the same document the two come back equal, which is the
    /// answer that stops a caller skipping over a document it shares.
    fn front_two(&self) -> (usize, DocId, DocId) {
        let mut which = 0;
        let mut first = DocId::MAX;
        let mut second = DocId::MAX;
        for (at, head) in self.heads.iter().enumerate() {
            if head.doc < first {
                second = first;
                first = head.doc;
                which = at;
            } else if head.doc < second {
                second = head.doc;
            }
        }
        (which, first, second)
    }

    /// Takes the rest of one list's block in one step, and says how many
    /// documents that was.
    ///
    /// Only sound when the caller has established that no other list reaches
    /// into this block, which [`Lists::front_two`] is what answers.
    fn take_block<T: Tally>(&mut self, at: usize, last: DocId, tally: &mut T) -> Result<u64> {
        let taken = self.cursors[at].remaining_in_block() as u64;
        match last.checked_add(1) {
            Some(next) => self.seek(at, next, tally)?,
            None => self.heads[at].doc = DocId::MAX,
        }
        Ok(taken)
    }

    /// Moves one list to its next document.
    fn advance<T: Tally>(&mut self, at: usize, tally: &mut T) -> Result<()> {
        tally.advanced();
        self.heads[at].doc = self.cursors[at].advance()?.unwrap_or(DocId::MAX);
        Ok(())
    }

    /// Moves one list to the first document at or after `target`.
    fn seek<T: Tally>(&mut self, at: usize, target: DocId, tally: &mut T) -> Result<()> {
        tally.sought();
        self.heads[at].doc = self.cursors[at].seek(target)?.unwrap_or(DocId::MAX);
        Ok(())
    }

    /// Hands the cursors' decode counts to the tally, once the walk is over.
    ///
    /// Collected at the end rather than as it happens because the cursor is the
    /// only thing that knows a block was unpacked and it has no tally to tell.
    fn report<T: Tally>(&self, tally: &mut T) {
        let mut blocks = 0;
        let mut postings = 0;
        for cursor in &self.cursors {
            let (b, p) = cursor.decoded();
            blocks += b;
            postings += p;
        }
        tally.decoded(blocks, postings);
    }

    /// The most one term can add to a document in the block its cursor is in.
    ///
    /// The frequency comes from the byte the posting format stores per block,
    /// and the length is taken to be zero, which is the shortest a document can
    /// be and so the kindest the normalisation can be.
    #[expect(
        clippy::cast_precision_loss,
        reason = "a frequency past the range of f32 would need a document of four \
                  billion words, and it is a bound rather than a score"
    )]
    fn block_bound(&self, at: usize, k1: f32, floor: f32) -> f32 {
        let frequency = self.cursors[at].block_max_frequency() as f32;
        self.heads[at].idf * (frequency * (k1 + 1.0)) / (frequency + floor)
    }

    /// What one term adds to the document its cursor is on.
    #[expect(
        clippy::cast_precision_loss,
        reason = "the same bound as block_bound, on the same quantity"
    )]
    fn score(&self, at: usize, k1: f32, norm: f32) -> f32 {
        let frequency = self.cursors[at].frequency() as f32;
        self.heads[at].idf * (frequency * (k1 + 1.0)) / (frequency + norm)
    }
}

/// Runs a query through the analyser and returns its distinct terms in order.
fn analyse(query: &str) -> Vec<Vec<u8>> {
    let mut analyzer = Analyzer::new();
    let mut words: Vec<Vec<u8>> = Vec::new();
    analyzer.analyze(query, |term, _| words.push(term.to_vec()));
    words.sort_unstable();
    words.dedup();
    words
}

/// The first list whose cumulative bound could beat the threshold.
///
/// Everything before it is on a document that cannot win even if every one of
/// those terms is at its best, so the only candidate worth looking at is where
/// this one is. The bounds here are the ones that hold everywhere in a list
/// rather than the ones that hold in the block a cursor is in, because the lists
/// before the pivot are on documents of their own and a block bound taken from
/// where they are now says nothing about the candidate. The tighter bound is
/// used once the cursors agree, which is where it is sound.
fn pivot(lists: &Lists<'_>, order: &[usize], threshold: f32) -> Option<usize> {
    let mut sum = 0.0;
    for (at, &which) in order.iter().enumerate() {
        sum += lists.heads[which].bound;
        if sum > threshold {
            return Some(at);
        }
    }
    None
}

/// How surprising a term is.
///
/// A word in every document says nothing about which one is wanted, and a word
/// in one document says everything. This is the smoothed form, which stays
/// positive for a term that really is in every document rather than going
/// negative and subtracting from the score.
#[expect(
    clippy::cast_precision_loss,
    reason = "document counts are exact well past any corpus that fits on a disk, \
              and the result goes into a logarithm"
)]
fn idf(documents: u32, holding: u32) -> f32 {
    let n = documents as f32;
    let f = holding as f32;
    (1.0 + (n - f + 0.5) / (f + 0.5)).ln()
}

/// The length of a document, as a float.
#[expect(
    clippy::cast_precision_loss,
    reason = "a document longer than sixteen million terms is not a document"
)]
fn length_of(index: &Reader<'_>, doc: DocId) -> f32 {
    index.length(doc) as f32
}

/// The length normalisation, with the query's constants already folded in.
///
/// Written out, the denominator BM25 divides by is `k1 * (1 - b + b * len /
/// average)`, which is a division per document scored. Nothing in it varies
/// with the document except the length, so the rest is worked out once when the
/// query starts and what is left is a multiply and an add.
#[derive(Debug, Clone, Copy)]
struct Norm {
    base: f32,
    per_term: f32,
}

impl Norm {
    fn new(k1: f32, b: f32, average: f32) -> Self {
        Self {
            base: k1 * (1.0 - b),
            per_term: k1 * b / average.max(1.0),
        }
    }

    fn of(self, index: &Reader<'_>, doc: DocId) -> f32 {
        self.per_term.mul_add(length_of(index, doc), self.base)
    }
}

/// The best `k` hits seen so far, as a heap with the worst at the root.
///
/// A heap rather than a sorted list because `k` is a caller's number and the
/// cost of an insert has to stay logarithmic when somebody asks for a thousand.
#[derive(Debug)]
struct TopK {
    k: usize,
    hits: Vec<Hit>,
}

impl TopK {
    fn new(k: usize) -> Self {
        Self {
            k,
            hits: Vec::with_capacity(k),
        }
    }

    /// Whether `a` is worse than `b`, which is what the root of the heap holds.
    ///
    /// Equal scores are broken by document identifier, with the lower one
    /// winning, so that a query over the same index gives the same answer
    /// whatever order the lists happened to be walked in.
    #[expect(
        clippy::float_cmp,
        reason = "two hits scored by the same code over the same terms produce \
                  bit identical sums when they tie, and the tie is what is being \
                  asked about"
    )]
    fn worse(a: Hit, b: Hit) -> bool {
        if a.score == b.score {
            return a.doc > b.doc;
        }
        a.score < b.score
    }

    fn push(&mut self, hit: Hit) {
        if self.hits.len() < self.k {
            self.hits.push(hit);
            let mut at = self.hits.len() - 1;
            while at > 0 {
                let parent = (at - 1) / 2;
                if Self::worse(self.hits[at], self.hits[parent]) {
                    self.hits.swap(at, parent);
                    at = parent;
                } else {
                    break;
                }
            }
            return;
        }
        if !Self::worse(self.hits[0], hit) {
            return;
        }
        self.hits[0] = hit;
        let mut at = 0;
        loop {
            let left = at * 2 + 1;
            let right = left + 1;
            let mut worst = at;
            if left < self.hits.len() && Self::worse(self.hits[left], self.hits[worst]) {
                worst = left;
            }
            if right < self.hits.len() && Self::worse(self.hits[right], self.hits[worst]) {
                worst = right;
            }
            if worst == at {
                break;
            }
            self.hits.swap(at, worst);
            at = worst;
        }
    }

    /// The score a document has to beat to get in, or zero while there is room.
    fn threshold(&self) -> f32 {
        if self.hits.len() < self.k {
            return 0.0;
        }
        self.hits[0].score
    }

    fn into_sorted(mut self) -> Vec<Hit> {
        self.hits.sort_unstable_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(core::cmp::Ordering::Equal)
                .then(a.doc.cmp(&b.doc))
        });
        self.hits
    }
}

#[cfg(test)]
#[expect(
    clippy::cast_precision_loss,
    reason = "the check by hand mirrors the scorer, which carries the same note"
)]
mod tests {
    use super::*;
    use crate::index::Writer;
    use crate::segment::Segment;

    const DOCS: [&str; 6] = [
        "the quick brown fox jumps over the lazy dog",
        "the dog barks at the fox and the fox runs",
        "quick quick quick brown",
        "a lazy afternoon",
        "nothing to do with any of it",
        "fox",
    ];

    fn build(docs: &[&str]) -> Vec<u8> {
        let mut writer = Writer::new();
        for doc in docs {
            writer.add(doc).expect("a handful of documents fit");
        }
        writer.finish().expect("what was written decodes")
    }

    /// Scores every document the slow, obvious way, which is what the fast way
    /// has to agree with.
    ///
    /// A term at a time into an array of scores rather than a document at a
    /// time, so that a corpus large enough to make the pruning fire is still
    /// cheap enough to check exhaustively. The terms are added in the order they
    /// were given, which is the order the walk adds them in, so two scores that
    /// agree here agree to the last bit rather than to a tolerance.
    fn by_hand(index: &Reader<'_>, terms: &[&[u8]], k1: f32, b: f32) -> Vec<Hit> {
        let average = index.average_length().max(1.0);
        let mut scores = vec![0.0f32; index.documents() as usize];
        for term in terms {
            let Some(list) = index.postings(term).expect("decodes") else {
                continue;
            };
            let idf = idf(index.documents(), list.len());
            for (doc, frequency) in list.to_postings().expect("decodes") {
                let f = frequency as f32;
                let norm = k1 * (1.0 - b + b * index.length(doc) as f32 / average);
                scores[doc as usize] += idf * (f * (k1 + 1.0)) / (f + norm);
            }
        }
        let mut hits: Vec<Hit> = scores
            .iter()
            .enumerate()
            .filter(|(_, score)| **score > 0.0)
            .map(|(doc, &score)| Hit {
                doc: u32::try_from(doc).expect("a test corpus is not four billion documents"),
                score,
            })
            .collect();
        hits.sort_unstable_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .expect("no score is a nan")
                .then(a.doc.cmp(&b.doc))
        });
        hits
    }

    fn same(fast: &[Hit], slow: &[Hit]) {
        assert_eq!(fast.len(), slow.len(), "different number of hits");
        for (a, b) in fast.iter().zip(slow) {
            assert_eq!(a.doc, b.doc, "different document");
            assert!(
                (a.score - b.score).abs() < 1e-4,
                "different score for {}: {} against {}",
                a.doc,
                a.score,
                b.score
            );
        }
    }

    #[test]
    fn one_term_ranks_by_how_much_of_the_document_is_that_term() {
        let bytes = build(&DOCS);
        let segment = Segment::open(&bytes).expect("opens");
        let index = Reader::open(&segment).expect("opens");
        let hits = Searcher::new(&index).search("fox", 10).expect("searches");
        // Document five is nothing but the word, document one has it twice in a
        // longer text, document zero has it once.
        assert_eq!(
            hits.iter().map(|hit| hit.doc).collect::<Vec<_>>(),
            [5, 1, 0]
        );
    }

    #[test]
    fn a_document_holding_more_of_the_query_beats_one_holding_less() {
        let bytes = build(&DOCS);
        let segment = Segment::open(&bytes).expect("opens");
        let index = Reader::open(&segment).expect("opens");
        let hits = Searcher::new(&index)
            .search("quick brown fox", 10)
            .expect("searches");
        // Document two is four words long and three of them are the query,
        // which beats document zero holding all three terms once each in nine
        // words. That is length normalisation doing what it is there for.
        assert_eq!(hits[0].doc, 2);
        assert_eq!(hits[1].doc, 0);
        assert!(!hits.iter().any(|hit| hit.doc == 4));
    }

    #[test]
    fn a_total_counts_the_union_and_not_the_sum() {
        // Two terms that share a document. Adding the list lengths would say
        // four, and four is wrong.
        let bytes = build(&["alpha", "beta", "alpha beta", "gamma"]);
        let segment = Segment::open(&bytes).expect("opens");
        let index = Reader::open(&segment).expect("opens");
        let searcher = Searcher::new(&index);
        assert_eq!(searcher.count("alpha").expect("counts"), 2);
        assert_eq!(searcher.count("alpha beta").expect("counts"), 3);
        assert_eq!(searcher.count("alpha beta gamma").expect("counts"), 4);
        assert_eq!(searcher.count("nothing here").expect("counts"), 0);
    }

    #[test]
    fn a_total_agrees_with_counting_the_hits_by_hand() {
        // The page is pruned and the total is not, so the two are different
        // walks over the same lists. Asking for every document back is what
        // checks they agree.
        let bytes = build(&DOCS);
        let segment = Segment::open(&bytes).expect("opens");
        let index = Reader::open(&segment).expect("opens");
        let searcher = Searcher::new(&index);
        for query in ["fox", "quick dog", "lazy fox brown", "absent"] {
            let hits = searcher.search(query, DOCS.len()).expect("searches");
            assert_eq!(
                searcher.count(query).expect("counts"),
                hits.len() as u64,
                "{query}"
            );
        }
    }

    #[test]
    fn skipping_gives_the_same_answer_as_scoring_everything() {
        // The whole point of the pruning is that it changes nothing but the
        // time, so this is the test the module exists to pass.
        let bytes = build(&DOCS);
        let segment = Segment::open(&bytes).expect("opens");
        let index = Reader::open(&segment).expect("opens");
        for query in [
            "fox",
            "the",
            "quick brown fox",
            "the dog and the fox",
            "lazy",
            "nothing quick",
        ] {
            let mut analyzer = Analyzer::new();
            let mut words: Vec<Vec<u8>> = Vec::new();
            analyzer.analyze(query, |term, _| words.push(term.to_vec()));
            words.sort_unstable();
            words.dedup();
            let terms: Vec<&[u8]> = words.iter().map(Vec::as_slice).collect();
            let slow = by_hand(&index, &terms, K1, B);
            let fast = Searcher::new(&index)
                .search_terms(&terms, slow.len().max(1))
                .expect("searches");
            same(&fast, &slow[..fast.len()]);
        }
    }

    #[test]
    fn pruning_agrees_with_scoring_everything_on_a_corpus_worth_pruning() {
        // Ten thousand documents, one term in all of them and a few that are
        // rare, which is the shape that makes the pivot move around.
        let docs: Vec<String> = (0..10_000)
            .map(|i| {
                let mut text = format!("common word{} filler filler", i % 500);
                if i % 97 == 0 {
                    text.push_str(" rare");
                }
                if i % 991 == 0 {
                    text.push_str(" rare rare rarer");
                }
                text
            })
            .collect();
        let refs: Vec<&str> = docs.iter().map(String::as_str).collect();
        let bytes = build(&refs);
        let segment = Segment::open(&bytes).expect("opens");
        let index = Reader::open(&segment).expect("opens");

        for query in ["rare", "common rare", "rarer common", "word7 rare common"] {
            let mut analyzer = Analyzer::new();
            let mut words: Vec<Vec<u8>> = Vec::new();
            analyzer.analyze(query, |term, _| words.push(term.to_vec()));
            words.sort_unstable();
            words.dedup();
            let terms: Vec<&[u8]> = words.iter().map(Vec::as_slice).collect();
            let slow = by_hand(&index, &terms, K1, B);
            for k in [1, 5, 10, 100] {
                let fast = Searcher::new(&index)
                    .search_terms(&terms, k)
                    .expect("searches");
                let want = k.min(slow.len());
                assert_eq!(fast.len(), want, "{query} at k {k}");
                same(&fast, &slow[..want]);
            }
        }
    }

    #[test]
    fn one_pass_gives_the_page_and_the_total_that_two_passes_give() {
        // The one pass is only worth having if it is the same answer, so this
        // runs it against the two calls it replaces on a corpus with a term in
        // every document, a term in a few, and every query shape in between.
        let docs: Vec<String> = (0..5_000)
            .map(|i| {
                let mut text = format!("common word{} filler", i % 300);
                if i % 61 == 0 {
                    text.push_str(" rare rare");
                }
                if i % 907 == 0 {
                    text.push_str(" rarer");
                }
                text
            })
            .collect();
        let refs: Vec<&str> = docs.iter().map(String::as_str).collect();
        let bytes = build(&refs);
        let segment = Segment::open(&bytes).expect("opens");
        let index = Reader::open(&segment).expect("opens");
        let searcher = Searcher::new(&index);

        for query in [
            "rare",
            "common",
            "common rare",
            "rarer rare common",
            "word7 common",
            "word7 aardvark",
            "aardvark",
        ] {
            for k in [1, 10, 100] {
                let (hits, total) = searcher.search_and_count(query, k).expect("searches");
                same(&hits, &searcher.search(query, k).expect("searches"));
                assert_eq!(total, searcher.count(query).expect("counts"), "{query}");
            }
        }
    }

    #[test]
    fn a_block_no_other_term_reaches_into_is_still_counted_document_by_document() {
        // The walk takes a whole block in one step when no other list reaches
        // into it, which is the case this corpus is built to produce: one term
        // in every document and a second term clustered at each end, so most of
        // the common term's blocks belong to it alone. The total has to be the
        // same total either way, and the page has to be the same page.
        let docs: Vec<String> = (0..4_000)
            .map(|i| {
                let mut text = String::from("common filler filler filler");
                if !(40..=3_950).contains(&i) {
                    text.push_str(" cluster cluster");
                }
                if i == 2_000 {
                    text.push_str(" lonely");
                }
                text
            })
            .collect();
        let refs: Vec<&str> = docs.iter().map(String::as_str).collect();
        let bytes = build(&refs);
        let segment = Segment::open(&bytes).expect("opens");
        let index = Reader::open(&segment).expect("opens");
        let searcher = Searcher::new(&index);

        assert_eq!(searcher.count("common cluster").expect("counts"), 4_000);
        assert_eq!(searcher.count("common lonely").expect("counts"), 4_000);
        assert_eq!(searcher.count("cluster lonely").expect("counts"), 90);
        for query in ["common cluster", "common lonely", "cluster lonely"] {
            for k in [1, 10, 200] {
                let (hits, total) = searcher.search_and_count(query, k).expect("searches");
                same(&hits, &searcher.search(query, k).expect("searches"));
                assert_eq!(
                    total,
                    searcher.count(query).expect("counts"),
                    "{query} at k {k}"
                );
            }
        }
    }

    /// A corpus shaped like the one the engine is slow on.
    ///
    /// One term in every document at a frequency that varies, which is what a
    /// stop word looks like, and one term in the first two percent of the
    /// corpus. Where the rare term sits matters: spread evenly it would land in
    /// every block of the common term's list and there would be nothing to skip
    /// however good the bound was, so a test that wants to see skipping has to
    /// put it somewhere.
    fn skewed(documents: usize) -> Vec<String> {
        (0..documents)
            .map(|i| {
                let repeats = 1 + i % 40;
                let mut text = "common ".repeat(repeats);
                for word in 0..8 {
                    use core::fmt::Write as _;
                    let _ = write!(text, "word{} ", (i + word) % 500);
                }
                if i < documents / 50 {
                    text.push_str("rare ");
                }
                text
            })
            .collect()
    }

    #[test]
    fn counting_does_not_change_the_answer() {
        let bytes = build(&DOCS);
        let segment = Segment::open(&bytes).expect("opens");
        let index = Reader::open(&segment).expect("opens");
        let searcher = Searcher::new(&index);
        for query in ["fox", "the dog fox", "quick brown", "absent"] {
            let plain = searcher.search(query, 5).expect("searches");
            let (explained, _) = searcher.search_explained(query, 5).expect("searches");
            assert_eq!(plain, explained, "{query}");
        }
    }

    #[test]
    fn the_counters_describe_the_lists_the_query_opened() {
        let docs = skewed(6_000);
        let refs: Vec<&str> = docs.iter().map(String::as_str).collect();
        let bytes = build(&refs);
        let segment = Segment::open(&bytes).expect("opens");
        let index = Reader::open(&segment).expect("opens");
        let searcher = Searcher::new(&index);

        let (_, counters) = searcher.search_explained("common", 10).expect("searches");
        assert_eq!(counters.terms, 1);
        assert_eq!(counters.postings, 6_000);
        // Forty six full blocks and the leftovers.
        assert_eq!(counters.blocks, 6_000 / BLOCK_SIZE as u64 + 1);
        // What the skipped count is subtracted from, so it is worth checking on
        // a real walk rather than assuming. A cursor that loaded a block twice
        // would break the subtraction quietly and this is where that shows up.
        assert!(
            counters.blocks_decoded <= counters.blocks,
            "read {} blocks of {}",
            counters.blocks_decoded,
            counters.blocks
        );
        assert_eq!(counters.advances + counters.seeks, 6_000);
    }

    #[test]
    fn a_rare_term_beside_a_common_one_skips_most_of_the_common_one() {
        // This is the pruning working, and it is the case the block bound was
        // written for: the rare term's documents are the only candidates, so
        // most of the common term's blocks never get unpacked.
        let docs = skewed(6_000);
        let refs: Vec<&str> = docs.iter().map(String::as_str).collect();
        let bytes = build(&refs);
        let segment = Segment::open(&bytes).expect("opens");
        let index = Reader::open(&segment).expect("opens");
        let searcher = Searcher::new(&index);

        let (_, counters) = searcher
            .search_explained("rare common", 10)
            .expect("searches");
        assert!(
            counters.skipped() > 0.5,
            "read {} of {} postings",
            counters.postings_decoded,
            counters.postings
        );
    }

    #[test]
    fn a_common_term_on_its_own_skips_nothing_at_all() {
        // Not an assertion that this is right. It is an assertion of what the
        // engine does today, which is decode every posting of a term that is in
        // every document, because the block bound is computed from the largest
        // frequency in the block with the length normalisation pinned to zero
        // and is therefore too loose to ever fall under the threshold.
        //
        // The fix changes this test, and the test is here so that the fix has to
        // change it rather than being believed.
        let docs = skewed(6_000);
        let refs: Vec<&str> = docs.iter().map(String::as_str).collect();
        let bytes = build(&refs);
        let segment = Segment::open(&bytes).expect("opens");
        let index = Reader::open(&segment).expect("opens");
        let searcher = Searcher::new(&index);

        let (_, counters) = searcher.search_explained("common", 10).expect("searches");
        assert_eq!(
            counters.blocks_skipped, 0,
            "skipped {} of {} blocks, which would mean the bound now fires",
            counters.blocks_skipped, counters.blocks
        );
        assert_eq!(counters.documents_scored, 6_000);
    }

    #[test]
    fn every_block_is_either_read_or_stepped_over() {
        // Blocks skipped is worked out by subtraction, so this is the identity
        // that makes the number mean anything. It has to hold for a query that
        // prunes, a query that cannot, a query over a term nobody has, and both
        // walks, because a walk that decoded a block twice or lost one would
        // show up here and nowhere else.
        let docs = skewed(6_000);
        let refs: Vec<&str> = docs.iter().map(String::as_str).collect();
        let bytes = build(&refs);
        let segment = Segment::open(&bytes).expect("opens");
        let index = Reader::open(&segment).expect("opens");
        let searcher = Searcher::new(&index);

        for query in [
            "common",
            "rare",
            "rare common",
            "word7 common rare",
            "word7",
            "aardvark",
            "aardvark common",
        ] {
            for k in [1, 10, 1_000] {
                let (_, counters) = searcher.search_explained(query, k).expect("searches");
                assert_eq!(
                    counters.blocks_decoded + counters.blocks_skipped,
                    counters.blocks,
                    "{query} at k {k}"
                );
                assert!(
                    counters.blocks_decoded <= counters.blocks,
                    "{query} at k {k} read more blocks than the lists hold"
                );

                let (_, _, counters) = searcher
                    .search_and_count_explained(query, k)
                    .expect("searches");
                assert_eq!(
                    counters.blocks_decoded + counters.blocks_skipped,
                    counters.blocks,
                    "{query} at k {k}, page and total"
                );
            }

            let (_, counters) = searcher.count_explained(query).expect("counts");
            assert_eq!(
                counters.blocks_decoded + counters.blocks_skipped,
                counters.blocks,
                "{query}, total only"
            );
        }
    }

    #[test]
    fn asking_for_nothing_returns_nothing() {
        let bytes = build(&DOCS);
        let segment = Segment::open(&bytes).expect("opens");
        let index = Reader::open(&segment).expect("opens");
        let searcher = Searcher::new(&index);
        assert!(searcher.search("fox", 0).expect("searches").is_empty());
        assert!(searcher.search("", 10).expect("searches").is_empty());
        assert!(
            searcher
                .search("aardvark", 10)
                .expect("searches")
                .is_empty()
        );
    }

    #[test]
    fn a_term_that_is_not_in_the_index_does_not_stop_the_others() {
        let bytes = build(&DOCS);
        let segment = Segment::open(&bytes).expect("opens");
        let index = Reader::open(&segment).expect("opens");
        let hits = Searcher::new(&index)
            .search("aardvark fox", 10)
            .expect("searches");
        assert_eq!(
            hits.iter().map(|hit| hit.doc).collect::<Vec<_>>(),
            [5, 1, 0]
        );
    }

    #[test]
    fn searching_an_empty_index_returns_nothing() {
        let bytes = build(&[]);
        let segment = Segment::open(&bytes).expect("opens");
        let index = Reader::open(&segment).expect("opens");
        assert!(
            Searcher::new(&index)
                .search("anything", 10)
                .expect("searches")
                .is_empty()
        );
    }

    #[test]
    fn a_repeated_query_term_counts_once() {
        let bytes = build(&DOCS);
        let segment = Segment::open(&bytes).expect("opens");
        let index = Reader::open(&segment).expect("opens");
        let searcher = Searcher::new(&index);
        let once = searcher.search("fox", 10).expect("searches");
        let thrice = searcher.search("fox fox fox", 10).expect("searches");
        same(&thrice, &once);
    }

    /// A corpus with the shape real prose has: a handful of words in nearly
    /// every document and most words in almost none.
    ///
    /// The regular corpora above are too kind to the pruning. Every document is
    /// about the same length and every term is either everywhere or nowhere, so
    /// the walk meets the same decision over and over. Drawing the rank of each
    /// word log uniformly gets the long tail instead, and with it the case where
    /// several lists sit on the same document at once, which is where the walk
    /// has to be careful.
    fn heavy_tailed(documents: usize) -> Vec<String> {
        use core::fmt::Write as _;

        let mut state = 0x2545_f491_4f6c_dd1d_u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let vocabulary = 3_000.0_f64;
        let mut out = Vec::with_capacity(documents);
        for _ in 0..documents {
            let mut text = String::with_capacity(256);
            let length = 20 + (next() % 60) as usize;
            for _ in 0..length {
                #[expect(
                    clippy::cast_precision_loss,
                    reason = "a fraction of the way through a 64 bit range, which \
                              only has to be spread out rather than exact"
                )]
                let unit = (next() >> 11) as f64 / (1_u64 << 53) as f64;
                #[expect(
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss,
                    reason = "the exponential is bounded by the vocabulary size, \
                              which is three thousand"
                )]
                let rank = vocabulary.powf(unit) as usize;
                let _ = write!(text, "word{rank} ");
            }
            out.push(text);
        }
        out
    }

    #[test]
    fn pruning_keeps_every_document_that_belongs_on_the_page() {
        // A corpus shaped like prose, where several of a query's terms land on
        // the same document often enough to matter. The walk decides whether a
        // document is worth scoring from what the terms on it could add up to,
        // and a walk that leaves one of those terms out of the sum can talk
        // itself out of a document that belonged at the top of the page.
        let docs = heavy_tailed(20_000);
        let refs: Vec<&str> = docs.iter().map(String::as_str).collect();
        let bytes = build(&refs);
        let segment = Segment::open(&bytes).expect("opens");
        let index = Reader::open(&segment).expect("opens");
        let searcher = Searcher::new(&index);

        for query in [
            "word1 word7 word2",
            "word3 word40 word1",
            "word2 word5 word900",
            "word1 word2 word3",
            "word11 word12 word13",
            "word1 word2",
        ] {
            let mut analyzer = Analyzer::new();
            let mut words: Vec<Vec<u8>> = Vec::new();
            analyzer.analyze(query, |term, _| words.push(term.to_vec()));
            words.sort_unstable();
            words.dedup();
            let terms: Vec<&[u8]> = words.iter().map(Vec::as_slice).collect();
            let slow = by_hand(&index, &terms, K1, B);
            for k in [1, 10, 100] {
                let want = k.min(slow.len());
                let fast = searcher.search_terms(&terms, k).expect("searches");
                assert_eq!(fast.len(), want, "{query} at k {k}");
                same(&fast, &slow[..want]);
                // The other walk over the same lists, which prunes by a
                // different argument and has to reach the same page.
                let (page, _) = searcher
                    .search_and_count_terms(&terms, k)
                    .expect("searches");
                same(&page, &slow[..want]);
            }
        }
    }

    /// Splits a corpus into `parts` segments, in order, so that a document's
    /// place in the searcher's numbering is the place it had in the corpus.
    fn split(docs: &[&str], parts: usize) -> Vec<Vec<u8>> {
        let per = docs.len().div_ceil(parts);
        docs.chunks(per).map(build).collect()
    }

    #[test]
    fn eight_segments_rank_the_same_as_one() {
        // The one that matters. A store writes a new segment every time it
        // flushes, so the same corpus is one segment on Monday and eight on
        // Friday, and a page of results that changed on the way would mean the
        // engine cannot be trusted to have said anything.
        let docs = skewed(6_000);
        let refs: Vec<&str> = docs.iter().map(String::as_str).collect();

        let whole = build(&refs);
        let one = Segment::open(&whole).expect("opens");
        let one = Reader::open(&one).expect("opens");
        let one = Searcher::new(&one);

        let parts = split(&refs, 8);
        let segments: Vec<Segment<'_>> = parts
            .iter()
            .map(|bytes| Segment::open(bytes).expect("opens"))
            .collect();
        let readers: Vec<Reader<'_>> = segments
            .iter()
            .map(|segment| Reader::open(segment).expect("opens"))
            .collect();
        let many = Searcher::over(&readers).expect("six thousand documents are numberable");

        assert_eq!(many.segments().len(), 8);
        assert_eq!(many.documents(), one.documents());
        assert!((many.average_length() - one.average_length()).abs() < 1e-3);

        for query in [
            "common",
            "rare",
            "rare common",
            "word7",
            "word7 common rare",
            "aardvark",
            "aardvark common",
        ] {
            for k in [1, 10, 200] {
                same(
                    &many.search(query, k).expect("searches"),
                    &one.search(query, k).expect("searches"),
                );
                assert_eq!(
                    many.count(query).expect("counts"),
                    one.count(query).expect("counts"),
                    "{query}"
                );
                let (hits, total) = many.search_and_count(query, k).expect("searches");
                same(&hits, &one.search(query, k).expect("searches"));
                assert_eq!(total, one.count(query).expect("counts"), "{query} at k {k}");
            }
        }
    }

    #[test]
    fn a_term_weighs_the_same_wherever_the_document_holding_it_sits() {
        // Two identical documents, one in each segment, with a term that is
        // common in the first segment and absent from the second. Weighed per
        // segment they would score differently, and the copy in the segment that
        // had not seen the word before would win a page it has no business
        // winning.
        let first = build(&["alpha beta", "alpha", "alpha", "alpha"]);
        let second = build(&["alpha beta", "gamma", "gamma", "gamma"]);
        let segments = [
            Segment::open(&first).expect("opens"),
            Segment::open(&second).expect("opens"),
        ];
        let readers: Vec<Reader<'_>> = segments
            .iter()
            .map(|segment| Reader::open(segment).expect("opens"))
            .collect();
        let searcher = Searcher::over(&readers).expect("eight documents are numberable");

        let hits = searcher.search("alpha beta", 10).expect("searches");
        let twins: Vec<&Hit> = hits
            .iter()
            .filter(|hit| hit.doc == 0 || hit.doc == 4)
            .collect();
        assert_eq!(twins.len(), 2, "both copies are hits");
        assert!(
            (twins[0].score - twins[1].score).abs() < 1e-6,
            "the same document scored {} in one segment and {} in the other",
            twins[0].score,
            twins[1].score
        );
    }

    #[test]
    fn a_hit_says_which_segment_it_came_from() {
        let parts = split(&DOCS, 3);
        let segments: Vec<Segment<'_>> = parts
            .iter()
            .map(|bytes| Segment::open(bytes).expect("opens"))
            .collect();
        let readers: Vec<Reader<'_>> = segments
            .iter()
            .map(|segment| Reader::open(segment).expect("opens"))
            .collect();
        let searcher = Searcher::over(&readers).expect("six documents are numberable");

        // Two documents per segment, so the numbering runs 0 and 1 in the first,
        // 2 and 3 in the second, and 4 and 5 in the third.
        assert_eq!(searcher.locate(0), Some((0, 0)));
        assert_eq!(searcher.locate(3), Some((1, 1)));
        assert_eq!(searcher.locate(5), Some((2, 1)));
        assert_eq!(searcher.locate(6), None);

        let hits = searcher.search("fox", 10).expect("searches");
        assert_eq!(
            hits.iter().map(|hit| hit.doc).collect::<Vec<_>>(),
            [5, 1, 0]
        );
        let (segment, doc) = searcher.locate(hits[0].doc).expect("a hit is somewhere");
        assert_eq!((segment, doc), (2, 1));
        assert_eq!(
            readers[segment].length(doc),
            1,
            "the document that is one word"
        );
    }

    #[test]
    fn a_segment_holding_none_of_the_query_does_not_stop_the_others() {
        let parts = [
            build(&["nothing to do with any of it", "or with this"]),
            build(&["the quick brown fox", "nor this"]),
        ];
        let segments: Vec<Segment<'_>> = parts
            .iter()
            .map(|bytes| Segment::open(bytes).expect("opens"))
            .collect();
        let readers: Vec<Reader<'_>> = segments
            .iter()
            .map(|segment| Reader::open(segment).expect("opens"))
            .collect();
        let searcher = Searcher::over(&readers).expect("four documents are numberable");
        let hits = searcher.search("fox", 10).expect("searches");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].doc, 2);
    }

    #[test]
    fn a_term_in_every_segment_is_still_one_term() {
        // The counters describe the query rather than the walk's internal
        // arrangements, so a word held by eight segments is one term that was
        // opened, and the postings are all of them wherever they live.
        let docs = skewed(6_000);
        let refs: Vec<&str> = docs.iter().map(String::as_str).collect();
        let parts = split(&refs, 8);
        let segments: Vec<Segment<'_>> = parts
            .iter()
            .map(|bytes| Segment::open(bytes).expect("opens"))
            .collect();
        let readers: Vec<Reader<'_>> = segments
            .iter()
            .map(|segment| Reader::open(segment).expect("opens"))
            .collect();
        let searcher = Searcher::over(&readers).expect("six thousand documents are numberable");

        let (_, counters) = searcher.search_explained("common", 10).expect("searches");
        assert_eq!(counters.terms, 1);
        assert_eq!(counters.postings, 6_000);
        // Eight segments of seven hundred and fifty, which is five whole blocks
        // and a remainder each.
        assert_eq!(counters.blocks, 8 * (750 / BLOCK_SIZE as u64 + 1));
        assert_eq!(
            counters.blocks_decoded + counters.blocks_skipped,
            counters.blocks
        );

        let (_, counters) = searcher
            .search_explained("rare common aardvark", 10)
            .expect("searches");
        assert_eq!(counters.terms, 2, "aardvark is in none of them");
    }

    #[test]
    fn searching_no_segments_at_all_returns_nothing() {
        let searcher = Searcher::over(&[]).expect("nothing is numberable");
        assert_eq!(searcher.documents(), 0);
        assert!(searcher.average_length().abs() < 1e-6);
        assert!(
            searcher
                .search("anything", 10)
                .expect("searches")
                .is_empty()
        );
        assert_eq!(searcher.count("anything").expect("counts"), 0);
        assert_eq!(searcher.locate(0), None);
    }

    #[test]
    fn the_same_query_twice_gives_the_same_answer() {
        let bytes = build(&DOCS);
        let segment = Segment::open(&bytes).expect("opens");
        let index = Reader::open(&segment).expect("opens");
        let searcher = Searcher::new(&index);
        let first = searcher.search("the dog fox", 3).expect("searches");
        let second = searcher.search("the dog fox", 3).expect("searches");
        assert_eq!(first, second);
    }
}
