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

A `<name>.lua` file or a `<name>/init.lua` directory directly inside a block
root. The roots are `<project>/blocks/` and `$AGENT_BLOCK_HOME/blocks/`
(default `~/.agent-block/blocks/`), whenever they exist, plus any `--block-dir`
the server was started with. The project root wins a name clash. Nothing else
is callable: the `block` argument is an enum of the registered names, so a path
that was never registered cannot be reached through this surface.

Blocks are re-scanned per request. Dropping a new file into a root makes it
callable without restarting the server.

## Where a block's helpers go

`blocks/` is for entry points only and is never on the `require` path. A
module a block needs goes in `lib/` beside it — `<project>/lib/` or
`$AGENT_BLOCK_HOME/lib/` — and is reached with `require("<name>")`. The two
directories do not cross: a file in `lib/` is never served as a block, and a
file in `blocks/` cannot be required. A module that proves useful in another
project moves from the project `lib/` to the user `lib/` unchanged.

## Failure

A Lua error surfaces as a failed tool call carrying the error text. A block
that ran and decided the answer is "no" should instead return successfully with
that fact in its JSON — the two are different events, and only the block knows
which one happened.

## How long a block should run here

This server speaks stdio, which means the client launched it as a subprocess:
it lives exactly as long as the client's session. A run still going when that
session ends goes with it. Nothing in the protocol changes that — a stdio
server outliving its client would be the bug, not the feature.

The environment is the server's too, fixed when the client launched it, not the
caller's shell. So anything that has to vary per call has to arrive in
`prompt` — `AGENT_BLOCK_KNL_PATH` set per run reaches the shell that set it and
not this server, and every run here writes to the one session log the server
resolved at startup. A block that wants its own log takes the path as an
argument and opens its session with `store = { sqlite = <path> }`.

So the blocks that belong on this surface are the ones whose answer belongs in
the conversation: fetch a specification, read some state, write a result back.
A long one — a coding loop, a batch over many inputs — will run, and is not
refused, but it is being run in the wrong place. Give that work its own process
through the CLI, owned by whatever schedules it — `agent-block serve` is that
scheduler for a block that declares a `job.toml` beside itself — and let this
server be the entry point rather than the executor. One run per process is what makes the
per-run environment, the per-run session log, and a lifetime independent of any
one client fall out for free instead of having to be rebuilt here.

If this surface grows, the direction is reading a run's record rather than
running longer ones.

## The job manager, from here

The scheduler for long work is `agent-block serve`: a block with a `job.toml`
beside it runs on its interval, in a process of its own, and the manager
records every run. This server carries the manager's five verbs as tools —
`jobs_list`, `runs_list`, `run_get`, `job_run`, `run_stop` — each one route
of the manager's HTTP listener (`--serve-url`, default `http://127.0.0.1:7788`,
token from `$AGENT_BLOCK_HOME/serve.token`). `job_run` and `run_stop` record
a request and answer at once; the manager acts on its next tick, so read the
outcome back with `runs_list` rather than waiting on the call. When the
manager is not running the tool says so; starting it is not this server's.

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
