-- blocks/tools/compile_loop/init.lua — the compile-and-fix loop, over `knl`.
--
-- Primary surface: `compile_loop.make(conf) -> tool_def`
--
--   local td = compile_loop.make({
--       runner    = function(path) return { ok = ..., stdout = ..., stderr = ..., exit_code = ... } end,
--       max_iters = 5,
--       edit_mode = "diff",   -- "full" (default) | "diff"
--       llm       = { provider = "anthropic", model = "...", api_key = "..." },
--   })
--   local json = td.handler({ spec = "...", target_file = "/abs/path.lua" })
--
-- What this module is
--   A CONSUMER of the kernel, like `blocks/agent`. `knl` provides one beat — a
--   model call plus the tools that call asked for — and holds no loop; this
--   module writes the loop, and the one thing it adds to it is the guarantee it
--   sells: THE VERIFY IS NOT A TOOL. `conf.runner` runs after every beat,
--   whatever the model asked for and whatever it answered, and its verdict is
--   the loop's. A tool the model can decline to call cannot carry "it compiles".
--
--   One iteration is one beat:
--
--       knl.session({ budget = { amount = max_iters } }, function(s)
--           s:append{ kind = "msg_user", ... }          -- the spec
--           while true do
--               local out = knl.beat(s, device)         -- the model, and its tools
--               ... Outcome.match ...
--               local rr = conf.runner(...)             -- the verify, always
--               s:append{ kind = "verify", beat = out.beat, data = { ... } }
--               if rr.ok then return end                -- green
--               s:append{ kind = "msg_user", ... }      -- the failure, back
--           end
--       end)
--
--   `max_iters` is the session's grant, so the iteration ceiling is the budget
--   and the beat past it comes back `stopped` with nothing called. The two
--   verdicts a run gives up on are the shell's: `policy.stagnation` over the
--   verify's stderr (the same error three times), and a count kept here of
--   iterations that applied no edit.
--
--   The loop's decisions are all in `run_loop` and nowhere else: the four
--   `Outcome.match` arms (what ends a run before the verify), the verify's own
--   verdict, the two give-up counters, and the message that goes back when it
--   failed. `make` prepares its arguments and validates what a caller got
--   wrong; `filter_for_tool_output` reads its result.
--
-- Two modes, and what each one is for
--   `edit_mode = "full"` is a whole-file rewrite: no tools, one completion an
--   iteration, the fenced block written to the target. It is the path for
--   models that cannot call tools.
--   `edit_mode = "diff"` edits through `std.fs`' path-locked tools. The target
--   files must exist: diff needs something to diff against, and a mode that
--   silently became another mode was worse than an error.
--
-- What it deliberately does not do
--   * No dump. `AGENT_BLOCK_LLM_DUMP`, the `ab.obs` iteration trail and the
--     header redaction that went with it are gone: every call is already a
--     durable fact in the session log (`llm_request` / `llm_response` /
--     `llm_call_failed`), every tool call is a recorded pair, and this loop's
--     own control flow is the `verify` events it appends. A second, lossier
--     copy on stdout was a transcription of the record rather than a reading.
--   * No transport of its own. The provider Port (`knl_adapter.anthropic` /
--     `.openai`) is the whole of it, and `conf.llm` is forwarded verbatim —
--     a whitelist here would silently strip every knob added upstream.
--   * No implicit context. There is no `_AGENT_LLM_CTX` to inherit a parent's
--     provider / model / key from: nothing is injected behind a caller's back,
--     so `conf.llm` and then the environment is the whole resolution.
--   * No distillation. A file over the size threshold answers with its length
--     and a pointer to `read_file_range` rather than an LLM-summarised digest,
--     and there is no digest cache to keep true across iterations.
--   * No registration. `make` returns the tool_def; putting it in the global
--     registry is the caller's call (`coding_agent.register_tool` does it).
--
-- The context defence: the handler's JSON never contains `code` or `history`.
-- The loop's transcript is the session log, and handing a caller a transcript
-- of every iteration contaminates its context. `TOOL_OUTPUT` is where that is
-- enforced — it is closed, so there is no field a transcript could leave by.

local M = {}

--- The kernel. Named `kernel` because the bare `knl` is the syscall bridge
--- global in a host VM and shadowing it here would read as the wrong one.
local kernel = require("knl")
local Outcome = kernel.Outcome

--- The Ports a device is built from: the `llm` closure and the tools map.
local adapter = require("knl_adapter")

--- The two values this loop plugs into the device / consults between beats.
local policy = require("policy")

local lshape = require("lshape")
local T = lshape.t
local shape = lshape.check

--- The provider Ports, by the name `conf.llm.provider` uses. The Port is
--- chosen here and nowhere else.
local PORTS = {
    anthropic = adapter.anthropic,
    openai = adapter.openai,
}

-- ============================================================
-- Boundary contracts
--
-- These were doc comments, which are worth what the next caller's attention is
-- worth. Checking is dev-mode only (LSHAPE_CHECK=1), so production pays nothing
-- and the specs run with it on.
-- ============================================================

--- What `conf.runner` hands back.
---
--- Open: a caller's runner may carry whatever else it likes, and these are the
--- keys the loop reads. `ok` is the one it branches on — a runner that returns
--- the wrong thing does not fail at the call, it fails several iterations later
--- as a run that will not converge, with the log blaming the model.
local RUNNER_RESULT = T.shape({
    ok = T.boolean,
    stdout = T.string:is_optional(),
    stderr = T.string:is_optional(),
    exit_code = T.number:is_optional(),
})

--- What the tool handler gives back to whoever called it.
---
--- Closed, and that is the point: the run carries a whole transcript in its
--- session log, and handing a caller one contaminates its context. Stated as a
--- comment, that invariant holds until someone adds a field; stated as a closed
--- shape, adding one fails.
---
--- `artifact_path` and `modified_files` are each other's alternative rather than
--- both optional in spirit: single-file mode sets the first, multi-file the
--- second. Expressed as two optionals because a run that edited nothing in
--- multi-file mode legitimately has neither.
local TOOL_OUTPUT = T.shape({
    ok = T.boolean,
    iters = T.number,
    summary = T.string,
    artifact_path = T.string:is_optional(),
    modified_files = T.array_of(T.string):is_optional(),
    failure_reason = T.string:is_optional(),
    last_error = T.string:is_optional(),
}, { open = false })

-- ============================================================
-- Internal constants
-- ============================================================

--- How many iterations in a row have to say the same thing before the loop
--- gives up: three identical verify failures, or three iterations that applied
--- no edit. Two is a retry; three is a pattern. The first is
--- `policy.stagnation`'s `same`, the second is counted here.
local STAGNATION_WINDOW = 3

--- Iterations a run gets when the caller names no ceiling. It is the session's
--- grant, so it bounds the beats and nothing else has to.
local DEFAULT_MAX_ITERS = 5

--- Bytes of file content the read tool will hand over whole. Above it the
--- answer is the file's length and a pointer to `read_file_range`: a model that
--- asked for a 400KB file did not mean to spend its context on one.
local READ_FILE_FULL_THRESHOLD = 10000

--- Lines a single `read_file_range` call may take.
local READ_FILE_RANGE_MAX_LINES = 500

--- Bytes of `last_error` the result carries. What the caller gets back is
--- bounded; the untruncated text is in the log.
local LAST_ERROR_MAX = 800

--- Bytes of verify output the next iteration's prompt carries. A failing
--- iteration must not push the spec out of the request.
local FEEDBACK_MAX = 2000

--- Bytes of `policy.carry`'s note — one rejected edit's reason and a sentence
--- around it.
local CARRY_MAX_BYTES = 512

-- ============================================================
-- The built-in prompts
-- ============================================================

local DEFAULT_SYSTEM = [[You are an expert programmer.
You will be given a spec and asked to write code that runs and passes its self-checks.
Output ONLY the complete file contents in a single fenced code block (e.g. ```lua\n...\n```).
No prose before or after the block.
On retry, output the WHOLE corrected file (not a diff). Keep changes minimal.]]

local DIFF_SYSTEM = [[You are an expert programmer editing an existing file.

Edit through the tools, not by printing code:
- fs_read to see the current content, read_file_range for a numbered slice of a
  large one. Line numbers are 1-based and are what fs_edit addresses.
- fs_edit to change it: give start_line, end_line and the expected current text
  of those lines. expect must be exact. A rejected edit tells you what is
  actually at those lines — correct it from that.
- Make the SMALLEST changes that satisfy the spec.

The build runs after every one of your turns, whether or not you ask for it,
and its output comes back to you. Keep editing until it passes.]]

-- Multi-file differs only in that a path is named on every call; the editing
-- contract is the same, so the two prompts stay parallel.
local DIFF_SYSTEM_MULTI = [[You are an expert programmer editing several existing files.

Edit through the tools, not by printing code:
- fs_read to see a file's current content, read_file_range for a numbered slice
  of a large one. Line numbers are 1-based and are what fs_edit addresses.
- fs_edit to change it: pass the path, plus edits giving start_line, end_line
  and the expected current text of those lines. expect must be exact. A
  rejected edit tells you what is actually at those lines — correct it from
  that.
- Every path must be one of the target files you were given.
- Make the SMALLEST changes that satisfy the spec.

The build runs after every one of your turns, whether or not you ask for it,
and its output comes back to you. Keep editing until it passes.]]

-- ============================================================
-- Small pure helpers
-- ============================================================

--- Resolve a path to absolute. Relative ones come from a tool call, so the
--- working directory is the only thing there is to resolve them against.
local function to_abs(path)
    if path:sub(1, 1) == "/" then
        return path
    end
    return (os.getenv("PWD") or ".") .. "/" .. path
end

--- The last `n` bytes of `text` — what a bounded field carries when the whole
--- of it does not fit. The tail rather than the head: a compiler says what went
--- wrong at the end.
local function tail(text, n)
    return tostring(text or ""):sub(-n)
end

--- A human sentence for whichever way the run ended.
local function make_summary(ok, iters, max_iters, reason)
    if ok then
        return string.format("PASS in %d iters", iters)
    end
    if reason == "stagnation" then
        return string.format(
            "give-up: stagnation at iter %d/%d (the verify said the same thing %dx)",
            iters,
            max_iters,
            STAGNATION_WINDOW
        )
    elseif reason == "no_edits_applied" then
        return string.format(
            "give-up: no edits applied in %d consecutive iters (%d/%d)",
            STAGNATION_WINDOW,
            iters,
            max_iters
        )
    elseif reason == "max_iters" then
        return string.format("give-up: max_iters reached (%d)", max_iters)
    elseif reason == "llm_call" then
        return string.format("give-up: llm_call failed at iter %d/%d", iters, max_iters)
    elseif reason == "open_target_file" then
        return string.format("give-up: open_target_file failed at iter %d/%d", iters, max_iters)
    elseif reason == "log_truncated" then
        -- The read that counts a beat's edits hit the kernel's row cap, so the
        -- loop stopped rather than reading "no edits" off a partial log.
        return string.format("give-up: the session log outgrew one read at iter %d/%d", iters, max_iters)
    end
    return string.format("give-up: %s", tostring(reason))
end

--- A path set as the sorted list the result carries.
local function collect_modified_paths(set)
    local paths = {}
    for path in pairs(set) do
        paths[#paths + 1] = path
    end
    table.sort(paths)
    return paths
end

--- Extract the FIRST fenced code block matching the lang label, falling back to
--- any fence and then to the raw text (a model that forgot the fences).
local function extract_code(text, lang)
    lang = lang or "lua"
    local m = text:match("```" .. lang .. "%s*\n(.-)\n```")
    if m then
        return m
    end
    m = text:match("```%w*%s*\n(.-)\n```")
    if m then
        return m
    end
    return text
end

--- The text blocks of a response, joined. The kernel keeps blocks because the
--- provider does; full mode wants one string to look for a fence in.
---
--- This, `error_text` and `refusal_text` below are the same functions
--- `blocks/agent` carries, character for character: reading an Outcome is
--- consumer plumbing and both consumers read it the same way. They are
--- duplicated rather than shared because a shared module would have to be an
--- embedded lib, and the two copies agree — a change to one belongs in the
--- other on the same commit.
local function text_of(content)
    local parts = {}
    for _, block in ipairs(content or {}) do
        if block.type == "text" and block.text then
            parts[#parts + 1] = block.text
        end
    end
    return table.concat(parts, "\n")
end

--- File content, or nil when the file is absent, empty or unreadable.
local function read_target_if_exists(path)
    local f = io.open(to_abs(path), "r")
    if not f then
        return nil
    end
    local content = f:read("*a")
    f:close()
    if not content or content == "" then
        return nil
    end
    return content
end

--- Write `content` to `path`, checking the open and the write. Returns true, or
--- false and the reason.
local function write_file(path, content)
    local f, oerr = io.open(path, "w")
    if not f then
        return false, tostring(oerr)
    end
    local wok, werr = f:write(content)
    f:close()
    if not wok then
        return false, tostring(werr or "write failed")
    end
    return true
end

--- The failure-feedback user message for full mode.
---
--- Only the spec and the build's output: no tool names, no JSON schema, no
--- tool_use vocabulary. Full mode's whole action space is a fenced block.
local function build_failure_msg(lang, rr)
    return string.format(
        "Run FAILED. Fix the code and re-output the WHOLE corrected file in a single ```%s ... ``` block.\n\n=== stdout ===\n%s\n\n=== stderr ===\n%s\n\n=== exit_code ===\n%s",
        lang,
        tail(rr.stdout, FEEDBACK_MAX),
        tail(rr.stderr, FEEDBACK_MAX),
        tostring(rr.exit_code or "unknown")
    )
end

--- The failure-feedback user message for diff mode.
local function build_verify_msg(rr)
    return string.format(
        "The build FAILED after your edits. Fix it with more edits.\n\n=== stdout ===\n%s\n\n=== stderr ===\n%s\n\n=== exit_code ===\n%s",
        tail(rr.stdout, FEEDBACK_MAX),
        tail(rr.stderr, FEEDBACK_MAX),
        tostring(rr.exit_code or "unknown")
    )
end

--- The result, filtered for the tool boundary: `code` and the transcript are
--- not in `TOOL_OUTPUT`, so they cannot leave through it.
local function filter_for_tool_output(res)
    return shape.assert_dev({
        ok = res.ok,
        artifact_path = res.artifact_path,
        modified_files = res.modified_files,
        iters = res.iters,
        summary = res.summary,
        failure_reason = res.failure_reason,
        last_error = res.last_error,
    }, TOOL_OUTPUT, "compile_loop tool output")
end

--- Prefix each line with its 1-based number.
---
--- `fs_edit` addresses lines, so a read that feeds it has to say which line is
--- which. Only ever applied to verbatim content.
local function with_line_numbers(text, first_line)
    local out = {}
    local n = (first_line or 1) - 1
    for line in (text .. "\n"):gmatch("(.-)\n") do
        n = n + 1
        table.insert(out, string.format("%d\t%s", n, line))
    end
    -- gmatch on text .. "\n" yields one trailing empty element for text that
    -- already ended in a newline; drop it so no phantom line is numbered.
    if #out > 0 and out[#out]:match("^%d+\t$") and text:sub(-1) == "\n" then
        table.remove(out)
    end
    return table.concat(out, "\n")
end

-- ============================================================
-- The tools diff mode hands the model
-- ============================================================

--- The verbatim range read.
---
--- `std.fs`' own read takes a range too, and this one exists beside it for the
--- case that one refuses: a file over the threshold. It never consults a
--- digest, a cache or a summary — a range is the file, or it is nothing.
local RANGE_TOOL = {
    name = "read_file_range",
    description = "Read a verbatim, line-numbered range of a target file. "
        .. "Use it when fs_read answers that the file is too large. "
        .. "1-indexed and inclusive; line_end - line_start + 1 must be at most "
        .. tostring(READ_FILE_RANGE_MAX_LINES)
        .. ".",
    input_schema = {
        type = "object",
        required = { "path", "line_start", "line_end" },
        properties = {
            path = { type = "string", description = "Absolute path. Must be one of the target files." },
            line_start = { type = "integer", description = "1-indexed start line, inclusive." },
            line_end = { type = "integer", description = "1-indexed end line, inclusive." },
        },
    },
}

--- Read `[line_start, line_end]` of `path`, verbatim.
---
--- Returns `{ ok = true, content, first_line }` or `{ ok = false, error }`; a
--- refusal is data the model can correct from rather than a raise.
local function read_file_range_handler(path, line_start, line_end, allowed)
    if not allowed[path] then
        return { ok = false, error = "path '" .. tostring(path) .. "' is not one of the target files" }
    end
    if
        type(line_start) ~= "number"
        or type(line_end) ~= "number"
        or math.floor(line_start) ~= line_start
        or math.floor(line_end) ~= line_end
    then
        return { ok = false, error = "line_start and line_end must be integers" }
    end
    if line_start < 1 or line_end < line_start then
        return { ok = false, error = "invalid range: require 1 <= line_start <= line_end" }
    end
    if (line_end - line_start + 1) > READ_FILE_RANGE_MAX_LINES then
        return {
            ok = false,
            error = string.format("range %d-%d is more than %d lines", line_start, line_end, READ_FILE_RANGE_MAX_LINES),
        }
    end
    local f, open_err = io.open(path, "r")
    if not f then
        return { ok = false, error = "cannot open: " .. tostring(open_err) }
    end
    local lines = {}
    local cur = 0
    for line in f:lines() do
        cur = cur + 1
        if cur >= line_start then
            table.insert(lines, line)
        end
        if cur >= line_end then
            break
        end
    end
    f:close()
    if cur < line_start then
        return { ok = false, error = string.format("file has %d lines; line_start=%d is past the end", cur, line_start) }
    end
    return { ok = true, content = table.concat(lines, "\n"), first_line = line_start }
end

--- `std.fs`' read, with the one branch this loop adds: a whole-file read of
--- something over the threshold answers with its length instead of its content.
---
--- The answer names the tool that will still work, so a model that meets this
--- has somewhere to go. A ranged read passes through untouched — that is the
--- request this branch is pointing at.
local function sized_read(handler)
    return function(input)
        input = input or {}
        local res = handler(input)
        if input.start_line ~= nil or type(res) ~= "table" or type(res.content) ~= "string" then
            return res
        end
        if #res.content <= READ_FILE_FULL_THRESHOLD then
            return res
        end
        return {
            ok = false,
            reason = "too_large",
            error = string.format(
                "too large: %d lines. Read a range instead — fs_read with start_line / end_line, or %s.",
                res.lines or 0,
                RANGE_TOOL.name
            ),
        }
    end
end

--- One `conf.extra_tools` entry as the flat spec `knl_adapter.tools` binds.
---
--- Two accepted shapes, because the agent layer's nested one
--- (`{ name, schema = { description, input_schema }, handler }`) is what
--- callers have always written. A flat entry passes through; a flat entry that
--- spells the schema field `schema` reaches `knl_adapter`'s loud error rather
--- than the provider with no schema.
---
--- `blocks/agent`'s `extra_candidates` does the same normalization with one
--- deliberate difference: there, an entry with no handler is a DECLARATION that
--- dispatches through the Lua registry. Here `make` has already refused it —
--- this loop's device carries the tools it built and no registry behind them.
local function extra_spec(t)
    if t.schema ~= nil then
        return {
            name = t.name,
            description = t.schema.description,
            input_schema = t.schema.input_schema,
            handler = t.handler,
        }
    end
    return {
        name = t.name,
        description = t.description,
        input_schema = t.input_schema,
        handler = t.handler,
    }
end

--- The tools map for diff mode: `std.fs`' path-locked read and edit, the range
--- read, and whatever the caller added.
---
--- `tool_specs` rather than `register_tools` — the registry is global and this
--- lock is per-invocation. `read_only` withholds the edit tool: it can inspect
--- and it cannot converge, so it is a dry run rather than a fix.
---
--- A name claimed twice raises out of `knl_adapter.tools`, which is what makes
--- a caller's `get_hint` colliding with `fs_edit` a wiring error rather than a
--- silent winner.
---
--- @return table tools  the device's tools map
--- @return string edit_name  the name applied edits are counted under
local function build_tools(conf, targets, allowed)
    local read_spec = std.fs.tool_specs({ allowed = { "read" }, path_lock = targets })[1]
    local edit_spec = std.fs.tool_specs({ allowed = { "edit" }, path_lock = targets })[1]

    local specs = {
        {
            name = read_spec.name,
            description = read_spec.description,
            input_schema = read_spec.input_schema,
            handler = sized_read(read_spec.handler),
        },
        {
            name = RANGE_TOOL.name,
            description = RANGE_TOOL.description,
            input_schema = RANGE_TOOL.input_schema,
            handler = function(input)
                input = input or {}
                local res = read_file_range_handler(input.path or "", input.line_start, input.line_end, allowed)
                if not res.ok then
                    return res
                end
                return with_line_numbers(res.content, res.first_line)
            end,
        },
    }
    if conf.tool_mode ~= "read_only" then
        specs[#specs + 1] = edit_spec
    end
    for _, t in ipairs(conf.extra_tools or {}) do
        specs[#specs + 1] = extra_spec(t)
    end
    return adapter.tools(specs), edit_spec.name
end

-- ============================================================
-- Reading the run back
-- ============================================================

--- The edits one beat landed, and the paths they landed in.
---
--- Read off the log rather than counted in a wrapper: `std.fs` reports a
--- rejected edit by RETURNING `{ ok = false, reason = ... }`, so the kernel
--- closes the pair `ok = true` (nothing raised) and the tool's own verdict is
--- in the recorded result. What an edit MEANT to this loop is the loop's, and
--- the log is where it reads it from.
---
--- A READ THAT WAS CUT SHORT IS NOT AN ANSWER OF ZERO. `session:events()` is
--- bounded and reports it (knl's header), and the cap counts forward from the
--- start of the log — so what a truncated read is missing is the END, which is
--- exactly the beat this is looking for. Folding it would report "no edits
--- applied" for a beat that edited every target, and the loop would give up
--- (or count a stagnation) over a hole in the read. A bounded tail read is no
--- way out either: the bound is in EVENTS and a beat writes one pair per tool
--- call, so a window can begin in the middle of this beat and lose the
--- `tool_call` half of a pair whose `tool_result` it kept — the same wrong
--- count, arrived at more quietly. So it answers `nil, <why>` and the loop
--- ends the run with it.
---
--- @param session userdata|table  a knl session
--- @param beat_id string  the beat whose pairs to read
--- @param edit_name string  the edit tool's name
--- @param modified table  path set, added to
--- @return number|nil  how many edits applied, or nil when the log did not fit
--- @return string|nil  why, when the count could not be taken
local function beat_edits(session, beat_id, edit_name, modified)
    local edited_by_call = {}
    local applied = 0
    local events, truncated = session:events()
    if truncated then
        return nil,
            "the session log is longer than one read of it ("
                .. #events
                .. " events, the kernel's row cap), so this beat's edits cannot be counted"
    end
    for _, ev in ipairs(events) do
        if ev.beat == beat_id then
            local data = type(ev.data) == "table" and ev.data or {}
            -- A pair is looked up by its call id, so a record that carries none
            -- is skipped rather than indexed: a nil key is a raise, and a
            -- provider that named no id is the kernel's problem to report, not
            -- this loop's to crash on.
            if ev.kind == "tool_call" and data.name == edit_name and data.call_id ~= nil then
                local args = type(data.args) == "table" and data.args or {}
                edited_by_call[data.call_id] = args.path or ""
            elseif ev.kind == "tool_result" and data.call_id ~= nil then
                local path = edited_by_call[data.call_id]
                if path ~= nil and type(data.result) == "table" and data.result.ok == true then
                    applied = applied + 1
                    if path ~= "" then
                        modified[path] = true
                    end
                end
            end
        end
    end
    return applied
end

--- What a tool pair says about itself, for `policy.carry`.
---
--- The kernel's `ok` flag catches a handler that RAISED; `std.fs` rejects an
--- edit by returning one, which is the failure this loop most needs carried
--- forward — asking the same wrong thing again is what the note is for.
local function pair_failed(pair)
    if pair.ok == false then
        return true
    end
    return type(pair.result) == "table" and pair.result.ok == false
end

--- The verify's stderr, as the signature `policy.stagnation` compares beats by.
---
--- The verify is part of its beat (the loop stamps the beat id on it), so the
--- question "did the build say the same thing three times" is one the policy
--- can answer off the log. A beat with no verify has no signature and cannot be
--- part of a repetition.
local function verify_signature(beat)
    for _, ev in ipairs(beat.events) do
        if ev.kind == "verify" then
            local data = type(ev.data) == "table" and ev.data or {}
            return tostring(data.stderr or "")
        end
    end
    return nil
end

--- A failed beat as one sentence: the stage, the classification the port or the
--- kernel put on it, and the message.
local function error_text(o)
    local detail = o.detail
    if type(detail) ~= "table" then
        return tostring(o.kind) .. ": " .. tostring(detail)
    end
    local message = tostring(detail.message or "unknown failure")
    if detail.kind ~= nil then
        return tostring(o.kind) .. ": " .. tostring(detail.kind) .. ": " .. message
    end
    return tostring(o.kind) .. ": " .. message
end

--- A refusal names its class: the adapter's provider-neutral classification,
--- plus what the provider said when it said anything.
local function refusal_text(o)
    local text = "model refused to respond (kind=" .. tostring(o.reason) .. ")"
    local detail = o.detail
    local said = type(detail) == "table" and type(detail.refusal) == "table" and detail.refusal.detail or nil
    if type(said) == "string" and said ~= "" then
        text = text .. ": " .. said
    end
    return text
end

--- The caller's per-iteration hook. A broken callback must not take the run
--- down with it.
local function fire_on_iter(on_iter, entry)
    if not on_iter then
        return
    end
    local ok, err = pcall(on_iter, entry)
    if not ok then
        log.warn("compile_loop: on_iter callback error: " .. tostring(err))
    end
end

-- ============================================================
-- The port conf
-- ============================================================

--- Everything the Port is opened with: `conf.llm` verbatim, with one
--- translation.
---
--- `disable_thinking` is this block's own word for Qwen's switch and predates
--- the shared vocabulary; it becomes `thinking = { enabled = false }`, which is
--- what `llm_proto` reads. An explicit `thinking` wins. Everything else is
--- forwarded untouched — a whitelist here would silently strip every knob added
--- to `llm_proto` upstream, and the key resolution (`api_key` / `api_key_env`,
--- then the environment) is `llm_proto`'s and not restated here.
local function port_conf(llm)
    llm = llm or {}
    local conf = {}
    for key, value in pairs(llm) do
        if key ~= "disable_thinking" then
            conf[key] = value
        end
    end
    if conf.thinking == nil and llm.disable_thinking then
        conf.thinking = { enabled = false }
    end
    return conf
end

-- ============================================================
-- The loop
-- ============================================================

--- The first user message: the spec, plus what the mode needs beside it.
local function seed_content(conf, targets, diff, lang)
    if diff then
        local lines = {}
        for _, p in ipairs(targets) do
            lines[#lines + 1] = "  " .. p
        end
        return conf.spec .. "\n\nFiles:\n" .. table.concat(lines, "\n")
    end
    local existing = read_target_if_exists(targets[1])
    if existing then
        return conf.spec .. "\n\n=== Current file content ===\n```" .. lang .. "\n" .. existing .. "\n```"
    end
    return conf.spec
end

--- Run the loop. Returns the internal result (the filtered one is the
--- handler's); raises only for what a caller got wrong — a runner that answered
--- the wrong shape, a duplicate tool name, a store that would not take the log.
---
--- conf, all resolved before entry: runner, spec, target_files (list of
--- absolute paths), multi_file, lang, max_iters, system, edit_mode, tool_mode,
--- extra_tools, on_iter, llm.
local function run_loop(conf)
    local lang = conf.lang or "lua"
    local max_iters = conf.max_iters or DEFAULT_MAX_ITERS
    local targets = conf.target_files
    local multi = conf.multi_file or false
    local diff = conf.edit_mode == "diff"
    local artifact_path = (not multi) and targets[1] or nil

    local allowed = {}
    for _, p in ipairs(targets) do
        allowed[p] = true
    end

    local system = conf.system
    if system == nil then
        if not diff then
            system = DEFAULT_SYSTEM
        elseif multi then
            system = DIFF_SYSTEM_MULTI
        else
            system = DIFF_SYSTEM
        end
    end

    local tools, edit_name
    if diff then
        tools, edit_name = build_tools(conf, targets, allowed)
    end

    local llm = PORTS[conf.llm and conf.llm.provider or "anthropic"]:open(port_conf(conf.llm))
    local stalled = policy.stagnation({ same = STAGNATION_WINDOW, signature = verify_signature })

    local modified = {}
    local iters, converged = 0, false
    local failure_reason, last_error = nil, nil

    kernel.session({
        owner = "compile_loop",
        -- One unit per beat (the device's default cost), so the grant IS the
        -- iteration ceiling: the beat after the last one the caller allowed
        -- comes back `stopped` with nothing called and nothing recorded.
        budget = { amount = max_iters, tag = "iterations", desc = "one unit per iteration" },
    }, function(s)
        local device = kernel.device({
            llm = llm,
            system = system,
            tools = tools,
            -- What the model asked for and was refused, carried forward as one
            -- bounded note. Diff mode only: it is the rejected edit that this
            -- answers, and full mode has no tools to reject anything.
            filters = diff and { policy.carry({ max_bytes = CARRY_MAX_BYTES, failed = pair_failed })(s) } or nil,
        })

        s:append({
            kind = "msg_user",
            meta = { label = "spec" },
            data = { content = seed_content(conf, targets, diff, lang) },
        })

        local zero_edits = 0

        while true do
            local answer
            local going = Outcome.match(kernel.beat(s, device), {
                stopped = function(o)
                    -- Not a failure and not the model's doing: the quota would
                    -- not cover another beat. The only grant here is the
                    -- iteration ceiling, so that is what it says.
                    if o.reason == "budget" then
                        failure_reason = "max_iters"
                    else
                        failure_reason = "stopped"
                        last_error = tostring(o.reason)
                    end
                    return false
                end,
                error = function(o)
                    -- A transport or protocol failure is not the model failing
                    -- to edit: it ends the run instead of counting as an
                    -- iteration that got nowhere.
                    failure_reason = "llm_call"
                    last_error = tail(error_text(o), LAST_ERROR_MAX)
                    return false
                end,
                refused = function(o)
                    failure_reason = "llm_call"
                    last_error = tail(refusal_text(o), LAST_ERROR_MAX)
                    return false
                end,
                ok = function(o)
                    answer = o.out
                    return true
                end,
            })
            if not going then
                break
            end

            iters = iters + 1
            local raw = text_of(answer.content)
            local code, applied = nil, 0

            if diff then
                local unread
                applied, unread = beat_edits(s, answer.beat, edit_name, modified)
                if applied == nil then
                    -- Not "no edits": the count could not be taken at all, so
                    -- the run ends here rather than giving up over a hole.
                    failure_reason = "log_truncated"
                    last_error = tail(unread, LAST_ERROR_MAX)
                    break
                end
            else
                -- A response cut off at max_tokens is half a file. Writing it
                -- would hand the runner code the model never finished, and the
                -- runner would then blame the model for a syntax error the
                -- transport caused.
                if answer.stop_reason == "max_tokens" then
                    failure_reason = "llm_call"
                    last_error = "response truncated at max_tokens; nothing written (raise llm.max_tokens)"
                    break
                end
                code = extract_code(raw, lang)
                if #code > 0 then
                    applied = 1
                    local wok, werr = write_file(targets[1], code)
                    if not wok then
                        failure_reason = "open_target_file"
                        last_error = tail(werr, LAST_ERROR_MAX)
                        break
                    end
                    modified[targets[1]] = true
                end
            end

            -- The verify. The loop's step and never the model's: it runs after
            -- every beat, whatever was asked for and whatever landed.
            local rr = shape.assert_dev(
                conf.runner(multi and targets or targets[1]),
                RUNNER_RESULT,
                "compile_loop conf.runner result"
            ) or {}
            s:append({
                kind = "verify",
                -- Stamped with the beat it judges, so the record reads back as
                -- part of it — which is what lets a stagnation signature see it.
                beat = answer.beat,
                data = {
                    ok = rr.ok == true,
                    stdout = tostring(rr.stdout or ""),
                    stderr = tostring(rr.stderr or ""),
                    exit_code = rr.exit_code,
                    iteration = iters,
                },
            })
            fire_on_iter(conf.on_iter, { iter = iters, code = code, result = rr, raw = raw })

            if rr.ok then
                converged = true
                break
            end
            last_error = tail(rr.stderr, LAST_ERROR_MAX)

            -- The model is calling tools and nothing is landing. Distinct from
            -- "the same error three times": the run is busy, not stuck.
            if applied == 0 then
                zero_edits = zero_edits + 1
            else
                zero_edits = 0
            end
            if zero_edits >= STAGNATION_WINDOW then
                failure_reason = "no_edits_applied"
                break
            end

            if stalled(s) ~= nil then
                failure_reason = "stagnation"
                break
            end

            -- The failure goes back as the next user message. The verify is a
            -- caller's kind, which the fold skips — a record of what happened
            -- rather than a message — so what the model is told is said here.
            local feedback
            if not diff then
                if applied == 0 then
                    feedback = "Your previous attempt produced no usable code."
                        .. " Output the whole corrected file in a single fenced code block."
                else
                    feedback = build_failure_msg(lang, rr)
                end
            elseif applied == 0 then
                feedback = "No edits were applied. Re-read the file — fs_read numbers its lines —"
                    .. " and retry with an exact expect.\n\n"
                    .. build_verify_msg(rr)
            else
                feedback = build_verify_msg(rr)
            end
            s:append({ kind = "msg_user", data = { content = feedback } })
        end
    end)

    return {
        ok = converged,
        iters = iters,
        summary = make_summary(converged, iters, max_iters, failure_reason),
        artifact_path = artifact_path,
        modified_files = diff and collect_modified_paths(modified) or nil,
        failure_reason = (not converged) and failure_reason or nil,
        last_error = (not converged) and last_error or nil,
    }
end

-- ============================================================
-- Public API
-- ============================================================

--- Build the tool_def: `{ name, schema, handler }`.
---
--- The def is returned and nothing else happens to it. Registering it is the
--- caller's — `agent.run{ extra_tools = { td } }` takes it directly, and a
--- caller that wants it in the global registry puts it there itself. A factory
--- that registered on the side made two runs in one process collide on a name
--- neither of them chose.
---
--- What the caller got wrong is loud here rather than five iterations in: the
--- runner, the two modes, the provider, and the shape of every extra tool.
---
--- @param conf table  { runner, llm?, max_iters?, lang?, name?, system?,
---                      edit_mode?, tool_mode?, extra_tools?, on_iter? }
--- @return table tool_def  { name, schema, handler }
function M.make(conf)
    assert(type(conf) == "table", "conf table required")
    assert(type(conf.runner) == "function", "conf.runner function required")

    local edit_mode = conf.edit_mode or "full"
    assert(edit_mode == "full" or edit_mode == "diff", "conf.edit_mode must be 'full' or 'diff'")

    -- tool_mode (diff mode only): "auto" declares fs_read / read_file_range /
    -- fs_edit, "read_only" declares only the reads — it can inspect but never
    -- converge, so it is a dry run rather than a fix.
    local tool_mode = conf.tool_mode or "auto"
    assert(
        tool_mode == "auto" or tool_mode == "read_only",
        "conf.tool_mode must be 'auto' or 'read_only' (use edit_mode='full' for a no-tools run)"
    )

    local provider = (conf.llm and conf.llm.provider) or "anthropic"
    assert(PORTS[provider] ~= nil, "unknown conf.llm.provider '" .. tostring(provider) .. "' (anthropic | openai)")

    -- extra_tools: the agent layer's nested form or a flat spec. A name that
    -- collides with a built-in is caught by `knl_adapter.tools` when the map is
    -- built, so there is no reserved list to keep in step with the tool set.
    if conf.extra_tools ~= nil then
        assert(type(conf.extra_tools) == "table", "conf.extra_tools must be a list")
        for i, t in ipairs(conf.extra_tools) do
            assert(type(t) == "table", "conf.extra_tools[" .. i .. "] must be a table")
            assert(
                type(t.name) == "string" and t.name ~= "",
                "conf.extra_tools[" .. i .. "].name must be a non-empty string"
            )
            assert(type(t.handler) == "function", "conf.extra_tools[" .. i .. "].handler must be a function")
            assert(
                t.schema == nil or type(t.schema) == "table",
                "conf.extra_tools[" .. i .. "].schema must be a table when present"
            )
        end
    end

    local name = conf.name or "compile_loop"

    local schema = {
        description = [[Run an autonomous compile-and-fix loop: a child LLM edits the
target file (through tools in diff mode, or by emitting the whole file in full
mode), the runner executes it after every turn, and its output is fed back
until the run passes or the give-up gate triggers. Returns ok/iters/summary
and, on failure, failure_reason/last_error.

Single-file mode: provide target_file (string).
Multi-file mode: provide target_files (array of absolute paths). Requires edit_mode=diff.
target_file and target_files are mutually exclusive.]],
        input_schema = {
            type = "object",
            required = { "spec" },
            properties = {
                spec = {
                    type = "string",
                    description = "Full specification the child LLM must satisfy.",
                },
                target_file = {
                    type = "string",
                    description = "Absolute path of the file (single-file mode). Read on entry if it already exists, then written on each iteration. Mutually exclusive with target_files.",
                },
                target_files = {
                    type = "array",
                    items = { type = "string" },
                    description = "Array of absolute paths (multi-file mode). Mutually exclusive with target_file. Multi-file mode requires edit_mode=diff.",
                },
                lang = {
                    type = "string",
                    description = "Code fence language label (default: lua).",
                },
            },
        },
    }

    local function handler(input)
        assert(not (input.target_file and input.target_files), "target_file and target_files are mutually exclusive")
        assert(input.target_file or input.target_files, "target_file (string) or target_files (array) is required")

        local multi_file, files_list
        if input.target_files then
            multi_file = true
            assert(type(input.target_files) == "table", "target_files must be an array")
            assert(#input.target_files > 0, "target_files must not be empty")
            files_list = {}
            for i, v in ipairs(input.target_files) do
                assert(type(v) == "string", "target_files[" .. i .. "] must be a string")
                files_list[#files_list + 1] = to_abs(v)
            end
        else
            multi_file = false
            files_list = { to_abs(input.target_file) }
        end

        assert(not (multi_file and edit_mode == "full"), "multi-file mode requires edit_mode=diff")

        -- Diff needs something to diff against. This used to fall back to full
        -- mode with a warning, which meant a caller asking for minimal edits
        -- could silently get its file rewritten instead.
        if edit_mode == "diff" then
            for _, p in ipairs(files_list) do
                assert(
                    read_target_if_exists(p) ~= nil,
                    "edit_mode='diff' requires an existing, non-empty target file: " .. p
                )
            end
        end

        local res = run_loop({
            runner = conf.runner,
            spec = input.spec,
            target_files = files_list,
            multi_file = multi_file,
            lang = input.lang or conf.lang or "lua",
            max_iters = conf.max_iters,
            system = conf.system,
            edit_mode = edit_mode,
            tool_mode = tool_mode,
            extra_tools = conf.extra_tools,
            on_iter = conf.on_iter,
            llm = conf.llm,
        })

        local enc_ok, enc_str = pcall(std.json.encode, filter_for_tool_output(res))
        if enc_ok then
            return enc_str
        end
        return '{"ok":false,"failure_reason":"encode_failed","iters":0,"summary":"json encode failed"}'
    end

    return { name = name, schema = schema, handler = handler }
end

--- The contracts this module holds callers to, as data.
---
--- Public rather than test-only: a caller writing a `runner` should be able to
--- check its own return against the same schema the loop checks it against,
--- instead of reading a doc comment and hoping.
M.shapes = {
    runner_result = RUNNER_RESULT,
    tool_output = TOOL_OUTPUT,
}

--- The pure helpers, for the specs. Everything here is a string / table
--- transform over its arguments: what the loop itself does needs the kernel,
--- and is covered by the e2e tests that give it one.
function M._test_helpers()
    return {
        extract_code = extract_code,
        text_of = text_of,
        make_summary = make_summary,
        build_failure_msg = build_failure_msg,
        build_verify_msg = build_verify_msg,
        filter_for_tool_output = filter_for_tool_output,
        collect_modified_paths = collect_modified_paths,
        with_line_numbers = with_line_numbers,
        read_file_range_handler = read_file_range_handler,
        sized_read = sized_read,
        extra_spec = extra_spec,
        pair_failed = pair_failed,
        verify_signature = verify_signature,
        port_conf = port_conf,
        -- Reads a session, but only through `events()`, so a table answering
        -- that one method is a whole stand-in — no kernel needed to hold it to
        -- what it counts, or to what it refuses to count.
        beat_edits = beat_edits,
    }
end

return M
