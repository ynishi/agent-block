//! K4 — the budget counter: a quota an owner grants a scope.
//!
//! The counter is what makes a run stop: termination is undecidable, so
//! the owner injects a resource that only decreases and the run ends when
//! it runs out (`ulimit` / cgroup semantics).  The decision is taken
//! *before* the spending, by [`Budget::reserve`] — asking whether `n` may
//! be consumed — and [`Budget::spend`] settles afterwards.
//!
//! It is not accounting.  What a run actually consumed is the `usage`
//! projection over the fact-log ([`super::projection`]); the two are
//! independent, and an estimate that missed does not make either wrong.
//!
//! # The balance is a fold
//!
//! The counter below is the hot path, not the truth.  Every move it makes
//! is recorded first, as a `budget_granted` / `budget_reserved` /
//! `budget_spent` event ([`super::event`]), so the balance can be
//! recovered from the log alone by [`fold_balance`] — which is exactly
//! what [`super::Session::resume`] does.  The two must agree: the fold
//! applies the same arithmetic in the same order, so the invariant a test
//! can state is `fold_balance(log) == remaining()`, at any point.
//!
//! The counter knows only numbers.  It has no idea what a `model_response`
//! is, what a beat is, or what unit the amount is in — the unit lives in
//! the grant's `tag` ([`BudgetGrant`]), for whoever reads the log.
//!
//! v1 is single-axis.  Multiple axes (turns / cost / time) fit later as a
//! map of named counters — `reserve(n)` / `spend(n)` gaining an `(axis, n)`
//! form — so nothing here needs to be taken back to get there.

use serde_json::Value;

use super::event::{
    kind_of, FIELD_AMOUNT, FIELD_DESC, FIELD_TAG, KIND_BUDGET_GRANTED, KIND_BUDGET_RESERVED,
    KIND_BUDGET_SPENT,
};
use super::projection::whole;
use super::{KnlError, KnlResult};

/// Reject an amount the counter cannot take.
///
/// One rule for `reserve`, `spend` and a grant: the counter moves by whole
/// non-negative steps, so a negative amount is a refund by another name and
/// is refused before anything — the balance or the log — is touched.
pub(super) fn check_amount(amount: i64) -> KnlResult<()> {
    if amount < 0 {
        return Err(KnlError::new(format!(
            "amount must be a non-negative whole number, got {amount}"
        )));
    }
    Ok(())
}

/// The balance a log implies: `granted − reserved − spent`, in seq order.
///
/// The definition of the counter, as a fold — [`Budget`] is a cache of
/// this.  `None` means no grant was ever recorded, which is not the same
/// as a balance of zero: a run with no budget refuses nothing, a run whose
/// balance reached zero refuses everything.
///
/// Applied in order, with the same floor at each step as the live counter,
/// so the two cannot drift: a run that overspent past zero and was granted
/// again starts from what the counter said, not from a debt the floor had
/// already forgiven.  `budget_refused` moves nothing, which is the point
/// of recording it — the fact that a stop happened, with no effect on the
/// balance.
pub fn fold_balance(events: &[Value]) -> Option<i64> {
    let mut balance: Option<i64> = None;
    for event in events {
        let amount = event.get(FIELD_AMOUNT).map_or(0, whole).max(0);
        match kind_of(event) {
            KIND_BUDGET_GRANTED => {
                balance = Some(balance.unwrap_or(0).saturating_add(amount));
            }
            KIND_BUDGET_RESERVED | KIND_BUDGET_SPENT => {
                balance = balance.map(|b| b.saturating_sub(amount).max(0));
            }
            _ => {}
        }
    }
    balance
}

/// Recover the grant a log records, from its last `budget_granted`.
///
/// A resumed run keeps the words of the grant it is continuing (the `tag`
/// a refusal reports), and — more than cosmetically — keeps *having* a
/// budget: a session whose log says a quota was granted must go on
/// recording its moves, whether or not the resuming caller granted again.
pub fn last_grant(events: &[Value]) -> Option<BudgetGrant> {
    events
        .iter()
        .rev()
        .find(|event| kind_of(event) == KIND_BUDGET_GRANTED)
        .map(|event| BudgetGrant {
            amount: event.get(FIELD_AMOUNT).map_or(0, whole).max(0),
            tag: string_field(event, FIELD_TAG),
            desc: string_field(event, FIELD_DESC),
        })
}

/// An optional string payload field of a stored event.
fn string_field(event: &Value, field: &str) -> Option<String> {
    event.get(field).and_then(Value::as_str).map(str::to_string)
}

/// What an owner grants a scope: an amount, and the words for what it is.
///
/// The kernel reads `amount` and nothing else.  `tag` names the unit (the
/// shell writes `"tokens"`), and `desc` says who allowed what and why;
/// both ride onto the `budget_granted` event verbatim, so a log can be
/// audited without asking the shell what it meant.  `tag` comes back with
/// a refused [`Budget::reserve`] at the call site, so a caller can say
/// which allowance stopped it without reading the log at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetGrant {
    /// The quota, in whatever unit `tag` names.  Non-negative.
    pub amount: i64,
    /// The unit / identity of the grant, kernel-uninterpreted.
    pub tag: Option<String>,
    /// Free-text audit note, kernel-uninterpreted.
    pub desc: Option<String>,
}

impl BudgetGrant {
    /// A grant of `amount` with no tag or description.
    pub fn new(amount: i64) -> Self {
        Self {
            amount,
            tag: None,
            desc: None,
        }
    }
}

/// A monotonically decreasing balance, or no budget at all.
#[derive(Debug, Clone, Default)]
pub struct Budget {
    /// Remaining balance; `None` when the session was created without one.
    remaining: Option<i64>,
}

impl Budget {
    /// A budget with `amount` remaining, or an unlimited one for `None`.
    ///
    /// A negative starting balance is floored at `0`: the balance is
    /// defined to be non-negative, and callers validate their own input
    /// before it reaches here.
    pub fn new(amount: Option<i64>) -> Self {
        Self {
            remaining: amount.map(|t| t.max(0)),
        }
    }

    /// Ask to consume `n`: `true` when it was taken, `false` when the
    /// balance would not cover it.
    ///
    /// This is the decision point — the whole reason a budget exists.  A
    /// refusal is atomic: the balance is left exactly as it was, so a
    /// caller that stops on `false` has spent nothing on the attempt.
    /// Without a budget every request is granted (`true`) and there is no
    /// balance to move.
    ///
    /// A negative amount is an error, as it is for [`Budget::spend`]: the
    /// counter decreases, and a "reservation" that raised the balance
    /// would be a refund by another name.
    pub fn reserve(&mut self, n: i64) -> KnlResult<bool> {
        check_amount(n)?;
        let Some(remaining) = self.remaining.as_mut() else {
            return Ok(true);
        };
        if *remaining < n {
            return Ok(false);
        }
        *remaining -= n;
        Ok(true)
    }

    /// Settle `amount` after the fact, returning the new balance (`None`
    /// without a budget).
    ///
    /// The counterpart of [`Budget::reserve`]: the reservation was an
    /// estimate, and this is where the difference is paid.  Unlike
    /// `reserve` it never refuses — the consumption already happened — so
    /// the balance is floored at `0` rather than going negative.  A
    /// negative amount is an error and leaves the balance untouched.
    pub fn spend(&mut self, amount: i64) -> KnlResult<Option<i64>> {
        check_amount(amount)?;
        let Some(remaining) = self.remaining.as_mut() else {
            return Ok(None);
        };
        *remaining = remaining.saturating_sub(amount).max(0);
        Ok(Some(*remaining))
    }

    /// Raise the balance by `amount`: the owner granted again.
    ///
    /// The one way the balance rises, and it is not an exception to I3 but
    /// its boundary: a run cannot give itself more, an owner can give a
    /// resumed run more, and the giving is a recorded `budget_granted`
    /// event ([`super::Session::resume`]) rather than a counter that can
    /// be nudged.  Granting to a session that had no budget gives it one.
    pub fn grant(&mut self, amount: i64) -> KnlResult<()> {
        check_amount(amount)?;
        self.remaining = Some(self.remaining.unwrap_or(0).saturating_add(amount));
        Ok(())
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

    /// The decision point: a request the balance covers is taken, and the
    /// balance falls by exactly what was asked for.
    #[test]
    fn reserve_takes_what_the_balance_covers() {
        let mut b = Budget::new(Some(100));
        assert_eq!(b.reserve(40), Ok(true));
        assert_eq!(b.remaining(), Some(60));
        assert_eq!(b.reserve(60), Ok(true), "the exact balance is coverable");
        assert_eq!(b.remaining(), Some(0));
        assert!(b.exhausted());
        // Zero is always coverable, even at zero.
        assert_eq!(b.reserve(0), Ok(true));
        assert_eq!(b.remaining(), Some(0));
    }

    /// A refusal is atomic: the balance is exactly what it was, so a
    /// caller that stops on `false` has paid nothing for the attempt.
    #[test]
    fn a_refused_reserve_leaves_the_balance_untouched() {
        let mut b = Budget::new(Some(50));
        assert_eq!(b.reserve(51), Ok(false));
        assert_eq!(b.remaining(), Some(50), "a refusal must not deduct");
        assert!(!b.exhausted(), "a refusal must not exhaust the budget");

        // Still spendable afterwards: the refusal changed no state at all.
        assert_eq!(b.reserve(50), Ok(true));
        assert_eq!(b.remaining(), Some(0));
        assert_eq!(b.reserve(1), Ok(false));
        assert_eq!(b.remaining(), Some(0));
    }

    /// Without a budget there is nothing to refuse: every request is
    /// granted and no balance moves.
    #[test]
    fn without_a_budget_reserve_always_grants() {
        let mut b = Budget::new(None);
        assert_eq!(b.reserve(0), Ok(true));
        assert_eq!(b.reserve(i64::MAX), Ok(true));
        assert_eq!(b.remaining(), None);
        assert!(!b.exhausted());
    }

    /// The counter only decreases: a negative reservation is rejected
    /// rather than handing balance back, with or without a budget.
    #[test]
    fn a_negative_reserve_is_rejected_and_changes_nothing() {
        let mut b = Budget::new(Some(100));
        let err = b.reserve(-1).expect_err("negative reserve");
        assert!(err.reason().contains("non-negative"), "{err}");
        assert_eq!(b.remaining(), Some(100));

        let mut none = Budget::new(None);
        none.reserve(-1)
            .expect_err("negative reserve without a budget");
    }

    /// reserve and spend move the same counter in the same direction:
    /// across any interleaving the balance is non-increasing.
    #[test]
    fn reserve_and_spend_are_one_monotonic_counter() {
        let mut b = Budget::new(Some(1000));
        let mut prev = 1000;
        for (i, n) in [10, 300, 0, 90].into_iter().enumerate() {
            if i % 2 == 0 {
                assert_eq!(b.reserve(n), Ok(true));
            } else {
                b.spend(n).expect("spend");
            }
            let now = b.remaining().expect("has a budget");
            assert!(now <= prev, "balance rose: {prev} -> {now}");
            prev = now;
        }
        assert_eq!(b.remaining(), Some(600));
    }

    /// The fold is the definition and the counter is the cache: the same
    /// sequence of moves, applied both ways, lands on the same number.
    #[test]
    fn the_fold_of_the_ledger_is_the_balance() {
        use serde_json::json;

        let mut b = Budget::new(Some(100));
        let mut log = vec![json!({ "kind": "budget_granted", "amount": 100 })];
        assert_eq!(fold_balance(&log), b.remaining());

        assert_eq!(b.reserve(30), Ok(true));
        log.push(json!({ "kind": "budget_reserved", "amount": 30 }));
        assert_eq!(fold_balance(&log), b.remaining(), "after a reservation");

        // A refusal is recorded and moves nothing.
        assert_eq!(b.reserve(1_000), Ok(false));
        log.push(json!({ "kind": "budget_refused", "amount": 1000, "remaining": 70 }));
        assert_eq!(fold_balance(&log), b.remaining(), "after a refusal");

        b.spend(20).expect("spend");
        log.push(json!({ "kind": "budget_spent", "amount": 20 }));
        assert_eq!(fold_balance(&log), Some(50));
        assert_eq!(fold_balance(&log), b.remaining(), "after a settlement");

        // Overspending floors the same way on both sides…
        b.spend(9_999).expect("spend");
        log.push(json!({ "kind": "budget_spent", "amount": 9999 }));
        assert_eq!(fold_balance(&log), Some(0));
        assert_eq!(fold_balance(&log), b.remaining(), "at the floor");

        // …so a later grant starts from zero, not from a forgiven debt.
        b.grant(10).expect("grant");
        log.push(json!({ "kind": "budget_granted", "amount": 10 }));
        assert_eq!(fold_balance(&log), Some(10));
        assert_eq!(fold_balance(&log), b.remaining(), "after a re-grant");
    }

    /// No grant in the log is no budget — which is not a balance of zero:
    /// one refuses nothing, the other refuses everything.
    #[test]
    fn a_log_without_a_grant_folds_to_no_budget() {
        use serde_json::json;

        assert_eq!(fold_balance(&[]), None);
        assert_eq!(
            fold_balance(&[
                json!({ "kind": "run_started" }),
                json!({ "kind": "model_response", "usage": { "input_tokens": 500 } }),
            ]),
            None,
            "a model response is not a budget move"
        );
        assert_eq!(
            fold_balance(&[json!({ "kind": "budget_granted", "amount": 0 })]),
            Some(0),
            "a grant of zero is a budget, and an empty one"
        );
    }

    /// The tag a refusal reports survives a resume: it is read back off the
    /// last grant the log recorded.
    #[test]
    fn the_last_grant_is_recovered_from_the_log() {
        use serde_json::json;

        assert_eq!(last_grant(&[]), None);

        let log = vec![
            json!({ "kind": "budget_granted", "amount": 100, "tag": "tokens", "desc": "first" }),
            json!({ "kind": "budget_reserved", "amount": 10 }),
            json!({ "kind": "budget_granted", "amount": 50, "tag": "tokens", "desc": "second" }),
        ];
        let grant = last_grant(&log).expect("a grant was recorded");
        assert_eq!(grant.amount, 50, "the latest grant, not the first");
        assert_eq!(grant.tag.as_deref(), Some("tokens"));
        assert_eq!(grant.desc.as_deref(), Some("second"));

        // A grant with no words comes back with none invented.
        let bare = last_grant(&[json!({ "kind": "budget_granted", "amount": 7 })])
            .expect("a grant was recorded");
        assert_eq!(bare, BudgetGrant::new(7));
    }

    /// A grant is an amount plus words the kernel does not read.
    #[test]
    fn a_grant_carries_the_amount_and_its_words() {
        let plain = BudgetGrant::new(100);
        assert_eq!(plain.amount, 100);
        assert_eq!(plain.tag, None);
        assert_eq!(plain.desc, None);

        let tagged = BudgetGrant {
            amount: 10,
            tag: Some("tokens".to_string()),
            desc: Some("one turn's worth".to_string()),
        };
        assert_eq!(Budget::new(Some(tagged.amount)).remaining(), Some(10));
        assert_eq!(tagged.tag.as_deref(), Some("tokens"));
    }
}
