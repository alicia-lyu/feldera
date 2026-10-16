use super::*;
use crate::{CustomerPayload, LineitemPayload, OrdersPayload};
use dbsp::{
    dynamic::{DowncastTrait, DynData},
    trace::{BatchReaderFactories, OrdIndexedWSetFactories, WithSnapshot},
    DynZWeight,
};

type SourceRow = (SourceKey, SourcePayload, i64);

fn customer(id: i64, segment: &str, weight: i64) -> SourceRow {
    (
        SourceKey::Customer(id),
        SourcePayload::Customer(CustomerPayload {
            segment: segment.into(),
        }),
        weight,
    )
}

fn order(c: i64, o: i64, day: i32, weight: i64) -> SourceRow {
    (
        SourceKey::Orders(c, o),
        SourcePayload::Orders(OrdersPayload {
            order_date: Date::from_days(day),
            ship_priority: 7,
        }),
        weight,
    )
}

fn line(c: i64, o: i64, l: i32, price: i128, weight: i64) -> SourceRow {
    (
        SourceKey::Lineitem(c, o, l),
        SourcePayload::Lineitem(LineitemPayload {
            ship_date: Date::from_days(30),
            extended_price: SqlDecimal::new(price, 2).unwrap(),
            discount: SqlDecimal::new(4, 2).unwrap(),
        }),
        weight,
    )
}

fn source(batches: Vec<Vec<SourceRow>>) -> SourceSnapshot {
    let index = CustomerOrdersLineitemIndex::default();
    let factories = OrdIndexedWSetFactories::<DynData, DynData, DynZWeight>::new::<
        Vec<u8>,
        SourcePayload,
        i64,
    >();
    SourceSnapshot::with_batches(
        &factories,
        batches
            .into_iter()
            .map(|rows| Arc::new(index.build_batch(rows).unwrap()))
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
