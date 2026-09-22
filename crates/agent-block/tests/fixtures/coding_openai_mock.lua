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
--
-- One marker line per fact, so a Rust assertion names the fact and not a
-- position in the output.

local base_url = std.env.get("OPENAI_BASE_URL_TEST")
assert(base_url, "OPENAI_BASE_URL_TEST must be set")
local repo = std.env.get("CODING_REPO_TEST")
assert(repo, "CODING_REPO_TEST must be set")
local done_mode = std.env.get("CODING_DONE_TEST") or "declare"
local omit_reserve = std.env.get("CODING_OMIT_RESERVE") == "1"

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
            dialect = "vllm",
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
if omit_reserve then
    opts.reserve = nil
end

local ok, result = pcall(coding.run, opts)
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
print("CODING_MOCK_DONE")
