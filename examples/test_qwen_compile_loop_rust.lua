-- test_qwen_compile_loop_rust.lua — compile_loop with cargo test runner
-- Tricky Rust task: parse a small custom format with edge cases.
local coding = require("coding_agent")

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

local res = coding.run({
    provider     = "openai",
    base_url     = QWEN_BASE_URL,
    api_key      = "dummy",
    model        = "qwen",
    target_file  = TARGET,
    lang         = "rust",
    spec         = SPEC,
    runner       = cargo_runner,
    max_iters    = 5,
    max_tokens   = 3000,
    temperature  = 0.2,
    disable_thinking = true,
    timeout      = 240,
    on_iter = function(info)
        local r = info.result
        local tail = (r.stdout or ""):sub(-500):gsub("\n", " | ")
        log.info(string.format("iter %d: ok=%s exit=%s", info.iter, tostring(r.ok), tostring(r.exit_code)))
        log.info("  tail: " .. tail)
    end,
})

log.info("=== RESULT ===")
log.info(string.format("ok=%s iters=%d", tostring(res.ok), res.iters))
if res.failure_reason then log.info("failure_reason: " .. tostring(res.failure_reason)) end
os.exit(res.ok and 0 or 2)
