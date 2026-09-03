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
//!   overwritten rather than trusted.  Reads hand back clones, so a
//!   caller cannot reach recorded state through a returned value.
//! - **I3 budget monotonicity.**  [`Budget`] accepts non-negative
//!   amounts only and the balance can only decrease (floored at `0`).
//!   There is no API to raise or reset it.
//! - **I6 run scope.**  All state lives inside a [`Session`] value — no
//!   statics — so two sessions are fully independent, and `close` ends
//!   the run scope (later `append` / `spend` are errors).
//! - **K2 model call.**  [`call`] holds the kernel half of the model-call
//!   syscall: a backend result is checked before anything is written, the
//!   response is recorded before the budget is charged, and the turn it
//!   is stamped with is the kernel's own count of the responses it took.
//!   Calling the backend is the adapter's job — it is a Lua function, and
//!   the kernel must not be holding its own state while the shell runs.
//!
//! Projections ([`projection`]) are *derived*: folding never changes the
//! history and a fold result is a cache, not a capture — it can always be
//! recomputed from the events.

pub mod budget;
pub mod call;
pub mod event;
pub mod history;
pub mod projection;
pub mod session;

use std::fmt;

pub use budget::Budget;
pub use call::{validate_backend_result, CallOutcome, ModelResult};
pub use event::{now_ms, validate_event, FIELD_EPOCH_MS, FIELD_KIND, FIELD_SEQ};
pub use history::History;
pub use projection::{DialogueFold, UsageFold, Views};
pub use session::Session;

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
