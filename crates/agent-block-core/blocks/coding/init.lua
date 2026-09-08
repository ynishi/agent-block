-- blocks/coding/init.lua — a consumer of the kernel that edits files until a
-- verify command passes.
--
-- Usage:
--   local coding = require("coding")
--   local adapter = require("knl_adapter")
--   local result = coding.run({
--       spec    = "Add a pub fn double(n: i64) -> i64, with a test.",
--       targets = { "src/lib.rs" },             -- or "a.rs, b.rs"; relative to repo
--       verify  = "cargo test --quiet",         -- runs in repo after every iteration
--       repo    = "/path/to/repo",              -- default: the process's directory
--       llm     = { port = adapter.openai, conf = { base_url = ..., model = ..., ... } },
--       iters   = 5,                            -- iterations, each ending in a verify
--       turns   = 8,                            -- beats per iteration before verify runs anyway
--       timeout = { first = 900, factor = 3, floor = 60 },  -- seconds a verify may take
--       store   = { sqlite = "/path/run.sqlite" },          -- the session's store; default the host's
--   })
--
-- result: { ok, iters, summary, session, failure_reason?, last_error? }
--
-- What this module is
--   A CONSUMER of the kernel, beside `agent`: the kernel provides one beat and
--   says a loop is written on the spot; this module writes the loop for one
--   job — change these files until that command says green — and holds no
--   guarantee the seams do not already sell. Every part is a value in a seam
--   the kernel has, and the module is the wiring:
--
--     the task            `spec`, pinned as the seed the fold keeps
--     the files           `std.fs.tool_specs` read / search_replace, path-locked
--                         to the targets; a large file enters the spec as a map
--                         of its declaration lines, a small one whole
--     what one beat sends `policy.window{ fit, keep_seed }`
--     what a tool answers `policy.result_cap` (a result may not outgrow a share
--                         of the window) and `policy.repeat_cap` (the same read
--                         again, with no edit between, is refused)
--     what a failure says `policy.carry` — one note about an edit the tool refused
--     when to stop        `policy.verdict{ run = verify, changed, timeout }` after
--                         every iteration — green counts only with an edit landed —
--                         `policy.stagnation` on the verify output, and the grant
--                         of beats on the session
--
--   `compile_loop` sold the same job as a block that owned its loop and its
--   own fold; it went in 0.38.0 when the seams could carry every part. This
--   is what stands where it stood: the seams, composed, as a consumer a
--   caller may copy (`lib/coding/init.lua` in a project shadows it, the
--   README's four-layer table says how) or call.
--
-- What it does not do
--   No retry policy of its own, no branch on a model's name, no reading of
--   what the verify printed beyond handing it back to the model: a model's
--   limits go into the seams (the `policy` header says where), and a
--   different verify is a different `verify` string. It does not commit,
--   branch, or talk to an issue tracker — the loop ends with the files
--   edited on disk and the verdict in the result; what happens to them next
--   is the caller's.
--
-- The value it answers is a table; a block that returns it to the host
-- encodes it (`std.json.encode`), which is what `examples/coding_loop.lua`
-- does and what makes the same script a `run_block` tool and a
-- `agent-block serve` job.

local kernel = require("knl")
local Outcome = kernel.Outcome
local adapter = require("knl_adapter")
local policy = require("policy")
local lshape = require("lshape")

local T = lshape.t
local shape = lshape.check

local M = {}

local DEFAULT_ITERS = 5
local DEFAULT_TURNS = 8
local DEFAULT_SEED_FULL_MAX = 16000
local DEFAULT_TIMEOUT = { first = 900, factor = 3, floor = 60 }
local DEFAULT_RESULT_SHARE = 0.25
local DEFAULT_REPEAT_MAX = 2
local STAGNATION_WINDOW = 3
local VERIFY_TAIL = 6000
local FEEDBACK_TAIL = 2000
local ERROR_TAIL = 800

--- Lines that stand for a file's structure when the file is too large to
--- seed whole: declarations, attributes, and their Lua equivalents.
local DEFAULT_ANCHORS = {
    "^%s*pub[%s(]",
    "^%s*fn%s",
    "^%s*impl[%s<]",
    "^%s*struct%s",
    "^%s*enum%s",
    "^%s*trait%s",
    "^%s*mod%s",
    "^%s*const%s",
    "^%s*static%s",
    "^%s*type%s",
    "^%s*macro_rules!",
    "^%s*#%[",
    "^%s*local%s+function",
    "^%s*function%s",
}

-- ============================================================
-- Shapes
-- ============================================================

local RUN_OPTS = T.shape({
    spec = T.string:describe("what to change: the task, as the model reads it"),
    targets = T.any:describe(
        "the files the model may read and edit: an array of paths, or one string of paths split on commas / newlines; relative paths are under `repo`"
    ),
    verify = T.string:describe("the command that says green (exit 0); runs in `repo` after every iteration"),
    repo = T.string
        :describe("the directory verify runs in and relative targets are under; default: the process's")
        :is_optional(),
    llm = T.shape({
        port = T.table:describe("a knl_adapter LLMPort — knl_adapter.openai / knl_adapter.anthropic"),
        conf = T.table:describe("the conf the port is opened with (model, base_url, api_key, ...)"),
    }):describe("the model"),
    iters = T.number:describe("iterations, each ending in a verify; default 5"):is_optional(),
    turns = T.number:describe("beats per iteration before verify runs anyway; default 8"):is_optional(),
    timeout = T.table
        :describe("policy.verdict's timeout: { first, factor?, floor? } seconds; default { 900, 3, 60 }")
        :is_optional(),
    store = T.any:describe("the session's store, as knl.session takes it; default the host's"):is_optional(),
    owner = T.string:describe('the session\'s owner; default "coding"'):is_optional(),
    system = T.string:describe("the system line; default: the one below, naming the two tools"):is_optional(),
    anchors = T.table
        :describe("Lua patterns for the lines a large file is mapped by; default: declarations and attributes")
        :is_optional(),
    seed_full_max = T.number
        :describe("a file up to this many bytes is seeded whole; larger ones as a map; default 16000")
        :is_optional(),
    result_share = T.number
        :describe("policy.result_cap's share of the window per tool result; default 0.25")
        :is_optional(),
    repeat_max = T.number:describe("policy.repeat_cap's max; default 2"):is_optional(),
})

local RUN_RESULT = T.shape({
    ok = T.boolean:describe("the verify passed with at least one edit landed"),
    iters = T.number:describe("iterations run, each ending in a verify"),
    summary = T.string:describe("one line: PASS in n iters, or give-up: reason at iter n/m"),
    session = T.string:describe("the session id the run's record is under"):is_optional(),
    failure_reason = T.string
        :describe("max_iters | stagnation | context | stopped | llm_call, when not ok")
        :is_optional(),
    last_error = T.string:describe("the tail of the last verify output or model error, when not ok"):is_optional(),
})

M.shapes = {
    run_opts = RUN_OPTS,
    run_result = RUN_RESULT,
}

-- ============================================================
-- Pure helpers — what the seed is made of
-- ============================================================

local function tail(text, n)
    return tostring(text or ""):sub(-n)
end

--- The list of target paths, absolute under `repo`: an array, or one
--- string split on commas and newlines. The `std.fs` tools resolve a path
--- against the process's directory, and the path the model passes is the
--- one the spec names, so both have to be the same absolute file.
---
--- @param targets table|string
--- @param repo string
--- @return table paths
function M.targets(targets, repo)
    repo = tostring(repo or "."):gsub("/+$", "")
    local raw = {}
    if type(targets) == "string" then
        for item in targets:gmatch("[^,\n]+") do
            raw[#raw + 1] = item
        end
    elseif type(targets) == "table" then
        for _, item in ipairs(targets) do
            raw[#raw + 1] = tostring(item)
        end
    else
        error("coding.targets: targets must be an array of paths or a string", 2)
    end
    local out = {}
    for _, item in ipairs(raw) do
        local t = item:gsub("^%s+", ""):gsub("%s+$", "")
        if t ~= "" then
            out[#out + 1] = t:sub(1, 1) == "/" and t or (repo .. "/" .. t)
        end
    end
    if #out == 0 then
        error("coding.targets: no target files", 2)
    end
    return out
end

--- `text` with each line prefixed by its 1-based number and a tab.
function M.numbered(text)
    if text == "" then
        return ""
    end
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

--- The lines of `text` that match one of `anchors`, numbered, and the
--- total line count — a map of a file too large to seed whole.
function M.structural_map(text, anchors)
    anchors = anchors or DEFAULT_ANCHORS
    local out, n = {}, 0
    for line in (text .. "\n"):gmatch("(.-)\n") do
        n = n + 1
        for _, pat in ipairs(anchors) do
            if line:match(pat) then
                out[#out + 1] = string.format("%d\t%s", n, line)
                break
            end
        end
    end
    if text:sub(-1) == "\n" then
        n = n - 1
    end
    return table.concat(out, "\n"), n
end

--- The seed: `spec`, then each target as the model should first see it —
--- whole and numbered when small, a structural map when large, absent when
--- it does not exist yet.
---
--- @param spec string
--- @param targets table  absolute paths
--- @param opts table|nil  { read?, seed_full_max?, anchors?, read_tool?, edit_tool? }
--- @return string seed
function M.seed(spec, targets, opts)
    opts = opts or {}
    local read = opts.read
        or function(path)
            local f = io.open(path, "r")
            if not f then
                return nil
            end
            local content = f:read("*a") or ""
            f:close()
            return content
        end
    local full_max = opts.seed_full_max or DEFAULT_SEED_FULL_MAX
    local read_tool = opts.read_tool or "the read tool"
    local edit_tool = opts.edit_tool or "the edit tool"
    local out = spec
    for _, path in ipairs(targets) do
        local content = read(path)
        if content ~= nil then
            if #content <= full_max then
                out = out
                    .. "\n\n## Current content of "
                    .. path
                    .. "\n(line-numbered as it is NOW; after an edit shifts lines, read the range again "
                    .. "before editing near it)\n\n"
                    .. M.numbered(content)
            else
                local map, total = M.structural_map(content, opts.anchors)
                out = out
                    .. "\n\n## Structural map of "
                    .. path
                    .. " ("
                    .. tostring(total)
                    .. " lines)\nDeclaration lines with their line numbers — not the file. Pick the region, "
                    .. "read it with "
                    .. read_tool
                    .. " (start_line / end_line, under ~150 lines), then edit with "
                    .. edit_tool
                    .. " using the exact text you saw.\n\n"
                    .. map
            end
        end
    end
    return out
end

--- The system line, naming the two tools.
function M.system(read_tool, edit_tool)
    return "You are an expert programmer editing existing files through tools, not by printing code.\n"
        .. "- "
        .. read_tool
        .. " shows a file's current content (start_line / end_line for a slice of a large one).\n"
        .. "- "
        .. edit_tool
        .. " changes it: `search` is a verbatim snippet of the CURRENT file, unique in it; `replace` is the new text. "
        .. "Keep each search small (1-10 lines); split a big change into several edits. `search_not_found` means you "
        .. "guessed the text: re-read that region and copy it exactly.\n"
        .. "- Every path must be one of the target files. Make the SMALLEST change that satisfies the spec.\n"
        .. "The verify command runs after every one of your turns whether or not you ask, and its output comes back. "
        .. "Old reads drop out of the conversation as you go; read a region, edit it at once, move on."
end

-- ============================================================
-- The loop
-- ============================================================

local function check_opts(opts)
    if type(opts) ~= "table" then
        error("coding.run: opts must be a table", 3)
    end
    for _, k in ipairs({ "spec", "verify" }) do
        if type(opts[k]) ~= "string" or opts[k] == "" then
            error("coding.run: `" .. k .. "` must be a non-empty string", 3)
        end
    end
    if opts.targets == nil then
        error("coding.run: `targets` is required", 3)
    end
    if type(opts.llm) ~= "table" or type(opts.llm.port) ~= "table" or type(opts.llm.conf) ~= "table" then
        error("coding.run: `llm` must be { port = <LLMPort>, conf = <table> }", 3)
    end
    for _, k in ipairs({ "iters", "turns" }) do
        local v = opts[k]
        if v ~= nil and (type(v) ~= "number" or v < 1 or v % 1 ~= 0) then
            error("coding.run: `" .. k .. "` must be a whole number >= 1", 3)
        end
    end
    shape.assert_dev(opts, RUN_OPTS, "coding.run opts")
end

function M._run_impl(opts)
    check_opts(opts)

    local repo = tostring(opts.repo or "."):gsub("/+$", "")
    local targets = M.targets(opts.targets, repo)
    local port, conf = opts.llm.port, opts.llm.conf
    local max_iters = opts.iters or DEFAULT_ITERS
    local max_turns = opts.turns or DEFAULT_TURNS
    local verify_cmd = opts.verify

    -- Tools: std.fs, path-locked to the targets, declared as the adapter
    -- takes them, then wrapped by the two caps. repeat_cap needs the
    -- session, so it is bound inside the session below.
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
    local size_cap = policy.result_cap({ port = port, conf = conf, share = opts.result_share or DEFAULT_RESULT_SHARE })
    local repeat_cap = policy.repeat_cap({ max = opts.repeat_max or DEFAULT_REPEAT_MAX, resets = { edit_spec.name } })

    local seed = M.seed(opts.spec, targets, {
        seed_full_max = opts.seed_full_max,
        anchors = opts.anchors,
        read_tool = read_spec.name,
        edit_tool = edit_spec.name,
    })
    local system = opts.system or M.system(read_spec.name, edit_spec.name)

    -- Verify, as a verdict: `timeout` hands the seconds to the run, `changed`
    -- withholds a green until an edit has landed.
    local edits_applied = 0
    local function verify(seconds)
        local res = sh.exec(verify_cmd, { cwd = repo, timeout = seconds })
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
        return { ok = res.code == 0, stdout = "", stderr = tail(merged, VERIFY_TAIL), exit_code = res.code }
    end
    local verdict = policy.verdict({
        run = verify,
        changed = function()
            return edits_applied > 0
        end,
        timeout = opts.timeout or DEFAULT_TIMEOUT,
    })

    --- Edits this beat applied, read off the log: a `tool_result` for the
    --- edit tool whose result says ok.
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

    local stalled = policy.stagnation({ same = STAGNATION_WINDOW, signature = verify_signature })
    local iters, converged, failure_reason, last_error, session_id = 0, false, nil, nil, nil

    kernel.session({
        owner = opts.owner or "coding",
        budget = { amount = max_iters * max_turns, tag = "beats" },
        store = opts.store,
    }, function(s)
        session_id = tostring(s:id())
        local fold, fits = policy.window({ fit = { port = port, conf = conf }, keep_seed = true })
        local device = kernel.device({
            llm = port:open(conf),
            system = system,
            tools = repeat_cap(s)(size_cap(raw_tools)),
            fold = fold,
            filters = { policy.carry({ max_bytes = 512, failed = failed_pair })(s) },
        })
        s:append({ kind = "msg_user", meta = { label = "spec" }, data = { content = seed } })

        local function stop(reason, err)
            failure_reason, last_error = reason, err
            return false
        end
        local function detail_of(o)
            local d = o.detail
            if type(d) == "table" then
                return tostring(d.kind or o.kind) .. ": " .. tostring(d.message or "unknown failure")
            end
            return tostring(o.kind) .. ": " .. tostring(d)
        end
        local arms = function(sink)
            return {
                stopped = function(o)
                    return stop(o.reason == "budget" and "max_iters" or "stopped", tostring(o.reason))
                end,
                error = function(o)
                    return stop("llm_call", tail(detail_of(o), ERROR_TAIL))
                end,
                refused = function(o)
                    return stop("llm_call", tail(detail_of(o), ERROR_TAIL))
                end,
                ok = function(o)
                    sink.out = o.out
                    return true
                end,
            }
        end

        while true do
            -- One iteration: beats until an edit lands, the model stops
            -- asking for tools, or the turn cap — then verify, whatever the
            -- model said.
            local applied_here, answer, halted = 0, nil, false
            for turn = 1, max_turns do
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
            if iters >= max_iters then
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
                last_error = tail(v.result.stderr, ERROR_TAIL)
                if stalled(s) ~= nil then
                    stop("stagnation", last_error)
                    break
                end
                feedback = (applied_here == 0 and "No edits were applied. " or "")
                    .. "The verify failed:\n"
                    .. tail(v.result.stderr, FEEDBACK_TAIL)
            end
            s:append({ kind = "msg_user", data = { content = feedback } })
        end
    end)

    return {
        ok = converged,
        iters = iters,
        summary = converged and string.format("PASS in %d iters", iters)
            or string.format("give-up: %s at iter %d/%d", tostring(failure_reason), iters, max_iters),
        session = session_id,
        failure_reason = (not converged) and failure_reason or nil,
        last_error = (not converged) and last_error or nil,
    }
end

--- Run the loop. See the header for `opts` and the result.
function M.run(opts)
    return shape.assert_dev(M._run_impl(opts), RUN_RESULT, "coding.run result")
end

return M
