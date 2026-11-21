# EpocNote

EpocNote gives coding agents and their teams a place to keep project
decisions, architectural boundaries, and hard-won debugging findings.
It stores that knowledge as Markdown alongside the code.

**Project status.** The CLI includes a local MCP server for search and note
creation, plus project initialization, index rebuilds, and metadata checks.
[Track development on GitHub](https://github.com/andikapradanaarif/epocNote/issues).

## Background

A coding agent tracing a failure may discover that session revocation has
to keep working when the cache is down. That finding explains why a future
refactor should preserve the behavior. When the reasoning stays in one
chat, the next agent or teammate may have to trace the failure again.

EpocNote keeps the current architecture in `.context/architecture.md` and
records individual discoveries and decisions in `.context/notes/`. Notes can
name the source paths they affect, so a path search for
`src/session/store.rs` can surface a decision about `src/session/`. When a
decision changes, its replacement can link to the older note so the
history stays readable.

Because the notes are ordinary Markdown, a team can review them with the
changes they describe and share them through Git. SQLite FTS5 provides
a local search index that can be rebuilt from the notes.

## Build and run

### 1. Install the CLI

Download the archive and its `.sha256` file for your platform from
[GitHub Releases](https://github.com/andikapradanaarif/epocNote/releases) or
the [CI run for the commit](https://github.com/andikapradanaarif/epocNote/actions/workflows/ci.yml).
Each package includes the executable, build details, and SQLite compile
options. SQLite is bundled in the executable.

| Package | Oldest OS tested by the release workflow |
| --- | --- |
| `epocnote-linux-x86_64.tar.gz` | Ubuntu 24.04 |
| `epocnote-macos-arm64.tar.gz` | macOS 15 |
| `epocnote-macos-x86_64.tar.gz` | macOS 15 |
| `epocnote-windows-x86_64.zip` | Windows Server 2022 |

On Linux, verify the archive with `sha256sum -c <archive>.sha256`. On macOS,
use `shasum -a 256 -c <archive>.sha256`. On Windows, run
`Get-FileHash -Algorithm SHA256 <archive>` and compare the hash with the
first field in the checksum file.

Extract the archive into a directory you control. Run the executable by
its absolute path or add its directory to `PATH`. Verify the install with
`epocnote --version`. To upgrade, stop any running EpocNote servers, replace
the executable, and restart your client. Your Markdown stays in the
project's `.context/` directory.

#### Build from source

You need Git, [Rustup](https://rustup.rs/), and a C compiler and linker for
your Rust target. Rustup selects the version pinned in
[`rust-toolchain.toml`](rust-toolchain.toml). Cargo builds SQLite from the
bundled source.

```sh
git clone https://github.com/andikapradanaarif/epocNote.git
cd epocNote
cargo build --release --locked
```

On macOS or Linux, verify the executable from the checkout.

```sh
./target/release/epocnote --version
./target/release/epocnote init --help
```

On Windows, use PowerShell.

```powershell
.\target\release\epocnote.exe --version
.\target\release\epocnote.exe init --help
```

To run `epocnote` by name from any directory, install it from the checkout
with `cargo install --locked --path .`. Ensure Cargo's bin directory is on
your `PATH`. Its default is `~/.cargo/bin` on macOS and Linux and
`$HOME\.cargo\bin` in PowerShell.

### 2. Initialize the project you want to document

Run the installed CLI with the path to an existing project directory.
Keep the quotes if the path contains spaces.

```sh
epocnote init --root "/absolute/path/to/your project"
```

If you built without installing, use `./target/release/epocnote` or
`.\target\release\epocnote.exe` from the EpocNote checkout. On Windows, a
project path could be `"C:\Projects\your project"`.

You can also run `epocnote init` from inside your project. With no `--root`,
EpocNote walks upward to the nearest `.context` or `.git` marker. If it finds
neither, `init` uses the current directory. An explicit `--root` selects the
directory directly.

### 3. Install a project skill if you use an agent client

Run `init` with the target for your client. You can also do this in the first
invocation.

```sh
epocnote init --root "/absolute/path/to/your project" --target codex
```

| Target | Project-local skill path |
| --- | --- |
| `codex` | `.agents/skills/epocnote/SKILL.md` |
| `claude-code` | `.claude/skills/epocnote/SKILL.md` |
| `cursor` | `.cursor/skills/epocnote/SKILL.md`, unless it reuses an identical compatible skill |
| `none` (default) | Markdown setup only |

Each run accepts one target. Rerunning `init` preserves existing architecture,
notes, and skills. The generated skill gives the agent instructions for when
to read, search, and record context.

### 4. Verify the setup

Open the selected project root and check for these files and directories.

| Path | What to expect |
| --- | --- |
| `.context/architecture.md` | A scaffold for the project's purpose, boundaries, dependencies, and constraints |
| `.context/notes/` | A directory for project findings and architecture decision records |
| `.gitignore` | Rules for `.context/.epocnote.sqlite*` and `.context/notes/.epocnote-tmp-*` |
| The selected skill path | An `epocnote` skill manifest when you choose a client target |

In a Git working tree, run this from the project root to see which rules
ignore the database and its WAL sidecar. If `init` reports an ignore conflict,
inspect the effective rule before committing the scaffold.

```sh
git check-ignore -v -- .context/.epocnote.sqlite .context/.epocnote.sqlite-wal
```

Fill in `architecture.md` and commit it with any notes and skills you want
the team to share.

## Write a project note

Create `.context/notes/session-revocation.md` and adapt this example to your
project. Use `kind: note` for a finding or fix, and `kind: adr` for a decision
whose reasoning and trade-offs the team should keep.

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

Write `impacts` relative to the project root using `/` separators. Use
`scope: global` for context that applies across the project. If a decision
changes, add a `supersedes` list to the new note with the previous note's
project-relative path, such as `.context/notes/session-revocation.md`.
Superseded notes remain searchable and rank after notes with no recorded
replacement.

## Maintain the index and notes

Run these commands from the project root, or pass `--root` with the path to
the project.

```sh
epocnote rebuild
epocnote check
epocnote check --strict
```

`rebuild` reads and hashes every eligible Markdown file, so it catches edits
that preserve both file size and modification time. It updates the existing
SQLite index in batches and reports indexed, excluded, and failed document
counts. If a document cannot be read, its last indexed version stays available
and `rebuild` exits unsuccessfully after processing the other files.

`check` reads the Markdown directly. It reports stale impact paths, unresolved
supersession references, cycles, and invalid frontmatter. The default exits
successfully when it finishes, even with findings. Use `--strict` to fail on
findings in CI. An incomplete inspection fails in either mode.

## Connect an MCP client

Add a local stdio server entry to your MCP client with absolute paths to the
executable and the project root. Use your client's configuration format
around this command and argument list.
On Windows, use the `.exe` and escape backslashes in JSON paths.

```json
{
  "command": "/absolute/path/to/epocnote",
  "args": ["serve", "--root", "/absolute/path/to/your project"]
}
```

The client launches the process and discovers the tools through MCP. The
server supports `2026-07-28` discovery and `2025-11-25` initialization. To
verify the connection, try this workflow in the initialized project.

1. Confirm the client lists `epocnote_search` and `epocnote_note`. If you
   installed a project skill, check whether the client can invoke it.
2. Read `.context/architecture.md` and call `epocnote_search` with
   `{"path_filter":"src/session/store.rs"}`. The example note should be
   eligible. Add `"query":"revocation"` to narrow the search by text.
3. Call `epocnote_note` with a title, Markdown body, and an `impacts` array.
   For example, use this request in a sample project with `src/session/`.

   ```json
   {
     "title": "Cache outage regression test",
     "body": "The revocation test disables the cache before checking the session store. Keep that setup when changing session reads.",
     "impacts": ["src/session/"]
   }
   ```

4. Check the returned path under `.context/notes/` and search for the new
   note. If the result says `saved: true` and `indexed: false`, the Markdown
   already exists. Search again after the index has caught up.

See the [project specification](plan.md) for the Markdown and tool contracts
and [release verification](docs/acceptance.md) for tested protocols,
benchmarks, and pilot status.

## Development

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo build --release --locked
```

CI runs these checks on Ubuntu, macOS, and Windows.
