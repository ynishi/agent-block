//! `std.fs` editing primitives, layered on the mlua-batteries `fs` module.
//!
//! mlua-batteries gives Lua `read` / `write`; what a coding loop actually
//! needs is "change these lines and nothing else", and that is the operation
//! every block has so far reinvented for itself.
//!
//! # Why this is not `Edit(old_string, new_string)`
//!
//! The usual shape searches the file for `old_string`. That makes the *text*
//! the address, which brings three problems the caller then has to work
//! around: the match may be ambiguous (so the model pads context until it is
//! unique, without knowing how much is enough), the match must reproduce
//! whitespace exactly (which models do unreliably, pushing implementations
//! toward fuzzy matching and therefore toward edits landing in the wrong
//! place), and nothing detects that the file changed between the read the
//! model reasoned about and the write it asked for.
//!
//! Here the address is a line range and the text is only a *check*:
//!
//! * `read_versioned` returns the content plus a `version` fingerprint.
//! * `edit` takes that version as `base`. If the file moved on, nothing is
//!   applied — the model's premise is stale and a "successful" write would be
//!   against a file it never saw.
//! * each edit names `start_line` / `end_line` and the `expect`ed text there.
//!   A mismatch returns the text that is actually at those lines, so the
//!   caller can correct without re-reading the whole file.
//! * edits are validated as a set (in range, non-overlapping) and applied
//!   bottom-up in one write, so a rejected batch leaves the file untouched
//!   rather than half-edited.
//!
//! There is deliberately no fuzzy fallback and no `replace_all`. A precise
//! failure is more useful than a guess at what the caller meant, and "replace
//! every occurrence" is how a failed uniqueness check turns into damage.
//!
//! `rollback` restores the content captured before the last successful edit,
//! which is what lets a loop discard an iteration it decided was wrong.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use mlua::prelude::*;
use sha2::{Digest, Sha256};

/// Pre-edit content, keyed by path — one level deep, which is what "undo the
/// last thing I did" needs.
pub type SnapshotStore = Arc<Mutex<HashMap<PathBuf, String>>>;

/// Content fingerprint used as the optimistic-concurrency token.
fn version_of(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    format!("{:x}", hasher.finalize())[..16].to_string()
}

/// Split into lines, remembering whether the file ended with a newline so the
/// edit round-trip does not silently add or drop one.
fn split_lines(content: &str) -> (Vec<&str>, bool) {
    let trailing_newline = content.ends_with('\n');
    let body = if trailing_newline {
        &content[..content.len() - 1]
    } else {
        content
    };
    if body.is_empty() && trailing_newline {
        return (vec![""], true);
    }
    (body.split('\n').collect(), trailing_newline)
}

fn join_lines(lines: &[String], trailing_newline: bool) -> String {
    let mut out = lines.join("\n");
    if trailing_newline {
        out.push('\n');
    }
    out
}

/// One requested edit, already validated against the current content.
struct Edit {
    index: usize,
    start_line: usize,
    end_line: usize,
    replace: String,
}

fn read_to_string(path: &str) -> LuaResult<String> {
    std::fs::read_to_string(path)
        .map_err(|e| LuaError::external(format!("fs.edit: cannot read {path}: {e}")))
}

/// Build the `{ ok = false, reason = ..., ... }` table returned for every
/// rejection. Failures are values, not Lua errors: the caller is usually an
/// LLM tool handler that has to turn the reason into a message.
fn failure(lua: &Lua, reason: &str) -> LuaResult<LuaTable> {
    let t = lua.create_table()?;
    t.set("ok", false)?;
    t.set("reason", reason)?;
    Ok(t)
}

pub fn register(lua: &Lua, snapshots: SnapshotStore) -> LuaResult<()> {
    let globals = lua.globals();
    let std_tbl: LuaTable = globals.get("std")?;
    let fs_tbl: LuaTable = std_tbl.get("fs")?;

    // ── read_versioned ────────────────────────────────────────────
    fs_tbl.set(
        "read_versioned",
        lua.create_function(|lua, path: String| {
            let content = read_to_string(&path)?;
            let (lines, _) = split_lines(&content);
            let t = lua.create_table()?;
            t.set("content", content.as_str())?;
            t.set("lines", lines.len())?;
            t.set("version", version_of(&content))?;
            Ok(t)
        })?,
    )?;

    // ── edit ──────────────────────────────────────────────────────
    let edit_snapshots = Arc::clone(&snapshots);
    fs_tbl.set(
        "edit",
        lua.create_function(move |lua, (path, opts): (String, LuaTable)| {
            let content = read_to_string(&path)?;
            let current_version = version_of(&content);

            // Stale premise: the file is not the one the caller read.
            if let Ok(base) = opts.get::<String>("base") {
                if !base.is_empty() && base != current_version {
                    let t = failure(lua, "stale_base")?;
                    t.set("expected_version", base)?;
                    t.set("actual_version", current_version)?;
                    return Ok(t);
                }
            }

            let edits_tbl: LuaTable = opts.get("edits")?;
            let (lines, trailing_newline) = split_lines(&content);
            let mut edits: Vec<Edit> = Vec::new();

            for (i, entry) in edits_tbl.sequence_values::<LuaTable>().enumerate() {
                let entry = entry?;
                let start_line: usize = entry.get("start_line")?;
                let end_line: usize = entry.get("end_line")?;
                let expect: String = entry.get("expect")?;
                let replace: String = entry.get("replace")?;

                if start_line == 0 || end_line < start_line {
                    let t = failure(lua, "bad_range")?;
                    t.set("edit_index", i + 1)?;
                    t.set("start_line", start_line)?;
                    t.set("end_line", end_line)?;
                    return Ok(t);
                }
                if end_line > lines.len() {
                    let t = failure(lua, "out_of_range")?;
                    t.set("edit_index", i + 1)?;
                    t.set("end_line", end_line)?;
                    t.set("file_lines", lines.len())?;
                    return Ok(t);
                }

                let actual = lines[start_line - 1..end_line].join("\n");
                if actual != expect {
                    let t = failure(lua, "expect_mismatch")?;
                    t.set("edit_index", i + 1)?;
                    t.set("start_line", start_line)?;
                    t.set("end_line", end_line)?;
                    // The text actually there, so the caller can correct
                    // without re-reading the file.
                    t.set("actual", actual)?;
                    return Ok(t);
                }

                edits.push(Edit {
                    index: i + 1,
                    start_line,
                    end_line,
                    replace,
                });
            }

            if edits.is_empty() {
                let t = failure(lua, "no_edits")?;
                return Ok(t);
            }

            // Overlap check across the whole set, before anything is applied.
            let mut ordered: Vec<&Edit> = edits.iter().collect();
            ordered.sort_by_key(|e| e.start_line);
            for pair in ordered.windows(2) {
                if pair[0].end_line >= pair[1].start_line {
                    let t = failure(lua, "overlapping_edits")?;
                    t.set("edit_index", pair[0].index)?;
                    t.set("other_edit_index", pair[1].index)?;
                    return Ok(t);
                }
            }

            // Bottom-up so earlier line numbers stay valid as we splice.
            let mut out: Vec<String> = lines.iter().map(|s| (*s).to_string()).collect();
            for e in ordered.iter().rev() {
                let replacement: Vec<String> = if e.replace.is_empty() {
                    Vec::new()
                } else {
                    e.replace.split('\n').map(|s| s.to_string()).collect()
                };
                out.splice(e.start_line - 1..e.end_line, replacement);
            }

            let new_content = join_lines(&out, trailing_newline);
            std::fs::write(&path, &new_content)
                .map_err(|e| LuaError::external(format!("fs.edit: cannot write {path}: {e}")))?;

            if let Ok(mut map) = edit_snapshots.lock() {
                map.insert(PathBuf::from(&path), content);
            }

            let t = lua.create_table()?;
            t.set("ok", true)?;
            t.set("applied", edits.len())?;
            t.set("version", version_of(&new_content))?;
            Ok(t)
        })?,
    )?;

    // ── rollback ──────────────────────────────────────────────────
    let rollback_snapshots = Arc::clone(&snapshots);
    fs_tbl.set(
        "rollback",
        lua.create_function(move |lua, path: String| {
            let key = PathBuf::from(&path);
            let previous = rollback_snapshots
                .lock()
                .ok()
                .and_then(|mut map| map.remove(&key));

            match previous {
                Some(content) => {
                    std::fs::write(&path, &content).map_err(|e| {
                        LuaError::external(format!("fs.rollback: cannot write {path}: {e}"))
                    })?;
                    let t = lua.create_table()?;
                    t.set("ok", true)?;
                    t.set("version", version_of(&content))?;
                    Ok(t)
                }
                None => failure(lua, "no_snapshot"),
            }
        })?,
    )?;

    // Load std.fs.register_tools (LLM-facing helper; requires `tool` global).
    lua.load(include_str!("fs_tools.lua"))
        .set_name("std.fs.register_tools")
        .exec()?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines_of(s: &str) -> Vec<String> {
        split_lines(s).0.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn split_and_join_round_trip() {
        for s in ["a\nb\nc\n", "a\nb\nc", "", "\n", "single"] {
            let (lines, nl) = split_lines(s);
            let owned: Vec<String> = lines.iter().map(|x| x.to_string()).collect();
            assert_eq!(join_lines(&owned, nl), s, "round trip failed for {s:?}");
        }
    }

    #[test]
    fn version_changes_with_content() {
        assert_ne!(version_of("a"), version_of("b"));
        assert_eq!(version_of("a"), version_of("a"));
    }

    #[test]
    fn lines_helper_counts_final_newline_once() {
        assert_eq!(lines_of("a\nb\n").len(), 2);
        assert_eq!(lines_of("a\nb").len(), 2);
    }
}
