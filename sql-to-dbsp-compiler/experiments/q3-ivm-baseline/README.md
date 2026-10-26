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
  `--q3-merged-index` applies to this circuit only.

[query.sql](query.sql) remains the logical definition: keyed tables plus `V`. The merged-index
module is generated from its schema.

Each runtime has one worker, its own storage directory (`run-N/base` and `run-N/q3`), and its own
buffer cache. The driver splits `--cache-bytes` between them (the base circuit gets half the MiB,
rounded down) and gives both the same `--memory-target-bytes`, because RSS is process-wide. The
report records both. In reconstructed mode the merged index lives in the base runtime (storage
under `run-N/base`), because its upkeep is base maintenance. The phases are `base_ms` (the base
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
| `generate.sh` | Generates `q3-base-circuit/src/{lib,stubs}.rs`, `q3-view-circuit/src/{lib,stubs}.rs`, and `plan.json` (the view circuit's plan), all git-ignored. `generate.sh --q3-merged-index` generates the reconstructed view circuit. It strips the generated allocator, which `src/main.rs` defines once. |
| `q3-base-circuit/`, `q3-view-circuit/` | Library crates around the generated circuits. The view crate has the Cargo feature `merged-index`, which the driver's feature of the same name enables. |
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
reconstructed path needs the compiler option `--q3-merged-index` and the Cargo feature together.
The concrete COL codecs and storage tests live in the standalone
[`merged-index-col`](../merged-index-col/README.md) library, which the native path does not use.

In reconstructed mode the harness maintains the merged index in `ManualIndex`, outside both
circuits, on an auxiliary thread of the base circuit's runtime (measurement version 4). Per
transaction it converts the base circuit's net delta into signed index rows (`reconstruction::prepare`), appends them, then
publishes the before and after snapshots to the view circuit, which keeps them until its
consumers finish. The placement map (order → customer) is built from the Orders rows of that net
delta, so only RF1 and RF2 are supported. The index is fed from the net delta and not from the
driver's logical change, so a workload with upserts would feed it correctly too. The contract and its limits are in
[reconstruction.md](../../../docs/design/merged-index/reconstruction.md).

Reconstruction decodes the requested key ranges and copies them into temporary native batches for
the existing joins. This copy is suboptimal; a direct reconstructed cursor is a future
optimization. The prototype makes no performance-gain or hard-memory-cap claim.

## Verification

`Reference` is a measurement-harness checker, not DBSP state. With verification enabled it keeps
independent Customer, Orders, and Lineitem maps and compares each transaction's emitted delta and
materialized `V` with a full join; `--no-verify` allocates none of them. The shared tests in
`src/verification.rs` run loading, then RF1 insertion and RF2 deletion, in both modes, and
`verify.sh` compares the two modes' transcripts byte for byte. Details are in
[reconstruction.md](../../../docs/design/merged-index/reconstruction.md). Replacements, key moves,
orphan rows, independent Lineitem changes, and Customer updates are unsupported.

## Measurement

R5 reports are defined in [measurement.md](../../../docs/design/merged-index/measurement.md). The
qualifying Linux run is pending; macOS runs are smoke tests. The original R1 driver buffered
whole base files and retained source maps even with `--no-verify`; only [RESULTS.md](RESULTS.md)
describes it.
