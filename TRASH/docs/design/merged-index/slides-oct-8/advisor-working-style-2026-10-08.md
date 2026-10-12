# Advisor guidance: physical-design revision, 2026-10-08

This dated supplement records explicit task-specific direction from the final
revision plan. Apply it alongside `~/.codex/advisor-working-style.md`, with
latest task instructions taking precedence.

- Introduce DBSP's purpose and theory briefly, then recover physical behavior
  from code. Remove the prominent opening backdrop.
- Explain an already-loaded static primary-key table before IVM mechanics.
  Call its spine an LSM-like trace and include configured disk backing.
- Use Orders–Lineitem first for the explanatory program. Identify the existing
  experiment's Orders–Customer-first circuit separately and preserve its actual
  accesses. Do not relabel the prototype as `OL`-first.
- Show the complete program and worked matches before representation internals.
  Use five advances per join, with stable changed-row, matching-state, and
  signed-output columns and readable completed information.
- Use transaction delta for logical changes and run for an immutable sorted
  retained collection. Reserve relation subscripts for access ordering and
  visibility superscripts for before/after state, distinct from signed weights.
- State trace sharing prominently. Explain run construction and the missing
  mutable memtable across transactions in the inspected input/join paths.
  Show separate small runs and merging in memory. Keep structural limitations
  separate from unmeasured performance consequences.
- Answer run-size and transaction-count questions without implying a fixed
  run size or fixed transactions per run. Put the join-output chunk target,
  overshoot, and final partial output in notes, separate from placement policy.
- Preserve all payloads and native memory accounting/control qualifications.
  Make no measured savings, update-cost, or comparative performance claims.

These directions are specific to this 16-topic, 20–25 minute talk. They do not
replace the user's broader research priorities or generalize prototype scope.
