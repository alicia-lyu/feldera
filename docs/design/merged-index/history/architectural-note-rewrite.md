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

## Shared-scan pseudocode expansion

On 2026-09-28, the user requested a detailed elaboration of storage ownership, provider cursors, ordering,
and bounded shared scans, using the mi_db session design. Read its modified architecture document and record
its revision/hash alongside the source links. Keep its fail-on-overflow policy distinct from proposed
Feldera spill/reread extensions. Add pseudocode to the active supporting report and link it from the main
note; replace repeated buffer prose. Verify lifecycle, reader positions, EOF, release, bounded allocation,
ordering, and scheduler progress without adding runtime code or claiming executed conformance tests.

Read-only protocol review confirmed the mi_db attribution and identified two fixes: serialize mutations of
shared session state, and explicitly mark readers closed with idempotent close after borrowed values are
released. Both are reflected in the pseudocode. Spilled-record reloads also reserve memory before reading.
Markdown, active links/anchors, source line ranges, Mermaid diagrams, and whitespace checks passed; no runtime
or conformance tests were added or claimed for this documentation-only design.

On 2026-09-28, clarified the provenance and scope of aggregate output-state discussion: the cited generic
incremental aggregate explicitly chooses retained previous output over recomputation. Separate reconstruction
routines can supply each input/output state request directly from merged source ranges; no materialized
reconstruction chain is required. Added pseudocode for per-state routine bindings and distinguished this
freedom from preserving the shared IVM output path. Re-read aggregate construction and its design comment.

The user then corrected the terminology: the sole target is an integrator's accumulated output state, never
its input delta stream. Replaced “input-state routine” wording and pseudocode with per-integrator H/A/O/B/C
routines. An aggregate's input trace is itself an integrator output consumed downstream; its delta input
remains unchanged. The generic runtime example is evidence about retained state, not the selected five-state
Q3 graph. This supersedes the ambiguous wording in the preceding entry.

## Figure 4c and generic runtime state

On 2026-09-28, the user pointed out that Figure 4c has only one integrator inside grouping. Read the actual
TikZ source and the generic aggregate/upsert implementation. Corrected the conflation: the paper integrates
Count/Revenue H (named M there), delays that summary, and emits both group tuples; its A integrator belongs
to the Orders join. The generic runtime instead retains output for retractions. Added separate diagrams and
removed the implication that generic runtime output retention adds another grouping integrator to Figure 4c.
Bindings must follow the selected circuit, not the union of alternative implementations' state objects.

The user clarified that prior-output retention serves the z^-1 role and can be reconstructed from timed
weighted source tuples. Added the equivalence E(H[t-1]) = (z^-1 E(H))[t], a delay-placement diagram, and
as-of weighted reconstruction formulas. Distinguish the figure's delayed summary from the runtime's delayed
emitted tuple without implying another required physical state copy. Source batch versions are separate
from root-circuit unit timestamps; previous payloads and parent placements remain readable until release.

The user also flagged the ambiguous attribution “the paper's Figure 4c.” Active documents now name the
lecture note Maintaining a Query, One Change at a Time, link its “Equivalent Q3 circuits” figure source,
and refer to panel (c) thereafter. Distinguish that note from the interesting-orderings manuscript used for
refresh-performance evidence.

On 2026-09-28, explained why Q3 summaries contain both N and R: N determines group existence, while R is
revenue. Added the zero-revenue versus empty-group comparison and last-zero-line deletion example. Clarified
that a scan deriving only A can detect existence directly, while reconstruction of H must preserve its
count/revenue contract; no separate durable count is implied.

## Glossary presentation

On 2026-09-28, the user requested distinctive styling for nonstandard terminology. Use Markdown NOTE
callouts labeled Glossary, with bold term labels and plain definitions beside first use. Group terms first
introduced together, including terms previously defined inside table cells. Apply this consistently to the
main note and supporting report; retain ordinary emphasis for technical conclusions.

## Trace interface and grouped traversal

On 2026-09-28, the user requested interface pseudocode and how K -> {V -> weight} is unpacked. Read Trace,
BatchReader, Cursor, and WithSnapshot source. Added a source-backed reduced interface, a sorted-array/offset
example, cross-run weight consolidation, nested cursor traversal, exact-key lookup, and proposed interchangeable
read providers. Keep actual APIs distinct from adapter pseudocode and keep reconstructed integrator outputs
off the write path. Explain timestamp scope, borrowed-value lifetime, and bounded navigation without maps.

The user subsequently requested a Next Steps section in the main README. Added five ordered steps covering
concrete Q3 state bindings, the flat KV/LSM adapter, bounded reconstruction sessions, end-to-end correctness,
and beyond-memory measurement. This intentionally extends the original four-section outline to five sections.
The trace pseudocode and next steps remain documentation; no runtime implementation is claimed.

On 2026-09-28, codified Retained state and Reconstructed state as paired glossary definitions at the start
of both active documents. Retained state is a maintained integrator output; reconstructed state computes the
same requested output from versioned weighted sources. Both obey the same state-access contract, neither
reconstructs input deltas, and reconstruction still uses retained source data and bounded temporary memory.

## Opening reference guide

On 2026-09-28, the user requested all references at the beginning of the README with an explanation of each.
Added an opening guide covering every source file cited by the active documents, the companion operator
checker used by the recorded validation commands, and all supporting-report sections. Group repeated line
citations by source file while retaining inline evidence links. Explain each source's role and distinguish
semantic evidence, implemented behavior, proposed design, and unmeasured performance. The README now has a
reference section followed by the existing five main sections.
