# Generated native state-read contracts (R2)

Inspected on 2026-10-06 at Feldera revision
`308950de73a28bb014c8ad002365d2aa64f1f152`. The source links below refer to the
generated program and runtime inspected at that revision. The generated files
were tracked at that revision; they are now Git-ignored. Run
[generate.sh](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/generate.sh)
before following their local source links. The
[native circuit note](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/CIRCUIT.md#reading-the-generated-program)
records the generated topology; this document binds its accumulated-state reads.
R1–R5 name the [README roadmap](README.md#next-steps). The older storage Step 2
and its Phase numbering retain their meaning.

R2 documents existing behavior and observed compatibility gaps. R3 owns
reconstruction and runtime integration, including decisions about adapters,
ordering strategies, memory policies, and recovery. No such implementation is
claimed here. Maintenance targets one DBSP worker; initial loading may use
multiple workers under the [worker policy](README.md).

## Relations and state notation

`O`, `C`, and `L` denote logical Orders, Customer, and Lineitem. Following the
[storage design notation](folded-key-layer-file-plan.md#compatibility-with-feldera-operators),
for any relation `X`:

- `X⁻` is accumulated state immediately before the current update set.
- `X⁺` is accumulated state after applying that update set.
- `ΔX = X⁺ − X⁻`, so `X⁺ = X⁻ + ΔX`.

The superscripts identify states, **not negative and positive tuple weights**.
Unchanged tuples belong to both states. Insertions, deletions, and replacements
contribute through the signed relation `ΔX`; addition consolidates weights of
identical complete tuples.

`B` names the actual generated Orders–Customer intermediate, stream
[s6](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/src/lib.rs#L487).
It is an order-keyed projection of `O ⋈ C`, retaining customer ID, segment,
order date, and ship priority. It is not a named SQL view. `V` is the
materialized nine-field Customer–Orders–Lineitem join generated as
[s7](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/src/lib.rs#L503).
Neither join applies Q3's residual filters or aggregation.

## Generated bindings and equations

The first generated call is `s3.join_index_balanced(&s4)`; the second is
`s5.join_balanced(&s6)`. Thus Orders is the first join's left input, and
Lineitem is the second join's left input. Each row below identifies the
accumulated operand read by a delta branch, rather than a separate input delta.

| Delta input | Accumulated state read | Lookup key | Generated stream and runtime wiring |
| --- | --- | --- | --- |
| `ΔO` (`s3`) | `C⁺` (`s4` integral) | Customer ID | [s3/s4 join](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/src/lib.rs#L487); [left delta with current right trace](../../../crates/dbsp/src/operator/dynamic/join.rs#L714). |
| `ΔC` (`s4`) | `O⁻` (`s3` integral) | Customer ID | [s3/s4 join](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/src/lib.rs#L487); [right delta with delayed left trace](../../../crates/dbsp/src/operator/dynamic/join.rs#L731). |
| `ΔL` (`s5`) | `B⁺` (`s6` integral) | Order ID | [s5/s6 join](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/src/lib.rs#L518); [left delta with current right trace](../../../crates/dbsp/src/operator/dynamic/join.rs#L714). |
| `ΔB` (`s6`) | `L⁻` (`s5` integral) | Order ID | [s5/s6 join](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/src/lib.rs#L518); [right delta with delayed left trace](../../../crates/dbsp/src/operator/dynamic/join.rs#L731). |

These bindings give the generated equations:

```text
ΔB = ΔO ⋈ C⁺ + O⁻ ⋈ ΔC
ΔV = ΔL ⋈ B⁺ + L⁻ ⋈ ΔB
V⁺ = V⁻ + ΔV
```

The first equality expands to
`ΔO ⋈ C⁻ + ΔO ⋈ ΔC + O⁻ ⋈ ΔC`; the simultaneous-change term occurs once.
The same reasoning applies to the second join. The runtime
[creates each input trace](../../../crates/dbsp/src/operator/dynamic/join.rs#L705),
wires the two branches as cited above, and [adds their outputs](../../../crates/dbsp/src/operator/dynamic/join.rs#L748).
The [delayed-trace accessor](../../../crates/dbsp/src/operator/dynamic/accumulate_trace.rs#L773)
uses the cached prior-cycle trace; its
[snapshot operator](../../../crates/dbsp/src/operator/dynamic/accumulate_trace.rs#L1416)
captures that trace on initialization or on evaluation after the runtime's
notification that transaction inputs are complete (`Operator::flush`). A delayed snapshot is a
read of existing accumulation, not a second accumulation of the input.

The generated `*_balanced` wrappers use ordinary joins when adaptive joins are
disabled, as in the native baseline: see
[join_balanced](../../../crates/dbsp/src/mono.rs#L181),
[join_index_balanced](../../../crates/dbsp/src/mono.rs#L485), and the
[default setting](../../../crates/feldera-types/src/config/dev_tweaks.rs#L335).
This contract describes that configuration, without extending it to adaptive
or multiworker maintenance.

## Concrete keyed values

The generated stream declarations and projection closures establish these
types and field orders. Each indexed collection associates a signed
[i64 weight](../../../crates/dbsp/src/algebra/zset.rs#L39) with a complete
`(key, value)` pair; the superscript selects visibility, not the weight's sign.

| Read | Concrete key type | Concrete value type | Value fields in order | Generated source |
| --- | --- | --- | --- | --- |
| `C⁺` | `Tup1<i64>` | `Tup1<SqlString>` | Market segment | [s4](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/src/lib.rs#L456) |
| `O⁻` | `Tup1<i64>` | `Tup3<i64, Date, i32>` | Order ID, order date, ship priority | [s3](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/src/lib.rs#L442) |
| `B⁺` | `Tup1<i64>` | `Tup4<i64, SqlString, Date, i32>` | Customer ID, market segment, order date, ship priority | [s6](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/src/lib.rs#L487) |
| `L⁻` | `Tup1<i64>` | `Tup4<i32, SqlDecimal<15, 2>, SqlDecimal<4, 2>, Date>` | Line number, extended price, discount, ship date | [s5](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/src/lib.rs#L465) |

The keys are customer ID for `C⁺` and `O⁻`, and order ID for `B⁺` and `L⁻`.
The logical Lineitem identity is `(order ID, line number)`; its generated
indexed value retains the line number. Its schema has no customer ID.

The output [s7 declaration and closure](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/src/lib.rs#L503)
use `WSet<Tup9<i64, SqlString, i64, Date, i32, i32, SqlDecimal<15, 2>, SqlDecimal<4, 2>, Date>>`.
The fields are customer ID, segment, order ID, order date, ship priority,
line number, extended price, discount, and ship date, in that order. The
[materialized output registration](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/src/lib.rs#L630)
exposes `V` through Catalog. Base input maps and materialized `V` are outside
the four accumulated join-read bindings above.

## Existing snapshot, fetch, and cursor behavior

All four bindings use the same `JoinTrace` implementation with their respective
types and current or delayed trace. The following operations are confirmed in
the inspected source:

1. [JoinTrace::eval](../../../crates/dbsp/src/operator/dynamic/join.rs#L1633)
   takes a read-only snapshot of an arriving delta and retains it until
   [Operator::flush](../../../crates/dbsp/src/circuit/operator_traits.rs#L349)
   notifies the operator that all predecessors have produced their outputs
   for the current transaction. The
   [join's notification handler](../../../crates/dbsp/src/operator/dynamic/join.rs#L1454)
   sets a flag; the following evaluation snapshots the supplied accumulated
   trace and processes the retained delta. That trace's wiring determines
   whether it represents the before or after state. Here `flush` denotes
   transaction readiness, not a memory-limit flush to storage or a prerequisite
   for taking a snapshot.
2. [WithSnapshot](../../../crates/dbsp/src/trace/spine_async/snapshot.rs#L23)
   defines read-only, non-merging snapshots. A
   [SpineSnapshot](../../../crates/dbsp/src/trace/spine_async/snapshot.rs#L55)
   owns `Vec<Arc<B>>`; acquisition from a
   [Spine](../../../crates/dbsp/src/trace/spine_async/snapshot.rs#L170)
   obtains batch references. Its
   [cursor](../../../crates/dbsp/src/trace/spine_async/snapshot.rs#L213)
   retains cloned batch references. Both memory-backed and file-backed batches
   can be retained; snapshot acquisition does not require spilling in-memory
   batches. The existing
   [storage test](../../../crates/dbsp/src/trace/merged_index/storage_tests.rs#L216)
   preserves a memory-backed snapshot across compaction and later checks a
   [snapshot containing both batch locations](../../../crates/dbsp/src/trace/merged_index/storage_tests.rs#L321).
   Reading that view does not append updates or merge its batches. These
   snapshot and notification operations do not define a memory-limit policy.
3. When `fetch_join == Some(true)`, the join invokes
   [trace.fetch(&delta)](../../../crates/dbsp/src/operator/dynamic/join.rs#L1661).
   [SpineSnapshot::fetch](../../../crates/dbsp/src/trace/spine_async/snapshot.rs#L252)
   delegates to `FetchList`, which concurrently fetches each batch and keeps
   batch cursors for batches without a specialized fetch result. The join
   uses the fetched cursor when present, otherwise the snapshot's full cursor.
4. [JointKeyCursor](../../../crates/dbsp/src/operator/dynamic/join.rs#L1572)
   enumerates one side's keys and calls `seek_key_exact` on the other. For
   these inner joins, fetched state makes the delta drive; otherwise
   [estimated key counts](../../../crates/dbsp/src/operator/dynamic/join.rs#L1667)
   choose which side drives. Missing keys produce no joined rows.
5. The [cursor interface](../../../crates/dbsp/src/trace/cursor.rs#L42)
   visits ordered keys, then ordered values within each key, and returns
   borrowed key/value references. The root-time
   [join loop](../../../crates/dbsp/src/operator/dynamic/join.rs#L1809)
   reads `key_valid`/`val_valid`, `key`/`val`, delta `weight` and trace
   `weight_checked`, enumerates matching value pairs with `step_val`, rewinds
   trace values with `rewind_vals`, and advances with `step_key`.
   It multiplies the signed weights and sends resulting tuples to the native
   batcher for consolidation. The
   [unit-time CursorList](../../../crates/dbsp/src/trace/cursor/cursor_list.rs#L150)
   sums matching values' contributions across batches and skips zero totals.

Root-circuit [time is `()`](../../../crates/dbsp/src/time.rs#L214).
Before/after states describe logical update visibility, not additional runtime
timestamps or merged-index record versions. Fetch is conditional; a binding
does not imply that its delta always drives a selective lookup.

## Worked signed examples

All examples use unit source weights and matching identities: customer 7,
order 10 owned by customer 7, and line 1 of order 10. Unmentioned payload
fields are fixed. `{+x}` means tuple `x` with weight `+1`; `{-x}` means weight
`−1`. Braces denote weighted relations, not before/after states. Each example
is a separate update set.

### Empty bootstrap

Start with `O⁻ = C⁻ = L⁻ = B⁻ = V⁻ = ∅` and supply
`ΔO = {+o}`, `ΔC = {+c}`, and `ΔL = {+l}` together.
The resulting states have `C⁺ = {+c}` and `B⁺ = {+b}`, where `b` is the
generated order-keyed projection of the matching order/customer pair.

```text
ΔB = {+o} ⋈ {+c} + ∅ ⋈ {+c} = {+b}
ΔV = {+l} ⋈ {+b} + ∅ ⋈ {+b} = {+v}
V⁺ = ∅ + {+v} = {+v}
```

Every row enters through the native delta ports; no accumulated bootstrap
relation is supplied as a second delta.

### Simultaneous customer and order deletion

Start with `O⁻ = {+o}`, `C⁻ = {+c}`, `L⁻ = {+l}`, `B⁻ = {+b}`,
and `V⁻ = {+v}`. Supply `ΔO = {-o}`, `ΔC = {-c}`, and `ΔL = ∅`.
Now `C⁺ = B⁺ = ∅` while the unchanged line belongs to both `L⁻` and `L⁺`.

```text
ΔB = {-o} ⋈ ∅ + {+o} ⋈ {-c} = {-b}
ΔV = ∅ ⋈ ∅ + {+l} ⋈ {-b} = {-v}
V⁺ = {+v} + {-v} = ∅
```

Reading `C⁻` on the first branch would instead count the deletion twice.
The correct expanded first join has weights `−1 − 1 + 1 = −1`.

### Simultaneous payload replacement at unchanged keys

Start with customer `c_old` in BUILDING and line `l_old` priced at 100.00,
with the unchanged order `o`. Replace the customer segment with AUTOMOBILE
and the line price with 120.00 in the same update set:
`ΔC = {-c_old, +c_new}`, `ΔL = {-l_old, +l_new}`, and `ΔO = ∅`.
Thus `ΔB = {-b_old, +b_new}` and `B⁺ = {+b_new}`. Let `v(l,b)` denote
the generated nine-field output for a matching line/intermediate pair.

```text
ΔL ⋈ B⁺ = {-v(l_old,b_new), +v(l_new,b_new)}
L⁻ ⋈ ΔB = {-v(l_old,b_old), +v(l_old,b_new)}
ΔV       = {-v(l_old,b_old), +v(l_new,b_new)}
```

The mixed old-line/new-segment tuple cancels. `V⁺` contains the new segment
and price; no BUILDING filter runs in this circuit. The unchanged order
belongs to both `O⁻` and `O⁺`. Projecting either replacement delta to its
unchanged identity and summing would yield zero, although its payload changes.

## Observed prototype gaps and R3 handoff

The [storage Step 2 implementation evidence](phase-4-implementation-note.md)
establishes folded weighted batches and native snapshot reads. It does not
establish these generated typed relations or their runtime substitution.

| Observed boundary | Native request | Current storage prototype evidence |
| --- | --- | --- |
| Key placement | `B⁺` and `L⁻` request order-ID keys; `O⁻` requests customer-ID keys. | [Source key fields](../../../crates/dbsp/src/trace/merged_index/customer_orders_lineitem.rs#L17) place Customer at `(c)`, Orders at `(c,o)`, and extended Lineitem at `(c,o,l)`. [Order/line prefix helpers](../../../crates/dbsp/src/trace/merged_index/customer_orders_lineitem.rs#L203) require customer ID as well as order ID. |
| Identity widths | Customer and order IDs are `i64`; line number is `i32`. | [Key domains and SourceKey](../../../crates/dbsp/src/trace/merged_index/customer_orders_lineitem.rs#L46) use `i32` for all three identity domains. |
| Payload representation | Values use `SqlString`, `Date`, and fixed-precision `SqlDecimal` types as listed above. | [Prototype payloads](../../../crates/dbsp/src/trace/merged_index/customer_orders_lineitem.rs#L143) use `String`, `i32` day fields, and `i64` price-cents/discount-hundredths fields. |
| Cursor representation | Each join reads its typed key/value relation, including the joined `B` projection. | [MergedIndexBatch and batch_cursor](../../../crates/dbsp/src/trace/merged_index/batch.rs#L16) store folded `Vec<u8>` keys and serialized `Vec<u8>` payloads through dynamic types and expose consolidated source rows. |
| Logical versus physical Lineitem | `s5` contains order ID and line number, with no customer ID. | [LineitemByCustomer](../../../crates/dbsp/src/trace/merged_index/customer_orders_lineitem.rs#L71) stores `ExtendedLineitem` under a customer-leading key. Its producer and related placement changes remain separate work under the [storage contract](folded-key-layer-file-plan.md#contract-and-references). |
| Runtime integration | The generated joins build native retained traces and read current/delayed snapshots. | The [generated calls](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/src/lib.rs#L487) do not instantiate the merged-index prototype or redirect any accumulated read to it. |

R3 must investigate these compatibility boundaries and implement the four
bindings with the documented tuples, signed weights, key/value order, absence,
and before/after visibility. It must identify the actual trace owners and
consumers so redirecting selected reads also stops their independent
accumulation. Base input storage, the secondary merged index, materialized `V`,
and unreplaced state remain separately accounted for. This document selects no
remedy for the observed gaps; reconstruction, integration, and lifecycle
implementation decisions belong to R3. R4 owns the broader correctness
verification, and R5 owns maintenance measurements.
