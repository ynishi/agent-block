# Running `agent-block serve` under a service manager

`agent-block serve` is the one long-lived process a machine needs for its
jobs: the blocks that declare a `job.toml` run on their interval, each in a
process of its own, and the manager records them. The unit below is the only
unit a service manager holds — a lane never writes one. Adding a job is
adding a file beside a block; removing it is removing the file; both take
effect on the manager's next start.

What the manager needs from its environment:

| | |
|---|---|
| `AGENT_BLOCK_HOME` | where the record lives: `serve.sqlite`, `serve.session`, `serve.token`, `runs/`. Default `~/.agent-block` |
| `--project <dir>` | the project whose `blocks/` is scanned (with `~/.agent-block/blocks/`; `--block-dir` adds more) |
| `--bind` | `127.0.0.1:7788` unless told otherwise |
| the block's own `.env` | a run is started in the block's project root and loads that `.env`; the manager's environment does not need model credentials |
| `PATH` | a run inherits the manager's environment, and a service manager's `PATH` is short: `~/.cargo/bin` and the rustup shims are not on it. A block that runs `agent-block`, `cargo` or anything from a user toolchain gets `command not found` (exit 127) that a shell never shows. Set it on the unit, as below |

Two things a lane learns only under the manager, both from the environment
being the unit's rather than a shell's: the `PATH` above, and that a block
which reports failure in its return value ends as `outcome = "ok"` — the
outcome is the process's word (it returned, exit 0) and `result` is the
block's. A block whose work could not be done should raise; a value is an
answer.

The outcome is read off the exit code, and off nothing else:

| The run | `outcome` |
|---|---|
| exit 0 | `ok` |
| exit 75 (`job.defer`) | `deferred` |
| any other non-zero, or a process that would not start | `failed` |
| killed at `timeout` | `timeout` |
| ended by a signal — a `run_stop`, or the manager leaving | `stopped` |
| started by a manager that is gone | `lost`, written by the next start |

A lane that runs a block from a shell reads the same codes by hand (README,
"What the exit code says"), and one that tests only for non-zero reads a
`deferred` run as a failure.

A block that runs unattended checks what it needs before it starts, and
says so when it is not there — the manager keeps no health of its own, so
the check is the block's first lines (the shape systemd calls
`ExecCondition=`). `port:probe(conf)` asks the LLM endpoint one GET (its
`/health` where it has one, the models list where it does not; no tokens),
and `job.defer(reason)` ends the run with exit 75, which the manager records
as `outcome = "deferred"` — neither `ok` nor `failed`, so `runs_list` shows
a pod that was down as that and not as a block that broke:

```lua
local job = require("job")
local adapter = require("knl_adapter")

local h = adapter.openai:probe(conf)
if h.alive == false then
    job.defer("llm endpoint " .. h.kind .. ": " .. tostring(h.message))
end
-- the work
```

The manager does not count deferrals or back off on them; `every` stands,
and each deferred run is one line in the record. How many in a row are too
many, and what to do then, is the lane's or its service manager's
(`StartLimitBurst=` and the like), not a number the block knows.

The listener requires the bearer token in `$AGENT_BLOCK_HOME/serve.token` on
every request, loopback included. Reaching it from another machine is a
tunnel to the loopback port (`ssh -L 7788:127.0.0.1:7788 host`); a wider
`--bind` is the opt-in, and the token is what stands between it and the
network.

## Linux — systemd user unit

`~/.config/systemd/user/agent-block-serve.service`:

```ini
[Unit]
Description=agent-block job manager
After=network.target

[Service]
ExecStart=%h/.cargo/bin/agent-block serve --project %h/projects/lane
Restart=on-failure
RestartSec=5
KillSignal=SIGTERM
TimeoutStopSec=30
Environment=AGENT_BLOCK_HOME=%h/.agent-block
# The PATH a run inherits. A user service starts with the system default,
# without the user's toolchains; every command a block runs resolves here.
Environment=PATH=%h/.cargo/bin:%h/.local/bin:/usr/local/bin:/usr/bin:/bin

[Install]
WantedBy=default.target
```

```sh
systemctl --user daemon-reload
systemctl --user enable --now agent-block-serve.service
journalctl --user -u agent-block-serve.service -f
```

`SIGTERM` is what `bus.serve` returns on: the manager stops its live runs,
records them as `stopped`, and exits 0. `TimeoutStopSec` bounds the wait; a
run killed harder than that is closed as `lost` on the next start.

For a unit that survives logout, `loginctl enable-linger $USER`.

## macOS — launchd agent

launchd expands neither `~` nor `$HOME`, so the plist carries absolute
paths; write it through `sed` from this template, which fills them in:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>dev.agent-block.serve</string>
  <key>ProgramArguments</key>
  <array>
    <string>__HOME__/.cargo/bin/agent-block</string>
    <string>serve</string>
    <string>--project</string>
    <string>__HOME__/projects/lane</string>
  </array>
  <key>EnvironmentVariables</key>
  <dict>
    <key>AGENT_BLOCK_HOME</key>
    <string>__HOME__/.agent-block</string>
    <key>PATH</key>
    <string>__HOME__/.cargo/bin:__HOME__/.local/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin</string>
  </dict>
  <key>KeepAlive</key>
  <true/>
  <key>RunAtLoad</key>
  <true/>
  <key>ExitTimeOut</key>
  <integer>30</integer>
  <key>StandardErrorPath</key>
  <string>__HOME__/Library/Logs/agent-block-serve.log</string>
</dict>
</plist>
```

```sh
sed "s|__HOME__|$HOME|g" agent-block-serve.plist.template > ~/Library/LaunchAgents/dev.agent-block.serve.plist
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/dev.agent-block.serve.plist
launchctl kickstart -k gui/$(id -u)/dev.agent-block.serve
tail -f ~/Library/Logs/agent-block-serve.log
```

launchd sends `SIGTERM` on `bootout` / `kickstart -k` and waits
`ExitTimeOut`; the same shutdown as above applies.

## Checking it

```sh
TOKEN=$(cat ~/.agent-block/serve.token)
curl -sH "Authorization: Bearer $TOKEN" http://127.0.0.1:7788/jobs
curl -sH "Authorization: Bearer $TOKEN" "http://127.0.0.1:7788/runs?limit=5"
curl -sH "Authorization: Bearer $TOKEN" -X POST http://127.0.0.1:7788/jobs/drain/runs
```

A run's session is in its project's own log, labelled with the run it was:

```lua
local rows = knl.views.sessions(s)          -- s: a session on that project's log
for _, row in ipairs(rows) do
    local labels = std.json.decode(row.meta)
    if labels.run == "<run_id>" then
        local run = knl.resume({ session = row.session })
        -- knl.views.beats(run) / tool_pairs(run) / usage(run), as any block's log
    end
end
```

Runs are not given a log each: they share their project's, which is what
keeps that log one stream to read.

## Stopping a run

Ask the manager, not the process:

```sh
curl -sH "Authorization: Bearer $TOKEN" "http://127.0.0.1:7788/runs?job=drain&limit=1"   # the live run_id
curl -sH "Authorization: Bearer $TOKEN" -X DELETE http://127.0.0.1:7788/runs/<run_id>     # run_stop
curl -sH "Authorization: Bearer $TOKEN" "http://127.0.0.1:7788/runs?job=drain&limit=1"   # outcome = "stopped"
```

(`run_stop { run_id }` from the MCP server is the same route.) The request
is recorded and answered at once; on its next tick the manager kills the
run's **process group** and records `outcome = "stopped"`. Two things a
`kill` from outside does not give you:

- **The record.** A run killed by pid ends `failed` with whatever exit code
  the signal left, and the log can no longer tell "it broke" from "someone
  stopped it" — the distinction `ok` / `deferred` / `failed` / `stopped`
  exists to keep. (Seen: a run killed by hand recorded as `failed`, exit 1,
  800 s.)
- **The whole tree.** A run is a tree — the block, the shell it ran, the
  `cargo` under that — and the group is what takes the grandchildren with
  it. A `pkill -f <pattern>` misses what the pattern does not name, and
  matches the shell that typed it: a `pkill -f` from a script whose own
  command line contains the pattern kills the script, and whatever it was
  going to do next does not happen.

When the run's endpoint is going away too (a pod being returned), the
order is: `runs_list` → `run_stop` → read `stopped` back → take the endpoint
down. The other way round leaves the verify running on a shared host
against an endpoint that is gone.

Stopping the schedule rather than a run: remove `every` from `job.toml`
(the job becomes request-only), remove the file (the job is gone on the next
start), or stop the unit (`systemctl --user stop agent-block-serve.service`,
which takes its live runs with it — recorded `stopped`).
