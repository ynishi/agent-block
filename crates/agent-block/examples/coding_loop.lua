-- coding_loop.lua — a caller-written loop that edits files until a verify
-- command passes: the reference for a coding loop over the kernel.
--
-- There is no coding loop in agent-block — `compile_loop` and `coding_agent`
-- went in 0.38.0, because a loop is the caller's to compose. This is what
-- composing one looks like, with every part in the seam the kernel already
-- has:
--
--   the task            `_PROMPT` (the spec), pinned as the seed the fold keeps
--   the files           `std.fs.tool_specs` read / search_replace, path-locked
--                       to the targets; large files enter the spec as a
--                       structural map, small ones whole, line-numbered
--   what one beat sends `policy.window{ fit }` — as many beats as fit the
--                       model's window, the spec always kept
--   what a tool answers `policy.result_cap` (one result may not outgrow a share
--                       of the window) and `policy.repeat_cap` (the same read,
--                       again, with no edit in between, is refused)
--   what a failure says `policy.carry` — one bounded note about an edit the
--                       tool refused, carried into the next request
--   when to stop        `policy.verdict{ run = verify, changed, timeout }` after
--                       every iteration — green counts only with an edit
--                       applied — `policy.stagnation` on the verify output,
--                       and the kernel's budget of beats
--
-- Run (the spec is the prompt; targets and verify are the environment):
--
--   CODING_TARGETS="src/lib.rs" CODING_VERIFY="cargo check" \
--     agent-block -s crates/agent-block/examples/coding_loop.lua \
--       --prompt "Add a pub fn double(n: i64) -> i64 to src/lib.rs, with a test."
--
--   AGENT_PROVIDER=anthropic (default) needs ANTHROPIC_API_KEY;
--   AGENT_PROVIDER=openai reaches any OpenAI-compatible server —
--     QWEN_BASE_URL=https://<host>/v1  QWEN_MODEL=qwen  (vLLM: the api key is not checked)
--   CODING_REPO      the directory verify runs in and paths are relative to (default: cwd)
--   CODING_ITERS     iterations, each ending in a verify (default 5)
--   CODING_TURNS     beats per iteration before verify runs anyway (default 8)
--
-- Returns one JSON string — `{ ok, iters, summary, session, failure_reason?,
-- last_error? }` — which is what makes it a block: registered under `blocks/`
-- with a `job.toml` beside it, `agent-block serve` runs it on a schedule and
-- records that value; over MCP, `run_block` hands it back. The exit code is
-- 0 whenever the loop ran — `ok` in the value says whether the verify
-- passed — and non-zero only when it could not run (missing input raises).

local kernel = require("knl")
local Outcome = kernel.Outcome
local adapter = require("knl_adapter")
local policy = require("policy")

local E = std.env
local SPEC = _PROMPT or ""
local TARGETS_RAW = E.get("CODING_TARGETS") or ""
local VERIFY_CMD = E.get("CODING_VERIFY") or "cargo check --all-targets"
local REPO = (E.get("CODING_REPO") or "."):gsub("/+$", "")
local MAX_ITERS = tonumber(E.get("CODING_ITERS") or "5")
local MAX_TURNS = tonumber(E.get("CODING_TURNS") or "8")
local PROVIDER = E.get("AGENT_PROVIDER") or "anthropic"

-- Missing input is a raise, not an `os.exit`: this script is also a block,
-- and a block runs inside a host that is not its own process (`run_block`
-- over MCP), where an exit would end the server. A raise is a failed call
-- there and a non-zero exit at a shell.
if SPEC == "" or TARGETS_RAW == "" then
    error("coding_loop: pass the spec as --prompt and the target files in CODING_TARGETS", 0)
end
if PROVIDER == "openai" and (E.get("QWEN_BASE_URL") or "") == "" then
    error("coding_loop: AGENT_PROVIDER=openai needs QWEN_BASE_URL", 0)
end
if PROVIDER == "anthropic" and (E.get("ANTHROPIC_API_KEY") or "") == "" then
    error("coding_loop: ANTHROPIC_API_KEY is not set", 0)
end

-- Targets are made absolute under CODING_REPO: the `std.fs` tools resolve a
-- path against the process's own directory, not the repository's, and the
-- path the model passes is the one the spec names — so both name the same
-- file, in full.
local targets = {}
for item in TARGETS_RAW:gmatch("[^,\n]+") do
    local t = item:gsub("^%s+", ""):gsub("%s+$", "")
    if t ~= "" then
        targets[#targets + 1] = t:sub(1, 1) == "/" and t or (REPO .. "/" .. t)
    end
end

-- ============================================================
-- The Port and its conf. The same conf opens the Port and sizes the fold.
-- ============================================================
local port, CONF
if PROVIDER == "openai" then
    port = adapter.openai
    CONF = {
        base_url = E.get("QWEN_BASE_URL"),
        api_key = E.get("OPENAI_API_KEY") or "dummy",
        model = E.get("QWEN_MODEL") or "qwen",
        dialect = "vllm",
        thinking = { enabled = false },
        temperature = 0.2,
        max_tokens = 4096,
        timeout = 600,
    }
else
    port = adapter.anthropic
    CONF = {
        api_key = E.get("ANTHROPIC_API_KEY"),
        model = E.get("ANTHROPIC_MODEL") or "claude-haiku-4-5-20251001",
        max_tokens = 4096,
        timeout = 600,
    }
end

-- ============================================================
-- Tools: std.fs, path-locked to the targets. `read` answers a range;
-- `search_replace` edits by a verbatim snippet. Wrapped by two policies —
-- repeat_cap needs the session, so it is bound inside the session below.
-- ============================================================
local read_spec = std.fs.tool_specs({ allowed = { "read" }, path_lock = targets })[1]
local edit_spec = std.fs.tool_specs({ allowed = { "search_replace" }, path_lock = targets })[1]
local function declared(spec)
    return {
        name = spec.name,
        description = spec.description,
        input_schema = spec.input_schema,
        handler = spec.handler,
    }
end
local raw_tools = adapter.tools({ declared(read_spec), declared(edit_spec) })
local size_cap = policy.result_cap({ port = port, conf = CONF, share = 0.25 })
local repeat_cap = policy.repeat_cap({ max = 2, resets = { edit_spec.name } })

-- ============================================================
-- The seed: the spec, plus each target as the model should first see it.
-- A small file goes in whole and line-numbered; a large one as a map of its
-- declaration lines, so the model picks a region to read instead of reading
-- everything. The fold keeps this seed (`keep_seed`), so it never drops out.
-- ============================================================
local SEED_FULL_MAX = 16000

local function numbered(text)
    local out, n = {}, 0
    for line in (text .. "\n"):gmatch("(.-)\n") do
        n = n + 1
        out[#out + 1] = string.format("%d\t%s", n, line)
    end
    if #out > 0 and text:sub(-1) == "\n" then
        table.remove(out)
    end
    return table.concat(out, "\n")
end

local ANCHORS = {
    "^%s*pub[%s(]",
    "^%s*fn%s",
    "^%s*impl[%s<]",
    "^%s*struct%s",
    "^%s*enum%s",
    "^%s*trait%s",
    "^%s*mod%s",
    "^%s*#%[",
    "^%s*local%s+function",
    "^%s*function%s",
}

local function structural_map(text)
    local out, n = {}, 0
    for line in (text .. "\n"):gmatch("(.-)\n") do
        n = n + 1
        for _, pat in ipairs(ANCHORS) do
            if line:match(pat) then
                out[#out + 1] = string.format("%d\t%s", n, line)
                break
            end
        end
    end
    return table.concat(out, "\n"), n
end

for _, path in ipairs(targets) do
    local f = io.open(path, "r")
    if f then
        local content = f:read("*a") or ""
        f:close()
        if #content <= SEED_FULL_MAX then
            SPEC = SPEC
                .. "\n\n## Current content of "
                .. path
                .. "\n(line-numbered as it is NOW; after an edit shifts lines, read the range again "
                .. "before editing near it)\n\n"
                .. numbered(content)
        else
            local map, total = structural_map(content)
            SPEC = SPEC
                .. "\n\n## Structural map of "
                .. path
                .. " ("
                .. tostring(total)
                .. " lines)\nDeclaration lines with their line numbers — not the file. Pick the region, "
                .. "read it with "
                .. read_spec.name
                .. " (start_line / end_line, under ~150 lines), then edit with the exact text you saw.\n\n"
                .. map
        end
    end
end

local SYSTEM = "You are an expert programmer editing existing files through tools, not by printing code.\n"
    .. "- "
    .. read_spec.name
    .. " shows a file's current content (start_line / end_line for a slice of a large one).\n"
    .. "- "
    .. edit_spec.name
    .. " changes it: `search` is a verbatim snippet of the CURRENT file, unique in it; `replace` is the new text. "
    .. "Keep each search small (1-10 lines); split a big change into several edits. `search_not_found` means you "
    .. "guessed the text: re-read that region and copy it exactly.\n"
    .. "- Every path must be one of the target files. Make the SMALLEST change that satisfies the spec.\n"
    .. "The verify command runs after every one of your turns whether or not you ask, and its output comes back. "
    .. "Old reads drop out of the conversation as you go; read a region, edit it at once, move on."

-- ============================================================
-- Verify, as a verdict. `timeout` hands the seconds to the run: the first
-- gets `first`, the ones after it three times what the last one took, held
-- between the floor and `first`. `changed` withholds a green until an edit
-- has landed — a task whose deliverable is the test passes before any work.
-- ============================================================
local edits_applied = 0

local function verify(seconds)
    local res = sh.exec(VERIFY_CMD, { cwd = REPO, timeout = seconds })
    if res.ok ~= true then
        return {
            ok = false,
            ran = false,
            stdout = "",
            stderr = "verify did not run: " .. tostring(res.error),
            exit_code = -1,
        }
    end
    local merged = tostring(res.stdout or "") .. tostring(res.stderr or "")
    return { ok = res.code == 0, stdout = "", stderr = merged:sub(-6000), exit_code = res.code }
end

local verdict = policy.verdict({
    run = verify,
    changed = function()
        return edits_applied > 0
    end,
    timeout = { first = 900, factor = 3, floor = 60 },
})

--- How many edits this beat applied, read off the log: a `tool_result` for
--- the edit tool whose result says ok.
local function edits_in(session, beat_id)
    local by_call, applied = {}, 0
    for _, ev in ipairs((session:events())) do
        if ev.beat == beat_id then
            local data = type(ev.data) == "table" and ev.data or {}
            if ev.kind == "tool_call" and data.name == edit_spec.name then
                by_call[data.call_id] = true
            elseif ev.kind == "tool_result" and by_call[data.call_id] then
                if type(data.result) == "table" and data.result.ok == true then
                    applied = applied + 1
                end
            end
        end
    end
    return applied
end

local function verify_signature(beat)
    for _, ev in ipairs(beat.events) do
        if ev.kind == "verify" then
            return tostring((ev.data or {}).stderr or "")
        end
    end
    return nil
end

local function failed_pair(pair)
    return pair.ok == false or (type(pair.result) == "table" and pair.result.ok == false)
end

-- ============================================================
-- The loop
-- ============================================================
local stalled = policy.stagnation({ same = 3, signature = verify_signature })
local iters, converged, failure_reason, last_error, session_id = 0, false, nil, nil, nil

kernel.session({
    owner = "coding_loop",
    budget = { amount = MAX_ITERS * MAX_TURNS, tag = "beats" },
}, function(s)
    session_id = tostring(s:id())
    local fold, fits = policy.window({ fit = { port = port, conf = CONF }, keep_seed = true })
    local device = kernel.device({
        llm = port:open(CONF),
        system = SYSTEM,
        tools = repeat_cap(s)(size_cap(raw_tools)),
        fold = fold,
        filters = { policy.carry({ max_bytes = 512, failed = failed_pair })(s) },
    })
    s:append({ kind = "msg_user", meta = { label = "spec" }, data = { content = SPEC } })

    local function stop(reason, err)
        failure_reason, last_error = reason, err
        return false
    end
    local arms = function(sink)
        return {
            stopped = function(o)
                return stop(o.reason == "budget" and "max_iters" or "stopped", tostring(o.reason))
            end,
            error = function(o)
                return stop("llm_call", tostring(o.kind) .. ": " .. tostring((o.detail or {}).message or o.detail))
            end,
            refused = function(o)
                return stop("llm_call", tostring(o.kind) .. ": " .. tostring((o.detail or {}).message or o.detail))
            end,
            ok = function(o)
                sink.out = o.out
                return true
            end,
        }
    end

    while true do
        -- One iteration: beats until an edit lands, the model stops asking
        -- for tools, or the turn cap — then verify, whatever the model said.
        local applied_here, answer, halted = 0, nil, false
        for turn = 1, MAX_TURNS do
            if fits and fits(s, device) ~= nil then
                stop("context", "the newest beat does not fit the model's window")
                halted = true
                break
            end
            local sink = {}
            if not Outcome.match(kernel.beat(s, device), arms(sink)) then
                halted = true
                break
            end
            answer = sink.out
            applied_here = applied_here + edits_in(s, answer.beat)
            if applied_here > 0 or not (answer.tools and #answer.tools > 0) then
                break
            end
            if turn == 3 or turn == 6 then
                s:append({
                    kind = "msg_user",
                    data = {
                        content = "You have been reading without editing; old reads are already gone. Apply an edit NOW with "
                            .. edit_spec.name
                            .. " to the region you most recently read.",
                    },
                })
            end
        end
        if halted or answer == nil then
            break
        end

        iters = iters + 1
        edits_applied = edits_applied + applied_here
        local v = verdict(s, answer)
        if v.ok then
            converged = true
            break
        end
        if iters >= MAX_ITERS then
            stop("max_iters", nil)
            break
        end

        local feedback
        if v.result.ok then
            feedback = "The verify command passes on the UNMODIFIED code, so nothing is done yet: the spec still has "
                .. "to be implemented. Read the relevant range and apply edits with "
                .. edit_spec.name
                .. "."
        else
            last_error = tostring(v.result.stderr or ""):sub(-800)
            if stalled(s) ~= nil then
                stop("stagnation", last_error)
                break
            end
            feedback = (applied_here == 0 and "No edits were applied. " or "") .. "The verify failed:\n" .. last_error
        end
        s:append({ kind = "msg_user", data = { content = feedback } })
    end
end)

local result = {
    ok = converged,
    iters = iters,
    summary = converged and string.format("PASS in %d iters", iters)
        or string.format("give-up: %s at iter %d/%d", tostring(failure_reason), iters, MAX_ITERS),
    session = session_id,
    failure_reason = (not converged) and failure_reason or nil,
    last_error = (not converged) and last_error or nil,
}
local out = std.json.encode(result)
-- The value is the answer; `ok` in it says whether the verify passed. No
-- `os.exit` on a miss: the same script is a block, and a block that did
-- its job and reports "not green" has returned normally — the manager's
-- record carries the value, an MCP caller reads it, a shell reads the line.
print("[coding_loop] " .. result.summary)
return out
