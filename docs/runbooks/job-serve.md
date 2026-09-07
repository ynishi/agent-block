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

A run's own session log is the `log` path on its record; read it with
`knl.resume{ session = ..., store = { sqlite = <path> } }` and `knl.views`,
as any block's log.
