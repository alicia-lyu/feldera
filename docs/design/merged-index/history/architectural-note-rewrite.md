# Architectural note rewrite

Date: 2026-09-28. Scope: documentation only. Status: complete.

## Accepted plan

The user supplied the implementation plan. Write a 1,500–2,000-word note with four sections: existing storage,
the chosen design, Q3 reconstruction, and I/O tradeoffs. Preserve operator algorithms and reuse Feldera's
storage backend. Retain only directly referenced supporting evidence. Archive superseded reports and keep
planning/orchestration outside the reading path.

## Execution and decisions

1. Review every report and evidence section, with parallel read-only source audits.
2. Replace the overview with the main note and consolidate necessary evidence into `../support.md`.
3. Archive all six reports, three evidence documents, and the old overview under `TRASH/`.
4. Keep the original investigation plan and orchestration record here as historical records.
5. Check source claims, anchors, diagrams, Markdown, weighted examples, and existing semantic tests.
6. Fetch and merge the tracking branch, then commit the documentation with explicit file staging.

The earlier replacement-evaluator recommendation is superseded. The selected architecture reconstructs
accumulated-state access, including aggregate output's old-value access. Immutable snapshots replace the
previous proposed physical phase-bit/fold scheme; publication and recovery still require implementation.

## Verification

The existing Q3 model passed 12 methods and the operator model passed 8. These demonstrate model semantics,
not this runtime integration or I/O savings. No runtime code or new tests were added.

The main note has approximately 1,900 words and four sections. Both Mermaid diagrams parsed; all 36 active
links and source line ranges passed checks, and all seven supporting headings are referenced from the main
note. Markdown lint and `git diff --check` passed. A second source review checked the example, aggregate
output access, and immutable snapshot lifecycle. The tracking branch was fetched and merged before commit
(already up to date). Historical reports are preserved outside the active reading path.

## Terminology refinement

On 2026-09-28, the user requested definitions beside the first occurrence of nonstandard terminology and
an LSM description restricted to differences from RocksDB. Replaced the mixed logical/physical table with
an update-semantics and storage-representation comparison, defined specialized terms inline in both active
documents, and added official RocksDB sources. General LSM mechanics are assumed knowledge.

## Follow-up clarifications

On 2026-09-28, the user requested that follow-up questions trigger documentation edits that anticipate those
questions. Apply this to subsequent revisions of this note, rather than leaving clarifications only in chat.
Added the one-to-many search-key mapping, per-file-batch index scope, and the distinction between nested
storage levels and SQL attributes. Explained why the outer key follows the operator's access pattern and
why inner ordering and optional use of inner seeks are separate concerns. Checked the file format and batch
writer against the existing pinned source citations.

## Flat KV requirement

On 2026-09-28, the user specified that merged-index keys are opaque byte strings folded differently for each
record type. The merged index must use standard flat KV storage, not Feldera's grouped key/value layout.
Updated the recommendation, architecture diagram, lifecycle qualifications, and adapter acceptance criteria.
Existing grouped storage now describes only the baseline. Reuse is at the lower storage-infrastructure level;
byte ordering, weighted payload updates, and snapshot semantics require an adapter. Physical encoding and
Rust interfaces remain future work. This supersedes any earlier implication that grouped batches can be reused
unchanged for the merged index.

The user's subsequent follow-ups clarified that the same LSM machinery remains the intended backend.
The flat KV adapter changes representation and merge/comparison rules, not the spine/run-management design.
Also replaced “implements that abstraction” with the concrete relationship: Trace is the retained-state
interface; Spine implements it by holding and merging immutable sorted runs.

On 2026-09-28, clarified the comparator at each level of existing grouped state: the outer level compares
K, and the inner level compares V within a fixed K. Consolidation matches complete (K,V) pairs across runs;
inner ordering does not supply a global V index. Applied this follow-up clarification to both active docs.

On 2026-09-28, the user rejected “recovers logical records above storage” as hiding physical execution and
memory costs. Replaced it with byte-range cursors, field decoding, per-order old/new count/revenue scalars,
and incremental tuple delivery. Specified bounded shared buffers, consumer positions, value lifetimes,
and charged spill/reread or external ordering. Exact ownership and fallback choices remain implementation
work; the architecture must not assume an entire reconstructed relation fits in memory.

On 2026-09-28, replaced the ambiguous phrase “transient reconstructed batches” with computed cursor results
and byte-limited buffers retained until consumption. Distinguished these from intermediate delta streams and
immutable storage batches. Any operator API that requires a batch object needs explicit budgeted storage;
no complete reconstructed relation is assumed to fit in RAM.

On 2026-09-28, the user clarified that reconstruction must be interchangeable with existing integrator-state
access for requested keys. The existing IVM algorithm and output path remain shared, including additive
value differences or old-tuple retraction/new-tuple insertion. Updated the integration boundary, diagram,
aggregate evidence, and acceptance criteria. Distinguished value differences from tuple multiplicities;
the merged-index provider must not select or duplicate aggregate delta-emission logic.
