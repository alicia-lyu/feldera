use std::collections::{BTreeMap, BTreeSet};

use super::*;
use crate::{
    build_batch, prefix_custkey, CustomerByCustkeyKey, CustomerByCustkeyPayload,
    LineitemByCustkeyOrderkeyLinenumberKey, LineitemByCustkeyOrderkeyLinenumberPayload,
    OrdersByCustkeyOrderkeyKey, OrdersByCustkeyOrderkeyPayload, StoredPayload,
    INDEX_ID_CUSTOMER_BY_CUSTKEY, INDEX_ID_LINEITEM_BY_CUSTKEY_ORDERKEY_LINENUMBER,
    INDEX_ID_ORDERS_BY_CUSTKEY_ORDERKEY,
};
use dbsp::{
    circuit::{CircuitConfig, CircuitStorageConfig},
    dynamic::{DowncastTrait, DynData, Erase},
    storage::write_attribution::WriteOwner,
    trace::{
        merged_index::{Entry, GroupBuffers, Pass, ScanSession, SessionCounts},
        Batch, BatchLocation, BatchReaderFactories, Builder, OrdIndexedWSetFactories, RunReads,
        WithSnapshot,
    },
    DynZWeight, Runtime,
};
use feldera_types::config::{StorageConfig, StorageOptions};
use proptest::prelude::*;

type SourceRow = (Key, Payload, i64);

fn customer(custkey: i64, segment: &str, weight: i64) -> SourceRow {
    (
        Key::CustomerByCustkey(CustomerByCustkeyKey { custkey }),
        Payload::CustomerByCustkey(CustomerByCustkeyPayload {
            c_mktsegment: segment.into(),
        }),
        weight,
    )
}

fn order(custkey: i64, orderkey: i64, day: i32, weight: i64) -> SourceRow {
    (
        Key::OrdersByCustkeyOrderkey(OrdersByCustkeyOrderkeyKey { custkey, orderkey }),
        Payload::OrdersByCustkeyOrderkey(OrdersByCustkeyOrderkeyPayload {
            o_orderdate: Date::from_days(day),
            o_shippriority: 7,
        }),
        weight,
    )
}

fn line(custkey: i64, orderkey: i64, linenumber: i32, price: i128, weight: i64) -> SourceRow {
    (
        Key::LineitemByCustkeyOrderkeyLinenumber(LineitemByCustkeyOrderkeyLinenumberKey {
            custkey,
            orderkey,
            linenumber,
        }),
        Payload::LineitemByCustkeyOrderkeyLinenumber(LineitemByCustkeyOrderkeyLinenumberPayload {
            l_shipdate: Date::from_days(30),
            l_extendedprice: SqlDecimal::new(price, 2).unwrap(),
            l_discount: SqlDecimal::new(4, 2).unwrap(),
        }),
        weight,
    )
}

fn source(batches: Vec<Vec<SourceRow>>) -> SourceSnapshot {
    let factories = OrdIndexedWSetFactories::<DynData, DynData, DynZWeight>::new::<
        Vec<u8>,
        StoredPayload,
        i64,
    >();
    SourceSnapshot::with_batches(
        &factories,
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

fn delta<K: DBData, V: DBData>(rows: Vec<(K, V, i64)>) -> SpineSnapshot<OrdIndexedZSet<K, V>> {
    let batch = OrdIndexedZSet::from_tuples(
        (),
        rows.into_iter()
            .map(|(k, v, w)| Tup2(Tup2(k, v), w))
            .collect(),
    );
    TypedBatch::new(batch.into_inner().into_ro_snapshot())
}

/// The rows of `batch` with nonzero weights. The cursor of a snapshot of
/// several batches sums each row's weights across them.
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

fn money(hundredths: i128) -> SqlDecimal<15, 2> {
    SqlDecimal::new(hundredths, 2).unwrap()
}

fn discount() -> SqlDecimal<4, 2> {
    SqlDecimal::new(4, 2).unwrap()
}

fn line_value(
    linenumber: i32,
    price: i128,
) -> Tup4<i32, SqlDecimal<15, 2>, SqlDecimal<4, 2>, Date> {
    Tup4(linenumber, money(price), discount(), Date::from_days(30))
}

/// Publishes `old` and `dlt` and collects their groups with the `Seek` pass.
fn published(old: Vec<Vec<SourceRow>>, dlt: Vec<SourceRow>) -> Q3State {
    let state = Q3State::default();
    state.publish(source(old), run(dlt)).unwrap();
    state.collect(Pass::Seek).unwrap();
    state
}

#[test]
fn four_readers_preserve_weighted_before_after_relations() {
    let initial = vec![
        customer(2, "old", 2),
        order(2, 20, 10, 3),
        line(2, 20, 2, 100, 4),
        line(2, 20, 1, 100, 4),
        customer(9, "unrequested", 1),
        order(9, 90, 10, 1),
        line(9, 90, 1, 100, 1),
    ];
    let changes = vec![
        customer(2, "old", -2),
        customer(2, "new", 1),
        order(2, 20, 10, -3),
        order(2, 20, 11, 1),
        line(2, 20, 1, 100, -4),
        line(2, 20, 1, 200, 1),
    ];
    let state = published(vec![initial], changes);
    let orders_delta = delta(vec![(Tup1(2), Tup3(20, Date::from_days(11), 7), 1)]);
    let customers_delta = delta(vec![(Tup1(2), Tup1(SqlString::from("new")), 1)]);
    let lines_delta = delta(vec![(Tup2(2, 20), line_value(1, 200), 1)]);
    let joined_delta = delta(vec![(
        Tup2(2, 20),
        Tup3(SqlString::from("new"), Date::from_days(11), 7),
        1,
    )]);

    assert_eq!(
        tuples(&state.customers_after(&orders_delta).unwrap()),
        vec![(Tup1(2), Tup1(SqlString::from("new")), 1)]
    );
    assert_eq!(
        tuples(&state.orders_before(&customers_delta).unwrap()),
        vec![(Tup1(2), Tup3(20, Date::from_days(10), 7), 3)]
    );
    assert_eq!(
        tuples(&state.joined_orders_after(&lines_delta).unwrap()),
        vec![(
            Tup2(2, 20),
            Tup3(SqlString::from("new"), Date::from_days(11), 7),
            1
        )]
    );
    assert_eq!(
        tuples(&state.lineitems_before(&joined_delta).unwrap()),
        vec![
            (Tup2(2, 20), line_value(1, 100), 4),
            (Tup2(2, 20), line_value(2, 100), 4),
        ]
    );
    state.finish().unwrap();
}

/// A line item reaches `V` through the order it joins at its own customer:
/// the same order key at another customer is another node.
#[test]
fn joined_orders_are_read_at_the_customer_and_order_of_the_request() {
    let state = published(
        vec![vec![customer(2, "old", 1), customer(3, "new", 1)]],
        vec![
            order(2, 20, 10, -1),
            order(3, 20, 11, 1),
            order(3, 21, 12, 1),
        ],
    );
    let request = delta(vec![
        (Tup2(2, 20), line_value(1, 100), 1),
        (Tup2(3, 20), line_value(1, 100), 1),
        (Tup2(3, 21), line_value(1, 100), 1),
    ]);
    assert_eq!(
        tuples(&state.joined_orders_after(&request).unwrap()),
        vec![
            (
                Tup2(2, 20),
                Tup3(SqlString::from("old"), Date::from_days(10), 7),
                -1
            ),
            (
                Tup2(3, 20),
                Tup3(SqlString::from("new"), Date::from_days(11), 7),
                1
            ),
            (
                Tup2(3, 21),
                Tup3(SqlString::from("new"), Date::from_days(12), 7),
                1
            ),
        ]
    );
    // The line items before the transaction are every line item of the
    // touched customers; the join seeks only the nodes it asks for, such as
    // `(3, 20)`, and finds nothing at `(2, 21)`.
    let state_with_lines = published(
        vec![vec![line(2, 20, 1, 100, 1), line(3, 20, 5, 100, 1)]],
        vec![customer(2, "x", 1), customer(3, "y", 1)],
    );
    let joined = delta(vec![
        (
            Tup2(3, 20),
            Tup3(SqlString::from("y"), Date::from_days(0), 0),
            1,
        ),
        (
            Tup2(2, 21),
            Tup3(SqlString::from("x"), Date::from_days(0), 0),
            1,
        ),
    ]);
    assert_eq!(
        tuples(&state_with_lines.lineitems_before(&joined).unwrap()),
        vec![
            (Tup2(2, 20), line_value(1, 100), 1),
            (Tup2(3, 20), line_value(5, 100), 1),
        ]
    );
}

#[test]
fn joined_projection_multiplies_signed_weights_and_ignores_unrequested_ranges() {
    let state = published(
        vec![],
        vec![
            customer(2, "A", -2),
            order(2, 20, 10, 3),
            order(2, 21, 10, 7),
            customer(9, "unused", 100),
            order(9, 90, 10, 100),
        ],
    );
    let request = delta(vec![(Tup2(2, 20), line_value(1, 100), 1)]);
    assert_eq!(
        tuples(&state.joined_orders_after(&request).unwrap()),
        vec![(
            Tup2(2, 20),
            Tup3(SqlString::from("A"), Date::from_days(10), 7),
            -6
        )]
    );
}

#[test]
fn cancelled_keys_have_no_values_and_untouched_groups_are_refused() {
    let state = published(
        vec![vec![customer(2, "A", 1), customer(3, "B", 1)]],
        vec![customer(2, "A", -1)],
    );
    let touched = delta(vec![(Tup1(2), Tup3(20, Date::from_days(0), 0), 1)]);
    assert!(tuples(&state.customers_after(&touched).unwrap()).is_empty());
    assert!(tuples(
        &state
            .orders_before(&delta(vec![(Tup1(2), Tup1(SqlString::from("A")), 1)]))
            .unwrap()
    )
    .is_empty());

    // Customer 3 exists in `Old` but `Dlt` does not touch it: no driving delta
    // can name it, so a request for it breaks the contract.
    let untouched = delta(vec![(Tup1(3), Tup3(30, Date::from_days(0), 0), 1)]);
    assert!(state
        .customers_after(&untouched)
        .unwrap_err()
        .contains("outside the touched groups"));
    let missing = delta(vec![(Tup2(4, 40), line_value(1, 100), 1)]);
    assert!(state
        .joined_orders_after(&missing)
        .unwrap_err()
        .contains("outside the touched groups"));
}

/// The after reads take `Old` and then `Dlt`, unconsolidated; the before reads
/// take `Old` alone. The published runs are the caller's own.
#[test]
fn after_reads_overlay_dlt_on_old() {
    let state = Q3State::default();
    assert!(state.old().unwrap_err().contains("publish"));
    let old = source(vec![vec![customer(2, "old", 1)]]);
    let dlt = run(vec![customer(2, "old", -1), customer(2, "new", 1)]);
    state.publish(old.clone(), dlt.clone()).unwrap();
    assert!(Arc::ptr_eq(&state.dlt().unwrap(), &dlt));
    assert!(Arc::ptr_eq(
        &state.old().unwrap().batches()[0],
        &old.batches()[0]
    ));
    state.collect(Pass::Seek).unwrap();
    let after = delta(vec![(Tup1(2), Tup3(20, Date::from_days(0), 0), 1)]);
    assert_eq!(
        tuples(&state.customers_after(&after).unwrap()),
        vec![(Tup1(2), Tup1(SqlString::from("new")), 1)]
    );
    state.finish().unwrap();

    // An empty `Dlt` touches no group, so no read can be asked of it.
    state.publish(old, run(vec![])).unwrap();
    state.collect(Pass::Seek).unwrap();
    assert!(state
        .customers_after(&after)
        .unwrap_err()
        .contains("outside the touched groups"));
    assert_eq!(state.finish().unwrap().counts.groups, 0);
}

#[test]
fn publication_collection_and_release_follow_one_order() {
    let state = Q3State::default();
    let request = delta(vec![(Tup2(2, 20), line_value(1, 100), 1)]);
    let joined = delta(vec![(
        Tup2(2, 20),
        Tup3(SqlString::from("A"), Date::from_days(0), 0),
        1,
    )]);
    assert!(state
        .lineitems_before(&joined)
        .unwrap_err()
        .contains("publish"));
    assert!(state.collect(Pass::Seek).unwrap_err().contains("publish"));
    state
        .publish(source(vec![]), run(vec![order(2, 20, 10, 1)]))
        .unwrap();
    assert!(state
        .publish(source(vec![]), run(vec![]))
        .unwrap_err()
        .contains("finish the previous"));
    // Reading before collecting is an error, not an empty answer.
    assert!(state
        .joined_orders_after(&request)
        .unwrap_err()
        .contains("collect"));
    state.collect(Pass::Seek).unwrap();
    assert!(state
        .collect(Pass::Seek)
        .unwrap_err()
        .contains("already collected"));
    assert!(tuples(&state.lineitems_before(&delta(Vec::new())).unwrap()).is_empty());
    state.finish().unwrap();
    assert!(state.finish().is_err());
}

#[test]
fn reconstruction_chunks_use_native_ordering_and_consolidation() {
    let mut output = RequestedRows::<Tup1<i64>, Tup1<i64>>::new();
    let count = splitter_output_chunk_size() + 1;
    for i in (0..count).rev() {
        output.push(Tup1(i as i64), Tup1(9), 1);
    }
    output.push(Tup1(0), Tup1(9), -1);
    let snapshot = output.finish();
    let actual = tuples(&snapshot);
    assert_eq!(actual.len(), count - 1);
    assert_eq!(actual.first(), Some(&(Tup1(1), Tup1(9), 1)));
    assert_eq!(actual.last(), Some(&(Tup1((count - 1) as i64), Tup1(9), 1)));
    assert!(actual.windows(2).all(|rows| rows[0].0 < rows[1].0));
}

/// The driving deltas are the readers' `Dlt` batches: each row carries its
/// trie keys, and a moved line item appears under both customers.
#[test]
fn driving_deltas_carry_trie_keys() {
    let state = published(
        vec![vec![
            customer(2, "old", 1),
            order(2, 20, 10, 1),
            line(2, 20, 1, 100, 1),
        ]],
        vec![
            customer(2, "old", -1),
            customer(2, "new", 1),
            order(3, 21, 11, 1),
            line(2, 20, 1, 100, -1),
            line(3, 20, 1, 100, 1),
        ],
    );
    let DrivingDeltas {
        customers,
        orders,
        lines,
    } = state.take_driving_deltas().unwrap();
    assert!(state
        .take_driving_deltas()
        .err()
        .is_some_and(|error| error.contains("already taken")));
    state.finish().unwrap();

    assert_eq!(
        tuples(&customers),
        vec![
            (Tup1(2), Tup1(SqlString::from("new")), 1),
            (Tup1(2), Tup1(SqlString::from("old")), -1),
        ]
    );
    assert_eq!(
        tuples(&orders),
        vec![(Tup1(3), Tup3(21, Date::from_days(11), 7), 1)]
    );
    assert_eq!(
        tuples(&lines),
        vec![
            (Tup2(2, 20), line_value(1, 100), -1),
            (Tup2(3, 20), line_value(1, 100), 1),
        ]
    );
}

// ---- The readers against a brute force ----

/// The summed weight of each `(key, payload)` over `runs`, zero sums dropped:
/// the consolidated rows of an index state.
fn consolidated(runs: &[&Vec<SourceRow>]) -> BTreeMap<(Key, Payload), i64> {
    let mut sums = BTreeMap::new();
    for rows in runs {
        for (key, payload, weight) in rows.iter() {
            *sums.entry((key.clone(), payload.clone())).or_insert(0) += weight;
        }
    }
    sums.retain(|_, weight| *weight != 0);
    sums
}

fn wanted<K: Ord, V: Ord>(rows: Vec<(K, V, i64)>) -> Vec<(K, V, i64)> {
    let mut sums = BTreeMap::new();
    for (key, value, weight) in rows {
        *sums.entry((key, value)).or_insert(0) += weight;
    }
    sums.into_iter()
        .filter(|(_, weight)| *weight != 0)
        .map(|((key, value), weight)| (key, value, weight))
        .collect()
}

/// A random row of a small trie: customers 1 to 3, orders 1 to 3, and line
/// numbers 1 to 2, with two payloads each and a nonzero weight.
fn source_rows() -> impl Strategy<Value = Vec<SourceRow>> {
    let weight = prop_oneof![Just(-2_i64), Just(-1), Just(1), Just(2)];
    let row = (1_i64..=3, 1_i64..=3, 1_i32..=2, 0_u8..3, 0_i32..2, weight).prop_map(
        |(custkey, orderkey, linenumber, stream, variant, weight)| match stream {
            0 => customer(custkey, ["A", "B"][variant as usize], weight),
            1 => order(custkey, orderkey, variant, weight),
            _ => line(
                custkey,
                orderkey,
                linenumber,
                100 + i128::from(variant),
                weight,
            ),
        },
    );
    proptest::collection::vec(row, 0..14)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// Every reader, asked about every node of every touched customer, equals
    /// a brute force over the consolidated `Old` and `Old + Dlt`, whatever the
    /// split of `Old` into runs and the pass.
    #[test]
    fn readers_match_a_brute_force(
        first_run in source_rows(),
        second_run in source_rows(),
        dlt_rows in source_rows(),
        step_pass in any::<bool>(),
    ) {
        let state = Q3State::default();
        state
            .publish(
                source(vec![first_run.clone(), second_run.clone()]),
                run(dlt_rows.clone()),
            )
            .unwrap();
        state
            .collect(if step_pass { Pass::Step } else { Pass::Seek })
            .unwrap();

        let before = consolidated(&[&first_run, &second_run]);
        let dlt = consolidated(&[&dlt_rows]);
        let after = consolidated(&[&first_run, &second_run, &dlt_rows]);
        let touched: BTreeSet<i64> = dlt
            .keys()
            .map(|(key, _)| match key {
                Key::CustomerByCustkey(k) => k.custkey,
                Key::OrdersByCustkeyOrderkey(k) => k.custkey,
                Key::LineitemByCustkeyOrderkeyLinenumber(k) => k.custkey,
            })
            .collect();
        let nodes: Vec<(i64, i64)> = touched
            .iter()
            .flat_map(|&c| (1..=3).map(move |o| (c, o)))
            .collect();
        let dummy_order = Tup3(0, Date::from_days(0), 0);
        let dummy_segment = Tup1(SqlString::from("request"));
        let dummy_joined = Tup3(SqlString::from("request"), Date::from_days(0), 0);

        let customers: Vec<_> = after
            .iter()
            .filter_map(|((key, payload), weight)| match (key, payload) {
                (Key::CustomerByCustkey(k), Payload::CustomerByCustkey(p))
                    if touched.contains(&k.custkey) =>
                {
                    Some((Tup1(k.custkey), Tup1(p.c_mktsegment.clone()), *weight))
                }
                _ => None,
            })
            .collect();
        prop_assert_eq!(
            tuples(
                &state
                    .customers_after(&delta(
                        touched.iter().map(|&c| (Tup1(c), dummy_order, 1)).collect()
                    ))
                    .unwrap()
            ),
            wanted(customers)
        );

        let orders: Vec<_> = before
            .iter()
            .filter_map(|((key, payload), weight)| match (key, payload) {
                (Key::OrdersByCustkeyOrderkey(k), Payload::OrdersByCustkeyOrderkey(p))
                    if touched.contains(&k.custkey) =>
                {
                    Some((
                        Tup1(k.custkey),
                        Tup3(k.orderkey, p.o_orderdate, p.o_shippriority),
                        *weight,
                    ))
                }
                _ => None,
            })
            .collect();
        prop_assert_eq!(
            tuples(
                &state
                    .orders_before(&delta(
                        touched.iter().map(|&c| (Tup1(c), dummy_segment.clone(), 1)).collect()
                    ))
                    .unwrap()
            ),
            wanted(orders)
        );

        let mut joined = Vec::new();
        for ((customer_key, customer_payload), customer_weight) in &after {
            for ((order_key, order_payload), order_weight) in &after {
                if let (
                    Key::CustomerByCustkey(c),
                    Payload::CustomerByCustkey(cp),
                    Key::OrdersByCustkeyOrderkey(o),
                    Payload::OrdersByCustkeyOrderkey(op),
                ) = (customer_key, customer_payload, order_key, order_payload)
                {
                    if c.custkey == o.custkey && touched.contains(&c.custkey) {
                        joined.push((
                            Tup2(o.custkey, o.orderkey),
                            Tup3(cp.c_mktsegment.clone(), op.o_orderdate, op.o_shippriority),
                            customer_weight * order_weight,
                        ));
                    }
                }
            }
        }
        prop_assert_eq!(
            tuples(
                &state
                    .joined_orders_after(&delta(
                        nodes.iter().map(|&(c, o)| (Tup2(c, o), line_value(0, 0), 1)).collect()
                    ))
                    .unwrap()
            ),
            wanted(joined)
        );

        let lines: Vec<_> = before
            .iter()
            .filter_map(|((key, payload), weight)| match (key, payload) {
                (
                    Key::LineitemByCustkeyOrderkeyLinenumber(k),
                    Payload::LineitemByCustkeyOrderkeyLinenumber(p),
                ) if touched.contains(&k.custkey) => Some((
                    Tup2(k.custkey, k.orderkey),
                    Tup4(k.linenumber, p.l_extendedprice, p.l_discount, p.l_shipdate),
                    *weight,
                )),
                _ => None,
            })
            .collect();
        prop_assert_eq!(
            tuples(
                &state
                    .lineitems_before(&delta(
                        nodes.iter().map(|&(c, o)| (Tup2(c, o), dummy_joined.clone(), 1)).collect()
                    ))
                    .unwrap()
            ),
            wanted(lines)
        );
        state.finish().unwrap();
    }
}

/// Copies `run` into a batch in a file.
fn file_run(run: &MergedIndexBatch) -> MergedIndexBatch {
    let factories = OrdIndexedWSetFactories::<DynData, DynData, DynZWeight>::new::<
        Vec<u8>,
        StoredPayload,
        i64,
    >();
    let mut builder = <MergedIndexBatch as Batch>::Builder::with_capacity_in_location(
        &factories,
        run.key_count(),
        run.len(),
        Some(BatchLocation::Storage),
    );
    let mut cursor = run.cursor();
    while cursor.key_valid() {
        while cursor.val_valid() {
            let weight = **cursor.weight();
            builder.push_val_diff(cursor.val(), weight.erase());
            cursor.step_val();
        }
        builder.push_key(cursor.key());
        cursor.step_key();
    }
    builder.done()
}

/// Runs `f` on the one worker of a runtime with storage. Only batches built
/// for storage go there, so the readers' driving deltas and outputs stay in
/// memory and add no file reads.
fn in_storage_runtime(f: impl FnOnce() + Clone + Send + 'static) {
    let storage_dir = tempfile::tempdir().unwrap();
    let config = CircuitConfig::with_workers(1).with_storage(Some(
        CircuitStorageConfig::for_config(
            StorageConfig {
                path: storage_dir.path().to_string_lossy().into_owned(),
                cache: Default::default(),
            },
            StorageOptions::default(),
        )
        .unwrap(),
    ));
    let _handle = Runtime::init_circuit(config, move |_| {
        f();
        Ok(())
    })
    .unwrap();
}

/// The run reads that `f` makes on this thread for the merged index, and for
/// every other owner together.
fn run_reads_of<T>(f: impl FnOnce() -> T) -> (T, RunReads, RunReads) {
    let index = || RunReads::current_for(WriteOwner::MergedIndex);
    let (total_before, index_before) = (RunReads::current(), index());
    let result = f();
    let index_reads = index().since(index_before);
    let total_reads = RunReads::current().since(total_before);
    (result, index_reads, total_reads.since(index_reads))
}

/// The worked transaction's `Old` and `Dlt` runs. Needs a runtime with
/// storage, since `Old`'s first run is in a file.
fn worked_runs() -> (MergedIndexBatch, MergedIndexBatch, MergedIndexBatch) {
    // `Old`: a file run with customer 5 (orders 3 and 4, line items 7, 8,
    // and 9) and customer 6, plus an in-memory run with customer 7.
    let old_file = file_run(
        &build_batch(vec![
            customer(5, "old", 1),
            order(5, 3, 10, 1),
            line(5, 3, 7, 100, 1),
            line(5, 3, 8, 100, 1),
            order(5, 4, 10, 1),
            line(5, 4, 9, 100, 1),
            customer(6, "other", 1),
        ])
        .unwrap(),
    );
    let old_memory = build_batch(vec![customer(7, "later", 1)]).unwrap();
    // `Dlt`: customer 5 replaced, and order 5 inserted with line items 10
    // and 11.
    let dlt = build_batch(vec![
        customer(5, "old", -1),
        customer(5, "new", 1),
        order(5, 5, 11, 1),
        line(5, 5, 10, 200, 1),
        line(5, 5, 11, 200, 1),
    ])
    .unwrap();
    assert_eq!(old_file.location(), BatchLocation::Storage);
    assert_eq!(old_memory.location(), BatchLocation::Memory);
    assert_eq!(dlt.location(), BatchLocation::Memory);
    (old_file, old_memory, dlt)
}

/// `collect` is the scan session of the worked transaction (see
/// `session_run_reads_on_the_worked_transaction`), and `finish` reports what it
/// read. The four partner reads then read the index not at all: they read the
/// readers' batches, which collection built.
#[test]
fn collect_reads_the_index_once_and_the_readers_not_at_all() {
    in_storage_runtime(|| {
        let (old_file, old_memory, dlt) = worked_runs();
        let factories = OrdIndexedWSetFactories::<DynData, DynData, DynZWeight>::new::<
            Vec<u8>,
            StoredPayload,
            i64,
        >();
        let old = SourceSnapshot::with_batches(
            &factories,
            vec![Arc::new(old_file), Arc::new(old_memory)],
        );
        let state = Q3State::default();
        state.publish(old, Arc::new(dlt)).unwrap();

        let (collected, collecting, collecting_other) = run_reads_of(|| state.collect(Pass::Seek));
        collected.unwrap();
        assert_eq!(collecting_other, RunReads::default());

        let orders_delta = delta(vec![(Tup1(5), Tup3(5, Date::from_days(11), 7), 1)]);
        let customers_delta = delta(vec![
            (Tup1(5), Tup1(SqlString::from("old")), -1),
            (Tup1(5), Tup1(SqlString::from("new")), 1),
        ]);
        let lines_delta = delta(
            [10, 11]
                .map(|linenumber| (Tup2(5, 5), line_value(linenumber, 200), 1))
                .to_vec(),
        );
        let joined_delta = delta(
            [3, 4, 5]
                .map(|order| {
                    (
                        Tup2(5, order),
                        Tup3(SqlString::from("new"), Date::from_days(10), 7),
                        1,
                    )
                })
                .to_vec(),
        );
        let (customers, c_reads, _) = run_reads_of(|| state.customers_after(&orders_delta));
        let (orders, o_reads, _) = run_reads_of(|| state.orders_before(&customers_delta));
        let (joined, j_reads, _) = run_reads_of(|| state.joined_orders_after(&lines_delta));
        let (lines, l_reads, _) = run_reads_of(|| state.lineitems_before(&joined_delta));
        for reads in [c_reads, o_reads, j_reads, l_reads] {
            assert_eq!(reads, RunReads::default());
        }

        assert_eq!(
            tuples(&customers.unwrap()),
            vec![(Tup1(5), Tup1(SqlString::from("new")), 1)]
        );
        assert_eq!(
            tuples(&orders.unwrap()),
            vec![
                (Tup1(5), Tup3(3, Date::from_days(10), 7), 1),
                (Tup1(5), Tup3(4, Date::from_days(10), 7), 1),
            ]
        );
        assert_eq!(
            tuples(&joined.unwrap()),
            vec![(
                Tup2(5, 5),
                Tup3(SqlString::from("new"), Date::from_days(11), 7),
                1
            )]
        );
        assert_eq!(
            tuples(&lines.unwrap())
                .into_iter()
                .map(|(Tup2(_, order), Tup4(linenumber, ..), weight)| (order, linenumber, weight))
                .collect::<Vec<_>>(),
            vec![(3, 7, 1), (3, 8, 1), (4, 9, 1)]
        );

        // `finish` returns the session's counts and the reads that collecting
        // made, as the harness reports them: the pinned reads of the session
        // test.
        let report = state.finish().unwrap();
        assert_eq!(report.run_reads, collecting);
        assert_eq!(collecting, with_memory(reads(0, 0, 7, 6, 6), 5, 5, 5));
        assert_eq!(
            report.counts,
            SessionCounts {
                groups: 1,
                cursor_opens: 2,
                logical_seeks: 0,
                logical_steps: 6,
                positions: 7,
                payload_decodes_old: 6,
                payload_decodes_dlt: 5,
                cancelled: 0,
            }
        );
    });
}

/// The reads of a transaction that never collected are nothing, and the next
/// transaction starts from zero.
#[test]
fn finish_returns_each_transactions_session_reads() {
    let state = Q3State::default();
    let one_run = || source(vec![vec![customer(5, "only", 1)]]);

    state.publish(one_run(), run(vec![])).unwrap();
    assert_eq!(state.finish().unwrap(), SessionReads::default());

    // One touched group: the session opens a cursor over `Old` and `Dlt`,
    // reads the old customer once, and the delta's two entries.
    state
        .publish(
            one_run(),
            run(vec![customer(5, "only", -1), customer(5, "new", 1)]),
        )
        .unwrap();
    state.collect(Pass::Seek).unwrap();
    let first = state.finish().unwrap();
    assert_eq!(first.counts.groups, 1);
    assert_eq!(first.counts.payload_decodes_old, 1);
    assert_eq!(first.counts.payload_decodes_dlt, 2);

    state.publish(one_run(), run(vec![])).unwrap();
    state.collect(Pass::Seek).unwrap();
    assert_eq!(state.finish().unwrap().counts.groups, 0);

    let mut total = first;
    total += first;
    assert_eq!(total.counts.groups, 2);
    assert_eq!(
        total.run_reads.memory_key_reads,
        2 * first.run_reads.memory_key_reads
    );
}

/// A synthetic segment filter: rejects an old customer outside `BUILDING`.
fn outside_building_segment(entry: &Entry<Payload>) -> bool {
    matches!(
        &entry.payload,
        Payload::CustomerByCustkey(customer)
            if customer.c_mktsegment != SqlString::from("BUILDING")
    )
}

type Groups = Vec<(Vec<u8>, GroupBuffers<Payload>)>;

/// Runs `Maintain` over the three COL streams, returning the groups and the
/// session counts, with the run reads of the merged index and of every other
/// owner.
fn maintain_col(
    old: &SourceSnapshot,
    dlt: &MergedIndexBatch,
    pass: Pass,
    reject: bool,
) -> ((Groups, SessionCounts), RunReads, RunReads) {
    run_reads_of(|| {
        with_read_owner(WriteOwner::MergedIndex, || {
            let mut session = ScanSession::<CustomerOrdersLineitemDefinition>::new(old, dlt, pass);
            let mut groups = Vec::new();
            let mut reject_entry = outside_building_segment;
            session
                .maintain(
                    &BTreeSet::from([
                        INDEX_ID_CUSTOMER_BY_CUSTKEY,
                        INDEX_ID_ORDERS_BY_CUSTKEY_ORDERKEY,
                        INDEX_ID_LINEITEM_BY_CUSTKEY_ORDERKEY_LINENUMBER,
                    ]),
                    reject.then_some(&mut reject_entry as &mut dyn FnMut(&Entry<Payload>) -> bool),
                    |group, buffers| {
                        groups.push((group.to_vec(), buffers));
                        Ok(())
                    },
                )
                .unwrap();
            (groups, session.counts())
        })
    })
}

/// How many `(old, dlt)` entries a group buffers for customers, orders, and
/// line items.
fn buffered(buffers: &GroupBuffers<Payload>) -> [(usize, usize); 3] {
    [
        INDEX_ID_CUSTOMER_BY_CUSTKEY,
        INDEX_ID_ORDERS_BY_CUSTKEY_ORDERKEY,
        INDEX_ID_LINEITEM_BY_CUSTKEY_ORDERKEY_LINENUMBER,
    ]
    .map(|id| (buffers.old(id).len(), buffers.dlt(id).len()))
}

/// `reads` with the accesses of the in-memory runs.
fn with_memory(file: RunReads, keys: u64, values: u64, weights: u64) -> RunReads {
    RunReads {
        memory_key_reads: keys,
        memory_value_reads: values,
        memory_weight_reads: weights,
        ..file
    }
}

fn reads(
    seeks: u64,
    filter_rejections: u64,
    key_decodes: u64,
    value_decodes: u64,
    weight_decodes: u64,
) -> RunReads {
    RunReads {
        seeks,
        filter_rejections,
        key_decodes,
        value_decodes,
        weight_decodes,
        ..RunReads::default()
    }
}

/// The scan session on the worked transaction, under `Seek`: one group, and the storage reads of the same index state as
/// the step 1 baseline of today's four readers, which
/// the doc keeps as history (22 value decodes there, against 6 here).
#[test]
fn session_run_reads_on_the_worked_transaction() {
    in_storage_runtime(|| {
        let (old_file, old_memory, dlt) = worked_runs();
        let factories = OrdIndexedWSetFactories::<DynData, DynData, DynZWeight>::new::<
            Vec<u8>,
            StoredPayload,
            i64,
        >();
        let old = SourceSnapshot::with_batches(
            &factories,
            vec![Arc::new(old_file), Arc::new(old_memory)],
        );

        let ((groups, counts), index, other) = maintain_col(&old, &dlt, Pass::Seek, false);

        // One group, c5, with the buffers of the worked transaction: the old
        // entries c5, o3, o4, l7, l8, l9, and the delta's two entries of c5
        // (-old, +new), o5, l10, and l11.
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].0, prefix_custkey(5));
        assert_eq!(buffered(&groups[0].1), [(1, 2), (2, 1), (3, 2)]);

        // The write-up predicted one logical seek and two per-run seeks, for
        // the pass to c5. There are none: `Old` opens on c5, the smallest key
        // of both runs, so the seek to c5's prefix finds every run at or past
        // it and moves nothing. The A/B test below pins seeks that move runs.
        //
        // The positions are the seven the index has up to c6: c5 at open,
        // then o3, l7, l8, o4, l9, and c6, which ends the group. Six steps
        // reach them. c7, in the memory run, is never reached.
        assert_eq!(
            counts,
            SessionCounts {
                groups: 1,
                cursor_opens: 2,
                logical_seeks: 0,
                logical_steps: 6,
                positions: 7,
                payload_decodes_old: 6,
                payload_decodes_dlt: 5,
                cancelled: 0,
            }
        );

        // Seven key decodes, one for each position (the memory run decodes
        // nothing). Six value and six weight decodes, for the six old entries
        // taken; c6, which only ends the group, costs none of them. No seek.
        //
        // Memory: the memory run M opens on c7, one key read, and is never
        // moved or taken (the group ends at c6). `Dlt` is read whole: its
        // keys c5, o5, l10, and l11 are four key reads (the step past l11
        // lands on nothing), and its five entries (c5 has two) are five
        // value reads and five weight reads. In all: 1 + 4 keys, 5 values,
        // 5 weights.
        assert_eq!(index, with_memory(reads(0, 0, 7, 6, 6), 5, 5, 5),);
        // The session reads nothing outside its own owner.
        assert_eq!(other, RunReads::default());
    });
}

/// The A/B of the `Pass` knob on one transaction with untouched customers
/// between the touched ones and a segment filter.
///
/// `Old` holds, in key order (file run `F`, but `c7` in memory run `M`):
///
/// ```text
///  1 c3   2 o1   3 l1                                   untouched
///  4 c5   5 o3   6 l7   7 l8   8 o4   9 l9              touched
/// 10 c6  11 o6  12 l12                                  untouched
/// 13 c7 (M)                                             untouched
/// 14 c8  15 o8  16 l13                                  touched, segment AUTO
/// 17 c9  18 o9                                          untouched
/// ```
///
/// `Dlt` touches c5 (its segment changes, and order 5 gains two line items),
/// c8 (a new line item), and c10 (a new customer with an order, past the end
/// of `Old`). Streams customer, orders, and line items are all inputs. The
/// filter rejects c8, so its order and line item are passed unread.
#[test]
fn the_passes_pass_untouched_groups_and_rejected_subtrees_in_different_ways() {
    in_storage_runtime(|| {
        let old_file = file_run(
            &build_batch(vec![
                customer(3, "AUTO", 1),
                order(3, 1, 10, 1),
                line(3, 1, 1, 100, 1),
                customer(5, "BUILDING", 1),
                order(5, 3, 10, 1),
                line(5, 3, 7, 100, 1),
                line(5, 3, 8, 100, 1),
                order(5, 4, 10, 1),
                line(5, 4, 9, 100, 1),
                customer(6, "AUTO", 1),
                order(6, 6, 10, 1),
                line(6, 6, 12, 100, 1),
                customer(8, "AUTO", 1),
                order(8, 8, 10, 1),
                line(8, 8, 13, 100, 1),
                customer(9, "BUILDING", 1),
                order(9, 9, 10, 1),
            ])
            .unwrap(),
        );
        let old_memory = build_batch(vec![customer(7, "AUTO", 1)]).unwrap();
        let dlt = build_batch(vec![
            customer(5, "BUILDING", -1),
            customer(5, "MACHINERY", 1),
            order(5, 5, 11, 1),
            line(5, 5, 10, 200, 1),
            line(5, 5, 11, 200, 1),
            line(8, 8, 14, 200, 1),
            customer(10, "BUILDING", 1),
            order(10, 10, 12, 1),
        ])
        .unwrap();
        let factories = OrdIndexedWSetFactories::<DynData, DynData, DynZWeight>::new::<
            Vec<u8>,
            StoredPayload,
            i64,
        >();
        let old = SourceSnapshot::with_batches(
            &factories,
            vec![Arc::new(old_file), Arc::new(old_memory)],
        );

        let ((seek_groups, seek), seek_reads, seek_other) =
            maintain_col(&old, &dlt, Pass::Seek, true);
        let ((step_groups, step), step_reads, step_other) =
            maintain_col(&old, &dlt, Pass::Step, true);

        // The outputs are equal, and are the three touched groups. c5 buffers
        // its old entries and the delta's. c8's customer stays buffered
        // though rejected, and its order and line item are never read; its
        // delta line item is. c10 has no old entry.
        assert_eq!(step_groups, seek_groups);
        let touched: Vec<_> = seek_groups.iter().map(|(group, _)| group.clone()).collect();
        assert_eq!(touched, [5, 8, 10].map(prefix_custkey));
        let shapes: Vec<_> = seek_groups.iter().map(|(_, b)| buffered(b)).collect();
        assert_eq!(
            shapes,
            [
                [(1, 2), (2, 1), (3, 2)],
                [(1, 0), (0, 0), (0, 1)],
                [(0, 1), (0, 1), (0, 0)],
            ]
        );
        for other in [seek_other, step_other] {
            assert_eq!(other, RunReads::default());
        }

        // Both passes decode the same payloads and the same
        // values and weights: the seven buffered old entries (c5, o3, l7, l8,
        // o4, l9, and the rejected c8), and the eight delta entries.
        for counts in [seek, step] {
            assert_eq!(counts.groups, 3);
            assert_eq!(counts.cursor_opens, 2);
            assert_eq!(counts.payload_decodes_old, 7);
            assert_eq!(counts.payload_decodes_dlt, 8);
            assert_eq!(counts.cancelled, 0);
        }
        for reads in [seek_reads, step_reads] {
            assert_eq!((reads.value_decodes, reads.weight_decodes), (7, 7));
        }

        // Seek. Open on c3 (position 1).
        // - c5: the seek moves F from c3 to c5 (run seek 1, position 2).
        //   Collect takes c5 and steps through o3, l7, l8, o4, l9 (positions
        //   3-7) to c6 (8), which ends the group: 6 steps.
        // - c8: the seek moves F from c6 and M from c7 (run seeks 2 and 3),
        //   landing on c8 (9); M is exhausted. c8 is rejected, so the skip
        //   seeks past [1, c8] (run seek 4) to c9 (10), with no step.
        // - c10: the seek moves F from c9 (run seek 5) past the end.
        // 4 logical seeks, 5 run seeks, 10 positions; F decodes a key for each
        // of them (the memory run decodes nothing, and the seek past the end
        // lands on no key).
        //
        // Memory, for both passes: `Dlt` is read whole, and its keys are c5,
        // o5, l10, l11, l14 (c8's), c10, and o10: 7 key reads for 8 entries
        // (c5 has two), so 8 values and 8 weights. M opens on c7: 1 more key
        // read. Its value is never taken (c7's group is not touched), and the
        // seek or step past c7 lands on nothing. In all: 8 keys, 8 values,
        // 8 weights.
        assert_eq!(
            (seek.logical_seeks, seek.logical_steps, seek.positions),
            (4, 6, 10)
        );
        assert_eq!(seek_reads, with_memory(reads(5, 0, 10, 7, 7), 8, 8, 8));

        // Step. Every key of the index is a position, 18 in
        // all, and every one is stepped past once: 18 steps and, F having 17
        // of the keys, 17 key decodes (c7 is in memory). No seek. The memory
        // reads are the seek pass's: `Dlt` is read whole, and M's one key is
        // read at the opening and never taken.
        assert_eq!(
            (step.logical_seeks, step.logical_steps, step.positions),
            (0, 18, 18)
        );
        assert_eq!(step_reads, with_memory(reads(0, 0, 17, 7, 7), 8, 8, 8));
    });
}
