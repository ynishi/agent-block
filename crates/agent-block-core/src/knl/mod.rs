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
//!   first step that is owed has one site to be registered at.  The seam is
//!   a type: a backend deals in raw `Value`s, [`CurrentStore`] reads through
//!   the chain and hands back [`Current`]s, and every fold below takes those
//!   — so a read that went round the chain does not compile.
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

pub use budget::{fold_balance, BudgetGrant};
pub use event::{
    is_kernel_only, now_ms, validate_event, BUDGET_KINDS, FIELD_EPOCH_MS, FIELD_KIND, FIELD_SEQ,
};
pub use event_store::{
    apply_upcasters, kernel_upcasters, Committed, Current, CurrentDecision, CurrentStore, Decision,
    EventStore, MemEventStore, Upcaster, CURRENT_SCHEMA_VERSION, SCHEMA_VERSION_FIELD,
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

/// What went wrong in the kernel core, classified.
///
/// A failure is not one thing.  A contended database will succeed if it is
/// asked again; a row that will not decode never will.  A caller that passed
/// a negative amount has a bug in its own code; a caller that wrote to a
/// closed handle has finished with the session and needs a new one.  Folding
/// all four into one opaque string leaves every caller — the Lua shell most
/// of all — matching on message text to tell them apart, and message text is
/// the one part of an error that is meant to change.
///
/// So the variant *is* the classification, and it is the whole of it: the
/// payload is a human-readable sentence and nothing a caller should branch
/// on.  [`KnlError::kind`] names the class in one stable word, and
/// [`KnlError::is_retryable`] answers the only question whose answer is a
/// program's rather than a person's.
///
/// The core does not know which Lua method the caller invoked, so the
/// message carries the reason only; the adapter renders the
/// `knl: <method>: <kind>: <reason>` attribution.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KnlError {
    /// Lock contention: the store was busy and the same call may succeed if
    /// it is made again.  The one retryable class ([`KnlError::is_retryable`]).
    #[error("busy: {0}")]
    Busy(String),
    /// The store could not do the work — an IO fault, a connection that is
    /// gone, an encode failure on the way in.  Not busy, so retrying it is a
    /// caller's gamble rather than the kernel's advice.
    #[error("storage: {0}")]
    Storage(String),
    /// A stored row could not be read back as the event it was written as.
    /// Distinct from [`KnlError::Storage`] on purpose: the IO succeeded and
    /// the bytes came back, so what is wrong is the data, and no retry and
    /// no reconnect will change it.
    #[error("corruption: {0}")]
    Corruption(String),
    /// The session is over — this handle closed, or a resume was pointed at
    /// a stream whose log already carries its ending.  A session is
    /// disposable, so the answer is to open another, not to try again.
    #[error("closed: {0}")]
    Closed(String),
    /// The caller asked for something the kernel refuses to record: an event
    /// that does not meet its kind's shape, a kernel-only kind, a negative
    /// amount, an unknown view or a malformed option.  Nothing was written.
    #[error("validation: {0}")]
    Validation(String),
    /// The request is well-formed but this build or this backend cannot
    /// serve it — resuming an in-memory store, or a durable store in a build
    /// without the `sqlite` feature.
    #[error("unsupported: {0}")]
    Unsupported(String),
}

impl KnlError {
    /// The stable name of the [`KnlError::Busy`] class.
    pub const BUSY: &'static str = "busy";
    /// The stable name of the [`KnlError::Storage`] class.
    pub const STORAGE: &'static str = "storage";
    /// The stable name of the [`KnlError::Corruption`] class.
    pub const CORRUPTION: &'static str = "corruption";
    /// The stable name of the [`KnlError::Closed`] class.
    pub const CLOSED: &'static str = "closed";
    /// The stable name of the [`KnlError::Validation`] class.
    pub const VALIDATION: &'static str = "validation";
    /// The stable name of the [`KnlError::Unsupported`] class.
    pub const UNSUPPORTED: &'static str = "unsupported";

    /// Every class a kernel failure can have, in one closed list.
    ///
    /// Published so a caller can hold its own error vocabulary against the
    /// kernel's — the Lua bridge hands this to `knl.api()`, and the shell's
    /// declaration is checked against it rather than against a list somebody
    /// retyped.
    pub const KINDS: &'static [&'static str] = &[
        Self::BUSY,
        Self::STORAGE,
        Self::CORRUPTION,
        Self::CLOSED,
        Self::VALIDATION,
        Self::UNSUPPORTED,
    ];

    /// This failure's class, as one of [`KnlError::KINDS`].
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Busy(_) => Self::BUSY,
            Self::Storage(_) => Self::STORAGE,
            Self::Corruption(_) => Self::CORRUPTION,
            Self::Closed(_) => Self::CLOSED,
            Self::Validation(_) => Self::VALIDATION,
            Self::Unsupported(_) => Self::UNSUPPORTED,
        }
    }

    /// Whether making the same call again could succeed.
    ///
    /// True for [`KnlError::Busy`] and nothing else.  A storage fault *might*
    /// clear, but the kernel does not know that it will, and an error that
    /// says "try again" when it means "maybe" is how a retry loop becomes an
    /// infinite one.
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Busy(_))
    }

    /// Whether a class *name* is the retryable one.
    ///
    /// The same answer as [`KnlError::is_retryable`], for a caller that has
    /// the word rather than the value — the Lua bridge, which parses a kind
    /// back out of an attributed message.
    pub fn kind_is_retryable(kind: &str) -> bool {
        kind == Self::BUSY
    }

    /// The reason, without the class name or any attribution prefix.
    ///
    /// [`Display`](std::fmt::Display) writes `<kind>: <reason>`; this is the
    /// second half alone, for a caller that renders the class itself.
    pub fn reason(&self) -> &str {
        match self {
            Self::Busy(reason)
            | Self::Storage(reason)
            | Self::Corruption(reason)
            | Self::Closed(reason)
            | Self::Validation(reason)
            | Self::Unsupported(reason) => reason,
        }
    }
}

/// Result alias for the kernel core.
pub type KnlResult<T> = Result<T, KnlError>;

#[cfg(test)]
mod tests {
    use super::*;

    /// One error of every class, so a test over the classification cannot
    /// quietly skip a variant that was added later.
    fn one_of_each() -> Vec<KnlError> {
        vec![
            KnlError::Busy("contended".to_string()),
            KnlError::Storage("gone".to_string()),
            KnlError::Corruption("not json".to_string()),
            KnlError::Closed("session is closed".to_string()),
            KnlError::Validation("kind is required".to_string()),
            KnlError::Unsupported("no sqlite feature".to_string()),
        ]
    }

    /// Every variant names its class with a stable word, and the published
    /// list is exactly those words in that order.
    #[test]
    fn every_variant_names_its_class() {
        let kinds: Vec<&str> = one_of_each().iter().map(KnlError::kind).collect();
        assert_eq!(
            kinds,
            vec![
                "busy",
                "storage",
                "corruption",
                "closed",
                "validation",
                "unsupported"
            ]
        );
        assert_eq!(
            kinds,
            KnlError::KINDS.to_vec(),
            "KINDS is the vocabulary itself, not a second copy of it"
        );
    }

    /// Only contention says "ask again".  A storage fault might clear on its
    /// own, but the kernel does not know that, and an error that promises a
    /// retry it cannot back is how a loop stops terminating.
    #[test]
    fn only_busy_is_retryable() {
        for error in one_of_each() {
            let expected = error.kind() == KnlError::BUSY;
            assert_eq!(error.is_retryable(), expected, "{error}");
            assert_eq!(
                KnlError::kind_is_retryable(error.kind()),
                expected,
                "the name and the value must agree: {error}"
            );
        }
        assert!(!KnlError::kind_is_retryable("nonsense"));
    }

    /// `Display` is `<kind>: <reason>`, and `reason` is the second half on
    /// its own — the adapter renders the class itself, so it must be able to
    /// get the sentence without it.
    #[test]
    fn display_carries_the_class_and_reason_carries_only_the_sentence() {
        let error = KnlError::Validation("kind is required (string)".to_string());
        assert_eq!(error.to_string(), "validation: kind is required (string)");
        assert_eq!(error.reason(), "kind is required (string)");

        for error in one_of_each() {
            assert_eq!(
                error.to_string(),
                format!("{}: {}", error.kind(), error.reason())
            );
        }
    }
}
