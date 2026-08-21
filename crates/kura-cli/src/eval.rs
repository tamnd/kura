//! Relevance, measured the way everybody else measures it.
//!
//! A ranking change that nobody measured is a ranking change nobody can defend.
//! Every change to the scorer so far has been argued from a plausible story
//! about what should rank higher, which is exactly the kind of argument that
//! cannot be lost. This module is how those arguments get settled.
//!
//! # `trec_eval` semantics
//!
//! The numbers here have to be comparable with the numbers other people publish,
//! and that means matching `trec_eval` rather than matching the definition in a
//! textbook. The places the two differ are the places this module is opinionated.
//!
//! The rank column in a run file is ignored. Results are sorted by score,
//! descending, and ties are broken by document identifier in reverse
//! lexicographic order. That tie break looks arbitrary because it is, but it is
//! the arbitrary rule `trec_eval` uses, and a run with tied scores scores
//! differently under any other one.
//!
//! A retrieved document with no judgment is not relevant. A judgment below one
//! is not relevant, which covers both the zeroes and the negative values some
//! collections use for documents that were looked at and rejected.
//!
//! Gain is linear. nDCG@10 uses the relevance level itself as the gain, with a
//! discount of one over the log base two of one plus the rank. The exponential
//! gain that some papers use gives higher numbers on the same run, which is a
//! good reason to be explicit about which one is being reported.
//!
//! A query whose judgments contain nothing relevant scores zero rather than
//! being skipped, and a query in the judgments that the run never answered is
//! only counted when [`Coverage::Complete`] is asked for. Both of those match
//! `trec_eval` with and without its `-c` flag, and the difference between them
//! on a run that timed out on a few queries is large.

use std::collections::HashMap;
use std::collections::hash_map::Entry;

/// Which queries an evaluation covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Coverage {
    /// Only the queries the run answered, which is `trec_eval` by default.
    Answered,
    /// Every query in the judgments, scoring a missing one as zero.
    ///
    /// `trec_eval -c`. The honest choice when comparing engines, because an
    /// engine that returns nothing for its hard queries should not score as
    /// though those queries were not asked.
    Complete,
}

/// The judgments: which documents are relevant to which query, and how much.
#[derive(Debug, Default)]
pub struct Qrels {
    by_query: HashMap<String, HashMap<String, i32>>,
}

impl Qrels {
    /// Reads a qrels file.
    ///
    /// Four whitespace separated columns per line: query, iteration, document,
    /// relevance. The iteration column has meant nothing for thirty years and is
    /// ignored here as it is everywhere else.
    ///
    /// # Errors
    ///
    /// Returns the line number and what was wrong with it.
    pub fn parse(text: &str) -> Result<Self, Bad> {
        let mut by_query: HashMap<String, HashMap<String, i32>> = HashMap::new();
        for (at, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let mut fields = line.split_whitespace();
            let (Some(query), Some(_iteration), Some(doc), Some(relevance)) =
                (fields.next(), fields.next(), fields.next(), fields.next())
            else {
                return Err(Bad::new(at + 1, "wanted four columns"));
            };
            let relevance: i32 = relevance
                .parse()
                .map_err(|_| Bad::new(at + 1, format!("relevance {relevance} is not a number")))?;
            by_query
                .entry(query.to_string())
                .or_default()
                .insert(doc.to_string(), relevance);
        }
        Ok(Self { by_query })
    }

    /// How many queries have judgments.
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_query.len()
    }

    /// Whether there are no judgments at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_query.is_empty()
    }

    /// The relevance of a document to a query, which is zero when nobody judged
    /// it.
    fn relevance(&self, query: &str, doc: &str) -> i32 {
        self.by_query
            .get(query)
            .and_then(|judged| judged.get(doc))
            .copied()
            .unwrap_or(0)
    }

    /// How many documents are relevant to a query.
    fn relevant(&self, query: &str) -> usize {
        self.by_query
            .get(query)
            .map_or(0, |judged| judged.values().filter(|r| **r > 0).count())
    }

    /// Every relevance level for a query, largest first, which is the ranking a
    /// perfect engine would have returned.
    fn ideal(&self, query: &str) -> Vec<i32> {
        let mut levels: Vec<i32> = self
            .by_query
            .get(query)
            .map(|judged| judged.values().copied().filter(|r| *r > 0).collect())
            .unwrap_or_default();
        levels.sort_unstable_by(|a, b| b.cmp(a));
        levels
    }
}

/// What an engine returned, for every query it answered.
#[derive(Debug, Default)]
pub struct Run {
    by_query: HashMap<String, Vec<Ranked>>,
}

/// One line of a run: a document and the score that put it where it is.
#[derive(Debug)]
struct Ranked {
    doc: String,
    score: f64,
}

impl Run {
    /// Reads a run file.
    ///
    /// Six whitespace separated columns per line: query, iteration, document,
    /// rank, score, tag. The rank and the tag are read and ignored, because the
    /// score is what decides the order and the tag names the run rather than
    /// describing it.
    ///
    /// # Errors
    ///
    /// Returns the line number and what was wrong with it. A document that
    /// appears twice for one query is an error rather than a warning, because
    /// every measure below would silently count it twice.
    pub fn parse(text: &str) -> Result<Self, Bad> {
        let mut by_query: HashMap<String, Vec<Ranked>> = HashMap::new();
        let mut seen: HashMap<(String, String), usize> = HashMap::new();
        for (at, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let mut fields = line.split_whitespace();
            let (Some(query), Some(_iteration), Some(doc), Some(_rank), Some(score)) = (
                fields.next(),
                fields.next(),
                fields.next(),
                fields.next(),
                fields.next(),
            ) else {
                return Err(Bad::new(at + 1, "wanted at least five columns"));
            };
            let score: f64 = score
                .parse()
                .map_err(|_| Bad::new(at + 1, format!("score {score} is not a number")))?;
            if !score.is_finite() {
                return Err(Bad::new(at + 1, format!("score {score} is not a number")));
            }
            match seen.entry((query.to_string(), doc.to_string())) {
                Entry::Occupied(first) => {
                    return Err(Bad::new(
                        at + 1,
                        format!(
                            "{doc} is already ranked for {query} on line {}",
                            first.get()
                        ),
                    ));
                }
                Entry::Vacant(slot) => slot.insert(at + 1),
            };
            by_query.entry(query.to_string()).or_default().push(Ranked {
                doc: doc.to_string(),
                score,
            });
        }

        // The rank column is not trusted, so the order has to be established
        // here. Descending by score, and ties broken by document identifier in
        // reverse lexicographic order, which is what trec_eval does.
        for ranked in by_query.values_mut() {
            ranked.sort_by(|a, b| {
                b.score
                    .partial_cmp(&a.score)
                    .expect("scores were checked for being numbers while parsing")
                    .then_with(|| b.doc.cmp(&a.doc))
            });
        }
        Ok(Self { by_query })
    }

    /// How many queries the run answered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_query.len()
    }

    /// Whether the run answered nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_query.is_empty()
    }
}

/// What a run scored.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Scores {
    /// How many queries went into the averages.
    pub queries: usize,
    /// Normalised discounted cumulative gain over the first ten results.
    pub ndcg_10: f64,
    /// The fraction of the relevant documents found in the first hundred.
    pub recall_100: f64,
    /// One over the rank of the first relevant result, zero past ten.
    pub mrr_10: f64,
}

/// Scores every query separately, in query order.
///
/// The averages hide the shape of a result. A run that scores 0.4 because every
/// query scored 0.4 and a run that scores 0.4 because half the queries scored
/// 0.8 and half scored nothing are different engines, and only this tells them
/// apart.
#[must_use]
pub fn each<'a>(run: &'a Run, qrels: &'a Qrels, coverage: Coverage) -> Vec<(&'a str, Scores)> {
    let mut queries: Vec<&str> = match coverage {
        Coverage::Answered => run
            .by_query
            .keys()
            .filter(|query| qrels.by_query.contains_key(*query))
            .map(String::as_str)
            .collect(),
        Coverage::Complete => qrels.by_query.keys().map(String::as_str).collect(),
    };
    // Sorted so a per query report comes out in a stable order rather than in
    // whatever order the hashing happened to produce.
    queries.sort_unstable();
    queries
        .into_iter()
        .map(|query| (query, one_query(run, qrels, query)))
        .collect()
}

/// Scores a run against the judgments.
///
/// The averages are arithmetic means over queries, so every query counts the
/// same however many documents were judged for it.
#[must_use]
#[expect(
    clippy::cast_precision_loss,
    reason = "a divisor that is a count of queries, which is thousands rather than \
              anything an f64 loses a digit of"
)]
pub fn score(run: &Run, qrels: &Qrels, coverage: Coverage) -> Scores {
    let each = each(run, qrels, coverage);
    if each.is_empty() {
        return Scores::default();
    }
    let mut total = Scores {
        queries: each.len(),
        ..Scores::default()
    };
    for (_, one) in &each {
        total.ndcg_10 += one.ndcg_10;
        total.recall_100 += one.recall_100;
        total.mrr_10 += one.mrr_10;
    }
    let count = each.len() as f64;
    total.ndcg_10 /= count;
    total.recall_100 /= count;
    total.mrr_10 /= count;
    total
}

/// Every measure for one query, which is where the definitions actually live.
#[expect(
    clippy::cast_precision_loss,
    reason = "ranks are at most a few thousand and counts of judged documents are \
              smaller than that, so nothing here is near the limit of an f64"
)]
fn one_query(run: &Run, qrels: &Qrels, query: &str) -> Scores {
    let empty: Vec<Ranked> = Vec::new();
    let ranked = run.by_query.get(query).unwrap_or(&empty);

    let mut dcg = 0.0;
    for (at, hit) in ranked.iter().take(10).enumerate() {
        let gain = f64::from(qrels.relevance(query, &hit.doc).max(0));
        dcg += gain / discount(at);
    }
    let mut ideal = 0.0;
    for (at, level) in qrels.ideal(query).into_iter().take(10).enumerate() {
        ideal += f64::from(level) / discount(at);
    }
    // A query with nothing relevant in its judgments has no ideal ranking to be
    // compared against, and scores zero rather than one. Dividing zero by zero
    // and calling the answer perfect would be the other choice.
    let ndcg_10 = if ideal > 0.0 { dcg / ideal } else { 0.0 };

    let relevant = qrels.relevant(query);
    let found = ranked
        .iter()
        .take(100)
        .filter(|hit| qrels.relevance(query, &hit.doc) > 0)
        .count();
    let recall_100 = if relevant > 0 {
        found as f64 / relevant as f64
    } else {
        0.0
    };

    let mrr_10 = ranked
        .iter()
        .take(10)
        .position(|hit| qrels.relevance(query, &hit.doc) > 0)
        .map_or(0.0, |at| 1.0 / (at + 1) as f64);

    Scores {
        queries: 1,
        ndcg_10,
        recall_100,
        mrr_10,
    }
}

/// The discount on the gain of the result at a zero based rank.
///
/// One over the log base two of one plus the one based rank, so the first result
/// is discounted by one and the second by log base two of three.
#[expect(
    clippy::cast_precision_loss,
    reason = "a rank, which is at most a few thousand"
)]
fn discount(at: usize) -> f64 {
    ((at + 2) as f64).log2()
}

/// A line that could not be read, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bad {
    /// The one based line number.
    pub line: usize,
    /// What was wrong with it.
    pub why: String,
}

impl Bad {
    fn new(line: usize, why: impl Into<String>) -> Self {
        Self {
            line,
            why: why.into(),
        }
    }
}

impl std::fmt::Display for Bad {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "line {}: {}", self.line, self.why)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two relevant documents for one query, one of them highly so.
    const QRELS: &str = "\
q1 0 a 3
q1 0 b 1
q1 0 c 0
q2 0 d 1
";

    fn qrels() -> Qrels {
        Qrels::parse(QRELS).expect("the judgments read")
    }

    fn run(text: &str) -> Run {
        Run::parse(text).expect("the run reads")
    }

    #[test]
    fn a_perfect_ranking_scores_one() {
        let run = run("q1 Q0 a 1 2.0 t\nq1 Q0 b 2 1.0 t\n");
        let scored = score(&run, &qrels(), Coverage::Answered);
        assert!((scored.ndcg_10 - 1.0).abs() < 1e-9, "{scored:?}");
        assert!((scored.recall_100 - 1.0).abs() < 1e-9, "{scored:?}");
        assert!((scored.mrr_10 - 1.0).abs() < 1e-9, "{scored:?}");
    }

    #[test]
    fn the_gain_is_linear_rather_than_exponential() {
        // Swapping the two relevant documents costs exactly what linear gain
        // says it costs. The ideal is 3 + 1/log2(3), the run is 1 + 3/log2(3),
        // and under exponential gain the same swap would cost more.
        let run = run("q1 Q0 b 1 2.0 t\nq1 Q0 a 2 1.0 t\n");
        let scored = score(&run, &qrels(), Coverage::Answered);
        let ideal = 3.0 + 1.0 / 3.0_f64.log2();
        let got = 1.0 + 3.0 / 3.0_f64.log2();
        assert!((scored.ndcg_10 - got / ideal).abs() < 1e-9, "{scored:?}");
    }

    #[test]
    fn an_unjudged_document_is_not_relevant() {
        let run = run("q1 Q0 zzz 1 9.0 t\nq1 Q0 a 2 1.0 t\n");
        let scored = score(&run, &qrels(), Coverage::Answered);
        assert!((scored.mrr_10 - 0.5).abs() < 1e-9, "{scored:?}");
    }

    #[test]
    fn a_judgment_below_one_is_not_relevant() {
        // `c` is judged and is not relevant, so it contributes nothing to any
        // measure and does not count towards recall's denominator either.
        let run = run("q1 Q0 c 1 9.0 t\nq1 Q0 b 2 1.0 t\n");
        let scored = score(&run, &qrels(), Coverage::Answered);
        assert!((scored.mrr_10 - 0.5).abs() < 1e-9, "{scored:?}");
        assert!((scored.recall_100 - 0.5).abs() < 1e-9, "{scored:?}");
    }

    #[test]
    fn the_rank_column_is_ignored_and_the_score_decides() {
        // The ranks say `b` is first. The scores say `a` is. trec_eval believes
        // the scores, so a run written out in the wrong order still scores what
        // its scores deserve.
        let run = run("q1 Q0 b 1 1.0 t\nq1 Q0 a 2 2.0 t\n");
        let scored = score(&run, &qrels(), Coverage::Answered);
        assert!((scored.ndcg_10 - 1.0).abs() < 1e-9, "{scored:?}");
    }

    #[test]
    fn a_tie_is_broken_by_document_identifier_descending() {
        // `a` and `b` have the same score. Reverse lexicographic order puts `b`
        // first, which costs the run the nDCG it would have had the other way.
        let run = run("q1 Q0 a 1 1.0 t\nq1 Q0 b 2 1.0 t\n");
        let scored = score(&run, &qrels(), Coverage::Answered);
        let ideal = 3.0 + 1.0 / 3.0_f64.log2();
        let got = 1.0 + 3.0 / 3.0_f64.log2();
        assert!((scored.ndcg_10 - got / ideal).abs() < 1e-9, "{scored:?}");
    }

    #[test]
    fn a_query_the_run_did_not_answer_only_counts_under_complete_coverage() {
        let run = run("q1 Q0 a 1 2.0 t\nq1 Q0 b 2 1.0 t\n");
        let answered = score(&run, &qrels(), Coverage::Answered);
        assert_eq!(answered.queries, 1);
        assert!((answered.ndcg_10 - 1.0).abs() < 1e-9);

        let complete = score(&run, &qrels(), Coverage::Complete);
        assert_eq!(complete.queries, 2);
        assert!((complete.ndcg_10 - 0.5).abs() < 1e-9, "{complete:?}");
    }

    #[test]
    fn a_query_the_judgments_do_not_cover_is_not_scored() {
        // Scoring a query nobody judged as zero would punish an engine for
        // answering a question the collection never asked.
        let run = run("q9 Q0 a 1 2.0 t\n");
        let scored = score(&run, &qrels(), Coverage::Answered);
        assert_eq!(scored.queries, 0);
        assert!(scored.ndcg_10.abs() < 1e-9, "{scored:?}");
    }

    #[test]
    fn a_relevant_document_past_ten_scores_no_reciprocal_rank() {
        use std::fmt::Write as _;

        let mut text = String::new();
        for at in 0..10 {
            let _ = writeln!(text, "q1 Q0 filler{at} {} {}.0 t", at + 1, 100 - at);
        }
        text.push_str("q1 Q0 a 11 1.0 t\n");
        let scored = score(&run(&text), &qrels(), Coverage::Answered);
        assert!(scored.mrr_10.abs() < 1e-9, "{scored:?}");
        // Recall reaches a hundred, so the document still counts there.
        assert!((scored.recall_100 - 0.5).abs() < 1e-9, "{scored:?}");
    }

    #[test]
    fn a_document_ranked_twice_for_one_query_is_an_error() {
        let bad = Run::parse("q1 Q0 a 1 2.0 t\nq1 Q0 a 2 1.0 t\n").expect_err("a duplicate");
        assert_eq!(bad.line, 2);
    }

    #[test]
    fn the_same_document_may_be_ranked_for_two_queries() {
        let run = Run::parse("q1 Q0 a 1 2.0 t\nq2 Q0 a 1 2.0 t\n").expect("not a duplicate");
        assert_eq!(run.len(), 2);
    }

    #[test]
    fn a_score_that_is_not_a_number_is_an_error_rather_than_a_silent_zero() {
        let bad = Run::parse("q1 Q0 a 1 nan t\n").expect_err("not a number");
        assert_eq!(bad.line, 1);
        let bad = Run::parse("q1 Q0 a 1 high t\n").expect_err("not a number");
        assert_eq!(bad.line, 1);
    }

    #[test]
    fn a_short_line_says_which_line_it_was() {
        let bad = Qrels::parse("q1 0 a 1\nq1 0 b\n").expect_err("three columns");
        assert_eq!(bad.line, 2);
    }

    #[test]
    fn blank_lines_are_not_an_error() {
        let qrels = Qrels::parse("\nq1 0 a 1\n\n").expect("blank lines are fine");
        assert_eq!(qrels.len(), 1);
    }

    #[test]
    fn recall_counts_what_was_found_against_what_there_was() {
        // One of the two relevant documents for q1, so a half, regardless of
        // where in the first hundred it landed.
        let run = run("q1 Q0 b 1 1.0 t\n");
        let scored = score(&run, &qrels(), Coverage::Answered);
        assert!((scored.recall_100 - 0.5).abs() < 1e-9, "{scored:?}");
    }

    #[test]
    fn a_query_with_nothing_relevant_scores_zero_rather_than_one() {
        let qrels = Qrels::parse("q1 0 a 0\n").expect("reads");
        let run = run("q1 Q0 a 1 1.0 t\n");
        let scored = score(&run, &qrels, Coverage::Answered);
        assert_eq!(scored.queries, 1);
        assert!(scored.ndcg_10.abs() < 1e-9, "{scored:?}");
        assert!(scored.recall_100.abs() < 1e-9, "{scored:?}");
    }
}
