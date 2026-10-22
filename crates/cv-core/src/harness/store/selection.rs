//! An ordered index of positions into an inventory; payloads stay with their source owner.
use std::cmp::Ordering;

pub(crate) struct Positions(Vec<usize>);
impl Positions {
    pub(crate) fn new(len: usize, compare: impl Fn(usize, usize) -> Ordering) -> Self {
        let mut positions: Vec<_> = (0..len).collect();
        // Original ordinal breaks ties, preserving first-duplicate and file-order contracts.
        positions.sort_unstable_by(|&a, &b| compare(a, b).then(a.cmp(&b)));
        Self(positions)
    }
    /// Select positions after the `before` prefix and through the inclusive `through` prefix.
    /// Both predicates must be monotone true-then-false in sorted order, with before ⊆ through.
    pub(crate) fn range(&self, before: impl Fn(usize) -> bool, through: impl Fn(usize) -> bool) -> &[usize] {
        let start = self.0.partition_point(|&i| before(i));
        let end = self.0.partition_point(|&i| through(i));
        &self.0[start..end]
    }
    pub(crate) fn equal(&self, compare: impl Fn(usize) -> Ordering) -> &[usize] {
        self.range(|i| compare(i).is_lt(), |i| !compare(i).is_gt())
    }
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.0.len()
    }
}
