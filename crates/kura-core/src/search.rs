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

use crate::DocId;
use crate::analysis::Analyzer;
use crate::error::Result;
use crate::index::Reader;
use crate::posting::Cursor;

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
    /// Which document.
    pub doc: DocId,
    /// What it scored. Higher is better, and the number is only comparable
    /// against other hits for the same query.
    pub score: f32,
}

/// Runs queries against an index.
#[derive(Debug)]
pub struct Searcher<'a, 'b> {
    index: &'a Reader<'b>,
    k1: f32,
    b: f32,
}

impl<'a, 'b> Searcher<'a, 'b> {
    /// A searcher with the usual BM25 parameters.
    #[must_use]
    pub const fn new(index: &'a Reader<'b>) -> Self {
        Self {
            index,
            k1: K1,
            b: B,
        }
    }

    /// A searcher with parameters of the caller's choosing.
    ///
    /// Worth tuning per corpus and not worth guessing at. Code is short and
    /// title fields want a different `b` from long prose.
    #[must_use]
    pub const fn with_parameters(index: &'a Reader<'b>, k1: f32, b: f32) -> Self {
        Self { index, k1, b }
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
        let mut lists = self.open(terms)?;
        // One term is the common case and its answer is already in the header
        // of its posting list, so nothing needs decoding at all.
        if lists.len() <= 1 {
            return Ok(lists.counts.first().map_or(0, |&count| u64::from(count)));
        }
        let mut total = 0;
        let mut doc = lists.front();
        while doc != DocId::MAX {
            total += 1;
            for at in 0..lists.len() {
                if lists.heads[at].doc == doc {
                    lists.advance(at)?;
                }
            }
            doc = lists.front();
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
        if k == 0 {
            return Ok((Vec::new(), self.count_terms(terms)?));
        }
        let mut lists = self.open(terms)?;
        if lists.is_empty() {
            return Ok((Vec::new(), 0));
        }
        if lists.len() == 1 {
            // One term, and its total is in the header of its own list. Nothing
            // has to be walked to know it, so the search can prune the way it
            // does when no total was asked for.
            let total = u64::from(lists.counts[0]);
            return Ok((self.search_terms(terms, k)?, total));
        }

        let average = self.index.average_length().max(1.0);
        let mut top = TopK::new(k);
        let mut total = 0u64;
        let mut doc = lists.front();
        while doc != DocId::MAX {
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
                let length = length_of(self.index, doc);
                let norm = self.k1 * (1.0 - self.b + self.b * length / average);
                let mut score = 0.0;
                for at in 0..lists.len() {
                    if lists.heads[at].doc == doc {
                        score += lists.score(at, self.k1, norm);
                    }
                }
                top.push(Hit { doc, score });
            }

            for at in 0..lists.len() {
                if lists.heads[at].doc == doc {
                    lists.advance(at)?;
                }
            }
            doc = lists.front();
        }
        Ok((top.into_sorted(), total))
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
        if k == 0 || self.index.is_empty() {
            return Ok(Vec::new());
        }
        let mut lists = self.open(terms)?;
        if lists.is_empty() {
            return Ok(Vec::new());
        }
        if lists.len() == 1 {
            return self.search_one(&mut lists, k);
        }

        let average = self.index.average_length().max(1.0);
        // The smallest the denominator of the tf factor can get, which is what
        // turns a frequency into an upper bound on a score.
        let floor = self.k1 * (1.0 - self.b);
        let mut top = TopK::new(k);
        let mut threshold = 0.0;
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
            let Some(pivot) = pivot(&lists, &order[..live], threshold) else {
                break;
            };
            let candidate = lists.heads[order[pivot]].doc;

            if lists.heads[order[0]].doc != candidate {
                // The lists before the pivot are behind it, and nothing between
                // where they are and the pivot can reach the threshold, so there
                // is no reason to decode any of it.
                for &at in &order[..pivot] {
                    lists.seek(at, candidate)?;
                }
                continue;
            }

            // Everything up to the pivot is on the candidate. Before paying for
            // the frequencies, ask what the blocks these cursors are sitting in
            // could possibly add up to.
            let ceiling: f32 = order[..=pivot]
                .iter()
                .map(|&at| lists.block_bound(at, self.k1, floor))
                .sum();
            if ceiling <= threshold {
                let next = order[..=pivot]
                    .iter()
                    .filter_map(|&at| lists.cursors[at].block_last())
                    .min()
                    .unwrap_or(candidate)
                    .saturating_add(1)
                    .max(candidate.saturating_add(1));
                for at in 0..lists.len() {
                    if lists.heads[at].doc < next {
                        lists.seek(at, next)?;
                    }
                }
                continue;
            }

            let length = length_of(self.index, candidate);
            let norm = self.k1 * (1.0 - self.b + self.b * length / average);
            let mut score = 0.0;
            let mut moved = 0;
            while moved < live && lists.heads[order[moved]].doc == candidate {
                score += lists.score(order[moved], self.k1, norm);
                moved += 1;
            }
            top.push(Hit {
                doc: candidate,
                score,
            });
            threshold = top.threshold();
            for &at in &order[..moved] {
                lists.advance(at)?;
            }
        }

        Ok(top.into_sorted())
    }

    /// Returns the best `k` documents for a query that came down to one list.
    ///
    /// One term is the commonest query there is and it has no pivot to find:
    /// every document in the list is a candidate and the only question is which
    /// of them score. What is left of the pruning is the block bound, which
    /// still steps over a whole block whose best posting cannot displace the
    /// worst hit in hand.
    fn search_one(&self, lists: &mut Lists<'b>, k: usize) -> Result<Vec<Hit>> {
        let average = self.index.average_length().max(1.0);
        let floor = self.k1 * (1.0 - self.b);
        let mut top = TopK::new(k);

        while lists.heads[0].doc != DocId::MAX {
            let doc = lists.heads[0].doc;
            if lists.block_bound(0, self.k1, floor) <= top.threshold() {
                let next = lists.cursors[0]
                    .block_last()
                    .unwrap_or(doc)
                    .saturating_add(1)
                    .max(doc.saturating_add(1));
                lists.seek(0, next)?;
                continue;
            }
            let length = length_of(self.index, doc);
            let norm = self.k1 * (1.0 - self.b + self.b * length / average);
            top.push(Hit {
                doc,
                score: lists.score(0, self.k1, norm),
            });
            lists.advance(0)?;
        }

        Ok(top.into_sorted())
    }

    /// Opens a cursor for each term that is in the index.
    fn open(&self, terms: &[&[u8]]) -> Result<Lists<'b>> {
        let documents = self.index.documents();
        let mut lists = Lists {
            heads: Vec::with_capacity(terms.len()),
            cursors: Vec::with_capacity(terms.len()),
            counts: Vec::with_capacity(terms.len()),
        };
        for term in terms {
            let Some(list) = self.index.postings(term)? else {
                continue;
            };
            if list.is_empty() {
                continue;
            }
            let idf = idf(documents, list.len());
            let mut cursor = list.cursor();
            let Some(doc) = cursor.advance()? else {
                continue;
            };
            lists.heads.push(Head {
                doc,
                idf,
                bound: idf * (self.k1 + 1.0),
            });
            lists.cursors.push(cursor);
            lists.counts.push(list.len());
        }
        Ok(lists)
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

    /// The lowest document any list is still on, or [`DocId::MAX`] when they
    /// are all spent.
    fn front(&self) -> DocId {
        self.heads
            .iter()
            .map(|head| head.doc)
            .min()
            .unwrap_or(DocId::MAX)
    }

    /// Moves one list to its next document.
    fn advance(&mut self, at: usize) -> Result<()> {
        self.heads[at].doc = self.cursors[at].advance()?.unwrap_or(DocId::MAX);
        Ok(())
    }

    /// Moves one list to the first document at or after `target`.
    fn seek(&mut self, at: usize, target: DocId) -> Result<()> {
        self.heads[at].doc = self.cursors[at].seek(target)?.unwrap_or(DocId::MAX);
        Ok(())
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
    fn by_hand(index: &Reader<'_>, terms: &[&[u8]], k1: f32, b: f32) -> Vec<Hit> {
        let average = index.average_length().max(1.0);
        let mut hits = Vec::new();
        for doc in 0..index.documents() {
            let mut score = 0.0;
            for term in terms {
                let Some(list) = index.postings(term).expect("decodes") else {
                    continue;
                };
                let postings = list.to_postings().expect("decodes");
                let Some((_, frequency)) = postings.iter().find(|(id, _)| *id == doc) else {
                    continue;
                };
                let f = *frequency as f32;
                let norm = k1 * (1.0 - b + b * index.length(doc) as f32 / average);
                score += idf(index.documents(), list.len()) * (f * (k1 + 1.0)) / (f + norm);
            }
            if score > 0.0 {
                hits.push(Hit { doc, score });
            }
        }
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
