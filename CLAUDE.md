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
# One JUnit test (generated Rust lands in sql-to-dbsp-compiler/temp/)
mvn test -pl SQL-compiler -Dtest=<TestClass> -Dsurefire.failIfNoSpecifiedTests=false

# Pipeline manager (needs the compiler built first)
cargo run --bin=pipeline-manager

# Python tests (from python/, need a running manager)
uv run python -m pytest tests/platform/test_pipeline_crud.py::test_pipeline_post
```

## Merged-index research: range-aligned IVM

Active branch: `docs/merged-index-tidy-up`. The former `feat/merged-index-storage` is archived as `archive/q3-investigation-abandoned`; do not build on it.

This branch prototypes maintaining TPC-H Q3 as the predicate-free join `V = Customer ⋈ Orders ⋈ Lineitem`. Two modes exist:

- **Native:** ordinary Feldera state. The two joins keep four integrator traces.
- **Reconstructed:** those four traces are not built. The join reads requested key ranges from a secondary "COL" merged index (folded keys: customer > order > line). The index is maintained outside the circuit on an auxiliary thread, and its before/after snapshots are published before each transaction.

The harness runs two chained circuits in one DBSP runtime (user decisions of 2026-10-23 and 2026-11-05), with one storage directory and one buffer cache. The base circuit (`base.sql`) holds the primary-key base tables. The view circuit (`q3.sql`, keyless Z-set tables plus `V`) maintains `V` from the base circuit's net delta. `--q3-merged-index` applies to the view circuit only. The base transaction commits before the view transaction starts, and the phases are told apart by those boundaries; the CPU profiler's per-circuit step CPU time (`profiled_cpu_ms`) is a cross-check. `ManualIndex` runs on an auxiliary thread of the runtime and writes through the runtime's buffer cache, which the view circuit's index readers also use. Its upkeep is base maintenance (user decision of 2026-10-26): `base_ms` includes `index_ms`, and `ivm_ms` equals `q3_ms`. Always report the base and IVM phases side by side.

### Where the code lives

| Piece | Location |
| --- | --- |
| Key trie (`trie.rs`), order-preserving key encoders, packed payload encoders (`packed.rs`), folded keys, merged-index batches (schema-independent) | `crates/dbsp/src/trace/merged_index/` |
| Merging cursor over the runs of an index snapshot: `step` and `seek` read no value or weight; `take` sums weights per value and counts cancelled values (roadmap step 2 of shared-traversal.md) | `RunMerge` in `crates/dbsp/src/trace/merged_index/run_merge.rs` |
| Several circuits in one runtime (`Runtime::init_circuits`, `CircuitConstructor`, one `DBSPHandle` per circuit); each generated crate exports `SqlCircuit::build` beside `circuit`; an associated function of an empty enum, so a user function named `build` cannot clash with it; its parameter is `__circuit`, because a SQL-bodied function named `circuit` is declared inside `build` and would shadow a parameter of that name (`SqlCircuitConstructorTest`) | `crates/dbsp/src/circuit/dbsp_handle.rs`; `ToRustVisitor.java` |
| Join that takes supplied snapshot readers instead of traces | `dyn_join_generic_with_snapshot_readers` in `crates/dbsp/src/operator/dynamic/join.rs`; typed wrapper in `mono.rs` |
| Compiler hook (`--q3-merged-index`) that pattern-matches the exact Q3 circuit | `Q3MergedIndexBinding.java`, `ToRustVisitor.java`, `CompilerOptions.java` |
| Q3 readers, `Q3State`, and the COL storage tests | `sql-to-dbsp-compiler/experiments/merged-index-col/` |
| Design and roadmap for declaring merged indexes as key-trie specs | `docs/design/merged-index/general-definition.md` |
| Meta plan for shared partner reads (thesis Chapters 2 and 3): buffered execution (collect each touched customer's group once, evaluate the delta rules over its buffers), its audit, its mapping onto Feldera code, decode-free storage, the scan session (`Maintain` with an `on_group` callback; knobs `Pass` and `GroupStart`), coding roadmap. Selective execution is deferred (`deferred.md`) | `docs/design/merged-index/shared-traversal.md` |
| Spec parser and schema validator (roadmap Step 3) | package `org.dbsp.sqlCompiler.compiler.frontend.mergedIndex` in `sql-to-dbsp-compiler/SQL-compiler` |
| Rust emitter for a spec (Step 4), selected by `--merged-index` and `--merged-index-output` | `MergedIndexRustEmitter.java` (`backend/rust`), `CompilerMain.emitMergedIndex` |
| COL spec and its generated module, which COL re-exports as its definition (`lib.rs` adds only `build_batch`) | `merged-index-col/col.mi`, `generate.sh`, `src/generated.rs` |
| Run counters: per-thread seeks, filter rejections, and key, value, and weight decodes of single runs (roadmap step 1 of shared-traversal.md) | `RunReads` in `crates/dbsp/src/trace/cursor/run_reads.rs`; counted in `VecIndexedWSetCursor` and `FileIndexedWSetCursor` (which positions values lazily: a landing decodes the key only, and the first read of a value or weight fills and counts the value); attributed by `with_read_owner` (COL's `scan` reads under `MergedIndex`); `Q3State::finish` returns each transaction's index reads, and `DBSPHandle::run_reads` the view worker's reads per owner, both reported per refresh by the harness; baseline in `merged-index-col/src/q3/tests.rs` |
| Harness: loading, RF1/RF2 refreshes, `ManualIndex`, R5 measurement | `sql-to-dbsp-compiler/experiments/q3-ivm-baseline/` |

### Experiment commands

Run these from `sql-to-dbsp-compiler/experiments/q3-ivm-baseline` with JDK 21. `generate.sh` writes the git-ignored `q3-base-circuit/src/lib.rs` and `q3-view-circuit/src/lib.rs` (and each `stubs.rs`); the driver has no `src/lib.rs`.

```bash
./generate.sh                       # native circuit
cargo +1.96.1 test --offline --locked
./generate.sh --q3-merged-index     # reconstructed circuit
cargo +1.96.1 test --offline --locked --features merged-index
./verify.sh                         # full gate: fmt, both modes, clippy, transcript cmp, COL suite, Python tests; restores native generation
python3 measure.py --help           # R5 measurement runner
```

The COL library tests with `cargo +1.96.1 test --offline --locked` in its own directory. After changing `col.mi`, the schema, or the emitter, rebuild the compiler JAR (`mvn package -DskipTests` in `SQL-compiler`), run `merged-index-col/generate.sh`, and commit `src/generated.rs`; `verify.sh` fails on a stale file. The DBSP side tests with `cargo +1.96.1 test --offline -p dbsp --lib merged_index` (also `supplied_snapshot_join`).

### Contracts that the code enforces but the docs omit

- **One worker.** Maintenance and initial loading both use one DBSP worker; this is the user's decision of 2026-10-12. Reconstructed mode and R5 runs reject any other count in `main.rs`.
- **Transactions.** A DBSP transaction is `start_transaction`, any number of `step()` calls, then `commit_transaction`. It is not an OLTP commit: outputs appear at commit and the logical clock advances once.
  - A transaction runs on the base circuit, then on the view circuit. Wrap the view circuit's transaction in `Q3State::publish(before, after, placements)` and `finish()`.
  - The join readers run once, at the commit flush. Steps inside one transaction share one publish, but separate transactions cannot share one.
- **Placements and supported changes.** The placement map (order → customer) comes only from Orders rows in the same transaction's net delta. Changes outside RF1/RF2 are therefore unsupported by design, an accepted limitation.
  - The harness's `prepare` rejects such changes.
  - Reaching the circuit any other way hits `expect` in `merged-index-col/src/q3.rs` and panics the worker.
- **Uniqueness.** Folded keys are unique because of the base tables' primary keys; no separate validator is needed. The merged index is fed the base circuit's net delta (`workload::net_delta`, via `reconstruction::prepare`), not the driver's logical change, so an upsert reaches it as a deletion of the old row and an insertion of the new one.
- **One runtime, two handles.** The handles share the runtime and its one worker thread, so `run_reads()` (a per-thread total) reports the same answer from either handle; the harness takes it around the view transaction, when only the view circuit runs. Checkpointing a runtime with several circuits is unsupported (`init_circuits` rejects an initial checkpoint), so the harness has no checkpoint test.
  - The shared buffer cache is tested by behavior, not only by pointer equality: `init_circuits_read_each_others_cached_blocks` (a file one circuit's step wrote is read by the other's step with foreground hits and no misses) against its control `separate_runtimes_miss_each_others_blocks` (two runtimes miss), both in `dbsp_handle.rs`.
  - The index's own binding is covered by `index_file_runs_land_in_the_view_workers_cache` in the harness's `main.rs` (reconstructed feature). A memory target far below the process's size puts the runtime under pressure, so that `ManualIndex`'s thread spills its one appended run to a file. The test asserts that `Runtime::buffer_cache()` on that thread and on the view worker are one object (`Arc::ptr_eq`), that every block of the file (`Batch::file_block_locations`) is in the worker's cache by direct `BufferCache::get` lookup, and that the cache is below capacity, so no block was evicted. Its control, `index_file_runs_miss_the_view_workers_cache_without_the_binding`, leaves the thread unbound (`ManualIndex::without_cache_binding`): the caches differ and no block is found. Neither test counts hits or misses.
- **Net delta.** The base tables' delta streams stay idle until `workload::enable_net_delta` enables them, which `Pipeline::new` does. Reading `net_delta` drains them, and reading a snapshot through `BaseState::new` consumes it, so take one `BaseState` per transaction. Deletions in the view circuit need the complete row (`DeleteStyle::WholeRow`); the base circuit deletes by key.

### Trusting the docs

The docs in `docs/design/merged-index/` were consolidated on 2026-10-12. Each file owns one topic; `README.md` is the entry point and `WORKFLOW.md` is the only home for commands.

- Prefer code and tests. Docs cite code by symbol name; line anchors drift.
- The user's confirmed decisions live in `docs/design/merged-index/decisions.md`, dated. A "decision" elsewhere without an entry there is unconfirmed. Record new user decisions there.
- R1–R5 is the only numbering scheme. Completed plans and history are archived under `TRASH/` and are not authoritative.
- Never infer an unmeasured quantity from leftover arithmetic, such as assigning residual storage to the index. The user rejected this as circular. Report only what was measured.
- Make no performance or savings claim without a raw artifact. Raw artifacts live in git-ignored `target/` and are local-only.
- "Residual execution" is an acceptable term.

### Slides (`docs/design/merged-index/slides-oct-8/`)

- Build with `sh build.sh`, then inspect the rendered pages. A clean TeX build does not prove the slides look right.
- `build.sh` hard-codes the page count.
- The user's own edits to the deck are authoritative. Do not reapply earlier agent guidance over them.
- Notation:
  - "transaction delta", not "batch"
  - "run" for an immutable sorted collection
  - subscripts for access order, superscripts for before/after state
