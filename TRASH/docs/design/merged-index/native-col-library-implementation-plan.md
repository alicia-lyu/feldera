# Native COL library implementation plan

Status: implemented and verified on 2026-10-06.

Implement the accepted plan to keep general merged-index machinery in DBSP and
move Customer–Orders–Lineitem (COL) into a reusable standalone experiment library.

## Execution

1. Inspect the current implementation, scoped documentation, native SQL codecs,
   and standalone experiment configuration.
2. Expose general DBSP APIs and add metadata-driven mixed-width signed folding,
   including checked `i32` conversion from generic `i64` fields.
3. Relocate COL and its tests into `experiments/merged-index-col`, use native SQL
   payloads and checked serialization, and validate archived `Date`.
4. Update active design notes, type tables, source links, and test commands while
   preserving deferred scope and historical measurements.
5. Run focused general, COL, date, and two-column storage tests; formatting,
   Clippy, Markdown/link checks, and diff checks. Fetch and merge the tracking
   branch before each commit, staging only explicit files.

Query reconstruction, order-only lookup, join integration, recovery, measurements,
and persisted-file migration remain deferred. Q3 behavior remains unchanged.

## Implementation outcome

The general definition, folding/prefix operations, batch builder, and consolidated
cursor are public in DBSP. `PayloadBytes::from(Vec<u8>)` accepts client-serialized
bytes. Field widths and decode offsets come from domain metadata; generic `i64`
inputs undergo checked conversion for `I32` domains.

The standalone COL library owns its concrete definition, codecs, and tests. Its
identity types are `Customer(i64)`, `Orders(i64, i64)`, and
`Lineitem(i64, i64, i32)`, producing 11-, 20-, and 25-byte complete keys. Payloads
use `SqlString`, `Date`, `i32`, and the declared fixed-precision `SqlDecimal`
types. Archived `Date` now supports `CheckBytes` with its four-byte layout
unchanged; sqllib declares the derive's `bytecheck` dependency directly.

COL storage tests initialize a single-worker circuit through the public runtime
API and retain native batches, Spine compaction, signed updates, snapshots, and
file reopening. Prototype fixtures use the revised codecs. Existing generated
Q3 code was left unchanged.

## Verification

On 2026-10-06, DBSP's general merged-index suite passed all 10 tests, the standalone
COL suite passed all 14 tests, and the sqllib timestamp filter passed all 16 tests,
including checked Date archives. The eight two-column storage tests, nine Spine
threshold tests, runtime pressure test, and empty-builder policy test also passed.
The storage baselines ran directly against the compiled DBSP test binary while
Cargo builds shared its target directory; the filters below reproduce those checks.

Commands from the repository root:

```sh
cargo +1.96.1 test -p dbsp --lib trace::merged_index --offline
CARGO_TARGET_DIR="$PWD/target" cargo +1.96.1 test --manifest-path sql-to-dbsp-compiler/experiments/merged-index-col/Cargo.toml --offline
cargo +1.96.1 test -p feldera-sqllib --lib timestamp --offline
cargo +1.96.1 test -p dbsp --lib storage::file::test::two_columns --offline
cargo +1.96.1 test -p dbsp --lib trace::spine_async::merge_threshold_test --offline
cargo +1.96.1 test -p dbsp --lib circuit::runtime::tests::memory_pressure_thresholds_and_spill_behavior --offline
cargo +1.96.1 test -p dbsp --lib trace::test::an_empty_builder_stays_in_memory_under_a_zero_threshold --offline
cargo +1.96.1 fmt --all --check
cargo +1.96.1 fmt --manifest-path sql-to-dbsp-compiler/experiments/merged-index-col/Cargo.toml --check
cargo +1.96.1 clippy -p dbsp --lib --tests --offline
CARGO_TARGET_DIR="$PWD/target" cargo +1.96.1 clippy --manifest-path sql-to-dbsp-compiler/experiments/merged-index-col/Cargo.toml --offline --all-targets -- -D warnings
git diff --check
```

Rust formatting and COL Clippy with warnings denied passed. DBSP Clippy completed
with unrelated existing warnings and no diagnostics in changed merged-index code.
Markdown lint passed
across the ten affected documents, with existing long-line and inline-HTML
conventions retained. All 196 local link targets and source line bounds passed.
Historical measurements in earlier notes were preserved as pre-extraction results.
