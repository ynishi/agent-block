-- Fixture: `coding.run` end to end against the scripted in-process mock.
--
-- The Rust side (tests/e2e_coding.rs) makes the repository, writes the one
-- target file, scripts what the model answers on each call, and reads the
-- facts back off the marker lines below. Everything this fixture decides is
-- read off the environment, so one fixture serves every case:
--
--   OPENAI_BASE_URL_TEST  the scripted mock's base url
--   CODING_REPO_TEST      the temporary repository (holds `lib.lua`)
--   CODING_DONE_TEST      "declare" (default) | "plan"
--   CODING_OMIT_RESERVE   "1": run with neither `reserve` nor `max_tokens`,
--                         which is the tripwire case — no model is called
--   CODING_EDIT_OPS_TEST  comma-separated std.fs edit ops, e.g.
--                         "search_replace,write,append"; default the module's
--   CODING_SEED_TEST      "full" (default) | "names"
--   CODING_CALL_RESERVE_TEST  tokens kept out of the reasoning for the call;
--                         set, the conf also asks for thinking, so the vllm
--                         dialect puts `thinking_token_budget` on the wire
--   CODING_DIALECT_TEST   the conf's dialect; default "vllm", whose count is
--                         the mock's fixed `/tokenize` answer. "openai" asks
--                         the server for no count, so the Port estimates from
--                         the bytes and a result's size is what it costs
--   CODING_EXTRA_TARGETS_TEST  comma-separated files in the repository the
--                         run may also touch, after `lib.lua` (the tools are
--                         path-locked to the targets, so a read of any other
--                         file is refused before a cap sees it)
--   CODING_CHECKPOINT_TEST  "0": run with `checkpoint = false`
--   CODING_FORK_EVENTS_TEST  a file of a finished run's events, one JSON
--                         object a line: the run is `coding.fork` from them
--                         instead of `coding.run`, into CODING_REPO_TEST (a
--                         fresh directory), at the beat CODING_FORK_BEAT_TEST
--                         names ("first" for the first one that recorded a
--                         state), with CODING_FORK_PARENT_TEST as the parent's
--                         session id
--   CODING_RESTORE_EVENTS_TEST  a file of a finished run's events, one JSON
--                         object a line (`agent-block knl export --as
--                         events`): no run and no model — the fixture lists
--                         the recorded states and restores the one
--                         CODING_RESTORE_BEAT_TEST names ("baseline", or
--                         "last" for the latest one recorded)
--
-- One marker line per fact, so a Rust assertion names the fact and not a
-- position in the output.

local repo = std.env.get("CODING_REPO_TEST")
assert(repo, "CODING_REPO_TEST must be set")

local restore_from = std.env.get("CODING_RESTORE_EVENTS_TEST")
if restore_from then
    local coding = require("coding")
    local policy = require("policy")
    local events = {}
    for line in std.fs.read(restore_from):gmatch("[^\n]+") do
        events[#events + 1] = std.json.decode(line)
    end
    local beats = {}
    for _, point in ipairs(policy.checkpoints(events)) do
        beats[#beats + 1] = point.beat
    end
    print("checkpoint.beats=" .. table.concat(beats, ","))
    local beat = std.env.get("CODING_RESTORE_BEAT_TEST") or "baseline"
    if beat == "last" then
        beat = beats[#beats]
    end
    local r = coding.restore(events, beat)
    print("restored=" .. table.concat(r.restored, ","))
    print("restore.missing=" .. #r.missing)
    print("CODING_MOCK_RESTORED")
    return
end

local base_url = std.env.get("OPENAI_BASE_URL_TEST")
assert(base_url, "OPENAI_BASE_URL_TEST must be set")
local done_mode = std.env.get("CODING_DONE_TEST") or "declare"
local omit_reserve = std.env.get("CODING_OMIT_RESERVE") == "1"
local edit_ops = std.env.get("CODING_EDIT_OPS_TEST")
local seed_mode = std.env.get("CODING_SEED_TEST")
local call_reserve = tonumber(std.env.get("CODING_CALL_RESERVE_TEST") or "")
local extra_targets = std.env.get("CODING_EXTRA_TARGETS_TEST")
local dialect = std.env.get("CODING_DIALECT_TEST") or "vllm"
local checkpoint_off = std.env.get("CODING_CHECKPOINT_TEST") == "0"

local coding = require("coding")
local adapter = require("knl_adapter")

local opts = {
    spec = "Make `double` return n * 2 instead of nil.",
    targets = { "lib.lua" },
    -- A fixed-string grep, so the verify is the file's content and nothing
    -- else: no toolchain, no network, and the same answer on every machine.
    verify = "grep -qF 'return n * 2' lib.lua",
    repo = repo,
    llm = {
        port = adapter.openai,
        conf = {
            base_url = base_url,
            api_key = "dummy",
            model = "mock",
            dialect = dialect,
            timeout = 30,
            -- Declared rather than discovered, so `config.values.context_window`
            -- reads `from = "caller"` and the assertion has something to name.
            context_window = 32768,
        },
    },
    reserve = 1024,
    iters = 3,
    turns = 4,
    baseline = true,
    done = done_mode,
}
if done_mode == "plan" then
    opts.check_timeout = 10
end
if seed_mode then
    opts.seed = seed_mode
end
if call_reserve then
    opts.call_reserve = call_reserve
    -- The budget only reaches the wire when the request asks for reasoning at
    -- all, and only on the vllm dialect this conf already names.
    opts.llm.conf.thinking = { enabled = true }
end
if extra_targets then
    for name in extra_targets:gmatch("[^,]+") do
        opts.targets[#opts.targets + 1] = name
    end
end
if edit_ops then
    local edit = {}
    for op in edit_ops:gmatch("[^,]+") do
        edit[#edit + 1] = op
    end
    opts.ops = { read = "read", edit = edit }
end
if omit_reserve then
    opts.reserve = nil
end
if checkpoint_off then
    opts.checkpoint = false
end

local fork_from = std.env.get("CODING_FORK_EVENTS_TEST")
local ok, result
if fork_from then
    local policy = require("policy")
    local events = {}
    for line in std.fs.read(fork_from):gmatch("[^\n]+") do
        events[#events + 1] = std.json.decode(line)
    end
    local beat = std.env.get("CODING_FORK_BEAT_TEST") or "first"
    if beat == "first" then
        beat = policy.checkpoints(events)[2].beat
    end
    -- The task is the parent's seed, copied with its history.
    opts.spec = nil
    opts.parent = std.env.get("CODING_FORK_PARENT_TEST")
    opts.reason = "e2e"
    print("fork.beat=" .. beat)
    ok, result = pcall(coding.fork, events, beat, opts)
else
    ok, result = pcall(coding.run, opts)
end
if not ok then
    print("refused=" .. tostring(result))
    print("CODING_MOCK_REFUSED")
    return
end

print("ok=" .. tostring(result.ok))
print("iters=" .. tostring(result.iters))
print("failure_reason=" .. tostring(result.failure_reason))
print("done=" .. tostring(result.done))
print("baseline_ok=" .. tostring(result.baseline_ok))
print("session=" .. tostring(result.session))
print("checkpoints=" .. tostring(result.checkpoints))
if result.plan then
    print(
        ("plan.total=%s plan.passed=%s plan.filed=%s"):format(
            tostring(result.plan.total),
            tostring(result.plan.passed),
            tostring(result.plan.filed)
        )
    )
end
print("config.values.iters.from=" .. tostring(result.config.values.iters.from))
print("config.values.context_window.from=" .. tostring(result.config.values.context_window.from))
-- The room's numbers ride beside `values`: the window the room was built
-- over, and a result limit inside the beat's budget.
print("config.room.window=" .. tostring(result.config.room.window))
print("config.room.beat_budget=" .. tostring(result.config.room.beat_budget))
print(
    "config.room.result_within_beat="
        .. tostring(
            result.config.room.result_limit > 0 and result.config.room.result_limit <= result.config.room.beat_budget
        )
)
print("CODING_MOCK_DONE")
