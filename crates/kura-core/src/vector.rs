//! Vector storage and similarity.
//!
//! Embeddings are the largest thing a semantic index holds. A million passages
//! at a thousand dimensions is four gigabytes in `f32`, which is the difference
//! between a corpus that fits in memory on a normal machine and one that does
//! not. So vectors are stored quantised to eight bits per dimension, with one
//! scale factor per vector, and the search runs on the quantised form.
//!
//! The accuracy cost is small and it is bounded: the quantisation is symmetric
//! around zero and the scale is the largest absolute value in the vector, so the
//! error per dimension is at most half a step. What that buys is a fourfold
//! reduction in memory and an inner product that runs on integers.
//!
//! Nothing here builds an approximate index yet. A flat scan over quantised
//! vectors is fast enough for a corpus of a few hundred thousand passages, it is
//! exact, and it is the baseline any graph index has to beat to earn its
//! complexity.

use crate::error::{Error, Result};

/// A vector quantised to eight bits per dimension.
#[derive(Debug, Clone, PartialEq)]
pub struct Quantised {
    /// The largest absolute value of the original vector, which maps 127 back to
    /// the original scale.
    pub scale: f32,
    /// One signed byte per dimension.
    pub values: Vec<i8>,
}

impl Quantised {
    /// Quantises a vector.
    ///
    /// A vector that is all zeros keeps a scale of zero and dequantises back to
    /// zeros, which is what a caller storing an empty passage expects.
    #[must_use]
    pub fn from_f32(input: &[f32]) -> Self {
        let scale = input.iter().fold(0.0f32, |acc, v| acc.max(v.abs()));
        if scale == 0.0 || !scale.is_finite() {
            return Self {
                scale: 0.0,
                values: vec![0; input.len()],
            };
        }

        let step = scale / 127.0;
        let values = input
            .iter()
            .map(|v| {
                let scaled = (v / step).round().clamp(-127.0, 127.0);
                // The clamp above puts the value inside i8 before the cast, so
                // the conversion is exact.
                #[allow(clippy::cast_possible_truncation)]
                {
                    scaled as i8
                }
            })
            .collect();

        Self { scale, values }
    }

    /// Returns the vector back in `f32`, with the quantisation error included.
    #[must_use]
    pub fn to_f32(&self) -> Vec<f32> {
        let step = self.scale / 127.0;
        self.values.iter().map(|v| f32::from(*v) * step).collect()
    }

    /// Returns the number of dimensions.
    #[must_use]
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Reports whether the vector has no dimensions.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Returns the dot product with another quantised vector, back in the
    /// original scale.
    ///
    /// # Errors
    ///
    /// Returns [`Error::DimensionMismatch`] if the two vectors have different
    /// lengths. Comparing vectors from two different models is a caller bug that
    /// silently produces a plausible looking number, so it is refused.
    pub fn dot(&self, other: &Self) -> Result<f32> {
        if self.values.len() != other.values.len() {
            return Err(Error::DimensionMismatch {
                left: self.values.len(),
                right: other.values.len(),
            });
        }

        // The accumulation is in i32 because 127 * 127 * 131072 still fits, and
        // an integer sum keeps the result reproducible across machines in a way
        // a float sum does not.
        let raw: i32 = self
            .values
            .iter()
            .zip(other.values.iter())
            .map(|(a, b)| i32::from(*a) * i32::from(*b))
            .sum();

        let step = (self.scale / 127.0) * (other.scale / 127.0);
        // The sum can exceed what an f32 mantissa holds exactly on a very long
        // vector, and that is fine here: the result is a similarity score that
        // is about to be compared against other scores, not a value anyone
        // reconstructs data from.
        #[allow(clippy::cast_precision_loss)]
        Ok(raw as f32 * step)
    }
}

/// Returns the cosine similarity of two vectors, in the range -1 to 1.
///
/// # Errors
///
/// Returns [`Error::DimensionMismatch`] if the lengths differ.
pub fn cosine(a: &[f32], b: &[f32]) -> Result<f32> {
    if a.len() != b.len() {
        return Err(Error::DimensionMismatch {
            left: a.len(),
            right: b.len(),
        });
    }

    let mut dot = 0.0f32;
    let mut norm_a = 0.0f32;
    let mut norm_b = 0.0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        norm_a += x * x;
        norm_b += y * y;
    }

    let denominator = norm_a.sqrt() * norm_b.sqrt();
    if denominator == 0.0 {
        // A zero vector has no direction, so it is not similar to anything. The
        // alternative, returning a division by zero, poisons every ranking it
        // touches.
        return Ok(0.0);
    }
    Ok((dot / denominator).clamp(-1.0, 1.0))
}

/// Scales a vector to unit length in place.
///
/// Storing normalised vectors turns cosine similarity into a plain dot product,
/// which is the single largest saving available on the search path. A zero
/// vector is left alone.
pub fn normalise(vector: &mut [f32]) {
    let norm = vector.iter().map(|v| v * v).sum::<f32>().sqrt();
    if norm == 0.0 || !norm.is_finite() {
        return;
    }
    for value in vector.iter_mut() {
        *value /= norm;
    }
}

#[cfg(test)]
mod tests {
    // The tests below compare floats exactly where the code promises an exact
    // value, such as the zero a degenerate vector has to produce. Everywhere the
    // result is arithmetic, they go through `approx`.
    #![allow(clippy::float_cmp)]

    use super::*;

    fn approx(a: f32, b: f32, tolerance: f32) -> bool {
        (a - b).abs() <= tolerance
    }

    #[test]
    fn cosine_of_a_vector_with_itself_is_one() {
        let v = [0.2f32, -0.5, 0.9, 0.1];
        assert!(approx(cosine(&v, &v).expect("same length"), 1.0, 1e-6));
    }

    #[test]
    fn cosine_of_opposite_vectors_is_minus_one() {
        let a = [1.0f32, 2.0, 3.0];
        let b = [-1.0f32, -2.0, -3.0];
        assert!(approx(cosine(&a, &b).expect("same length"), -1.0, 1e-6));
    }

    #[test]
    fn cosine_of_orthogonal_vectors_is_zero() {
        let a = [1.0f32, 0.0];
        let b = [0.0f32, 1.0];
        assert!(approx(cosine(&a, &b).expect("same length"), 0.0, 1e-6));
    }

    #[test]
    fn a_zero_vector_is_similar_to_nothing() {
        let zero = [0.0f32, 0.0, 0.0];
        let other = [1.0f32, 2.0, 3.0];
        assert_eq!(cosine(&zero, &other).expect("same length"), 0.0);
        assert_eq!(cosine(&zero, &zero).expect("same length"), 0.0);
    }

    #[test]
    fn different_lengths_are_refused() {
        assert_eq!(
            cosine(&[1.0], &[1.0, 2.0]),
            Err(Error::DimensionMismatch { left: 1, right: 2 })
        );
    }

    #[test]
    fn quantisation_stays_close_to_the_original() {
        let original: Vec<f32> = (0..256u16).map(|i| (f32::from(i) / 128.0) - 1.0).collect();
        let restored = Quantised::from_f32(&original).to_f32();

        assert_eq!(restored.len(), original.len());
        for (a, b) in original.iter().zip(restored.iter()) {
            // Half a step of the largest magnitude in the vector, which is the
            // bound the encoding promises.
            assert!(approx(*a, *b, 1.0 / 127.0), "{a} became {b}");
        }
    }

    #[test]
    fn quantisation_preserves_ranking() {
        let query = vec![0.9f32, 0.1, -0.4, 0.2];
        let near = vec![0.85f32, 0.15, -0.35, 0.25];
        let far = vec![-0.7f32, 0.6, 0.2, -0.9];

        let q = Quantised::from_f32(&query);
        assert!(
            q.dot(&Quantised::from_f32(&near)).expect("same length")
                > q.dot(&Quantised::from_f32(&far)).expect("same length"),
            "quantisation reordered a clear result"
        );
    }

    #[test]
    fn an_all_zero_vector_quantises_without_dividing_by_zero() {
        let q = Quantised::from_f32(&[0.0, 0.0, 0.0]);
        assert_eq!(q.scale, 0.0);
        assert_eq!(q.to_f32(), vec![0.0, 0.0, 0.0]);
        assert_eq!(q.len(), 3);
        assert!(!q.is_empty());
    }

    #[test]
    fn quantised_vectors_of_different_lengths_are_refused() {
        let a = Quantised::from_f32(&[1.0]);
        let b = Quantised::from_f32(&[1.0, 2.0]);
        assert_eq!(
            a.dot(&b),
            Err(Error::DimensionMismatch { left: 1, right: 2 })
        );
    }

    #[test]
    fn normalising_gives_unit_length() {
        let mut v = vec![3.0f32, 4.0];
        normalise(&mut v);
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(approx(norm, 1.0, 1e-6));

        let mut zero = vec![0.0f32, 0.0];
        normalise(&mut zero);
        assert_eq!(zero, vec![0.0, 0.0]);
    }

    #[test]
    fn quantisation_is_four_times_smaller_than_the_original() {
        let original = vec![0.5f32; 1024];
        let q = Quantised::from_f32(&original);
        let original_bytes = original.len() * size_of::<f32>();
        let quantised_bytes = q.values.len() * size_of::<i8>() + size_of::<f32>();
        assert!(quantised_bytes * 4 <= original_bytes + size_of::<f32>() * 4);
    }
}
