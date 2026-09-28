# Reconstructing the selected pipelines and their I/O

**Conclusion.** Weighted Customer, Orders, and extended Lineitem tuples suffice to reconstruct every
accumulated input in the selected Q3 and Q10 shapes, and Q5 with its explicitly supplied Nation/Region gate,
at a complete before/after batch boundary. This establishes a semantic route, not a drop-in replacement for
Feldera traces or an I/O improvement. The first prototype should use a manually composed Rust DBSP circuit
with an explicit evaluator boundary: reconstruct affected contributions before and after the batch and emit
their difference, bypassing the replaced stateful subcircuit.

| Finding | Status | Consequence |
| --- | --- | --- |
| Q3's five logical accumulations follow from complete weighted order ranges and parent rows. | Semantically demonstrated; supplied model checks pass | Reuse one traversal to derive summaries, group tuples, and joined tuples. |
| Q5 and Q10's selected COL joins have four accumulated join inputs each. | Analytical derivation; unexecuted | Reconstruct filtered sources and the order-line join; preserve all downstream payloads. |
| Q5's pushed Customer–Nation/Region gate is an external input. | Implemented visitor; proposed weighted boundary | Supply consistent old/new gate values and account for gate-induced affected customers. |
| Shared scans can reduce repeated reads across integrators and refresh processing. | Proposed | Measure both the union of blocks touched and actual reads after cache eviction. |
| Line changes can require complete order scans; customer changes and rekeys can touch many descendants. | Source-backed access requirement | Charge discovery, staging, reconstruction, and finalization to the same batch. |
| Beyond-memory benefit over unmodified Feldera. | Unverified | Compare equal memory budgets, matched predicates, and comparable durability. |

## Domain, notation, and output boundaries

The default domain has unit-weight primary-key rows, valid foreign keys, non-null query values, fixed
parameters, exact arithmetic, and valid complete before/after states. Changes are signed integer-weight Z-sets
over complete tuples; a replacement retracts the old payload and inserts the new one. Generalized bag stress
cases are labeled separately in the [worked examples](../evidence/weighted-examples.md). An intermediate delta
may be negative even though both endpoint databases are valid nonnegative bags. Native Lineitem's customer
prefix is derived from its order; maintaining that extension, including unchanged descendants during a rekey,
is part of index maintenance. See [paper contracts](paper-contracts.md).

For `s ∈ {−,+}`, let `Xˢ(x)` be the weight of complete typed tuple `x` in the stable base or consolidated
base-plus-pending view. Weights are stored directly; an independent phase bit distinguishes base from pending.
Write `ρ(l)` for `price(l) × (1 − discount(l))`, `k=(c,o)` for an order prefix, and `[P]` for a zero/one
predicate indicator. Filtering preserves weights; joining multiplies them; projection sums weights of
identical projected tuples. Summations below also consolidate identical output tuples and omit zero weights.

| Query | Exact selected output | Explicitly beyond this boundary |
| --- | --- | --- |
| Q3 | `V3 = π(o,revenue,day,priority)(B3 ⋈c C3)` using the aggregation-first shape below. | Sorting and ten-row limit. |
| Q5 | `J5 = C5 ⋈c B5`, where `C5` is Customer after the externally supplied Nation/Region gate and `B5 = O5 ⋈(c,o) L5`. Preserve customer nation, supplier key, line identity, price, discount, and needed order fields. | Computing the Nation/Region gate; Supplier-side matching; final nation aggregation and ordering. Physical visitor fusion does not move those dependencies into the COL index. |
| Q10 | `J10 = C10 ⋈c B10`, where `B10 = O10 ⋈(c,o) L10`. Preserve returned-line revenue inputs and Customer's name, balance, address, phone, comment, and nation key. | Per-customer aggregation, Nation-name attachment, and top twenty. No stored per-order partial aggregate is assumed. |

These are explicit subexpressions for a hand-composed circuit, not claims about an optimized SQL-generated
Feldera circuit. LeanStore's Q5 visitor applies the nation gate, order-date predicate, and supplier probe in
one walk; Q10's visitor directly accumulates per customer. The Q10 shared logical plan has no per-order
aggregate; its view-scan alternative and stored partial-aggregate variant are different shapes. Sources: [Q5
visitor](https://github.com/alicia-lyu/leanstore/blob/305ad0a98b147d048a37a1eba3787b35b1181b85/frontend/tpch/q5/query.tpp#L246-L335),
[Q10 logical
plan](https://github.com/alicia-lyu/leanstore/blob/305ad0a98b147d048a37a1eba3787b35b1181b85/frontend/tpch/q10/plans/family_logical.dot#L34-L69),
and [Q10
visitor](https://github.com/alicia-lyu/leanstore/blob/305ad0a98b147d048a37a1eba3787b35b1181b85/frontend/tpch/q10_family/visitor.hpp#L126-L200).

Use explicit fixed bounds `lo5, hi5, lo10, hi10` in the expressions. The inspected LeanStore code uses
`hi5=lo5+365` days and `hi10=lo10+90` days; these are not generally identical to SQL calendar intervals of one
year and three months. Any Feldera comparison must resolve and match the actual endpoints first. Sources: [Q5
date
gate](https://github.com/alicia-lyu/leanstore/blob/305ad0a98b147d048a37a1eba3787b35b1181b85/frontend/tpch/q5/query.tpp#L307-L315),
[Q10 date
gate](https://github.com/alicia-lyu/leanstore/blob/305ad0a98b147d048a37a1eba3787b35b1181b85/frontend/tpch/q10/query.tpp#L100-L107).

## Every selected accumulated input

For Q3 set `F3ˢ(l)=Lˢ(l)[ship(l)>d]`. The tuple identity of `A3` includes its revenue field; its weight is one
per nonempty group, not the line count.

| Q3 logical integrator | Exact reconstruction at state `s` | Required access |
| --- | --- | --- |
| Count/Revenue summary `H3ˢ(k)` | `(N3ˢ(k), R3ˢ(k)) = Σ(l:key(l)=k) F3ˢ(l) × (1,ρ(l))` | All qualifying line members of each requested order in that state. |
| Group relation `A3ˢ` | `Σ(k:N3ˢ(k)>0) {(k,R3ˢ(k)) ↦ 1}` | Derived from `H3ˢ`; no additional source read. |
| Eligible Orders `O3ˢ` | `O3ˢ(o)=Oˢ(o)[day(o)<d]` | Complete old/new parent payloads. |
| Joined relation `B3ˢ` | `A3ˢ ⋈(c,o) O3ˢ`; a matching pair has weight `A3ˢ(a) × O3ˢ(o)` | Reuse `A3ˢ` and `O3ˢ`. |
| Eligible Customers `C3ˢ` | `C3ˢ(c)=Cˢ(c)[segment(c)=requested]` | Old/new customer payloads. |

`V3ˢ` projects `B3ˢ ⋈c C3ˢ`; each joined tuple has weight `A3ˢ(a) × O3ˢ(o) × C3ˢ(c)`. A live zero-revenue
group remains present because `N3>0`; an empty group does not emit. Both versions of these expressions are
needed wherever a delayed branch reads old state. Five logical integrators therefore do not mean five stored
copies or only five stateful runtime objects. The [DBSP note's five-integrator
mapping](../../../../../../DBSP_w_merged_index/dbsp-merged-index-feasibility.tex#L405) and
[support/group-existence rule](../../../../../../DBSP_w_merged_index/operator-state.tex#L39) provide the supplied
semantic derivation.

For Q5 let `G5ˢ(n)` be the weight of nation key `n` in the externally computed, projected
Nation–selected-Region inner join. Under the strict key assumptions this weight is zero or one, matching the
visitor's membership gate. If the external relations are generalized to bags, an inner join requires `G5`'s
full multiplicity; a boolean membership test alone would lose it.

| Q5 logical integrator | Exact reconstruction at state `s` | Required access |
| --- | --- | --- |
| Customer-side input `C5ˢ` | `C5ˢ(c)=Cˢ(c) × G5ˢ(nation(c))` | Customer rows plus supplied `G5ˢ`; COL alone cannot derive the gate. |
| Eligible Orders `O5ˢ` | `O5ˢ(o)=Oˢ(o)[lo5≤day(o)<hi5]` | Parent rows and exact window bounds. |
| Line input `L5ˢ` | `L5ˢ(l)=Lˢ(l)` | Weighted line tuples including supplier and revenue fields. |
| Order-line relation `B5ˢ` | `O5ˢ ⋈(c,o) L5ˢ`; matching pair weight `O5ˢ(o) × L5ˢ(l)` | Complete requested order-line ranges and their parents. |

`J5ˢ` has matching-triple weight `Cˢ(c) × G5ˢ(nation(c)) × O5ˢ(o) × L5ˢ(l)`. The external Supplier check still
needs `(nation(c),supplier(l))`; replacing all lines by an undifferentiated order revenue would lose that
information. In an RF1/RF2-only experiment, `G5` can be fixed. If it changes, either supply all induced
changes to `C5` and expand affected descendants, or explicitly exclude that batch from this bounded
experiment. Sources: [Q5 logical
joins](https://github.com/alicia-lyu/leanstore/blob/305ad0a98b147d048a37a1eba3787b35b1181b85/frontend/tpch/q5/plans/family_logical.dot#L15-L68)
and [supplier-side
consumption](https://github.com/alicia-lyu/leanstore/blob/305ad0a98b147d048a37a1eba3787b35b1181b85/frontend/tpch/q5/query.tpp#L219-L242).

| Q10 logical integrator | Exact reconstruction at state `s` | Required access |
| --- | --- | --- |
| Customer input `C10ˢ` | `C10ˢ(c)=Cˢ(c)` | Full required Customer projection, including output payload. |
| Eligible Orders `O10ˢ` | `O10ˢ(o)=Oˢ(o)[lo10≤day(o)<hi10]` | Parent rows and exact window bounds. |
| Returned lines `L10ˢ` | `L10ˢ(l)=Lˢ(l)[returnflag(l)='R']` | Weighted returned-line tuples. |
| Order-line relation `B10ˢ` | `O10ˢ ⋈(c,o) L10ˢ`; matching pair weight `O10ˢ(o) × L10ˢ(l)` | Complete requested ranges with parent payloads. |

`J10ˢ` has matching-triple weight `Cˢ(c) × O10ˢ(o) × L10ˢ(l)`. The downstream sum must multiply `ρ(l)` by this
weight. There is no selected per-order or per-customer aggregate integrator in this boundary. Temporary
per-order summaries may optimize a separately chosen aggregation boundary, but they cannot replace `J10` if
its individual line tuples are required.

## Changes, affected keys, and complete batches

For every listed relation `T`, its integrated value is `Tˢ` above and its batch change is `ΔT=T⁺−T⁻`. Q3
summary changes add weighted `(count,revenue)` pairs, whereas group changes replace tuples:

```text
ΔH3(k) = Σ(l:key(l)=k) ΔF3(l) × (1,ρ(l))
ΔA3(k) = [N3⁺(k)>0] {(k,R3⁺(k)) ↦ +1}
        − [N3⁻(k)>0] {(k,R3⁻(k)) ↦ +1}
Δ(R⋈S) = ΔR⋈S⁻ + R⁺⋈ΔS
        = ΔR⋈S⁺ + R⁻⋈ΔS
```

Either join orientation counts the simultaneous-change term once. The supplied note uses old-right/new-left.
The inspected Feldera join uses left delta with current right and right delta with delayed left; adapt branch
input ordering when comparing. Applying old state on both branches misses the cross term; applying new state
on both double-counts it. Old/new views suffice for these nonrecursive complete-batch expressions. Adapting
existing trace-consuming operators additionally requires their cursor, snapshot, and scheduling contracts;
nested timestamp history is a separate generalization. See [runtime
reconstruction](runtime-reconstruction.md).

This proposed evaluator uses a **set of affected keys**, not a signed projection of deltas. `support(ΔX)`
below means complete tuples whose consolidated delta weight is nonzero; a price replacement has two such
tuples even when their projected key weights sum to zero.

```text
collect the complete input batch; keep the stable base available
derive consistent extended-line changes from old/new Orders
  (include unchanged descendants of every moved order)
stage all weighted tuples and parent-lookup changes; seal the pending view
U := empty set of native order identities
for x in support(ΔOrders) ∪ support(ΔextendedLineitem):
    U.add(native_order_identity(x))
K := customer identities in support(ΔCustomer)
K := K ∪ externally supplied affected customer identities for Q5 gate changes
for c in K:
    U.add_all(order identities below c in either the old or new view)
for o in U, sorted by the union of old/new physical prefixes:
    resolve every old/new customer-leading prefix of o
    reconstruct old and new order contributions using the tables above
    emit new contribution minus old contribution
consolidate identical emitted tuples, including across changed orders
after every consumer finishes, finalize the batch exactly once
```

For Q3 an order contribution is its projected group result; for Q5/Q10 it is the selected weighted joined line
rows. A line moved to another order marks both native orders. Customer identity changes mark both customer
identities; parent reassignments retain both physical prefixes. Q5 gate expansion needs an external way to
find affected customers by nation, or an explicitly charged Customer scan. An order outside `U` has unchanged
selected lines, parent payload/eligibility, and Customer-side input; its contribution cannot change. That
proves restriction to `U`. It does not prove that discovering or evaluating `U` is inexpensive. The [storage
lifecycle](merged-index-storage.md#batch-lifetime-and-visibility) owns publication, retained deletion
payloads, and finalization; no second lifecycle mechanism is implied here.

## Shared reads and total maintenance cost

Within one batch, coalesce overlapping requested prefixes and share each decoded tuple across its consumers.
One ordered base/pending traversal can derive old and new weights together. For Q3 it produces both summaries,
then both `A3` and `B3` values using the already decoded Orders/Customer rows. For Q5/Q10 it feeds the
filtered source inputs and order-line join from the same records. These are transient values for the current
range; no durable intermediate materialization is assumed. Reconstructing each integrator with an independent
full scan would forfeit much of the proposed benefit.

Same-type consumers need independent positions in shared decoded buffers, and independent ranges need
independent sessions. A lagging consumer pins data; bounded buffering may require coordinated consumption, an
explicitly charged spill/re-read, or rejection of an unsuitable schedule. It cannot silently skip rows.
Customer or order ranges need not fit in RAM. The dirty, pinned `mi_db` design specifies these sessions but
does not implement the proposed visibility protocol; see [source provenance](../evidence/source-map.md) and
[scan-session proposal](../../../../../../mi_db/docs/architecture.md#merged-index-scan-sessions).

Refresh processing belongs in the same access accounting:

| Operation | Shareable reads | Work that remains |
| --- | --- | --- |
| RF1: insert a new order and its lines | Known generated order/customer identity; one Customer read can serve validation and query eligibility. Sealed pending rows can feed new contributions. | Stage every weighted order/line tuple, maintain parent lookup, and fold writes. A no-old-order shortcut requires a verified fresh key. |
| RF2: remove an existing order and its lines | Persistent native-order lookup followed by its old COL range can supply deletion payloads, all old integrator inputs, and the old output contribution. | Retain full deleted payloads until all readers finish; stage negative weights and finalize. |
| Replace a line payload | Parent lookup and one shared old/new order range. | Recompute complete Q3 line summary; retract/insert complete tuple versions. |
| Change Customer payload or eligibility | One Customer version pair and the union of descendant order ranges. | Potentially all descendants; Q10 payload-only changes still replace selected joined tuples. |
| Reassign an order | One discovery of old descendants can serve rekey staging and old reconstruction. | Move every descendant to its new prefix, retain both paths, update parent lookup, reconstruct both sides. |

LeanStore's existing RF2 generator reads native Orders to find the customer and scans native Lineitem to
collect **line numbers**. Its COL helpers then erase records directly. Those functions demonstrate discovery
and key construction, not retained old payloads or scan sharing with incremental evaluation. Reusing those
reads requires preserving the full required payloads in a budgeted session; reading them again must be
charged. Sources: [RF2
discovery](https://github.com/alicia-lyu/leanstore/blob/305ad0a98b147d048a37a1eba3787b35b1181b85/frontend/tpch/tpch_family/refresh.hpp#L143-L178),
[COL maintenance
helpers](https://github.com/alicia-lyu/leanstore/blob/305ad0a98b147d048a37a1eba3787b35b1181b85/frontend/tpch/tpch_family/col_pipeline.tpp#L150-L220).

Let `P_i` be the physical block identities required by consumer `i`, including refresh discovery, parent
lookup, every examined integrator, and pending/fold processing. With a common storage block size `b`:

```text
unique footprint = b × |union_i P_i|
read traffic     = sum of bytes in actual physical read events
write traffic    = pending + fold + parent/access-path + journal/WAL
                   + recovery/checkpoint/background writes actually incurred
```

For variable block sizes, sum sizes over unique block identities instead. The unique footprint is a reuse
lower bound, not an actual read count: eviction can reread a block, while cached blocks can require no
physical read in the measured batch. Report cold/warm cache state, misses, repeat reads, records decoded,
seeks, temporary buffer/spill bytes, and elapsed time alongside it. Do not add per-integrator block counts and
call that total device I/O.

A line update can visit `Θ(f)` records for an order with `f` lines; a Customer change can visit the union of
all descendant ranges. A rekey writes both old and new placement deltas for unchanged descendants even when
the projected result cancels. Compare with Feldera's maintained indexed batches and actual root-circuit
trace/cache behavior. At root timestamp `()`, the timed-batch alias resolves to untimed batches; file-backed
`FileIndexedWSet` can support batched fetch. Do not assume the nested `FileValBatch` no-fetch path describes
this baseline. Record the actual fetch setting/path; see [index inventory](feldera-indexes.md). The proposal
does not require compaction-driven maintenance; ordinary background work of either chosen backend still counts
in measurements. The [validation plan](../evidence/validation-plan.md) defines the remaining correctness and
performance gates.
