//! The run scope: whose run this is, and what it was allowed to spend.
//!
//! A scope and a session are different things that share one lifetime.  The
//! session is the *stream*: one append-only history, its projections, its
//! turn numbering.  The scope is the *authority* the run holds while that
//! stream is written — a kernel-issued identity, the principal it belongs
//! to, and the quota an owner granted it.  Both begin when a run opens and
//! end when it closes, which is why [`super::Session`] holds a `Scope` by
//! value rather than pointing at one: there is no scope without the run,
//! and no run without a scope.
//!
//! Keeping the two apart is what makes the boundary readable in the log.
//! The session id names the stream a reader reopens; a [`ScopeId`] names
//! the authority the events were written under, and it is recorded — on
//! `run_started`, and on every `budget_*` event the ledger is folded from —
//! so the boundary is recoverable from the log alone.  The kernel issues
//! it (a fresh UUID v4, like the session id) and there is no API to set
//! one: an id a caller could choose is an authority a caller could claim.

use super::budget::BudgetGrant;
use super::{Budget, KnlResult};

/// The identity of a run scope: a kernel-issued UUID v4 string.
///
/// A `String` with a name, so the field it lands in says what it is.  There
/// is no constructor a caller can reach — [`Scope::new`] mints one, and a
/// resume takes what the log recorded.
pub type ScopeId = String;

/// Mint a fresh scope id.  The only place one is created.
fn mint_id() -> ScopeId {
    uuid::Uuid::new_v4().to_string()
}

/// One run scope: an identity, a principal, and the quota it was granted.
///
/// Read through [`super::Session::scope`].  The counter moves only through
/// the session's [`reserve`] / [`spend`], which record the move as an event
/// before it happens, so the scope's balance is never ahead of the log.
///
/// [`reserve`]: super::Session::reserve
/// [`spend`]: super::Session::spend
#[derive(Debug)]
pub struct Scope {
    /// Kernel-issued, recorded on `run_started` and every `budget_*` event.
    id: ScopeId,
    /// Whose scope this is: a real principal id, or the reserved
    /// [`super::ANON`] / [`super::SYSTEM`].  Total — never absent.
    owner: String,
    /// K4 budget counter: a cache of the `budget_*` ledger.
    budget: Budget,
    /// The grant this scope opened (or resumed) with, kept for its words: a
    /// refused reservation hands the `tag` back so a caller can say which
    /// allowance stopped it.  `None` when the run has no budget.
    grant: Option<BudgetGrant>,
}

impl Scope {
    /// Open a scope for `owner` with an optional `grant`, under a fresh
    /// kernel-issued id.
    pub fn new(owner: String, grant: Option<BudgetGrant>) -> Self {
        Self {
            id: mint_id(),
            owner,
            budget: Budget::new(grant.as_ref().map(|g| g.amount)),
            grant,
        }
    }

    /// Restore the scope a log records: `id` and `owner` as they were
    /// written, `balance` as the ledger folds to, `grant` as the last
    /// `budget_granted` said.
    ///
    /// `id` is `None` for a log written before the scope id was recorded,
    /// and a fresh one is issued rather than the resume failing — the same
    /// shape of fallback as an ownerless `run_started` resuming as
    /// [`super::ANON`], and for the same reason: a log that predates a field
    /// is still a run.
    pub(super) fn restore(
        id: Option<ScopeId>,
        owner: String,
        balance: Option<i64>,
        grant: Option<BudgetGrant>,
    ) -> Self {
        Self {
            id: id.unwrap_or_else(mint_id),
            owner,
            budget: Budget::new(balance),
            grant,
        }
    }

    /// The kernel-issued scope id.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Whose scope this is (a principal id, or [`super::ANON`] /
    /// [`super::SYSTEM`]).
    pub fn owner(&self) -> &str {
        &self.owner
    }

    /// The grant this scope opened (or resumed) with, if any.
    pub fn grant(&self) -> Option<&BudgetGrant> {
        self.grant.as_ref()
    }

    /// The remaining balance (`None` without a budget).
    pub fn remaining(&self) -> Option<i64> {
        self.budget.remaining()
    }

    /// Whether the budget is used up (never true without a budget).
    pub fn exhausted(&self) -> bool {
        self.budget.exhausted()
    }

    /// Take `amount` off the balance, or refuse without moving it.
    pub(super) fn reserve(&mut self, amount: i64) -> KnlResult<bool> {
        self.budget.reserve(amount)
    }

    /// Settle `amount`, returning the new balance (`None` without a budget).
    pub(super) fn spend(&mut self, amount: i64) -> KnlResult<Option<i64>> {
        self.budget.spend(amount)
    }

    /// The owner granting again: raise the balance by `grant.amount` and
    /// take its words as the scope's.
    pub(super) fn grant_more(&mut self, grant: BudgetGrant) -> KnlResult<()> {
        self.budget.grant(grant.amount)?;
        self.grant = Some(grant);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::knl::ANON;

    /// The id is the kernel's: every scope gets its own, and it is a real
    /// (non-empty) string before anything is recorded.
    #[test]
    fn every_scope_is_issued_its_own_id() {
        let a = Scope::new(ANON.to_string(), None);
        let b = Scope::new(ANON.to_string(), None);
        assert!(!a.id().is_empty());
        assert_ne!(a.id(), b.id(), "scope ids must be unique");
        assert_eq!(a.owner(), ANON);
        assert_eq!(a.remaining(), None, "no grant, no balance");
    }

    /// A restored scope keeps the id the log recorded; a log with none gets
    /// a fresh kernel-issued one rather than an empty or absent id.
    #[test]
    fn restore_keeps_the_recorded_id_and_issues_one_when_there_is_none() {
        let kept = Scope::restore(
            Some("scope-from-the-log".to_string()),
            "user-1".to_string(),
            Some(40),
            Some(BudgetGrant::new(100)),
        );
        assert_eq!(kept.id(), "scope-from-the-log");
        assert_eq!(kept.owner(), "user-1");
        assert_eq!(kept.remaining(), Some(40), "the balance is the fold's");

        let minted = Scope::restore(None, ANON.to_string(), None, None);
        assert!(
            !minted.id().is_empty(),
            "an older log still resumes under a scope id"
        );
        assert_ne!(minted.id(), kept.id());
    }

    /// The counter moves the way the budget does, and a second grant raises
    /// it and replaces the words a refusal reports.
    #[test]
    fn the_scope_counter_reserves_spends_and_takes_a_second_grant() {
        let mut scope = Scope::new(
            "user-2".to_string(),
            Some(BudgetGrant {
                amount: 100,
                tag: Some("tokens".to_string()),
                desc: None,
            }),
        );
        assert_eq!(scope.reserve(30), Ok(true));
        assert_eq!(scope.remaining(), Some(70));
        assert_eq!(scope.reserve(1_000), Ok(false), "a refusal moves nothing");
        assert_eq!(scope.remaining(), Some(70));
        assert_eq!(scope.spend(70), Ok(Some(0)));
        assert!(scope.exhausted());

        scope
            .grant_more(BudgetGrant {
                amount: 5,
                tag: Some("calls".to_string()),
                desc: None,
            })
            .expect("a second grant");
        assert_eq!(scope.remaining(), Some(5));
        assert!(!scope.exhausted());
        assert_eq!(scope.grant().and_then(|g| g.tag.as_deref()), Some("calls"));
    }
}
