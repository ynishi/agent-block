//! In-process mock LLM servers and a provider wire-shape catalog for testing
//! tool-calling clients.
//!
//! Two pieces:
//!
//! - [`shapes`] — builders for provider response bodies. Alongside the
//!   spec-conformant shapes, the catalog carries **deliberately broken
//!   variants observed in the wild** on OpenAI-compatible stacks (object
//!   `arguments`, missing `id`, malformed argument strings). A client that is
//!   only exercised against spec-shaped input silently loses tolerance for
//!   the shapes real serving stacks emit; the broken variants exist to guard
//!   exactly that failure mode.
//! - [`server`] — a small axum-backed mock server ([`server::MockLlm`]) that
//!   serves an Anthropic Messages (`/v1/messages`) or OpenAI Chat Completions
//!   (`/chat/completions`) endpoint from a caller-supplied responder closure,
//!   while recording request facts (tool declarations, `tool_call_id`s of
//!   returned tool results) for assertions.
//!
//! ```no_run
//! use agent_block_testkit::server::MockLlm;
//! use agent_block_testkit::shapes::openai;
//! use serde_json::json;
//!
//! # async fn demo() {
//! let handle = MockLlm::openai(|req| {
//!     if !req.has_tool_results {
//!         // Broken-but-observed shape: object arguments, no id.
//!         openai::tool_calls_response(vec![openai::tool_call_object_args_no_id(
//!             "apply_search_replace",
//!             json!({"path": "/tmp/f.lua", "search": "a", "replace": "b"}),
//!         )])
//!     } else {
//!         openai::text_response("DONE")
//!     }
//! })
//! .spawn()
//! .await;
//! // point the client under test at handle.base_url ...
//! assert_eq!(handle.state.call_count(), 0);
//! handle.ct.cancel();
//! # }
//! ```

pub mod server;
pub mod shapes;
