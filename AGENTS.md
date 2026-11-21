# Agent notes

## Project

EpocNote is a Rust MCP server/CLI that stores durable, searchable project
context as Git-tracked Markdown under `.context/` (see `README.md` and the
full spec in `plan.md` — section numbers in code comments refer to it).

Key invariants:

- Markdown files are authoritative; `.context/.epocnote.sqlite*` is a
  disposable, rebuildable index (git-ignored).
- Only exact-lowercase `.md` files are indexed; symlinks and special files
  are never followed into the index.
- Chunk metadata is replaced, never updated (FTS triggers enforce this).

## Verify before committing

```text
cargo fmt
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo build --release --locked
```

All four must pass; CI runs the same on Ubuntu, Windows, and macOS. Pin new
dependencies to exact versions that are at least 7 days old.

## Layout

- `src/` — library modules; `main.rs` implements `init`, `rebuild`, and
  `check`. `serve` is a stub.
- `src/root.rs`, `src/canon.rs`, `src/scan.rs` — root discovery, canonical
  paths, no-follow filesystem scan/read rules.
- `src/db.rs`, `src/sync.rs` — SQLite lifecycle, search-time
  synchronization, and forced rebuilds.
- `src/check.rs` — read-only checks of impacts and supersession.
- `src/doc.rs` — frontmatter parsing and chunking.
- `src/search.rs`, `src/note.rs` — the `epocnote_search` and
  `epocnote_note` tool logic.
- `src/error.rs` — application error codes and the warning shape.
- `tests/` — one integration file per module.
- `plan.md` — the active specification; `doc.md.before-monorepo` is a
  historical snapshot, do not implement against it.

## Conventions

- Map failures onto the error codes in `src/error.rs`; MCP application
  errors are tool results (`isError: true`), not protocol errors.
- Keep protocol stdout clean once `serve` exists — diagnostics go to
  stderr or warning fields.
- Commit to `main` and push when a task completes (the user asked for
  this explicitly). The `origin` remote uses SSH but pushes currently go
  over HTTPS via `gh` credentials:
  `git -c credential.helper='!gh auth git-credential' push https://github.com/andikapradanaarif/epocNote.git main`
- Windows cannot delete or replace an open file; file-replacement tests
  are `#[cfg(unix)]`-gated for that reason, not because the code paths
  differ.
