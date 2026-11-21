# Release verification

The release workflow builds and tests native binaries on Ubuntu 24.04 x86-64,
macOS 15 on Intel and Apple silicon, and Windows Server 2022 x86-64. Each
archive includes a SHA-256 checksum and the SQLite version and compile options
recorded in the release profile. A second job on a fresh runner verifies each
archive, runs the executable with an empty PATH, and checks that replacing the
executable preserves project Markdown. Tagged builds produce a draft release
from the verified packages. [GitHub Actions](https://github.com/andikapradanaarif/epocNote/actions/workflows/ci.yml)

## Automated checks

| Behavior | Evidence in the repository |
| --- | --- |
| CLI validation, initialization, and project skill files | [CLI tests](../tests/cli.rs), [initialization tests](../tests/init.rs) |
| Root discovery, canonical paths, scan policy, and file reads | [Filesystem tests](../tests/fs.rs), [canonical path tests](../src/canon.rs) |
| SQLite setup and version handling, triggers, and bundled FTS5 | [Database tests](../tests/db.rs), [engine and trigger tests](../src/db.rs) |
| Scan reuse, read failures, reconciliation, and exclusions | [Synchronization tests](../tests/sync.rs), [expired deadline test](../src/sync.rs) |
| Forced rebuild, fresh and stale metadata, and check exit status | [Maintenance tests](../tests/maintenance.rs) |
| Frontmatter fallbacks, heading parsing, chunking, and limits | [Markdown tests](../tests/doc.rs) |
| Eligibility, path ordering, text tiers, stemming, and supersession | [Search tests](../tests/search.rs) |
| Real stdio envelopes, tool errors, response budget, context reinitialization, and note/search round trips | [MCP tests](../tests/mcp.rs) |
| Atomic note publication, cancellation, and saved-but-unindexed notes | [Note tests](../tests/note.rs), [publication tests](../src/note.rs), [MCP tests](../tests/mcp.rs) |
| Extracted binary, paths with spaces, checksum, and Markdown preservation when replacing the binary | [Package smoke test](../scripts/package.py) |

The protocol tests run against `epocnote serve` and pinned `rmcp` 3.10.0.
They cover MCP `2026-07-28` discovery and direct tool calls, `2025-11-25`
initialization, protocol probe errors, and tool responses in both eras.

| Client | Version and mode | Evidence |
| --- | --- | --- |
| Codex CLI | 0.171.0, local app-server on macOS 15.7.9 Intel | Listed the project skill, discovered both MCP tools, and made direct note and search calls in an ephemeral local thread |
| Claude Code and Cursor | Project-local skill files | The generated manifests and Cursor reuse rules are covered by initialization tests |

Explicit skill invocation and automatic activation during model turns are
part of the pilot.

## Benchmarks

The [benchmark driver](../examples/benchmark.rs) generates deterministic
fixtures of 1,000 and 100,000 Markdown files. Each file is 5,120 bytes, with
one unique marker, repeated session text, and an impact in one of 100 shards.
Both queries target the same shard. The text query searches for one unique
marker, while the path query ranks the notes in that shard. Each reported
median and p95 uses 15 searches against a populated index with normal scan
reuse. The separate post-index search can refresh an expired scan.

Build the example, then generate and measure each fixture in a separate
directory. Use a fresh directory for each run. On macOS, `/usr/bin/time -l`
reports process peak RSS in bytes. On Linux, use `/usr/bin/time -v` and
convert its peak RSS from KiB.

```sh
cargo build --release --locked --example benchmark
run_dir="$(mktemp -d)"
for notes in 1000 100000; do
  target/release/examples/benchmark fixture "$run_dir/$notes" "$notes"
  /usr/bin/time -l target/release/examples/benchmark measure "$run_dir/$notes" "$notes"
done
```

### Measured results

One release-profile run on macOS 15.7.9 (24G830), an Intel Core i7-9750H,
16 GiB RAM, and local APFS. Rust was 1.100.0 and bundled SQLite was 3.54.1.
All timings are milliseconds. The first-answer measurement includes
opening an empty index. The write-lock measurement includes commit and
any associated checkpoint work.

| Notes | Scan | First answer | Total initial indexing | Longest write lock | Post-index search | Warm text median / p95 | Warm path median / p95 | Peak RSS |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1,000 | 8.05 | 556.24 | 556.24 | 71.76 | 8.35 | 0.27 / 0.29 | 0.49 / 0.72 | 8.18 MiB |
| 100,000 | 838.96 | 2,014.39 | 125,365.65 | 1,113.07 | 2,439.51 | 5.72 / 8.48 | 66.05 / 165.52 | 45.82 MiB |

The first answer was partial for 100,000 notes and complete for 1,000.
An earlier run loaded the first chunk of every document for path ranking.
Ranking metadata first and fetching only returned chunks brought the
observed 100,000-note median path search from 795 ms to 66 ms and peak
process RSS from 271 MiB to 46 MiB. The earlier run included the first
post-index scan in its warm samples.

The two-second catch-up budget yielded a first answer near two seconds
on the large fixture. Keep the 100-document, 250 ms, and scan-reuse
defaults for this baseline. The measured 1.11 s lock includes work after
the batch's last document boundary. Separate commit and checkpoint
timings and repeat under competing writers before tuning the batch limit.

## Usefulness pilot

The [pilot protocol](pilot.md) records the four measures in plan section 13.1.
It requires at least two weeks of normal work in a real repository and human
review of a sample of searches and notes.
