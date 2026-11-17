# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

- To gather more context beyond README.md:
  - Look at the outstanding changes in the tree
  - If on a branch check the last 2-3 to commits
  - Look at relevant README.md in sub-folders

- Write production quality code.
  - Adhere to rules in "Code Complete" by Steve McConnell.
  - Adhere to rules in "The Art of Readable Code" by Dustin Boswell & Trevor Foucher.
- Make sure the code compiles.
- When adding code always ensure that tests cover the newly added code:
  - Unit tests that validate for regular and exceptional inputs
  - Use property based testing/model based testing/fuzzing when appropriate
  - Integration tests for big platform-level features (in @python/tests)

- Add/update documenation and comments.
  - Adhere to rules in "Bugs in Writing: A Guide to Debugging Your Prose" by Lyn Dupre.
  - Adhere to rules in "The Elements of Style, Fourth Edition" by William Strunk Jr. & E. B. White

On this fork, commit `CLAUDE.md` files in the same commits and branch as the code and docs they describe. Do not use a separate `claude-context` branch or `scripts/claude.sh push`.

## Architecture

SQL flows through four layers:

1. `sql-to-dbsp-compiler/` (Java, Calcite) compiles tables and views into a Rust DBSP circuit. Flags are documented in `sql-to-dbsp-compiler/using.md`.
2. The generated crate depends on `crates/sqllib` (SQL runtime functions) and `crates/dbsp` (the incremental engine: circuits, operators, Z-set batches, `Spine` traces, file-backed storage).
3. `crates/adapters` (with `adapterlib`, `feldera-types`) connects circuits to input and output connectors.
4. `crates/pipeline-manager` is the REST server that compiles and runs pipelines. `python/` holds the SDK and integration tests; `js-packages/web-console` is the SvelteKit UI, whose API client is generated from `openapi.json`.

## Commands

Toolchain: Rust 1.96.1 (`rust-version` in `Cargo.toml`; no toolchain file, so pass `+1.96.1`). The SQL compiler needs JDK 19–21; the default JDK on this machine is 17, so export `JAVA_HOME=/opt/homebrew/opt/openjdk@21`.

```bash
# Rust: one crate, one test filter
cargo +1.96.1 test -p dbsp --lib <test_name_substring>

# Lint, as scripts/pre-push runs it
cargo fmt --all -- --check
cargo clippy --workspace -- -D warnings

# SQL compiler: full build (also builds Calcite), or the compiler alone
cd sql-to-dbsp-compiler && ./build.sh
cd sql-to-dbsp-compiler/SQL-compiler && mvn package -DskipTests
# One JUnit test (generated Rust lands in sql-to-dbsp-compiler/temp/); tests that cargo-check
# the generated crate call plain `cargo`, so also export RUSTUP_TOOLCHAIN=1.96.1
mvn test -pl SQL-compiler -Dtest=<TestClass> -Dsurefire.failIfNoSpecifiedTests=false

# Pipeline manager (needs the compiler built first)
cargo run --bin=pipeline-manager

# Python tests (from python/, need a running manager)
uv run python -m pytest tests/platform/test_pipeline_crud.py::test_pipeline_post
```

## Merged-index research: range-aligned IVM

Active branch: `docs/merged-index-tidy-up`. The former `feat/merged-index-storage` is archived as `archive/q3-investigation-abandoned`; do not build on it.

The fork maintains SQL views incrementally by reading join partners from a secondary *merged index* instead of DBSP's integrator traces. A merged index stores several tables in one run, sorted by folded keys along a key trie (for COL: customer > order > line). The first prototype is TPC-H Q3 over COL. The view `V = Customer ⋈ Orders ⋈ Lineitem` is compared in two modes. **Native** is ordinary Feldera state. **Reconstructed** uses the same DBSP operators, but their partner reads come from the index (user decision of 2026-11-11). The next index is MI_P (Part > Partsupp > Lineitem) for Q2, Q9, Q14, Q16, Q17, Q19, and Q20; start with [Building a second merged index](docs/design/merged-index/new-merged-index.md).

### Where to read

Each code directory has a short `CLAUDE.md` that Claude loads when it works there. Rules that bind any index or harness are kept one level up, here or in `experiments/CLAUDE.md`, so a new sibling directory inherits them.

| Directory | Its `CLAUDE.md` covers |
| --- | --- |
| [`docs/design/merged-index/`](docs/design/merged-index/README.md) | Entry point: design, R1–R5 status, code map, terms; cross-directory commands in `WORKFLOW.md` |
| [`crates/dbsp/`](crates/dbsp/CLAUDE.md) | Fork additions to DBSP: several circuits in one runtime, the supplied-snapshot join, run counters and read owners |
| [`crates/dbsp/src/trace/merged_index/`](crates/dbsp/src/trace/merged_index/CLAUDE.md) | The generic index machinery: key trie, encoders, folded keys, `RunMerge`, `ScanSession`, readers |
| [`sql-to-dbsp-compiler/SQL-compiler/`](sql-to-dbsp-compiler/SQL-compiler/CLAUDE.md) | `.mi` spec parser and validator, Rust emitter, `SqlCircuit::build`, generated names |
| [`sql-to-dbsp-compiler/experiments/`](sql-to-dbsp-compiler/experiments/CLAUDE.md) | Rules for any merged index and IVM harness: one runtime, accounting, transactions, general deltas, placement, images |
| [`.../experiments/merged-index-col/`](sql-to-dbsp-compiler/experiments/merged-index-col/CLAUDE.md) | COL's definition and the Q3 readers |
| [`.../experiments/q3-ivm-baseline/`](sql-to-dbsp-compiler/experiments/q3-ivm-baseline/CLAUDE.md) | The Q3 harness: circuits, `ManualIndex`, `verify.sh`, `measure.py`; siblings `MEASUREMENT.md`, `LINUX-HANDOFF.md`, `DECISIONS.md`, `CIRCUIT.md` |
| [`docs/design/merged-index/slides-oct-8/`](docs/design/merged-index/slides-oct-8/README.md) | The advisor deck; its README holds the editing rules |

### Research-wide rules

- **One worker.** Maintenance and initial loading both use one DBSP worker (user decision of 2026-10-12). Say "the worker", not per-worker detail.
- **Fair mechanism, not imitation.** Compare both modes under the same real mechanism and config; state a limit as a caveat instead of staging a typical placement.
- **Report only what was measured.** Never infer an unmeasured quantity from leftover arithmetic, such as assigning residual storage to the index; the user rejected this as circular. Make no performance or savings claim without a raw artifact. Raw artifacts live in git-ignored `target/` and are local-only. Always report the base and IVM phases side by side.
- **Terms.**
  - Say "run" for any immutable sorted collection, in memory or in a file; Feldera's code calls it a batch.
  - Say "transaction delta" for one transaction's change set, and "compaction", not "merge".
  - Say "prefix truncation", not "front coding"; "driving delta", not "driving stream"; and "DBSP operator seeks/reads", not "view seeks". Never say "projection" for converting an index entry to a row.
  - Code identifiers keep their names.
  - "Residual execution" is an acceptable term.

### Trusting the docs

- Prefer code and tests. Docs cite code by symbol name, since line anchors drift.
- The user's confirmed decisions are dated and split by owner. Generic design and process decisions are in [`docs/design/merged-index/decisions.md`](docs/design/merged-index/decisions.md). Q3 harness and measurement decisions are in [`q3-ivm-baseline/DECISIONS.md`](sql-to-dbsp-compiler/experiments/q3-ivm-baseline/DECISIONS.md). A "decision" without an entry in one of them is unconfirmed. Record new decisions in the file that owns them.
- R1–R5 is the only numbering scheme. `history.md` and `TRASH/` hold superseded material and are not authoritative.
