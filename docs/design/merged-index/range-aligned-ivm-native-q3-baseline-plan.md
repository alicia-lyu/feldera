# Document range-aligned IVM and implement the native Q3 baseline

Date: 2026-10-01.

## Scope and decisions

The future maintenance method is **range-aligned IVM**. Compatible join/group keys and
the merged-index constitution align delta locations with accumulated-state reads in common
or nested ranges. Globally order-preserving delta execution is not required.

Document the architecture before implementing Step 1: externally authored predicate-free,
order-preserving physical pipelines; a merged-index constitution for the chosen pipeline;
logical SQL defining its maintained result `V` (`J` in earlier notes); and accumulated
state supplied to the logical delta program through selected reconstructed reads.

Step 1 uses native Feldera state and execution only. Compile one materialized predicate-free
Customer–Orders–Lineitem view incrementally in Catalog mode at default optimization,
without intermediate views, hints, star-join opt-out, or compiler changes. Logical Lineitem
has order/line identities and no customer ID. Extended records belong to future secondary
index maintenance and must be available before the associated join maintenance runs.

## Implementation sequence

1. Review active and historical design documents and existing compiler/runtime interfaces.
2. Update the README, support note, and active storage-plan passages. Preserve historical
   plans and verified storage evidence; add dated supersession pointers where needed.
3. Create the standalone experiment under
   `sql-to-dbsp-compiler/experiments/q3-ivm-baseline`, using the generated circuit for
   bootstrap and updates. Explain the actual relational and expanded incremental circuit,
   accumulated state, delays, before/after reads, and output integration.
4. Implement exact dbgen date/decimal parsing, related order/line RF1 and RF2 transactions,
   and separate load/refresh measurements. Default to one worker, 10,000-row load batches,
   three independent runs, and memory-backed state, with configuration options.
5. Compare emitted deltas and materialized `V` against independent full joins for inserts,
   deletes, replacements, simultaneous changes, unmatched rows, key moves, and identical
   payloads with different line identities. Verify parser/refresh fixtures and local dbgen.
6. Check Rust formatting/lint, Markdown links/formatting, and source mappings. Fetch and
   merge the remote tracking branch before each signed commit using specific staged files.

## Measurement and acceptance

Report native transaction latency and input throughput including materialized `V`
maintenance; report preparation, driver memory, and end-to-end time separately. Record
commands, environment, dataset identity, and reproducible semantic counts. No merged-index
access, extension producer, reconstruction, residual Q3 execution, or performance-gain
claim is part of Step 1. Native trace sharing prevents treating one index per integrator
as a universal physical allocation count.

## Progress

- Architecture review and documentation are complete, committed in `8897e3293` and `a9789d911`.
- The native experiment is implemented and committed in `830ec4f43`. The pinned compiler
  generates Orders–Customer followed by Lineitem–joined-orders, using two balanced binary joins. Its generated Rust, relational
  plan, and circuit explanation are retained with the experiment.
- All six native/parser/refresh tests pass, including simultaneous changes to all three
  sources with one and two workers. Rust formatting and strict Clippy checks pass.
- Generated Rust and relational-plan bytes reproduce exactly. Markdown formatting, links,
  source line ranges, and all five Mermaid diagrams pass checks.
- Three optimized local SF 0.01 dbgen runs pass independent delta and materialized-view
  checks for loading, RF1, and RF2. [Verified results and reproduction commands][native-results]
  record native transaction measurements, preparation time, driver memory, and end-to-end time.
- Step 1 is complete. Merged-index reconstruction and the extended-record producer remain
  future work outside this baseline.

[native-results]: ../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/RESULTS.md
