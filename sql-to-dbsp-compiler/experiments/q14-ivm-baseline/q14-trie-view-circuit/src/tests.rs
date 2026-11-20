//! The circuit against a brute-force join, over general-delta transactions:
//! parts inserted, retyped, and deleted; line items moved between parts and
//! suppliers; Partsupp-only changes; and empty transactions. Both scan passes
//! must give the same `V`.

use std::{collections::BTreeMap, sync::Arc};

use dbsp::{
    circuit::CircuitConfig,
    dynamic::{DowncastTrait, DynData},
    trace::{
        merged_index::{MergedIndexBatch, Pass},
        BatchReader as DynamicBatchReader, BatchReaderFactories, Cursor, OrdIndexedWSetFactories,
        SpineSnapshot,
    },
    typed_batch::BatchReader,
    utils::Tup7,
    DynZWeight, OrdZSet, OutputHandle, Runtime,
};
use feldera_sqllib::{Date, SqlDecimal};
use merged_index_p::{
    build_batch, q14::Q14State, Key, LineitemByPartkeySuppkeyOrderkeyLinenumberKey,
    LineitemByPartkeySuppkeyOrderkeyLinenumberPayload, PartByPartkeyKey, PartByPartkeyPayload,
    PartsuppByPartkeySuppkeyKey, PartsuppByPartkeySuppkeyPayload, Payload, StoredPayload,
};
use proptest::prelude::*;

use super::{view, TrieInputs, ViewTuple};

type Row = (Key, Payload);

/// A state of the base tables, as the index stores it: each row once.
type State = BTreeMap<Row, i64>;

fn part(partkey: i64, part_type: &str) -> Row {
    (
        Key::PartByPartkey(PartByPartkeyKey { partkey }),
        Payload::PartByPartkey(PartByPartkeyPayload {
            p_type: part_type.into(),
        }),
    )
}

fn partsupp(partkey: i64, suppkey: i64, available: i32) -> Row {
    (
        Key::PartsuppByPartkeySuppkey(PartsuppByPartkeySuppkeyKey { partkey, suppkey }),
        Payload::PartsuppByPartkeySuppkey(PartsuppByPartkeySuppkeyPayload {
            ps_availqty: available,
        }),
    )
}

fn line(partkey: i64, suppkey: i64, orderkey: i64, linenumber: i32, price: i128) -> Row {
    (
        Key::LineitemByPartkeySuppkeyOrderkeyLinenumber(
            LineitemByPartkeySuppkeyOrderkeyLinenumberKey {
                partkey,
                suppkey,
                orderkey,
                linenumber,
            },
        ),
        Payload::LineitemByPartkeySuppkeyOrderkeyLinenumber(
            LineitemByPartkeySuppkeyOrderkeyLinenumberPayload {
                l_shipdate: Date::from_days(30),
                l_extendedprice: SqlDecimal::new(price, 2).unwrap(),
                l_discount: SqlDecimal::new(4, 2).unwrap(),
            },
        ),
    )
}

fn state(rows: impl IntoIterator<Item = Row>) -> State {
    rows.into_iter().map(|row| (row, 1)).collect()
}

/// `V = P ⋈ L` on `partkey`, by nested loops over `state`.
fn brute_force_view(state: &State) -> BTreeMap<ViewTuple, i64> {
    let mut view = BTreeMap::new();
    for ((line_key, line_payload), line_weight) in state {
        let (
            Key::LineitemByPartkeySuppkeyOrderkeyLinenumber(l),
            Payload::LineitemByPartkeySuppkeyOrderkeyLinenumber(lp),
        ) = (line_key, line_payload)
        else {
            continue;
        };
        for ((part_key, part_payload), part_weight) in state {
            let (Key::PartByPartkey(p), Payload::PartByPartkey(pp)) = (part_key, part_payload)
            else {
                continue;
            };
            if p.partkey != l.partkey {
                continue;
            }
            *view
                .entry(Tup7::new(
                    p.partkey,
                    pp.p_type.clone(),
                    l.orderkey,
                    l.linenumber,
                    lp.l_extendedprice,
                    lp.l_discount,
                    lp.l_shipdate,
                ))
                .or_default() += line_weight * part_weight;
        }
    }
    view.retain(|_, weight| *weight != 0);
    view
}

/// `after - before`, without zeros.
fn difference<T: Ord + Clone>(
    before: &BTreeMap<T, i64>,
    after: &BTreeMap<T, i64>,
) -> BTreeMap<T, i64> {
    let mut delta = after.clone();
    for (row, weight) in before {
        *delta.entry(row.clone()).or_default() -= weight;
    }
    delta.retain(|_, weight| *weight != 0);
    delta
}

fn rows_of(zset: &OrdZSet<ViewTuple>) -> BTreeMap<ViewTuple, i64> {
    let mut cursor = zset.inner().cursor();
    let mut rows = BTreeMap::new();
    while cursor.key_valid() {
        *rows
            .entry(cursor.key().downcast_checked::<ViewTuple>().clone())
            .or_default() += **cursor.weight();
        cursor.step_key();
    }
    rows
}

/// Runs the base tables through `states` in order, starting empty. Each
/// transaction's `Dlt` is the difference between consecutive states; after
/// each, the circuit's output must be the brute-force change of `V`.
fn check(states: &[State], pass: Pass) -> Result<(), String> {
    let state = Q14State::default();
    state.enable_contract_check();
    let build_state = state.clone();
    let (mut circuit, (inputs, output)): (_, (TrieInputs, OutputHandle<OrdZSet<ViewTuple>>)) =
        Runtime::init_circuit(CircuitConfig::with_workers(1), move |circuit| {
            let (inputs, view) = view(circuit, build_state.clone());
            Ok((inputs, view.output()))
        })
        .map_err(|error| error.to_string())?;

    let factories = OrdIndexedWSetFactories::<DynData, DynData, DynZWeight>::new::<
        Vec<u8>,
        StoredPayload,
        i64,
    >();
    let mut runs: Vec<Arc<MergedIndexBatch>> = Vec::new();
    let mut before = State::new();
    for (number, after) in states.iter().enumerate() {
        let rows = difference(&before, after)
            .into_iter()
            .map(|((key, payload), weight)| (key, payload, weight));
        let old = SpineSnapshot::with_batches(&factories, runs.clone());
        let dlt = Arc::new(build_batch(rows)?);
        runs.push(dlt.clone());
        state.publish(old, dlt)?;
        inputs.set(state.collect(pass)?.driving);
        circuit.transaction().map_err(|error| error.to_string())?;
        state.finish()?;

        let observed = rows_of(&output.consolidate());
        let wanted = difference(&brute_force_view(&before), &brute_force_view(after));
        if observed != wanted {
            return Err(format!(
                "transaction {number}: observed {observed:?}, wanted {wanted:?}"
            ));
        }
        before = after.clone();
    }
    Ok(())
}

fn check_both_passes(states: &[State]) {
    for pass in [Pass::Seek, Pass::Step] {
        check(states, pass).unwrap_or_else(|error| panic!("{pass:?}: {error}"));
    }
}

/// The load, then one transaction with every kind of change: part 1 is
/// retyped and gains a line; part 2 is deleted with its line; a line moves
/// from part 3 to part 4 and another to a different supplier; part 5 arrives
/// with its line; part 6 changes only in Partsupp. Part 1's suppliers carry
/// orderkeys out of order. Then an empty transaction, a part deleted while its
/// line stays, and its return with a new type.
#[test]
fn general_deltas_match_the_brute_force_join() {
    let load = state([
        part(1, "A"),
        partsupp(1, 10, 5),
        partsupp(1, 20, 5),
        line(1, 10, 100, 1, 100),
        line(1, 20, 99, 1, 110),
        part(2, "B"),
        line(2, 10, 101, 1, 200),
        part(3, "C"),
        line(3, 10, 103, 1, 300),
        line(3, 10, 104, 1, 310),
        part(4, "D"),
        part(6, "F"),
        partsupp(6, 10, 1),
        line(6, 10, 106, 1, 600),
    ]);
    let general = state([
        part(1, "AA"),
        partsupp(1, 10, 5),
        partsupp(1, 20, 5),
        line(1, 10, 100, 1, 100),
        line(1, 20, 99, 1, 110),
        line(1, 10, 100, 2, 120),
        part(3, "C"),
        line(3, 20, 104, 1, 310),
        part(4, "D"),
        line(4, 10, 103, 1, 300),
        part(5, "E"),
        line(5, 10, 105, 1, 500),
        part(6, "F"),
        partsupp(6, 10, 2),
        line(6, 10, 106, 1, 600),
    ]);
    let mut part_1_deleted = general.clone();
    part_1_deleted.remove(&part(1, "AA"));
    let mut part_1_returns = part_1_deleted.clone();
    part_1_returns.insert(part(1, "AB"), 1);

    check_both_passes(&[
        load,
        general.clone(),
        general,
        part_1_deleted,
        part_1_returns,
    ]);
}

/// One random state of small tables: 4 parts (each absent or of 2 types), their
/// Partsupp rows over 2 suppliers, and 6 line items over them. A line's
/// primary key is not its index key, so moving it to another part or supplier
/// deletes it at one key and inserts it at another.
fn arbitrary_state() -> impl Strategy<Value = State> {
    let parts = prop::collection::vec(prop::option::of(0..2usize), 4);
    let partsupps = prop::collection::vec(prop::option::of(0..2i32), 8);
    // A line item per primary key `(orderkey, linenumber)`: its part,
    // supplier, and price, or absent.
    let lines = prop::collection::vec(prop::option::of((0..4i64, 0..2i64, 1..3i128)), 6);
    (parts, partsupps, lines).prop_map(|(parts, partsupps, lines)| {
        let mut rows = Vec::new();
        for (partkey, part_type) in parts.into_iter().enumerate() {
            if let Some(part_type) = part_type {
                rows.push(part(partkey as i64, ["A", "B"][part_type]));
            }
        }
        for (slot, available) in partsupps.into_iter().enumerate() {
            if let Some(available) = available {
                rows.push(partsupp(slot as i64 / 2, slot as i64 % 2, available));
            }
        }
        for (slot, line_slot) in lines.into_iter().enumerate() {
            if let Some((partkey, suppkey, price)) = line_slot {
                // Orderkeys fall as `slot` rises, so a later supplier's line can
                // carry a smaller orderkey.
                let orderkey = 10 - slot as i64 / 2;
                rows.push(line(partkey, suppkey, orderkey, slot as i32 % 2, price));
            }
        }
        state(rows)
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Random sequences of states: every transaction is the general delta
    /// between two of them.
    #[test]
    fn random_transactions_match_the_brute_force_join(
        states in prop::collection::vec(arbitrary_state(), 1..5),
        step in any::<bool>(),
    ) {
        let pass = if step { Pass::Step } else { Pass::Seek };
        check(&states, pass).map_err(TestCaseError::fail)?;
    }
}
