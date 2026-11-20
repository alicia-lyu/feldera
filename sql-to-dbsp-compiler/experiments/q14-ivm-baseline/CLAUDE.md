# q14-ivm-baseline: TPC-H Q14 over MI_P

This harness maintains `V = Part ⋈ Lineitem` on `partkey` (Q14 without its predicates) in two modes, native and reconstructed from the MI_P merged index, and checks that they agree. It is step 5 of [new-merged-index.md](../../../docs/design/merged-index/new-merged-index.md), reduced to correctness; measurement is a later step (user decision of 2026-11-20). The index, its readers, and `Q14State` are in [merged-index-p](../merged-index-p/CLAUDE.md); the rules for any index and harness are in [experiments/CLAUDE.md](../CLAUDE.md). The Q3 counterpart, whose structure this follows, is [q3-ivm-baseline](../q3-ivm-baseline/CLAUDE.md).

## Files

| Piece | Location |
| --- | --- |
| Base circuit: the three MI_P tables with primary keys, no view (generated, git-ignored) | `base.sql`, crate `q14-base-circuit` |
| Native view circuit: keyless Part and Lineitem plus `V`, as in `../merged-index-p/schema.sql` (generated, git-ignored). `V` reads no Partsupp, so this circuit has none | `q14.sql`, crate `q14-view-circuit` |
| Reconstructed view circuit, hand-written: one `join_lineitems_parts`; two `add_input_stream` handles (`TrieInputs`) take `ΔP` and `ΔL` from `Q14State::collect`; `V`'s seven columns, so a line's `l_suppkey` is dropped; `build` registers `V` in a catalog as the generated circuit does; no `ERROR_VIEW` | `q14-trie-view-circuit/` |
| `Pipeline`: both circuits in one runtime, base first; `transact` runs a change and returns the net delta; `view_output` reads the change of `V` and `V` | `src/pipeline.rs` |
| `ManualIndex` (the index's owner thread on the runtime's buffer cache: snapshot and append only) and `index_rows` (the net delta folded row by row) | `src/index.rs` |
| Rows, `Change`, `ViewRow`, and `Reference` (the tests' oracle by primary key) | `src/data.rs` |
| Tests and the transcript | `src/verification.rs` |
| Generates the native pair and `plan.json`, as Q3's does | `generate.sh` |

The Cargo feature `merged-index` builds the reconstructed mode (`q14-trie-view-circuit` in place of `q14-view-circuit`). The crate is a library: there is no driver CLI until measurement.

## Q14 instances of the shared rules

- **One runtime, one cache.** `Pipeline::new` builds both circuits with `Runtime::init_circuits`. `ManualIndex::new` errors if its thread is not on `Runtime::foreground_buffer_cache(0)`; the session collects with that cache bound.
- **Transaction order.** The base circuit commits first; its net delta (`Pipeline::net_delta`, the base tables' delta streams, enabled in `new`) goes natively to the view circuit's keyless tables, whole rows for deletions, and in reconstructed mode to `index_rows`, `ManualIndex::append`, `Q14State::publish`, `collect`, the input handles, the view transaction, and `finish`. A stepped transaction has three steps in each circuit (`Change::steps`: parts, Partsupp, lines); the reconstructed view's steps share one collection.
- **General deltas need no owners.** Every MI_P path is made of its own row's columns (a line item's part and supplier are `l_partkey` and `l_suppkey`), so the net delta folds row by row: no `prepare`, no side store. A line item of an absent part folds under its partkey and joins nothing until the part arrives.

## Tests and gate

```bash
./generate.sh                                                   # native pair; needs JDK 21 and the compiler JAR
cargo +1.96.1 test --offline --locked                           # native
cargo +1.96.1 test --offline --locked --features merged-index   # reconstructed
cargo +1.96.1 test --offline --locked -p q14-trie-view-circuit  # the hand-written circuit alone
./verify.sh                                                     # the gate
```

- `general_transcript`: ten transactions over every kind of general change (load with a line of an absent part, empty, part retyped, lines moved between parts and suppliers in three steps, part deleted with its lines kept, part inserted for waiting lines, Partsupp-only, part returned with a new type and a line in three steps, lines and a part deleted, empty in three steps). After each, the committed change of `V` and the materialized `V` must equal `Reference`'s; the contract check is on. With `Q14_GENERAL_TRANSCRIPT` set, it writes the transcript.
- `the_net_delta_has_whole_rows_for_upserts_and_deletions`; `random_general_changes_match_the_reference` (proptest, 16 sequences of random table states, one step or three); `the_transcript_does_not_depend_on_the_pass` (reconstructed only).
- `q14-trie-view-circuit`: `general_deltas_match_the_brute_force_join` and `random_transactions_match_the_brute_force_join`, against a brute-force join with no base circuit.
- `verify.sh` generates the native pair, runs fmt, both builds' tests and Clippy, the hand-written circuit's tests, compares the two transcripts with `cmp`, checks that MI_P's `generated.rs` is fresh, runs MI_P's fmt, tests, and Clippy, and checks shell syntax and whitespace. Logs go to `target/q14-verification/<run>/`.
