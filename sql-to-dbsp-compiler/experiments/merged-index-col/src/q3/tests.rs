use std::collections::{BTreeMap, BTreeSet};

use super::*;
use crate::{
    build_batch, prefix_custkey, CustomerByCustkeyKey, CustomerByCustkeyPayload,
    LineitemByCustkeyOrderkeyLinenumberKey, LineitemByCustkeyOrderkeyLinenumberPayload,
    OrdersByCustkeyOrderkeyKey, OrdersByCustkeyOrderkeyPayload, StoredPayload,
};
use dbsp::{
    circuit::{CircuitConfig, CircuitStorageConfig},
    dynamic::{DowncastTrait, DynData, Erase},
    storage::attribution::Component,
    trace::{
        merged_index::{Pass, ScanSession, SessionCounts},
        Batch, BatchLocation, BatchReaderFactories, Builder, OrdIndexedWSetFactories, RunReads,
        WithSnapshot,
    },
    typed_batch::TypedBatch,
    DynZWeight, Runtime,
};
use feldera_types::config::{StorageConfig, StorageOptions};
use proptest::{prelude::*, test_runner::TestCaseError};

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

/// A state whose partner reads check their contract, as the tests and the validation runs do;
/// the measured driver's state does not.
fn checked_state() -> Q3State {
    let state = Q3State::default();
    state.enable_contract_check();
    state
}

/// Publishes `old` and `dlt` and collects their groups with the `Seek` pass.
fn published(old: Vec<Vec<SourceRow>>, dlt: Vec<SourceRow>) -> Q3State {
    let state = checked_state();
    state.publish(source(old), run(dlt)).unwrap();
    state.collect(Pass::Seek).unwrap();
    state
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
    let index = || RunReads::current_for(Component::MergedIndex);
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

/// A driving delta as the snapshot that a join hands its reader.
fn snap<K: DBData, V: DBData>(batch: OrdIndexedZSet<K, V>) -> SpineSnapshot<OrdIndexedZSet<K, V>> {
    TypedBatch::new(batch.into_inner().into_ro_snapshot())
}

/// The readers of a published and collected `state`.
fn with_readers<T>(state: &Q3State, f: impl FnOnce(&CollectedReaders) -> T) -> T {
    let views = state.views().unwrap();
    let readers = views.collected_readers().unwrap();
    f(&readers)
}

fn requests_of<R: ReaderDefinition>(reader: &Reader<R>) -> Vec<Vec<u8>> {
    reader.requests().iter().map(<[u8]>::to_vec).collect()
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
    let DrivingDeltas {
        customers: customers_delta,
        orders: orders_delta,
        lines: lines_delta,
    } = state.take_driving_deltas().unwrap();
    let joined_delta = delta(vec![(
        Tup2(2, 20),
        Tup3(SqlString::from("new"), Date::from_days(11), 7),
        1,
    )]);

    // Each driving delta is its owner's batch of `Dlt`'s rows, unfolded.
    assert_eq!(
        tuples(&orders_delta),
        vec![
            (Tup1(2), Tup3(20, Date::from_days(10), 7), -3),
            (Tup1(2), Tup3(20, Date::from_days(11), 7), 1),
        ]
    );

    // `C⁺`: `Old`'s customer (weight 2) and `ΔC`'s rows, consolidated.
    assert_eq!(
        tuples(&state.customers_after(&snap(orders_delta)).unwrap()),
        vec![(Tup1(2), Tup1(SqlString::from("new")), 1)]
    );
    assert_eq!(
        tuples(&state.orders_before(&snap(customers_delta)).unwrap()),
        vec![(Tup1(2), Tup3(20, Date::from_days(10), 7), 3)]
    );
    assert_eq!(
        tuples(&state.joined_orders_after(&snap(lines_delta)).unwrap()),
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
            line(2, 20, 1, 100, 1),
            line(3, 20, 1, 100, 1),
            line(3, 21, 1, 100, 1),
        ],
    );
    let lines = state.take_driving_deltas().unwrap().lines;
    assert_eq!(
        tuples(&state.joined_orders_after(&snap(lines)).unwrap()),
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
    state.finish().unwrap();

    // `ΔC` changes both customers, so `ΔJ` has a row at each of their orders:
    // `L⁻` keeps those orders' line items before the transaction. It keeps
    // no line item of the order `(2, 21)`, which no one asks for.
    let state_with_lines = published(
        vec![vec![
            order(2, 20, 10, 1),
            order(3, 20, 10, 1),
            order(2, 21, 10, 1),
            line(2, 20, 1, 100, 1),
            line(3, 20, 5, 100, 1),
            line(2, 22, 1, 100, 1),
        ]],
        vec![customer(2, "x", 1), customer(3, "y", 1)],
    );
    let joined = delta(vec![
        (
            Tup2(3, 20),
            Tup3(SqlString::from("y"), Date::from_days(10), 7),
            1,
        ),
        (
            Tup2(2, 20),
            Tup3(SqlString::from("x"), Date::from_days(10), 7),
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
    // The order `(2, 21)` has a row in `ΔJ` too (it is in `ΔC`'s customer), and
    // is requested, but holds no line item.
    let with_empty = delta(vec![(
        Tup2(2, 21),
        Tup3(SqlString::from("x"), Date::from_days(10), 7),
        1,
    )]);
    assert!(tuples(&state_with_lines.lineitems_before(&with_empty).unwrap()).len() == 2);
    // `(2, 22)` has a line item but no order: `ΔJ` has no row there, so a
    // request for it is outside `L⁻`'s requests.
    let stray = delta(vec![(
        Tup2(2, 22),
        Tup3(SqlString::from("x"), Date::from_days(10), 7),
        1,
    )]);
    assert!(state_with_lines
        .lineitems_before(&stray)
        .unwrap_err()
        .contains("outside the requests"));
}

#[test]
fn joined_projection_multiplies_signed_weights_and_ignores_unrequested_ranges() {
    let state = published(
        vec![],
        vec![
            customer(2, "A", -2),
            order(2, 20, 10, 3),
            order(2, 21, 10, 7),
            line(2, 20, 1, 100, 1),
            customer(9, "unused", 100),
            order(9, 90, 10, 100),
        ],
    );
    let lines = state.take_driving_deltas().unwrap().lines;
    assert_eq!(
        tuples(&state.joined_orders_after(&snap(lines)).unwrap()),
        vec![(
            Tup2(2, 20),
            Tup3(SqlString::from("A"), Date::from_days(10), 7),
            -6
        )]
    );
}

#[test]
fn cancelled_keys_have_no_values_and_unrequested_keys_are_refused() {
    // Customer 2's row cancels against `ΔC`, and its order is inserted.
    let state = published(
        vec![vec![customer(2, "A", 1), customer(3, "B", 1)]],
        vec![customer(2, "A", -1), order(2, 20, 0, 1)],
    );
    let requested = delta(vec![(Tup1(2), Tup3(20, Date::from_days(0), 0), 1)]);
    assert!(tuples(&state.customers_after(&requested).unwrap()).is_empty());
    assert!(tuples(
        &state
            .orders_before(&delta(vec![(Tup1(2), Tup1(SqlString::from("A")), 1)]))
            .unwrap()
    )
    .is_empty());

    // Customer 3 exists in `Old` but `Dlt` does not touch it: no driving
    // delta can name it, so a request for it breaks the contract. So does a
    // customer that `Dlt` touches but `ΔO` does not name, and an order that
    // no line item of `Dlt` names.
    let untouched = delta(vec![(Tup1(3), Tup3(30, Date::from_days(0), 0), 1)]);
    assert!(state
        .customers_after(&untouched)
        .unwrap_err()
        .contains("outside the requests"));
    let missing = delta(vec![(Tup2(4, 40), line_value(1, 100), 1)]);
    assert!(state
        .joined_orders_after(&missing)
        .unwrap_err()
        .contains("outside the requests"));
    let not_in_delta_c = delta(vec![(Tup1(2), Tup1(SqlString::from("A")), 1)]);
    let unrequested_customer = delta(vec![(Tup1(7), Tup1(SqlString::from("A")), 1)]);
    assert!(state.orders_before(&not_in_delta_c).is_ok());
    assert!(state
        .orders_before(&unrequested_customer)
        .unwrap_err()
        .contains("outside the requests"));
}

/// `C⁺` is `Old`'s customer and `ΔC`'s rows of the customers of `ΔO`, and no
/// more: the slice holds the requested customers only, and `ΔC` is not shared
/// whole with it.
#[test]
fn the_after_read_is_the_requested_slice_of_old_and_dlt() {
    let state = checked_state();
    assert!(state.old().unwrap_err().contains("publish"));
    let old = source(vec![vec![customer(2, "old", 1), customer(4, "four", 1)]]);
    let dlt = run(vec![
        customer(2, "old", -1),
        customer(2, "new", 1),
        customer(3, "three", 1),
        customer(4, "four", -1),
        customer(4, "FOUR", 1),
        order(2, 20, 0, 1),
        order(4, 40, 0, 1),
    ]);
    state.publish(old.clone(), dlt.clone()).unwrap();
    assert!(Arc::ptr_eq(&state.dlt().unwrap(), &dlt));
    assert!(Arc::ptr_eq(
        &state.old().unwrap().batches()[0],
        &old.batches()[0]
    ));
    state.collect(Pass::Seek).unwrap();
    let driving = state.take_driving_deltas().unwrap();
    // `ΔC` holds all three customers' changes, and goes whole to the circuit.
    assert_eq!(tuples(&driving.customers).len(), 5);
    // `C⁺` holds the customers that `ΔO` requests (2 and 4), not customer 3
    // (`ΔC` only): one batch, one row per consolidated value.
    let after = state.customers_after(&snap(driving.orders)).unwrap();
    assert_eq!(after.inner().batches().len(), 1);
    assert_eq!(
        tuples(&after),
        vec![
            (Tup1(2), Tup1(SqlString::from("new")), 1),
            (Tup1(4), Tup1(SqlString::from("FOUR")), 1),
        ]
    );
    state.finish().unwrap();

    // An empty `Dlt` touches no group, so no read can be asked of it.
    state.publish(old, run(vec![])).unwrap();
    state.collect(Pass::Seek).unwrap();
    let asked = delta(vec![(Tup1(2), Tup3(20, Date::from_days(0), 0), 1)]);
    assert!(state
        .customers_after(&asked)
        .unwrap_err()
        .contains("outside the requests"));
    assert_eq!(state.finish().unwrap().counts.groups, 0);
}

#[test]
fn publication_collection_and_release_follow_one_order() {
    let state = checked_state();
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

/// The driving deltas are their owners' batches: each row carries its trie
/// keys, and a moved line item appears under both customers. They are moved
/// out, once.
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

type State = BTreeMap<(Key, Payload), i64>;

/// `J = O ⋈ C` of an index state, by `(custkey, orderkey, segment, date,
/// priority)`, zero sums dropped.
fn joined_rows(state: &State) -> BTreeMap<(i64, i64, SqlString, Date, i32), i64> {
    let mut joined = BTreeMap::new();
    for ((customer_key, customer_payload), customer_weight) in state {
        let (Key::CustomerByCustkey(c), Payload::CustomerByCustkey(cp)) =
            (customer_key, customer_payload)
        else {
            continue;
        };
        for ((order_key, order_payload), order_weight) in state {
            if let (Key::OrdersByCustkeyOrderkey(o), Payload::OrdersByCustkeyOrderkey(op)) =
                (order_key, order_payload)
            {
                if o.custkey == c.custkey {
                    *joined
                        .entry((
                            c.custkey,
                            o.orderkey,
                            cp.c_mktsegment.clone(),
                            op.o_orderdate,
                            op.o_shippriority,
                        ))
                        .or_insert(0) += customer_weight * order_weight;
                }
            }
        }
    }
    joined.retain(|_, weight| *weight != 0);
    joined
}

/// The nodes `(custkey, orderkey)` at which `ΔJ = J(after) - J(before)` has a
/// row.
fn delta_joined_keys(before: &State, after: &State) -> BTreeSet<(i64, i64)> {
    let mut difference: BTreeMap<_, i64> = BTreeMap::new();
    for (row, weight) in joined_rows(after) {
        *difference.entry(row).or_insert(0) += weight;
    }
    for (row, weight) in joined_rows(before) {
        *difference.entry(row).or_insert(0) -= weight;
    }
    difference
        .into_iter()
        .filter(|(_, weight)| *weight != 0)
        .map(|((custkey, orderkey, ..), _)| (custkey, orderkey))
        .collect()
}

fn custkey_of(key: &Key) -> i64 {
    match key {
        Key::CustomerByCustkey(k) => k.custkey,
        Key::OrdersByCustkeyOrderkey(k) => k.custkey,
        Key::LineitemByCustkeyOrderkeyLinenumber(k) => k.custkey,
    }
}

/// Collects `old_runs` and `dlt_rows` and checks every reader's requests and
/// partner batch against a brute force over the consolidated states: `C⁺` at
/// the customers of `ΔO`, `O⁻` at those of `ΔC`, `J⁺` at the nodes of `ΔL`,
/// and `L⁻` at the nodes where the brute-force `ΔJ` has a row.
fn check_readers(
    old_runs: Vec<Vec<SourceRow>>,
    dlt_rows: Vec<SourceRow>,
    pass: Pass,
) -> Result<(), TestCaseError> {
    let state = checked_state();
    state
        .publish(source(old_runs.clone()), run(dlt_rows.clone()))
        .unwrap();
    state.collect(pass).unwrap();

    let before = consolidated(&old_runs.iter().collect::<Vec<_>>());
    let dlt = consolidated(&[&dlt_rows]);
    let mut every_run = old_runs.clone();
    every_run.push(dlt_rows.clone());
    let after = consolidated(&every_run.iter().collect::<Vec<_>>());

    let customers_of_delta_orders: BTreeSet<i64> = dlt
        .keys()
        .filter(|(key, _)| matches!(key, Key::OrdersByCustkeyOrderkey(_)))
        .map(|(key, _)| custkey_of(key))
        .collect();
    let customers_of_delta_customers: BTreeSet<i64> = dlt
        .keys()
        .filter(|(key, _)| matches!(key, Key::CustomerByCustkey(_)))
        .map(|(key, _)| custkey_of(key))
        .collect();
    let nodes_of_delta_lines: BTreeSet<(i64, i64)> = dlt
        .keys()
        .filter_map(|(key, _)| match key {
            Key::LineitemByCustkeyOrderkeyLinenumber(k) => Some((k.custkey, k.orderkey)),
            _ => None,
        })
        .collect();
    let nodes_of_delta_joined = delta_joined_keys(&before, &after);

    with_readers(&state, |readers| {
        // The requests: each reader's driving delta, in key order.
        let groups = |customers: &BTreeSet<i64>| -> Vec<Vec<u8>> {
            customers.iter().map(|&c| prefix_custkey(c)).collect()
        };
        let nodes = |nodes: &BTreeSet<(i64, i64)>| -> Vec<Vec<u8>> {
            nodes
                .iter()
                .map(|&(c, o)| prefix_custkey_orderkey(c, o))
                .collect()
        };
        prop_assert_eq!(
            requests_of(&readers.customers),
            groups(&customers_of_delta_orders)
        );
        prop_assert_eq!(
            requests_of(&readers.orders),
            groups(&customers_of_delta_customers)
        );
        prop_assert_eq!(requests_of(&readers.joined), nodes(&nodes_of_delta_lines));
        prop_assert_eq!(requests_of(&readers.lines), nodes(&nodes_of_delta_joined));

        // `C⁺`: the customers after, at the customers of `ΔO`.
        let customers: Vec<_> = after
            .iter()
            .filter_map(|((key, payload), weight)| match (key, payload) {
                (Key::CustomerByCustkey(k), Payload::CustomerByCustkey(p))
                    if customers_of_delta_orders.contains(&k.custkey) =>
                {
                    Some((Tup1(k.custkey), Tup1(p.c_mktsegment.clone()), *weight))
                }
                _ => None,
            })
            .collect();
        prop_assert_eq!(
            tuples(&readers.customers.partner().unwrap()),
            wanted(customers)
        );

        // `O⁻`: the orders before, at the customers of `ΔC`.
        let orders: Vec<_> = before
            .iter()
            .filter_map(|((key, payload), weight)| match (key, payload) {
                (Key::OrdersByCustkeyOrderkey(k), Payload::OrdersByCustkeyOrderkey(p))
                    if customers_of_delta_customers.contains(&k.custkey) =>
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
        prop_assert_eq!(tuples(&readers.orders.partner().unwrap()), wanted(orders));

        // `J⁺`: `J` after, at the nodes of `ΔL`.
        let joined: Vec<_> = joined_rows(&after)
            .into_iter()
            .filter(|((custkey, orderkey, ..), _)| {
                nodes_of_delta_lines.contains(&(*custkey, *orderkey))
            })
            .map(|((custkey, orderkey, segment, date, priority), weight)| {
                (
                    Tup2(custkey, orderkey),
                    Tup3(segment, date, priority),
                    weight,
                )
            })
            .collect();
        prop_assert_eq!(tuples(&readers.joined.partner().unwrap()), wanted(joined));

        // `L⁻`: the line items before, at the nodes where `ΔJ` has a row.
        let lines: Vec<_> = before
            .iter()
            .filter_map(|((key, payload), weight)| match (key, payload) {
                (
                    Key::LineitemByCustkeyOrderkeyLinenumber(k),
                    Payload::LineitemByCustkeyOrderkeyLinenumber(p),
                ) if nodes_of_delta_joined.contains(&(k.custkey, k.orderkey)) => Some((
                    Tup2(k.custkey, k.orderkey),
                    Tup4(k.linenumber, p.l_extendedprice, p.l_discount, p.l_shipdate),
                    *weight,
                )),
                _ => None,
            })
            .collect();
        prop_assert_eq!(tuples(&readers.lines.partner().unwrap()), wanted(lines));
        Ok(())
    })?;
    // Whatever the delta, the collecting thread reads under no owner but the index's and the
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

/// A key of a small trie: customers 1 to 3, orders 1 to 3, and line numbers 1 to 2.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum SmallKey {
    Customer(i64),
    Order(i64, i64),
    Line(i64, i64, i32),
}

fn small_key() -> impl Strategy<Value = SmallKey> {
    prop_oneof![
        (1_i64..=3).prop_map(SmallKey::Customer),
        (1_i64..=3, 1_i64..=3).prop_map(|(c, o)| SmallKey::Order(c, o)),
        (1_i64..=3, 1_i64..=3, 1_i32..=2).prop_map(|(c, o, l)| SmallKey::Line(c, o, l)),
    ]
}

/// The row of `key` with one of three payloads (`variant`) and `weight`. The segments are ordered
/// against their packed form (`AAA` < `BB` < `C`, but the packed length comes first), so a
/// replaced segment arrives in `Dlt` against the typed order.
fn small_row(key: &SmallKey, variant: u8, weight: i64) -> SourceRow {
    match *key {
        SmallKey::Customer(c) => customer(c, ["AAA", "BB", "C"][usize::from(variant)], weight),
        SmallKey::Order(c, o) => order(c, o, i32::from(variant), weight),
        SmallKey::Line(c, o, l) => line(c, o, l, 100 + i128::from(variant), weight),
    }
}

/// A logical `Old` (each key holds one payload with weight 1 or 2, as the primary keys make it),
/// split at random into two runs, and a `Dlt` that replaces, deletes, or inserts some keys: for
/// each changed key, `-old` if it has one and `+new` if it gets one. A folded key then yields at
/// most three rows, its `Old` row and `Dlt`'s pair.
fn old_runs_and_dlt() -> impl Strategy<Value = (Vec<Vec<SourceRow>>, Vec<SourceRow>)> {
    let old =
        proptest::collection::btree_map(small_key(), (0_u8..3, 1_i64..=2, any::<bool>()), 0..10);
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
    #![proptest_config(ProptestConfig::with_cases(96))]

    /// Every reader's requests and partner batch equal a brute force over the
    /// consolidated `Old` and `Old + Dlt`, whatever the split of `Old` into
    /// runs and the pass. In particular `L⁻`'s requests are exactly the nodes
    /// where `ΔJ` has a row.
    #[test]
    fn readers_match_a_brute_force(
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

/// A segment that changes to one that is smaller as a string but longer when packed (length
/// first), so that `Dlt`'s `-old` and `+new` of the customer arrive against the typed order, and
/// the driving delta and the `C⁺` slice are built in typed order all the same.
#[test]
fn a_replaced_segment_is_built_in_typed_order_though_it_arrives_in_packed_order() {
    let state = published(
        vec![vec![customer(5, "C", 1), order(5, 50, 10, 1)]],
        vec![
            customer(5, "C", -1),
            customer(5, "AAA", 1),
            order(5, 51, 10, 1),
        ],
    );
    let DrivingDeltas {
        customers, orders, ..
    } = state.take_driving_deltas().unwrap();
    assert_eq!(
        tuples(&customers),
        vec![
            (Tup1(5), Tup1(SqlString::from("AAA")), 1),
            (Tup1(5), Tup1(SqlString::from("C")), -1),
        ]
    );
    // `C⁺`: `Old`'s `C` and `ΔC`'s pair, three rows of one entry, consolidate to `AAA`.
    assert_eq!(
        tuples(&state.customers_after(&snap(orders)).unwrap()),
        vec![(Tup1(5), Tup1(SqlString::from("AAA")), 1)]
    );
}

/// `ΔJ` on the general deltas that `reconstruction::prepare` produces: a
/// customer that changes its segment, an order that moves between customers
/// with its line item, and an order inserted at a customer that is inserted
/// in the same transaction.
#[test]
fn delta_joined_requests_follow_customer_changes_and_moved_orders() {
    for pass in [Pass::Seek, Pass::Step] {
        // A customer changes segment: `ΔJ` has a row at each of its orders.
        let segment_change = (
            vec![vec![
                customer(2, "A", 1),
                order(2, 20, 10, 1),
                order(2, 21, 10, 1),
                line(2, 20, 1, 100, 1),
                line(2, 21, 1, 100, 1),
            ]],
            vec![customer(2, "A", -1), customer(2, "B", 1)],
        );
        // An order and its line item move from customer 2 to customer 3.
        let moved_order = (
            vec![vec![
                customer(2, "A", 1),
                customer(3, "B", 1),
                order(2, 20, 10, 1),
                line(2, 20, 1, 100, 1),
            ]],
            vec![
                order(2, 20, 10, -1),
                order(3, 20, 10, 1),
                line(2, 20, 1, 100, -1),
                line(3, 20, 1, 100, 1),
            ],
        );
        // A customer and its first order arrive together; `Old` has neither.
        let arriving = (
            vec![vec![customer(1, "A", 1)]],
            vec![customer(5, "A", 1), order(5, 50, 10, 1)],
        );
        // An order arrives with no line item before it, and a line item with
        // no order, which `ΔJ` cannot reach.
        let strays = (
            vec![vec![customer(2, "A", 1), line(2, 22, 1, 100, 1)]],
            vec![
                order(2, 21, 10, 1),
                line(2, 21, 1, 100, 1),
                line(2, 22, 2, 100, 1),
            ],
        );
        for (old, dlt) in [segment_change, moved_order, arriving, strays] {
            check_readers(old, dlt, pass).unwrap();
        }
    }

    // The segment change's `L⁻` requests, spelled out.
    let state = published(
        vec![vec![
            customer(2, "A", 1),
            order(2, 20, 10, 1),
            order(2, 21, 10, 1),
            line(2, 20, 1, 100, 1),
        ]],
        vec![customer(2, "A", -1), customer(2, "B", 1)],
    );
    with_readers(&state, |readers| {
        assert_eq!(
            requests_of(&readers.lines),
            vec![
                prefix_custkey_orderkey(2, 20),
                prefix_custkey_orderkey(2, 21)
            ]
        );
        assert!(requests_of(&readers.customers).is_empty());
        assert!(requests_of(&readers.joined).is_empty());
    });
}

/// The contract check is off by default, as in the measured driver: a read of a key outside the
/// requests then returns the batch, and the partner read walks nothing of the circuit's delta.
#[test]
fn the_contract_check_is_off_unless_enabled() {
    let state = Q3State::default();
    state
        .publish(source(vec![]), run(vec![order(2, 20, 10, 1)]))
        .unwrap();
    state.collect(Pass::Seek).unwrap();
    let outside = delta(vec![(Tup1(9), Tup3(90, Date::from_days(0), 0), 1)]);
    let (result, _, other) = run_reads_of(|| state.customers_after(&outside));
    assert!(tuples(&result.unwrap()).is_empty());
    // Nothing of the delta was read under any owner.
    assert_eq!(other, RunReads::default());
    state.finish().unwrap();

    let checked = checked_state();
    checked
        .publish(source(vec![]), run(vec![order(2, 20, 10, 1)]))
        .unwrap();
    checked.collect(Pass::Seek).unwrap();
    assert!(checked
        .customers_after(&outside)
        .unwrap_err()
        .contains("outside the requests"));
}

/// `ΔJ` has no row where its two terms cancel: a customer arrives while its
/// order, which `Old` held without one, leaves.
#[test]
fn a_node_where_the_delta_terms_cancel_is_not_requested() {
    let state = published(
        vec![vec![order(5, 50, 10, 1), line(5, 50, 1, 100, 1)]],
        vec![customer(5, "A", 1), order(5, 50, 10, -1)],
    );
    with_readers(&state, |readers| {
        assert!(requests_of(&readers.lines).is_empty());
        assert!(tuples(&readers.lines.partner().unwrap()).is_empty());
    });
}

/// The routing orders every step by the folded keys: an order that `ΔO`
/// inserts between two orders of `Old` is stepped between them.
#[test]
fn an_inserted_order_is_stepped_between_the_orders_of_old() {
    let state = published(
        vec![vec![
            customer(2, "A", 1),
            order(2, 10, 1, 1),
            line(2, 10, 1, 100, 1),
            order(2, 30, 1, 1),
            line(2, 30, 1, 100, 1),
        ]],
        vec![
            order(2, 20, 1, 1),
            order(2, 40, 1, 1),
            line(2, 20, 1, 100, 1),
            line(2, 40, 1, 100, 1),
        ],
    );
    with_readers(&state, |readers| {
        assert_eq!(
            requests_of(&readers.lines),
            vec![
                prefix_custkey_orderkey(2, 20),
                prefix_custkey_orderkey(2, 40)
            ]
        );
        assert_eq!(
            tuples(&readers.joined.partner().unwrap())
                .into_iter()
                .map(|(Tup2(_, order), _, weight)| (order, weight))
                .collect::<Vec<_>>(),
            vec![(20, 1), (40, 1)]
        );
    });
}

// ---- The scan session on the worked transaction ----

/// The run reads that `collect` makes on this thread for the merged index, and
/// for every other owner together.
fn collect_reads_of(state: &Q3State, pass: Pass) -> (RunReads, RunReads) {
    let (_, index, other) = run_reads_of(|| state.collect(pass).unwrap());
    (index, other)
}

/// `collect` is one pass over `Dlt` and one scan session over the worked
/// transaction, and `finish` reports what they read. The four partner reads
/// then read the index not at all: they read the readers' batches, which
/// collection built.
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
        let state = checked_state();
        state.publish(old, Arc::new(dlt)).unwrap();

        let (collecting, collecting_other) = collect_reads_of(&state, Pass::Seek);
        // The routing's reads of the driving deltas `ΔC` and `ΔO`, which it merges forward
        // alongside the scan, are the requested-slice owner's: each cursor lands on its first
        // key (2 key reads), and customer 5's rows are read (`ΔC`'s two and `ΔO`'s one, 3 value
        // and 3 weight reads). No seek, and nothing is left to the default owner.
        assert_eq!(
            collecting_other,
            RunReads {
                memory_key_reads: 2,
                memory_value_reads: 3,
                memory_weight_reads: 3,
                ..RunReads::default()
            }
        );

        let DrivingDeltas {
            customers: customers_delta,
            orders: orders_delta,
            lines: lines_delta,
        } = state.take_driving_deltas().unwrap();
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
        let (customers, c_reads, _) = run_reads_of(|| state.customers_after(&snap(orders_delta)));
        let (orders, o_reads, _) = run_reads_of(|| state.orders_before(&snap(customers_delta)));
        let (joined, j_reads, _) = run_reads_of(|| state.joined_orders_after(&snap(lines_delta)));
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
        // made, as the harness reports them.
        //
        // One group, c5, and every entry of it is kept: `ΔO` requests c5 (so
        // `C⁺` keeps c5), `ΔC` requests c5 (so `O⁻` keeps o3 and o4), and
        // `ΔJ` has a row at (5, 3), (5, 4), and (5, 5) (so `L⁻` keeps l7, l8,
        // and l9). Six payloads are decoded, as when the group was buffered
        // whole; five of `Dlt` (`ΔC` two, `ΔO` one, `ΔL` two), now in the
        // capture. One cursor is opened, over `Old`: the session does not read
        // `Dlt`.
        //
        // Index reads: seven key decodes (the positions c5, o3, l7, l8, o4,
        // l9, c6), six value and six weight decodes for the kept entries;
        // memory: the memory run's first key (c7), and `Dlt`'s four keys, five
        // values, and five weights, read by the capture.
        let report = state.finish().unwrap();
        assert_eq!(report.run_reads, collecting);
        // The collecting thread's reads, per owner: the index's, the routing's under the
        // requested slice, and none under `query` (nor any other owner).
        let owner = |owner: Component| report.driver_reads[owner as usize];
        assert_eq!(owner(Component::MergedIndex), collecting);
        assert_eq!(owner(Component::RequestedSlice), collecting_other);
        for silent in [
            Component::Query,
            Component::BaseRelation,
            Component::PartnerTrace,
        ] {
            assert_eq!(owner(silent), RunReads::default(), "{silent:?}");
        }
        assert_eq!(collecting, with_memory(reads(0, 0, 7, 6, 6), 5, 5, 5));
        assert_eq!(
            report.counts,
            SessionCounts {
                groups: 1,
                cursor_opens: 1,
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
    let state = checked_state();
    let one_run = || source(vec![vec![customer(5, "only", 1)]]);

    state.publish(one_run(), run(vec![])).unwrap();
    assert_eq!(state.finish().unwrap(), SessionReads::default());

    // One touched group, whose delta changes the customer's segment: `ΔC`
    // drives `O⁻`, which keeps orders and so reads nothing of `Old`, and
    // nothing requests the customer. The two delta entries are decoded by the
    // capture.
    state
        .publish(
            one_run(),
            run(vec![customer(5, "only", -1), customer(5, "new", 1)]),
        )
        .unwrap();
    state.collect(Pass::Seek).unwrap();
    let first = state.finish().unwrap();
    assert_eq!(first.counts.groups, 1);
    assert_eq!(first.counts.payload_decodes_old, 0);
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

/// Collecting leaves no read to the default owner on the collecting thread: the capture of
/// `Dlt` is the index's, and the routing's merge of the driving deltas is the requested slice's.
#[test]
fn collect_leaves_zero_query_reads_on_the_collecting_thread() {
    let state = published(
        vec![vec![
            customer(2, "A", 1),
            customer(3, "B", 1),
            order(2, 20, 10, 1),
            order(3, 30, 10, 1),
            line(2, 20, 1, 100, 1),
            line(3, 30, 1, 100, 1),
        ]],
        vec![
            customer(2, "A", -1),
            customer(2, "C", 1),
            order(3, 31, 11, 1),
            order(4, 40, 11, 1),
            line(3, 30, 2, 100, 1),
            line(4, 40, 1, 100, 1),
        ],
    );
    let reads = state.finish().unwrap();
    assert_eq!(
        reads.driver_reads[Component::Query as usize],
        RunReads::default()
    );
    assert!(reads.driver_reads[Component::MergedIndex as usize].memory_key_reads > 0);
    // The routing merged `ΔC` and `ΔO` forward: reads, and no seek.
    let slice = reads.driver_reads[Component::RequestedSlice as usize];
    assert!(slice.memory_key_reads > 0);
    assert_eq!(slice.seeks, 0);
}

/// What a scan of the touched groups saw: per group, the positions and the
/// payloads decoded.
type Scanned = Vec<(Vec<u8>, usize, usize)>;

/// Scans the touched groups, reading every entry, with the run reads of the
/// merged index and of every other owner.
fn scan_col(
    old: &SourceSnapshot,
    touched: &[Vec<u8>],
    pass: Pass,
) -> ((Scanned, SessionCounts), RunReads, RunReads) {
    run_reads_of(|| {
        with_read_owner(Component::MergedIndex, || {
            let mut session =
                ScanSession::<CustomerOrdersLineitemDefinition>::new(old, touched, pass);
            let mut scanned = Vec::new();
            while let Some(group) = session.next_group() {
                let (mut positions, mut payloads) = (0, 0);
                while session.key().is_some() {
                    payloads += session.take_and_decode().unwrap().len();
                    positions += 1;
                    session.step();
                }
                scanned.push((group.to_vec(), positions, payloads));
            }
            (scanned, session.counts())
        })
    })
}

/// The A/B of the `Pass` knob on one transaction with untouched customers
/// between the touched ones.
///
/// `Old` holds, in key order (file run `F`, but `c7` in memory run `M`):
///
/// ```text
///  1 c3   2 o1   3 l1                                   untouched
///  4 c5   5 o3   6 l7   7 l8   8 o4   9 l9              touched
/// 10 c6  11 o6  12 l12                                  untouched
/// 13 c7 (M)                                             untouched
/// 14 c8  15 o8  16 l13                                  touched
/// 17 c9  18 o9                                          untouched
/// ```
///
/// The touched groups are c5, c8, and c10 (a customer past the end of `Old`).
/// The scan reads every entry of a touched group: nothing inside a group is
/// skipped.
#[test]
fn the_passes_pass_untouched_groups_in_different_ways() {
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
        let factories = OrdIndexedWSetFactories::<DynData, DynData, DynZWeight>::new::<
            Vec<u8>,
            StoredPayload,
            i64,
        >();
        let old = SourceSnapshot::with_batches(
            &factories,
            vec![Arc::new(old_file), Arc::new(old_memory)],
        );
        let touched = [5, 8, 10].map(prefix_custkey).to_vec();

        let ((seek_scanned, seek), seek_reads, seek_other) = scan_col(&old, &touched, Pass::Seek);
        let ((step_scanned, step), step_reads, step_other) = scan_col(&old, &touched, Pass::Step);

        // The outputs are equal: the six entries of c5, the three of c8, and
        // nothing of c10, which has no entry in `Old`.
        assert_eq!(step_scanned, seek_scanned);
        assert_eq!(
            seek_scanned,
            vec![
                (prefix_custkey(5), 6, 6),
                (prefix_custkey(8), 3, 3),
                (prefix_custkey(10), 0, 0),
            ]
        );
        for other in [seek_other, step_other] {
            assert_eq!(other, RunReads::default());
        }
        for counts in [seek, step] {
            assert_eq!(counts.groups, 3);
            assert_eq!(counts.cursor_opens, 1);
            assert_eq!(counts.payload_decodes_old, 9);
            assert_eq!(counts.cancelled, 0);
        }
        for reads in [seek_reads, step_reads] {
            assert_eq!((reads.value_decodes, reads.weight_decodes), (9, 9));
        }

        // Seek. Open on c3 (position 1).
        // - c5: the seek moves F from c3 to c5 (run seek 1, position 2). The
        //   scan steps through o3, l7, l8, o4, l9 (positions 3-7) to c6 (8),
        //   which ends the group: 6 steps.
        // - c8: the seek moves F from c6 and M from c7 (run seeks 2 and 3),
        //   landing on c8 (9); M is exhausted. The scan steps through o8 and
        //   l13 (10, 11) to c9 (12): 3 steps. Nothing is skipped.
        // - c10: the seek moves F from c9 (run seek 4) past the end.
        // 3 logical seeks, 4 run seeks, 9 steps, 12 positions; F decodes a key
        // for each of them (the memory run decodes nothing, and the seek past
        // the end lands on no key). The one memory key read is M's opening
        // key; `Dlt` is not read.
        assert_eq!(
            (seek.logical_seeks, seek.logical_steps, seek.positions),
            (3, 9, 12)
        );
        assert_eq!(seek_reads, with_memory(reads(4, 0, 12, 9, 9), 1, 0, 0));

        // Step. Every key up to c9 and o9 is a position, 18 in all, and every
        // one is stepped past once: 18 steps and, F having 17 of the keys,
        // 17 key decodes (c7 is in memory). No seek. The memory read is again
        // M's opening key.
        assert_eq!(
            (step.logical_seeks, step.logical_steps, step.positions),
            (0, 18, 18)
        );
        assert_eq!(step_reads, with_memory(reads(0, 0, 17, 9, 9), 1, 0, 0));
    });
}
