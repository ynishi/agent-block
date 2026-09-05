//! The block registry — what `agent-block --block <name>` and `agent-block mcp`
//! resolve a name against.
//!
//! A block is an entry point: a Lua script the host runs to completion for the
//! one value it returns. The registry is the set of `<name>.lua` files and
//! `<name>/init.lua` directories directly under the block roots, which are
//! `project_root/blocks/` and `$AGENT_BLOCK_HOME/blocks/` (see
//! [`agent_block_core::host::block_roots`]) plus whatever `--block-dir` adds.
//! The file stem, or the directory name, is the block name.
//!
//! What is deliberately not here: `require`. Block roots are never on the
//! module path, and `lib/` roots are never scanned for blocks. A helper
//! dropped beside a block stays a helper only if it lives in `lib/`; dropped
//! in `blocks/` it becomes a callable block, which is the one thing the split
//! exists to make impossible to do by accident.
//!
//! The scan runs per lookup rather than once at start so that a file landing
//! in a block directory is callable at once — for the MCP server that means
//! without a restart, for the CLI it is simply how a filesystem works.

use std::path::{Path, PathBuf};

/// One callable block.
#[derive(Debug, Clone)]
pub struct Block {
    /// The name a caller passes: the file stem of `<name>.lua`, or the
    /// directory name of `<name>/init.lua`.
    pub name: String,
    /// The script the host runs.
    pub path: PathBuf,
    /// The script's leading `--` comment lines — the author's own description,
    /// and the only thing a caller sees before choosing to run it. Empty when
    /// the file opens with code.
    pub doc: String,
}

/// The directories to scan, highest priority first: the tiers from
/// [`agent_block_core::host::block_roots`] (only the ones that exist) followed
/// by `extra` in the order given. `extra` is not filtered — a caller that
/// named a directory wants to hear if it is missing, and `scan` logs that.
pub fn dirs(project_root: &Path, extra: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = agent_block_core::host::block_roots(project_root);
    out.extend(extra.iter().cloned());
    out
}

/// Scan `dirs` for blocks, sorted by name.
///
/// Later directories do not shadow earlier ones: a duplicate name is skipped
/// and logged, because silently preferring one of two files that share a name
/// is the kind of thing that is only noticed once it has already run the wrong
/// one. Within a directory, `<name>.lua` and `<name>/init.lua` naming the same
/// block is the same conflict and gets the same treatment.
pub fn scan(dirs: &[PathBuf]) -> Vec<Block> {
    let mut blocks: Vec<Block> = Vec::new();

    for dir in dirs {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(e) => {
                tracing::warn!(dir = %dir.display(), error = %e, "block dir unreadable; skipped");
                continue;
            }
        };

        // Deterministic within a directory so that the duplicate rule picks
        // the same winner on every scan; `read_dir` order is not promised.
        let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        paths.sort();

        for path in paths {
            let Some((name, script)) = entry_point(&path) else {
                continue;
            };

            if let Some(existing) = blocks.iter().find(|b| b.name == name) {
                tracing::warn!(
                    block = %name,
                    kept = %existing.path.display(),
                    skipped = %script.display(),
                    "duplicate block name; later entry ignored"
                );
                continue;
            }

            let doc = std::fs::read_to_string(&script)
                .map(|src| leading_doc(&src))
                .unwrap_or_default();

            blocks.push(Block {
                name,
                path: script,
                doc,
            });
        }
    }

    blocks.sort_by(|a, b| a.name.cmp(&b.name));
    blocks
}

/// The block a directory entry is, if it is one: `<name>.lua` → that file,
/// `<name>/init.lua` → that file, anything else → nothing.
fn entry_point(path: &Path) -> Option<(String, PathBuf)> {
    if path.is_file() {
        if path.extension().and_then(|s| s.to_str()) != Some("lua") {
            return None;
        }
        let name = path.file_stem()?.to_str()?.to_string();
        return Some((name, path.to_path_buf()));
    }
    if path.is_dir() {
        let init = path.join("init.lua");
        if !init.is_file() {
            return None;
        }
        let name = path.file_name()?.to_str()?.to_string();
        return Some((name, init));
    }
    None
}

/// The block named `name`, if registered.
pub fn find<'a>(blocks: &'a [Block], name: &str) -> Option<&'a Block> {
    blocks.iter().find(|b| b.name == name)
}

/// The registered names, comma-joined, for an error message that says what
/// the caller could have asked for.
pub fn names(blocks: &[Block]) -> String {
    blocks
        .iter()
        .map(|b| b.name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// The leading `--` comment block of a Lua source, with the markers stripped.
///
/// This is lifted into the tool description and the CLI listing rather than
/// asking authors to repeat themselves in a manifest.
pub fn leading_doc(src: &str) -> String {
    let mut lines: Vec<&str> = Vec::new();
    for line in src.lines() {
        let trimmed = line.trim_start();
        if trimmed.is_empty() && lines.is_empty() {
            continue;
        }
        let Some(rest) = trimmed.strip_prefix("--") else {
            break;
        };
        lines.push(rest.trim_start_matches('-').trim());
    }
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leading_doc_takes_the_header_comment_and_stops_at_code() {
        let src = "-- Summarize a document.\n--\n-- Returns { ok, text }.\nlocal agent = require(\"agent\")\n-- not part of the header\n";
        assert_eq!(
            leading_doc(src),
            "Summarize a document.\n\nReturns { ok, text }."
        );
    }

    #[test]
    fn leading_doc_is_empty_when_the_file_opens_with_code() {
        assert_eq!(leading_doc("local x = 1\n-- later\n"), "");
    }

    #[test]
    fn scan_finds_lua_files_and_ignores_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("b.lua"), "-- second\nreturn \"\"").unwrap();
        std::fs::write(dir.path().join("a.lua"), "-- first\nreturn \"\"").unwrap();
        std::fs::write(dir.path().join("notes.md"), "not a block").unwrap();

        let blocks = scan(&[dir.path().to_path_buf()]);

        let names: Vec<&str> = blocks.iter().map(|b| b.name.as_str()).collect();
        assert_eq!(names, ["a", "b"]);
        assert_eq!(blocks[0].doc, "first");
    }

    /// A block may be a directory: the form the CLI has always run
    /// (`-s blocks/<name>/init.lua`) is registered under the same name, so a
    /// block does not change shape to become callable from MCP.
    #[test]
    fn scan_registers_a_directory_with_an_init_lua_under_the_directory_name() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("summarize")).unwrap();
        std::fs::write(
            dir.path().join("summarize/init.lua"),
            "-- Summarize.\nreturn \"\"",
        )
        .unwrap();
        // A directory without `init.lua` is not a block, nor is a module file
        // inside a block directory — only the entry point is registered.
        std::fs::create_dir(dir.path().join("notablock")).unwrap();
        std::fs::write(dir.path().join("summarize/helper.lua"), "return {}").unwrap();

        let blocks = scan(&[dir.path().to_path_buf()]);

        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].name, "summarize");
        assert_eq!(blocks[0].doc, "Summarize.");
        assert!(blocks[0].path.ends_with("summarize/init.lua"));
    }

    #[test]
    fn a_duplicate_name_keeps_the_first_directory() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        std::fs::write(first.path().join("dup.lua"), "-- kept\nreturn \"\"").unwrap();
        std::fs::write(second.path().join("dup.lua"), "-- shadowed\nreturn \"\"").unwrap();

        let blocks = scan(&[first.path().to_path_buf(), second.path().to_path_buf()]);

        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].doc, "kept");
    }

    /// `dirs` puts the project tier first and the user tier second, and
    /// leaves out a tier whose directory does not exist — so a project with no
    /// `blocks/` still sees the user's blocks, and a project that has one
    /// shadows them by name.
    #[test]
    fn dirs_orders_project_before_user_before_extra_and_skips_missing_tiers() {
        let project = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let extra = tempfile::tempdir().unwrap();
        std::fs::create_dir(home.path().join("blocks")).unwrap();

        // Serialised through the env var: `block_roots` reads
        // `AGENT_BLOCK_HOME` at call time.
        let _guard = EnvGuard::set("AGENT_BLOCK_HOME", home.path());

        let without_project = dirs(project.path(), &[extra.path().to_path_buf()]);
        assert_eq!(
            without_project,
            [home.path().join("blocks"), extra.path().to_path_buf()]
        );

        std::fs::create_dir(project.path().join("blocks")).unwrap();
        let with_project = dirs(project.path(), &[]);
        assert_eq!(
            with_project,
            [project.path().join("blocks"), home.path().join("blocks")]
        );
    }

    /// Sets an env var for the test's lifetime and restores the prior value.
    /// Tests that touch `AGENT_BLOCK_HOME` share one process, so they hold
    /// `ENV_LOCK` for the duration.
    struct EnvGuard {
        key: &'static str,
        prior: Option<std::ffi::OsString>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    impl EnvGuard {
        fn set(key: &'static str, value: &Path) -> Self {
            let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let prior = std::env::var_os(key);
            std::env::set_var(key, value);
            Self {
                key,
                prior,
                _lock: lock,
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.prior {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }
}
