# compile_loop

Autonomous compile-and-fix loop — Tool factory block.

`compile_loop.make(conf)` returns a `tool_def = {name, schema, handler}` that can be
passed to `agent.run({extra_tools = {tool_def}})`. When the calling LLM invokes the tool,
it runs an iterative edit-compile-check loop until the runner reports success or the
iteration ceiling is reached.

## API

### `compile_loop.make(conf)`

| Field | Type | Required | Default | Description |
|---|---|---|---|---|
| `runner` | `function` | yes | — | See §Runner signature |
| `llm` | `table` | no | inherited | `{provider, base_url, api_key, api_key_env, model, max_tokens, temperature, disable_thinking, timeout}` |
| `max_iters` | `int` | no | `5` | Maximum iterations before giving up |
| `lang` | `string` | no | `"lua"` | Language hint for the LLM |
| `name` | `string` | no | `"compile_loop"` | Tool name registered in the tool registry |
| `system` | `string` | no | `nil` | Additional system prompt prepended to the default |
| `edit_mode` | `"full"\|"diff"` | no | `"full"` | `"full"` rewrites the entire file in one completion; `"diff"` edits through tools |
| `tool_mode` | `"auto"\|"read_only"` | no | `"auto"` | `"diff"` mode only. `"auto"` declares `read_file` / `read_file_range` / `fs_edit`; `"read_only"` declares just the read tools, which can inspect but never converge |
| `extra_tools` | `array` | no | — | Multi-file only. Caller-registered tools in the agent-layer nested form `{name, schema = {description?, input_schema}, handler}`. Declared alongside the built-in tools; dispatched inside the tool loop; built-in names are reserved. `handler(input)` returns a string; errors are propagated as recoverable tool_result text. Extra-tool calls do not count as applied edits |

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
local compile_loop = require("blocks/tools/compile_loop")

local LUA_TIMEOUT = 60

local tool = compile_loop.make({
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
    provider = "anthropic",
    model    = "claude-haiku-4-5",
    extra_tools = { tool },
    messages = {{
        role    = "user",
        content = "Fix the script so it runs without errors.",
    }},
})
```

## Multi-file mode

Multiple target files edited in a single loop. Requires `edit_mode = "diff"`.

```lua
-- pseudo (requires subtask-1 implementation)
local compile_loop = require("blocks/tools/compile_loop")

local CARGO_TIMEOUT = 300

local tool = compile_loop.make({
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
    provider = "anthropic",
    model    = "claude-haiku-4-5",
    extra_tools = { tool },
    messages = {{
        role    = "user",
        content = "Fix the failing tests across both files.",
    }},
})
-- result.modified_files contains the list of absolute paths that were written
```

### How `diff` mode edits

Editing goes through tools; there is no text contract. Each iteration runs
`tool_loop` (`blocks/lib/tool_loop`) with exactly three tools and nothing else:

- `read_file` / `read_file_range` — the loop's own readers, so a large file goes
  through the digest/distill path instead of the context. Their output is
  line-numbered, and those line numbers are what the edit tool addresses.
- `fs_edit` — `std.fs`'s editor, scoped to `target_files` by `path_lock`. Edits
  are addressed by line range and verified against the `expect`ed text there;
  a rejection reports what is actually at those lines. There is no fuzzy match.

Caller-supplied `extra_tools` are declared alongside these. Their calls never
count as edits: they are read-like by contract, and counting them would let a
loop that only queried the caller's tool look like it was making progress.

**The build is not a tool.** The loop runs the runner itself after the editing
turns finish. That is what the block's guarantee rests on — "it compiles" has to
be something the loop verified, not something the model reported, so the runner
is never something the model can decline to call.

An iteration that applied no edits skips the runner (it would report what it
already reported) and carries the rejection text into the next one.

## Edit format

`fs_edit` takes a path and a list of edits:

```json
{
  "path": "/abs/path/to/file.lua",
  "base": "<version from read_file, optional>",
  "edits": [
    { "start_line": 12, "end_line": 14,
      "expect": "the current text of those lines",
      "replace": "the new text" }
  ]
}
```

Line numbers are 1-based and inclusive, and come from the numbered `read_file`
output. `expect` must match exactly; it verifies the address rather than being
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
| `modified_files` | `list<string>\|nil` | multi-file only (absolute paths of all written files) |
| `failure_reason` | `string\|nil` | on failure (`"max_iters"`, `"stagnation"`, or `"no_edits_applied"`) |
| `last_error` | `string\|nil` | on failure |

In multi-file mode `artifact_path` is `nil`; use `modified_files` instead.

## Constraints

- **`edit_mode = "diff"` is required for multi-file mode.** Specifying `edit_mode = "full"`
  with `target_files` raises an assertion error at handler entry.
- `target_file` and `target_files` are mutually exclusive. Supplying both raises an assertion
  error.
- `target_files` must be a non-empty list of strings.
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

The OpenAI body defaults `temperature = 0.0` for deterministic greedy decoding,
which is the desired behaviour for code-editing loops. Callers can override via
either:

- `compile_loop.make({ llm = { temperature = <number> } })` — explicit caller value
- `COMPILE_LOOP_LLM_TEMPERATURE=<number>` — env override applied when caller does
  not pass `llm.temperature`

Precedence: caller > env > `0.0` default. Setting `COMPILE_LOOP_LLM_TEMPERATURE`
to a non-numeric value falls back to `0.0` with a warning log entry.

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
        -- temperature defaults to 0.0; set COMPILE_LOOP_LLM_TEMPERATURE
        -- or pass explicit temperature here to override.
    },
    runner = function(path) ... end,
})
```

### Bad vs good stagnation

The loop distinguishes two failure modes when iterations do not converge:

- `failure_reason = "stagnation"` — runner produced identical `stderr` for
  `STAGNATION_WINDOW = 3` consecutive iterations after at least one successful
  edit. This is the "good" stagnation case: the LLM is editing, but the runner
  is stuck on the same error.
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
