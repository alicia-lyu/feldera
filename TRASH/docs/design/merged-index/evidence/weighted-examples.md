# Exact weighted examples

These are worked calculations for the [selected expressions](../reports/query-reconstruction.md), not new
executed tests. The existing supplied Q3 and operator suites passed as recorded in
[source-map](source-map.md#executed-semantic-checks). Q5/Q10 calculations below are analytical only. All
arithmetic is integer or rational.

## Strict-key Q3 fixture

Use date threshold `d=10` and requested segment `B`. Omitted fields stay fixed; every listed complete tuple
initially has weight `+1`.

| Type | Identity and relevant payload |
| --- | --- |
| Customer | `C1=(c=1,segment=B)`; `C2=(c=2,segment=X)` |
| Orders | `O10=(o=10,c=1,day=5,priority=0)` |
| Lineitem | `L1=(o=10,line=1,price=60,discount=0,ship=11)` |
| Lineitem | `L2=(o=10,line=2,price=40,discount=0,ship=12)` |

The derived physical line keys are `(1,10,1)` and `(1,10,2)`. `H3⁻(1,10)=(2,100)`, `A3⁻={(1,10,100)↦1}`, and
`V3⁻={(10,100,5,0)↦1}`. Write `v(r,d,p)=(10,r,d,p)` below. Each row of the next table is an independent valid
before/after batch starting from this fixture, unless explicitly stated otherwise. A replacement means `−1`
for the complete old tuple and `+1` for the complete new tuple, with the same primary key permitted only
because the old tuple is retracted.

| Batch | New Count/Revenue | Exact consolidated output delta |
| --- | --- | --- |
| Insert fresh line 3 with price 30, discount 0, ship 11. | `(3,130)` | `−{v(100,5,0)↦1}+{v(130,5,0)↦1}` |
| Delete `L2`. | `(1,60)` | `−{v(100,5,0)↦1}+{v(60,5,0)↦1}` |
| Replace `L1`'s discount by `1/2`. | `(2,70)` | `−{v(100,5,0)↦1}+{v(70,5,0)↦1}` |
| Replace `L2`'s ship day by 10. | `(1,60)` | Same delta as deleting `L2`; the stored row remains but fails the strict predicate. |
| Delete both lines, keeping the order. | `(0,0)` | `−{v(100,5,0)↦1}`; no zero-revenue row is inserted. |
| Delete both lines and insert fresh line 3 with zero price and ship 11. | `(1,0)` | `−{v(100,5,0)↦1}+{v(0,5,0)↦1}` |
| Replace `O10`'s day by 10. | `(2,100)` | `−{v(100,5,0)↦1}`; line group exists but order is ineligible. |
| Replace `O10`'s day/priority by `(6,2)`. | `(2,100)` | `−{v(100,5,0)↦1}+{v(100,6,2)↦1}` |

A line summary update of `(+1,+30)` is a numeric change. Emitting an aggregate tuple with revenue 30 would be
wrong: the relation needs to retract revenue 100 and insert revenue 130, each with weight one. The count
distinguishes deletion of the last line from a surviving zero-valued group.

## Support, cancellation, and simultaneous changes

Replace `L1`'s price 60 by 90. Projecting its delta onto order key gives `−{(1,10)↦1}+{(1,10)↦1}=0`, but the
revenue changes from 100 to 130. The set of keys in the support of the **complete-tuple delta** still contains
`(1,10)` and forces reconstruction. Giving each raw changed line weight to the affected-key join is also
wrong: it can cancel or multiply the one group update.

Replace prices `(60,40)` by `(50,50)` in one batch. Both complete line tuples change; `H3⁺=(2,100)` remains
equal to `H3⁻`. The old and new aggregate tuples cancel, so `ΔA3=ΔV3=0`. Index maintenance still replaces both
payloads. In contrast, receiving `+L3` and `−L3` for the same complete fresh line tuple in one batch
consolidates to zero **source** delta. It needs no affected key after consolidation; any writes already
performed before consolidation still count.

For simultaneous changes, replace `L2`'s price 40 by 70, replace `O10`'s priority 0 by 2, and change `C1` from
segment B to X in the same complete batch. Let `b(r,p)` be the joined `(customer,order,revenue,day,priority)`
tuple.

```text
A3⁻ = {(1,10,100)↦1}           A3⁺ = {(1,10,130)↦1}
ΔB3 = −{b(100,0)↦1} + {b(130,2)↦1}
C3⁻ = {C1_B↦1}                C3⁺ = empty
ΔC3 = −{C1_B↦1}

ΔB3⋈C3⁻        = −{v(100,5,0)↦1} + {v(130,5,2)↦1}
B3⁺⋈ΔC3        =                         −{v(130,5,2)↦1}
ΔV3            = −{v(100,5,0)↦1}
```

The alternative orientation used by the inspected Feldera join gives `ΔB3⋈C3⁺=0` and
`B3⁻⋈ΔC3=−{v(100,5,0)↦1}`. Both are correct. Using old state for both branches would leave a spurious
intermediate result.

## Parent changes, rekeying, and refresh batches

Move `O10` from Customer 1 to Customer 2 while leaving its native lines unchanged. Maintenance retracts the
order and both extended lines under prefix `(1,10)` and inserts them under `(2,10)`. The source-plan aggregate
changes from `{(1,10,100)↦1}` to `{(2,10,100)↦1}`. Because Customer 2 is in segment X, the Q3 output delta is
`−{v(100,5,0)↦1}`.

If the same batch also changes Customer 2 to B, the old and new projected results are both `{v(100,5,0)↦1}`.
The output delta is zero, although every order/line placement changed. Both prefixes and the old payloads must
remain readable until the batch is consumed. This is the cancellation case that an output-only maintenance
metric would miss. The [storage trace](../reports/merged-index-storage.md#batch-lifetime-and-visibility) shows
phase/weight visibility for a related rekey with a line replacement.

A Customer change must visit unchanged orders too. Add `O11` under Customer 1 with one qualifying line worth
20. Changing Customer 1 from B to X then retracts both `{(10,100,5,0)↦1}` and `{(11,20,5,0)↦1}` even with no
Orders or Lineitem source deltas. Changing a customer's primary identity is more extensive: a valid
after-state must also update referencing Orders and derived line prefixes.

An RF1-style complete batch starts with existing Customer 1 but no order 10; it inserts `O10`, `L1`, and `L2`
together. The old contribution is empty and the new contribution is `{v(100,5,0)↦1}`. An RF2-style batch
removes this whole order group and produces the opposite delta. The sealed before/after states are valid even
if a staging sequence temporarily contains an order without its lines, or lines pending deletion. No consumer
evaluates a partial batch.

## Multiplicity with valid primary keys

Start from the strict-key fixture, replacing `L2` by a line worth 60 and adding fresh line 3 worth 60. The
three full line tuples have different line numbers, unit weights, and the same owning order; all
primary/foreign keys remain valid. A bag projection to `(customer,order,revenue)` consolidates them into
`{(1,10,60)↦3}`. Joining that projected relation with the unique eligible Order and Customer preserves weight
`3`; weighted summation gives revenue `3×60=180`. Retracting two of the distinct full line tuples yields
projected delta `{(1,10,60)↦−2}`, new projected weight `1`, and revenue 60. The Q3 aggregate output change is
`−{v(180,5,0)↦1}+{v(60,5,0)↦1}`.

This projection is only a small arithmetic demonstration; the stored full line identities and Q5/Q10 boundary
payloads remain available. It exhibits positive multiplicity, a negative weight of magnitude greater than one,
and consolidation without violating source primary keys. The supplemental cases below explore a broader
expression domain and are not part of the strict-key workload claim.

## Supplemental bag algebra: outside strict SQL primary keys

Duplicate complete tuples with weights above one violate a SQL primary-key constraint if interpreted as
physical duplicate source rows. The following cases therefore test the **weighted expression domain**, not
strict TPC-H endpoint legality. The derived owning order/customer remains unambiguous; all committed weights
remain nonnegative. Negative weights occur only in changes.

For Q3 keep Customer and Orders at unit weight, but assign the complete line tuple `L1` weight 3 and `L2`
weight 2. Then:

```text
N3⁻ = 3+2 = 5              R3⁻ = 3×60 + 2×40 = 260
ΔL  = {L1↦−2}
N3⁺ = 1+2 = 3              R3⁺ = 1×60 + 2×40 = 140
ΔA3 = −{(1,10,260)↦1} + {(1,10,140)↦1}
ΔV3 = −{v(260,5,0)↦1} + {v(140,5,0)↦1}
```

The aggregate emits weight one, not five or three. If instead several distinct primary-key lines share price
60, they remain distinct complete tuples and are a legal strict-key fixture with the same arithmetic.

For Q5/Q10, take one matching Customer tuple of weight 2, one eligible Orders tuple of weight 3, and one
admitted Lineitem tuple of weight 4. Fix `G5=1` and use a returned line for Q10. The order-line pair has
weight `3×4=12`; the selected COL output tuple has weight `2×3×4=24`. Retracting one line copy changes that
output weight to 18, a delta of `−6`. For `ρ(l)=25/2`, its downstream weighted revenue contribution changes
from 300 to 225.

Now simultaneously increase the Orders weight from 3 to 4 and decrease the line weight from 4 to 3. The joined
weight remains 12 and the COL weight remains 24:

```text
Δ(O⋈L) = ΔO⋈L⁻ + O⁺⋈ΔL = 1×4 + 4×(−1) = 0
```

Multiplication is essential; presence-only logic misses both nonzero weights and their cancellation. Extending
parent multiplicities in **Q3** needs extra care: the selected aggregation-first source expression would emit
an aggregate tuple with multiplied output weight, while native join-then-group SQL would sum those parent
copies into its revenue field. Those are not generally equal. The Q3 SQL-equivalence claim therefore retains
unit-weight parent keys; the Q5/Q10 example does not relax that claim silently.

## Q5 and Q10 boundary-specific checks

For Q5 use fixed `G5(7)=G5(9)=1`, an eligible order, one unit-weight customer whose nation is 7, and a
unit-weight line with supplier 70 and revenue 30. Changing the customer's nation to 9 retracts the selected
`J5` tuple carrying `nation=7` and inserts the tuple carrying `nation=9`. If the external supplier input
contains only `(nation=7,supplier=70)`, the supplier check admits the old tuple and rejects the new one.
Omitting either nation or supplier from the COL boundary would make this result impossible to reconstruct
correctly.

With Customer and Orders unchanged, changing the supplied gate from `G5(7)=1` to `G5(7)=0` retracts all
selected rows of every customer in nation 7. Source-COL support alone is empty in this case. Correctness
requires externally supplied affected customers (or a charged discovery scan); an RF1/RF2-only run instead
holds this side input fixed.

For Q10 use Customer 1, one eligible unit-weight order, a returned line worth 60, and a nonreturned line worth
40. `J10⁻` contains only the returned line. Replacing the second line's flag by R inserts its selected joined
tuple with weight one. An external per-customer sum consequently changes from 60 to 100; no per-order
aggregate is stored or counted as a selected integrator here. Retracting both returned lines makes `J10`
empty; a returned zero-price line would leave one selected tuple and a live downstream zero-sum group.

Replacing Customer 1's comment without changing any amount replaces **every** selected `J10` tuple's customer
payload. A revenue-only comparison would miss the change to the downstream result fields. Finally, both date
windows admit an order exactly at `lo` and reject one exactly at `hi`; use the explicit resolved bounds rather
than assuming 90 days means three calendar months.
