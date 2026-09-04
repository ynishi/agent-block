//! `knl` — kernel core (K1 History / K4 Budget / K5 Session).
//!
//! Pure Rust: nothing here knows about Lua.  Events are plain
//! `serde_json` objects, so the Lua ⇄ JSON conversion — and the
//! re-entrancy discipline that comes with walking a Lua table — stays in
//! the [`crate::bridge::knl`] adapter, one place, while the domain rules
//! below stay unit-testable without a VM.  This is the layering the
//! kernel/shell base design (D1) asks for.
//!
//! The invariants the kernel owns:
//!
//! - **I1 append-only.**  [`History`] has no mutation API — no `update`,
//!   `delete` or `replace`.  `seq` is assigned by the kernel, starts at
//!   `1` and increases strictly; a caller-supplied `seq` / `epoch_ms` is
//!   overwritten rather than trusted.  Reads hand back clones, so a caller
//!   cannot reach recorded state through a returned value.
//! - **Scope is the session.**  There is no per-event author.  A session
//!   holds only its own events, so ownership is the session-level
//!   [`Session::owner`] — a real principal id, or the reserved
//!   [`session::ANON`] / [`session::SYSTEM`] — total and read by the policy
//!   layer above the kernel.  The accounting keys on the `kind`: the
//!   `usage` view folds over every `model_response`, because in a
//!   session's own log a `model_response` is a call it made.  Appending
//!   one records and numbers it; appending a `run_finished` records a line
//!   without ending a run that only [`Session::close`] can end.
//! - **I3 budget monotonicity.**  [`Budget`] accepts non-negative amounts
//!   only, and within a run the balance can only decrease — it rises only
//!   when an owner grants again, which a resumed run records like any
//!   other fact.  It is a quota, not accounting: the decision is taken
//!   *before* the spending ([`Session::reserve`], which refuses without
//!   deducting) and settled after ([`Session::spend`]), by the layer that
//!   knows what a call costs.  No `append` moves it, and the `usage` view
//!   is the independent reading of what was consumed.
//! - **The balance is a fold.**  Every move is a `budget_*` event first,
//!   written by the kernel alone ([`is_kernel_only`]), so the counter is a
//!   cache of the log and [`fold_balance`] recovers it — which is how
//!   [`Session::resume`] restores a reopened run's remaining balance.
//! - **I6 run scope.**  All state lives inside a [`Session`] value — no
//!   statics — so two sessions are fully independent, and `close` ends
//!   the run scope (later `append` / `spend` are errors).
//! - **Turn numbering.**  The turn a `model_response` carries is the
//!   kernel's own count of the responses recorded, assigned on
//!   [`Session::append`] (like `seq`), so a loop cannot restart or forge
//!   it.
//!
//! Projections ([`projection`]) are *derived*: folding never changes the
//! history and a fold result is a cache, not a capture — it can always be
//! recomputed from the events.  The kernel names only the folds whose
//! consumer is fixed in its own terms (`usage`, `tail`); one whose shape
//! is a caller's decision is built on the shell side from
//! [`Session::events`].

pub mod budget;
pub mod event;
pub mod event_store;
pub mod history;
pub mod projection;
pub mod session;
#[cfg(feature = "sqlite")]
pub mod sqlite_store;

use std::fmt;

pub use budget::{fold_balance, Budget, BudgetGrant};
pub use event::{is_kernel_only, now_ms, validate_event, FIELD_EPOCH_MS, FIELD_KIND, FIELD_SEQ};
pub use event_store::{
    apply_upcasters, Committed, EventStore, MemEventStore, Upcaster, UpcastingEventStore,
    CURRENT_SCHEMA_VERSION, SCHEMA_VERSION_FIELD,
};
pub use history::History;
pub use projection::{UsageFold, Views};
pub use session::{
    Session, ANON, CLOSE_REASON_DROPPED, CLOSE_REASON_ERROR, CLOSE_REASON_SCOPE_EXIT,
    DEFAULT_CLOSE_REASON, SYSTEM,
};
#[cfg(feature = "sqlite")]
pub use sqlite_store::SqliteEventStore;

/// Failure reason produced by the kernel core.
///
/// The core does not know which Lua method the caller invoked, so the
/// message carries the reason only; the adapter renders the
/// `knl: <method>: <reason>` attribution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnlError(String);

impl KnlError {
    /// Build an error from a human-readable reason.
    pub fn new(reason: impl Into<String>) -> Self {
        Self(reason.into())
    }

    /// The reason, without any attribution prefix.
    pub fn reason(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for KnlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for KnlError {}

/// Result alias for the kernel core.
pub type KnlResult<T> = Result<T, KnlError>;
