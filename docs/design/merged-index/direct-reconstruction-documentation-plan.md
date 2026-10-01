# Document the choice of direct reconstruction

Date: 2026-10-01.

## Decision

For the current Feldera experiments, use direct, query-specific reconstruction without an
intermediate scan-operator layer. Such a layer is an architectural choice for either full-query
execution or incremental view maintenance (IVM), not an inherent distinction between them.

## Documentation changes

1. Organize the active [support discussion](support.md) around three points:
   most maintenance accesses request limited key ranges; each query/integrator routine controls
   its source lookups and reconstructs only requested accumulated state; the demonstrated native
   storage capabilities require no new common scan infrastructure as a prerequisite.
2. Support the lookup-first choice with the delta-key fetch path in
   [join.rs](../../../crates/dbsp/src/operator/dynamic/join.rs#L1661) and contrast it with
   [integrated trace construction](../../../crates/dbsp/src/operator/dynamic/accumulate_trace.rs#L372).
   Use the [Phase 4 note](phase-4-implementation-note.md) to distinguish proven storage behavior
   from deferred runtime integration.
3. Mark shared scan sessions as a deferred, optional abstraction usable in full-query execution
   or IVM. Preserve applicable weighted-state, cursor-ordering, memory, and snapshot-ownership
   requirements independently of that abstraction.
4. Update [README descriptions and next steps](README.md) and active storage-plan handoff wording
   that treats sessions as mandatory. Leave historical plans intact. Make no code changes.

## Verification

- Review the diff for consistent direct-reconstruction wording and unchanged weighted semantics.
- Run Markdown lint, check local links and heading anchors, and validate cited source line ranges.
- Run `git diff --check` and verify the change contains only Markdown files.
- Fetch and merge the remote tracking branch before making a signed documentation commit.
