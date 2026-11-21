# EpocNote

Markdown context memory for coding agents — durable, searchable project
knowledge that lives inside the repository itself.

## Background

AI coding sessions are ephemeral. When a session ends, everything the agent
learned is gone: why the session store was split, which module owns billing,
the fix for the flaky migration, the decision to reject Redis. The next
session — or the next agent — starts blank and either repeats the
investigation or, worse, makes a change that violates a constraint nobody
wrote down.

Built-in memory features don't quite solve this. Harness memory
(e.g. Claude Code's auto memory) lives in per-user home directories: it is
not committed with the code, not shared with teammates or other machines,
and typically skips architecture and debugging fixes by design.

## What it solves

EpocNote stores that knowledge as **ordinary Markdown files under
`.context/` in your repo**, so it is committed, code-reviewed, and shared
through Git like everything else. Agents get two MCP tools:

- `epocnote_search` — find notes by source path and/or full text. Each note
  records the paths it applies to (`impacts`), so an agent editing
  `src/auth/` sees the notes that matter *there*, ranked and budgeted.
- `epocnote_note` — record a discovery or decision. The tool generates the
  frontmatter, validates the inputs, publishes the file atomically, and
  indexes it immediately.

Design properties:

- **Markdown is the source of truth.** You can read, edit, and review
  notes without any tooling; Git handles sharing and history.
- **SQLite FTS5 is a disposable index.** `.context/.epocnote.sqlite`
  (git-ignored) is rebuilt from the Markdown at any time. Searches
  synchronize it incrementally in small batches.
- **Everything runs locally.** The harness spawns the binary over stdio;
  nothing leaves the machine unless you push the repo.

## Operation

### 1. Install

```sh
cargo install --locked --path .
# or build once:  cargo build --release --locked
```

### 2. Initialize a project

```sh
cd your-project
epocnote init --target claude-code   # or: codex | cursor | none
```

This creates `.context/` containing `architecture.md` (the project's
always-loadable summary) and `notes/`, adds the index/temp ignore rules to
`.gitignore`, and installs the harness skill that tells the agent when to
use the tools. Existing files are never overwritten; the command prints
the resolved root and the launch command to configure.

### 3. Point your harness at the server

Add an MCP server entry for your project, e.g.:

```json
{ "command": "epocnote", "args": ["serve", "--root", "/absolute/path/to/your-project"] }
```

The harness then discovers `epocnote_search` and `epocnote_note` through
the standard `tools/list` handshake, and the installed skill tells the
agent when to call them (search before structural edits, read
`architecture.md` at task start, record non-obvious discoveries).

### 4. Notes

Notes are Markdown with YAML frontmatter; agents write them through the
tool, but plain files work too:

```markdown
---
title: "Why the session store is split"
kind: adr
scope: paths
created_at: "2026-11-21T06:00:00Z"
impacts:
  - src/session/
supersedes:
  - .context/notes/2026-11-01-old-decision-a1b2c3d4e5f60718.md
---
Reasoning and constraints, as ordinary Markdown.
```

`impacts` are project-relative paths the note applies to; `scope: global`
makes a note eligible under every filter. `supersedes` marks replaced
decisions — they stay searchable but rank last.

### 5. Maintenance

```text
epocnote rebuild [--root <dir>]   # verify or recreate the index in place
epocnote check  [--root <dir>] [--strict]   # report notes whose impacts are stale
```

Run `rebuild` after cloning a large repo. `check --strict` exits nonzero on
findings, suitable for CI.

## Status

Early development. Implemented: init scaffolding, filesystem safety, the
Markdown contract, index lifecycle, search-time synchronization,
`epocnote_search`, and `epocnote_note`. Remaining: `rebuild`/`check`, the
`serve` MCP surface, and release packaging — so `serve` does not run yet.
See `plan.md` for the full specification and the GitHub issues for the
task breakdown.

## Development

```text
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo build --release --locked
```

Requires the Rust toolchain in `rust-toolchain.toml` (1.100+). CI runs the
same commands on Ubuntu, Windows, and macOS.
