//! e2e: `agent-block serve` runs a declared job in a process of its own,
//! records it, answers over HTTP, and continues the same log after a restart.
//!
//! One block, `echo`, declared to run every second: it writes `_PROMPT` to
//! the file `_CONTEXT` names and returns. The test reads the run back three
//! ways — the mark the run left, the record over `GET /runs`, and the run's
//! own session log on disk — then asks for a run, checks the refusals (no
//! token, unknown run), stops the manager with SIGTERM, starts it again on
//! the same home and sees the earlier run still on the record.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

const START_TIMEOUT: Duration = Duration::from_secs(30);
const RUN_TIMEOUT: Duration = Duration::from_secs(40);
const STOP_TIMEOUT: Duration = Duration::from_secs(15);

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind :0")
        .local_addr()
        .expect("local addr")
        .port()
}

struct Manager {
    child: Child,
    base: String,
    token: String,
    log: PathBuf,
}

/// A manager the test did not stop must not outlive the test: a panic
/// before `stop` would otherwise leave a process ticking every second on
/// the machine, starting runs, for as long as the machine is up.
impl Drop for Manager {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

impl Manager {
    fn start(root: &Path, home: &Path, port: u16) -> Self {
        let bind = format!("127.0.0.1:{port}");
        let log = root.join(format!("serve-{port}.log"));
        let out = std::fs::File::create(&log).expect("log file");
        let err = out.try_clone().expect("clone log file");
        let child = Command::new(assert_cmd::cargo::cargo_bin("agent-block"))
            .args([
                "serve",
                "--project",
                &root.display().to_string(),
                "--bind",
                &bind,
                "--tick-secs",
                "1",
            ])
            .env("AGENT_BLOCK_HOME", home)
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .stdin(Stdio::null())
            .stdout(Stdio::from(out))
            .stderr(Stdio::from(err))
            .spawn()
            .expect("spawn agent-block serve");
        let token_path = home.join("serve.token");
        let deadline = Instant::now() + START_TIMEOUT;
        let token = loop {
            if let Ok(t) = std::fs::read_to_string(&token_path) {
                if !t.trim().is_empty() {
                    break t.trim().to_string();
                }
            }
            assert!(
                Instant::now() < deadline,
                "no token minted at {}",
                token_path.display()
            );
            std::thread::sleep(Duration::from_millis(100));
        };
        Self {
            child,
            base: format!("http://{bind}"),
            token,
            log,
        }
    }

    fn client(&self) -> reqwest::Client {
        reqwest::Client::new()
    }

    async fn get(&self, path: &str) -> (u16, Value) {
        let res = self
            .client()
            .get(format!("{}{path}", self.base))
            .bearer_auth(&self.token)
            .send()
            .await
            .expect("GET");
        let status = res.status().as_u16();
        let body = res.json::<Value>().await.unwrap_or(Value::Null);
        (status, body)
    }

    async fn call(&self, method: reqwest::Method, path: &str) -> (u16, Value) {
        let res = self
            .client()
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(&self.token)
            .send()
            .await
            .expect("request");
        let status = res.status().as_u16();
        let body = res.json::<Value>().await.unwrap_or(Value::Null);
        (status, body)
    }

    /// Until `GET /jobs` answers 200, which is the listener up and the
    /// handler Isle answering.
    async fn wait_ready(&self) {
        let deadline = Instant::now() + START_TIMEOUT;
        loop {
            if let Ok(res) = self
                .client()
                .get(format!("{}/jobs", self.base))
                .bearer_auth(&self.token)
                .send()
                .await
            {
                if res.status().as_u16() == 200 {
                    return;
                }
            }
            assert!(
                Instant::now() < deadline,
                "manager never answered GET /jobs; log:\n{}",
                self.log_text()
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    /// The first run of `job` that ended with `outcome`, polling.
    async fn wait_run(&self, job: &str, outcome: &str) -> Value {
        let deadline = Instant::now() + RUN_TIMEOUT;
        loop {
            let (status, body) = self.get(&format!("/runs?job={job}")).await;
            assert_eq!(status, 200, "{body}");
            if let Some(row) = body["runs"]
                .as_array()
                .and_then(|rows| rows.iter().find(|r| r["outcome"] == outcome))
            {
                return row.clone();
            }
            assert!(
                Instant::now() < deadline,
                "no {outcome} run of {job}; last answer {body}; log:\n{}",
                self.log_text()
            );
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    }

    fn log_text(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// SIGTERM, then wait: `bus.serve` returns on it and the script ends.
    fn stop(mut self) -> String {
        let pid = self.child.id().to_string();
        let sent = Command::new("kill")
            .args(["-TERM", &pid])
            .status()
            .expect("kill");
        assert!(sent.success(), "kill -TERM {pid}");
        let deadline = Instant::now() + STOP_TIMEOUT;
        loop {
            if let Some(status) = self.child.try_wait().expect("try_wait") {
                assert!(
                    status.success(),
                    "serve exited {status}; log:\n{}",
                    self.log_text()
                );
                return self.log_text();
            }
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                panic!("serve did not stop on SIGTERM; log:\n{}", self.log_text());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

fn write_project(root: &Path) -> PathBuf {
    let mark = root.join("mark.txt");
    let block_dir = root.join("blocks/echo");
    std::fs::create_dir_all(&block_dir).expect("mkdir blocks/echo");
    std::fs::write(
        block_dir.join("init.lua"),
        "-- echo: leaves the prompt in the file the context names, and one\n\
         -- record on its own session log so the log exists where the\n\
         -- manager said it would.\n\
         local knl = require(\"knl\")\n\
         knl.session({ owner = \"echo\" }, function(s)\n\
             s:append({ kind = \"mark\", data = { prompt = _PROMPT } })\n\
         end)\n\
         std.fs.write(_CONTEXT, \"ran \" .. tostring(_PROMPT))\n\
         return std.json.encode({ ok = true })\n",
    )
    .expect("write block");
    std::fs::write(
        block_dir.join("job.toml"),
        format!(
            "every = \"1s\"\ntimeout = \"30s\"\nprompt = \"hello\"\ncontext = {:?}\n",
            mark.display().to_string()
        ),
    )
    .expect("write job.toml");
    mark
}

#[tokio::test]
async fn serve_runs_a_declared_job_records_it_and_answers_over_http() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("project");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&root).unwrap();
    let mark = write_project(&root);

    let port = free_port();
    let manager = Manager::start(&root, &home, port);
    manager.wait_ready().await;

    // The declaration is what `GET /jobs` lists.
    let (status, body) = manager.get("/jobs").await;
    assert_eq!(status, 200);
    assert_eq!(body["jobs"][0]["name"], "echo");
    assert_eq!(body["jobs"][0]["every"], 1);

    // A run happened in its own process: the mark it left, its record, its log.
    let run = manager.wait_run("echo", "ok").await;
    assert_eq!(run["exit_code"], 0);
    assert_eq!(std::fs::read_to_string(&mark).expect("mark"), "ran hello");
    let run_log = PathBuf::from(run["log"].as_str().expect("log path"));
    assert!(
        run_log.starts_with(home.join("runs/echo")),
        "{}",
        run_log.display()
    );
    assert!(run_log.is_file(), "run log {} missing", run_log.display());

    // One run by id, with the tail of its stderr.
    let run_id = run["run_id"].as_str().expect("run_id");
    let (status, body) = manager.get(&format!("/runs/{run_id}")).await;
    assert_eq!(status, 200);
    assert_eq!(body["run"]["run_id"], run_id);
    assert_eq!(body["run"]["outcome"], "ok");

    // A request is recorded and answered on a later tick.
    let (status, body) = manager.call(reqwest::Method::POST, "/jobs/echo/runs").await;
    assert_eq!(status, 202, "{body}");
    assert_eq!(body["requested"], "echo");
    let (status, _) = manager
        .call(reqwest::Method::POST, "/jobs/ghost/runs")
        .await;
    assert_eq!(status, 404);

    // Refusals: no such live run; no token.
    let (status, _) = manager.call(reqwest::Method::DELETE, "/runs/nope").await;
    assert_eq!(status, 404);
    let bare = reqwest::get(format!("{}/jobs", manager.base))
        .await
        .expect("GET");
    assert_eq!(bare.status().as_u16(), 401);

    let log = manager.stop();
    assert!(log.contains("serve: stopped"), "log:\n{log}");

    // A second manager on the same home continues the same log: the earlier
    // run is still on the record, and `every` counts from its end.
    let port = free_port();
    let manager = Manager::start(&root, &home, port);
    manager.wait_ready().await;
    let (status, body) = manager.get("/runs?job=echo&limit=100").await;
    assert_eq!(status, 200);
    let ids: Vec<&str> = body["runs"]
        .as_array()
        .expect("runs")
        .iter()
        .filter_map(|r| r["run_id"].as_str())
        .collect();
    assert!(
        ids.contains(&run_id),
        "earlier run {run_id} missing from {ids:?}"
    );
    let log = manager.stop();
    assert!(log.contains("serve: stopped"), "log:\n{log}");
}
