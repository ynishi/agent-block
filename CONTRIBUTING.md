# Contributing

Conventions for changes to agent-block, for people and coding agents alike.

## What may be published

agent-block is developed in public. Code, design alternatives, decisions and
their reasons belong in issues, commits and code documentation, where anyone
can read them.

What must not cross into the repository, an issue or a pull request:

- credentials, tokens, session material, signed or token-bearing URLs;
- someone else's private material: their identity, conversations, assets, or
  anything shared with you under an agreement, unless they agreed to it being
  published;
- details of a security problem that has not been fixed and disclosed;
- paths, hostnames or logs that name a person's machine or account.

An explanation has to stand on its own. A reference to a tracker or a note a
reader cannot open may sit beside it as provenance, never in place of it: say
what the problem was, what was decided and why, in the public artifact itself.

If something protected has already been published, deleting the commit does
not undo that. Revoke or rotate whatever can be revoked first, then report it
privately to the maintainer.

## Issues

Open an issue for anything beyond a typo. Describe the problem, what you
expected and what happened, and the version you ran (the release you installed,
or the commit you built from). A change that alters behaviour starts from an
issue, so the reasoning is in one place before the code is.

## Branches

Work on a branch, never on `main`. A worktree per change keeps parallel work
apart; put it in a directory git ignores so it never shows up in the tree.

## Building and verifying

Rust stable and [just](https://github.com/casey/just) are required. The Teal
gates need `htl`; [mise](https://mise.jdx.dev) installs the version the gates
pin (`mise install`, which reads `mise.toml`).

`just check` is the definition of green. It runs, in order: `gen` (regenerate
the files derived from the Rust types), `lint`, `test`, `test-lua`, `check-tl`
and `test-tl`. Each recipe's comment in the `justfile` says what it covers and
why it is ordered where it is.

While a change is still moving, run the narrow recipes:

- `cargo test -p <crate>` for one crate's Rust tests;
- `just test-lua <filter>` for the Lua specs whose file name contains `filter`;
- `just test-tl <filter>` for the Teal specs.

Do not run `cargo test --workspace`. It links every test binary at once, a
linker process and gigabytes of memory each, which is enough to exhaust a
shared or memory-tight machine. `just test` runs the crates one at a time for
that reason.

Report what you actually ran. "I did not run X" is a usable report; a green
claim resting on a recipe nobody ran is not.

## Documentation

Settled design lives in the code's own documentation and nowhere else: Rust
crate, module and item docs, and the `---` headers of the embedded Lua and Teal
modules. There is no separate design document tree. See "Design documentation
lives in the code" in the [README](README.md) for where the kernel's design
starts.

When a change moves a rule, move its statement in the doc comment beside the
code that enforces it, in the same change. When the code and a comment
disagree, the code wins and the comment is the bug.

## Commits

```text
<type>(<scope>): <what changed, one line>

<the problem, why this fix and not the alternative, what it cost>

Verified: <the recipes actually run, and their outcome>
```

- `type` is one of `feat`, `fix`, `refactor`, `docs`, `test`, `build`, `chore`;
  `scope` is the module or crate touched (`coding`, `policy`, `knl`, `core`).
- No AI attribution of any kind: no `Co-Authored-By`, no "Generated with", no
  `Signed-off-by` written on an agent's behalf.
- A user-visible change adds its entry to `CHANGELOG.md` under
  `## [Unreleased]` ([Keep a Changelog](https://keepachangelog.com/en/1.1.0/)).
- Never commit what `.gitignore` excludes. If a commit needs `git add -f`, stop:
  something is filed in the wrong place.

## Working with coding agents

Agents are welcome, under the same rules as everyone else. The loop that works:

```text
issue -> branch -> implement -> just check
      -> review the diff (disclosure, the issue's criteria, the doc comments)
      -> commit -> hand the push and the pull request to a person
```

An agent prepares everything that stays on the machine: the branch, the
commits, the verification, and the pull request body written to a file. Pushing,
opening the pull request, publishing a crate and cutting a release are done by
a person.

## Pull requests

- Base it on the current `main`, and run `just check` on the tree you hand over.
- Write the body to a file and open the pull request with `--body-file`, so the
  text reviewed is the text posted. The body says what changed, what was
  verified, and what the change deliberately does not cover.
- Keep one change per pull request. Formatting-only changes go in their own
  commit.

## License

By contributing you agree that your contribution is dual licensed under
Apache-2.0 and MIT, as the [README](README.md#license) states.
