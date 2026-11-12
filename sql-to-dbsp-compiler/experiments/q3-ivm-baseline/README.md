# Native Q3 join IVM baseline

This standalone experiment maintains `V = Customer ⋈ Orders ⋈ Lineitem` with the SQL compiler's
incremental circuit, and optionally with the reconstructed circuit that reads the
Customer–Orders–Lineitem (COL) merged index instead of four integrator traces. `V` carries the
identities and payloads Q3 needs before its residual filters, revenue aggregation, ranking, limit,
and projection. [query.sql](query.sql) has one predicate-free materialized view and no intermediate
view or join hint.

## Two chained circuits

Base-table maintenance and view maintenance are separate generated circuits, each in its own DBSP
runtime (user decision of 2026-10-23):

- **Base circuit** ([base.sql](base.sql), crate `q3-base-circuit`). The three tables with primary
  keys and no view. The driver applies each transaction here first. Its net delta, read from the
  delta stream that the catalog registers for each input map, is exactly the change to the base
  relations, with complete rows for deletions.
- **View circuit** ([q3.sql](q3.sql), crate `q3-view-circuit`). The same tables without keys (Z-set
  inputs) and `V`. The driver feeds it the net delta, so it maintains `V` and nothing else.
  This is the native view circuit. The reconstructed build replaces it with the hand-written
  `q3-trie-view-circuit`, whose inputs are keyed by their full trie paths and come from `Dlt`.

[query.sql](query.sql) remains the logical definition: keyed tables plus `V`. The merged-index
module is generated from its schema.

Both circuits run in one runtime (`Runtime::init_circuits`, measurement version 6), with one
worker, one storage directory (`run-N`), and one buffer cache of `--cache-bytes` (rounded down to
whole MiB) under one `--memory-target-bytes`. The base transaction commits before the view
transaction starts, and the CPU profiler's per-circuit step CPU time (`profiled_cpu_ms`) is
reported beside the phase timers as a cross-check. In reconstructed mode the merged index lives in
the same runtime and writes through its buffer cache, because its upkeep is base maintenance. The phases are `base_ms` (the base
circuit's transaction plus the index append), `index_ms` (the append alone, a sub-phase of
`base_ms` and never added to it again), `forward_ms`, and `q3_ms`, the view circuit alone, which
is also reported as `ivm_ms`. Base and IVM are always reported side by side.

All commands live in [WORKFLOW.md](../../../docs/design/merged-index/WORKFLOW.md). The
architecture and R1–R5 status are in the
[design README](../../../docs/design/merged-index/README.md).

## Worker policy

Maintenance and initial loading both use one DBSP worker
([decision of 2026-10-12](../../../docs/design/merged-index/decisions.md)). The driver defaults to
`--workers 1`. Reconstructed mode and R5 runs reject any other count. Record the worker count in
every run configuration.

## Layout

| Path | Role |
| --- | --- |
| `base.sql`, `q3.sql`, `query.sql` | The base circuit's SQL, the view circuit's SQL, and the logical definition (keyed tables plus `V`) from which the merged-index module is generated. |
| `generate.sh` | Generates `q3-base-circuit/src/{lib,stubs}.rs`, `q3-view-circuit/src/{lib,stubs}.rs`, and `plan.json` (the view circuit's plan), all git-ignored. `generate.sh` takes no option: the reconstructed view circuit is hand-written. It strips the generated allocator, which `src/main.rs` defines once. |
| `q3-base-circuit/`, `q3-view-circuit/` | Library crates around the generated circuits. |
| `q3-trie-view-circuit/` | The committed, hand-written reconstructed view circuit (the driver's `merged-index` feature builds it in place of `q3-view-circuit`). |
| `src/main.rs`, `src/data.rs` | Handwritten driver (`Pipeline` holds both circuits) and `.tbl` parser. |
| `src/workload.rs` | `BaseState`, the workload checks including `BaseState::validate`, and `net_delta`, which reads the base circuit's delta streams. |
| `src/reconstruction.rs` | `ManualIndex` and the auxiliary-thread index maintenance (Cargo feature `merged-index`). |
| `src/verification.rs` | Shared R4 tests for both modes. |
| `src/measurement.rs`, `measure.py` | R5 reporting and the runner. |
| `summarize.py`, `prepare_inputs.py` | Median, minimum and maximum of `measure.py` manifests; RF1/RF2 inputs for the Linux SF 10 campaign (module docstrings give the CLIs and rules). |
| `verify.sh` | The full verification gate. |
| [CIRCUIT.md](CIRCUIT.md) | The generated circuit and its state reads. |
| [RESULTS.md](RESULTS.md) | Dataset, correctness evidence, and historical measurements. |

## Modes

The native path is the default: `./generate.sh` and Cargo without the `merged-index` feature. The
reconstructed path needs the `merged-index` Cargo feature.
The concrete COL codecs and storage tests live in the standalone
[`merged-index-col`](../merged-index-col/README.md) library, which the native path does not use.

In reconstructed mode the harness maintains the merged index in `ManualIndex`, outside both
circuits, on an auxiliary thread of the runtime that hosts both circuits (booked as base maintenance since
measurement version 4). Per
transaction it converts the base circuit's net delta into signed index rows (`reconstruction::prepare`), appends them, then
publishes `Old` (the index before the append) and `Dlt` (the appended run). One scan session fills the typed readers, and
the harness hands their `Dlt` batches, the driving deltas, to the view circuit's input handles; the readers are kept until the view transaction
finishes. Any delta of the base tables is supported: `prepare` gives each changed order its owners, and line items with no order
wait in `OrphanLines`. The index is fed from the net delta and not from the
driver's logical change, so a workload with upserts would feed it correctly too. The contract and its limits are in
[reconstruction.md](../../../docs/design/merged-index/reconstruction.md).

Reconstruction copies the buffered rows into temporary native vectors for
the existing joins. This copy is suboptimal; a direct reconstructed cursor is a future
optimization. The prototype makes no performance-gain or hard-memory-cap claim.

## Verification

`Reference` is a measurement-harness checker, not DBSP state. With verification enabled it keeps
independent Customer, Orders, and Lineitem maps and compares each transaction's emitted delta and
materialized `V` with a full join; `--no-verify` allocates none of them. The shared tests in
`src/verification.rs` run loading, then RF1 insertion and RF2 deletion, in both modes, and
`verify.sh` compares the two modes' transcripts byte for byte, for RF1/RF2 and for general deltas (`general_transcript`). Details are in
[reconstruction.md](../../../docs/design/merged-index/reconstruction.md).

## Measurement

R5 reports are defined in [measurement.md](../../../docs/design/merged-index/measurement.md). The
qualifying Linux run is pending; macOS runs are smoke tests. The original R1 driver buffered
whole base files and retained source maps even with `--no-verify`; only [RESULTS.md](RESULTS.md)
describes it.
