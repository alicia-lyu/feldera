# AGENTS.md

This file provides guidance to Codex (Codex.ai/code) when working with code in this repository.

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

On this fork, commit `AGENTS.md` files in the same commits and branch as the code and docs they describe. Do not use a separate `Codex-context` branch or `scripts/Codex.sh push`.

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
# One JUnit test (generated Rust lands in sql-to-dbsp-compiler/temp/)
mvn test -pl SQL-compiler -Dtest=<TestClass> -Dsurefire.failIfNoSpecifiedTests=false

# Pipeline manager (needs the compiler built first)
cargo run --bin=pipeline-manager

# Python tests (from python/, need a running manager)
uv run python -m pytest tests/platform/test_pipeline_crud.py::test_pipeline_post
```

## Merged-index research

The authoritative guidance for the merged-index work lives in [`CLAUDE.md`](CLAUDE.md), section "Merged-index research", and in the `CLAUDE.md` files of the directories it lists. Read the root section first, then the `CLAUDE.md` of each directory you work in and of its parents. This file does not mirror them.
