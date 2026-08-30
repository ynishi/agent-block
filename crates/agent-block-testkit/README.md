# agent-block-testkit

In-process mock LLM servers and a provider wire-shape catalog for testing
tool-calling clients.

Most LLM-client test suites only exercise spec-shaped provider responses. Real
OpenAI-compatible stacks deviate: `function.arguments` arrives as a JSON
object instead of a string (Ollama native `/api/chat` leak-through, Gemini
`functionCall.args`, some vLLM tool-call parsers), the `id` field goes
missing, argument strings arrive malformed. This crate packages both the
spec-conformant shapes and those deliberately broken variants, plus a small
axum mock server to serve them, so a client's tolerance for the wire as it
actually exists can be regression-tested.

## Pieces

- `shapes::openai` / `shapes::anthropic` — response-body builders: plain text
  turns, tool-call turns, and the broken variants (each documented with the
  stack that emits it).
- `server::MockLlm` — spawns an Anthropic Messages (`/v1/messages`) or OpenAI
  Chat Completions (`/chat/completions`) endpoint on an ephemeral local port,
  driven by a responder closure that receives per-request facts (call index,
  whether tool results are present, extracted target paths). Request facts
  useful for assertions (tool declarations, `tool_call_id`s of returned tool
  results) are recorded on a shared `MockState`.

```rust
use agent_block_testkit::server::MockLlm;
use agent_block_testkit::shapes::openai;
use serde_json::json;

let handle = MockLlm::openai(|req| {
    if !req.has_tool_results {
        openai::tool_calls_response(vec![openai::tool_call_object_args_no_id(
            "apply_search_replace",
            json!({"path": "/tmp/f.lua", "search": "a", "replace": "b"}),
        )])
    } else {
        openai::text_response("DONE")
    }
})
.spawn()
.await;
// point the client under test at handle.base_url, then assert on handle.state
```

The first consumer is agent-block's own `compile_loop` e2e suite
(`crates/agent-block/tests/e2e_compile_loop.rs`), which doubles as the
regression gate for this crate.
