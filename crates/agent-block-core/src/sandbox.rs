//! Sandbox — a generic, process-wide execution boundary (Linux only).
//!
//! This is **not** a policy engine. There are no per-bridge rules, no domain
//! allowlists and no dynamic decisions: the sandbox installs one coarse OS-level
//! boundary around the whole process, once, at startup. Everything the process
//! does afterwards — `sh.exec`, `mcp.connect` child processes, Lua `io.*`/`os.*`,
//! the HTTP client — runs inside that boundary because Landlock rulesets and
//! seccomp filters are inherited across `fork(2)` / `execve(2)`.
//!
//! # What is enforced
//!
//! | Axis            | Behaviour |
//! |-----------------|-----------|
//! | read / execute  | unrestricted (`/` is granted `ReadFile`/`ReadDir`/`Execute`) |
//! | write           | denied except for an explicit allowlist (see below) |
//! | TCP             | unrestricted by default; `tcp = false` denies bind+connect |
//! | io_uring        | `io_uring_setup` / `io_uring_enter` / `io_uring_register` fail with `EPERM` |
//!
//! Reads and executes are deliberately left open so that PATH lookups, shared
//! library loading and ordinary tooling keep working — the boundary is about
//! *mutation*, not secrecy.
//!
//! The write allowlist is:
//!
//! - the project root (`--project`),
//! - the agent-block state dir (`AGENT_BLOCK_HOME`, default `$HOME/.agent-block`),
//! - `/tmp`,
//! - `/dev/null`, `/dev/urandom`, `/dev/tty`,
//! - every path listed in `AGENT_BLOCK_SANDBOX_FS_RW` (`:`-separated).
//!
//! Entries that do not exist are skipped rather than treated as an error, so a
//! shared config can list paths that are only present on some machines.
//!
//! io_uring is blocked because it lets a task submit file and socket operations
//! through a shared ring, bypassing the syscall-level view a seccomp filter has.
//! Landlock still covers ring-submitted filesystem operations, but denying the
//! setup syscall keeps the boundary easy to reason about.
//!
//! # Failure model
//!
//! Fail-closed: when the sandbox is requested but the kernel enforces *nothing*
//! (no Landlock support), [`apply`] returns an error and the caller is expected
//! to abort startup. A partial enforcement of the *default* rights (older
//! Landlock ABI, e.g. no `Truncate`) logs a `warn!` describing what was dropped
//! and continues — the filesystem boundary is still real in that case. Two
//! things are never downgraded to a warning: an unresolvable project root (the
//! primary write grant) and an explicitly requested TCP denial on a kernel
//! whose Landlock ABI predates network rights — both abort startup.
//!
//! # KNOWN LIMITATIONS
//!
//! - **Linux only.** On other platforms [`apply`] returns
//!   [`SandboxError::Unsupported`]; there is no silent no-op.
//! - **UDP and DNS are not restricted.** Landlock's network rights cover TCP
//!   bind/connect only, so `tcp = false` does not stop UDP traffic (including
//!   DNS resolution) or unix-domain sockets.
//! - **io_uring is unusable inside the sandbox**, including for dependencies
//!   that would otherwise opportunistically use it.
//! - **TCP is a single on/off switch.** There is no per-host or per-port
//!   granularity, by design — that would be policy, not a boundary.
//! - **The boundary is process-wide and irreversible.** It cannot be relaxed
//!   later in the process lifetime, and it must be installed before any thread
//!   that needs to be covered is spawned (Landlock's `restrict_self` applies to
//!   the calling thread and its future children).
//! - **The io_uring deny only exists on x86_64 and aarch64.** On other Linux
//!   architectures no seccomp filter is compiled and the deny is skipped with a
//!   `warn!`; the Landlock filesystem boundary still applies there.

use std::path::{Path, PathBuf};

/// Enables the sandbox. Also exposed as the `--sandbox` CLI flag.
const ENV_ENABLED: &str = "AGENT_BLOCK_SANDBOX";
/// `:`-separated list of extra writable paths.
const ENV_FS_RW: &str = "AGENT_BLOCK_SANDBOX_FS_RW";
/// `0` / `false` / `no` / `off` denies TCP; anything else (or unset) allows it.
const ENV_TCP: &str = "AGENT_BLOCK_SANDBOX_TCP";

/// Paths that are always writable when the sandbox is on, on top of the project
/// root and the agent-block state dir.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const ALWAYS_WRITABLE: &[&str] = &["/tmp", "/dev/null", "/dev/urandom", "/dev/tty"];

/// Resolved sandbox knobs.
///
/// Built with [`SandboxConfig::from_env`]; all knobs are ENV-driven (matching
/// the `bridge::config` house style) except `enabled`, which is also reachable
/// through the `--sandbox` CLI flag.
///
/// | ENV var                      | Default | Meaning |
/// |------------------------------|---------|---------|
/// | `AGENT_BLOCK_SANDBOX`        | off     | enable the sandbox |
/// | `AGENT_BLOCK_SANDBOX_FS_RW`  | empty   | `:`-separated extra writable paths |
/// | `AGENT_BLOCK_SANDBOX_TCP`    | `true`  | `0`/`false`/`no`/`off` denies TCP |
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxConfig {
    /// Whether the boundary should be installed at all.
    pub enabled: bool,
    /// Extra paths granted write access, on top of the built-in allowlist.
    pub fs_rw: Vec<PathBuf>,
    /// `true` (default) leaves TCP untouched; `false` denies bind + connect.
    pub tcp: bool,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            fs_rw: Vec::new(),
            tcp: true,
        }
    }
}

impl SandboxConfig {
    /// Resolve the config from the environment.
    ///
    /// `cli_enabled` is the `--sandbox` flag; the sandbox is enabled when
    /// either the flag or a truthy `AGENT_BLOCK_SANDBOX` is present. The env
    /// var is read here rather than through a clap `env = ...` binding: clap
    /// parses before the project `.env` is loaded and would accept only the
    /// literal strings `true`/`false`, while this path supports the documented
    /// truthy/falsy set.
    pub fn from_env(cli_enabled: bool) -> Self {
        Self::from_parts(
            cli_enabled,
            std::env::var(ENV_ENABLED).ok().as_deref(),
            std::env::var(ENV_FS_RW).ok().as_deref(),
            std::env::var(ENV_TCP).ok().as_deref(),
        )
    }

    /// Pure parsing core of [`SandboxConfig::from_env`], split out so it can be
    /// unit-tested without mutating process-wide environment state.
    fn from_parts(
        cli_enabled: bool,
        enabled_raw: Option<&str>,
        fs_rw_raw: Option<&str>,
        tcp_raw: Option<&str>,
    ) -> Self {
        Self {
            enabled: cli_enabled || enabled_raw.is_some_and(is_truthy),
            fs_rw: fs_rw_raw.map(split_paths).unwrap_or_default(),
            // Absent = allow: the sandbox must not break networking unless the
            // operator explicitly asks for it.
            tcp: tcp_raw.is_none_or(is_truthy),
        }
    }
}

/// Errors returned by [`apply`].
#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    /// The sandbox was requested on a platform that has no implementation.
    #[error("sandbox mode is Linux-only (Landlock + seccomp); this build targets '{os}'")]
    Unsupported {
        /// `std::env::consts::OS` of the running build.
        os: &'static str,
    },
    /// The kernel accepted nothing at all — the process would run unrestricted.
    #[error(
        "sandbox requested but Landlock is not enforced by this kernel \
         (needs Linux 5.13+ with CONFIG_SECURITY_LANDLOCK and landlock in the active LSM list)"
    )]
    NotEnforced,
    /// The project root — the primary write grant — could not be resolved.
    #[error("sandbox: project root '{path}' cannot be resolved: {error}")]
    ProjectRoot {
        /// The path as given on the command line.
        path: String,
        /// The underlying `canonicalize` error.
        error: String,
    },
    /// Building or applying the Landlock ruleset failed.
    #[error("failed to install Landlock ruleset: {0}")]
    Landlock(String),
    /// Building or applying the seccomp filter failed.
    #[error("failed to install seccomp filter: {0}")]
    Seccomp(String),
}

/// Install the execution boundary for this process.
///
/// A no-op when `config.enabled` is `false`. Otherwise the boundary is applied
/// to the calling thread and inherited by every thread and child process
/// created afterwards, so this must be called **before** any runtime spawns
/// worker threads.
///
/// # Errors
///
/// Returns [`SandboxError::Unsupported`] on non-Linux targets,
/// [`SandboxError::NotEnforced`] when the kernel enforces nothing, and the
/// `Landlock` / `Seccomp` variants when the respective syscalls fail. Callers
/// are expected to abort startup on any of these (fail-closed).
pub fn apply(config: &SandboxConfig, project_root: &Path) -> Result<(), SandboxError> {
    if !config.enabled {
        return Ok(());
    }
    apply_platform(config, project_root)
}

#[cfg(target_os = "linux")]
fn apply_platform(config: &SandboxConfig, project_root: &Path) -> Result<(), SandboxError> {
    linux::apply(config, project_root)
}

/// No sandbox implementation exists off Linux, and pretending otherwise would be
/// worse than refusing: the caller asked for a boundary, so say it is missing.
#[cfg(not(target_os = "linux"))]
fn apply_platform(_config: &SandboxConfig, _project_root: &Path) -> Result<(), SandboxError> {
    Err(SandboxError::Unsupported {
        os: std::env::consts::OS,
    })
}

/// Resolve the set of directories/files that stay writable under the sandbox.
///
/// Non-existent entries are skipped (a missing path cannot be granted to
/// Landlock, and treating it as fatal would make shared configs brittle).
/// Duplicates — e.g. a project root that is already inside `/tmp` — are removed
/// so the ruleset stays minimal.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn write_allowlist(config: &SandboxConfig, project_root: &Path) -> Vec<PathBuf> {
    // `bool` = "the operator named this path", which decides how loudly a
    // missing entry is reported. `/dev/tty` is routinely absent in containers,
    // so the built-ins stay at debug level.
    let mut candidates: Vec<(PathBuf, bool)> = Vec::new();
    candidates.push((project_root.to_path_buf(), true));
    if let Some(home) = state_home() {
        candidates.push((home, true));
    }
    candidates.extend(ALWAYS_WRITABLE.iter().map(|p| (PathBuf::from(p), false)));
    candidates.extend(config.fs_rw.iter().map(|p| (p.clone(), true)));

    let mut out: Vec<PathBuf> = Vec::with_capacity(candidates.len());
    for (path, explicit) in candidates {
        let resolved = match path.canonicalize() {
            Ok(p) => p,
            Err(err) => {
                // Skipping is the documented behaviour; report it so an operator
                // can tell why a write is denied later on.
                if explicit {
                    tracing::warn!(
                        path = %path.display(),
                        error = %err,
                        "sandbox: write path not granted (unresolvable) — writes there will fail"
                    );
                } else {
                    tracing::debug!(
                        path = %path.display(),
                        error = %err,
                        "sandbox: built-in write path absent, skipped"
                    );
                }
                continue;
            }
        };
        if !out.contains(&resolved) {
            out.push(resolved);
        }
    }
    out
}

/// Base dir for agent-block local state, mirroring `bridge::config::base_dir`
/// without the `sqlite` feature gate (the sandbox has to know about it even in
/// builds where the SQLite bridges are compiled out).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn state_home() -> Option<PathBuf> {
    if let Some(v) = std::env::var_os("AGENT_BLOCK_HOME") {
        return Some(PathBuf::from(v));
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".agent-block"))
}

/// `""` / `0` / `false` / `no` / `off` (any case, surrounding space ignored)
/// are false; every other value is true.
fn is_truthy(raw: &str) -> bool {
    !matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "" | "0" | "false" | "no" | "off"
    )
}

/// Split a `:`-separated path list, dropping empty segments so that a trailing
/// or doubled separator is harmless.
fn split_paths(raw: &str) -> Vec<PathBuf> {
    raw.split(':')
        .filter(|segment| !segment.trim().is_empty())
        .map(PathBuf::from)
        .collect()
}

#[cfg(target_os = "linux")]
mod linux {
    use super::{write_allowlist, SandboxConfig, SandboxError};
    use landlock::{
        path_beneath_rules, Access, AccessFs, AccessNet, CompatLevel, Compatible, Ruleset,
        RulesetAttr, RulesetCreatedAttr, RulesetStatus, ABI,
    };
    use std::path::Path;

    /// Landlock ABI this ruleset is written against. Older kernels downgrade
    /// through `CompatLevel::BestEffort`; newer kernels simply leave the access
    /// rights introduced after V4 unhandled (i.e. unrestricted), which is the
    /// safe direction for "must not break the default workflow".
    const FS_ABI: ABI = ABI::V4;

    /// Landlock ABI that introduced TCP bind/connect rights.
    const NET_ABI: ABI = ABI::V4;

    pub(super) fn apply(config: &SandboxConfig, project_root: &Path) -> Result<(), SandboxError> {
        // The project root is the primary write grant — a typo'd `--project`
        // must fail here, not as a distant EACCES once Lua starts writing.
        // Everything else in the allowlist keeps skip-on-missing semantics.
        let project_root =
            project_root
                .canonicalize()
                .map_err(|err| SandboxError::ProjectRoot {
                    path: project_root.display().to_string(),
                    error: err.to_string(),
                })?;
        let writable = write_allowlist(config, &project_root);
        let granted = writable.len();

        // The landlock crate deliberately exposes no runtime ABI probe: with
        // `CompatLevel::BestEffort` the kernel silently drops rights it does
        // not know, and `restrict_self()`'s `RulesetStatus` reports whether
        // that happened (handled below as the partial-enforcement warning).
        let mut ruleset = Ruleset::default()
            .set_compatibility(CompatLevel::BestEffort)
            .handle_access(AccessFs::from_all(FS_ABI))
            .map_err(landlock_err)?;

        if !config.tcp {
            // TCP denial is an explicit operator request, not a best-effort
            // default: on kernels whose Landlock ABI predates network rights
            // (< 6.7) this must abort startup instead of silently failing open.
            ruleset = ruleset
                .set_compatibility(CompatLevel::HardRequirement)
                .handle_access(AccessNet::from_all(NET_ABI))
                .map_err(landlock_err)?;
        }

        let created = ruleset.create().map_err(landlock_err)?;
        // Reads and executes stay open everywhere.
        let created = created
            .add_rules(path_beneath_rules(["/"], AccessFs::from_read(FS_ABI)))
            .map_err(landlock_err)?;
        // Writes are granted only under the allowlist. No `handle_access` for
        // net rights above means TCP is left untouched when `tcp = true`; when
        // it is handled and no rule is added, every TCP bind/connect is denied.
        //
        // `path_beneath_rules` masks directory-only rights (MakeDir,
        // RemoveFile, …) down to the file-applicable subset for non-directory
        // entries like `/dev/null`, so one rule set covers both kinds.
        let created = created
            .add_rules(path_beneath_rules(&writable, AccessFs::from_all(FS_ABI)))
            .map_err(landlock_err)?;

        let status = created.restrict_self().map_err(landlock_err)?;

        match status.ruleset {
            RulesetStatus::FullyEnforced => {
                tracing::info!(
                    writable = granted,
                    tcp = config.tcp,
                    "sandbox: filesystem boundary fully enforced"
                );
            }
            RulesetStatus::PartiallyEnforced => {
                tracing::warn!(
                    writable = granted,
                    tcp = config.tcp,
                    "sandbox: filesystem boundary only partially enforced — this kernel \
                     dropped some access rights (older Landlock ABI). Writes outside the \
                     allowlist are still denied; newer rights (e.g. file truncation) may \
                     not be. An explicit TCP denial is never dropped: it aborts startup \
                     on kernels that cannot enforce it"
                );
            }
            // `NotEnforced` plus any future variant: treat as no boundary at all.
            _ => return Err(SandboxError::NotEnforced),
        }

        super::seccomp::deny_io_uring()?;
        Ok(())
    }

    fn landlock_err<E: std::fmt::Debug>(err: E) -> SandboxError {
        SandboxError::Landlock(format!("{err:?}"))
    }
}

#[cfg(target_os = "linux")]
mod seccomp {
    use super::SandboxError;

    /// Deny the three io_uring entry points with `EPERM`.
    ///
    /// Everything else is allowed: this filter exists to close the ring-based
    /// bypass around the syscall view, not to enumerate a syscall policy.
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    pub(super) fn deny_io_uring() -> Result<(), SandboxError> {
        use seccompiler::{
            apply_filter_all_threads, BpfProgram, SeccompAction, SeccompFilter, SeccompRule,
            TargetArch,
        };
        use std::collections::BTreeMap;

        #[cfg(target_arch = "x86_64")]
        const ARCH: TargetArch = TargetArch::x86_64;
        #[cfg(target_arch = "aarch64")]
        const ARCH: TargetArch = TargetArch::aarch64;

        // An empty rule vector means "match this syscall unconditionally".
        let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();
        // `SYS_*` constants are `c_long`, which is i64 on 64-bit targets and i32
        // on 32-bit ones — the cast is width-normalising, not redundant.
        #[allow(clippy::unnecessary_cast)]
        for syscall in [
            libc::SYS_io_uring_setup,
            libc::SYS_io_uring_enter,
            libc::SYS_io_uring_register,
        ] {
            rules.insert(syscall as i64, Vec::new());
        }

        let filter = SeccompFilter::new(
            rules,
            // Mismatch (i.e. every other syscall) is allowed.
            SeccompAction::Allow,
            // Match returns EPERM instead of killing the process, so a caller
            // that probes for io_uring can fall back gracefully.
            SeccompAction::Errno(libc::EPERM as u32),
            ARCH,
        )
        .map_err(seccomp_err)?;

        let program: BpfProgram = filter.try_into().map_err(seccomp_err)?;
        apply_filter_all_threads(&program).map_err(seccomp_err)?;

        tracing::info!("sandbox: io_uring syscalls denied (EPERM)");
        Ok(())
    }

    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    pub(super) fn deny_io_uring() -> Result<(), SandboxError> {
        tracing::warn!(
            arch = std::env::consts::ARCH,
            "sandbox: io_uring deny skipped — no seccomp filter is compiled for this \
             architecture; the Landlock filesystem boundary is unaffected"
        );
        Ok(())
    }

    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    fn seccomp_err<E: std::fmt::Debug>(err: E) -> SandboxError {
        SandboxError::Seccomp(format!("{err:?}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // These exercise the pure parsing core rather than `from_env`, so no test
    // mutates process-wide environment state (which would race with the other
    // tests in this crate running on sibling threads).

    #[test]
    fn defaults_are_off_and_network_open() {
        let cfg = SandboxConfig::from_parts(false, None, None, None);
        assert!(!cfg.enabled);
        assert!(cfg.fs_rw.is_empty());
        assert!(cfg.tcp, "TCP must default to unrestricted");
        assert_eq!(cfg, SandboxConfig::default());
    }

    #[test]
    fn cli_flag_enables_without_env() {
        let cfg = SandboxConfig::from_parts(true, None, None, None);
        assert!(cfg.enabled);
    }

    #[test]
    fn env_enables_without_cli_flag() {
        assert!(SandboxConfig::from_parts(false, Some("1"), None, None).enabled);
        assert!(SandboxConfig::from_parts(false, Some("true"), None, None).enabled);
        // An explicitly falsy env var does not enable it...
        assert!(!SandboxConfig::from_parts(false, Some("0"), None, None).enabled);
        assert!(!SandboxConfig::from_parts(false, Some(""), None, None).enabled);
        // ...but it never disables an explicit CLI flag.
        assert!(SandboxConfig::from_parts(true, Some("0"), None, None).enabled);
    }

    #[test]
    fn fs_rw_splits_on_colon() {
        let cfg = SandboxConfig::from_parts(true, None, Some("/opt/cache:/srv/data"), None);
        assert_eq!(
            cfg.fs_rw,
            vec![PathBuf::from("/opt/cache"), PathBuf::from("/srv/data")]
        );
    }

    #[test]
    fn fs_rw_drops_empty_segments() {
        let cfg = SandboxConfig::from_parts(true, None, Some(":/a::/b: :"), None);
        assert_eq!(cfg.fs_rw, vec![PathBuf::from("/a"), PathBuf::from("/b")]);
    }

    #[test]
    fn fs_rw_empty_string_yields_no_paths() {
        let cfg = SandboxConfig::from_parts(true, None, Some(""), None);
        assert!(cfg.fs_rw.is_empty());
    }

    #[test]
    fn fs_rw_single_path_has_no_separator() {
        let cfg = SandboxConfig::from_parts(true, None, Some("/opt/cache"), None);
        assert_eq!(cfg.fs_rw, vec![PathBuf::from("/opt/cache")]);
    }

    #[test]
    fn tcp_falsy_values_deny() {
        for raw in ["0", "false", "FALSE", " False ", "no", "off", ""] {
            let cfg = SandboxConfig::from_parts(true, None, None, Some(raw));
            assert!(!cfg.tcp, "expected {raw:?} to deny TCP");
        }
    }

    #[test]
    fn tcp_truthy_values_allow() {
        for raw in ["1", "true", "TRUE", "yes", "on", "anything"] {
            let cfg = SandboxConfig::from_parts(true, None, None, Some(raw));
            assert!(cfg.tcp, "expected {raw:?} to allow TCP");
        }
    }

    #[test]
    fn write_allowlist_keeps_existing_and_drops_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("does-not-exist");
        let cfg = SandboxConfig {
            enabled: true,
            fs_rw: vec![missing.clone()],
            tcp: true,
        };

        let allowed = write_allowlist(&cfg, dir.path());

        let project = dir
            .path()
            .canonicalize()
            .expect("canonicalize project root");
        assert!(
            allowed.contains(&project),
            "project root must stay writable"
        );
        assert!(
            !allowed.iter().any(|p| p.ends_with("does-not-exist")),
            "missing paths are skipped, not fatal"
        );
    }

    #[test]
    fn write_allowlist_deduplicates() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = SandboxConfig {
            enabled: true,
            // Same dir listed twice, once via an un-normalised path.
            fs_rw: vec![dir.path().to_path_buf(), dir.path().join(".")],
            tcp: true,
        };

        let allowed = write_allowlist(&cfg, dir.path());
        let project = dir
            .path()
            .canonicalize()
            .expect("canonicalize project root");

        assert_eq!(
            allowed.iter().filter(|p| **p == project).count(),
            1,
            "duplicate entries must collapse to a single rule"
        );
    }
}
