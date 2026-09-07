-- A timed-out command takes what it started with it.
--
-- `sh -c "…"` is one process and whatever it spawns is another below it, so
-- killing the command alone leaves the descendant running — that is the shape
-- that leaves a runaway test binary on a shared machine after a build times
-- out. Each command gets a process group of its own and the timeout kills the
-- group, so the descendant goes too.
--
-- The probe starts a `sleep` in the background, prints its pid, and blocks on
-- `wait` so the command outlives its own child. The timeout then has to reach
-- past the command to end the sleep.

local dir = std.env.get("AGENT_BLOCK_HOME")
local pidfile = dir .. "/grandchild.pid"

local res = sh.exec("sleep 30 & echo $! > " .. pidfile .. "; wait", { timeout = 2 })

print(
    "[SHG] timed_out                = "
        .. tostring(res.ok == false and tostring(res.error):find("timeout", 1, true) ~= nil)
)

-- The pid was written by the shell before it blocked, so it is on disk even
-- though the command never returned.
local pid = (std.fs.read(pidfile) or ""):gsub("%s+", "")
print("[SHG] grandchild_pid_recorded  = " .. tostring(pid ~= ""))

-- `kill -0` asks whether the process is still there without touching it. The
-- kill happens as the timeout returns, so give the group a moment to go.
local alive = sh.exec("sleep 0.5; kill -0 " .. pid .. " 2>/dev/null && echo ALIVE || echo GONE", { timeout = 10 })
print("[SHG] grandchild_gone          = " .. tostring((alive.stdout or ""):find("GONE", 1, true) ~= nil))

print("[SHG] done")
