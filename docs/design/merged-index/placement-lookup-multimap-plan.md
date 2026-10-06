# Placement correction: one multimap of candidate locations

Date: 2026-10-06. Status: implemented and verified.

## Summary

Replace the existing order-to-customer lookup with one order-to-customer-set
multimap. Both reconstructed readers probe supplied locations in their designated
accumulated snapshot. This work is separate from R4's RF1/RF2 verification and
does not extend the driver's supported workloads.

## Changes

- Change the existing lookup in `merged-index-col/src/q3.rs` to
  `BTreeMap<i64, BTreeSet<i64>>`. Keep the `publish`/`finish` lifecycle.
- For each requested order, probe every candidate once: Customer/Orders in the
  after snapshot for `joined_orders_after`, and line prefixes in the before
  snapshot for `lineitems_before`. Missing or empty candidate sets retain the
  missing-placement error.
- Update RF1/RF2 preparation and existing callers to supply singleton sets.
  Preserve workload validation and `ManualIndex::append`.
- Deduplicate addresses without cancelling them by delta signs. The lookup
  contains neither payloads nor weights; no additional delta snapshot is published.

## Verification and documentation

- Run existing singleton RF1/RF2 and reader regressions.
- Add direct reader tests for an order under customer 2 before and customer 3
  afterward, repeated locations, absent prefixes, and missing/empty candidates.
- Run the COL suite, reconstructed harness regressions, formatting, and strict Clippy.
- Document the multimap and distinguish supplied-location reader support from
  general-update maintenance. Keep container selection as a future optimization
  beside the existing copying/sorting note.

## Outcome

The lookup now stores one customer set per order, and both readers probe the
same candidates in their respective snapshots. RF1/RF2 preparation supplies
singleton sets and retains its existing validation.

Completed checks with Rust 1.96.1 and offline locked dependencies:

- All 21 COL tests passed, including the new candidate-location and missing/empty
  lookup tests, plus existing storage and compaction regressions.
- All six original reconstructed R3 harness regressions passed in an isolated
  generated copy, including RF1/RF2, multi-step transactions, and checkpoint
  restoration. This avoided changing the other agent's R4 generated artifacts.
- COL and the reconstructed harness passed Clippy with warnings denied.
  Rust formatting and whitespace checks passed.

The transaction-local `BTreeMap`/`BTreeSet` container choice remains a future
optimization alongside transient row copying and possible sorting. General-update
maintenance remains deferred.
