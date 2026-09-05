# agent-block over MCP

This server hands a *block* — one Lua script — to a calling agent as a tool.
The block runs in its own agent-block host with its own model credentials, so
its LLM turns never enter the caller's context. What comes back is the one
value the script returned.

## The one contract

**A block returns a JSON string.**

```lua
-- blocks/summarize.lua
local agent = require("agent")

local result = agent.run({
    provider = "anthropic",
    model = "claude-haiku-4-5-20251001",
    prompt = _PROMPT,
    system = _CONTEXT,
})

return std.json.encode({
    ok = result.ok,
    text = result.content,
    turns = result.num_turns,
})
```

`agent.run` returns `content`, not `text` — the block above renames it on the
way out because that is its own choice of wire shape. The full result shape is
`{ ok, content, usage, num_turns, messages }` on success and
`{ ok = false, error, usage, num_turns, messages }` on failure.

The value is stringified by the host on the way out. A Lua string passes
through unchanged; a table does not — it becomes `table: 0x…`, which tells the
caller nothing. Hence `std.json.encode`. A block that returns nothing is not an
error, it just has nothing to say.

## What the block receives

| Lua global | Source |
|---|---|
| `_PROMPT` | the `prompt` argument of `run_block` |
| `_CONTEXT` | the `context` argument of `run_block` |

Both are absent when the caller omits them, so a block that requires one should
say so in its own header comment — that comment is what the caller reads in the
tool description and in `agent-block://blocks`.

## What counts as a block

Any `.lua` file directly inside a registered block directory (`--block-dir`,
repeatable). The file stem is the block name. Nothing else is callable: the
`block` argument is an enum of the registered names, so a path that was never
registered cannot be reached through this surface.

Blocks are re-scanned per request. Dropping a new `.lua` file into the
directory makes it callable without restarting the server.

## Failure

A Lua error surfaces as a failed tool call carrying the error text. A block
that ran and decided the answer is "no" should instead return successfully with
that fact in its JSON — the two are different events, and only the block knows
which one happened.

## Reading a run afterwards

A run leaves a durable record rather than a log to grep: every model call is an
`llm_request` / `llm_response` (or `llm_call_failed`) event on the block's
session log, every tool call is a recorded pair, and a loop block's own steps
are the kinds it appends. Read them back with `session:query` or `knl.views`.

What still reaches this server's stderr — which the MCP client shows as server
logs — is the http bridge's `ab.obs` `http_request` / `http_response` pair,
carrying `trace_id` / `run_id` / `agent_id` / `agent_name`. Set those ids
(`AGENT_BLOCK_RUN_ID`, …) in the server's environment when you want to
correlate a run against something outside.
