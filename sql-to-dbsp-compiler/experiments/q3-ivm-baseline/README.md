# Native Q3 join IVM baseline

This standalone experiment maintains `V = Customer ⋈ Orders ⋈ Lineitem` with the
SQL compiler's native incremental circuit. `V` includes the identities and
payloads needed by Q3 before its residual filters, revenue aggregation,
ranking, limit, and projection. The SQL in [query.sql](query.sql) has one
predicate-free materialized view and no intermediate view or join hint.

## Worker policy

All future IVM maintenance runs use one DBSP worker (`--workers 1`), including
RF1 and RF2. Initial loading may use multiple workers. The driver defaults to one worker. Keep this
setting explicit in recorded run commands and configurations.

The IVM maintenance implementation targets single-worker execution. Partitioning,
cross-worker exchange, distributed state placement, and migration are outside
its current scope. No multiworker maintenance extension is planned.

## Generate and run

The [compilation and run workflow](../../../docs/design/merged-index/WORKFLOW.md)
walks through compiler builds, incremental inspection artifacts,
baseline comparisons, native execution, and the agent-added report formatting.
The concrete Customer–Orders–Lineitem codecs and storage fixture are also
available as a reusable standalone library in
[`merged-index-col`](../merged-index-col/src/lib.rs). This baseline does not
instantiate that library on its default path. The opt-in `merged-index`
feature runs the RF1/RF2 reconstruction prototype described below.

Use the repository's pinned Calcite build and Java 21 as described in the
[compiler README](../../README.md). From this directory, regenerate the Rust
circuit with:

```sh
./generate.sh
```

This is Catalog mode and the compiler's default optimization level 2. Run
`./generate.sh` before building or testing a fresh checkout. It produces
`src/lib.rs`, `src/stubs.rs`, and `plan.json`; these compiler artifacts are
Git-ignored. The handwritten `src/main.rs` driver, `src/data.rs` parser,
`src/udf.rs` scaffolding, SQL, and generator remain tracked. The generated
circuit and plan are available locally for inspection after generation.
Run the deterministic native transactions with:

```sh
cargo test --offline
```

The native path is the default: use `./generate.sh` and Cargo without the
`merged-index` feature. To generate and build the reconstructed path, use:

```sh
./generate.sh --q3-merged-index
cargo build --offline --features merged-index
cargo +1.96.1 test --offline --features merged-index
```

The generated compiler option and Cargo feature must be enabled together. Run
the executable with the same input arguments as the native command. Initial
loading prepares the base relations and merged index; it is setup, not part of
the RF1/RF2 comparison. The reconstructed mode loads Customers initially,
then keeps complete Order and Lineitem groups together. Its measured workload
is limited to RF1 insertion and RF2 deletion under referential integrity.
Replacements, key moves, orphan rows, and independent Lineitem changes are
unsupported.

The harness maintains the merged index manually on a DBSP auxiliary thread
outside the Q3 graph. Before each logical refresh, it captures the previous
index snapshot, appends physical signed changes, waits for insertion, and
publishes previous and current snapshots to Q3. The temporary order-to-customer
map is built from complete Orders records in that refresh's delta. No persistent
lookup is kept. Views remain published until Q3 finishes the transaction.

Reconstruction reads only requested matching key ranges, decodes their rows,
and copies them into temporary native batches for the existing joins. This
copying path is suboptimal: decoding, copying, and possible sorting are charged
to `q3_ms`. A direct reconstructed cursor is a future optimization. Report
`index_ms`, `q3_ms`, and `combined_maintenance_ms` for each RF1/RF2 refresh;
input preparation and RF2 discovery remain separate. The prototype makes no
performance-gain or hard-memory-cap claim. Its checkpoint test covers save and
restore at a completed transaction boundary; circuit stacking and coordinated
cross-circuit checkpoints are deferred.

For a local dbgen run, supply exact paths to complete base files and one
refresh batch. For example, after setting `TPCH_DATA` to a dbgen output
directory:

```sh
cargo run -- \
  --customer "$TPCH_DATA/customer.tbl" \
  --orders "$TPCH_DATA/orders.tbl" \
  --lineitem "$TPCH_DATA/lineitem.tbl" \
  --rf1-orders "$TPCH_DATA/orders.tbl.u1" \
  --rf1-lines "$TPCH_DATA/lineitem.tbl.u1" \
  --rf2 "$TPCH_DATA/delete.1" \
  --workers 1
```

The defaults are one worker, 10,000 input records per load transaction, three
independent runs, full delta and materialized snapshot verification, and
memory-backed DBSP state. `--batch` and `--runs` override these counts;
retain `--workers 1` for IVM maintenance runs. `--storage-path DIR` enables native file-backed state in a distinct
`run-N` subdirectory for each run. `--no-verify` excludes the independent
full-join oracle from measurement runs. RF1 submits related Orders and Lineitem inserts in one
transaction. RF2 removes each present order and its lines in one transaction.
The initial load is submitted through the same incremental circuit in batches.

Each phase prints native transaction time, minimum/median/p95/maximum native
transaction latency, input-record throughput, input preparation time,
end-to-end time, retained driver reference bytes, and peak process RSS. Native time includes
maintenance of materialized `V`; it excludes parsing, input serialization,
the independent checker, and output inspection. Preparation includes dbgen
parsing, refresh discovery, batch assembly, and input serialization. The
end-to-end load interval includes circuit initialization and checking; RF1 and
RF2 intervals cover their respective phase preparation, native step, and
checking. Peak process RSS includes the
driver and embedded DBSP runtime, so it is not a measure of retained operator
state alone. The driver buffers each entire base file during load; the
reported reference bytes measure its retained source maps after a phase.
`--no-verify` still retains those maps for RF2 discovery. The
memory-backed configuration uses `CircuitConfig::with_workers` with no storage
backend.
With verification enabled, `delta_plus`, `delta_minus`, and `view_rows` report
signed output multiplicities and the materialized cardinality. With
`--no-verify`, those counters are not computed and `verified=false` marks them
as unavailable.

## Generated circuit

The compiler-expanded logical graph and per-worker runtime graph exports,
including raw DOT/JSON and provenance, are documented in [CIRCUIT.md](CIRCUIT.md).
The shared [workflow](../../../docs/design/merged-index/WORKFLOW.md) gives the
capture commands and artifact layout.
The pinned dataset, verification, and measured runs are in
[RESULTS.md](RESULTS.md).
