//! K4 — the budget: a quota an owner grants a scope.
//!
//! The quota is what makes a run stop: termination is undecidable, so the
//! owner injects a resource that only decreases and the run ends when it
//! runs out (`ulimit` / cgroup semantics).  The decision is taken *before*
//! the spending, by [`super::Session::reserve`] — asking whether `n` may be
//! consumed — and [`super::Session::spend`] settles afterwards.
//!
//! # The balance is the ledger, and nothing else
//!
//! There is no counter.  Every move of the balance is an event first — a
//! `budget_granted` / `budget_reserved` / `budget_spent`
//! ([`super::event`]) — and the balance *is* [`fold_balance`] over them.
//! A reservation is a command with an invariant, so it is decided inside
//! the store against the ledger as it stands there
//! ([`super::EventStore::append_if`]); a *read* of the balance
//! ([`super::Session::remaining`]) folds that same ledger.  A number kept
//! beside the log would be a second answer to one question, and on a stream
//! two handles write to it would be the wrong one.
//!
//! It is not accounting.  What a run actually consumed is the `usage`
//! projection over the fact-log ([`super::projection`]); the two are
//! independent, and an estimate that missed does not make either wrong.
//!
//! The fold knows only numbers.  It has no idea what an `llm_response` is,
//! what a beat is, or what unit the amount is in — the unit lives in the
//! grant's `tag` ([`BudgetGrant`]), for whoever reads the log.
//!
//! v1 is single-axis.  Multiple axes (turns / cost / time) fit later as a
//! map of named balances — `reserve(n)` / `spend(n)` gaining an `(axis, n)`
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
/// One rule for `reserve`, `spend` and a grant: the balance moves by whole
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
/// The balance itself, and the only definition of one.  `None` means no
/// grant was ever recorded, which is not the same as a balance of zero: a
/// run with no budget refuses nothing, a run whose balance reached zero
/// refuses everything.
///
/// Applied in order, floored at zero at each step: a run that overspent past
/// zero and was granted again starts from zero, not from a debt the floor
/// had already forgiven.  `budget_refused` moves nothing, which is the point
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
/// a refused [`super::Session::reserve`] at the call site, so a caller can
/// say which allowance stopped it without reading the log at all.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A negative amount is a refund by another name, and is refused before
    /// anything is touched — whether or not there is a budget, because the
    /// rule is about the amount.
    #[test]
    fn a_negative_amount_is_refused() {
        let err = check_amount(-1).expect_err("a negative amount");
        assert!(err.reason().contains("non-negative"), "{err}");
        assert_eq!(check_amount(0), Ok(()));
        assert_eq!(check_amount(i64::MAX), Ok(()));
    }

    /// The fold is the balance, and the only definition of one: every kind
    /// of move applied in order, floored at zero, with a refusal moving
    /// nothing.
    #[test]
    fn the_fold_of_the_ledger_is_the_balance() {
        use serde_json::json;

        let mut log = vec![json!({ "kind": "budget_granted", "amount": 100 })];
        assert_eq!(fold_balance(&log), Some(100));

        // A reservation of 30 was decided in the store and recorded there.
        log.push(json!({ "kind": "budget_reserved", "amount": 30 }));
        assert_eq!(fold_balance(&log), Some(70), "after a reservation");

        // A refusal is recorded and moves nothing.
        log.push(json!({ "kind": "budget_refused", "amount": 1000, "remaining": 70 }));
        assert_eq!(fold_balance(&log), Some(70), "after a refusal");

        log.push(json!({ "kind": "budget_spent", "amount": 20 }));
        assert_eq!(fold_balance(&log), Some(50), "after a settlement");

        // Overspending floors at zero rather than going into debt, and a
        // huge amount cannot wrap it…
        log.push(json!({ "kind": "budget_spent", "amount": i64::MAX }));
        assert_eq!(fold_balance(&log), Some(0), "at the floor");

        // …so a later grant starts from zero, not from a forgiven debt.
        log.push(json!({ "kind": "budget_granted", "amount": 10 }));
        assert_eq!(fold_balance(&log), Some(10), "after a re-grant");
    }

    /// No grant in the log is no budget — which is not a balance of zero:
    /// one refuses nothing, the other refuses everything.
    #[test]
    fn a_log_without_a_grant_folds_to_no_budget() {
        use serde_json::json;

        assert_eq!(fold_balance(&[]), None);
        assert_eq!(
            fold_balance(&[
                json!({ "kind": "session_opened" }),
                json!({ "kind": "llm_response", "usage": { "input_tokens": 500 } }),
            ]),
            None,
            "a provider response is not a budget move"
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
        assert_eq!(tagged.amount, 10);
        assert_eq!(tagged.tag.as_deref(), Some("tokens"));
        assert_eq!(tagged.desc.as_deref(), Some("one turn's worth"));
    }
}
