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
//! * every line number in a call addresses the content as read, before any of
//!   the call's own edits. A batch is one question about one version of the
//!   file, not a script run top to bottom, and the tool description says so —
//!   a caller left to guess will number the second edit against the file the
//!   first one would produce, and every edit after the first is then wrong by
//!   the lines the earlier ones added.
//! * edits are validated as a set (in range, non-overlapping) and applied
//!   bottom-up in one write, so a rejected batch leaves the file untouched
//!   rather than half-edited. Validation does not stop at the first bad edit:
//!   the reply keeps the first failure's fields and adds `failures`, every
//!   rejected edit in order, because a wrong basis shows as a constant offset
//!   across several of them and one rejection cannot show that.
//!
//! There is deliberately no fuzzy fallback and no `replace_all`. A precise
//! failure is more useful than a guess at what the caller meant, and "replace
//! every occurrence" is how a failed uniqueness check turns into damage.
//!
//! `rollback` restores the content captured before the last successful edit,
//! which is what lets a loop discard an iteration it decided was wrong.
//!
//! # `inspect`: what is at a path, without following it
//!
//! `std.fs.exists` / `is_dir` follow a symlink, and say nothing when it
//! leads nowhere, so a caller that is about to write a path cannot tell a
//! free path from a dangling link the write would follow. `inspect(path)`
//! answers both questions in one call:
//!
//! * `kind`: `"file"`, `"dir"`, `"symlink"`, `"other"` (a FIFO, a socket, a
//!   device) or `"missing"` — of the path itself, the last component not
//!   followed (`symlink_metadata`).
//! * `dangling`: `true` for a symlink whose target does not resolve (any
//!   link along the way leads to nothing); `false` for any other symlink;
//!   absent for every other kind.
//! * `canonical`: the path canonicalized (absolute, every symlink and `..`
//!   resolved), when it resolves — absent for `"missing"` and a dangling
//!   link.
//! * `dev` / `ino`: the device and inode of what the path resolves to, on
//!   Unix, absent where the path does not resolve. They are an identity the
//!   canonical string is not: a bind mount, or a Linux casefold directory
//!   named in another case, is the same directory under a different
//!   canonical string, and only `(dev, ino)` says so. Both are the host's
//!   `u64` values carried bit for bit as Lua integers (a value past
//!   `i64::MAX` reads negative), which is enough for comparing them, the
//!   only thing they are for. **Not on other platforms**: std exposes no
//!   stable file identity there, so `dev` / `ino` are absent and a caller
//!   falls back to comparing `canonical`.
//!
//! Not-found is an answer (`"missing"`, or a dangling link); every other
//! failure — a directory that cannot be searched, a symlink loop — raises,
//! so a guard built on this fails closed rather than reading an error as
//! "nothing there".
//!
//! # Where the waiting happens
//!
//! All four entries are async functions, and every `read` / `write` they do
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
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
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

/// One requested edit that did not pass, held until the whole call is checked.
///
/// Collected rather than returned on the spot so the reply can carry every
/// rejected edit: a caller whose line numbers came from the wrong basis is
/// wrong in several of them at once, and that is only visible together.
struct EditFailure {
    index: usize,
    reason: &'static str,
    start_line: usize,
    end_line: usize,
    /// The text actually at that range, for `expect_mismatch`. `None` when the
    /// range could not be read at all.
    actual: Option<String>,
    /// The file's line count, for `out_of_range`.
    file_lines: Option<usize>,
}

impl EditFailure {
    fn to_table(&self, lua: &Lua) -> LuaResult<LuaTable> {
        let t = lua.create_table()?;
        t.set("reason", self.reason)?;
        t.set("edit_index", self.index)?;
        t.set("start_line", self.start_line)?;
        t.set("end_line", self.end_line)?;
        if let Some(actual) = self.actual.as_deref() {
            t.set("actual", actual)?;
        }
        if let Some(file_lines) = self.file_lines {
            t.set("file_lines", file_lines)?;
        }
        Ok(t)
    }
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

/// What `std.fs.inspect` reports a path as: the path itself, its last
/// component not followed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    File,
    Dir,
    Symlink,
    Other,
    Missing,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::File => "file",
            Kind::Dir => "dir",
            Kind::Symlink => "symlink",
            Kind::Other => "other",
            Kind::Missing => "missing",
        }
    }
}

/// One answer of `std.fs.inspect`; see the module doc for each field.
#[derive(Debug)]
struct Inspected {
    kind: Kind,
    /// Meaningful for `Kind::Symlink` only.
    dangling: bool,
    canonical: Option<PathBuf>,
    /// `(dev, ino)` of what the path resolves to; `None` off Unix, and where
    /// it does not resolve.
    id: Option<(u64, u64)>,
}

#[cfg(unix)]
fn identity(meta: &std::fs::Metadata) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    Some((meta.dev(), meta.ino()))
}

/// No stable file identity in std off Unix: callers fall back to the
/// canonical path.
#[cfg(not(unix))]
fn identity(_meta: &std::fs::Metadata) -> Option<(u64, u64)> {
    None
}

/// What is at `path`, without following its last component. Not-found is
/// an answer; any other error is returned, for the caller to raise.
fn inspect_path(path: &Path) -> std::io::Result<Inspected> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == ErrorKind::NotFound => {
            return Ok(Inspected {
                kind: Kind::Missing,
                dangling: false,
                canonical: None,
                id: None,
            })
        }
        Err(e) => return Err(e),
    };
    let ft = meta.file_type();
    let kind = if ft.is_symlink() {
        Kind::Symlink
    } else if ft.is_dir() {
        Kind::Dir
    } else if ft.is_file() {
        Kind::File
    } else {
        Kind::Other
    };
    let canonical = match std::fs::canonicalize(path) {
        Ok(p) => p,
        // The link is there and leads to nothing. Anything else that stops
        // the resolution (a loop, a directory that cannot be searched) is an
        // error, not an answer.
        Err(e) if kind == Kind::Symlink && e.kind() == ErrorKind::NotFound => {
            return Ok(Inspected {
                kind,
                dangling: true,
                canonical: None,
                id: None,
            })
        }
        Err(e) => return Err(e),
    };
    // A link's identity is its target's; anything else's is its own, which
    // `symlink_metadata` already read.
    let id = if kind == Kind::Symlink {
        identity(&std::fs::metadata(&canonical)?)
    } else {
        identity(&meta)
    };
    Ok(Inspected {
        kind,
        dangling: false,
        canonical: Some(canonical),
        id,
    })
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

                // Every edit is checked, not just up to the first bad one.
                // A caller that numbered its edits against the file as it
                // would be *after* the earlier ones is wrong by the same
                // offset in each of them, and one rejection cannot show that:
                // the reply would name a single line whose text looks
                // unrelated, which reads as "I misread that line" rather than
                // "I used the wrong basis". Reporting them together makes the
                // constant shift visible, and a caller can correct the whole
                // call in one turn instead of re-reading the file.
                let mut failures: Vec<EditFailure> = Vec::new();

                for (i, entry) in edits_tbl.sequence_values::<LuaTable>().enumerate() {
                    let entry = entry?;
                    let start_line: usize = entry.get("start_line")?;
                    let end_line: usize = entry.get("end_line")?;
                    let expect: String = entry.get("expect")?;
                    let replace: String = entry.get("replace")?;

                    if start_line == 0 || end_line < start_line {
                        failures.push(EditFailure {
                            index: i + 1,
                            reason: "bad_range",
                            start_line,
                            end_line,
                            actual: None,
                            file_lines: None,
                        });
                        continue;
                    }
                    if end_line > lines.len() {
                        failures.push(EditFailure {
                            index: i + 1,
                            reason: "out_of_range",
                            start_line,
                            end_line,
                            actual: None,
                            file_lines: Some(lines.len()),
                        });
                        continue;
                    }

                    let actual = lines[start_line - 1..end_line].join("\n");
                    if actual != expect {
                        failures.push(EditFailure {
                            index: i + 1,
                            reason: "expect_mismatch",
                            start_line,
                            end_line,
                            // The text actually there, so the caller can
                            // correct without re-reading the file.
                            actual: Some(actual),
                            file_lines: None,
                        });
                        continue;
                    }

                    edits.push(Edit {
                        index: i + 1,
                        start_line,
                        end_line,
                        replace,
                    });
                }

                // The first failure is the reply, unchanged in shape from when
                // this returned on the spot; `failures` carries every one of
                // them, the first included, so a reader that only knows the
                // old fields still works.
                if let Some(first) = failures.first() {
                    let t = failure(&lua, first.reason)?;
                    t.set("edit_index", first.index)?;
                    match first.reason {
                        "out_of_range" => {
                            t.set("end_line", first.end_line)?;
                            t.set("file_lines", first.file_lines.unwrap_or(lines.len()))?;
                        }
                        _ => {
                            t.set("start_line", first.start_line)?;
                            t.set("end_line", first.end_line)?;
                        }
                    }
                    if let Some(actual) = first.actual.as_deref() {
                        t.set("actual", actual)?;
                    }
                    let all = lua.create_table()?;
                    for (n, f) in failures.iter().enumerate() {
                        all.set(n + 1, f.to_table(&lua)?)?;
                    }
                    t.set("failures", all)?;
                    return Ok(t);
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

    // ── inspect ───────────────────────────────────────────────────
    fs_tbl.set(
        "inspect",
        lua.create_async_function(|lua: Lua, path: String| async move {
            let shown = path.clone();
            let got = blocking("fs.inspect", move || {
                inspect_path(Path::new(&path))
                    .map_err(|e| format!("fs.inspect: cannot inspect {shown}: {e}"))
            })
            .await?
            .map_err(LuaError::external)?;
            let t = lua.create_table()?;
            t.set("kind", got.kind.as_str())?;
            if got.kind == Kind::Symlink {
                t.set("dangling", got.dangling)?;
            }
            if let Some(canonical) = got.canonical.as_deref() {
                t.set("canonical", canonical.to_string_lossy().as_ref())?;
            }
            if let Some((dev, ino)) = got.id {
                // Bit for bit: equality is all these are compared for.
                t.set("dev", dev as i64)?;
                t.set("ino", ino as i64)?;
            }
            Ok(t)
        })?,
    )?;

    // The Lua half — the `fs_tools` library, whose exported functions are
    // installed onto the `std.fs` built above as `tool_specs` /
    // `register_tools`. Through `require`, so a vendored `fs_tools` wins.
    // After `tool::register`, because `register_tools` calls the `tool`
    // global.
    super::load_tools_module(
        lua,
        "fs_tools",
        "fs",
        htl::include_tl!("blocks/lib/fs_tools/init.tl"),
    )?;

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

    // -- inspect ------------------------------------------------------------

    /// A fresh directory, canonicalized so the expected `canonical` values
    /// are comparable on a host whose temp dir sits behind a symlink.
    fn scratch() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path().canonicalize().expect("canonicalize the tempdir");
        (dir, base)
    }

    #[cfg(unix)]
    fn id_of(p: &Path) -> (u64, u64) {
        use std::os::unix::fs::MetadataExt;
        let m = std::fs::metadata(p).expect("metadata");
        (m.dev(), m.ino())
    }

    /// A regular file and a directory answer their kind, their canonical
    /// path and (on Unix) their own `(dev, ino)`.
    #[test]
    fn inspect_reports_a_file_and_a_directory_as_themselves() {
        let (_keep, base) = scratch();
        let file = base.join("a.txt");
        std::fs::write(&file, "x").expect("write");
        let got = inspect_path(&file).expect("inspect the file");
        assert_eq!(got.kind, Kind::File);
        assert_eq!(got.canonical.as_deref(), Some(file.as_path()));
        let got = inspect_path(&base).expect("inspect the directory");
        assert_eq!(got.kind, Kind::Dir);
        assert_eq!(got.canonical.as_deref(), Some(base.as_path()));
        #[cfg(unix)]
        {
            assert_eq!(inspect_path(&file).unwrap().id, Some(id_of(&file)));
            assert_eq!(got.id, Some(id_of(&base)));
        }
        #[cfg(not(unix))]
        assert_eq!(got.id, None, "no identity off Unix");
    }

    /// Nothing at the path is an answer, not an error: `missing`, with no
    /// canonical path and no identity.
    #[test]
    fn inspect_reports_nothing_there_as_missing() {
        let (_keep, base) = scratch();
        let got = inspect_path(&base.join("nope")).expect("not-found is an answer");
        assert_eq!(got.kind, Kind::Missing);
        assert!(got.canonical.is_none() && got.id.is_none());
    }

    /// A symlink is reported as one — not followed — and, when it resolves,
    /// carries its target's canonical path and identity.
    #[cfg(unix)]
    #[test]
    fn inspect_reports_a_resolving_symlink_as_a_symlink_with_its_targets_identity() {
        let (_keep, base) = scratch();
        let target = base.join("real");
        std::fs::create_dir(&target).expect("mkdir");
        let link = base.join("link");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");
        let got = inspect_path(&link).expect("inspect the link");
        assert_eq!(got.kind, Kind::Symlink);
        assert!(!got.dangling);
        assert_eq!(got.canonical.as_deref(), Some(target.as_path()));
        assert_eq!(got.id, Some(id_of(&target)));
    }

    /// A symlink to nothing — directly, or through another link — is a
    /// dangling symlink, not `missing` and not an error: a write to the path
    /// would follow it.
    #[cfg(unix)]
    #[test]
    fn inspect_reports_a_dangling_symlink_as_dangling() {
        let (_keep, base) = scratch();
        let link = base.join("dangling");
        std::os::unix::fs::symlink(base.join("not-there"), &link).expect("symlink");
        let got = inspect_path(&link).expect("a dangling link is an answer");
        assert_eq!(got.kind, Kind::Symlink);
        assert!(got.dangling);
        assert!(got.canonical.is_none() && got.id.is_none());

        let chained = base.join("chained");
        std::os::unix::fs::symlink(&link, &chained).expect("symlink to the dangling link");
        let got = inspect_path(&chained).expect("a chain to nothing is an answer");
        assert_eq!(got.kind, Kind::Symlink);
        assert!(got.dangling);
    }

    /// A FIFO is neither a file nor a directory.
    #[cfg(target_os = "linux")]
    #[test]
    fn inspect_reports_a_fifo_as_other() {
        use std::os::unix::ffi::OsStrExt;
        let (_keep, base) = scratch();
        let fifo = base.join("fifo");
        let c = std::ffi::CString::new(fifo.as_os_str().as_bytes()).expect("no interior NUL");
        // SAFETY: `c` is a valid NUL-terminated path inside a fresh tempdir.
        let rc = unsafe { libc::mkfifo(c.as_ptr(), 0o600) };
        assert_eq!(rc, 0, "mkfifo: {}", std::io::Error::last_os_error());
        assert_eq!(inspect_path(&fifo).expect("inspect").kind, Kind::Other);
    }

    /// Two names for one object answer two canonical strings and one
    /// `(dev, ino)`. A hard link is the case an unprivileged test can build;
    /// a bind mount (root) and a casefold directory (a file system created
    /// with the feature) are the directory cases this identity is for, and
    /// are not built here.
    #[cfg(unix)]
    #[test]
    fn inspect_gives_two_names_for_one_file_one_identity() {
        let (_keep, base) = scratch();
        let a = base.join("a.txt");
        std::fs::write(&a, "x").expect("write");
        let b = base.join("b.txt");
        std::fs::hard_link(&a, &b).expect("hard link");
        let (ga, gb) = (inspect_path(&a).unwrap(), inspect_path(&b).unwrap());
        assert_ne!(ga.canonical, gb.canonical, "the strings differ");
        assert_eq!(ga.id, gb.id, "the identity does not");
        let other = base.join("c.txt");
        std::fs::write(&other, "x").expect("write");
        assert_ne!(inspect_path(&other).unwrap().id, ga.id);
    }

    /// An error other than not-found is returned, so the bridge raises: a
    /// path under a directory that cannot be searched is not "missing".
    /// Skipped as root, which searches any directory.
    #[cfg(unix)]
    #[test]
    fn inspect_fails_closed_on_an_error_that_is_not_not_found() {
        use std::os::unix::fs::PermissionsExt;
        // SAFETY: geteuid has no preconditions and cannot fail.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let (_keep, base) = scratch();
        let shut = base.join("shut");
        std::fs::create_dir(&shut).expect("mkdir");
        std::fs::write(shut.join("x"), "x").expect("write");
        std::fs::set_permissions(&shut, std::fs::Permissions::from_mode(0o000)).expect("chmod");
        let got = inspect_path(&shut.join("x"));
        std::fs::set_permissions(&shut, std::fs::Permissions::from_mode(0o700))
            .expect("chmod back");
        let err = got.expect_err("a permission error is not an answer");
        assert_eq!(err.kind(), ErrorKind::PermissionDenied);
    }

    /// The Lua face: the table's fields, `dangling` only on a symlink, and a
    /// raise for an error that is not not-found.
    #[cfg(unix)]
    #[test]
    fn std_fs_inspect_answers_the_table_and_raises_on_error() {
        use std::os::unix::fs::PermissionsExt;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime");
        let (_keep, base) = scratch();
        let file = base.join("f.txt");
        std::fs::write(&file, "x").expect("write");
        std::os::unix::fs::symlink(base.join("none"), base.join("dl")).expect("symlink");
        let lua = Lua::new();
        let std_tbl = lua.create_table().expect("std table");
        std_tbl
            .set("fs", lua.create_table().expect("fs table"))
            .expect("set std.fs");
        lua.globals().set("std", std_tbl).expect("set std");
        register(&lua, SnapshotStore::default()).expect("register");
        lua.globals()
            .set("BASE", base.to_string_lossy().as_ref())
            .expect("set BASE");
        let (dev, ino) = id_of(&file);
        lua.globals().set("DEV", dev as i64).expect("set DEV");
        lua.globals().set("INO", ino as i64).expect("set INO");
        rt.block_on(
            lua.load(
                r#"
                local f = std.fs.inspect(BASE .. "/f.txt")
                assert(f.kind == "file", f.kind)
                assert(f.canonical == BASE .. "/f.txt", tostring(f.canonical))
                assert(f.dangling == nil, "dangling only on a symlink")
                assert(f.dev == DEV and f.ino == INO, "dev/ino of the file")
                local d = std.fs.inspect(BASE .. "/dl")
                assert(d.kind == "symlink" and d.dangling == true, d.kind)
                assert(d.canonical == nil and d.dev == nil and d.ino == nil)
                local m = std.fs.inspect(BASE .. "/none")
                assert(m.kind == "missing" and m.canonical == nil and m.dangling == nil)
                assert(std.fs.inspect(BASE).kind == "dir")
            "#,
            )
            .exec_async(),
        )
        .expect("the Lua assertions");

        // SAFETY: geteuid has no preconditions and cannot fail.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let shut = base.join("shut");
        std::fs::create_dir(&shut).expect("mkdir");
        std::fs::set_permissions(&shut, std::fs::Permissions::from_mode(0o000)).expect("chmod");
        let raised = rt.block_on(
            lua.load(r#"local ok, err = pcall(std.fs.inspect, BASE .. "/shut/x"); return ok, tostring(err)"#)
                .eval_async::<(bool, String)>(),
        );
        std::fs::set_permissions(&shut, std::fs::Permissions::from_mode(0o700))
            .expect("chmod back");
        let (ok, msg) = raised.expect("pcall returns");
        assert!(!ok, "raised");
        assert!(msg.contains("fs.inspect: cannot inspect"), "{msg}");
    }
}
