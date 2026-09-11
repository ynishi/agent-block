//! `agent-block vendor` — write an embedded module into the project.
//!
//! Changing an embedded module has always been possible: drop a file of the
//! same name in a `lib/` tier and it is what `require` resolves. What was
//! missing is the first step — getting the original out of the binary, which
//! meant finding the crate's source tree and copying by hand, and a copy made
//! that way records nothing about where it came from. This subcommand is that
//! step: it expands an entry into the project's own `.agent-block/` directory,
//! which is the first tier both lookups search
//! ([`agent_block_core::host::PROJECT_DIR`]), and puts a header on the copy
//! saying what it now answers for.
//!
//! ```text
//! agent-block vendor [--path <dir>] [--force] <name>...
//! agent-block vendor --list
//! ```
//!
//! Three refusals, and each of them is the same point made about a different
//! name. A **sealed** module is the kernel and its declaration layer; a project
//! cannot shadow it at all, so handing it a copy would be handing it a file
//! that fails the next run. A **sub-module** (`lshape.t`) is not a unit: a
//! module vendors whole, because half of one on disk and half in memory is two
//! versions of the same module wearing one name. An **existing file** is
//! whatever the project has already edited, and overwriting it without being
//! told to would throw that away — `--force` is that telling.
//!
//! A **pack** (`policy`, `supervisor`) vendors, with a warning: it is a value a
//! script hands to `knl.device`, not a registry the host reads, so a whole copy
//! is rarely what its author wanted. That one is a warning and not a refusal
//! because the copy is still a legal thing to have — it just is not the way to
//! make a small change.
//!
//! Nothing here keeps a copy in step with the binary it came from. That is the
//! deal the header states plainly: a vendored module is the project's from the
//! moment it is written, and an upgrade of agent-block moves the embedded one
//! underneath it without touching it.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context as _};
use clap::Args;

use agent_block_core::embedded::{self, Entry};
use agent_block_core::host::PROJECT_DIR;

/// The two shell packs: named here because the warning is about what they are,
/// which is a fact of this crate's own layout rather than of the embedded list
/// (see the README's "Embedded blocks: four layers").
const PACKS: &[&str] = &["policy", "supervisor"];

/// `agent-block vendor` arguments.
#[derive(Debug, Args)]
pub struct VendorArgs {
    /// What to write out: an embedded module (`agent`, `session`, …).
    /// Repeatable. A module is written whole, sub-modules included.
    #[arg(value_name = "NAME", required_unless_present = "list")]
    pub names: Vec<String>,

    /// Where to write it. Defaults to the project root (`-p/--project`); the
    /// copy lands under `<dir>/.agent-block/`, not directly in `<dir>`.
    #[arg(long, value_name = "DIR")]
    pub path: Option<PathBuf>,

    /// Overwrite a copy that is already there.
    ///
    /// Without it an existing file is a refusal: the copy is the project's own
    /// by then, and it is likely to have been edited — which is the whole point
    /// of having made it.
    #[arg(long)]
    pub force: bool,

    /// List what can be vendored instead of vendoring: name, whether it is
    /// sealed or a pack, and whether this project already has a copy.
    #[arg(long, conflicts_with_all = ["names", "force"])]
    pub list: bool,
}

/// One file a vendor writes.
#[derive(Debug)]
struct Vendored {
    /// Where it goes, relative to `<dir>/.agent-block/`.
    rel: PathBuf,
    /// The `require` name this file answers for.
    name: String,
    /// The embedded source it is a copy of.
    source: &'static str,
}

/// What one name expands to: the files, and anything the caller should hear
/// before they are written.
#[derive(Debug)]
struct Plan {
    files: Vec<Vendored>,
    warning: Option<String>,
}

/// Run `agent-block vendor`.
pub fn run(args: VendorArgs, project: &Path) -> anyhow::Result<()> {
    let dir = args.path.clone().unwrap_or_else(|| project.to_path_buf());

    if args.list {
        print!("{}", listing(&dir));
        return Ok(());
    }

    // Every name is resolved before anything is written, and every target is
    // checked before the first one lands: a run that would refuse its second
    // name should not leave the first one on disk, half-vendoring a set the
    // caller asked for as a set.
    let mut files: Vec<Vendored> = Vec::new();
    for name in &args.names {
        let plan = resolve(name)?;
        if let Some(warning) = plan.warning {
            eprintln!("{warning}");
        }
        files.extend(plan.files);
    }

    let root = dir.join(PROJECT_DIR);
    if !args.force {
        for file in &files {
            let target = root.join(&file.rel);
            if target.exists() {
                bail!(
                    "'{}' is already there. That copy is this project's own and may well have \
                     been edited; `--force` overwrites it.",
                    target.display()
                );
            }
        }
    }

    for file in &files {
        let target = root.join(&file.rel);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating '{}'", parent.display()))?;
        }
        let body = render(&file.name, file.source);
        std::fs::write(&target, body).with_context(|| format!("writing '{}'", target.display()))?;
        println!("{}", target.display());
    }

    Ok(())
}

/// What `name` vendors to, or why it does not.
fn resolve(name: &str) -> anyhow::Result<Plan> {
    if embedded::is_sealed(name) {
        bail!(
            "`{name}` is sealed: a project cannot shadow it (the kernel is the contract every \
             block is written against); read it with require(\"embedded.{name}\")"
        );
    }

    if let Some((root, _)) = name.split_once('.') {
        bail!(
            "`{name}` is part of `{root}`, and a module vendors whole: run `agent-block vendor \
             {root}`, which writes `{root}` and every sub-module it has. Half a module on disk \
             and half in memory is two versions of it under one name."
        );
    }

    let Some(entry) = embedded::find(name) else {
        bail!(
            "`{name}` is not an embedded module. There is: {}. \
             `agent-block vendor --list` says what each one is.",
            roots().join(", ")
        );
    };

    // Every embedded entry vendors as a module, `agent` and `coding` included:
    // they are `require`d by the scripts that use them, so the copy has to land
    // where `require` looks. `.agent-block/blocks/` is for a project's own entry
    // points and `vendor` never writes there — a copy put there would be a
    // second name for the same code that `require` cannot see.
    let mut files = vec![Vendored {
        rel: module_path(name),
        name: name.to_string(),
        source: entry.source,
    }];
    for sub in subs_of(name) {
        let tail = sub.name.trim_start_matches(name).trim_start_matches('.');
        files.push(Vendored {
            rel: PathBuf::from("lib")
                .join(name)
                .join(format!("{}.lua", tail.replace('.', "/"))),
            name: sub.name.to_string(),
            source: sub.source,
        });
    }

    let warning = PACKS.contains(&name).then(|| {
        format!(
            "warning: `{name}` is a pack — a value you hand to `knl.device` or consult in your \
             own loop, not a registry the host reads — so a whole copy of it is rarely the \
             change you meant; for a partial one, delegate through require(\"embedded.{name}\"). \
             Writing it anyway."
        )
    });

    Ok(Plan { files, warning })
}

/// Where a root's own file lands under `.agent-block/` — what `--list` looks for
/// to say `vendored`, and the first file [`resolve`] plans.
fn module_path(name: &str) -> PathBuf {
    PathBuf::from("lib").join(name).join("init.lua")
}

/// The embedded entries that are sub-modules of `name`, in the order the binary
/// lists them.
fn subs_of(name: &str) -> Vec<&'static Entry> {
    let prefix = format!("{name}.");
    embedded::entries()
        .iter()
        .filter(|e| e.name.starts_with(&prefix))
        .collect()
}

/// The names a caller may ask for: the roots, sub-modules folded away.
fn roots() -> Vec<&'static str> {
    embedded::entries()
        .iter()
        .map(|e| e.name)
        .filter(|name| !name.contains('.'))
        .collect()
}

/// The copy: a header saying what it is, then the source verbatim.
///
/// The header is three lines because a copy raises three questions later, and
/// the file is the only place the answers survive: where it came from (and from
/// which version — the embedded one has moved since), what name it now answers
/// to, and how to reach the original it replaced.
fn render(name: &str, source: &str) -> String {
    format!("{}\n{source}", header(name))
}

/// The header lines, without the trailing blank one.
fn header(name: &str) -> String {
    format!(
        "-- vendored from agent-block {version} (embedded {name})\n\
         -- This copy is what require(\"{name}\") resolves to in this project;\n\
         -- the original stays reachable as require(\"embedded.{name}\"). Edit freely; nothing keeps it in step with upstream.\n",
        version = env!("CARGO_PKG_VERSION"),
    )
}

/// What every row of `--list` says in its second column.
///
/// One word and always the same one: everything the binary carries is a module
/// as far as a project is concerned, because that is how it is reached. The
/// column stays because the line reads as a table and the next thing this list
/// grows may not be a module.
const KIND: &str = "lib";

/// One line of `--list`.
struct Row {
    /// The root's name, with its sub-modules in brackets.
    label: String,
    /// `sealed`, `pack`, or nothing.
    tag: &'static str,
    /// Whether this project already has a copy.
    vendored: bool,
}

/// `--list`, rendered: every embedded root, one line each.
fn listing(dir: &Path) -> String {
    let rows = rows(dir);
    let width = rows.iter().map(|r| r.label.len()).max().unwrap_or(0);
    let mut out = String::new();
    for row in rows {
        let mut line = format!("{:<width$}  {KIND:<5}", row.label, width = width);
        for extra in [row.tag, if row.vendored { "vendored" } else { "" }] {
            if !extra.is_empty() {
                line.push_str("  ");
                line.push_str(extra);
            }
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// The rows `--list` prints, sub-modules folded under the root they belong to.
fn rows(dir: &Path) -> Vec<Row> {
    let root_dir = dir.join(PROJECT_DIR);
    embedded::entries()
        .iter()
        .filter(|e| !e.name.contains('.'))
        .map(|e| {
            let subs = subs_of(e.name);
            let label = if subs.is_empty() {
                e.name.to_string()
            } else {
                let tails: Vec<&str> = subs
                    .iter()
                    .map(|s| s.name.trim_start_matches(e.name).trim_start_matches('.'))
                    .collect();
                format!("{} (+{})", e.name, tails.join(", "))
            };
            let tag = if embedded::is_sealed(e.name) {
                "sealed"
            } else if PACKS.contains(&e.name) {
                "pack"
            } else {
                ""
            };
            Row {
                label,
                tag,
                vendored: root_dir.join(module_path(e.name)).exists(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rels(plan: &Plan) -> Vec<String> {
        plan.files
            .iter()
            .map(|f| f.rel.display().to_string())
            .collect()
    }

    /// `agent` is required by the scripts that use it, so its copy lands where
    /// `require` looks — under `lib/`, like every other embedded entry.
    /// Nothing vendors into `blocks/`, which holds a project's own entry points.
    #[test]
    fn an_embedded_consumer_vendors_as_a_module() {
        let plan = resolve("agent").expect("agent is embedded");
        assert_eq!(rels(&plan), ["lib/agent/init.lua"]);
        assert!(plan.warning.is_none());
        assert!(plan.files[0].source.contains("function"));

        let plan = resolve("coding").expect("coding is embedded");
        assert_eq!(rels(&plan), ["lib/coding/init.lua"]);
    }

    /// A module vendors whole: the root and every sub-module the binary
    /// carries, laid out the way `require` reads a dotted name.
    #[test]
    fn a_module_vendors_with_every_sub_module_it_has() {
        let plan = resolve("llm_proto").expect("llm_proto is embedded");
        assert_eq!(
            rels(&plan),
            [
                "lib/llm_proto/init.lua",
                "lib/llm_proto/openai.lua",
                "lib/llm_proto/anthropic.lua",
            ]
        );
        assert_eq!(plan.files[1].name, "llm_proto.openai");

        let plan = resolve("session").expect("session is embedded");
        assert_eq!(rels(&plan), ["lib/session/init.lua"]);
    }

    /// Half a module is not a unit, and the refusal names the whole it belongs
    /// to rather than leaving the caller to work it out.
    #[test]
    fn a_sub_module_on_its_own_is_refused_naming_the_root() {
        let err = resolve("llm_proto.openai").expect_err("a sub-module is not a unit");
        let msg = err.to_string();
        assert!(msg.contains("vendor llm_proto"), "{msg}");
        assert!(msg.contains("vendors whole"), "{msg}");
    }

    /// A sealed module is refused, root and sub-module alike, and the refusal
    /// says both why and how to read it anyway.
    #[test]
    fn a_sealed_name_is_refused_with_the_way_to_read_it() {
        let err = resolve("knl").expect_err("knl is sealed");
        let msg = err.to_string();
        assert!(msg.contains("`knl` is sealed"), "{msg}");
        assert!(msg.contains("require(\"embedded.knl\")"), "{msg}");

        // The generated one has no file behind it and is sealed as well, and a
        // sub-module of a sealed root is answered by the seal, not by the
        // sub-module rule.
        assert!(resolve("knl_types").is_err());
        let err = resolve("lshape.t").expect_err("lshape.t is sealed");
        assert!(err.to_string().contains("sealed"), "{err}");
    }

    /// An unknown name is told what there is, roots only — a list including
    /// `lshape.t` would be offering something the next line refuses.
    #[test]
    fn an_unknown_name_is_refused_listing_what_exists() {
        let err = resolve("nope").expect_err("nope is not embedded");
        let msg = err.to_string();
        assert!(msg.contains("`nope` is not an embedded module"), "{msg}");
        assert!(msg.contains("agent"), "{msg}");
        assert!(msg.contains("session"), "{msg}");
        assert!(!msg.contains("lshape.t"), "{msg}");
    }

    /// A pack is written, and the caller hears why a whole copy is rarely what
    /// they wanted: the warning is advice, not a refusal.
    #[test]
    fn a_pack_warns_and_is_written_anyway() {
        let plan = resolve("policy").expect("policy is embedded");
        assert_eq!(rels(&plan), ["lib/policy/init.lua"]);
        let warning = plan.warning.expect("a pack warns");
        assert!(warning.contains("knl.device"), "{warning}");
        assert!(warning.contains("embedded.policy"), "{warning}");
    }

    /// The header names the version it came from and both ways back: the name
    /// this copy now answers for, and the original under `embedded.`.
    #[test]
    fn the_header_names_the_version_and_the_original() {
        let body = render("session", "return {}\n");
        let mut lines = body.lines();
        assert_eq!(
            lines.next(),
            Some(
                format!(
                    "-- vendored from agent-block {} (embedded session)",
                    env!("CARGO_PKG_VERSION")
                )
                .as_str()
            )
        );
        assert!(body.contains("require(\"session\")"), "{body}");
        assert!(body.contains("require(\"embedded.session\")"), "{body}");
        // One way back, not two: `-b <name>` does not reach a module.
        assert!(!body.contains("-b session"), "{body}");
        assert!(body.ends_with("\nreturn {}\n"), "{body}");
    }

    /// The listing folds sub-modules under their root, calls every entry what
    /// it is to a project (a module), marks the two layers a caller should
    /// think twice about, and says which names this project has already taken
    /// over.
    #[test]
    fn the_listing_folds_sub_modules_and_marks_what_is_already_vendored() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join(".agent-block/lib/session")).expect("mkdir");
        std::fs::write(
            dir.path().join(".agent-block/lib/session/init.lua"),
            "return {}",
        )
        .expect("write");

        let listing = listing(dir.path());
        let line = |name: &str| {
            listing
                .lines()
                .find(|l| l.starts_with(name))
                .unwrap_or_else(|| panic!("no line for {name} in:\n{listing}"))
                .to_string()
        };

        assert!(line("agent").contains("lib"), "{listing}");
        assert!(!listing.contains("block"), "{listing}");
        assert!(line("session").ends_with("vendored"), "{listing}");
        assert!(!line("coding").contains("vendored"), "{listing}");
        assert!(
            line("lshape").starts_with("lshape (+t, check, reflect, luacats)"),
            "{listing}"
        );
        assert!(line("lshape").contains("sealed"), "{listing}");
        assert!(line("policy").contains("pack"), "{listing}");
        assert!(
            !listing.lines().any(|l| l.starts_with("lshape.t")),
            "{listing}"
        );
    }
}
