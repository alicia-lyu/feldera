# Single-pipeline merged-index investigation plan

Date: 2026-09-28. Status: investigation in progress.

## Deliverable and boundaries

Produce a documentation-only feasibility recommendation with three reading levels:
overview, six technical summaries, and detailed source/semantic/validation evidence.
Preserve the prior plans in the sibling `DBSP_w_merged_index/plans/` directory.
The user-supplied plan is the authority for this investigation.

Use Q3 as the complete relational example and LeanStore's Q5/Q10 merged-index
pipeline portions as additional cases. Store weighted tuples directly and begin
with a separate per-record old/new visibility bit. Investigate reconstruction of
each examined integrator and shared transaction-level access, including RF1/RF2.
The performance target is beyond-memory, I/O-dominated execution, compared with
unmodified Feldera under equal memory and comparable durability settings.

Multi-pipeline design, Q9, and compaction-driven maintenance are excluded. Do not
add runtime code, diagnostics, or public APIs. Existing semantic models may run.

## Execution

1. Inventory existing manuscripts, operator-state guide, plans, semantic models,
   source revisions, and relevant repository guidance.
2. Run paper-contract, Feldera-index, and merged-storage investigations concurrently.
3. Release paper-contract slot to compiler research. Once index and storage
   handoffs arrive, run runtime reconstruction and query/I/O research concurrently.
4. Check agreement on pipeline boundaries, old/new snapshots, weighted tuple
   identity, affected keys, state ownership, and finalization.
5. Synthesize the integration recommendation and blockers in a short overview.
6. Run existing semantic validation; check citations, Markdown, links, diagrams,
   formulas, scope, and cross-report consistency. Record unverified claims and
   concrete follow-up experiments.
7. Fetch and merge the tracking branch before each explicit-file documentation
   commit. The coordinator serializes commits to avoid shared-index races.

## Acceptance checklist

- Overview exposes architecture, recommendation, confidence, and blockers in
  approximately 600 words or fewer, excluding diagrams and tables.
- Every examined logical integrator has a weighted-base reconstruction expression.
- Examples cover multiplicity, negative deltas, cancellation, simultaneous changes,
  deletion, empty groups, parent changes, and rekeying using exact arithmetic.
- Storage report includes batch lifecycle and visibility/read/finalization trace.
- Source map pins inspected revisions and marks dirty or documentation-only inputs.
- Baseline and integration-point inventories are backed by actual source symbols.
- Future tests cover correctness, storage conformance, compiler shape, and physical
  I/O; no semantic result is presented as a performance or durability measurement.

## Evidence vocabulary

**Implemented** means present in the cited source, not necessarily executed here.
**Semantically demonstrated** means shown algebraically or by the cited semantic
model, not integrated into Feldera. **Proposed** means a design to implement.
**Unverified** means a claim still needs the identified experiment or contract.
