//! `host_types.d.tl` against the host: every function the declaration says a
//! host table holds, the running VM holds.
//!
//! The declaration is hand-written (`crates/agent-block-core/blocks/lib/
//! host_types.d.tl`): the `std.*` batteries and the `tool` / `log` bridges
//! are registered as plain Lua functions, which nothing generates a `.d.tl`
//! from. So this is the guard: it reads the declaration's records, asks a VM
//! what it registered (`fixtures/host_types_keys.lua`), and names any
//! function that is declared and not there. Signatures are out of its reach;
//! names — a misspelling, a function the bridge dropped — are not.

mod common;

use std::collections::BTreeMap;

const DECLARATION: &str = include_str!("../../agent-block-core/blocks/lib/host_types.d.tl");

/// `table label → declared function names`, read off the declaration's
/// indentation: a record at four spaces is a global (`Std`, `Tool`, `Log`),
/// one at eight under `Std` is a `std.<name>` table, and a `name: function`
/// line is a function of the nearest enclosing table. Records nested deeper
/// (`ExecResult`, `ReadVersioned`, `Meta`) are value shapes, not tables the
/// host registers, and are skipped.
fn declared() -> BTreeMap<String, Vec<String>> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut top: Option<String> = None;
    let mut sub: Option<String> = None;
    let mut inner_depth = 0usize;
    for raw in DECLARATION.lines() {
        let indent = raw.len() - raw.trim_start().len();
        let line = raw.trim();
        if line.starts_with("---") || line.starts_with("--") {
            continue;
        }
        if let Some(name) = line.strip_prefix("record ") {
            match indent {
                4 => {
                    top = Some(name.to_string());
                    sub = None;
                }
                8 => sub = Some(name.to_string()),
                _ => inner_depth += 1,
            }
            continue;
        }
        if line == "end" {
            if inner_depth > 0 {
                inner_depth -= 1;
            } else if indent == 8 {
                sub = None;
            } else if indent == 4 {
                top = None;
            }
            continue;
        }
        if inner_depth > 0 {
            continue;
        }
        let Some((field, ty)) = line.split_once(':') else {
            continue;
        };
        if !ty.trim_start().starts_with("function") {
            continue;
        }
        let label = match (top.as_deref(), sub.as_deref(), indent) {
            (Some("Std"), Some(table), 12) => format!("std.{}", table.to_lowercase()),
            (Some(global), None, 8) => global.to_lowercase(),
            _ => continue,
        };
        out.entry(label).or_default().push(field.trim().to_string());
    }
    out
}

#[test]
fn every_declared_function_is_registered_by_the_host() {
    let home = tempfile::tempdir().expect("tempdir");
    let project = tempfile::tempdir().expect("tempdir");
    let output = common::agent_block_std_cmd()
        .env("AGENT_BLOCK_HOME", home.path())
        .args(["-p", &project.path().to_string_lossy()])
        .args(["-s", &common::fixture("host_types_keys.lua")])
        .output()
        .expect("run agent-block");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);

    let mut registered: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix("KEYS ") {
            let (label, names) = rest.split_once('=').expect("KEYS <label>=<names>");
            registered.insert(
                label.to_string(),
                names
                    .split(',')
                    .filter(|n| !n.is_empty())
                    .map(str::to_string)
                    .collect(),
            );
        }
    }

    let declared = declared();
    assert!(
        declared.len() >= 9,
        "the declaration reader found only {declared:?} — the parser and the file disagree"
    );
    let mut missing = Vec::new();
    for (table, functions) in &declared {
        let Some(have) = registered.get(table) else {
            missing.push(format!("{table}: the host registers no such table"));
            continue;
        };
        for f in functions {
            if !have.contains(f) {
                missing.push(format!("{table}.{f}: declared, not registered"));
            }
        }
    }
    assert!(
        missing.is_empty(),
        "host_types.d.tl has drifted from the host:\n  {}",
        missing.join("\n  ")
    );
}
