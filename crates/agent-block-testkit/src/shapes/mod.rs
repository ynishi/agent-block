//! Provider response-shape catalog: spec-conformant and deliberately broken
//! tool-calling shapes.
//!
//! Broken variants carry a comment naming the stack that emits them; the
//! shapes are grounded in public provider documentation and issue trackers
//! (see the crate-level docs and the source comments per builder).

pub mod anthropic;
pub mod openai;
