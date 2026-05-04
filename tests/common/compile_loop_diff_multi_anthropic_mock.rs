//! In-process Anthropic Messages API mock server for compile_loop multi-file diff-mode e2e tests.
#![allow(dead_code)]
//!
//! Provides two mock servers:
//!
//! ## Happy path (1-turn, 2-file):
//!   - Single HTTP call returns path-header SEARCH/REPLACE for both file_a and file_b.
//!   - apply succeeds on both files → mock_runner returns {ok=true} → loop converges in 1 turn.
//!
//! ## 2-iter scenario:
//!   - Turn 1 (prev == 0): file_a SEARCH deliberately mismatches ("WRONG").
//!     compile_loop detects apply failure and feeds back a retry message.
//!   - Turn 2 (prev >= 1): correct SEARCH for file_a (and file_b) → apply succeeds → ok=true.
//!
//! Path-header format (subtask-2.md design choice 3):
//!   <<< path=<abs_path> >>>
//!   <<<<<<< SEARCH
//!   <old content>
//!   =======
//!   <new content>
//!   >>>>>>> REPLACE
//!
//! The mock extracts the absolute paths from the request body (user message contains
//!   `=== Current file content (path=<abs_path>) ===`).
//!
//! Initial file contents written by the Lua fixture:
//!   file_a: `print("a-old")\n`
//!   file_b: `print("b-old")\n`
//! After successful apply:
//!   file_a: `print("a-new")\n`
//!   file_b: `print("b-new")\n`

use axum::{
    extract::State,
    http::{header, StatusCode},
    response::IntoResponse,
    routing::post,
    Router,
};
use serde_json::json;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

/// Shared state for the multi-file diff-mode Anthropic mock.
#[derive(Clone)]
pub struct MultiDiffMockState {
    pub call_count: Arc<AtomicUsize>,
}

// ---------------------------------------------------------------------------
// Path extraction helper
// ---------------------------------------------------------------------------

/// Extract absolute file paths from a compile_loop messages request body.
///
/// The Lua init.lua embeds target file paths in the user message using one of:
///   1. Multi-file lazy-load format: `Files:\n  <abs_path>\n  <abs_path>`
///   2. Single-file diff format: `=== Current file content (path=<abs_path>) ===`
///   3. Non-existent file format: `=== File (path=<abs_path>) does not exist yet ===`
///
/// Returns at most 2 paths in order of appearance.
fn extract_paths_from_request(body: &serde_json::Value) -> Vec<String> {
    let mut paths = Vec::new();
    let messages = match body.get("messages").and_then(|m| m.as_array()) {
        Some(m) => m,
        None => return paths,
    };
    for msg in messages {
        let content = match msg.get("content").and_then(|c| c.as_str()) {
            Some(c) => c,
            None => continue,
        };
        // Match multi-file lazy-load format:
        //   `Files:\n  <abs_path>\n  <abs_path>`
        // Lines after "Files:" that start with whitespace + "/" are absolute paths.
        let mut in_files_section = false;
        for line in content.lines() {
            if line.trim() == "Files:" {
                in_files_section = true;
                continue;
            }
            if in_files_section {
                let trimmed = line.trim();
                if trimmed.starts_with('/') {
                    let p = trimmed.to_string();
                    if !p.is_empty() && !paths.contains(&p) {
                        paths.push(p);
                    }
                    if paths.len() >= 2 {
                        break;
                    }
                } else if !trimmed.is_empty() {
                    // Non-empty, non-path line ends the Files: section.
                    in_files_section = false;
                }
            }
        }

        if paths.len() >= 2 {
            break;
        }

        // Match `=== Current file content (path=<abs_path>) ===`
        // or `=== File (path=<abs_path>) does not exist yet ===`
        for line in content.lines() {
            if let Some(rest) = line.strip_prefix("=== Current file content (path=") {
                if let Some(path) = rest.strip_suffix(") ===") {
                    let p = path.trim().to_string();
                    if !p.is_empty() && !paths.contains(&p) {
                        paths.push(p);
                    }
                }
            } else if let Some(rest) = line.strip_prefix("=== File (path=") {
                if let Some(path) = rest.strip_suffix(") does not exist yet ===") {
                    let p = path.trim().to_string();
                    if !p.is_empty() && !paths.contains(&p) {
                        paths.push(p);
                    }
                }
            }
        }
        if paths.len() >= 2 {
            break;
        }
    }
    paths
}

// ---------------------------------------------------------------------------
// Happy-path handler (always returns correct SEARCH/REPLACE for both files)
// ---------------------------------------------------------------------------

/// POST /v1/messages handler — always returns a correct 2-file SEARCH/REPLACE response.
///
/// Both file_a and file_b are patched in a single LLM turn.
/// Paths are extracted from the request body so they match the absolute paths used
/// by the Lua fixture.
async fn multi_diff_happy_handler(
    State(state): State<MultiDiffMockState>,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    let req_value = match serde_json::from_slice::<serde_json::Value>(&body) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[compile_loop_diff_multi_anthropic_mock] bad request body: {e}");
            let err_body = json!({ "error": format!("bad request: {e}") }).to_string();
            return (
                StatusCode::BAD_REQUEST,
                [(header::CONTENT_TYPE, "application/json")],
                err_body,
            );
        }
    };

    let prev = state.call_count.fetch_add(1, Ordering::SeqCst);
    let paths = extract_paths_from_request(&req_value);

    // Build path-header SEARCH/REPLACE for both files using absolute paths.
    // Falls back to basename-only if paths could not be extracted (should not happen in normal runs).
    let (path_a, path_b) = if paths.len() >= 2 {
        (paths[0].clone(), paths[1].clone())
    } else if paths.len() == 1 {
        (paths[0].clone(), "file_b.lua".to_string())
    } else {
        ("file_a.lua".to_string(), "file_b.lua".to_string())
    };

    // Both files with correct SEARCH text — single-turn happy path.
    let text = format!(
        "<<< path={path_a} >>>\n<<<<<<< SEARCH\nprint(\"a-old\")\n=======\nprint(\"a-new\")\n>>>>>>> REPLACE\n\n<<< path={path_b} >>>\n<<<<<<< SEARCH\nprint(\"b-old\")\n=======\nprint(\"b-new\")\n>>>>>>> REPLACE"
    );

    let response_json = json!({
        "id": format!("msg_multi_diff_happy_{}", prev + 1),
        "type": "message",
        "role": "assistant",
        "content": [{ "type": "text", "text": text }],
        "model": "claude-haiku-mock",
        "stop_reason": "end_turn",
        "usage": { "input_tokens": 10, "output_tokens": 20 }
    });

    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        response_json.to_string(),
    )
}

// ---------------------------------------------------------------------------
// 2-iter handler (Turn 1: file_a SEARCH mismatch; Turn 2+: correct)
// ---------------------------------------------------------------------------

/// POST /v1/messages handler — deliberate file_a mismatch on Turn 1, correct on Turn 2+.
///
/// Turn 1: file_a SEARCH is "WRONG" (not in initial content) — apply fails for file_a.
///   compile_loop feeds back failure and sends a second LLM call.
/// Turn 2+: correct SEARCH for both file_a and file_b → apply succeeds → ok=true.
async fn multi_diff_two_iter_handler(
    State(state): State<MultiDiffMockState>,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    let req_value = match serde_json::from_slice::<serde_json::Value>(&body) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[compile_loop_diff_multi_anthropic_mock] bad request body: {e}");
            let err_body = json!({ "error": format!("bad request: {e}") }).to_string();
            return (
                StatusCode::BAD_REQUEST,
                [(header::CONTENT_TYPE, "application/json")],
                err_body,
            );
        }
    };

    let prev = state.call_count.fetch_add(1, Ordering::SeqCst);
    let paths = extract_paths_from_request(&req_value);

    let (path_a, path_b) = if paths.len() >= 2 {
        (paths[0].clone(), paths[1].clone())
    } else if paths.len() == 1 {
        (paths[0].clone(), "file_b.lua".to_string())
    } else {
        ("file_a.lua".to_string(), "file_b.lua".to_string())
    };

    let text = if prev == 0 {
        // Turn 1: only file_a with deliberately wrong SEARCH — "WRONG" is not in the file.
        // file_b is omitted so it remains untouched and can be correctly patched on turn 2.
        format!(
            "<<< path={path_a} >>>\n<<<<<<< SEARCH\nprint(\"WRONG\")\n=======\nprint(\"a-new\")\n>>>>>>> REPLACE"
        )
    } else {
        // Turn 2+: correct SEARCH for both files (both still contain original content).
        format!(
            "<<< path={path_a} >>>\n<<<<<<< SEARCH\nprint(\"a-old\")\n=======\nprint(\"a-new\")\n>>>>>>> REPLACE\n\n<<< path={path_b} >>>\n<<<<<<< SEARCH\nprint(\"b-old\")\n=======\nprint(\"b-new\")\n>>>>>>> REPLACE"
        )
    };

    let response_json = json!({
        "id": format!("msg_multi_diff_two_iter_{}", prev + 1),
        "type": "message",
        "role": "assistant",
        "content": [{ "type": "text", "text": text }],
        "model": "claude-haiku-mock",
        "stop_reason": "end_turn",
        "usage": { "input_tokens": 10, "output_tokens": 20 }
    });

    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        response_json.to_string(),
    )
}

// ---------------------------------------------------------------------------
// Spawn helpers
// ---------------------------------------------------------------------------

/// Spawn an in-process Anthropic mock for the multi-file diff-mode happy-path test.
///
/// Returns a single-turn mock: one HTTP call → both files patched → loop converges.
///
/// # Returns
/// - `base_url`: pass as `ANTHROPIC_BASE_URL_TEST`.
/// - `call_count`: assert `load(SeqCst) == 1` after the subprocess.
/// - `ct`: call `ct.cancel()` to shut down gracefully.
///
/// # Panics
/// Panics only on OS-level port bind failure (fatal test infra condition).
pub async fn spawn_compile_loop_diff_multi_anthropic_mock_server(
) -> (String, Arc<AtomicUsize>, CancellationToken) {
    let call_count = Arc::new(AtomicUsize::new(0));
    let ct = CancellationToken::new();

    let state = MultiDiffMockState {
        call_count: call_count.clone(),
    };

    let router = Router::new()
        .route("/v1/messages", post(multi_diff_happy_handler))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port for compile_loop diff multi anthropic mock");
    let addr = listener.local_addr().expect("local_addr");

    let ct_shutdown = ct.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router)
            .with_graceful_shutdown(async move { ct_shutdown.cancelled_owned().await })
            .await;
    });

    (format!("http://{addr}"), call_count, ct)
}

/// Spawn an in-process Anthropic mock for the multi-file diff-mode 2-iter test.
///
/// Returns a two-turn mock: turn 1 has file_a SEARCH mismatch → turn 2 corrects both files.
///
/// # Returns
/// - `base_url`: pass as `ANTHROPIC_BASE_URL_TEST`.
/// - `call_count`: assert `load(SeqCst) == 2` after the subprocess.
/// - `ct`: call `ct.cancel()` to shut down gracefully.
///
/// # Panics
/// Panics only on OS-level port bind failure (fatal test infra condition).
pub async fn spawn_compile_loop_diff_multi_anthropic_mock_two_iter_server(
) -> (String, Arc<AtomicUsize>, CancellationToken) {
    let call_count = Arc::new(AtomicUsize::new(0));
    let ct = CancellationToken::new();

    let state = MultiDiffMockState {
        call_count: call_count.clone(),
    };

    let router = Router::new()
        .route("/v1/messages", post(multi_diff_two_iter_handler))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port for compile_loop diff multi anthropic mock two-iter");
    let addr = listener.local_addr().expect("local_addr");

    let ct_shutdown = ct.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router)
            .with_graceful_shutdown(async move { ct_shutdown.cancelled_owned().await })
            .await;
    });

    (format!("http://{addr}"), call_count, ct)
}
