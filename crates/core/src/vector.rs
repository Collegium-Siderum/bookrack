// SPDX-License-Identifier: Apache-2.0

//! Whether an embedding vector can be stored and indexed.
//!
//! Every index the vector store builds measures cosine distance, and
//! LanceDB normalizes each training vector by its L2 norm before it
//! trains, dropping any vector the division leaves non-finite. A vector
//! with a non-finite component, a zero length, or a squared length that
//! underflows to zero is dropped there; one whose length overflows is
//! flattened to zero by the division. [`vector_defect`] computes the
//! norm the same way — squares summed in order in `f32`, then the
//! square root — and accepts only a finite, non-zero norm, so a vector
//! it accepts normalizes to finite components and is trained on.

/// What makes a vector unusable for cosine search.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VectorDefect {
    /// A component is NaN or infinite.
    NonFinite,
    /// The `f32` L2 norm is zero or not finite: an all-zero vector, one
    /// so small its squared length underflows, or one so large it
    /// overflows.
    DegenerateNorm,
}

impl VectorDefect {
    /// A phrase naming the defect, for an error's detail.
    pub fn describe(self) -> &'static str {
        match self {
            VectorDefect::NonFinite => "a component is NaN or infinite",
            VectorDefect::DegenerateNorm => "its length is zero or too large to normalize",
        }
    }
}

/// The defect that keeps `vector` out of the store, or `None` when it
/// can be stored and indexed.
pub fn vector_defect(vector: &[f32]) -> Option<VectorDefect> {
    if vector.iter().any(|x| !x.is_finite()) {
        return Some(VectorDefect::NonFinite);
    }
    let norm = vector.iter().map(|x| x.powi(2)).sum::<f32>().sqrt();
    (!norm.is_finite() || norm == 0.0).then_some(VectorDefect::DegenerateNorm)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_unit_vector_and_an_ordinary_embedding_are_usable() {
        assert_eq!(vector_defect(&[1.0, 0.0, 0.0]), None);
        assert_eq!(vector_defect(&[0.12, -0.5, 0.03, 0.9]), None);
    }

    #[test]
    fn a_non_finite_component_is_reported_before_the_norm() {
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert_eq!(
                vector_defect(&[0.5, bad, 0.5]),
                Some(VectorDefect::NonFinite),
                "{bad}"
            );
        }
    }

    #[test]
    fn a_zero_or_underflowing_length_is_degenerate() {
        assert_eq!(
            vector_defect(&[0.0, 0.0, 0.0]),
            Some(VectorDefect::DegenerateNorm)
        );
        // 1e-30 squared underflows f32 to zero, so the norm is zero
        // although no component is.
        assert_eq!(
            vector_defect(&[1e-30, 1e-30]),
            Some(VectorDefect::DegenerateNorm)
        );
        // 1e-20 squared is 1e-40, a subnormal: still a non-zero norm.
        assert_eq!(vector_defect(&[1e-20, 0.0]), None);
    }

    #[test]
    fn an_overflowing_length_is_degenerate() {
        // Each component is finite; their squares overflow f32.
        assert_eq!(
            vector_defect(&[1e20, 1e20]),
            Some(VectorDefect::DegenerateNorm)
        );
    }

    #[test]
    fn an_empty_vector_has_no_length() {
        assert_eq!(vector_defect(&[]), Some(VectorDefect::DegenerateNorm));
    }
}
