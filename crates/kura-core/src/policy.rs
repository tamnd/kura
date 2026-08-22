//! Which segments to fold, and when.
//!
//! [`compact`](crate::compact) writes a replacement for some segments and
//! [`Store::compact`](crate::file::Store::compact) swaps it in. Neither of them
//! decides anything. This is the part that looks at a store and says which fold
//! is due, and it is a function of the manifest alone: no file, no clock, no
//! reader, so a policy question can be asked of a store that is a hundred
//! gigabytes without opening it and can be tested by writing down a list of
//! segments.
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
//! That is also why a fold of a single segment is never chosen here. It is a
//! copy of the segment with its deleted documents left out, which is a thing
//! worth doing when there are enough of them to pay for it, and that is the
//! tombstone trigger rather than this one.
//!
//! # What it does not do
//!
//! It does not say when to run, only what to run. Nothing here knows how much
//! read latency a fold is allowed to cost or whether the machine is busy, and a
//! caller that folds every job this returns as fast as it can return them will
//! spend the whole store's write bandwidth on folding. The rate limit and the
//! backpressure are the caller's, and they are what turn this from a rule into a
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
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            level_zero_cap: LEVEL_ZERO_CAP,
            growth: GROWTH,
            base: BASE,
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
}

impl Reason {
    /// What to print when a report says why it is folding.
    #[must_use]
    pub const fn why(self) -> &'static str {
        match self {
            Self::LevelZeroFull => "level zero is full",
            Self::OverCapacity => "the level holds more than it is allowed",
        }
    }
}

/// One fold, ready to be handed to
/// [`Store::compact`](crate::file::Store::compact).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    /// Which segments, as positions in the manifest.
    pub run: Range<usize>,
    /// Which level they are at now. The replacement lands one past it.
    pub level: u32,
    /// Which rule chose them.
    pub reason: Reason,
    /// How many bytes of segment and tombstone the run comes to, which is the
    /// work the fold has to read and roughly what it will write.
    pub bytes: u64,
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

    /// The fold that is due, or nothing if none is.
    ///
    /// Level zero is asked first, because it is the trigger an ingest run keeps
    /// pulling and the one whose cost every query pays. After that the runs are
    /// taken in the order they sit in the file, which is oldest first, so a
    /// store with two levels over capacity folds the older one first and the
    /// newer one on the next call.
    #[must_use]
    pub fn choose(self, segments: &[Segment]) -> Option<Job> {
        let runs = runs(segments);
        for run in &runs {
            if run.level == 0 && run.run.len() >= self.level_zero_cap {
                return Some(job(run, Reason::LevelZeroFull));
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
}

/// One run and a reason, as a job.
fn job(run: &Stretch, reason: Reason) -> Job {
    Job {
        run: run.run.clone(),
        level: run.level,
        reason,
        bytes: run.bytes,
    }
}

/// A maximal run of segments sitting next to each other at the same level.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Stretch {
    run: Range<usize>,
    level: u32,
    bytes: u64,
}

/// Every maximal run of one level, in the order they sit in the file.
fn runs(segments: &[Segment]) -> Vec<Stretch> {
    let mut found: Vec<Stretch> = Vec::new();
    for (at, segment) in segments.iter().enumerate() {
        let weight = segment
            .len
            .saturating_add(u64::from(segment.tombstones_len));
        match found.last_mut() {
            Some(open) if open.level == segment.level => {
                open.run.end = at + 1;
                open.bytes = open.bytes.saturating_add(weight);
            }
            _ => found.push(Stretch {
                run: at..at + 1,
                level: segment.level,
                bytes: weight,
            }),
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A segment at a level, of a size, with nothing else in it that a policy
    /// reads.
    fn segment(level: u32, len: u64) -> Segment {
        Segment {
            len,
            level,
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
    fn a_reason_says_what_it_was() {
        assert_ne!(Reason::LevelZeroFull.why(), Reason::OverCapacity.why());
        assert!(!Reason::LevelZeroFull.why().is_empty());
    }
}
