# Native Q3 join IVM baseline

This standalone experiment maintains `V = Customer ⋈ Orders ⋈ Lineitem` with the
SQL compiler's native incremental circuit. `V` includes the identities and
payloads needed by Q3 before its residual filters, revenue aggregation,
ranking, limit, and projection. The SQL in [query.sql](query.sql) has one
predicate-free materialized view and no intermediate view or join hint.

## Generate and run

Use the repository's pinned Calcite build and Java 21 as described in the
[compiler README](../../README.md). From this directory, regenerate the Rust
circuit with:

```sh
./generate.sh
```

This is Catalog mode and the compiler's default optimization level 2. The
generated file is checked in so the experiment's actual circuit is reviewable.
Run the deterministic native transactions with:

```sh
cargo test --offline
```

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
  --rf2 "$TPCH_DATA/delete.1"
```

The defaults are one worker, 10,000 input records per load transaction, three
independent runs, full delta and materialized snapshot verification, and
memory-backed DBSP state. `--workers`, `--batch`, and `--runs` override these
counts. `--storage-path DIR` enables native file-backed state in a distinct
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

The circuit evidence and its mapping to the generated Rust and runtime join
implementation are in [CIRCUIT.md](CIRCUIT.md).
The pinned dataset, verification, and measured runs are in
[RESULTS.md](RESULTS.md).
