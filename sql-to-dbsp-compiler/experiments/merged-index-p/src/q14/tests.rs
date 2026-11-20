use std::collections::{BTreeMap, BTreeSet};

use super::*;
use crate::{
    build_batch, LineitemByPartkeySuppkeyOrderkeyLinenumberKey,
    LineitemByPartkeySuppkeyOrderkeyLinenumberPayload, PartByPartkeyKey, PartByPartkeyPayload,
    PartsuppByPartkeySuppkeyKey, PartsuppByPartkeySuppkeyPayload, StoredPayload,
};
use dbsp::{
    circuit::CircuitConfig,
    dynamic::{DowncastTrait, DynData},
    storage::attribution::Component,
    trace::{
        BatchReader as DynamicBatchReader, BatchReaderFactories, Cursor, OrdIndexedWSetFactories,
        RunReads, WithSnapshot,
    },
    typed_batch::{BatchReader, TypedBatch},
    utils::{Tup2, Tup8},
    DynZWeight, OutputHandle, Runtime,
};
use proptest::{prelude::*, test_runner::TestCaseError};

type SourceRow = (Key, Payload, i64);

fn part(partkey: i64, part_type: &str, weight: i64) -> SourceRow {
    (
        Key::PartByPartkey(PartByPartkeyKey { partkey }),
        Payload::PartByPartkey(PartByPartkeyPayload {
            p_type: part_type.into(),
        }),
        weight,
    )
}

fn partsupp(partkey: i64, suppkey: i64, available: i32, weight: i64) -> SourceRow {
    (
        Key::PartsuppByPartkeySuppkey(PartsuppByPartkeySuppkeyKey { partkey, suppkey }),
        Payload::PartsuppByPartkeySuppkey(PartsuppByPartkeySuppkeyPayload {
            ps_availqty: available,
        }),
        weight,
    )
}

fn line(
    partkey: i64,
    suppkey: i64,
    orderkey: i64,
    linenumber: i32,
    price: i128,
    weight: i64,
) -> SourceRow {
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
                l_shipdate: ship_date(),
                l_extendedprice: money(price),
                l_discount: discount(),
            },
        ),
        weight,
    )
}

fn money(hundredths: i128) -> SqlDecimal<15, 2> {
    SqlDecimal::new(hundredths, 2).unwrap()
}

fn discount() -> SqlDecimal<4, 2> {
    SqlDecimal::new(4, 2).unwrap()
}

fn ship_date() -> Date {
    Date::from_days(30)
}

/// The value of a line item in the readers' batches.
fn line_value(suppkey: i64, orderkey: i64, linenumber: i32, price: i128) -> LineValue {
    Tup6(
        suppkey,
        orderkey,
        linenumber,
        money(price),
        discount(),
        ship_date(),
    )
}

fn factories() -> dbsp::trace::OrdIndexedWSetFactories<DynData, DynData, DynZWeight> {
    OrdIndexedWSetFactories::<DynData, DynData, DynZWeight>::new::<Vec<u8>, StoredPayload, i64>()
}

/// `Old`, as runs of `batches`.
fn source(batches: Vec<Vec<SourceRow>>) -> SourceSnapshot {
    SourceSnapshot::with_batches(
        &factories(),
        batches
            .into_iter()
            .map(|rows| Arc::new(build_batch(rows).unwrap()))
            .collect(),
    )
}

/// One `Dlt` run of `rows`.
fn run(rows: Vec<SourceRow>) -> Arc<MergedIndexBatch> {
    Arc::new(build_batch(rows).unwrap())
}

/// A circuit delta of `rows`.
fn delta<K: DBData, V: DBData>(rows: Vec<(K, V, i64)>) -> SpineSnapshot<OrdIndexedZSet<K, V>> {
    let batch = OrdIndexedZSet::from_tuples(
        (),
        rows.into_iter()
            .map(|(k, v, w)| Tup2(Tup2(k, v), w))
            .collect(),
    );
    TypedBatch::new(batch.into_inner().into_ro_snapshot())
}

/// The rows of `batch` with nonzero weights.
fn tuples<K: DBData, V: DBData>(
    batch: &impl BatchReader<Key = K, Val = V, DynR = DynZWeight, Time = ()>,
) -> Vec<(K, V, i64)> {
    let mut cursor = batch.inner().cursor();
    let mut rows = Vec::new();
    while cursor.key_valid() {
        while cursor.val_valid() {
            let weight = **cursor.weight();
            if weight != 0 {
                rows.push((
                    cursor.key().downcast_checked::<K>().clone(),
                    cursor.val().downcast_checked::<V>().clone(),
                    weight,
                ));
            }
            cursor.step_val();
        }
        cursor.step_key();
    }
    rows
}

/// A state whose partner reads check their contract.
fn checked_state() -> Q14State {
    let state = Q14State::default();
    state.enable_contract_check();
    state
}

/// Publishes `old` and `dlt` and collects them with `pass`.
fn published(
    old: Vec<Vec<SourceRow>>,
    dlt: Vec<SourceRow>,
    pass: Pass,
) -> (Q14State, DrivingDeltas) {
    let state = checked_state();
    state.publish(source(old), run(dlt)).unwrap();
    let driving = state.collect(pass).unwrap().driving;
    (state, driving)
}

fn requests_of<R: ReaderDefinition>(reader: &SealedReader<R>) -> Vec<Vec<u8>> {
    reader.requests().iter().map(<[u8]>::to_vec).collect()
}

fn with_readers<T>(state: &Q14State, f: impl FnOnce(&CollectedReaders) -> T) -> T {
    let views = state.views().unwrap();
    f(views.collected().unwrap())
}

// ---- A worked transaction ----

/// `Old`: parts 1 to 4 and 6, with supplies and line items. Part 1's second
/// supplier holds an order (99) below the first supplier's (100, 101), so its
/// lines arrive in index order only if their value starts with the supplier.
fn worked_old() -> Vec<Vec<SourceRow>> {
    vec![
        vec![
            part(1, "A", 1),
            partsupp(1, 10, 5, 1),
            line(1, 10, 100, 1, 1_000, 1),
            line(1, 10, 101, 1, 1_100, 1),
            partsupp(1, 20, 6, 1),
            line(1, 20, 99, 1, 990, 1),
            part(2, "B", 1),
            line(2, 10, 102, 1, 2_000, 1),
        ],
        vec![
            part(3, "C", 1),
            line(3, 20, 103, 1, 3_000, 1),
            part(4, "D", 1),
            partsupp(4, 20, 7, 1),
            part(6, "F", 1),
            partsupp(6, 30, 9, 1),
        ],
    ]
}

/// The transaction: part 1's type changes and it gains a line; part 2 is
/// deleted with its line; part 5 is inserted with a line (`Old` has no part
/// 5); line `(103, 1)` moves from part 3 to part 4 (its `l_partkey`
/// changes); part 6's supply changes, and nothing else of part 6.
fn worked_dlt() -> Vec<SourceRow> {
    vec![
        part(1, "A", -1),
        part(1, "AA", 1),
        line(1, 10, 104, 1, 1_200, 1),
        part(2, "B", -1),
        line(2, 10, 102, 1, 2_000, -1),
        line(3, 20, 103, 1, 3_000, -1),
        line(4, 20, 103, 1, 3_000, 1),
        part(5, "E", 1),
        line(5, 30, 105, 1, 5_000, 1),
        partsupp(6, 30, 9, -1),
        partsupp(6, 30, 8, 1),
    ]
}

#[test]
fn a_worked_transaction_reads_the_parts_after_and_the_lines_before() {
    for pass in [Pass::Seek, Pass::Step] {
        let (state, driving) = published(worked_old(), worked_dlt(), pass);

        // The driving deltas, indexed by part.
        assert_eq!(
            tuples(&driving.parts),
            vec![
                (Tup1(1), Tup1("A".into()), -1),
                (Tup1(1), Tup1("AA".into()), 1),
                (Tup1(2), Tup1("B".into()), -1),
                (Tup1(5), Tup1("E".into()), 1),
            ]
        );
        assert_eq!(
            tuples(&driving.lines),
            vec![
                (Tup1(1), line_value(10, 104, 1, 1_200), 1),
                (Tup1(2), line_value(10, 102, 1, 2_000), -1),
                (Tup1(3), line_value(20, 103, 1, 3_000), -1),
                (Tup1(4), line_value(20, 103, 1, 3_000), 1),
                (Tup1(5), line_value(30, 105, 1, 5_000), 1),
            ]
        );

        with_readers(&state, |readers| {
            // `P⁺` requests the parts of `ΔL`; `L⁻` those of `ΔP`.
            let groups = |parts: &[i64]| -> Vec<Vec<u8>> {
                parts.iter().map(|&p| prefix_partkey(p)).collect()
            };
            assert_eq!(requests_of(&readers.parts), groups(&[1, 2, 3, 4, 5]));
            assert_eq!(requests_of(&readers.lines), groups(&[1, 2, 5]));

            // `P⁺`: part 1 after its change, part 2 gone, parts 3 and 4 as in
            // `Old`, and part 5 from `ΔP` alone.
            assert_eq!(
                tuples(&readers.parts.partner()),
                vec![
                    (Tup1(1), Tup1("AA".into()), 1),
                    (Tup1(3), Tup1("C".into()), 1),
                    (Tup1(4), Tup1("D".into()), 1),
                    (Tup1(5), Tup1("E".into()), 1),
                ]
            );
            // `L⁻`: `Old`'s lines of parts 1 and 2; part 5 had none.
            assert_eq!(
                tuples(&readers.lines.partner()),
                vec![
                    (Tup1(1), line_value(10, 100, 1, 1_000), 1),
                    (Tup1(1), line_value(10, 101, 1, 1_100), 1),
                    (Tup1(1), line_value(20, 99, 1, 990), 1),
                    (Tup1(2), line_value(10, 102, 1, 2_000), 1),
                ]
            );
        });

        // Part 6, touched by its supply only, is scanned too.
        let reads = state.finish().unwrap();
        assert_eq!(reads.counts.groups, 6, "{pass:?}");
    }
}

#[test]
fn a_partner_read_refuses_a_part_outside_its_requests_when_checked() {
    let (state, ..) = published(worked_old(), worked_dlt(), Pass::Seek);
    let requested = delta(vec![(Tup1(3_i64), line_value(20, 103, 1, 3_000), 1)]);
    assert!(state.parts_after(&requested).is_ok());
    let unrequested = delta(vec![(Tup1(6_i64), line_value(1, 1, 1, 1), 1)]);
    assert!(state
        .parts_after(&unrequested)
        .unwrap_err()
        .contains("part 6 is outside the requests of P⁺"));
    let unrequested = delta(vec![(Tup1(3_i64), Tup1(SqlString::from("C")), 1)]);
    assert!(state
        .lineitems_before(&unrequested)
        .unwrap_err()
        .contains("part 3 is outside the requests of L⁻"));
    state.finish().unwrap();

    // Off by default: the same read returns the partner batch.
    let state = Q14State::default();
    state
        .publish(source(worked_old()), run(worked_dlt()))
        .unwrap();
    state.collect(Pass::Seek).unwrap();
    assert!(state
        .parts_after(&delta(vec![(Tup1(6_i64), line_value(1, 1, 1, 1), 1)]))
        .is_ok());
}

#[test]
fn a_publication_is_collected_once() {
    let state = checked_state();
    state
        .publish(source(worked_old()), run(worked_dlt()))
        .unwrap();
    state.collect(Pass::Seek).unwrap();
    assert!(state
        .collect(Pass::Seek)
        .err()
        .unwrap()
        .contains("already collected"));
    state.finish().unwrap();
    assert!(state.collect(Pass::Seek).is_err());
}

// ---- The readers and the delta rule against a brute force ----

type State = BTreeMap<(Key, Payload), i64>;

/// The summed weight of each `(key, payload)` over `runs`, zero sums dropped.
fn consolidated(runs: &[&Vec<SourceRow>]) -> State {
    let mut sums = BTreeMap::new();
    for rows in runs {
        for (key, payload, weight) in rows.iter() {
            *sums.entry((key.clone(), payload.clone())).or_insert(0) += weight;
        }
    }
    sums.retain(|_, weight| *weight != 0);
    sums
}

fn wanted<K: Ord, V: Ord>(rows: impl IntoIterator<Item = (K, V, i64)>) -> Vec<(K, V, i64)> {
    let mut sums = BTreeMap::new();
    for (key, value, weight) in rows {
        *sums.entry((key, value)).or_insert(0) += weight;
    }
    sums.into_iter()
        .filter(|(_, weight)| *weight != 0)
        .map(|((key, value), weight)| (key, value, weight))
        .collect()
}

/// The parts of a state.
fn parts_of(state: &State) -> Vec<(Tup1<i64>, Tup1<SqlString>, i64)> {
    state
        .iter()
        .filter_map(|((key, payload), weight)| match (key, payload) {
            (Key::PartByPartkey(k), Payload::PartByPartkey(p)) => {
                Some((Tup1(k.partkey), Tup1(p.p_type.clone()), *weight))
            }
            _ => None,
        })
        .collect()
}

/// The line items of a state, as the readers index them.
fn lines_of(state: &State) -> Vec<(Tup1<i64>, LineValue, i64)> {
    state
        .iter()
        .filter_map(|((key, payload), weight)| match (key, payload) {
            (
                Key::LineitemByPartkeySuppkeyOrderkeyLinenumber(k),
                Payload::LineitemByPartkeySuppkeyOrderkeyLinenumber(p),
            ) => Some((
                Tup1(k.partkey),
                Tup6(
                    k.suppkey,
                    k.orderkey,
                    k.linenumber,
                    p.l_extendedprice,
                    p.l_discount,
                    p.l_shipdate,
                ),
                *weight,
            )),
            _ => None,
        })
        .collect()
}

/// A row of `V`: the part's key and type, and the line item's value.
type ViewRow = (i64, SqlString, LineValue);

/// `L ⋈ P` on `partkey`, weights multiplied, zero sums dropped.
fn join(
    lines: &[(Tup1<i64>, LineValue, i64)],
    parts: &[(Tup1<i64>, Tup1<SqlString>, i64)],
) -> BTreeMap<ViewRow, i64> {
    let mut view = BTreeMap::new();
    for (Tup1(line_part), value, line_weight) in lines {
        for (Tup1(partkey), Tup1(part_type), part_weight) in parts {
            if line_part == partkey {
                *view
                    .entry((*partkey, part_type.clone(), *value))
                    .or_insert(0) += line_weight * part_weight;
            }
        }
    }
    view.retain(|_, weight| *weight != 0);
    view
}

/// `sum` plus `addend`, zero sums dropped.
fn add(
    mut sum: BTreeMap<ViewRow, i64>,
    addend: BTreeMap<ViewRow, i64>,
    sign: i64,
) -> BTreeMap<ViewRow, i64> {
    for (row, weight) in addend {
        *sum.entry(row).or_insert(0) += sign * weight;
    }
    sum.retain(|_, weight| *weight != 0);
    sum
}

fn partkeys(rows: &[(Tup1<i64>, impl Sized, i64)]) -> BTreeSet<i64> {
    rows.iter().map(|(Tup1(partkey), ..)| *partkey).collect()
}

/// Collects `old_runs` and `dlt_rows` with `pass` and checks against a brute
/// force over the consolidated states: the requests, `P⁺` (the parts after
/// at the parts of `ΔL`), `L⁻` (the lines before at the parts of `ΔP`), and
/// the delta rule `ΔL ⋈ P⁺ + L⁻ ⋈ ΔP = V(after) - V(before)`.
fn check_readers(
    old_runs: Vec<Vec<SourceRow>>,
    dlt_rows: Vec<SourceRow>,
    pass: Pass,
) -> Result<(), TestCaseError> {
    let (state, driving) = published(old_runs.clone(), dlt_rows.clone(), pass);

    let before = consolidated(&old_runs.iter().collect::<Vec<_>>());
    let dlt = consolidated(&[&dlt_rows]);
    let mut every_run = old_runs;
    every_run.push(dlt_rows);
    let after = consolidated(&every_run.iter().collect::<Vec<_>>());

    let delta_parts = tuples(&driving.parts);
    let delta_lines = tuples(&driving.lines);
    prop_assert_eq!(&delta_parts, &parts_of(&dlt));
    prop_assert_eq!(&delta_lines, &wanted(lines_of(&dlt)));
    let parts_of_delta_lines = partkeys(&delta_lines);
    let parts_of_delta_parts = partkeys(&delta_parts);

    let (parts_after, lines_before) = with_readers(&state, |readers| {
        let groups = |parts: &BTreeSet<i64>| -> Vec<Vec<u8>> {
            parts.iter().map(|&p| prefix_partkey(p)).collect()
        };
        prop_assert_eq!(requests_of(&readers.parts), groups(&parts_of_delta_lines));
        prop_assert_eq!(requests_of(&readers.lines), groups(&parts_of_delta_parts));
        Ok((
            tuples(&readers.parts.partner()),
            tuples(&readers.lines.partner()),
        ))
    })?;

    let wanted_parts: Vec<_> = parts_of(&after)
        .into_iter()
        .filter(|(Tup1(p), ..)| parts_of_delta_lines.contains(p))
        .collect();
    prop_assert_eq!(&parts_after, &wanted_parts);
    let wanted_lines = wanted(
        lines_of(&before)
            .into_iter()
            .filter(|(Tup1(p), ..)| parts_of_delta_parts.contains(p)),
    );
    prop_assert_eq!(&lines_before, &wanted_lines);

    // The delta rule, from the readers alone.
    let delta_view = add(
        join(&delta_lines, &parts_after),
        join(&lines_before, &delta_parts),
        1,
    );
    let brute_force = add(
        join(&lines_of(&after), &parts_of(&after)),
        join(&lines_of(&before), &parts_of(&before)),
        -1,
    );
    prop_assert_eq!(delta_view, brute_force);

    // The collecting thread reads under no owner but the index's and the
    // requested slice's.
    let reads = state.finish().unwrap();
    for silent in [
        Component::Query,
        Component::BaseRelation,
        Component::PartnerTrace,
    ] {
        prop_assert_eq!(reads.driver_reads[silent as usize], RunReads::default());
    }
    Ok(())
}

/// A key of a small trie: parts 1 to 3, suppliers 1 to 2, orders 1 to 2, and
/// line numbers 1 to 2.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum SmallKey {
    Part(i64),
    Partsupp(i64, i64),
    Line(i64, i64, i64, i32),
}

fn small_key() -> impl Strategy<Value = SmallKey> {
    prop_oneof![
        (1_i64..=3).prop_map(SmallKey::Part),
        (1_i64..=3, 1_i64..=2).prop_map(|(p, s)| SmallKey::Partsupp(p, s)),
        (1_i64..=3, 1_i64..=2, 1_i64..=2, 1_i32..=2)
            .prop_map(|(p, s, o, l)| SmallKey::Line(p, s, o, l)),
    ]
}

/// The row of `key` with one of three payloads (`variant`) and `weight`. The
/// types are ordered against their packed form (`AAA` < `BB` < `C`, but the
/// packed length comes first), so a replaced type arrives in `Dlt` against
/// the typed order.
fn small_row(key: &SmallKey, variant: u8, weight: i64) -> SourceRow {
    match *key {
        SmallKey::Part(p) => part(p, ["AAA", "BB", "C"][usize::from(variant)], weight),
        SmallKey::Partsupp(p, s) => partsupp(p, s, i32::from(variant), weight),
        SmallKey::Line(p, s, o, l) => line(p, s, o, l, 100 + i128::from(variant), weight),
    }
}

/// A logical `Old` (each key holds one payload with weight 1 or 2), split at
/// random into two runs, and a `Dlt` that replaces, deletes, or inserts some
/// keys: for each changed key, `-old` if it has one and `+new` if it gets
/// one. A line that leaves one part and enters another is a delete and an
/// insert of two keys.
fn old_runs_and_dlt() -> impl Strategy<Value = (Vec<Vec<SourceRow>>, Vec<SourceRow>)> {
    let old =
        proptest::collection::btree_map(small_key(), (0_u8..3, 1_i64..=2, any::<bool>()), 0..12);
    let changes = proptest::collection::btree_map(small_key(), proptest::option::of(0_u8..3), 0..8);
    (old, changes).prop_map(|(old, changes)| {
        let mut runs = vec![Vec::new(), Vec::new()];
        for (key, (variant, weight, second)) in &old {
            runs[usize::from(*second)].push(small_row(key, *variant, *weight));
        }
        let mut dlt = Vec::new();
        for (key, new) in changes {
            if let Some((variant, weight, _)) = old.get(&key) {
                dlt.push(small_row(&key, *variant, -*weight));
            }
            if let Some(variant) = new {
                dlt.push(small_row(&key, variant, 1));
            }
        }
        (runs, dlt)
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// Both readers, their requests, and the delta rule equal a brute force,
    /// whatever the split of `Old` into runs and the pass.
    #[test]
    fn readers_and_the_delta_rule_match_a_brute_force(
        (old_runs, dlt_rows) in old_runs_and_dlt(),
        step_pass in any::<bool>(),
    ) {
        check_readers(
            old_runs,
            dlt_rows,
            if step_pass { Pass::Step } else { Pass::Seek },
        )?;
    }
}

#[test]
fn the_worked_transaction_satisfies_the_delta_rule() {
    for pass in [Pass::Seek, Pass::Step] {
        check_readers(worked_old(), worked_dlt(), pass).unwrap();
    }
}

// ---- The join in a circuit ----

/// A row of the circuit's `V`: `(p_partkey, p_type, l_suppkey, l_orderkey,
/// l_linenumber, l_extendedprice, l_discount, l_shipdate)`.
type CircuitRow = Tup8<i64, SqlString, i64, i64, i32, SqlDecimal<15, 2>, SqlDecimal<4, 2>, Date>;

fn circuit_row(
    (partkey, part_type, Tup6(supplier, order, number, price, disc, date)): ViewRow,
) -> CircuitRow {
    Tup8(
        partkey, part_type, supplier, order, number, price, disc, date,
    )
}

/// Runs `transactions` (each the rows of its `Dlt`) through a circuit of
/// [`join_lineitems_parts`]; after each, the circuit's output must be the
/// brute-force change of `V`.
fn check_circuit(transactions: Vec<Vec<SourceRow>>) {
    let state = checked_state();
    let build_state = state.clone();
    type Inputs = (
        dbsp::InputHandle<PartByPartkeyVector>,
        dbsp::InputHandle<LineitemByPartkeyVector>,
    );
    let (mut circuit, ((parts, lines), output)): (_, (Inputs, OutputHandle<OrdZSet<CircuitRow>>)) =
        Runtime::init_circuit(CircuitConfig::with_workers(1), move |circuit| {
            let (part_stream, parts) = circuit.add_input_stream::<PartByPartkeyVector>();
            let (line_stream, lines) = circuit.add_input_stream::<LineitemByPartkeyVector>();
            let view = join_lineitems_parts(
                &line_stream,
                &part_stream,
                build_state.clone(),
                |Tup1(partkey), line, Tup1(part_type)| {
                    circuit_row((*partkey, part_type.clone(), *line))
                },
            );
            Ok(((parts, lines), view.output()))
        })
        .unwrap();

    let mut runs: Vec<Vec<SourceRow>> = Vec::new();
    for (number, rows) in transactions.into_iter().enumerate() {
        let before = consolidated(&runs.iter().collect::<Vec<_>>());
        state
            .publish(source(runs.clone()), run(rows.clone()))
            .unwrap();
        runs.push(rows);
        let after = consolidated(&runs.iter().collect::<Vec<_>>());

        let driving = state.collect(Pass::Seek).unwrap().driving;
        parts.set_for_worker(0, driving.parts);
        lines.set_for_worker(0, driving.lines);
        circuit.transaction().unwrap();
        state.finish().unwrap();

        let wanted: BTreeMap<CircuitRow, i64> = add(
            join(&lines_of(&after), &parts_of(&after)),
            join(&lines_of(&before), &parts_of(&before)),
            -1,
        )
        .into_iter()
        .map(|(row, weight)| (circuit_row(row), weight))
        .collect();
        let mut observed = BTreeMap::new();
        let output = output.consolidate();
        let mut cursor = output.inner().cursor();
        while cursor.key_valid() {
            *observed
                .entry(cursor.key().downcast_checked::<CircuitRow>().clone())
                .or_insert(0) += **cursor.weight();
            cursor.step_key();
        }
        assert_eq!(observed, wanted, "transaction {number}");
    }
}

/// The load, then the worked transaction, then one that deletes part 1 while
/// its line items stay (they join nothing until the part returns), then its
/// return with a new type and a new line.
#[test]
fn the_join_emits_the_change_of_v_over_transactions() {
    check_circuit(vec![
        worked_old().concat(),
        worked_dlt(),
        vec![part(1, "AA", -1)],
        vec![part(1, "AB", 1), line(1, 10, 106, 2, 1_300, 1)],
    ]);
}
