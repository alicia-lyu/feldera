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
