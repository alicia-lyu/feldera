use super::*;
use crate::{
    build_batch, prefix_custkey, CustomerByCustkeyPayload,
    LineitemByCustkeyOrderkeyLinenumberPayload, OrdersByCustkeyOrderkeyPayload,
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

/// The storage-level baseline of today's four readers on the worked
/// transaction of `docs/design/merged-index/shared-traversal.md`, without
/// its invoices, since COL has none.
#[test]
fn todays_readers_run_reads_on_the_worked_transaction() {
    in_storage_runtime(|| {
        let (old_file, old_memory, dlt) = worked_runs();
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
        // cursor over all runs, which lands the file run on c5 (one key
        // decode).
        //
        // The file cursor decodes a value only when asked (lazy values), and
        // `CursorList` asks for the first value of every run at its smallest
        // key. The file run is at the smallest key at every landing
        // here: the memory run holds only c7, past every file key, and `Dlt`
        // holds c5 and the keys under it. So each landing of the file run
        // costs one key decode and one value decode, as it did before lazy
        // values, and lazy values change none of the counts below. A seek that
        // leaves the file run on its current key still decodes that key's
        // first value, since the seek empties the cursor's value and
        // `CursorList` asks again. `CursorList` decodes a file weight
        // whenever the file run holds the smallest value, to skip zero
        // weights. `RunMerge` (roadmap step 2) avoids these reads.
        //
        // The path reads, C⁺ and J⁺, probe one exact key (`scan_entry`): a run
        // whose filter excludes the key is neither sought nor decoded, and
        // nothing past the entry is read. The range reads, O⁻ and L⁻, seek a
        // prefix and step through it (`scan`), with no filter.
        //
        // The in-memory runs are the memory run M (c7 alone) and, for the
        // reads of the new state, `Dlt`. Their accesses count apart from the
        // file decodes (`memory_*`), by the same rules: opening reads a
        // run's first key; a seek reads a key only if it moves the run; a
        // value counts once per position, and `CursorList` reads the first
        // value of every run at its smallest key, plus that value's weight
        // to skip zero weights.
        let reads =
            |seeks, filter_rejections, key_decodes, value_decodes, weight_decodes| RunReads {
                seeks,
                filter_rejections,
                key_decodes,
                value_decodes,
                weight_decodes,
                ..RunReads::default()
            };
        // C⁺: one probe of c5's own key over three runs. Opening decodes c5's
        // key and value; the file filter admits c5, and its seek stays on c5,
        // which decodes the value again. `Dlt`'s "new" comes first, then
        // "old" cancels across `Dlt` and the file run (one weight decode).
        //
        // Memory: opening reads M's key (c7) and `Dlt`'s (c5): 2 keys. `Dlt`
        // holds c5's values "new" and "old", in that order. At the opening,
        // `Dlt` is at the smallest value, "new": a value and its weight (the
        // file run's value "old" is larger, so only its value is decoded).
        // The seeks stay (M is past c5; `Dlt` and the file run are on it),
        // and empty the values, so `Dlt` reads "new" and its weight again.
        // Stepping to "old" reads that value (the third), and the sum over
        // `Dlt` and the file run reads its weight (the third); the step past
        // it lands on nothing. M is never at the smallest key, so none of
        // its values is read.
        assert_eq!(c_reads, with_memory(reads(3, 0, 1, 2, 1), 2, 3, 3));
        // O⁻: one scan of c5's range over two runs. Opening and the seek stay
        // on c5 (a weight decode each); then six key steps, c5 through c6,
        // each decode a key, a value, and a weight.
        //
        // Memory: only M's key (c7), read at the opening. The scan neither
        // moves M (it is past the range) nor reads its value.
        assert_eq!(o_reads, with_memory(reads(2, 0, 7, 8, 8), 1, 0, 0));
        // J⁺: c5's key as in C⁺, then a probe of (c5, o5)'s key. Its opening
        // decodes c5 again (one key, one value); the file filter rejects o5,
        // which only `Dlt` holds, so the file run is not sought, and the two
        // in-memory runs are sought once each.
        //
        // Memory: C⁺'s 2 keys, 3 values, and 3 weights. The second probe
        // opens M and `Dlt` again (2 keys) and reads `Dlt`'s "new" and its
        // weight, as the opening of C⁺ does (1 value, 1 weight). The seek
        // to (c5, o5) moves `Dlt` from c5 to o5 (1 key); M stays on c7. Only
        // `Dlt` holds o5, so `CursorList` reads its first value and weight
        // there (1 and 1). In all: keys 2 + 2 + 1, values 3 + 1 + 1, weights
        // 3 + 1 + 1.
        assert_eq!(j_reads, with_memory(reads(5, 1, 2, 3, 1), 5, 5, 5));
        // L⁻: the line items of o3, o4, and o5, one scan each over two runs.
        // Each scan opens on c5 and seeks to its order's first line item: o3
        // decodes l7, l8, and o4 (4 of each); o4 decodes l9 and c6 (3 of
        // each); o5 has no line items and lands on c6 (2 of each).
        //
        // Memory: each of the three scans opens M, and reads its one key
        // (c7). M is past every range, and so never at the smallest key.
        assert_eq!(l_reads, with_memory(reads(6, 0, 9, 9, 9), 3, 0, 0));

        // The driving deltas are in memory and read by `requested_keys`,
        // which steps through their keys: `CursorList` reads each landed key,
        // and the first value and weight under it, to skip zero weights.
        // That is one key, one value, and one weight for each key of a delta:
        // one each for the Orders delta (key 5), the Customer delta (key 5,
        // two values), and the Lineitem delta (key 5, two values), and three
        // for the joined delta (keys 3, 4, and 5). Nothing is decoded.
        assert_eq!(c_other, memory_only(1));
        assert_eq!(o_other, memory_only(1));
        assert_eq!(j_other, memory_only(1));
        assert_eq!(l_other, memory_only(3));

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

    // Two calls in one transaction accumulate. Each scan of the one in-memory
    // run seeks it once (the seek stays on c5), reads its key at the opening
    // (1), and reads c5's first value and weight twice: at the opening, and
    // again after the seek empties the value (2 and 2). So the two calls make
    // 2 seeks, 2 key reads, 4 value reads, and 4 weight reads, and no decode.
    state
        .publish(one_run(), one_run(), BTreeMap::new())
        .unwrap();
    state.customers_after(&orders_delta).unwrap();
    state.customers_after(&orders_delta).unwrap();
    let tally = state.finish().unwrap();
    assert_eq!(
        tally.customers_after,
        with_memory(
            RunReads {
                seeks: 2,
                ..RunReads::default()
            },
            2,
            4,
            4
        )
    );
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
                        groups.push((group.to_vec(), buffers.clone()));
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

/// The reads of in-memory runs that land on `keys` keys and read the first
/// value and weight of each.
fn memory_only(keys: u64) -> RunReads {
    with_memory(RunReads::default(), keys, keys, keys)
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
/// `todays_readers_run_reads_on_the_worked_transaction` (22 value decodes
/// there, against 6 here).
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

/// The session on the worked transaction over `Old` in native's layout: one run holding all
/// eight old keys (c5, o3, l7, l8, o4, l9, c6, c7), in memory or in a file. The native view
/// circuit's traces are one run each after the load (see
/// `native_worked_transaction_tests` in the harness), so this is the layout that the native
/// counts are read from. `Dlt` is the transaction's delta, in memory.
fn assert_session_on_the_native_layout(old_in_file: bool, expected: RunReads) {
    in_storage_runtime(move || {
        let (_, _, dlt) = worked_runs();
        let all_old = build_batch(vec![
            customer(5, "old", 1),
            order(5, 3, 10, 1),
            line(5, 3, 7, 100, 1),
            line(5, 3, 8, 100, 1),
            order(5, 4, 10, 1),
            line(5, 4, 9, 100, 1),
            customer(6, "other", 1),
            customer(7, "later", 1),
        ])
        .unwrap();
        let all_old = if old_in_file {
            file_run(&all_old)
        } else {
            all_old
        };
        assert_eq!(
            all_old.location(),
            if old_in_file {
                BatchLocation::Storage
            } else {
                BatchLocation::Memory
            }
        );
        let factories = OrdIndexedWSetFactories::<DynData, DynData, DynZWeight>::new::<
            Vec<u8>,
            StoredPayload,
            i64,
        >();
        let old = SourceSnapshot::with_batches(&factories, vec![Arc::new(all_old)]);
        let ((groups, counts), index, other) = maintain_col(&old, &dlt, Pass::Seek, false);

        // One group, c5, with the buffers of the worked transaction, and the same positions
        // as over the layout of the test above: c5 at open, six steps, c6 ends the group.
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].0, prefix_custkey(5));
        assert_eq!(buffered(&groups[0].1), [(1, 2), (2, 1), (3, 2)]);
        assert_eq!((counts.logical_seeks, counts.logical_steps), (0, 6));
        assert_eq!(counts.positions, 7);
        assert_eq!(other, RunReads::default());
        assert_eq!(index, expected);
    });
}

/// `Old` in one memory run, as in native with default placement at low memory pressure.
///
/// Memory reads of `Old`'s run: its keys are the seven positions (c5 at open, then six steps;
/// c7 is never reached), so 7 key reads; the six taken entries are 6 value and 6 weight reads.
/// `Dlt` adds its 4 keys, 5 values, and 5 weights, as in the test above. In all: 11 keys, 11
/// values, 11 weights, no seek, and nothing decoded.
#[test]
fn session_run_reads_on_the_native_layout_in_memory() {
    assert_session_on_the_native_layout(false, with_memory(reads(0, 0, 0, 0, 0), 11, 11, 11));
}

/// `Old` in one file, as in native under high memory pressure. The file run decodes what the
/// memory run read: 7 keys, 6 values, 6 weights. `Dlt` is still in memory: 4 keys, 5 values, and
/// 5 weights.
#[test]
fn session_run_reads_on_the_native_layout_in_a_file() {
    assert_session_on_the_native_layout(true, with_memory(reads(0, 0, 7, 6, 6), 4, 5, 5));
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
