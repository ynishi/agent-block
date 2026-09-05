# compile_loop

Autonomous compile-and-fix loop — Tool factory block.

`compile_loop.make(conf)` returns a `tool_def = {name, schema, handler}` that can be
passed to `agent.run({extra_tools = {tool_def}})`. When the calling LLM invokes the tool,
it runs an iterative edit-compile-check loop until the runner reports success or the
iteration ceiling is reached.

One iteration is one beat of the kernel (`knl.beat`: one model call plus the tools that
call asked for), run inside a `knl.session` whose grant is `max_iters`. The design and its
reasons are in the module doc at the head of `init.lua`; this file is the API surface.
`make` returns the def and does not register it — that is the caller's, and
`coding_agent.register_tool` is the entry point whose job it is.

## API

### `compile_loop.make(conf)`

| Field | Type | Required | Default | Description |
|---|---|---|---|---|
| `runner` | `function` | yes | — | See §Runner signature |
| `llm` | `table` | no | `{}` | Forwarded to the provider Port verbatim; nothing is inherited from a calling agent. `{provider, base_url, api_key, api_key_env, model, max_tokens, temperature, disable_thinking, timeout, …}` — every other key reaches `llm_proto` untouched, and an omitted `api_key` falls through to its env resolution |
| `max_iters` | `int` | no | `5` | Maximum iterations before giving up |
| `lang` | `string` | no | `"lua"` | Language hint for the LLM |
| `name` | `string` | no | `"compile_loop"` | The returned def's name. Nothing is registered under it |
| `system` | `string` | no | `nil` | Replaces the built-in system prompt for the mode |
| `edit_mode` | `"full"\|"diff"` | no | `"full"` | `"full"` rewrites the entire file in one completion; `"diff"` edits through tools |
| `tool_mode` | `"auto"\|"read_only"` | no | `"auto"` | `"diff"` mode only. `"auto"` declares `fs_read` / `read_file_range` / `fs_edit`; `"read_only"` declares just the reads, which can inspect but never converge |
| `on_iter` | `function` | no | — | `fn({ iter, code, result, raw })` after each iteration's verify. A raise is warned about, not propagated |
| `extra_tools` | `array` | no | — | Caller tools in the nested form `{name, schema = {description?, input_schema}, handler}` or a flat `{name, description?, input_schema?, handler}`. Declared alongside the built-ins; a name that collides with one is a loud error rather than a reserved-word check. Their calls never count as applied edits |

**Tool inputs** (`spec`, `target_file` or `target_files`, `lang?`) are supplied by the
calling LLM at tool-call time; factory `conf` fixes the runner and LLM policy at
registration time.

### Inputs: `target_file` XOR `target_files`

The tool schema accepts **either** `target_file` **or** `target_files` — not both.
Supplying both simultaneously raises an assertion error at handler entry.

| Field | Type | Mode |
|---|---|---|
| `target_file` | `string` | Single-file mode |
| `target_files` | `array<string>` | Multi-file mode (requires `edit_mode = "diff"`) |

Internally both forms are normalised to a list before any downstream logic runs. Existing
callers that supply only `target_file` continue to work unchanged.

## Single-file mode

Classic behaviour: one target file, any `edit_mode`.

```lua
local compile_loop = require("compile_loop")
local agent = require("agent")

local LUA_TIMEOUT = 60

local td = compile_loop.make({
    edit_mode = "diff",
    runner = function(path)
        -- path is an absolute string
        local res = sh.exec("lua " .. path, { timeout = LUA_TIMEOUT })
        if not res.ok then
            -- spawn failure or timeout: no exit code exists
            return { ok = false, stdout = "", stderr = tostring(res.error), exit_code = -1 }
        end
        return { ok = res.code == 0, stdout = res.stdout, stderr = res.stderr, exit_code = res.code }
    end,
})

local result = agent.run({
    provider    = "anthropic",
    model       = "claude-haiku-4-5",
    extra_tools = { td },
    prompt      = "Fix the script so it runs without errors.",
})
```

## Multi-file mode

Multiple target files edited in a single loop. Requires `edit_mode = "diff"`.

```lua
local compile_loop = require("compile_loop")
local agent = require("agent")

local CARGO_TIMEOUT = 300

local td = compile_loop.make({
    edit_mode = "diff",
    runner = function(paths)
        -- paths is a list<string> of absolute paths
        local res = sh.exec("cargo test", { timeout = CARGO_TIMEOUT })
        if not res.ok then
            -- spawn failure or timeout: no exit code exists
            return { ok = false, stdout = "", stderr = tostring(res.error), exit_code = -1 }
        end
        return { ok = res.code == 0, stdout = res.stdout, stderr = res.stderr, exit_code = res.code }
    end,
})

local result = agent.run({
    provider    = "anthropic",
    model       = "claude-haiku-4-5",
    extra_tools = { td },
    prompt      = "Fix the failing tests across both files.",
})
-- The tool's own JSON carries modified_files: the absolute paths an edit
-- landed in. `result` above is the parent agent's, not the loop's.
```

### How `diff` mode edits

Editing goes through tools; there is no text contract. The device carries exactly
three of them, plus whatever the caller added:

- `fs_read` — `std.fs`'s reader, path-locked to `target_files`. It answers
  `{ content, lines, version }`, takes an optional `start_line` / `end_line`, and
  refuses a whole-file read over 10 000 characters with the file's length and a
  pointer at the range read. There is no digest and no summarising sub-call: a
  summary is not something `fs_edit` can address.
- `read_file_range` — a verbatim, line-numbered slice of at most 500 lines. Those
  line numbers are what `fs_edit` addresses.
- `fs_edit` — `std.fs`'s editor, scoped to `target_files` by `path_lock`. Edits
  are addressed by line range and verified against the `expect`ed text there;
  a rejection reports what is actually at those lines. There is no fuzzy match.

Caller-supplied `extra_tools` are declared alongside these. Their calls never
count as edits: they are read-like by contract, and counting them would let a
loop that only queried the caller's tool look like it was making progress.

**The build is not a tool.** The loop runs the runner itself after every beat —
whatever the model asked for, and whether or not anything landed. That is what
the block's guarantee rests on: "it compiles" has to be something the loop
verified, not something the model reported, so the runner is never something the
model can decline to call, and there is no turn in which the model gets to say
the run is over.

Each iteration's result is recorded as a `verify` event on the session log,
stamped with the beat it judges, and the failure goes back to the model as the
next user turn.

## Edit format

`fs_edit` takes a path and a list of edits:

```json
{
  "path": "/abs/path/to/file.lua",
  "base": "<version from fs_read, optional>",
  "edits": [
    { "start_line": 12, "end_line": 14,
      "expect": "the current text of those lines",
      "replace": "the new text" }
  ]
}
```

Line numbers are 1-based and inclusive, and come from the numbered
`read_file_range` output (or from counting `fs_read`'s content, which the tool's
own description says is 1-based). `expect` must match exactly; it verifies the address rather than being
the address, so a wrong guess is reported with the text actually present instead
of landing the edit somewhere else. Every edit in a call is checked before any
is applied, so a rejected call changes nothing. Passing `base` makes the call
fail if the file changed since it was read.

## Runner signature

The runner signature differs by mode. Callers must write a runner appropriate for the mode
they select; the two signatures must **not** be unified into a single function that silently
changes behaviour when the mode changes.

**Single-file mode:**

```lua
runner = function(path)  -- path: string (absolute)
    -- ...
    return { ok = bool, stdout = string, stderr = string, exit_code = int }
end
```

**Multi-file mode:**

```lua
runner = function(paths)  -- paths: list<string> (absolute paths)
    -- ...
    return { ok = bool, stdout = string, stderr = string, exit_code = int }
end
```

## Return shape

`filter_for_tool_output` exposes the following fields to the calling agent:

| Field | Type | Present when |
|---|---|---|
| `ok` | `bool` | always |
| `iters` | `int` | always |
| `summary` | `string` | always |
| `artifact_path` | `string\|nil` | single-file only (absolute path of the edited file) |
| `modified_files` | `list<string>\|nil` | diff mode (absolute paths of every file an edit landed in, on every ending) |
| `failure_reason` | `string\|nil` | on failure (`"max_iters"`, `"stagnation"`, `"no_edits_applied"`, `"llm_call"`, `"open_target_file"`, `"stopped"`) |
| `last_error` | `string\|nil` | on failure (bounded; the untruncated text is in the session log) |

In multi-file mode `artifact_path` is `nil`; use `modified_files` instead.

`"max_iters"` is the session's grant running out. `"stopped"` is the kernel
stopping a beat for any other reason, which a caller should not normally see —
`last_error` carries the kernel's word for it when it happens.

The shape is closed (`compile_loop.shapes.tool_output`), which is how the run's
transcript is kept out of the caller's context: there is no field for it, so
adding one fails rather than leaking.

## Constraints

- **`edit_mode = "diff"` is required for multi-file mode.** Specifying `edit_mode = "full"`
  with `target_files` raises an assertion error at handler entry.
- `target_file` and `target_files` are mutually exclusive. Supplying both raises an assertion
  error.
- `target_files` must be a non-empty list of strings.
- **`edit_mode = "diff"` needs its target files to exist and be non-empty**, and says so at
  handler entry. It used to fall back to `"full"` with a warning, which meant a caller
  asking for minimal edits could silently get its file rewritten instead.
- Stagnation detection: when `STAGNATION_WINDOW = 3` consecutive iterations produce identical
  runner `stderr`, the loop exits immediately with `failure_reason = "stagnation"`.
- Bad stagnation: when `STAGNATION_WINDOW = 3` consecutive iterations apply zero edits (every
  `fs_edit` call was rejected, or the model never called it), the loop exits with
  `failure_reason = "no_edits_applied"`. See §Qwen path operational notes for details.

## Background

The compile_loop block was extracted from `coding_agent` to allow reuse as a standalone
Tool factory. Multi-file mode was added to address LLM context overflow (`max_model_len`
exceeded) when embedding entire large files in the prompt — diffing only the changed sections
across multiple files keeps context size bounded.

## Qwen path operational notes

These notes apply to the OpenAI provider path when targeting a Qwen vLLM endpoint
(e.g. RunPod proxy serving `qwen36-vllm-a40` or similar). The compile_loop block
itself is provider-agnostic — these are operational guidance for callers.

### Deterministic temperature

Greedy decoding is what a code-editing loop wants, and it is a conf key like any
other: `compile_loop.make({ llm = { temperature = 0.0 } })`. There is no default
and no env tier — `conf.llm` reaches the provider verbatim, and a block-private
environment variable for one provider knob was a tier nothing else had.

### Disable thinking mode

For Qwen-style models that expose a chain-of-thought thinking budget, set
`disable_thinking = true` on the LLM config to suppress reasoning output and
reduce latency. Example:

```lua
local tool = compile_loop.make({
    llm = {
        provider          = "openai",
        base_url          = "https://<runpod-proxy>/v1",
        api_key_env       = "QWEN_API_KEY",
        model             = "Qwen/Qwen2.5-Coder-32B-Instruct-AWQ",
        disable_thinking  = true,  -- recommended for code-editing loops
        temperature       = 0.0,   -- greedy decoding; no default is applied
    },
    runner = function(path) ... end,
})
```

### Bad vs good stagnation

The loop distinguishes two failure modes when iterations do not converge:

- `failure_reason = "stagnation"` — the runner produced identical `stderr` for
  `STAGNATION_WINDOW = 3` consecutive iterations. This is the "good" stagnation
  case: the LLM is editing, but the runner is stuck on the same error. The
  reading is `policy.stagnation`'s, over the `verify` events on the log.
- `failure_reason = "no_edits_applied"` — `STAGNATION_WINDOW = 3` consecutive
  iterations reached disk with nothing: every `fs_edit` was rejected, or the
  model never called it. The "bad" stagnation case: the LLM is not making
  progress in edits at all. Each such iteration carries the rejection text back
  into the next prompt; only after the third consecutive zero-edit iteration
  does the loop exit.

Callers should treat `no_edits_applied` as a stronger failure signal than
`stagnation` — it suggests the prompt or model is incompatible with the target
file shape, not just that the fix is hard.

### Cross-reference

For RunPod proxy operational gotchas (e.g. ~30s cold-start timeout on first
request after pod idle), consult your proxy-side documentation.
