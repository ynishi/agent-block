//! Runs the repository's mlua-lspec specs: the fixtures in
//! `crates/agent-block/tests/fixtures/`, and each block's own
//! `crates/agent-block-core/blocks/lib/<block>/spec/`.
//!
//! Both are Lua unit tests for the Lua side of the runtime (`blocks/agent`,
//! `blocks/lib/llm_proto`, `blocks/lib/knl`, `blocks/tools/compile_loop`).
//! They were reachable only by hand, through the lua-debugger MCP, so nothing
//! ran them on the way to a commit — which is how eight of them came to be
//! failing against a stub that had not kept up with the `std.fs` bridge, and
//! how four more, written beside the block they cover instead of under
//! `tests/fixtures`, went unrun entirely.
//!
//! A spec beside its block is the layout this repository is moving to (the
//! spec reads as part of the module it pins), so discovery follows it rather
//! than asking anyone to file specs where the runner happens to look.
//!
//! # Why this is a separate crate, outside the workspace
//!
//! `mlua-lspec` depends on mlua with the `send` feature. Cargo unifies features
//! across a build graph, so making it a dev-dependency of `agent-block` turns
//! `send` on for `mlua-batteries` as well — and that does not compile, because
//! its `CancelToken` is `!Sync` while `send` requires every async function
//! passed to mlua to be `Send`. Keeping the runner out of `[workspace] members`
//! keeps the two graphs apart: `cargo test --workspace` never builds this, and
//! this never builds `agent-block-core`.
//!
//! If a future `mlua-lspec` puts `send` behind a feature flag, this crate can
//! collapse into an ordinary `tests/lua_specs.rs`.
//!
//! # Usage
//!
//! ```sh
//! just test-lua                 # every fixture
//! just test-lua llm_proto_test  # fixtures whose name contains the argument
//! ```

use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// Specs live at fixed places relative to this crate, so the runner works
/// regardless of the directory it is invoked from.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("crate is two levels below the repository root")
}

/// A fixture is a Lua file that drives the lspec framework. Detected by use
/// rather than by filename: the existing files are named `*_test.lua`,
/// `*_lifecycle.lua` and `*_distill.lua`, and a naming convention nobody
/// enforces is a fixture waiting to be skipped silently.
fn is_spec(source: &str) -> bool {
    source.contains("lust.")
}

/// Every directory a spec may live in: the shared fixture directory, then one
/// `spec/` per block that has one.
///
/// A block without a `spec/` is skipped rather than reported: not every block
/// has unit tests, and a missing directory there is not a broken layout. The
/// fixture directory is not optional in the same way — if it is gone,
/// `discover` says so loudly.
fn spec_dirs(root: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![root.join("crates/agent-block/tests/fixtures")];

    let lib = root.join("crates/agent-block-core/blocks/lib");
    let entries = std::fs::read_dir(&lib).unwrap_or_else(|e| panic!("{}: {e}", lib.display()));
    let mut block_specs: Vec<PathBuf> = entries
        .filter_map(|entry| {
            let spec = entry.expect("readable directory entry").path().join("spec");
            spec.is_dir().then_some(spec)
        })
        .collect();
    // read_dir order is the filesystem's; sort so a run is reproducible.
    block_specs.sort();

    dirs.extend(block_specs);
    dirs
}

fn discover(dir: &Path, filter: Option<&str>) -> Vec<(PathBuf, String)> {
    let mut found = Vec::new();
    let entries = std::fs::read_dir(dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display()));
    for entry in entries {
        let path = entry.expect("readable directory entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("lua") {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if filter.is_some_and(|f| !name.contains(f)) {
            continue;
        }
        let source =
            std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        if is_spec(&source) {
            found.push((path, source));
        }
    }
    found.sort_by(|a, b| a.0.cmp(&b.0));
    found
}

fn main() -> ExitCode {
    let root = repo_root();
    let blocks = root.join("crates/agent-block-core/blocks");
    let dirs = spec_dirs(&root);

    // `require("compile_loop")` / `require("llm_proto")` / `require("knl")` /
    // `require("agent")` resolve against the three directories blocks are laid
    // out in — the same paths for a fixture and for a spec sitting inside the
    // block it covers.
    let search: Vec<String> = ["tools", "lib", ""]
        .iter()
        .map(|sub| blocks.join(sub).display().to_string())
        .collect();
    let search: Vec<&str> = search.iter().map(String::as_str).collect();

    let filter = std::env::args().nth(1);
    let mut specs = Vec::new();
    for dir in &dirs {
        specs.extend(discover(dir, filter.as_deref()));
    }

    if specs.is_empty() {
        // Silence here would read as success. It means the filter matched
        // nothing, or the specs moved.
        eprintln!("no specs found under any of:");
        for dir in &dirs {
            eprintln!("  {}", dir.display());
        }
        if let Some(f) = filter {
            eprintln!("(filter: {f})");
        }
        return ExitCode::FAILURE;
    }

    let mut total_passed = 0;
    let mut total_failed = 0;

    for (path, source) in &specs {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        match mlua_lspec::run_tests(source, name, &search) {
            Ok(summary) => {
                total_passed += summary.passed;
                total_failed += summary.failed;
                let verdict = if summary.failed == 0 { "ok" } else { "FAILED" };
                println!(
                    "{name}: {verdict}. {} passed; {} failed",
                    summary.passed, summary.failed
                );
                for test in summary.tests.iter().filter(|t| !t.passed) {
                    println!("    {} > {}", test.suite, test.name);
                    if let Some(err) = &test.error {
                        println!("      {err}");
                    }
                }
            }
            Err(err) => {
                // The chunk did not load or blew up outside a test body. No
                // counts exist to add, so record it as one failure rather than
                // letting a file that never ran pass silently.
                total_failed += 1;
                println!("{name}: FAILED to run");
                println!("      {err}");
            }
        }
    }

    println!();
    println!(
        "spec result: {}. {total_passed} passed; {total_failed} failed; {} files",
        if total_failed == 0 { "ok" } else { "FAILED" },
        specs.len()
    );

    if total_failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
