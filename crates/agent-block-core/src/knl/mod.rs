//! `knl` — kernel core (K1 History / K4 Budget ledger / K5 Session).
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
//! - **An append lands; a command decides inside the store.**  Recording a
//!   fact is never refused for what the writing handle last saw: the store
//!   assigns the `seq` and serializes writes per stream, so two handles on
//!   one stream both append and the log interleaves in arrival order.  The
//!   one place a check belongs — "reserve `n` only if the balance covers
//!   it" — runs inside that same serialized write
//!   ([`EventStore::append_if`]), never against a cached balance.
//! - **A session is disposable.**  It opens once and closes once, and
//!   [`Session::resume`] refuses a stream whose `session_closed` is already
//!   in the log: after an ending there is a new session, not a second life
//!   for the old one.
//! - **Stored shape change ⇒ upcaster, from the first release on.**  Stored
//!   bytes are never rewritten.  A change to the shape of a stored event
//!   ships with a bump of [`CURRENT_SCHEMA_VERSION`] and the matching
//!   read-time [`Upcaster`] ([`kernel_upcasters`]), which every session's
//!   reads pass through.  Nothing has been released yet, so the chain is
//!   empty and the version is `1`: the seam is in place and tested, and the
//!   first step that is owed has one site to be registered at.
//! - **A session has a scope.**  The two are different concepts sharing one
//!   lifetime: the session is the stream (history, projections), the
//!   [`Scope`] is the authority it is written under (a kernel-issued
//!   [`ScopeId`], the owner, the granted quota).  A session holds its scope
//!   by value, since neither outlives the other.  The scope id is recorded
//!   on `session_opened` and on every `budget_*` event, so the boundary is
//!   recoverable from the log — and unforgeable, since those kinds are the
//!   kernel's alone to write.  There is no per-event author: a session
//!   holds only its own events, so ownership is the scope-level
//!   [`Session::owner`] — a real principal id, or the reserved
//!   [`session::ANON`] / [`session::SYSTEM`] — total and read by the policy
//!   layer above the kernel.  The accounting keys on the `kind`: the
//!   `usage` view folds over every `llm_response`, because in a session's
//!   own log an `llm_response` is a call it made.
//! - **I3 budget monotonicity.**  The ledger accepts non-negative amounts
//!   only, and within a session the balance can only decrease — it rises
//!   only when an owner grants again, which a resumed session records like
//!   any other fact.  It is a quota, not accounting: the decision is taken
//!   *before* the spending ([`Session::reserve`], which refuses without
//!   deducting) and settled after ([`Session::spend`]), by the layer that
//!   knows what a call costs.  No `append` moves it, and the `usage` view
//!   is the independent reading of what was consumed.
//! - **The balance is a fold, and only a fold.**  Every move is a `budget_*`
//!   event first, written by the kernel alone ([`is_kernel_only`]), and
//!   [`fold_balance`] over those events *is* the balance — there is no
//!   counter beside them.  [`Session::remaining`] reads it back off the
//!   stream (cached against the store's head, refolded when the head moves),
//!   so two handles on one stream cannot hold two different answers.
//! - **I6 session lifecycle.**  All state lives inside a [`Session`] value
//!   — no statics — so two sessions are fully independent.  There is no
//!   "run" inside a session: the lifecycle is the session's own, bracketed
//!   by the `session_opened` that [`Session::open_on`] records and the
//!   `session_closed` that [`Session::close`] records.  Both are
//!   kernel-only ([`is_kernel_only`]), so a caller can neither fake an
//!   opening nor end a session by appending an event.  Closed is the
//!   handle's state, not the stream's: a handle that closed refuses its own
//!   later `append` / `spend`, while the log itself never refuses a write
//!   (a write arriving after another handle's ending lands, as evidence),
//!   and `resume` is the only reader of `session_closed`.
//! - **Beats are declared, not numbered.**  A `beat` is an opaque string
//!   the layer above mints and stamps on the facts that belong together.
//!   The kernel never generates one and never requires one; it only
//!   insists that a present `beat` is a string.
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
pub mod scope;
pub mod session;
#[cfg(feature = "sqlite")]
pub mod sqlite_store;

use std::fmt;

pub use budget::{fold_balance, BudgetGrant};
pub use event::{is_kernel_only, now_ms, validate_event, FIELD_EPOCH_MS, FIELD_KIND, FIELD_SEQ};
pub use event_store::{
    apply_upcasters, kernel_upcasters, Committed, Decision, EventStore, MemEventStore, Upcaster,
    UpcastingEventStore, CURRENT_SCHEMA_VERSION, SCHEMA_VERSION_FIELD,
};
pub use history::History;
pub use projection::{UsageFold, Views};
pub use scope::{Scope, ScopeId};
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
