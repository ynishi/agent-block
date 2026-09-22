//! The modules baked into the binary, as a list a caller outside this crate
//! can read.
//!
//! The host needs the embedded sources as two tables keyed by `require` name —
//! that is what [`crate::host`] holds them as, and it is the only shape the
//! require registry wants. A tool that writes one of them to disk needs
//! something else: the whole set, each entry knowing which directory it belongs
//! in and what its source is. Exposing the host's constants would answer that
//! by handing out the registry's internals; this module answers it with one
//! list and nothing else.
//!
//! `agent-block vendor` is the caller. It expands an entry into a project's
//! `.agent-block/` directory, which is the first tier both lookups search (see
//! [`crate::host::lib_roots`] / [`crate::host::block_roots`]), so the copy is
//! what the project resolves from then on.
//!
//! Two things are deliberately not here. `knl_types` is embedded but generated
//! at start from the Rust syscall surface, so it has no static source to hand
//! out. And nothing here writes: what a copy should say at the top of it, and
//! where it may land, belong to the tool doing the writing, not to the list.

use std::sync::OnceLock;

use crate::host::{EMBEDDED_BLOCKS, EMBEDDED_LIBS};

/// Which of the host's two embedded lists an entry came from.
///
/// `Block` means the module is also reported by `inspect_tools` as a tool
/// surface; both kinds are `require`d by name and neither is a file on disk, so
/// **this does not say where a copy of one belongs**. A project's copy is always
/// a module — `agent` is reached by `require("agent")` like the rest — and
/// `blocks/` holds a project's own entry points, which the binary has none of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// An entry point — `blocks/<name>.lua`.
    Block,
    /// A module — `lib/<name>/init.lua`.
    Lib,
}

impl Kind {
    /// The word a listing prints.
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Block => "block",
            Kind::Lib => "lib",
        }
    }
}

/// One embedded module: the name `require` resolves, which kind it is, and the
/// Lua source compiled into the binary.
///
/// A sub-module carries its dotted name whole (`lshape.t`), the same string the
/// require registry is keyed by, so a caller can tell a root from a part of one
/// by looking for the dot.
#[derive(Debug, Clone, Copy)]
pub struct Entry {
    /// The `require` name.
    pub name: &'static str,
    /// Entry point or module.
    pub kind: Kind,
    /// The source, verbatim.
    pub source: &'static str,
}

/// Every embedded entry: the blocks first, then the modules, each in the order
/// the binary lists them.
pub fn entries() -> &'static [Entry] {
    static ENTRIES: OnceLock<Vec<Entry>> = OnceLock::new();
    ENTRIES
        .get_or_init(|| {
            let blocks = EMBEDDED_BLOCKS.iter().map(|(name, source)| Entry {
                name,
                kind: Kind::Block,
                source,
            });
            let libs = EMBEDDED_LIBS.iter().map(|(name, source)| Entry {
                name,
                kind: Kind::Lib,
                source,
            });
            blocks.chain(libs).collect()
        })
        .as_slice()
}

/// The entry named `name`, if the binary carries one.
pub fn find(name: &str) -> Option<&'static Entry> {
    entries().iter().find(|e| e.name == name)
}

// `EMBEDDED_SPECS`: every `blocks/**/spec/*.lua`, listed by `build.rs`.
include!(concat!(env!("OUT_DIR"), "/embedded_specs.rs"));

/// One spec file the binary carries beside a module: the module it checks,
/// its file name under that module's `spec/`, and the Lua source.
///
/// A spec is not `require`d by name and is not on the require path; it is
/// part of the module the way a test is part of a crate. It is here so a copy
/// of the module can be written with its specs, and checked by them.
#[derive(Debug, Clone, Copy)]
pub struct Spec {
    /// The root module the spec belongs to (`policy`, `agent`, …).
    pub module: &'static str,
    /// The file name under `spec/` (`api_spec.lua`).
    pub file: &'static str,
    /// The source, verbatim.
    pub source: &'static str,
}

/// The specs of the root module `name`, in file-name order. Empty for a module
/// that has none, and for a name that is not embedded.
pub fn specs_of(name: &str) -> Vec<Spec> {
    EMBEDDED_SPECS
        .iter()
        .filter(|(module, _, _)| *module == name)
        .map(|(module, file, source)| Spec {
            module,
            file,
            source,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The list is the two tables and nothing else, in that order — a caller
    /// printing it is printing what the binary carries.
    #[test]
    fn the_entries_are_the_blocks_then_the_modules() {
        let names: Vec<&str> = entries().iter().map(|e| e.name).collect();
        assert_eq!(&names[..2], ["agent", "coding"]);
        assert!(names.contains(&"lshape.t"), "{names:?}");

        let blocks = entries().iter().filter(|e| e.kind == Kind::Block).count();
        assert_eq!(blocks, EMBEDDED_BLOCKS.len());
        assert_eq!(entries().len(), EMBEDDED_BLOCKS.len() + EMBEDDED_LIBS.len());
    }

    /// Every entry hands back the source that is compiled in, not a name to go
    /// looking for.
    #[test]
    fn an_entry_carries_its_source() {
        let session = find("session").expect("session is embedded");
        assert_eq!(session.kind, Kind::Lib);
        assert!(session.source.contains("return M"), "{}", session.source);
        assert!(find("no_such_module").is_none());
    }

    /// The specs come from the tree, not a hand list: a module with a `spec/`
    /// directory has them, one without has none, and each carries its source.
    #[test]
    fn a_module_carries_the_specs_beside_it() {
        let policy = specs_of("policy");
        assert!(
            policy.iter().any(|s| s.file == "api_spec.lua"),
            "{:?}",
            policy.iter().map(|s| s.file).collect::<Vec<_>>()
        );
        assert!(policy.iter().all(|s| s.module == "policy"));
        assert!(policy
            .iter()
            .find(|s| s.file == "api_spec.lua")
            .expect("api_spec")
            .source
            .contains("lust."));
        assert!(specs_of("session").is_empty(), "session has no spec/");
        assert!(specs_of("no_such_module").is_empty());
        // `agent` and `coding` are blocks, not under `lib/`, and are listed too.
        assert!(!specs_of("agent").is_empty());
        assert!(!specs_of("coding").is_empty());
    }
}
