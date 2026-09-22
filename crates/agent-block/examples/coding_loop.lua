-- coding_loop.lua — `coding.run` from the shell: edit the target files until
-- a verify command passes.
--
-- The loop is the `coding` consumer block (`require("coding")`, embedded
-- beside `agent`); this script is what a block over it looks like — the
-- spec from `--prompt`, the rest from the environment, one JSON string back.
-- Registered under `blocks/` with a `job.toml` beside it, `agent-block serve`
-- runs it on a schedule and records the value; over MCP, `run_block` hands
-- it back.
--
-- Run:
--
--   CODING_TARGETS="src/lib.rs" CODING_VERIFY="cargo test --quiet" \
--     agent-block -s crates/agent-block/examples/coding_loop.lua \
--       --prompt "Add a pub fn double(n: i64) -> i64 to src/lib.rs, with a test."
--
--   AGENT_PROVIDER=anthropic (default) needs ANTHROPIC_API_KEY;
--   AGENT_PROVIDER=openai reaches any OpenAI-compatible server —
--     QWEN_BASE_URL=https://<host>/v1  QWEN_MODEL=qwen  (vLLM: the api key is not checked)
--   CODING_REPO      the directory verify runs in and targets are under (default: cwd)
--   CODING_ITERS     iterations, each ending in a verify (default 5)
--   CODING_TURNS     beats per iteration before verify runs anyway (default 8)
--   CODING_BASELINE  "false" skips the verify before the first beat (default: run it)
--   CODING_DONE      what ends the run: "declare" (default) | "plan"
--   CODING_CHECK_TIMEOUT  seconds one plan check may take (default 120; "plan" only)
--   CODING_SEED      how the targets enter the seed: "names" (default) | "full"
--   CODING_RESERVE   tokens the fold holds back for the reply on the openai
--                    path, where the wire carries no cap (default 6144)
--   CODING_VERIFY_TIMEOUT  seconds one verify may take, every time (default 900)
--   CODING_RESULT_SHARE    share of the window one tool result may take (default 0.25)
--   CODING_REPEAT_MAX      times the same read may be made with no edit between (default 2)
--
-- `reserve` and `max_tokens` are the same question asked of two servers: how
-- much room the reply gets. The anthropic API requires a cap and that cap IS
-- the room, so the conf carries `max_tokens`. An OpenAI-compatible server
-- does not require one, and a cap invented here is a second, smaller ceiling
-- on top of the model's own [measured: 4096 cut 103 of 415 beats short], so
-- that path sends no cap and holds tokens back in the fold instead. 6144 was
-- chosen against a 32k window in a sibling lane; scale it with the window of
-- the model actually being run.
--
-- The exit code is 0 whenever the loop ran — `ok` in the value says whether
-- the verify passed — and non-zero only when it could not run (missing input
-- raises: this script is also a block, and a block runs inside a host that
-- is not its own process, where an `os.exit` would end the server).

local coding = require("coding")
local adapter = require("knl_adapter")

local E = std.env
local provider = E.get("AGENT_PROVIDER") or "anthropic"

local llm
if provider == "openai" then
    if (E.get("QWEN_BASE_URL") or "") == "" then
        error("coding_loop: AGENT_PROVIDER=openai needs QWEN_BASE_URL", 0)
    end
    llm = {
        port = adapter.openai,
        conf = {
            base_url = E.get("QWEN_BASE_URL"),
            api_key = E.get("OPENAI_API_KEY") or "dummy",
            model = E.get("QWEN_MODEL") or "qwen",
            dialect = "vllm",
            thinking = { enabled = false },
            temperature = 0.2,
            timeout = 600,
        },
    }
else
    if (E.get("ANTHROPIC_API_KEY") or "") == "" then
        error("coding_loop: ANTHROPIC_API_KEY is not set", 0)
    end
    llm = {
        port = adapter.anthropic,
        conf = {
            api_key = E.get("ANTHROPIC_API_KEY"),
            model = E.get("ANTHROPIC_MODEL") or "claude-haiku-4-5-20251001",
            max_tokens = 4096,
            timeout = 600,
        },
    }
end

if (_PROMPT or "") == "" or (E.get("CODING_TARGETS") or "") == "" then
    error("coding_loop: pass the spec as --prompt and the target files in CODING_TARGETS", 0)
end

local result = coding.run({
    spec = _PROMPT,
    targets = E.get("CODING_TARGETS"),
    verify = E.get("CODING_VERIFY") or "cargo check --all-targets",
    repo = E.get("CODING_REPO") or ".",
    llm = llm,
    -- The reply's room where the wire carries no cap. On the anthropic path
    -- `max_tokens` already names it and this holds nothing extra back.
    reserve = tonumber(E.get("CODING_RESERVE") or "6144"),
    iters = tonumber(E.get("CODING_ITERS") or "5"),
    turns = tonumber(E.get("CODING_TURNS") or "8"),
    timeout = tonumber(E.get("CODING_VERIFY_TIMEOUT") or "900"),
    result_share = tonumber(E.get("CODING_RESULT_SHARE") or "0.25"),
    repeat_max = tonumber(E.get("CODING_REPEAT_MAX") or "2"),
    baseline = E.get("CODING_BASELINE") ~= "false",
    done = E.get("CODING_DONE") or "declare",
    seed = E.get("CODING_SEED") or "names",
    check_timeout = tonumber(E.get("CODING_CHECK_TIMEOUT") or "120"),
})

print("[coding_loop] " .. result.summary)
return std.json.encode(result)
