# Paper contracts and reconstruction obligations

## Conclusion

**Semantically demonstrated:** A single Customer–Orders–Lineitem merged index can supply Q3's five logical
accumulated inputs from weighted base tuples at one complete batch boundary. It must expose the same rows,
tuple weights, and group existence that ordinary evaluation would produce in both the before and after states.
The paper's scan and co-location result supports an access layout; it does not establish a Feldera runtime
contract or a reduction in total maintenance I/O.

| Question | Finding | Status | Evidence |
| --- | --- | --- | --- |
| Which pipeline fits one index? | Customer `(c)`, Orders `(c,o)`, and extended Lineitem `(c,o,l)` form nested ranges. | Implemented in the LeanStore paper prototype; Feldera integration unverified | MI paper `main.tex:491-495,521-540` |
| What does Q3 maintain? | The complete unordered four-column result before sorting and limit. | Semantically demonstrated | DBSP note `dbsp-merged-index-feasibility.tex:69-98,156-186` |
| Which intermediate state must be supplied? | Count/Revenue summaries, `A`, eligible Orders, `B`, and eligible Customers; old delayed inputs are also required. | Semantically demonstrated | DBSP note `dbsp-merged-index-feasibility.tex:401-418,523-539` |
| Can one physical structure serve them? | Correctness permits reconstruction from weighted bases, provided all required ranges and old/new views are available. Physical cost and runtime integration remain open. | Proposed; physical performance unverified | DBSP note `dbsp-merged-index-feasibility.tex:508-579` |
| What do Q5 and Q10 add? | Both have the same order-sharing pipeline, but Q5 has later Supplier/Nation/Region joins; Q10's evaluated partial-aggregate option stores a per-order view. | Paper implemented; direct weighted-base reconstruction proposed | MI paper `sections/experiments_revised.tex:61-68,148-164`; `main.tex:749-760` |

Source IDs and checkout revisions are recorded in [source-map](../evidence/source-map.md). `MI paper` means
the local interesting-orderings manuscript; `DBSP note` means the supplied feasibility note. Line locators
refer to the checked-out source, including its input to the compiled figures.

## Legal domain and selected boundary

The present investigation uses valid primary and foreign keys, non-null query values, exact arithmetic, fixed
query parameters, and complete valid before and after batches. It starts with empty integrated state and an
initial load. Strict SQL primary-key endpoints have unit-weight rows. Multiplicity stress examples
additionally exercise the generalized bag algebra, with Customer and Orders still unique and unit-weight; they
are not claims that duplicate rows satisfy SQL primary-key uniqueness. A change is a signed, integer-weight
Z-set over **complete tuples**, including payload; a payload update is an old-tuple retraction and a new-tuple
insertion. These are the note's assumptions, not a claim that Feldera enforces them ([DBSP note
`:177-186,228-259,508-521`](../evidence/source-map.md)).

The physical hierarchy requires the Lineitem's owning customer key, which native TPC-H Lineitem lacks. The
source paper derives it through `orderkey → custkey`; the note treats consistent extended rows and their
reassignment deltas as **supplied by index maintenance**, outside the Q3 query circuit ([MI paper
`main.tex:473-478,491-495,521-535`](../evidence/source-map.md); [DBSP note
`:121-137`](../evidence/source-map.md)). An order reassignment must therefore move unchanged descendants from
the old customer prefix to the new one within the same batch.

| Case | Single-pipeline boundary examined | Work beyond this boundary |
| --- | --- | --- |
| Q3 | Filter extended Lineitem; group revenue per `(c,o)`; join eligible Orders on `(c,o)` and eligible Customers on `c`; project `(o,revenue,day,priority)`. | Final ordering and ten-row limit. |
| Q5 | `(C ⋈ RN) ⋈ (Oe ⋈ L)` over the same prefix chain; filtered Region/Nation gate `RN` is a supplied external input. | Constructing `RN`, Supplier join/check, final aggregation and ordering. The COL index cannot reconstruct `RN`. |
| Q10 | `C ⋈ (Oe ⋈ returned_L)`, retaining customer payload and returned-line contribution. | Per-customer final aggregation, Nation attachment, and ordering. The paper's **optional** per-order lost-revenue view is a distinct storage design, not assumed here. |

The Q3 boundary follows [DBSP note `:69-98,156-175`](../evidence/source-map.md) and [MI paper
`main.tex:749-757`](../evidence/source-map.md). The Q5/Q10 boundary follows [MI paper
`sections/experiments_revised.tex:61-68,148-164`](../evidence/source-map.md). The paper studies order-sharing
pipelines with prefix-compatible join and grouping keys; its general pipeline expression is at
`main.tex:168-175`. The targeted LeanStore experiment varies execution of only that subtree, keeping work
beyond it consistent (`sections/experiments_revised.tex:67-70`). For Q5, the selected subtree may consume an
external Region/Nation gate; that gate must be fixed or supplied consistently at both endpoints and is not
reconstructed by the COL index. The exact scoped outputs and full weighted Q5/Q10 derivations belong in [query
reconstruction](query-reconstruction.md).

## Exact reconstruction contract

Let `s ∈ {−,+}` denote the stable before and complete after state of one batch, and `M⁻ = base`, `M⁺ =
consolidate(base + pending)` for each typed source. Consolidation adds signed weights by complete tuple
identity and removes zero-weight tuples. For every requested key range `K` and pipeline intermediate `T`, the
obligation is:

```text
reconstruct(T, Mˢ, K) = T(DBˢ) restricted to K.
```

Equality includes tuple multiplicity and whether an aggregate group exists. Complete group members, predicate
and output payloads, consistent parent lookups, and paths from every changed input are required; a shared
prefix alone does not prove the contract ([DBSP note `:508-562`](../evidence/source-map.md)). A new/old bit
separate from weight is a **proposed** physical way to expose `M⁻` and `M⁺`; the cited semantic model instead
keeps a stable base plus pending deltas. Either implementation must preserve deleted old payloads and both
prefixes of a rekey until all consumers finish ([DBSP note `:514-521,575-579`](../evidence/source-map.md)).

For Q3, `Fˢ = σ(ship > date)Lˢ`. At each `(c,o)`, compute `Countˢ = Σ weight(l)` and `Revenueˢ = Σ weight(l) ×
price(l) × (1 − discount(l))` over `Fˢ`. Emit `Aˢ = {(c,o,Revenueˢ) ↦ 1 | Countˢ > 0}`; the count
distinguishes an absent group from a live zero-revenue group. Let `Oeˢ = σ(day < date)Oˢ`, `Ceˢ = σ(segment =
requested)Cˢ`, `Bˢ = Aˢ ⋈(c,o) Oeˢ`, and `Vˢ = π(o,revenue,day,priority)(Bˢ ⋈c Ceˢ)`. These are the five
logical accumulations, although the two old join partners and old grouping summary must also be observable at
the batch boundary ([DBSP note `:156-176,405-418,523-539`](../evidence/source-map.md)).

The required changes are relation-valued. Numeric summary addition alone cannot update an output tuple:

```text
ΔA(k) = [Count⁺(k) > 0] · {(k, Revenue⁺(k)) ↦ +1}
      − [Count⁻(k) > 0] · {(k, Revenue⁻(k)) ↦ +1}
ΔB = (ΔA ⋈ Oe⁻) + (A⁺ ⋈ ΔOe)
ΔW = (ΔB ⋈ Ce⁻) + (B⁺ ⋈ ΔCe)
ΔV = π(o,revenue,day,priority)(ΔW).
```

The two join branches are exact because weighted equijoin is bilinear: `Δ(R ⋈ S) = ΔR ⋈ S⁻ + R⁺ ⋈ ΔS`. The
second branch includes the simultaneous-change cross term once ([DBSP note
`:336-399,420-439,473-499`](../evidence/source-map.md)). In DBSP notation, `QΔ = D ∘ ↑Q ∘ I` defines the
output as `V⁺ − V⁻`; integration and delay describe logical state, not how it must be stored ([DBSP note
`:228-259`](../evidence/source-map.md)).

**Proposed affected-key algorithm:** collect order keys from changed extended lines and Orders, then enumerate
all Orders under each changed Customer in **both** states. Include both old and new order/customer prefixes
after reassignment. Deduplicate the keys; for each order, reconstruct its before and after contribution and
emit their weighted difference. An order outside this set has unchanged line group, parent fields, and
customer eligibility, so it contributes no difference ([DBSP note `:564-579`](../evidence/source-map.md);
semantic model `validation/check_q3.py:156-195,288-294`). Deduplication uses a support set: projecting signed
line changes to group keys can cancel during a price replacement even though the group must be read
([operator-state guide `operator-state.tex:39-57`](../evidence/source-map.md)).

## Evidence limits and follow-up

The supplied checker independently compares full native joins, aggregation-first evaluation, staged deltas,
and the base/pending reconstruction model. It covers initial load, empty and zero-valued groups, cancellation,
simultaneous source changes, deletion, changed-customer descendants, and reassignment with unchanged native
lines (`validation/check_q3.py:43-74,121-203,391-648`). This is **semantic evidence**, not a storage or
Feldera implementation. The broader operator-state guide shows why future `DISTINCT`, semijoin, outer join,
extrema, median, or top-k pipelines may require multiplicity counts or candidate bags beyond Q3's summaries
(`operator-state.tex:68-100,102-145,151-190,192-245`). Those operators are context, not new scope for the
proposed integration.

The paper's single-pass query scan and LeanStore latency results do not measure the union of before/after
reads across all DBSP integrators. Q3 may rescan all lines of an affected order for a one-line update; a
changed Customer can require all descendant orders. The paper itself notes that larger scans reduce the
per-seek advantage (`main.tex:673-685`), and the note calls for equal-memory, comparable-durability
measurements against the actual runtime (`dbsp-merged-index-feasibility.tex:581-611,657-682`). The concrete
correctness and I/O experiments are in [validation plan](../evidence/validation-plan.md).
