//! e2e: the job manager's verbs over MCP. `agent-block serve` runs one
//! declared job; `agent-block mcp --serve-url` is connected as an MCP
//! server and its five job tools are called through `McpManager`, against
//! the live manager and, first, against no manager at all.

mod common;

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use agent_block_mcp::McpManager;
use serde_json::{json, Value};

const START_TIMEOUT: Duration = Duration::from_secs(30);
const RUN_TIMEOUT: Duration = Duration::from_secs(40);

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind :0")
        .local_addr()
        .expect("local addr")
        .port()
}

/// A manager the test started; killed on drop so a panic leaves nothing
/// ticking on the machine.
struct Manager {
    child: Child,
    url: String,
}

impl Manager {
    fn start(root: &Path, home: &Path, port: u16) -> Self {
        let bind = format!("127.0.0.1:{port}");
        let log = std::fs::File::create(root.join("serve.log")).expect("log file");
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
            .stdout(Stdio::from(log.try_clone().expect("clone")))
            .stderr(Stdio::from(log))
            .spawn()
            .expect("spawn agent-block serve");
        let token = home.join("serve.token");
        let deadline = Instant::now() + START_TIMEOUT;
        while !token.is_file() {
            assert!(Instant::now() < deadline, "no token minted");
            std::thread::sleep(Duration::from_millis(100));
        }
        Self {
            child,
            url: format!("http://{bind}"),
        }
    }
}

impl Drop for Manager {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn write_project(root: &Path) {
    let block_dir = root.join("blocks/echo");
    std::fs::create_dir_all(&block_dir).expect("mkdir");
    std::fs::write(
        block_dir.join("init.lua"),
        "-- echo: returns.\nreturn std.json.encode({ ok = true })\n",
    )
    .expect("write block");
    std::fs::write(
        block_dir.join("job.toml"),
        "every = \"1s\"\ntimeout = \"30s\"\n",
    )
    .expect("write job.toml");
}

async fn connect(root: &Path, home: &Path, serve_url: &str) -> McpManager {
    let bin = assert_cmd::cargo::cargo_bin("agent-block");
    let mut mgr = McpManager::new();
    mgr.connect(
        "blocks",
        bin.to_str().expect("utf-8 path"),
        &[
            "mcp".to_string(),
            "--project".to_string(),
            root.display().to_string(),
            "--serve-url".to_string(),
            serve_url.to_string(),
        ],
        false,
        Some(root),
        &[("AGENT_BLOCK_HOME".to_string(), home.display().to_string())],
    )
    .await
    .expect("connect to agent-block mcp");
    mgr
}

fn text_of(result: &Value) -> String {
    result["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

fn is_error(result: &Value) -> bool {
    result["isError"].as_bool().unwrap_or(false)
}

async fn call(mgr: &McpManager, tool: &str, args: Value) -> Value {
    mgr.call_tool("blocks", tool, args)
        .await
        .unwrap_or_else(|e| panic!("call {tool}: {e}"))
}

#[tokio::test]
async fn the_job_tools_are_a_client_of_the_manager() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("project");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    write_project(&root);
    let port = free_port();
    let serve_url = format!("http://127.0.0.1:{port}");

    // Before the manager exists: the tools are listed, and say what is missing.
    let mut mgr = connect(&root, &home, &serve_url).await;
    let tools = mgr.list_tools("blocks").await.expect("list tools");
    let names: Vec<String> = tools
        .as_array()
        .expect("tools array")
        .iter()
        .filter_map(|t| t["name"].as_str().map(str::to_string))
        .collect();
    for expected in [
        "run_block",
        "jobs_list",
        "runs_list",
        "run_get",
        "job_run",
        "run_stop",
    ] {
        assert!(
            names.contains(&expected.to_string()),
            "{expected} missing from {names:?}"
        );
    }
    let answer = call(&mgr, "jobs_list", json!({})).await;
    assert!(is_error(&answer), "{answer}");
    assert!(
        text_of(&answer).contains("serve.token"),
        "{}",
        text_of(&answer)
    );
    mgr.disconnect_all().await.expect("disconnect");

    // With the manager up: list, run, read back, stop.
    let manager = Manager::start(&root, &home, port);
    let mut mgr = connect(&root, &home, &manager.url).await;

    let deadline = Instant::now() + START_TIMEOUT;
    let jobs = loop {
        let answer = call(&mgr, "jobs_list", json!({})).await;
        if !is_error(&answer) {
            break serde_json::from_str::<Value>(&text_of(&answer)).expect("json");
        }
        assert!(
            Instant::now() < deadline,
            "manager never answered: {}",
            text_of(&answer)
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
    };
    assert_eq!(jobs["jobs"][0]["name"], "echo");

    let answer = call(&mgr, "job_run", json!({ "job": "echo" })).await;
    assert!(!is_error(&answer), "{}", text_of(&answer));
    let body: Value = serde_json::from_str(&text_of(&answer)).expect("json");
    assert_eq!(body["requested"], "echo");

    let answer = call(&mgr, "job_run", json!({ "job": "ghost" })).await;
    assert!(is_error(&answer));
    assert!(text_of(&answer).contains("404"), "{}", text_of(&answer));

    let deadline = Instant::now() + RUN_TIMEOUT;
    let run = loop {
        let answer = call(&mgr, "runs_list", json!({ "job": "echo", "limit": 5 })).await;
        assert!(!is_error(&answer), "{}", text_of(&answer));
        let body: Value = serde_json::from_str(&text_of(&answer)).expect("json");
        if let Some(row) = body["runs"]
            .as_array()
            .and_then(|rows| rows.iter().find(|r| r["outcome"] == "ok"))
        {
            break row.clone();
        }
        assert!(Instant::now() < deadline, "no ok run yet: {body}");
        tokio::time::sleep(Duration::from_millis(300)).await;
    };
    let run_id = run["run_id"].as_str().expect("run_id").to_string();

    let answer = call(&mgr, "run_get", json!({ "run_id": run_id })).await;
    assert!(!is_error(&answer), "{}", text_of(&answer));
    let body: Value = serde_json::from_str(&text_of(&answer)).expect("json");
    assert_eq!(body["run"]["run_id"], run_id);
    assert_eq!(body["run"]["outcome"], "ok");
    // A run is not given a log of its own: it writes to its project's, with
    // the labels it was started under saying which run it was.
    assert!(body["run"]["log"].is_null(), "{}", body["run"]);

    let answer = call(&mgr, "run_stop", json!({ "run_id": "nope" })).await;
    assert!(is_error(&answer));
    assert!(text_of(&answer).contains("404"), "{}", text_of(&answer));

    mgr.disconnect_all().await.expect("disconnect");
    drop(manager);
}
