-- json_stub.lua — the one Lua stand-in for the host's `std.json.encode` that
-- every spec measuring or keying an encoded value is driven against.
--
-- NOT a spec: it declares no suite and calls nothing in the lspec framework,
-- which is exactly how the runner decides what to run (crates/lua-spec-runner/
-- src/main.rs, `is_spec` — detection is by USE, not by filename). A spec or a
-- spec support module reaches it with `require("knl.spec.json_stub")`, beside
-- the fake kernel it is used with.
--
-- Why there is one: the pure runner has no host, so `std.json` is absent, and
-- the code under test reaches `encode` in two ways that ask different things
-- of it. The kernel's fold, `policy.result_cap`, `policy.beat_cap` and the
-- file tools' `limits` COUNT what it answers, so its length has to grow with
-- the value — a constant-length answer (`tostring` of a table, a memo token)
-- would let every size check pass without checking anything. `policy.
-- repeat_cap` KEYS a call by it, so the same value has to give the same text
-- whatever order `pairs` walks it in. Each spec that wrote its own met one of
-- the two and not always the other; the day a copy drifts, a cap spec is
-- passing over a measure that does not measure.
--
-- What it is: keys sorted, keys and strings written with `%q`, numbers and
-- booleans with `tostring`, anything else as a quoted type name (a function's
-- address would make the text differ run to run). It is NOT the host's JSON
-- — arrays are braced like maps and `%q` is Lua's quoting — so a spec asserts
-- relations (over, under, equal, different), never the host's exact count.
--
-- How it is used: `encode(value)` is the encoder; `install()` puts
-- `std = { json = { encode = encode } }` on the global table when there is no
-- `std` yet and leaves one that is there alone. A spec that builds its own
-- `std` (with `fs`, say) takes `encode` into it instead.

local M = {}

--- The text for `value`. Its length grows with the value, and equal values
--- give equal text.
function M.encode(value)
    local t = type(value)
    if t == "string" then
        return string.format("%q", value)
    elseif t == "number" or t == "boolean" or t == "nil" then
        return tostring(value)
    elseif t ~= "table" then
        return string.format("%q", "<" .. t .. ">")
    end
    local keys = {}
    for k in pairs(value) do
        keys[#keys + 1] = k
    end
    table.sort(keys, function(a, b)
        return tostring(a) < tostring(b)
    end)
    local parts = {}
    for _, k in ipairs(keys) do
        parts[#parts + 1] = string.format("%q:%s", tostring(k), M.encode(value[k]))
    end
    return "{" .. table.concat(parts, ",") .. "}"
end

--- Put the encoder on `std.json` unless the global `std` is already there.
--- Answers the module.
function M.install()
    if rawget(_G, "std") == nil then
        _G.std = { json = { encode = M.encode } }
    end
    return M
end

return M
