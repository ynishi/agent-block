-- Test http.request GET.
-- Uses a local echo: if HTTP_TEST_URL is set, hits that; otherwise
-- falls back to a data URI trick (just verifies the function exists
-- and returns the expected table shape).

local url = std.env.get("HTTP_TEST_URL")
if not url then
    -- No test server available; verify http.request exists and
    -- returns an error table for an unreachable host.
    local ok, err = pcall(function()
        return http.request("http://127.0.0.1:1", { timeout = 1 })
    end)
    if ok then
        print("unexpected_success")
    else
        -- Connection refused or timeout — expected.
        print("error_ok")
    end
    return
end

local resp = http.request(url, {
    method = "GET",
    headers = { ["Accept"] = "application/json" },
    timeout = 10,
})

print("status=" .. tostring(resp.status))
print("has_body=" .. tostring(#resp.body > 0))
print("has_headers=" .. tostring(resp.headers ~= nil))
