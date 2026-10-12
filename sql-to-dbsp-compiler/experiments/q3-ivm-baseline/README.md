# Native Q3 join IVM baseline

This standalone experiment maintains `V = Customer ⋈ Orders ⋈ Lineitem` with the SQL compiler's
incremental circuit, and optionally with the reconstructed circuit that reads the
Customer–Orders–Lineitem (COL) merged index instead of four integrator traces. `V` carries the
identities and payloads Q3 needs before its residual filters, revenue aggregation, ranking, limit,
and projection. [query.sql](query.sql) has one predicate-free materialized view and no intermediate
view or join hint.

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
| `query.sql`, `generate.sh` | The SQL and the script that generates `src/lib.rs`, `src/stubs.rs`, and `plan.json` (git-ignored). `generate.sh --q3-merged-index` generates the reconstructed circuit. |
| `src/main.rs`, `src/data.rs`, `src/udf.rs` | Handwritten driver, `.tbl` parser, and UDF scaffolding. |
| `src/workload.rs` | `BaseState` and the workload checks, including `BaseState::validate`. |
| `src/reconstruction.rs` | `ManualIndex` and the auxiliary-thread index maintenance (Cargo feature `merged-index`). |
| `src/verification.rs` | Shared R4 tests for both modes. |
| `src/measurement.rs`, `measure.py` | R5 reporting and the runner. |
| `verify.sh` | The full verification gate. |
| [CIRCUIT.md](CIRCUIT.md) | The generated circuit and its state reads. |
| [RESULTS.md](RESULTS.md) | Dataset, correctness evidence, and historical measurements. |

## Modes

The native path is the default: `./generate.sh` and Cargo without the `merged-index` feature. The
reconstructed path needs the compiler option `--q3-merged-index` and the Cargo feature together.
The concrete COL codecs and storage tests live in the standalone
[`merged-index-col`](../merged-index-col/README.md) library, which the native path does not use.

In reconstructed mode the harness maintains the merged index in `ManualIndex`, outside the Q3
circuit, on a DBSP auxiliary thread. Per transaction it appends the signed index rows, then
publishes the before and after snapshots to Q3, which keeps them until its consumers finish.
The placement map (order → customer) is built from the Orders rows of that transaction's delta,
so only RF1 and RF2 are supported. The contract and its limits are in
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
