//! Which segments to fold, and when.
//!
//! [`compact`](crate::compact) writes a replacement for some segments and
//! [`Store::compact`](crate::file::Store::compact) swaps it in. Neither of them
//! decides anything. This is the part that looks at a store and says which fold
//! is due, and it is a function of the manifest and a count of the deleted
//! documents in each segment: no file, no clock, no reader, so a policy question
//! can be asked of a store that is a hundred gigabytes without opening it and
//! can be tested by writing down a list of segments.
//!
//! # The shape
//!
//! A store gains a segment every time a batch commits, and those arrive at level
//! zero. Eight of them are enough to fold: eight segments is eight postings
//! lists to walk for a term and eight key filters to ask before a document can
//! be called new, and folding them into one at level one turns that back into
//! one of each. That is the trigger nearly every fold comes from, because it is
//! the one an ingest run keeps pulling.
//!
//! Past level zero it is size tiered with a growth factor of ten. Level one is
//! allowed ten times what a level zero segment is expected to weigh, level two a
//! hundred times, and a level is folded into the next when the segments at it
//! come to more than that. The factor is what keeps the number of levels
//! logarithmic in the size of the store: a terabyte is five levels at a factor
//! of ten and thirty at a factor of two, and every level is a segment every
//! query opens.
//!
//! Beside those two there is the tombstone trigger, which is the only one that
//! does not care about size or count. A deleted document is still in the segment
//! it was written to and still costs what a live one costs: a posting to skip in
//! every list it appears in, an entry in the key filter, a row of the columns
//! and a slot in the vectors. Once three in ten of the documents in a run are
//! dead, a rewrite of that run pays for itself, and it is the one fold that
//! gives space back rather than only giving back segment count.
//!
//! # Why a run and not a set
//!
//! A fold can only take a contiguous run of the manifest, because the position
//! of a segment in that list is what decides which copy of a key wins. A fold of
//! the first and the third segment would leave the second holding a key the
//! first also holds, and wherever the replacement is put, one of those two keys
//! now answers with the wrong copy. So the levels are read as runs: the segments
//! at a level that sit next to each other are foldable together, and two runs of
//! the same level with something else between them are two candidates rather
//! than one.
//!
//! That is also why the two size rules never choose a run of one. A fold of a
//! single segment is a copy of it with its deleted documents left out, which is
//! worth doing when there are enough of them to pay for it and is worth nothing
//! at all when there are not, so it belongs to the tombstone trigger and to
//! nothing else. The tombstone trigger is the one rule here that will take a run
//! of one.
//!
//! # Where the replacement lands
//!
//! A size fold puts what it wrote one level deeper than the run it read, because
//! that is what the level number is for: it says how many folds a segment has
//! been through and therefore what it is expected to weigh. A tombstone fold
//! does not. Nothing about that run grew, it was rewritten in place to drop what
//! was dead in it, and promoting it would walk a segment down the levels every
//! time somebody deleted from it until it sat at a level whose capacity nothing
//! could ever fill. So a job carries the level its replacement should land at
//! rather than leaving the caller to assume.
//!
//! # Pushing back
//!
//! [`Policy::pressure`] is the other half, and it answers the other question. A
//! job says what is worth folding, pressure says whether whoever is writing
//! should be allowed to keep going, and the two have to be separate because a
//! store can be behind on its folding without anybody being to blame and can be
//! so far behind that carrying on is the thing making it worse.
//!
//! It counts level zero and nothing else. A deep level over its capacity is a
//! fold that ought to happen, but holding a writer up for it would not stop it
//! growing, because a writer does not add to a deep level. Level zero is where
//! every commit lands, so it is the only place where writing faster makes the
//! problem worse. Eight is where a fold is due and twelve is where a writer has
//! to stop and pay for one, and the gap between them is the room a store has to
//! fold in the background before folding happens in front of whoever is writing.
//!
//! # What it does not do
//!
//! It does not say when to run, only what to run and whether to wait. Nothing
//! here knows how much read latency a fold is allowed to cost or whether the
//! machine is busy, and a caller that folds every job this returns as fast as it
//! can return them will spend the whole store's write bandwidth on folding. The
//! rate limit is the caller's, and it is what turns this from a rule into a
//! policy.

use core::ops::Range;

use crate::manifest::Segment;

/// How many segments at level zero are enough to make a fold of them due.
///
/// Eight, because that is where the read cost of the list stops being noise: a
/// term costs one posting list walk per segment and a key costs one filter per
/// segment, so eight segments is eight of each on the way to an answer that one
/// segment gives in one.
const LEVEL_ZERO_CAP: usize = 8;

/// How much larger each level is than the one above it.
const GROWTH: u64 = 10;

/// What a segment at level zero is expected to weigh.
///
/// It is the flush budget rather than a measurement, because the budget is what
/// decides the size of a flushed segment and a policy that guessed at it would
/// be a second answer to a question that already has one.
const BASE: u64 = 128 << 20;

/// How many of every hundred documents in a run have to be deleted before
/// rewriting it is worth the read and the write.
///
/// Thirty is a trade rather than a derivation. Fold at a smaller share and a
/// store spends its write bandwidth copying documents that were live anyway,
/// fold at a larger one and every query walks postings for documents that
/// stopped answering long ago. Thirty is roughly where the two costs meet on the
/// stores this has been run on, and it is the figure most of the engines that
/// have had this argument before settled on.
const DEAD_SHARE: u32 = 30;

/// How many segments at level zero a writer is allowed to get to before it is
/// made to stop and pay for a fold itself.
///
/// Twelve rather than eight, so that there is room between the count at which a
/// fold becomes due and the count at which a writer has to wait for one. That
/// gap is the whole of the difference between a store that folds in the
/// background and a store that folds in front of whoever is writing to it, and
/// four segments of it is roughly a fold's worth of writing on the corpora this
/// has been run on.
const HARD_CAP: usize = 12;

/// The rule a store is folded by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Policy {
    /// How many level zero segments are allowed before a fold of them is due.
    pub level_zero_cap: usize,
    /// How much larger each level is than the one above it.
    pub growth: u64,
    /// What a level zero segment is expected to weigh, which is what every
    /// level's capacity is measured in multiples of.
    pub base: u64,
    /// How many of every hundred documents in a run have to be deleted before a
    /// rewrite of it is due.
    pub dead_share: u32,
    /// How many level zero segments there can be before a writer has to stop
    /// and fold before it commits anything else.
    pub hard_cap: usize,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            level_zero_cap: LEVEL_ZERO_CAP,
            growth: GROWTH,
            base: BASE,
            dead_share: DEAD_SHARE,
            hard_cap: HARD_CAP,
        }
    }
}

/// Why a fold is due.
///
/// It is carried out of the decision rather than reconstructed from the job,
/// because the two triggers can pick the same run and a report that guessed
/// which one fired would be guessing about the only interesting part.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Reason {
    /// There are as many segments at level zero as are allowed.
    LevelZeroFull,
    /// The segments at a level come to more than the level holds.
    OverCapacity,
    /// Enough of the documents in the run are deleted to pay for rewriting it.
    Deleted,
}

impl Reason {
    /// What to print when a report says why it is folding.
    #[must_use]
    pub const fn why(self) -> &'static str {
        match self {
            Self::LevelZeroFull => "level zero is full",
            Self::OverCapacity => "the level holds more than it is allowed",
            Self::Deleted => "enough of it is deleted to pay for the rewrite",
        }
    }
}

/// One fold, ready to be handed to
/// [`Store::compact`](crate::file::Store::compact).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    /// Which segments, as positions in the manifest.
    pub run: Range<usize>,
    /// Which level they are at now.
    pub level: u32,
    /// Which level the replacement should land at, which is one past `level`
    /// for the two size rules and `level` itself for a rewrite that only drops
    /// what is dead.
    pub into: u32,
    /// Which rule chose them.
    pub reason: Reason,
    /// How many bytes of segment and tombstone the run comes to, which is the
    /// work the fold has to read and roughly what it will write.
    pub bytes: u64,
    /// How many documents the run holds, including the deleted ones.
    pub docs: u64,
    /// How many of those are deleted, which is what the rewrite gets back.
    pub dead: u64,
}

impl Policy {
    /// How much a level is allowed to hold.
    ///
    /// The base times the growth for every level past zero, so level one is ten
    /// level zero segments and level two is a hundred of them. It saturates
    /// rather than wrapping, and a saturated capacity is a level nothing will
    /// ever fill, which is the right answer for a level that deep.
    #[must_use]
    pub const fn capacity(self, level: u32) -> u64 {
        let mut room = self.base;
        let mut at = 0;
        while at < level {
            room = match room.checked_mul(self.growth) {
                Some(room) => room,
                None => return u64::MAX,
            };
            at += 1;
        }
        room
    }

    /// The fold that is due, or nothing if none is, knowing nothing about what
    /// has been deleted.
    ///
    /// This is the manifest on its own, which is all a caller holding a manifest
    /// and no file has. The tombstone trigger cannot fire from it, because how
    /// many documents in a segment are deleted is in the bitmap beside the
    /// segment rather than in the manifest. Use [`choose_with`](Self::choose_with)
    /// where those counts can be had.
    #[must_use]
    pub fn choose(self, segments: &[Segment]) -> Option<Job> {
        self.choose_with(segments, &[])
    }

    /// The fold that is due, given how many documents in each segment are
    /// deleted.
    ///
    /// `deleted` is read by position and a position it does not reach counts as
    /// nothing deleted, so a caller that has the counts for some segments and
    /// not others is not obliged to invent the rest.
    ///
    /// Level zero is asked first, because it is the trigger an ingest run keeps
    /// pulling and the one whose cost every query pays. Dead weight is asked
    /// next, because it is the only fold that gives space back and because a
    /// level that is over capacity for the third time in an hour is usually a
    /// level full of documents that were replaced rather than one that grew.
    /// Size is asked last. Within each rule the runs are taken in the order they
    /// sit in the file, which is oldest first, so a store with two runs due
    /// folds the older one first and the newer one on the next call.
    #[must_use]
    pub fn choose_with(self, segments: &[Segment], deleted: &[u64]) -> Option<Job> {
        let runs = runs(segments, deleted);
        for run in &runs {
            if run.level == 0 && run.run.len() >= self.level_zero_cap {
                return Some(job(run, Reason::LevelZeroFull));
            }
        }
        for run in &runs {
            if run.docs > 0
                && run.dead.saturating_mul(100)
                    >= run.docs.saturating_mul(u64::from(self.dead_share))
            {
                return Some(job(run, Reason::Deleted));
            }
        }
        for run in &runs {
            // A fold of one segment is a copy of it, and the case where that is
            // worth doing is deletions rather than size.
            if run.level > 0 && run.run.len() >= 2 && run.bytes > self.capacity(run.level) {
                return Some(job(run, Reason::OverCapacity));
            }
        }
        None
    }

    /// What to say to somebody about to write into this store.
    ///
    /// The count is every segment at level zero rather than the longest run of
    /// them, because what this is about is what a reader pays, and a reader pays
    /// for a segment whether or not it sits next to another one at its level.
    #[must_use]
    pub fn pressure(self, segments: &[Segment]) -> Pressure {
        let zero = segments.iter().filter(|segment| segment.level == 0).count();
        if zero >= self.hard_cap {
            Pressure::Stalled
        } else if zero >= self.level_zero_cap {
            Pressure::Behind
        } else {
            Pressure::Clear
        }
    }
}

/// How far behind the folding is, from the point of view of somebody trying to
/// write.
///
/// It is about level zero and nothing else. A deep level that is over capacity
/// is a fold that ought to happen, but it is not a reason to hold a writer up,
/// because a writer does not add to a deep level and waiting would not stop it
/// growing. Level zero is the one a commit adds to, so it is the only one where
/// writing faster makes the problem worse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Pressure {
    /// Level zero is under the cap. Write.
    Clear,
    /// A fold of level zero is due and has not happened. A writer may carry on,
    /// and something ought to be folding.
    Behind,
    /// Level zero has got to the hard cap. A writer that commits anything else
    /// before a fold happens is making a store nobody can read quickly, so it
    /// pays for the fold itself.
    Stalled,
}

impl Pressure {
    /// Whether there is nothing to say to a writer.
    #[must_use]
    pub const fn is_clear(self) -> bool {
        matches!(self, Self::Clear)
    }

    /// What to print when a report says why a writer waited.
    #[must_use]
    pub const fn why(self) -> &'static str {
        match self {
            Self::Clear => "level zero is under the cap",
            Self::Behind => "a fold of level zero is due",
            Self::Stalled => "level zero is at the hard cap",
        }
    }
}

/// One run and a reason, as a job.
fn job(run: &Stretch, reason: Reason) -> Job {
    let into = match reason {
        Reason::Deleted => run.level,
        Reason::LevelZeroFull | Reason::OverCapacity => run.level.saturating_add(1),
    };
    Job {
        run: run.run.clone(),
        level: run.level,
        into,
        reason,
        bytes: run.bytes,
        docs: run.docs,
        dead: run.dead,
    }
}

/// A maximal run of segments sitting next to each other at the same level.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Stretch {
    run: Range<usize>,
    level: u32,
    bytes: u64,
    docs: u64,
    dead: u64,
}

/// Every maximal run of one level, in the order they sit in the file.
fn runs(segments: &[Segment], deleted: &[u64]) -> Vec<Stretch> {
    let mut found: Vec<Stretch> = Vec::new();
    for (at, segment) in segments.iter().enumerate() {
        let weight = segment
            .len
            .saturating_add(u64::from(segment.tombstones_len));
        let docs = u64::from(segment.docs);
        // A count past the end of the segment's own documents would make a run
        // look more than entirely dead, which is a number no report should ever
        // print.
        let dead = deleted.get(at).copied().unwrap_or(0).min(docs);
        match found.last_mut() {
            Some(open) if open.level == segment.level => {
                open.run.end = at + 1;
                open.bytes = open.bytes.saturating_add(weight);
                open.docs = open.docs.saturating_add(docs);
                open.dead = open.dead.saturating_add(dead);
            }
            _ => found.push(Stretch {
                run: at..at + 1,
                level: segment.level,
                bytes: weight,
                docs,
                dead,
            }),
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A segment at a level, of a size, holding ten documents.
    ///
    /// Ten because the tombstone rule counts in hundredths and a run of ten
    /// documents is the smallest one where three of them is the threshold
    /// exactly.
    fn segment(level: u32, len: u64) -> Segment {
        Segment {
            len,
            level,
            docs: 10,
            ..Segment::default()
        }
    }

    /// `count` segments at `level`, each of `len` bytes.
    fn several(level: u32, len: u64, count: usize) -> Vec<Segment> {
        (0..count).map(|_| segment(level, len)).collect()
    }

    #[test]
    fn an_empty_store_has_nothing_to_fold() {
        assert_eq!(Policy::default().choose(&[]), None);
    }

    #[test]
    fn level_zero_is_folded_at_the_cap_and_not_before_it() {
        let policy = Policy::default();
        let seven = several(0, 1 << 20, 7);
        assert_eq!(policy.choose(&seven), None);

        let eight = several(0, 1 << 20, 8);
        let job = policy.choose(&eight).expect("eight is enough");
        assert_eq!(job.run, 0..8);
        assert_eq!(job.level, 0);
        assert_eq!(job.reason, Reason::LevelZeroFull);
        assert_eq!(job.bytes, 8 << 20);
    }

    #[test]
    fn a_run_is_the_segments_that_sit_next_to_each_other() {
        // Four, then a folded one, then eight. Only the eight are foldable
        // together, and a fold that took all twelve would move the folded
        // segment past documents that replaced the ones in it.
        let mut segments = several(0, 1 << 20, 4);
        segments.push(segment(1, 64 << 20));
        segments.extend(several(0, 1 << 20, 8));
        let job = Policy::default().choose(&segments).expect("a fold is due");
        assert_eq!(job.run, 5..13);
        assert_eq!(job.reason, Reason::LevelZeroFull);
    }

    #[test]
    fn a_level_is_folded_once_it_holds_more_than_it_is_allowed() {
        let policy = Policy::default();
        // Level one holds ten level zero segments, so nine of them is under it
        // and eleven is over.
        let under = several(1, policy.base, 9);
        assert_eq!(policy.choose(&under), None);

        let over = several(1, policy.base, 11);
        let job = policy.choose(&over).expect("over capacity");
        assert_eq!(job.run, 0..11);
        assert_eq!(job.level, 1);
        assert_eq!(job.reason, Reason::OverCapacity);
    }

    #[test]
    fn one_segment_over_capacity_is_left_where_it_is() {
        // A fold of one segment writes the same documents out again, and the
        // reason to do that is deletions rather than size.
        let policy = Policy::default();
        let alone = vec![segment(1, policy.capacity(1) * 4)];
        assert_eq!(policy.choose(&alone), None);
    }

    #[test]
    fn a_full_level_zero_is_folded_before_a_level_that_is_over_capacity() {
        let policy = Policy::default();
        let mut segments = several(1, policy.base, 11);
        segments.extend(several(0, 1 << 20, 8));
        let job = policy.choose(&segments).expect("a fold is due");
        assert_eq!(job.reason, Reason::LevelZeroFull);
        assert_eq!(job.run, 11..19);
    }

    #[test]
    fn a_level_holds_the_growth_factor_more_than_the_one_above_it() {
        let policy = Policy::default();
        assert_eq!(policy.capacity(0), policy.base);
        assert_eq!(policy.capacity(1), policy.base * 10);
        assert_eq!(policy.capacity(2), policy.base * 100);
        // Deep enough that the answer does not fit, and a level nothing can
        // fill is the right answer rather than a wrapped one.
        assert_eq!(policy.capacity(64), u64::MAX);
    }

    #[test]
    fn the_tombstones_beside_a_segment_count_towards_its_level() {
        let policy = Policy::default();
        let mut segments = several(1, policy.base / 5, 5);
        for segment in &mut segments {
            segment.tombstones_len = u32::MAX;
        }
        let job = policy.choose(&segments).expect("over capacity");
        assert_eq!(job.bytes, 5 * (policy.base / 5 + u64::from(u32::MAX)));
    }

    #[test]
    fn a_run_is_rewritten_once_enough_of_it_is_dead() {
        let policy = Policy::default();
        // One segment of ten documents, sitting at a level where no size rule
        // will ever look at it on its own.
        let alone = vec![segment(2, 1 << 20)];
        assert_eq!(policy.choose_with(&alone, &[2]), None);

        let job = policy.choose_with(&alone, &[3]).expect("three in ten");
        assert_eq!(job.run, 0..1);
        assert_eq!(job.level, 2);
        assert_eq!(job.reason, Reason::Deleted);
        assert_eq!(job.docs, 10);
        assert_eq!(job.dead, 3);
    }

    #[test]
    fn a_rewrite_that_only_drops_the_dead_stays_where_it_is() {
        let policy = Policy::default();
        let alone = vec![segment(3, 1 << 20)];
        let job = policy.choose_with(&alone, &[9]).expect("nine in ten");
        assert_eq!(job.into, job.level, "a rewrite in place is not a promotion");

        // Where a size rule chose it, the replacement is a level deeper.
        let full = several(0, 1 << 20, 8);
        let job = policy.choose(&full).expect("level zero is full");
        assert_eq!(job.into, 1);
    }

    #[test]
    fn the_dead_are_counted_across_the_run_rather_than_segment_by_segment() {
        let policy = Policy::default();
        // Four segments of ten at the same level, one of them entirely dead.
        // Ten in forty is a quarter, which is under the share, so the run is
        // left alone even though one segment in it is nothing but deletions.
        let run = several(1, 1 << 20, 4);
        assert_eq!(policy.choose_with(&run, &[10, 0, 0, 0]), None);
        let job = policy
            .choose_with(&run, &[10, 2, 0, 0])
            .expect("twelve in forty");
        assert_eq!(job.run, 0..4);
        assert_eq!(job.dead, 12);
    }

    #[test]
    fn dead_weight_is_folded_before_a_level_that_is_over_capacity() {
        let policy = Policy::default();
        let mut segments = several(1, policy.base, 11);
        segments.push(segment(2, 1 << 20));
        let mut deleted = vec![0; 11];
        deleted.push(5);
        let job = policy
            .choose_with(&segments, &deleted)
            .expect("a fold is due");
        assert_eq!(job.reason, Reason::Deleted);
        assert_eq!(job.run, 11..12);
    }

    #[test]
    fn a_full_level_zero_is_folded_before_dead_weight() {
        let policy = Policy::default();
        let mut segments = vec![segment(1, 1 << 20)];
        segments.extend(several(0, 1 << 20, 8));
        let mut deleted = vec![10];
        deleted.extend(std::iter::repeat_n(0, 8));
        let job = policy
            .choose_with(&segments, &deleted)
            .expect("a fold is due");
        assert_eq!(job.reason, Reason::LevelZeroFull);
        assert_eq!(job.run, 1..9);
    }

    #[test]
    fn a_manifest_on_its_own_never_says_a_rewrite_is_due() {
        // Nothing in a manifest says how many documents in a segment are
        // deleted, so a caller that has not been given the counts is told that
        // nothing is due rather than being told a guess.
        let mut alone = vec![segment(2, 1 << 20)];
        alone[0].tombstones_len = 4096;
        assert_eq!(Policy::default().choose(&alone), None);
    }

    #[test]
    fn more_deletions_than_documents_are_taken_as_all_of_them() {
        // A count that disagrees with the manifest is a bug somewhere, and the
        // answer here is a run that is entirely dead rather than a report that
        // says two hundred percent of it is.
        let alone = vec![segment(2, 1 << 20)];
        let job = Policy::default()
            .choose_with(&alone, &[1000])
            .expect("all of it");
        assert_eq!(job.docs, 10);
        assert_eq!(job.dead, 10);
    }

    #[test]
    fn a_writer_is_told_to_wait_once_level_zero_is_at_the_hard_cap() {
        let policy = Policy::default();
        assert_eq!(policy.pressure(&[]), Pressure::Clear);
        assert_eq!(policy.pressure(&several(0, 1 << 20, 7)), Pressure::Clear);
        // A fold is due here, and a writer is not made to pay for it yet.
        assert_eq!(policy.pressure(&several(0, 1 << 20, 8)), Pressure::Behind);
        assert_eq!(policy.pressure(&several(0, 1 << 20, 11)), Pressure::Behind);
        assert_eq!(policy.pressure(&several(0, 1 << 20, 12)), Pressure::Stalled);
        assert!(Pressure::Clear.is_clear());
        assert!(!Pressure::Behind.is_clear());
    }

    #[test]
    fn only_level_zero_holds_a_writer_up() {
        // Twenty segments at level one is a store far behind on its folding and
        // is not a reason to stop somebody writing, because what they write does
        // not go there.
        let policy = Policy::default();
        assert_eq!(
            policy.pressure(&several(1, policy.base, 20)),
            Pressure::Clear
        );
    }

    #[test]
    fn level_zero_is_counted_wherever_it_sits() {
        // Six, then a folded segment, then six more. No run of them is at the
        // cap and there are twelve of them, and twelve is what a reader pays.
        let policy = Policy::default();
        let mut segments = several(0, 1 << 20, 6);
        segments.push(segment(1, 64 << 20));
        segments.extend(several(0, 1 << 20, 6));
        assert_eq!(policy.pressure(&segments), Pressure::Stalled);
    }

    #[test]
    fn a_pressure_says_what_it_was() {
        assert_ne!(Pressure::Behind.why(), Pressure::Stalled.why());
        assert!(!Pressure::Clear.why().is_empty());
    }

    #[test]
    fn a_reason_says_what_it_was() {
        assert_ne!(Reason::LevelZeroFull.why(), Reason::OverCapacity.why());
        assert_ne!(Reason::Deleted.why(), Reason::OverCapacity.why());
        assert!(!Reason::LevelZeroFull.why().is_empty());
        assert!(!Reason::Deleted.why().is_empty());
    }
}
