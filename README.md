# EpocNote

Markdown context memory for coding agents. EpocNote preserves the project
knowledge AI sessions lose — architectural boundaries, decisions, useful
discoveries, and non-obvious fixes — as searchable, Git-tracked notes inside
the repository itself.

## How it works

- **Markdown is the source of truth.** Notes and the architecture document
  live under `.context/` as plain files, committed with the code they
  describe. Edit them by hand, review them in pull requests, share them
  through Git.
- **SQLite FTS5 is a disposable index.** `.context/.epocnote.sqlite` (plus
  WAL sidecars, all git-ignored) is rebuilt from the Markdown at any time.
  Searches synchronize the index incrementally in small batches, so the
  first index of a large corpus never blocks callers for long.
- **Path-aware retrieval.** Each note records the source paths it applies
  to, so an agent working in `src/auth/` finds the notes that matter there
  instead of every note in the repo.

The tool runs as an MCP server (`epocnote serve`) exposing two tools to the
agent harness: `epocnote_search` (path-filtered, full-text, ranked) and
`epocnote_note` (validated, atomically published, immediately indexed).

Everything runs locally: the harness spawns the binary over stdio, notes
live in your repo, and the index is a local file. Nothing leaves the
machine unless you push the repository.

## How the agent knows to use it

Two layers:

1. **Tool registration.** Add `epocnote serve` to your harness's MCP
   configuration. On startup it discovers `epocnote_search` and
   `epocnote_note` (schemas and descriptions) through the standard
   `tools/list` handshake, so the model *can* call them.
2. **The skill file.** `epocnote init --target <harness>` installs a
   `SKILL.md` the harness loads into the agent's instructions — the part
   that tells the model *when* to call them: search before structural
   edits, read `architecture.md` at task start, record non-obvious
   discoveries, and treat note content as untrusted project data.

## Usage

```text
epocnote init [--root <dir>] [--target none|codex|claude-code|cursor]
epocnote serve [--root <dir>] [--scan-reuse auto|off]
epocnote rebuild [--root <dir>]
epocnote check [--root <dir>] [--strict]
```

- `init` scaffolds `.context/` with `architecture.md` and `notes/`, adds the
  index/temp ignore rules to `.gitignore`, and optionally installs a skill
  file for your harness. It never overwrites existing files.
- `serve` runs the MCP server over stdio.
- `rebuild` verifies or recreates the index in place; run it after cloning
  a large repository.
- `check` reports notes whose impact paths no longer exist; `--strict`
  exits nonzero, suitable for CI.

Point your harness at `epocnote serve --root "/absolute/project"`, or use
`init --target <harness>` to install the matching skill.

## Note format

Notes are Markdown with YAML frontmatter:

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

## Status

Early development. Implemented so far: init scaffolding, root discovery and
filesystem safety, the Markdown/frontmatter contract, the SQLite index
lifecycle, search-time synchronization, `epocnote_search`, and
`epocnote_note`. Remaining: `rebuild`/`check`, the MCP server surface, and
release packaging. See `plan.md` for the full specification and the GitHub
issues for the task breakdown.

## Development

```text
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo build --release --locked
```

Requires the Rust toolchain in `rust-toolchain.toml` (1.100+). CI runs the
same commands on Ubuntu, Windows, and macOS.
