# EpocNote

EpocNote is project memory for coding agents. It keeps the decisions and
discoveries that shaped a codebase available across sessions. A note about
session storage, for example, can explain why revocation must keep working
during a cache outage and point future editors to the relevant code.

That context lives in ordinary Markdown under `.context/`, where it can be
edited by hand and reviewed in the same pull request as the code it describes.
SQLite FTS5 provides a rebuildable local search index. Git carries the notes
and their history between teammates and worktrees.

**Early development.** The CLI currently initializes projects and installs
optional agent skills. MCP serving and the `rebuild` and `check` commands are
planned. See the [project issues](https://github.com/andikapradanaarif/epocNote/issues)
for progress.

## Get started

From a checkout of EpocNote with [Rustup](https://rustup.rs/) installed, build
and install the CLI using the toolchain pinned in
[`rust-toolchain.toml`](rust-toolchain.toml).

```sh
cargo install --locked --path .
epocnote init --root "/path/to/your project"
```

`init` creates `.context/architecture.md` and `.context/notes/`, and adds
`.gitignore` rules for the SQLite index, its sidecars, and temporary note files.
You can rerun it while preserving existing notes, skills, and ignore rules.
Start by filling in the architecture file with the project's purpose, module
boundaries, dependencies, and constraints.

Use `--target codex`, `--target claude-code`, or `--target cursor` to add
EpocNote's project-local skill for your client. The default, `--target none`,
sets up the Markdown workspace. The launch command printed by `init` is for
the planned MCP server.

## Write a project note

Put a finding that will matter to a future editor in `.context/notes/`. Use
`kind: adr` for a decision whose reasoning and trade-offs the team should keep.

```markdown
---
title: Keep session revocation independent of the cache
kind: adr
scope: paths
created_at: "2026-11-21T06:00:00Z"
impacts:
  - src/session/
---

Revocation checks use the session store directly so that a cache outage
cannot prevent us from revoking a compromised session. This adds a store
read on the revocation path.
```

`impacts` ties a note to project-relative paths, so a search for
`src/session/store.rs` can find notes about the whole `src/session/` directory.
Use `scope: global` for context that applies across the project. When a decision
changes, add the previous note's path to the new note's `supersedes` list.
Superseded notes remain searchable and rank lower.

## How agents will use it

The planned MCP server runs locally over stdio. MCP lets the client discover
the tools, and the optional project skill gives agents guidance on when to
use them. A typical session starts by reading `.context/architecture.md`.
Before editing, the agent calls `epocnote_search` with a source path, a text
query, or both. After a structural change or a non-obvious fix, it can use
`epocnote_note` to save a Markdown note with the reasoning and affected paths.

The [project specification](plan.md) covers search behavior, the Markdown
format, and the planned MCP and maintenance commands.

## Development

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo build --release --locked
```

CI runs these checks on Ubuntu, macOS, and Windows.
