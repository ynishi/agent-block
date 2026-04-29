-- test_qwen_compile_loop_rust.lua — compile_loop with cargo test runner
-- Tricky Rust task: parse a small custom format with edge cases.

local compile_loop = require("compile_loop")
local agent        = require("agent")

local QWEN_BASE_URL = std.env.get("QWEN_BASE_URL")
if not QWEN_BASE_URL or QWEN_BASE_URL == "" then
    log.error("QWEN_BASE_URL not set")
    os.exit(2)
end

local PROJ   = "/tmp/qwen-rust-react"
local TARGET = PROJ .. "/src/main.rs"

local function cargo_runner(file_path)
    -- Run cargo test in offline mode (faster, deterministic). Capture stdout+stderr.
    local cmd = string.format("cd %s && cargo test --offline 2>&1; echo __EXIT__=$?", PROJ)
    local p = io.popen(cmd, "r")
    if not p then return { ok=false, stdout="", stderr="popen failed", exit_code=-1 } end
    local out = p:read("*a") or ""
    p:close()
    local exit_str = out:match("__EXIT__=(%d+)%s*$") or "1"
    local exit_code = tonumber(exit_str) or 1
    out = out:gsub("__EXIT__=%d+%s*$", "")
    local pass = exit_code == 0 and out:find("test result: ok", 1, true) ~= nil
    return { ok = pass, stdout = out, stderr = "", exit_code = exit_code }
end

local SPEC = [[Write a single Rust file (`src/main.rs`) that:

1. Defines `pub fn parse_kv_lines(input: &str) -> Result<Vec<(String, String)>, ParseError>` where:
   - Each line is "KEY=VALUE" (KEY is alphanumeric + underscore, VALUE is anything until end-of-line).
   - **Lines starting with `#` (after optional whitespace) are comments and skipped.**
   - **Empty lines are skipped.**
   - **Whitespace around KEY and VALUE is trimmed.**
   - **A line missing `=` returns Err with the 1-based line number.**
   - Duplicate keys are kept as separate entries in order.
2. `ParseError` is an enum with one variant `MissingEquals { line: usize }` and derives Debug + PartialEq.
3. `fn main()` can be empty: `fn main() {}`
4. `#[cfg(test)] mod tests { use super::*; ... }` module containing **5 tests**:
   - `test_basic`: parses 2 simple lines correctly
   - `test_skip_comments`: lines starting with `#` (and `   #` with leading space) are skipped
   - `test_skip_empty`: empty lines and whitespace-only lines are skipped
   - `test_trim`: whitespace around key and value is trimmed
   - `test_missing_equals_error`: returns MissingEquals with correct 1-based line number when a line lacks `=`
5. No external crates — std only.

Output ONLY the file contents in a single ```rust ... ``` block.]]

log.info("Running compile_loop Rust task: " .. PROJ)

-- K-96: all LLM tuning fields are explicitly listed in the llm table.
-- timeout is placed inside the llm table per K-96 (subtask-2.md §Constraints).
local td = compile_loop.make({
    runner   = cargo_runner,
    llm      = {
        provider         = "openai",
        base_url         = QWEN_BASE_URL,
        api_key          = "dummy",
        model            = "qwen",
        disable_thinking = true,
        temperature      = 0.2,
        max_tokens       = 3000,
        timeout          = 240,
    },
    max_iters = 5,
    lang      = "rust",
    register  = false,
})

-- Parent also uses Qwen (minimum env: only QWEN_BASE_URL required).
local result = agent.run({
    provider       = "openai",
    base_url       = QWEN_BASE_URL,
    api_key        = "dummy",
    model          = "qwen",
    max_iterations = 3,
    extra_tools    = { td },
    prompt         = string.format(
        "Use the compile_loop tool to solve the following coding task.\nTarget file: %s\nSpec:\n%s",
        TARGET, SPEC
    ),
})

log.info("=== RESULT ===")
log.info("ok:        " .. tostring(result.ok))
log.info("num_turns: " .. tostring(result.num_turns))
if not result.ok then
    log.error("parent agent failed: " .. tostring(result.error or "unknown"))
    os.exit(2)
end

-- ── Extract tool_result from messages ─────────────────────────────────────────
local captured = nil
for _, msg in ipairs(result.messages or {}) do
    if msg.role == "user" and type(msg.content) == "table" then
        for _, block in ipairs(msg.content) do
            if type(block) == "table" and block.type == "tool_result" then
                captured = block.content
                break
            end
        end
    end
    if captured then break end
end

if not captured then
    log.error("FAIL: no tool_result found — compile_loop was never called")
    os.exit(2)
end

log.info("tool_result JSON: " .. tostring(captured):sub(1, 400))

-- ── Decode and assert shape ────────────────────────────────────────────────────
local dec_ok, tool_output = pcall(std.json.decode, captured)
if not dec_ok or type(tool_output) ~= "table" then
    log.error("FAIL: tool_result is not valid JSON: " .. tostring(captured))
    os.exit(2)
end

-- Required keys
assert(tool_output.ok ~= nil,      "FAIL: tool_output.ok is absent")
assert(tool_output.iters ~= nil,   "FAIL: tool_output.iters is absent")
assert(tool_output.summary ~= nil, "FAIL: tool_output.summary is absent")

-- Counter WF-A: code / history must NOT appear in tool output
assert(tool_output.code == nil,    "Counter WF-A: code leaked to caller")
assert(tool_output.history == nil, "Counter WF-A: history leaked to caller")

log.info(string.format("ok=%s iters=%s", tostring(tool_output.ok), tostring(tool_output.iters)))
if tool_output.failure_reason then log.info("failure_reason: " .. tostring(tool_output.failure_reason)) end
os.exit(result.ok and 0 or 2)
