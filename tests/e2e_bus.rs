//! End-to-end tests for the EventBus / `bus.serve` path.
//!
//! These tests spawn the `agent-block` binary with a Lua script that calls
//! `bus.serve()` and then deliver POSIX signals to verify graceful
//! shutdown. All signal-based scenarios are gated on `#[cfg(unix)]` — the
//! POSIX signal mechanics are not portable to Windows.
//!
//! | Scenario                          | Assertion                        |
//! |-----------------------------------|----------------------------------|
//! | single-run non-regression         | `bus.serve`-less script exits 0  |
//! | bus.serve + SIGTERM graceful      | exit 0 within grace + overhead   |
//! | bus.serve + SIGINT graceful       | exit 0 within grace + overhead   |
//! | idle bus.serve (no handlers)      | serves and exits 0 on SIGTERM    |
//! | double bus.serve                  | second call errors               |

mod common;

// ---------------------------------------------------------------------------
// Non-regression (works on every platform — no signals involved).
// ---------------------------------------------------------------------------

#[test]
fn single_run_non_regression_exits_zero() {
    // A script that does NOT call bus.serve() must still behave like
    // before ST4: run to completion and exit 0.
    common::agent_block_cmd()
        .args(["-s", &common::fixture("hello.lua")])
        .assert()
        .success();
}

// ---------------------------------------------------------------------------
// Signal-driven scenarios (unix-only).
// ---------------------------------------------------------------------------

#[cfg(unix)]
mod unix {
    use super::common;
    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;
    use std::io::Write;
    use std::process::{Child, Stdio};
    use std::thread;
    use std::time::{Duration, Instant};
    use tempfile::NamedTempFile;

    /// Wait for `child` to exit, polling at 50ms intervals, up to `timeout`.
    /// Returns the exit status on success, or the elapsed time on timeout.
    fn wait_with_timeout(
        child: &mut Child,
        timeout: Duration,
    ) -> Result<std::process::ExitStatus, Duration> {
        let start = Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(status)) => return Ok(status),
                Ok(None) => {
                    if start.elapsed() >= timeout {
                        return Err(start.elapsed());
                    }
                    thread::sleep(Duration::from_millis(50));
                }
                Err(e) => panic!("try_wait error: {e}"),
            }
        }
    }

    /// Spawn `agent-block -s <script>` as a managed child. Configures a
    /// `1000ms` grace window (the default) and gives us a PID we can signal.
    ///
    /// Each invocation uses a distinct `AGENT_BLOCK_HOME` so parallel
    /// `cargo test` runs do not collide on shared SQLite files.
    fn spawn_bus_serve(script_path: &str) -> (Child, tempfile::TempDir) {
        let home = tempfile::tempdir().expect("mktempdir");
        let child = common::agent_block_std_cmd()
            .args(["-s", script_path])
            .env("AGENT_BLOCK_TASK_GRACE_MS", "1000")
            .env("AGENT_BLOCK_HOME", home.path())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn agent-block");
        (child, home)
    }

    /// Assert the child exits 0 within `deadline` after we deliver `sig`.
    /// Sleeps briefly first so the signal handler has time to install.
    fn assert_graceful_exit(mut child: Child, sig: Signal, deadline: Duration) {
        // Give the binary time to boot and install signal handlers.
        // Under parallel `cargo test` the agent-block binary may take
        // hundreds of ms to reach bus.serve's signal-task spawn.
        thread::sleep(Duration::from_millis(1500));

        let pid = Pid::from_raw(child.id() as i32);
        kill(pid, sig).expect("kill");

        match wait_with_timeout(&mut child, deadline) {
            Ok(status) => {
                assert!(
                    status.success(),
                    "child exited {:?} after {:?}, expected success",
                    status,
                    sig
                );
            }
            Err(elapsed) => {
                // Kill it to avoid leaking zombies in the test runner.
                let _ = child.kill();
                panic!(
                    "child did not exit within {:?} after {:?} (elapsed {:?})",
                    deadline, sig, elapsed
                );
            }
        }
    }

    // ------- bus.serve + SIGTERM graceful -------

    #[test]
    fn bus_serve_sigterm_exits_zero_within_grace() {
        let script = common::example("test_bus.lua");
        let (child, _home) = spawn_bus_serve(&script);
        // default grace 1000ms + process shutdown overhead + generous margin.
        assert_graceful_exit(child, Signal::SIGTERM, Duration::from_secs(5));
    }

    // ------- bus.serve + SIGINT (Ctrl+C) graceful -------

    #[test]
    fn bus_serve_sigint_exits_zero_within_grace() {
        let script = common::example("test_bus.lua");
        let (child, _home) = spawn_bus_serve(&script);
        assert_graceful_exit(child, Signal::SIGINT, Duration::from_secs(5));
    }

    // ------- idle bus.serve (no handlers registered) -------

    #[test]
    fn bus_serve_with_no_handlers_exits_on_signal() {
        // Script calls bus.serve() without registering anything. Per plan.md
        // decision the dispatcher idles until shutdown.
        let mut tmp = NamedTempFile::new().expect("tempfile");
        writeln!(
            tmp,
            r#"log.info("idle serve start")
bus.serve()
log.info("idle serve stop")
"#
        )
        .expect("write tmp script");
        let path = tmp.path().to_string_lossy().to_string();

        let (child, _home) = spawn_bus_serve(&path);
        assert_graceful_exit(child, Signal::SIGTERM, Duration::from_secs(5));
    }

    // ------- double bus.serve returns an error -------

    #[test]
    fn double_bus_serve_second_call_errors() {
        // Script calls `bus.serve()`, then — after SIGTERM unblocks the
        // first call — invokes `bus.serve()` again inside `pcall` to
        // exercise the AtomicBool double-serve guard. The pcall result is
        // printed to stdout and the script then errors so the process
        // exits non-zero. The important assertion is that the second
        // call returns an error rather than hanging.
        let mut tmp = NamedTempFile::new().expect("tempfile");
        writeln!(
            tmp,
            r#"bus.serve()

local ok, err = pcall(function() bus.serve() end)
if ok then
    print("double-serve unexpectedly succeeded")
    error("double-serve unexpectedly succeeded")
else
    print("double-serve error: " .. tostring(err))
    error("intended: double serve blocked")
end
"#
        )
        .expect("write tmp script");
        let path = tmp.path().to_string_lossy().to_string();

        let home = tempfile::tempdir().expect("mktempdir");
        let mut child = common::agent_block_std_cmd()
            .args(["-s", &path])
            .env("AGENT_BLOCK_TASK_GRACE_MS", "1000")
            .env("AGENT_BLOCK_HOME", home.path())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn agent-block");

        // Let the first bus.serve() install its signal handlers, then
        // send SIGTERM so it returns. The Lua script then falls through
        // to the second `bus.serve()` in pcall.
        thread::sleep(Duration::from_millis(1500));
        let pid = Pid::from_raw(child.id() as i32);
        kill(pid, Signal::SIGTERM).expect("kill SIGTERM");

        match wait_with_timeout(&mut child, Duration::from_secs(5)) {
            Ok(status) => {
                // Non-zero exit is expected (the script errors out on
                // purpose after asserting the double-serve guard fired).
                // The assertion we care about is that the process
                // terminated rather than hanging on a second bus.serve.
                assert!(
                    !status.success(),
                    "double-serve test should exit non-zero, got {status:?}"
                );
                // Verify stdout contains the "already running" message so
                // we know the second call was rejected (not silently
                // succeeded).
                let out = child.wait_with_output().ok();
                if let Some(out) = out {
                    let stdout = String::from_utf8_lossy(&out.stdout);
                    assert!(
                        stdout.contains("double-serve error"),
                        "expected 'double-serve error' in stdout, got: {stdout}"
                    );
                    assert!(
                        stdout.contains("already running"),
                        "expected 'already running' in double-serve error, got: {stdout}"
                    );
                }
            }
            Err(elapsed) => {
                let _ = child.kill();
                panic!("double-serve test did not terminate within 5s (elapsed {elapsed:?})");
            }
        }
    }
}
