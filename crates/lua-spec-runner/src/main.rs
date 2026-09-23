//! Runs the repository's mlua-lspec specs: each module's own
//! `crates/agent-block-core/blocks/{,lib/}<module>/spec/`, plus any lspec
//! fixture left in `crates/agent-block/tests/fixtures/`.
//!
//! ```text
//! lua-spec-runner [<filter>]                 # the repository's specs
//! lua-spec-runner --project <dir> [<filter>] # a project's vendored specs,
//!                                            # against its vendored modules
//! ```
//!
//! `--project` is the other half of `agent-block vendor`: a vendored module
//! is written with its `spec/`, and this runs those specs with the project's
//! `.agent-block/lib/` first on the require path — the same order the host
//! uses — so the copy the project edited is the one they check.
//!
//! Both are Lua unit tests for the Lua side of the runtime (`blocks/agent`,
//! `blocks/lib/llm_proto`, `blocks/lib/knl`, `blocks/lib/policy`).
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
//! just test-lua llm_proto_spec  # fixtures whose name contains the argument
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
/// rather than by filename: the fixture directory holds e2e scripts and specs
/// side by side under no one naming convention, and a convention nobody
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

    // Both places a module lives: `blocks/<name>/` (the blocks, `agent` and
    // `coding`) and `blocks/lib/<name>/` (the libraries). A spec sits in the
    // `spec/` of whichever its module is in.
    let blocks = root.join("crates/agent-block-core/blocks");
    let mut block_specs: Vec<PathBuf> = Vec::new();
    for parent in [blocks.clone(), blocks.join("lib")] {
        let entries =
            std::fs::read_dir(&parent).unwrap_or_else(|e| panic!("{}: {e}", parent.display()));
        block_specs.extend(entries.filter_map(|entry| {
            let spec = entry.expect("readable directory entry").path().join("spec");
            spec.is_dir().then_some(spec)
        }));
    }
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

/// The command line: an optional `--project <dir>`, and an optional filter.
struct Cli {
    project: Option<PathBuf>,
    filter: Option<String>,
}

fn parse_args() -> Result<Cli, String> {
    let mut args = std::env::args().skip(1);
    let mut cli = Cli {
        project: None,
        filter: None,
    };
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--project" | "-p" => {
                let dir = args.next().ok_or("--project needs a directory")?;
                cli.project = Some(PathBuf::from(dir));
            }
            s if s.starts_with("--project=") => {
                cli.project = Some(PathBuf::from(&s["--project=".len()..]));
            }
            s if s.starts_with('-') => return Err(format!("unknown option `{s}`")),
            _ => {
                if cli.filter.is_some() {
                    return Err("one filter at most".to_string());
                }
                cli.filter = Some(arg);
            }
        }
    }
    Ok(cli)
}

/// With `--project <dir>`: the specs a project vendored, each `spec/` under
/// `<dir>/.agent-block/lib/<module>/`, sorted. Only those — a project's run is
/// about its copies, and a module it did not vendor is checked by the plain
/// run against the repository.
fn project_spec_dirs(project: &Path) -> Vec<PathBuf> {
    let lib = project.join(".agent-block").join("lib");
    let Ok(entries) = std::fs::read_dir(&lib) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(|entry| {
            let spec = entry.ok()?.path().join("spec");
            spec.is_dir().then_some(spec)
        })
        .collect();
    dirs.sort();
    dirs
}

/// The embedded modules written in Teal: every `<lib>/<name>/init.tl`, by
/// name, sorted.
fn teal_modules(lib: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(lib) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            path.join("init.tl").is_file().then(|| {
                path.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default()
                    .to_string()
            })
        })
        .collect();
    names.sort();
    names
}

/// Write the Lua the binary embeds for `names` under
/// `target/lua-spec-runner/.agent-block/lib/`, via `agent-block vendor`, and
/// answer that `lib/` directory — `None` when there is nothing to write. The
/// directory is emptied first, so a module that stopped being Teal leaves no
/// stale copy behind, and it is left in place after the run for a reader who
/// wants to see what the specs ran against.
fn vendor_teal_modules(root: &Path, names: &[String]) -> Result<Option<PathBuf>, String> {
    if names.is_empty() {
        return Ok(None);
    }
    let bin = root.join(format!("target/debug/agent-block{}", std::env::consts::EXE_SUFFIX));
    let built = std::fs::metadata(&bin)
        .and_then(|m| m.modified())
        .map_err(|e| {
            format!(
                "{}: not built ({e}) — the specs of a Teal module ({}) run against the Lua \
                 the binary embeds; `cargo build -p agent-block` first (`just test-lua` does)",
                bin.display(),
                names.join(", ")
            )
        })?;
    // A binary older than a `.tl` embeds the Lua of an earlier version of it:
    // the specs would pass or fail against something that is not in the tree.
    for name in names {
        let tl = root.join("crates/agent-block-core/blocks/lib").join(name).join("init.tl");
        let edited = std::fs::metadata(&tl)
            .and_then(|m| m.modified())
            .map_err(|e| format!("{}: {e}", tl.display()))?;
        if edited > built {
            return Err(format!(
                "{} is newer than {} — `cargo build -p agent-block` first (`just test-lua` \
                 does), or the specs run against the Lua of an older {name}",
                tl.display(),
                bin.display()
            ));
        }
    }
    let dir = root.join("target/lua-spec-runner");
    if dir.exists() {
        std::fs::remove_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let output = std::process::Command::new(&bin)
        .arg("vendor")
        .arg("--path")
        .arg(&dir)
        .args(names)
        .output()
        .map_err(|e| format!("{}: {e}", bin.display()))?;
    if !output.status.success() {
        return Err(format!(
            "agent-block vendor {} failed ({}):\n{}",
            names.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(Some(dir.join(".agent-block").join("lib")))
}

fn main() -> ExitCode {
    let cli = match parse_args() {
        Ok(cli) => cli,
        Err(err) => {
            eprintln!("{err}");
            eprintln!("usage: lua-spec-runner [--project <dir>] [<filter>]");
            return ExitCode::FAILURE;
        }
    };

    let root = repo_root();
    let blocks = root.join("crates/agent-block-core/blocks");

    // `require("llm_proto")` / `require("knl")` / `require("agent")` resolve
    // against the two directories blocks are laid out in — the same paths for
    // a fixture and for a spec sitting inside the block it covers. With
    // `--project`, the project's vendored copies come first, in the same order
    // the host searches at runtime (`.agent-block/lib/` ahead of embedded), so
    // a vendored `policy` is the one its vendored spec checks, and a module the
    // project did not vendor still resolves to the repository's — the closest
    // stand-in this runner has for the embedded one.
    let mut search: Vec<String> = Vec::new();
    if let Some(project) = &cli.project {
        search.push(
            project
                .join(".agent-block")
                .join("lib")
                .display()
                .to_string(),
        );
    }
    search.extend(
        ["lib", ""]
            .iter()
            .map(|sub| blocks.join(sub).display().to_string()),
    );
    // An embedded module written in Teal (`blocks/lib/<name>/init.tl`) has no
    // `.lua` for `require` to find here: what the host embeds is the Lua that
    // `include_tl!` generated at `cargo build`. This runner cannot generate
    // it — it sits outside the workspace on an older mlua than htl's, and
    // `htl gen` resolves a module's requires from its own directory only — so
    // it takes the Lua from the binary that embeds it, the way a project does:
    // `agent-block vendor` writes each such module out under `target/`, and
    // the copies go last on the search path, where the embedded tier sits
    // for the host. The binary is the one `cargo build -p agent-block`
    // leaves in `target/debug`, which `just test-lua` builds first.
    // Only for the repository's own run: with `--project`, the specs are a
    // project's and its copies are already first on the path.
    if cli.project.is_none() {
        match vendor_teal_modules(&root, &teal_modules(&blocks.join("lib"))) {
            Ok(Some(dir)) => search.push(dir.display().to_string()),
            Ok(None) => {}
            Err(err) => {
                eprintln!("{err}");
                return ExitCode::FAILURE;
            }
        }
    }
    let search: Vec<&str> = search.iter().map(String::as_str).collect();

    let dirs = match &cli.project {
        Some(project) => project_spec_dirs(project),
        None => spec_dirs(&root),
    };

    let filter = cli.filter;
    let mut specs = Vec::new();
    for dir in &dirs {
        specs.extend(discover(dir, filter.as_deref()));
    }

    if specs.is_empty() {
        // Silence here would read as success. It means the filter matched
        // nothing, or the specs moved — or, with `--project`, that the
        // project has vendored nothing that carries a spec.
        match &cli.project {
            Some(project) if dirs.is_empty() => eprintln!(
                "no vendored specs under {}: `agent-block vendor <name>` writes a module \
                 with its spec/",
                project.join(".agent-block/lib").display()
            ),
            _ => {
                eprintln!("no specs found under any of:");
                for dir in &dirs {
                    eprintln!("  {}", dir.display());
                }
            }
        }
        if let Some(f) = filter {
            eprintln!("(filter: {f})");
        }
        return ExitCode::FAILURE;
    }

    if let Some(project) = &cli.project {
        println!("project: {} ({} spec dirs)", project.display(), dirs.len());
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
