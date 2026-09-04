//! The scope: whose session this is, and what it was allowed to spend.
//!
//! A scope and a session are different things that share one lifetime.  The
//! session is the *stream*: one append-only history and its projections.
//! The scope is the *authority* held while that stream is written — a
//! kernel-issued identity, the principal it belongs to, and the quota an
//! owner granted it.  Both begin when the session opens and end when it
//! closes, which is why [`super::Session`] holds a `Scope` by value rather
//! than pointing at one: there is no scope without the session, and no
//! session without a scope.
//!
//! Keeping the two apart is what makes the boundary readable in the log.
//! The session id names the stream a reader reopens; a [`ScopeId`] names
//! the authority the events were written under, and it is recorded — on
//! `session_opened`, and on every `budget_*` event the ledger is folded
//! from — so the boundary is recoverable from the log alone.  The kernel
//! issues it (a fresh UUID v4, like the session id) and there is no API to
//! set one: an id a caller could choose is an authority a caller could
//! claim.

use super::budget::{check_amount, BudgetGrant};
use super::KnlResult;

/// The identity of a scope: a kernel-issued UUID v4 string.
///
/// A `String` with a name, so the field it lands in says what it is.  There
/// is no constructor a caller can reach — [`Scope::new`] mints one, and a
/// resume takes what the log recorded.
pub type ScopeId = String;

/// Mint a fresh scope id.  The only place one is created.
fn mint_id() -> ScopeId {
    uuid::Uuid::new_v4().to_string()
}

/// One scope: an identity, a principal, and the grant it was opened under.
///
/// Read through [`super::Session::scope`].  It holds no balance: the balance
/// is [`super::fold_balance`] over the ledger, decided inside the store for a
/// [`reserve`] and read back from the log for everything else
/// ([`super::Session::remaining`]).  A number kept here would be a second
/// answer to that question, and the wrong one on a stream more than one
/// handle writes to.  What the scope keeps of the grant is its *words* — the
/// `tag` a refusal reports, the `desc` an audit reads.
///
/// [`reserve`]: super::Session::reserve
#[derive(Debug)]
pub struct Scope {
    /// Kernel-issued, recorded on `session_opened` and every `budget_*`
    /// event.
    id: ScopeId,
    /// Whose scope this is: a real principal id, or the reserved
    /// [`super::ANON`] / [`super::SYSTEM`].  Total — never absent.
    owner: String,
    /// The grant this scope opened (or resumed) with, kept for its words: a
    /// refused reservation hands the `tag` back so a caller can say which
    /// allowance stopped it, and its presence is what says this session keeps
    /// a ledger at all.  `None` when the session has no budget.
    grant: Option<BudgetGrant>,
}

impl Scope {
    /// Open a scope for `owner` with an optional `grant`, under a fresh
    /// kernel-issued id.
    pub fn new(owner: String, grant: Option<BudgetGrant>) -> Self {
        Self {
            id: mint_id(),
            owner,
            grant,
        }
    }

    /// Restore the scope a log records: `id` and `owner` as they were
    /// written, `grant` as the last `budget_granted` said.
    ///
    /// No balance is handed over, because none is held: what is left is
    /// [`super::fold_balance`] over the ledger, read when it is asked for.
    ///
    /// `id` is `None` for a log written before the scope id was recorded,
    /// and a fresh one is issued rather than the resume failing — the same
    /// shape of fallback as an ownerless `session_opened` resuming as
    /// [`super::ANON`], and for the same reason: a log that predates a field
    /// is still a session.
    pub(super) fn restore(id: Option<ScopeId>, owner: String, grant: Option<BudgetGrant>) -> Self {
        Self {
            id: id.unwrap_or_else(mint_id),
            owner,
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

    /// The owner granting again: take the new grant's words as the scope's.
    ///
    /// The balance it raises is the ledger's business — the
    /// `budget_granted` event is already in the log by the time this runs
    /// ([`super::Session::grant_more`]) — so all that is left here is the
    /// amount check and the words a later refusal will report.
    pub(super) fn grant_more(&mut self, grant: BudgetGrant) -> KnlResult<()> {
        check_amount(grant.amount)?;
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
        assert_eq!(a.grant(), None, "no grant, no ledger");
    }

    /// A restored scope keeps the id the log recorded; a log with none gets
    /// a fresh kernel-issued one rather than an empty or absent id.
    #[test]
    fn restore_keeps_the_recorded_id_and_issues_one_when_there_is_none() {
        let kept = Scope::restore(
            Some("scope-from-the-log".to_string()),
            "user-1".to_string(),
            Some(BudgetGrant::new(100)),
        );
        assert_eq!(kept.id(), "scope-from-the-log");
        assert_eq!(kept.owner(), "user-1");
        assert_eq!(
            kept.grant().map(|g| g.amount),
            Some(100),
            "the grant the log recorded comes back"
        );

        let minted = Scope::restore(None, ANON.to_string(), None);
        assert!(
            !minted.id().is_empty(),
            "an older log still resumes under a scope id"
        );
        assert_ne!(minted.id(), kept.id());
    }

    /// A second grant replaces the words a refusal reports, and refuses a
    /// negative amount.  It moves no balance here: the balance is the
    /// ledger's, and the `budget_granted` event is what raised it.
    #[test]
    fn a_second_grant_replaces_the_words_and_refuses_a_negative_amount() {
        let mut scope = Scope::new(
            "user-2".to_string(),
            Some(BudgetGrant {
                amount: 100,
                tag: Some("tokens".to_string()),
                desc: None,
            }),
        );
        assert_eq!(scope.grant().and_then(|g| g.tag.as_deref()), Some("tokens"));

        scope
            .grant_more(BudgetGrant {
                amount: 5,
                tag: Some("calls".to_string()),
                desc: None,
            })
            .expect("a second grant");
        assert_eq!(scope.grant().and_then(|g| g.tag.as_deref()), Some("calls"));
        assert_eq!(scope.grant().map(|g| g.amount), Some(5));

        let err = scope
            .grant_more(BudgetGrant::new(-1))
            .expect_err("a negative grant");
        assert!(err.reason().contains("non-negative"), "{err}");
        assert_eq!(
            scope.grant().and_then(|g| g.tag.as_deref()),
            Some("calls"),
            "a refused grant leaves the words as they were"
        );
    }
}
