//! K4 — the budget counter.
//!
//! v1 is single-axis (`tokens`).  Multiple axes (turns / cost / time) fit
//! later as a map of named counters — `spend(n)` gaining a `spend(axis,
//! n)` form — so nothing here needs to be taken back to get there.  The
//! axis question is deferred until usage actually flows through the
//! kernel, rather than guessed now.

use super::{KnlError, KnlResult};

/// A monotonically decreasing balance, or no budget at all.
#[derive(Debug, Clone, Default)]
pub struct Budget {
    /// Remaining balance; `None` when the session was created without one.
    remaining: Option<i64>,
}

impl Budget {
    /// A budget with `tokens` remaining, or an unlimited one for `None`.
    ///
    /// A negative starting balance is floored at `0`: the balance is
    /// defined to be non-negative, and callers validate their own input
    /// before it reaches here.
    pub fn new(tokens: Option<i64>) -> Self {
        Self {
            remaining: tokens.map(|t| t.max(0)),
        }
    }

    /// Deduct `amount`, returning the new balance (`None` without a
    /// budget).
    ///
    /// The balance never rises and is floored at `0`; a negative amount
    /// is an error and leaves the balance untouched.
    pub fn spend(&mut self, amount: i64) -> KnlResult<Option<i64>> {
        if amount < 0 {
            return Err(KnlError::new(format!(
                "amount must be a non-negative whole number, got {amount}"
            )));
        }
        let Some(remaining) = self.remaining.as_mut() else {
            return Ok(None);
        };
        *remaining = remaining.saturating_sub(amount).max(0);
        Ok(Some(*remaining))
    }

    /// The remaining balance (`None` without a budget).
    pub fn remaining(&self) -> Option<i64> {
        self.remaining
    }

    /// Whether the budget is used up (never true without a budget).
    pub fn exhausted(&self) -> bool {
        matches!(self.remaining, Some(r) if r <= 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spend_is_monotonic_and_floors_at_zero() {
        let mut b = Budget::new(Some(1000));
        assert_eq!(b.remaining(), Some(1000));
        assert!(!b.exhausted());

        let mut prev = 1000;
        for amount in [120, 0, 300, 80] {
            let now = b.spend(amount).expect("spend").expect("has a budget");
            assert!(now <= prev, "balance rose: {prev} -> {now}");
            prev = now;
        }
        assert_eq!(b.remaining(), Some(500));

        assert_eq!(b.spend(9999), Ok(Some(0)), "overspending floors at zero");
        assert!(b.exhausted());
        assert_eq!(b.spend(1), Ok(Some(0)), "spending past zero stays at zero");
    }

    #[test]
    fn a_negative_amount_is_rejected_and_changes_nothing() {
        let mut b = Budget::new(Some(100));
        let err = b.spend(-1).expect_err("negative spend");
        assert!(err.reason().contains("non-negative"), "{err}");
        assert_eq!(b.remaining(), Some(100));

        // Rejected without a budget too — the rule is about the amount.
        let mut none = Budget::new(None);
        none.spend(-1).expect_err("negative spend without a budget");
    }

    #[test]
    fn without_a_budget_everything_is_nil_and_never_exhausted() {
        let mut b = Budget::new(None);
        assert_eq!(b.remaining(), None);
        assert_eq!(b.spend(50), Ok(None));
        assert!(!b.exhausted());
    }

    #[test]
    fn a_negative_starting_balance_is_floored() {
        let b = Budget::new(Some(-5));
        assert_eq!(b.remaining(), Some(0));
        assert!(b.exhausted());
    }

    #[test]
    fn a_huge_amount_cannot_wrap_the_balance() {
        let mut b = Budget::new(Some(10));
        assert_eq!(b.spend(i64::MAX), Ok(Some(0)));
        assert_eq!(b.remaining(), Some(0));
    }
}
