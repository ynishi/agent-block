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
//!
//! # Where the waiting happens
//!
//! All three entries are async functions, and every `read` / `write` they do
//! runs in `tokio::task::spawn_blocking`. The VM thread keeps the parts that
//! need the Lua state or are pure CPU work — reading the options table,
//! hashing, the range / `expect` / overlap checks, splicing the lines, and
//! building the result table — and yields for the file I/O, so a slow disk
//! stops this call and nothing else. That is the same split the
//! `mlua_batteries::async_overrides` versions of `std.fs.read` / `.write` make
//! (installed in `host.rs`), which is what these sit beside in the `std.fs`
//! table.
//!
//! Being async, they run under `call_async` / `eval_async` — which is how a
//! block script, a `std.task` body, and a `tool.call` handler are all driven.
//! A plain `Lua::load(...).eval()` cannot call them.

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

/// Run `f` on the blocking pool, reporting a join failure as a Lua error.
///
/// `op` names the Lua function in the message: a join error means the blocking
/// task panicked or the runtime is going away, neither of which is an I/O
/// failure, so it does not get to wear one's wording.
async fn blocking<T, F>(op: &'static str, f: F) -> LuaResult<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| LuaError::external(format!("{op}: spawn_blocking: {e}")))
}

/// Read the file, off the VM thread.
///
/// The message the caller sees is unchanged from when this was synchronous,
/// `fs.edit` prefix included: `read_versioned` and `edit` are its only two
/// callers and they shared it then too. The blocking closure returns the text
/// of the failure rather than a [`LuaError`], which is not `Send`; it becomes
/// one back on the VM thread.
async fn read_to_string(path: String) -> LuaResult<String> {
    blocking("fs.read", move || {
        std::fs::read_to_string(&path).map_err(|e| format!("fs.edit: cannot read {path}: {e}"))
    })
    .await?
    .map_err(LuaError::external)
}

/// Write the file, off the VM thread. `op` names the caller for the message.
async fn write_string(op: &'static str, path: String, content: String) -> LuaResult<()> {
    blocking("fs.write", move || {
        std::fs::write(&path, &content).map_err(|e| format!("{op}: cannot write {path}: {e}"))
    })
    .await?
    .map_err(LuaError::external)
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
        lua.create_async_function(|lua: Lua, path: String| async move {
            let content = read_to_string(path).await?;
            let (lines, _) = split_lines(&content);
            let t = lua.create_table()?;
            t.set("content", content.as_str())?;
            t.set("lines", lines.len())?;
            t.set("version", version_of(&content))?;
            Ok(t)
        })?,
    )?;

    // ── edit ──────────────────────────────────────────────────────
    //
    // Two awaits — the read at the top and the write at the bottom — with
    // every check between them on the VM thread, where the options table is.
    // A rejection returns before the write, so a refused batch does not even
    // reach the blocking pool.
    let edit_snapshots = Arc::clone(&snapshots);
    fs_tbl.set(
        "edit",
        lua.create_async_function(move |lua: Lua, (path, opts): (String, LuaTable)| {
            let snapshots = Arc::clone(&edit_snapshots);
            async move {
                let content = read_to_string(path.clone()).await?;
                let current_version = version_of(&content);

                // Stale premise: the file is not the one the caller read.
                if let Ok(base) = opts.get::<String>("base") {
                    if !base.is_empty() && base != current_version {
                        let t = failure(&lua, "stale_base")?;
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
                        let t = failure(&lua, "bad_range")?;
                        t.set("edit_index", i + 1)?;
                        t.set("start_line", start_line)?;
                        t.set("end_line", end_line)?;
                        return Ok(t);
                    }
                    if end_line > lines.len() {
                        let t = failure(&lua, "out_of_range")?;
                        t.set("edit_index", i + 1)?;
                        t.set("end_line", end_line)?;
                        t.set("file_lines", lines.len())?;
                        return Ok(t);
                    }

                    let actual = lines[start_line - 1..end_line].join("\n");
                    if actual != expect {
                        let t = failure(&lua, "expect_mismatch")?;
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
                    let t = failure(&lua, "no_edits")?;
                    return Ok(t);
                }

                // Overlap check across the whole set, before anything is
                // applied.
                let mut ordered: Vec<&Edit> = edits.iter().collect();
                ordered.sort_by_key(|e| e.start_line);
                for pair in ordered.windows(2) {
                    if pair[0].end_line >= pair[1].start_line {
                        let t = failure(&lua, "overlapping_edits")?;
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
                let version = version_of(&new_content);
                write_string("fs.edit", path.clone(), new_content).await?;

                if let Ok(mut map) = snapshots.lock() {
                    map.insert(PathBuf::from(&path), content);
                }

                let t = lua.create_table()?;
                t.set("ok", true)?;
                t.set("applied", edits.len())?;
                t.set("version", version)?;
                Ok(t)
            }
        })?,
    )?;

    // ── rollback ──────────────────────────────────────────────────
    let rollback_snapshots = Arc::clone(&snapshots);
    fs_tbl.set(
        "rollback",
        lua.create_async_function(move |lua: Lua, path: String| {
            let snapshots = Arc::clone(&rollback_snapshots);
            async move {
                let key = PathBuf::from(&path);
                let previous = snapshots.lock().ok().and_then(|mut map| map.remove(&key));

                match previous {
                    Some(content) => {
                        let version = version_of(&content);
                        write_string("fs.rollback", path, content).await?;
                        let t = lua.create_table()?;
                        t.set("ok", true)?;
                        t.set("version", version)?;
                        Ok(t)
                    }
                    None => failure(&lua, "no_snapshot"),
                }
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

    // -- the rule this round exists for -------------------------------------

    /// **A slow `std.fs.edit` does not stop the VM.**
    ///
    /// Asserted directly rather than inferred from the shape of the code, the
    /// way `bridge::ts` asserts the same property for a contended write: a
    /// second coroutine on the same Lua state goes on running — advancing a
    /// counter through an async function of its own — for the whole time an
    /// edit is waiting to read its file.
    ///
    /// The blocker is a FIFO. Opening one for reading waits for a writer, so
    /// the read at the top of `fs.edit` takes exactly as long as the writer
    /// thread makes it, with no large file and no sleeping inside the bridge.
    /// The edit is then refused for a stale `base`, which returns *before* the
    /// write — writing to the FIFO with nobody reading would block forever.
    ///
    /// Before this round the read was `std::fs::read_to_string` in a
    /// synchronous `create_function`, on the VM thread, and the ticker counted
    /// nothing until it returned.
    ///
    /// Linux-only because it needs `mkfifo`; the code under test is not.
    ///
    /// # Test categories
    ///
    /// - (T1) Happy path: the edit returns its verdict once the file is
    ///   readable.
    /// - (T2) Concurrency: ticks keep landing while the read waits.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_slow_edit_does_not_block_another_coroutine_on_the_same_vm() {
        use std::os::unix::ffi::OsStrExt;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Duration;

        /// How long the writer makes the read wait.
        const HELD: Duration = Duration::from_millis(300);
        /// How long each tick takes, so ~60 fit inside `HELD`.
        const TICK: Duration = Duration::from_millis(5);
        /// The floor the assertion uses. Far below what should actually
        /// happen (~60), because the point is "the VM kept running", not a
        /// measurement of how fast it ran.
        const AT_LEAST: usize = 5;

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime for the VM to yield into");

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("slow.txt");

        {
            let c = std::ffi::CString::new(path.as_os_str().as_bytes())
                .expect("a path with no interior NUL");
            // SAFETY: `c` is a valid NUL-terminated path inside a fresh
            // tempdir, and `mkfifo` only creates a filesystem entry there.
            let rc = unsafe { libc::mkfifo(c.as_ptr(), 0o600) };
            assert_eq!(rc, 0, "mkfifo: {}", std::io::Error::last_os_error());
        }

        // Opening the FIFO for writing releases the reader, so this is what
        // decides how long the edit's read takes.
        let writer_path = path.clone();
        let writer = std::thread::spawn(move || {
            std::thread::sleep(HELD);
            std::fs::write(&writer_path, "alpha\n").expect("feed the fifo");
        });

        let lua = Lua::new();
        let std_tbl = lua.create_table().expect("std table");
        std_tbl
            .set("fs", lua.create_table().expect("fs table"))
            .expect("set std.fs");
        lua.globals().set("std", std_tbl).expect("set std");
        register(&lua, SnapshotStore::default()).expect("register the fs primitives");
        lua.globals()
            .set("PATH", path.to_string_lossy().as_ref())
            .expect("set PATH");

        // `tick()` waits like any async bridge function does; `ticks()` reads
        // the counter without waiting for anything.
        let ticks = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&ticks);
        let tick = lua
            .create_async_function(move |_, ()| {
                let counter = Arc::clone(&counter);
                async move {
                    tokio::time::sleep(TICK).await;
                    counter.fetch_add(1, Ordering::Relaxed);
                    Ok(())
                }
            })
            .expect("create tick");
        lua.globals().set("tick", tick).expect("set tick");
        let counter = Arc::clone(&ticks);
        let read_ticks = lua
            .create_function(move |_, ()| Ok(counter.load(Ordering::Relaxed)))
            .expect("create ticks");
        lua.globals().set("ticks", read_ticks).expect("set ticks");

        // Two coroutines, driven together on the VM's runtime: one blocked on
        // the read, one counting. `during` is the number of ticks that landed
        // while the read was waiting.
        let during: usize = rt.block_on(async {
            let editor = lua
                .load(
                    r#"
                    local before = ticks()
                    local r = std.fs.edit(PATH, {
                        base = "0000000000000000",
                        edits = {
                            { start_line = 1, end_line = 1, expect = "alpha", replace = "ALPHA" },
                        },
                    })
                    assert(r.ok == false, "the edit should have been refused")
                    assert(r.reason == "stale_base", "refused for: " .. tostring(r.reason))
                    return ticks() - before
                "#,
                )
                .eval_async::<usize>();
            let ticker = lua.load(r#"for _ = 1, 200 do tick() end"#).exec_async();
            // Both futures poll the same Lua state on this one thread, which
            // is exactly what the VM's own LocalSet does with its coroutines.
            let (edited, _) = tokio::join!(editor, ticker);
            edited.expect("the edit eventually returns")
        });

        writer.join().expect("the writer thread");

        assert!(
            during >= AT_LEAST,
            "the VM stopped while the read was waiting: only {during} tick(s) ran"
        );
    }
}
