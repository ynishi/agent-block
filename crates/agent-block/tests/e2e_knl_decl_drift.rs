//! The kernel's declaration against the kernel's table: every name the `knl`
//! module DECLARES it exports — its functions and tables, `knl.views`,
//! `knl.shapes`, `knl.Outcome` — is one a running VM finds on it.
//!
//! The module is written in Teal (`crates/agent-block-core/blocks/lib/knl/
//! init.tl`) and its own record is the declaration, so this reads that
//! record's fields, asks a VM what `require("knl")` holds
//! (`fixtures/knl_decl_keys.lua`), and names any field that is declared and
//! not there. Teal holds the module to its record at build time, which is
//! what makes the two agree in the first place; what it cannot see is the
//! VM, where `knl.views` and `knl.Outcome` are tables filled in at load and
//! the dev-mode gate rewrites entries in place. Signatures are out of this
//! test's reach; names are not.

mod common;

use std::collections::BTreeMap;

const DECLARATION: &str = include_str!("../../agent-block-core/blocks/lib/knl/init.tl");

/// Which of the module's file-scope records are tables it exports, and the
/// label the fixture prints each one under. `M` is the module itself and is
/// handled apart; every other record (`Session`, `Device`, …) is a value
/// shape rather than an export, and is skipped.
const TABLES: &[(&str, &str)] = &[
    ("Views", "knl.views"),
    ("Shapes", "knl.shapes"),
    ("OutcomeApi", "knl.Outcome"),
];

/// `label -> declared names`.
///
/// The records are all at the top level of the file — a record nested in `M`
/// would be a KEY of the module in the generated Lua, which is why they are
/// declared beside it and aliased in with `type X = X`. So this walks
/// top-level `local record <Name>` blocks and takes their indented fields;
/// an alias line carries no `:` and falls out on its own.
fn declared() -> BTreeMap<String, Vec<String>> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut current: Option<String> = None;
    for raw in DECLARATION.lines() {
        let indent = raw.len() - raw.trim_start().len();
        let line = raw.trim();
        if line.starts_with("--") || line.is_empty() {
            continue;
        }
        if indent == 0 {
            if let Some(name) = line.strip_prefix("local record ") {
                current = Some(name.trim().to_string());
            } else if line == "end" {
                current = None;
            }
            continue;
        }
        let Some(record) = current.as_deref() else {
            continue;
        };
        if indent != 4 {
            continue;
        }
        let Some((field, _)) = line.split_once(':') else {
            continue;
        };
        let field = field.trim().to_string();
        if field.contains(char::is_whitespace) {
            continue;
        }
        let label = if record == "M" {
            Some("knl")
        } else {
            TABLES
                .iter()
                .find(|(name, _)| *name == record)
                .map(|(_, label)| *label)
        };
        if let Some(label) = label {
            out.entry(label.to_string()).or_default().push(field);
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
        "the knl declaration has drifted from the module:\n  {}",
        missing.join("\n  ")
    );
}
