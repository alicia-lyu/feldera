use super::*;
use crate::{
    build_batch, CustomerByCustkeyPayload, LineitemByCustkeyOrderkeyLinenumberPayload,
    OrdersByCustkeyOrderkeyPayload,
};
use dbsp::{
    circuit::{CircuitConfig, CircuitStorageConfig},
    dynamic::{DowncastTrait, DynData, Erase},
    storage::write_attribution::WriteOwner,
    trace::{
        Batch, BatchLocation, BatchReaderFactories, Builder, OrdIndexedWSetFactories, RunReads,
        WithSnapshot,
    },
    DynZWeight, Runtime,
};
use feldera_types::config::{StorageConfig, StorageOptions};

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

fn delta<K: DBData, V: DBData>(rows: Vec<(K, V, i64)>) -> SpineSnapshot<OrdIndexedZSet<K, V>> {
    let batch = OrdIndexedZSet::from_tuples(
        (),
        rows.into_iter()
            .map(|(k, v, w)| Tup2(Tup2(k, v), w))
            .collect(),
    );
    TypedBatch::new(batch.into_inner().into_ro_snapshot())
}

fn tuples<K: DBData, V: DBData>(
    snapshot: &SpineSnapshot<OrdIndexedZSet<K, V>>,
) -> Vec<(K, V, i64)> {
    let mut cursor = snapshot.inner().cursor();
    let mut rows = Vec::new();
    while cursor.key_valid() {
        while cursor.val_valid() {
            rows.push((
                cursor.key().downcast_checked::<K>().clone(),
                cursor.val().downcast_checked::<V>().clone(),
                **cursor.weight(),
            ));
            cursor.step_val();
        }
        cursor.step_key();
    }
    rows
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
    let state = Q3State::default();
    state
        .publish(
            source(vec![initial.clone()]),
            source(vec![initial, changes]),
            BTreeMap::from([(20, BTreeSet::from([2]))]),
        )
        .unwrap();
    let orders_delta = delta(vec![(Tup1(2), Tup3(20, Date::from_days(11), 7), 1)]);
    let customers_delta = delta(vec![(Tup1(2), Tup1(SqlString::from("new")), 1)]);
    let lines_delta = delta(vec![(
        Tup1(20),
        Tup4(
            1,
            SqlDecimal::new(200, 2).unwrap(),
            SqlDecimal::new(4, 2).unwrap(),
            Date::from_days(30),
        ),
        1,
    )]);
    let joined_delta = delta(vec![(
        Tup1(20),
        Tup4(2, SqlString::from("new"), Date::from_days(11), 7),
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
            Tup1(20),
            Tup4(2, SqlString::from("new"), Date::from_days(11), 7),
            1
        )]
    );
    assert_eq!(
        tuples(&state.lineitems_before(&joined_delta).unwrap()),
        vec![
            (
                Tup1(20),
                Tup4(
                    1,
                    SqlDecimal::new(100, 2).unwrap(),
                    SqlDecimal::new(4, 2).unwrap(),
                    Date::from_days(30)
                ),
                4
            ),
            (
                Tup1(20),
                Tup4(
                    2,
                    SqlDecimal::new(100, 2).unwrap(),
                    SqlDecimal::new(4, 2).unwrap(),
                    Date::from_days(30)
                ),
                4
            )
        ]
    );
    state.finish().unwrap();
}

#[test]
fn candidate_locations_follow_each_snapshot_without_duplicating_rows() {
    let initial = vec![
        customer(2, "old", 2),
        customer(3, "new", 3),
        order(2, 20, 10, 4),
        line(2, 20, 1, 100, 5),
        line(2, 20, 2, 100, 5),
    ];
    let changes = vec![
        order(2, 20, 10, -4),
        order(3, 20, 11, 7),
        line(2, 20, 1, 100, -5),
        line(2, 20, 2, 100, -5),
        line(3, 20, 1, 100, 5),
        line(3, 20, 2, 100, 5),
    ];
    let state = Q3State::default();
    state
        .publish(
            source(vec![initial.clone()]),
            source(vec![initial, changes]),
            BTreeMap::from([
                (20, BTreeSet::from([2, 3, 2, 9, 3])),
                (99, BTreeSet::from([2, 3])),
            ]),
        )
        .unwrap();
    let line_value = Tup4(
        1,
        SqlDecimal::new(100, 2).unwrap(),
        SqlDecimal::new(4, 2).unwrap(),
        Date::from_days(30),
    );
    let line_request = delta(vec![(Tup1(20), line_value, -1), (Tup1(99), line_value, 1)]);
    assert_eq!(
        tuples(&state.joined_orders_after(&line_request).unwrap()),
        vec![(
            Tup1(20),
            Tup4(3, SqlString::from("new"), Date::from_days(11), 7),
            21,
        )]
    );
    let joined_request = delta(vec![
        (
            Tup1(20),
            Tup4(2, SqlString::from("old"), Date::from_days(10), 7),
            -1,
        ),
        (
            Tup1(20),
            Tup4(3, SqlString::from("new"), Date::from_days(11), 7),
            1,
        ),
        (
            Tup1(99),
            Tup4(2, SqlString::from("old"), Date::from_days(10), 7),
            1,
        ),
    ]);
    let mut second_line = line_value;
    second_line.0 = 2;
    assert_eq!(
        tuples(&state.lineitems_before(&joined_request).unwrap()),
        vec![(Tup1(20), line_value, 5), (Tup1(20), second_line, 5)]
    );
}

#[test]
fn requested_orders_require_nonempty_candidate_locations() {
    let line_request = delta(vec![(
        Tup1(20),
        Tup4(
            1,
            SqlDecimal::new(100, 2).unwrap(),
            SqlDecimal::new(4, 2).unwrap(),
            Date::from_days(30),
        ),
        1,
    )]);
    let joined_request = delta(vec![(
        Tup1(20),
        Tup4(2, SqlString::from("old"), Date::from_days(10), 7),
        -1,
    )]);
    for locations in [BTreeMap::new(), BTreeMap::from([(20, BTreeSet::new())])] {
        let state = Q3State::default();
        state
            .publish(source(vec![]), source(vec![]), locations)
            .unwrap();
        assert!(state
            .joined_orders_after(&line_request)
            .unwrap_err()
            .contains("current Orders delta"));
        assert!(state
            .lineitems_before(&joined_request)
            .unwrap_err()
            .contains("current Orders delta"));
    }
}

#[test]
fn joined_projection_multiplies_signed_weights_and_ignores_unrequested_ranges() {
    let state = Q3State::default();
    state
        .publish(
            source(vec![]),
            source(vec![vec![
                customer(2, "A", -2),
                order(2, 20, 10, 3),
                order(2, 21, 10, 7),
                customer(9, "unused", 100),
                order(9, 90, 10, 100),
            ]]),
            BTreeMap::from([(20, BTreeSet::from([2]))]),
        )
        .unwrap();
    let request = delta(vec![(
        Tup1(20),
        Tup4(
            1,
            SqlDecimal::new(100, 2).unwrap(),
            SqlDecimal::new(4, 2).unwrap(),
            Date::from_days(30),
        ),
        1,
    )]);
    assert_eq!(
        tuples(&state.joined_orders_after(&request).unwrap()),
        vec![(
            Tup1(20),
            Tup4(2, SqlString::from("A"), Date::from_days(10), 7),
            -6
        )]
    );
}

#[test]
fn cancelled_and_missing_keys_have_no_values() {
    let state = Q3State::default();
    state
        .publish(
            source(vec![]),
            source(vec![vec![customer(2, "A", 1)], vec![customer(2, "A", -1)]]),
            BTreeMap::new(),
        )
        .unwrap();
    let request = delta(vec![
        (Tup1(2), Tup3(20, Date::from_days(0), 0), 1),
        (Tup1(3), Tup3(30, Date::from_days(0), 0), 1),
    ]);
    assert!(tuples(&state.customers_after(&request).unwrap()).is_empty());
    assert!(tuples(
        &state
            .orders_before(&delta(vec![(Tup1(2), Tup1(SqlString::from("A")), 1)]))
            .unwrap()
    )
    .is_empty());
}

#[test]
fn publication_and_batch_local_placement_are_required() {
    let state = Q3State::default();
    let request = delta(vec![(
        Tup1(20),
        Tup4(2, SqlString::from("A"), Date::from_days(0), 0),
        1,
    )]);
    assert!(state
        .lineitems_before(&request)
        .unwrap_err()
        .contains("publish"));
    state
        .publish(source(vec![]), source(vec![]), BTreeMap::new())
        .unwrap();
    assert!(state
        .publish(source(vec![]), source(vec![]), BTreeMap::new())
        .is_err());
    assert!(state
        .lineitems_before(&request)
        .unwrap_err()
        .contains("current Orders delta"));
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

/// The storage-level baseline of today's four readers on the worked
/// transaction of `docs/design/merged-index/shared-traversal.md`, without
/// its invoices, since COL has none.
#[test]
fn todays_readers_run_reads_on_the_worked_transaction() {
    in_storage_runtime(|| {
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

        let factories = OrdIndexedWSetFactories::<DynData, DynData, DynZWeight>::new::<
            Vec<u8>,
            StoredPayload,
            i64,
        >();
        let (old_file, old_memory, dlt) = (Arc::new(old_file), Arc::new(old_memory), Arc::new(dlt));
        let before =
            SourceSnapshot::with_batches(&factories, vec![old_file.clone(), old_memory.clone()]);
        let after = SourceSnapshot::with_batches(&factories, vec![old_file, old_memory, dlt]);
        // Today's harness takes placements from the Orders delta alone and
        // would reject this `ΔC`; the readers accept any placements.
        let state = Q3State::default();
        state
            .publish(
                before,
                after,
                BTreeMap::from([3, 4, 5].map(|order| (order, BTreeSet::from([5])))),
            )
            .unwrap();

        let orders_delta = delta(vec![(Tup1(5), Tup3(5, Date::from_days(11), 7), 1)]);
        let customers_delta = delta(vec![
            (Tup1(5), Tup1(SqlString::from("old")), -1),
            (Tup1(5), Tup1(SqlString::from("new")), 1),
        ]);
        let lines_delta = delta(
            [10, 11]
                .map(|linenumber| {
                    (
                        Tup1(5),
                        Tup4(
                            linenumber,
                            SqlDecimal::new(200, 2).unwrap(),
                            SqlDecimal::new(4, 2).unwrap(),
                            Date::from_days(30),
                        ),
                        1,
                    )
                })
                .to_vec(),
        );
        let joined_delta = delta(
            [3, 4, 5]
                .map(|order| {
                    (
                        Tup1(order),
                        Tup4(5, SqlString::from("new"), Date::from_days(10), 7),
                        1,
                    )
                })
                .to_vec(),
        );

        let (customers, c_reads, c_other) =
            run_reads_of(|| state.customers_after(&orders_delta).unwrap());
        let (orders, o_reads, o_other) =
            run_reads_of(|| state.orders_before(&customers_delta).unwrap());
        let (joined, j_reads, j_other) =
            run_reads_of(|| state.joined_orders_after(&lines_delta).unwrap());
        let (lines, l_reads, l_other) =
            run_reads_of(|| state.lineitems_before(&joined_delta).unwrap());
        let tally = state.finish().unwrap();

        assert_eq!(
            tuples(&customers),
            vec![(Tup1(5), Tup1(SqlString::from("new")), 1)]
        );
        assert_eq!(
            tuples(&orders),
            vec![
                (Tup1(5), Tup3(3, Date::from_days(10), 7), 1),
                (Tup1(5), Tup3(4, Date::from_days(10), 7), 1),
            ]
        );
        assert_eq!(
            tuples(&joined),
            vec![(
                Tup1(5),
                Tup4(5, SqlString::from("new"), Date::from_days(11), 7),
                1
            )]
        );
        assert_eq!(
            tuples(&lines)
                .into_iter()
                .map(|(order, Tup4(linenumber, ..), weight)| (order.0, linenumber, weight))
                .collect::<Vec<_>>(),
            vec![(3, 7, 1), (3, 8, 1), (4, 9, 1)]
        );

        // The index reads. The file run's keys, in folded-key order, are c5,
        // o3, l7, l8, o4, l9, and c6, each with one value. Every scan opens a
        // cursor over all runs, which lands the file run on c5 (one key read
        // and one value read), and seeks every run once. A key step of the file run reads
        // the new key and its first value. A seek that leaves the file run on
        // its current key still reads that key's first value. `CursorList`
        // reads a file weight whenever the file run holds the smallest value,
        // to skip zero weights.
        let reads = |seeks, key_reads, value_reads, weight_reads| RunReads {
            seeks,
            key_reads,
            value_reads,
            weight_reads,
        };
        // C⁺: one scan of c5's own key over three runs. The seek leaves the
        // file run on c5 (one value read); `Dlt`'s "new" comes first, then
        // "old" cancels across `Dlt` and the file run (one weight read). The
        // step to o3 ends the range (one key, value, and weight read).
        assert_eq!(c_reads, reads(3, 2, 3, 2));
        // O⁻: one scan of c5's range over two runs. Opening and the seek stay
        // on c5 (a weight read each); then six key steps, c5 through c6, each
        // read a key, a value, and a weight.
        assert_eq!(o_reads, reads(2, 7, 8, 8));
        // J⁺: c5's key as in C⁺, then (c5, o5)'s key, whose seek lands the
        // file run on c6 (one key and one value read). Only `Dlt` holds o5,
        // so no file weight is read.
        assert_eq!(j_reads, reads(6, 4, 5, 2));
        // L⁻: the line items of o3, o4, and o5, one scan each over two runs.
        // Each scan opens on c5 and seeks to its order's first line item: o3
        // reads l7, l8, and o4 (4 of each); o4 reads l9 and c6 (3 of each);
        // o5 has no line items and lands on c6 (2 of each).
        assert_eq!(l_reads, reads(6, 9, 9, 9));

        // The driving deltas are in memory and only stepped, so nothing but
        // the index is read.
        for other in [c_other, o_other, j_other, l_other] {
            assert_eq!(other, RunReads::default());
        }

        // `finish` returns the same counts, as the harness reports them.
        assert_eq!(
            tally,
            IndexReads {
                customers_after: c_reads,
                orders_before: o_reads,
                joined_orders_after: j_reads,
                lineitems_before: l_reads,
            }
        );
    });
}

#[test]
fn finish_returns_each_transactions_index_reads() {
    let state = Q3State::default();
    let orders_delta = delta(vec![(Tup1(5), Tup3(5, Date::from_days(11), 7), 1)]);
    let one_run = || source(vec![vec![customer(5, "only", 1)]]);

    // Two calls in one transaction accumulate: each scan seeks the one
    // in-memory run once, and in-memory runs count no other reads.
    state
        .publish(one_run(), one_run(), BTreeMap::new())
        .unwrap();
    state.customers_after(&orders_delta).unwrap();
    state.customers_after(&orders_delta).unwrap();
    let tally = state.finish().unwrap();
    assert_eq!(tally.customers_after.seeks, 2);
    assert_eq!(
        IndexReads {
            customers_after: RunReads::default(),
            ..tally
        },
        IndexReads::default()
    );

    // The next transaction starts from zero.
    state
        .publish(one_run(), one_run(), BTreeMap::new())
        .unwrap();
    assert_eq!(state.finish().unwrap(), IndexReads::default());
}
