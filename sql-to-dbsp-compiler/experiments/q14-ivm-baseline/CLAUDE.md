# q14-ivm-baseline: TPC-H Q14 over MI_P

MI_P's Q14 work outside its index crate. The index, its readers, and `Q14State` are in [merged-index-p](../merged-index-p/CLAUDE.md); the rules for any index and harness are in [experiments/CLAUDE.md](../CLAUDE.md). The Q3 counterpart is [q3-ivm-baseline](../q3-ivm-baseline/CLAUDE.md). Location: user decision of 2026-11-20.

| Piece | Where |
| --- | --- |
| Reconstructed view circuit (step 4), hand-written: `V = P ⋈ L` on `partkey`, one `join_lineitems_parts`; two `add_input_stream` handles (`TrieInputs`) take `ΔP` and `ΔL` from `Q14State::collect`; `V`'s seven columns are `schema.sql`'s, so a line's `l_suppkey` is dropped | `q14-trie-view-circuit/` |
| Harness (step 5): base circuit, native view circuit from SQL, index upkeep, transcript comparison | not written yet |

`q14-trie-view-circuit` has no catalog registration (`ViewRecord`, `build`) yet; the harness adds it.

## Tests

```bash
cd q14-trie-view-circuit && cargo +1.96.1 test --offline --locked
```

- `general_deltas_match_the_brute_force_join`: a load, one transaction with every kind of change (part retyped, deleted, inserted; lines moved between parts and suppliers; a Partsupp-only change; suppliers with orderkeys out of order), an empty transaction, and a part deleted and returned. Both `Pass` modes.
- `random_transactions_match_the_brute_force_join`: a proptest over sequences of random small table states; each transaction is the general delta between two of them.

Each compares the circuit's output delta with the brute-force change of `V` after every transaction.
