//! The write ahead log.
//!
//! Everything a writer does is written here first and acknowledged from here.
//! The memtable it also went into is memory, and memory does not survive the
//! machine losing power, so until a batch is in the log and the log is on the
//! platter the engine has not promised anything.
//!
//! # Why a ring inside the file
//!
//! Inside the file because a store is one file, and one file is the operational
//! property the whole design is arranged around: one thing to copy, one thing to
//! back up, one thing to lose.
//!
//! A ring rather than an append region because a ring bounds the space. The log
//! only has to hold what has not been flushed into a segment yet, so it has a
//! natural working size, and giving it a fixed region means truncating it is a
//! pointer move rather than a rewrite. The size is chosen when the store is
//! created, because everything after it in the file would have to move.
//!
//! # Positions
//!
//! The head and the tail are logical positions that only ever increase, and the
//! byte a position names is at `position % len` inside the ring. Positions that
//! wrap would make "is the tail ahead of the head" a question with two answers,
//! and at one record per position increment of a few tens of bytes a `u64` runs
//! for longer than the hardware will.
//!
//! Both live in the manifest, because how far the log has been consumed is part
//! of what a commit commits.
//!
//! # Records never straddle the end
//!
//! A record that ran off the end of the ring and continued at the start would be
//! two slices everywhere it is touched, for the sake of the few dozen bytes it
//! saves once per lap. So when a record does not fit in what is left, the writer
//! skips to the start of the ring instead.
//!
//! The reader has to know a skip happened, and it works it out the same way the
//! writer decided. If the bytes left before the end of the ring are fewer than
//! the smallest record, no record can begin there and both sides wrap. If there
//! are more, the writer leaves a padding record covering them, which is a header
//! and a checksum and no payload, and the reader skips it. One threshold, one
//! rule, and both sides read it off the same arithmetic.
//!
//! # What is not here
//!
//! The file. This module works on the ring as a byte slice, which is what a
//! mapping of that region of the file is, and the layer holding the descriptor
//! decides when to fsync. The payload format is not here either: a record
//! carries bytes, and what an upsert means is the writer's business.

use crate::codec::{get_u32, get_u64, get_u128, put_u32, put_u64, put_u128, split_at};
use crate::error::{Error, Result};
use crate::xxh3;

/// The fixed part at the front of a record.
///
/// ```text
/// 0    4   length, the whole record including this header and the checksum
/// 4    4   kind
/// 8    8   sequence number
/// 16   ...  payload
/// ...  16   xxh3-128 of everything before it
/// ```
pub const HEADER_LEN: usize = 16;

/// The number of bytes a checksum takes at the end of a record.
const SUM_LEN: usize = 16;

/// The smallest a record can be, which is a header, no payload and a checksum.
///
/// This is also the threshold that decides how the end of a lap is handled. With
/// this many bytes left or more the writer leaves a padding record, and with
/// fewer there is no room for one and both sides wrap without a marker.
pub const MIN_RECORD: usize = HEADER_LEN + SUM_LEN;

/// What a record is for.
pub mod kind {
    /// A document added or replaced, carrying its analysed form.
    ///
    /// Analysed rather than raw, so that recovery does not run the analyser
    /// again. Running it again would be slow, and if the analyser had changed
    /// between the crash and the recovery it would also be wrong.
    pub const UPSERT: u32 = 1;
    /// A document deleted.
    pub const DELETE: u32 = 2;
    /// Everything before this record is durable and can be replayed as a unit.
    ///
    /// One of these is written and fsynced for a batch of writers that arrived
    /// together, which is the difference between one fsync for five hundred
    /// documents and five hundred of them.
    pub const COMMIT: u32 = 3;
    /// Everything before this record is in a segment named by the manifest.
    pub const CHECKPOINT: u32 = 4;
    /// Nothing. It covers the bytes at the end of a lap that a record did not
    /// fit in.
    pub const PAD: u32 = 5;
}

/// One record, borrowed from the ring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Record<'a> {
    /// What the record is for, from [`kind`].
    pub kind: u32,
    /// Which record this is. Assigned by the log and never reused.
    pub sequence: u64,
    /// The bytes the writer put in, which this module does not interpret.
    pub payload: &'a [u8],
    /// Where the record begins, as a logical position.
    pub position: u64,
    /// How many bytes of the ring the record takes.
    ///
    /// The same as the header, the payload and the checksum together for
    /// everything except a padding record, which claims the whole gap it covers
    /// while only its header and its checksum are written.
    pub span: u32,
}

/// Writes one record into `out`.
///
/// # Errors
///
/// Returns [`Error::Overflow`] for a payload too long to record its own length.
pub fn encode(kind: u32, sequence: u64, payload: &[u8], out: &mut Vec<u8>) -> Result<()> {
    let span = u32::try_from(HEADER_LEN + payload.len() + SUM_LEN).map_err(|_| Error::Overflow)?;
    frame(kind, sequence, payload, span, out);
    Ok(())
}

/// Lays out one record, with the span given rather than derived.
///
/// The one place a record is written, so that the only encoder and the only
/// decoder cannot drift apart. The span is a parameter because a padding record
/// claims a gap that is larger than the bytes it writes, and it is the only
/// record that does.
fn frame(kind: u32, sequence: u64, payload: &[u8], span: u32, out: &mut Vec<u8>) {
    let start = out.len();
    put_u32(out, span);
    put_u32(out, kind);
    put_u64(out, sequence);
    out.extend_from_slice(payload);
    let sum = xxh3::hash128(&out[start..]);
    put_u128(out, sum);
}

/// Reads the record at the front of `bytes`.
///
/// # Errors
///
/// Returns [`Error::BadRecord`] if the length field is not a length a record can
/// have, [`Error::Truncated`] if the record runs past what was given, and
/// [`Error::Xxh3Mismatch`] if the bytes are not the bytes that were written.
///
/// The length is checked before it is used, because it comes from the file and
/// the first thing a flipped byte in a header does is make a record claim a size
/// it does not have.
pub fn decode(bytes: &[u8]) -> Result<Record<'_>> {
    let (span, rest) = get_u32(bytes)?;
    let claimed = span as usize;
    if claimed < MIN_RECORD {
        return Err(Error::BadRecord { length: span });
    }
    let (kind, _) = get_u32(rest)?;
    // A padding record is the one thing whose written length is not its claimed
    // length. It claims the gap it covers so that a reader knows how far to
    // skip, and only its header and its checksum are ever written, because the
    // rest of the gap is whatever the previous lap left there.
    let covered = if kind == kind::PAD {
        HEADER_LEN
    } else {
        claimed - SUM_LEN
    };
    let (frame, _) = split_at(bytes, covered + SUM_LEN)?;
    let (body, tail) = split_at(frame, covered)?;
    let (stored, _) = get_u128(tail)?;
    let computed = xxh3::hash128(body);
    if stored != computed {
        return Err(Error::Xxh3Mismatch { stored, computed });
    }
    let (_, rest) = split_at(body, 8)?;
    let (sequence, payload) = get_u64(rest)?;
    Ok(Record {
        kind,
        sequence,
        payload,
        position: 0,
        span,
    })
}

/// The log ring, over the bytes of the region it lives in.
///
/// The slice is the ring, so its length is the ring's length. A caller holding a
/// mapping of the file passes the sub slice covering the region the superblock
/// named.
#[derive(Debug)]
pub struct Log<'a> {
    /// The ring itself.
    bytes: &'a mut [u8],
    /// How far the log has been consumed, from the manifest.
    head: u64,
    /// How far the log has been written, from the manifest.
    tail: u64,
    /// The sequence number the next record gets.
    sequence: u64,
}

impl<'a> Log<'a> {
    /// Opens a ring at the positions the manifest recorded.
    ///
    /// The sequence is where numbering resumes, which after a recovery is one
    /// past the highest sequence a replay saw.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Truncated`] if the region is too small to hold a record
    /// at all, and [`Error::BadPositions`] if the head and the tail do not
    /// describe a ring.
    pub fn open(bytes: &'a mut [u8], head: u64, tail: u64, sequence: u64) -> Result<Self> {
        if bytes.len() < MIN_RECORD {
            return Err(Error::Truncated {
                needed: MIN_RECORD,
                available: bytes.len(),
            });
        }
        let len = bytes.len() as u64;
        if tail < head || tail - head > len {
            return Err(Error::BadPositions { head, tail });
        }
        Ok(Self {
            bytes,
            head,
            tail,
            sequence,
        })
    }

    /// A ring that has never been written to.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Truncated`] if the region is too small to hold a record.
    pub fn empty(bytes: &'a mut [u8]) -> Result<Self> {
        Self::open(bytes, 0, 0, 1)
    }

    /// How far the log has been consumed.
    #[must_use]
    pub const fn head(&self) -> u64 {
        self.head
    }

    /// How far the log has been written.
    #[must_use]
    pub const fn tail(&self) -> u64 {
        self.tail
    }

    /// The sequence the next record will get.
    #[must_use]
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    /// How long the ring is.
    #[must_use]
    pub fn len(&self) -> u64 {
        self.bytes.len() as u64
    }

    /// Whether the log holds nothing that has not been consumed.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.head == self.tail
    }

    /// How many bytes are free.
    #[must_use]
    pub fn free(&self) -> u64 {
        self.len() - (self.tail - self.head)
    }

    /// Where a logical position lands in the ring.
    #[must_use]
    pub fn physical(&self, position: u64) -> usize {
        // The remainder is smaller than the length of a slice, and a slice
        // length is a `usize`, so the conversion cannot fail. The fallback is
        // there rather than an unwrap so that nothing on this path can panic.
        usize::try_from(position % self.len()).unwrap_or_default()
    }

    /// Appends a record and returns the sequence it was given.
    ///
    /// The record is in the ring when this returns and durable when the caller
    /// has fsynced. The bytes that changed are the ones between the tail before
    /// the call and the tail after it, which is at most two runs, because the
    /// region can cross the end of the ring once.
    ///
    /// # Errors
    ///
    /// Returns [`Error::LogFull`] if the record does not fit in the free space,
    /// counting any padding it needs to reach the start of the ring, and
    /// [`Error::Overflow`] for a payload too long to record its own length.
    pub fn append(&mut self, kind: u32, payload: &[u8]) -> Result<u64> {
        let needed = HEADER_LEN
            .checked_add(payload.len())
            .and_then(|size| size.checked_add(SUM_LEN))
            .ok_or(Error::Overflow)? as u64;
        let remaining = self.len() - self.physical(self.tail) as u64;
        let padding = if needed > remaining { remaining } else { 0 };
        let total = needed.checked_add(padding).ok_or(Error::Overflow)?;
        if total > self.free() {
            return Err(Error::LogFull {
                needed: total,
                free: self.free(),
            });
        }
        if padding >= MIN_RECORD as u64 {
            self.put(kind::PAD, self.sequence, &[], padding)?;
        }
        self.tail += padding;
        let sequence = self.sequence;
        self.put(kind, sequence, payload, needed)?;
        self.tail += needed;
        self.sequence += 1;
        Ok(sequence)
    }

    /// Writes one record at the tail, filling to `length` bytes.
    ///
    /// A padding record is a header and a checksum with nothing between them,
    /// and it claims the whole gap, so its length is not derived from its
    /// payload the way a real record's is.
    fn put(&mut self, kind: u32, sequence: u64, payload: &[u8], length: u64) -> Result<()> {
        let at = self.physical(self.tail);
        let span = u32::try_from(length).map_err(|_| Error::Overflow)?;
        let mut record = Vec::with_capacity(HEADER_LEN + payload.len() + SUM_LEN);
        frame(kind, sequence, payload, span, &mut record);
        let end = at
            .checked_add(record.len())
            .filter(|end| *end <= self.bytes.len())
            .ok_or(Error::LogFull {
                needed: length,
                free: self.free(),
            })?;
        self.bytes[at..end].copy_from_slice(&record);
        Ok(())
    }

    /// Moves the head forward, freeing everything before it.
    ///
    /// Called once the records up to `through` are in a segment and the manifest
    /// naming that segment has been committed. Not before, because a log
    /// truncated ahead of the commit that replaces it is a hole in the only
    /// record of what the engine promised.
    ///
    /// # Errors
    ///
    /// Returns [`Error::BadPositions`] if the new head is behind the old one or
    /// ahead of the tail.
    pub fn truncate(&mut self, through: u64) -> Result<()> {
        if through < self.head || through > self.tail {
            return Err(Error::BadPositions {
                head: through,
                tail: self.tail,
            });
        }
        self.head = through;
        Ok(())
    }

    /// Walks the records the log holds, oldest first.
    #[must_use]
    pub fn replay(&self) -> Replay<'_> {
        Replay {
            bytes: self.bytes,
            position: self.head,
            tail: self.tail,
        }
    }
}

/// A walk over the records in a log, oldest first.
///
/// Made by [`Log::replay`]. It stops at the tail, and it stops at the first
/// record that does not decode, because a log is a sequence and the records
/// after a damaged one cannot be found without knowing how long the damaged one
/// was.
#[derive(Debug)]
pub struct Replay<'a> {
    /// The ring being walked.
    bytes: &'a [u8],
    /// Where the next record starts, as a logical position.
    position: u64,
    /// Where to stop.
    tail: u64,
}

impl Replay<'_> {
    /// Where the walk has reached, which after it ends is the position a writer
    /// should resume at.
    #[must_use]
    pub const fn position(&self) -> u64 {
        self.position
    }

    /// Where a logical position lands in the ring.
    fn physical(&self, position: u64) -> usize {
        // Cannot fail, as in [`Log::physical`].
        usize::try_from(position % self.bytes.len() as u64).unwrap_or_default()
    }
}

impl<'a> Iterator for Replay<'a> {
    type Item = Result<Record<'a>>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.position >= self.tail {
                return None;
            }
            let at = self.physical(self.position);
            let remaining = self.bytes.len() - at;
            if remaining < MIN_RECORD {
                // No record can begin this close to the end of the ring, so the
                // writer wrapped here and so does the walk. The same arithmetic
                // on both sides is what makes the skip need no marker.
                self.position += remaining as u64;
                continue;
            }
            let position = self.position;
            let record = match decode(&self.bytes[at..]) {
                Ok(record) => record,
                Err(error) => {
                    // Stop rather than search for the next header. There is
                    // nothing in the ring that says where a record starts except
                    // the record before it, so a scan would be a guess.
                    self.position = self.tail;
                    return Some(Err(error));
                }
            };
            self.position += u64::from(record.span);
            if record.kind == kind::PAD {
                continue;
            }
            return Some(Ok(Record { position, ..record }));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A ring big enough to wrap in a test and small enough to read in one.
    const RING: usize = 512;

    fn payload(n: usize) -> Vec<u8> {
        (0..n)
            .map(|i| u8::try_from(i % 251).unwrap_or_default())
            .collect()
    }

    fn records(log: &Log<'_>) -> Vec<(u32, u64, Vec<u8>)> {
        log.replay()
            .map(|record| {
                let record = record.expect("a record");
                (record.kind, record.sequence, record.payload.to_vec())
            })
            .collect()
    }

    #[test]
    fn a_record_round_trips() {
        let mut bytes = Vec::new();
        encode(kind::UPSERT, 42, &payload(100), &mut bytes).expect("a record");
        let record = decode(&bytes).expect("a record");
        assert_eq!(record.kind, kind::UPSERT);
        assert_eq!(record.sequence, 42);
        assert_eq!(record.payload, payload(100));
    }

    #[test]
    fn a_record_with_no_payload_is_the_smallest_there_is() {
        let mut bytes = Vec::new();
        encode(kind::COMMIT, 1, &[], &mut bytes).expect("a record");
        assert_eq!(bytes.len(), MIN_RECORD);
        let record = decode(&bytes).expect("a record");
        assert_eq!(record.kind, kind::COMMIT);
        assert!(record.payload.is_empty());
    }

    #[test]
    fn a_length_no_record_could_have_is_caught_before_it_is_used() {
        let mut bytes = Vec::new();
        encode(kind::UPSERT, 1, &payload(20), &mut bytes).expect("a record");
        for claimed in [0u32, 1, 15, 31] {
            bytes[0..4].copy_from_slice(&claimed.to_le_bytes());
            assert_eq!(decode(&bytes), Err(Error::BadRecord { length: claimed }));
        }
    }

    #[test]
    fn a_length_longer_than_what_is_there_is_truncation_and_not_a_read_past_the_end() {
        let mut bytes = Vec::new();
        encode(kind::UPSERT, 1, &payload(20), &mut bytes).expect("a record");
        bytes[0..4].copy_from_slice(&10_000u32.to_le_bytes());
        assert!(matches!(decode(&bytes), Err(Error::Truncated { .. })));
    }

    #[test]
    fn any_single_bit_changed_in_a_record_is_caught() {
        let mut bytes = Vec::new();
        encode(kind::UPSERT, 7, &payload(48), &mut bytes).expect("a record");
        for byte in 4..bytes.len() {
            for bit in 0..8 {
                let mut damaged = bytes.clone();
                damaged[byte] ^= 1 << bit;
                assert!(
                    decode(&damaged).is_err(),
                    "byte {byte} bit {bit} went unnoticed"
                );
            }
        }
    }

    #[test]
    fn truncating_a_record_at_every_length_is_an_error_rather_than_a_panic() {
        let mut bytes = Vec::new();
        encode(kind::UPSERT, 1, &payload(60), &mut bytes).expect("a record");
        for len in 0..bytes.len() {
            assert!(decode(&bytes[..len]).is_err(), "{len}");
        }
    }

    #[test]
    fn what_goes_into_a_log_comes_back_out_in_order() {
        let mut ring = vec![0u8; RING];
        let mut log = Log::empty(&mut ring).expect("a log");
        for n in 0..5 {
            let sequence = log.append(kind::UPSERT, &payload(n * 8)).expect("appended");
            assert_eq!(sequence, n as u64 + 1);
        }
        let found = records(&log);
        assert_eq!(found.len(), 5);
        for (n, (kind, sequence, bytes)) in found.iter().enumerate() {
            assert_eq!(*kind, kind::UPSERT);
            assert_eq!(*sequence, n as u64 + 1);
            assert_eq!(*bytes, payload(n * 8));
        }
    }

    #[test]
    fn an_empty_log_replays_nothing() {
        let mut ring = vec![0u8; RING];
        let log = Log::empty(&mut ring).expect("a log");
        assert!(log.is_empty());
        assert_eq!(log.free(), RING as u64);
        assert_eq!(log.replay().count(), 0);
    }

    #[test]
    fn a_record_that_would_run_off_the_end_starts_the_next_lap_instead() {
        let mut ring = vec![0u8; RING];
        let mut log = Log::empty(&mut ring).expect("a log");
        // Fill most of the ring and truncate it, so what is left before the end
        // is 80 bytes, then write a record that needs 232. This is the case the
        // padding rule exists for.
        log.append(kind::UPSERT, &payload(400)).expect("appended");
        log.truncate(log.tail()).expect("truncated");
        assert_eq!(log.tail(), 432);
        log.append(kind::UPSERT, &payload(200)).expect("appended");
        assert_eq!(
            log.tail(),
            RING as u64 + 232,
            "the record did not start the next lap"
        );
        let found = records(&log);
        assert_eq!(found.len(), 1, "the padding was replayed as a record");
        assert_eq!(found[0].2, payload(200));
    }

    #[test]
    fn a_gap_too_small_for_a_padding_record_is_skipped_without_one() {
        let mut ring = vec![0u8; RING];
        let mut log = Log::empty(&mut ring).expect("a log");
        // Leave 20 bytes before the end of the ring, which is fewer than the
        // smallest record, so there is nowhere to put a marker.
        log.append(kind::UPSERT, &payload(RING - MIN_RECORD - 20))
            .expect("appended");
        assert_eq!(log.physical(log.tail()), RING - 20);
        log.truncate(log.tail()).expect("truncated");
        log.append(kind::COMMIT, &[]).expect("appended");
        assert_eq!(log.physical(log.tail()), MIN_RECORD);
        let found = records(&log);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, kind::COMMIT);
    }

    #[test]
    fn a_record_that_does_not_fit_in_the_free_space_is_refused() {
        let mut ring = vec![0u8; RING];
        let mut log = Log::empty(&mut ring).expect("a log");
        log.append(kind::UPSERT, &payload(200)).expect("appended");
        let error = log.append(kind::UPSERT, &payload(400)).expect_err("full");
        assert!(matches!(error, Error::LogFull { .. }), "{error:?}");
        // And what was already there is untouched, which is the point of
        // refusing rather than overwriting.
        assert_eq!(records(&log).len(), 1);
    }

    #[test]
    fn a_record_larger_than_the_whole_ring_never_fits() {
        let mut ring = vec![0u8; RING];
        let mut log = Log::empty(&mut ring).expect("a log");
        let error = log.append(kind::UPSERT, &payload(RING)).expect_err("full");
        assert!(matches!(error, Error::LogFull { .. }), "{error:?}");
    }

    #[test]
    fn truncating_frees_the_space_and_keeps_the_positions_moving_forward() {
        let mut ring = vec![0u8; RING];
        let mut log = Log::empty(&mut ring).expect("a log");
        for _ in 0..4 {
            log.append(kind::UPSERT, &payload(64)).expect("appended");
        }
        let used = log.tail() - log.head();
        assert_eq!(log.free(), RING as u64 - used);
        log.truncate(log.tail()).expect("truncated");
        assert_eq!(log.free(), RING as u64);
        assert!(log.is_empty());
        assert_eq!(log.replay().count(), 0);
        assert!(log.head() > 0, "the head is a position, not an offset");
    }

    #[test]
    fn truncating_backwards_or_past_the_tail_is_refused() {
        let mut ring = vec![0u8; RING];
        let mut log = Log::empty(&mut ring).expect("a log");
        log.append(kind::UPSERT, &payload(64)).expect("appended");
        log.truncate(log.tail()).expect("truncated");
        assert!(log.truncate(0).is_err());
        assert!(log.truncate(log.tail() + 1).is_err());
    }

    #[test]
    fn a_log_goes_round_and_round() {
        let mut ring = vec![0u8; RING];
        let mut log = Log::empty(&mut ring).expect("a log");
        // Twenty laps of the ring, one record at a time, each read back before
        // the next goes in. Any arithmetic that is right for the first lap and
        // wrong afterwards shows up here.
        for n in 0..(RING / 8) {
            let bytes = payload(n % 96);
            log.append(kind::UPSERT, &bytes).expect("appended");
            let found = records(&log);
            assert_eq!(found.len(), 1, "lap {n}");
            assert_eq!(found[0].2, bytes, "lap {n}");
            log.truncate(log.tail()).expect("truncated");
        }
        assert!(log.tail() > RING as u64, "the ring never wrapped");
    }

    #[test]
    fn a_log_reopened_at_the_positions_the_manifest_held_holds_the_same_records() {
        let mut ring = vec![0u8; RING];
        let (head, tail, sequence, before) = {
            let mut log = Log::empty(&mut ring).expect("a log");
            log.append(kind::UPSERT, &payload(32)).expect("appended");
            log.append(kind::UPSERT, &payload(48)).expect("appended");
            log.append(kind::COMMIT, &[]).expect("appended");
            (log.head(), log.tail(), log.sequence(), records(&log))
        };
        let log = Log::open(&mut ring, head, tail, sequence).expect("a log");
        assert_eq!(records(&log), before);
        assert_eq!(log.sequence(), 4);
    }

    #[test]
    fn positions_that_do_not_describe_a_ring_are_refused() {
        let mut ring = vec![0u8; RING];
        assert!(matches!(
            Log::open(&mut ring, 10, 9, 1),
            Err(Error::BadPositions { .. })
        ));
        assert!(matches!(
            Log::open(&mut ring, 0, RING as u64 + 1, 1),
            Err(Error::BadPositions { .. })
        ));
        let mut tiny = vec![0u8; MIN_RECORD - 1];
        assert!(matches!(
            Log::empty(&mut tiny),
            Err(Error::Truncated { .. })
        ));
    }

    #[test]
    fn a_replay_stops_at_a_damaged_record_and_says_so() {
        let mut ring = vec![0u8; RING];
        let span = (MIN_RECORD + 32) as u64;
        {
            let mut log = Log::empty(&mut ring).expect("a log");
            for _ in 0..3 {
                log.append(kind::UPSERT, &payload(32)).expect("appended");
            }
            assert_eq!(log.tail(), 3 * span);
        }
        // A bit changed in the middle of the second record's payload.
        ring[MIN_RECORD + 32 + 20] ^= 0x01;
        let log = Log::open(&mut ring, 0, 3 * span, 4).expect("a log");
        let found: Vec<_> = log.replay().collect();
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found[0].is_ok());
        assert!(matches!(found[1], Err(Error::Xxh3Mismatch { .. })));
    }

    #[test]
    fn a_replay_reports_where_each_record_was() {
        let mut ring = vec![0u8; RING];
        let mut log = Log::empty(&mut ring).expect("a log");
        log.append(kind::UPSERT, &payload(16)).expect("appended");
        log.append(kind::UPSERT, &payload(16)).expect("appended");
        let positions: Vec<_> = log
            .replay()
            .map(|record| record.expect("a record").position)
            .collect();
        assert_eq!(positions, vec![0, (MIN_RECORD + 16) as u64]);
    }

    #[test]
    fn a_ring_full_of_rubbish_is_an_error_rather_than_a_panic() {
        let mut ring: Vec<u8> = (0..RING)
            .map(|i| u8::try_from(i * 31 % 256).unwrap_or_default())
            .collect();
        let log = Log::open(&mut ring, 0, RING as u64, 1).expect("a log");
        // Whatever it makes of them, it stops, and it does not read past the end
        // of the ring on the way.
        let found: Vec<_> = log.replay().collect();
        assert!(found.iter().any(std::result::Result::is_err), "{found:?}");
    }
}
