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
| `edit_mode` | `"full"\|"diff"` | no | `"full"` | `"full"` rewrites the entire file; `"diff"` uses SEARCH/REPLACE patches |

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
local compile_loop = require("blocks/compile_loop")

local tool = compile_loop.make({
    edit_mode = "diff",
    runner = function(path)
        -- path is an absolute string
        local handle = io.popen("lua " .. path .. " 2>&1", "r")
        local out = handle:read("*a")
        local ok = handle:close()
        return { ok = ok, stdout = out, stderr = "", exit_code = ok and 0 or 1 }
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
local compile_loop = require("blocks/compile_loop")

local tool = compile_loop.make({
    edit_mode = "diff",
    runner = function(paths)
        -- paths is a list<string> of absolute paths
        local cmd = "cargo test 2>&1"
        local handle = io.popen(cmd, "r")
        local out = handle:read("*a")
        local ok = handle:close()
        return { ok = ok, stdout = out, stderr = "", exit_code = ok and 0 or 1 }
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

### Multi-file examples (Anthropic)

End-to-end smoke scripts under `examples/`, runnable as `agent-block -s examples/<file>.lua` (requires `ANTHROPIC_API_KEY` in `.env`):

| Script | Scenario |
|---|---|
| `test_anthropic_compile_loop_multi.lua` | Add a function to **both** files (basic additive multi-file diff) |
| `test_anthropic_compile_loop_multi_delete.lua` | Remove a function + assertions from both files (REPLACE-empty deletion) |
| `test_anthropic_compile_loop_multi_selective.lua` | Edit one file only; verifies the untouched file is byte-identical |
| `test_anthropic_compile_loop_multi_stagnation.lua` | Forced-fail runner; asserts `max_iters` bound and `ok=false` return |

Single-file equivalents live alongside (`test_anthropic_compile_loop.lua` etc.).

## SEARCH/REPLACE format

### Single-file (`target_file`)

The LLM produces one or more SEARCH/REPLACE blocks. No path header is needed.

```
<<<<<<< SEARCH
<existing text to find>
=======
<replacement text>
>>>>>>> REPLACE
```

Path headers in single-file mode are accepted but ignored (lenient parse). All blocks are
applied to `target_file`.

### Multi-file (`target_files`)

Each group of SEARCH/REPLACE blocks must be preceded by a path header line that identifies
the target file:

```
<<< path=src/file_a.lua >>>
<<<<<<< SEARCH
<existing text in file_a>
=======
<replacement text>
>>>>>>> REPLACE

<<< path=src/file_b.lua >>>
<<<<<<< SEARCH
<existing text in file_b>
=======
<replacement text>
>>>>>>> REPLACE
```

Rules:

- The `<<< path=<relpath> >>>` line must appear **before** the first SEARCH/REPLACE block
  for that file.
- Consecutive SEARCH/REPLACE blocks under the same path header all apply to that file.
- A new path header switches the active file.
- Path headers are **required** in multi-file mode. A block with no preceding path header
  is a parse error.
- The path must appear in `target_files`. A path not in the allowlist is a parse error.
- Duplicate path headers (same path appearing twice) are a parse error.

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
| `failure_reason` | `string\|nil` | on failure (`"max_iters"` or `"stagnation"`) |
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

## Background

The compile_loop block was extracted from `coding_agent` to allow reuse as a standalone
Tool factory. Multi-file mode was added to address LLM context overflow (`max_model_len`
exceeded) when embedding entire large files in the prompt — diffing only the changed sections
across multiple files keeps context size bounded. For motivation see
[agent-profiles issue 1777766817-70585](https://github.com/ynishi/agent-profiles/issues/1777766817-70585).
