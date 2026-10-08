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
The concrete Customer–Orders–Lineitem codecs and storage tests are also
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
cargo +1.96.1 test --offline --locked
```

The native path is the default: use `./generate.sh` and Cargo without the
`merged-index` feature. To generate and build the reconstructed path, use:

```sh
./generate.sh --q3-merged-index
cargo +1.96.1 build --offline --locked --features merged-index
cargo +1.96.1 test --offline --locked --features merged-index
```

The generated compiler option and Cargo feature must be enabled together. Run
the executable with the same input arguments as the native command. Initial
loading prepares the base relations and merged index as setup for maintenance
measurements. The R5 driver reads bounded batches, loading Customers first and
then complete Order/Lineitem groups. It requires ascending Order keys and
Lineitem rows grouped by Order, and reports unsupported ordering. Both modes use
the same load transactions. Independent `Reference` validation is allocated
only when verification is enabled; `--no-verify` retains no reference maps.
The measured workload is limited to RF1 insertion and RF2 deletion under
referential integrity.
Replacements, key moves, orphan rows, independent Lineitem changes, and Customer
updates are unsupported.

The harness maintains the merged index manually on a DBSP auxiliary thread
outside the Q3 graph. Before each logical refresh, it captures the previous
index snapshot, appends physical signed changes, waits for insertion, and
publishes previous and current snapshots to Q3. The temporary placement map is
built from complete Orders records in that refresh's delta, supplying one candidate
customer per order. The readers' support for multiple candidates is documented
in the [separate placement correction](../../../docs/design/merged-index/placement-lookup-multimap-plan.md)
and does not extend the supported refresh workload. No persistent lookup is kept.
Views remain published until Q3 finishes the transaction.

Reconstruction reads only requested matching key ranges, decodes their rows,
and copies them into temporary native batches for the existing joins. This
copying path is suboptimal: decoding, copying, and possible sorting are charged
to `q3_ms`. A direct reconstructed cursor is a future optimization. The temporary
placement map currently uses `BTreeMap` and `BTreeSet`; container selection is also
a future optimization, preserving its transaction-local contents. Report
`index_ms`, `q3_ms`, and `combined_maintenance_ms` for each RF1/RF2 refresh;
input preparation and RF2 discovery remain separate. The prototype makes no
performance-gain or hard-memory-cap claim. Its checkpoint test covers save and
restore at a completed transaction boundary; circuit stacking and coordinated
cross-circuit checkpoints are deferred.

## R5 maintenance measurements

R5 resolves RF2 IDs from the current maintained Orders snapshot. It obtains
complete deletion rows from native Lineitem primary-key ranges or the manual
merged index's `(custkey, orderkey)` line range. Requested IDs are deduplicated;
absent Orders are ignored and counted. Discovery uses maintained storage rather
than rescanning base files or replaying refreshes. Operational checks enforce
the supported workload against maintained state and the current delta.

The R5 runner builds and preserves separate native and reconstructed release
executables, then restores default native generation. It starts a fresh process
and storage directory for each mode and repetition; three repetitions are the
default. Run from this directory with Java 21, Rust 1.96.1, and the compiler
already built:

```sh
python3 measure.py \
  --customer "$TPCH_DATA/customer.tbl" \
  --orders "$TPCH_DATA/orders.tbl" \
  --lineitem "$TPCH_DATA/lineitem.tbl" \
  --refresh-list "$TPCH_DATA/refreshes.json" \
  --report target/r5-measurements/report.json \
  --cache-bytes 1073741824 \
  --memory-target-bytes 4294967296 \
  --no-verify
```

The refresh list is a JSON array in execution order. Each item is either
`{"rf1_orders":"orders.tbl.u1","rf1_lines":"lineitem.tbl.u1"}` or
`{"rf2":"delete.1"}`; relative paths resolve against the list's directory.
`--cache-bytes` must be a whole number of MiB. The R5 driver requires file-backed
storage, one worker, native join fetch, no compression, and
`min_storage_bytes=0`. It records the cache capacity, runtime memory-pressure
target, and compaction timeout. Loading compaction completes before measurement;
after the refresh sequence, circuit and manual-index compaction drain with the
configured timeout. A complete refresh that cannot execute within its budget
fails the run rather than being split.

The JSON report records input and executable hashes, revision, configuration,
per-phase measurements, and qualification status. It separates RF2 discovery,
input preparation and operational checks, index maintenance, the Q3 transaction,
publication/cleanup, and complete foreground latency. Final compaction drainage
is included in sequence totals. `index_ms + q3_ms` is a subtotal that excludes
discovery and preparation. Backend operations and bytes are reported separately
from OS physical I/O; OS physical byte counters are unavailable on macOS and
appear as null. Reports include CPU time, process RSS high-water mark, cache
occupancy, settled and sampled peak storage, affected customers/orders, and line
fan-out; unavailable counters are marked explicitly. Independent correctness
checking is excluded from measured runs.

macOS reports are smoke tests and do not establish equal enforced total memory.
Linux qualification requires equal cgroup v2 total-memory limits, swap disabled,
and private copies of every input prepared inside each run's cgroup during
loading. The R5 harness is implemented; the qualifying measurement campaign is
pending. Refresh completion has no durability guarantee. Periodic checkpoint
measurement, broader updates, residual Q3, and Q5/Q10 remain deferred.

## RF1/RF2 verification

From this directory, run the complete verification workflow with Java 21 available
through `JAVA_HOME` or `PATH` and the compiler already built:

```sh
./verify.sh
```

The script generates both circuit modes and runs each full experiment suite,
plus the COL suite, using Rust 1.96.1 and offline locked dependencies. It checks
Rust formatting, Clippy with warnings denied, shell syntax, and whitespace. It
restores default native generation on exit, including when a check fails. Logs and
JSON result files are stored in a fresh ignored `target/r4-verification/`
directory; `CARGO_TARGET_DIR` can select an existing compilation cache.

The shared tests in `src/verification.rs` load Customers first, then complete
Order/Lineitem groups, and repeat RF1 insertion and RF2 deletion. They cover
multiple customers/orders, distinct lines with identical payloads, oversized
groups crossing batch boundaries, empty transactions, and multiple runtime
steps within a transaction. Every committed emitted delta and materialized `V`
is compared with the full join computed by `Reference::evaluate` from logical
input records.
The reconstructed run also checks all four readers against expected rows computed
directly from logical Customer, Orders, and Lineitem records for the requested keys.
Each run writes a JSON file containing each transaction's name, sorted emitted rows
and weights, and sorted materialized rows and weights. The script checks that the
native and reconstructed JSON files are identical byte for byte. See
[RESULTS.md](RESULTS.md) for executed evidence. These checks cover the supported
loading and RF1/RF2 workload, where each order has one candidate customer.
Broader updates, residual Q3, and performance comparisons are deferred.

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
full-join checks from measurement runs. RF1 submits related Orders
and Lineitem inserts in one transaction. RF2 removes each present order and its
lines in one transaction.
The initial load is submitted through the same incremental circuit in batches.

The original R1 driver buffered each entire base file, retained source maps for
RF2 discovery even with `--no-verify`, and used memory-backed state unless
`--storage-path` was supplied. Its phase reports and reference-byte counters
describe those original measurements only. The R5 driver uses bounded loading,
maintained-storage RF2 discovery, and file-backed state; see the R5 section
above for its intervals and report fields.
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
