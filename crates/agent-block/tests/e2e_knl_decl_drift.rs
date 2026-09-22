//! `knl.d.tl` against the module: every name the declaration says the `knl`
//! module exports — its functions and tables, `knl.views`, `knl.shapes`,
//! `knl.Outcome` — is one the module exports.
//!
//! The declaration is hand-written (`crates/agent-block-core/blocks/lib/
//! knl.d.tl`) while the module is Lua, so this is the guard on it: it reads
//! the record fields, asks a VM what `require("knl")` holds
//! (`fixtures/knl_decl_keys.lua`), and names any field that is declared and
//! not there. Signatures are out of its reach; names are not.

mod common;

use std::collections::BTreeMap;

const DECLARATION: &str = include_str!("../../agent-block-core/blocks/lib/knl.d.tl");

/// Which nested records of `knl` are tables the module exports, and the
/// label the fixture prints them under.
const TABLES: &[(&str, &str)] = &[
    ("Views", "knl.views"),
    ("Shapes", "knl.shapes"),
    ("OutcomeApi", "knl.Outcome"),
];

/// `label → declared names`. The top-level record's own fields are `knl`;
/// a nested record listed in `TABLES` is that table; every other nested
/// record (`Session`, `Device`, …) is a value shape, not an export, and
/// is skipped along with anything nested deeper.
fn declared() -> BTreeMap<String, Vec<String>> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut nested: Option<String> = None;
    let mut depth = 0usize;
    for raw in DECLARATION.lines() {
        let indent = raw.len() - raw.trim_start().len();
        let line = raw.trim();
        if line.starts_with("--") || line.is_empty() {
            continue;
        }
        if let Some(name) = line.strip_prefix("record ") {
            match indent {
                0 => {}
                4 => nested = Some(name.to_string()),
                _ => depth += 1,
            }
            continue;
        }
        if line == "end" {
            if depth > 0 {
                depth -= 1;
            } else if indent == 4 {
                nested = None;
            }
            continue;
        }
        if depth > 0 {
            continue;
        }
        let Some((field, _)) = line.split_once(':') else {
            continue;
        };
        let field = field.trim().to_string();
        match (indent, nested.as_deref()) {
            (4, None) => out.entry("knl".to_string()).or_default().push(field),
            (8, Some(record)) => {
                if let Some((_, label)) = TABLES.iter().find(|(name, _)| *name == record) {
                    out.entry(label.to_string()).or_default().push(field);
                }
            }
            _ => {}
        }
    }
    out
}

#[test]
fn every_declared_name_is_exported_by_the_module() {
    let home = tempfile::tempdir().expect("tempdir");
    let project = tempfile::tempdir().expect("tempdir");
    let output = common::agent_block_std_cmd()
        .env("AGENT_BLOCK_HOME", home.path())
        .args(["-p", &project.path().to_string_lossy()])
        .args(["-s", &common::fixture("knl_decl_keys.lua")])
        .output()
        .expect("run agent-block");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);

    let mut exported: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix("KEYS ") {
            let (label, names) = rest.split_once('=').expect("KEYS <label>=<names>");
            exported.insert(
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
    assert_eq!(
        declared.len(),
        4,
        "the declaration reader found {declared:?}"
    );
    let mut missing = Vec::new();
    for (table, names) in &declared {
        let Some(have) = exported.get(table) else {
            missing.push(format!("{table}: the module exports no such table"));
            continue;
        };
        for name in names {
            if !have.contains(name) {
                missing.push(format!("{table}.{name}: declared, not exported"));
            }
        }
    }
    assert!(
        missing.is_empty(),
        "knl.d.tl has drifted from the module:\n  {}",
        missing.join("\n  ")
    );
}
