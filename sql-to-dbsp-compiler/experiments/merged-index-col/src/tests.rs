use super::*;
use dbsp::trace::merged_index::batch::batch_cursor;
use dbsp::trace::merged_index::testing::check_against_trie;
use dbsp::trace::merged_index::{write_parent, ColumnRef, MergedIndexDefinition, PlacedIndex};
use dbsp::{
    dynamic::{DowncastTrait, DynData, DynUnit, DynWeight, Erase},
    storage::{
        backend::StorageBackend,
        file::{
            format::BatchMetadata,
            reader::Reader,
            writer::{Parameters, Writer2},
            Factories, FilterPlan,
        },
    },
    trace::{BatchLocation, BatchReader, Cursor},
    DynZWeight, ZWeight,
};
use feldera_sqllib::{Date, SqlDecimal};
use feldera_types::config::{StorageConfig, StorageOptions};
use proptest::prelude::*;
use tempfile::tempdir;

fn index() -> CustomerOrdersLineitemIndex {
    CustomerOrdersLineitemIndex::default()
}

fn customer_row(id: i64, segment: &str, weight: i64) -> (SourceKey, SourcePayload, i64) {
    (
        SourceKey::Customer(id),
        SourcePayload::Customer(CustomerPayload {
            segment: segment.into(),
        }),
        weight,
    )
}

fn cursor_row(
    index: &CustomerOrdersLineitemIndex,
    cursor: &mut impl Cursor<DynData, DynData, (), DynZWeight>,
) -> (SourceKey, SourcePayload, ZWeight) {
    let weight = *cursor.weight().downcast_checked::<ZWeight>();
    let key = cursor.key().downcast_checked::<Vec<u8>>();
    let payload = cursor.val().downcast_checked::<Vec<u8>>();
    (
        index.unfold(key).unwrap(),
        index.decode_payload(key, payload).unwrap(),
        weight,
    )
}

fn read_cursor(
    index: &CustomerOrdersLineitemIndex,
    cursor: &mut impl Cursor<DynData, DynData, (), DynZWeight>,
) -> Vec<(SourceKey, SourcePayload, ZWeight)> {
    let mut rows = Vec::new();
    while cursor.key_valid() {
        while cursor.val_valid() {
            rows.push(cursor_row(index, cursor));
            cursor.step_val();
        }
        cursor.step_key();
    }
    rows
}

#[test]
fn memory_batch_sorts_and_consolidates_within_batch() {
    let index = index();
    let batch = index
        .build_batch([
            customer_row(2, "B", 1),
            customer_row(1, "A", 3),
            customer_row(2, "B", -2),
            customer_row(1, "A", -1),
            customer_row(3, "gone", 1),
            customer_row(3, "gone", -1),
            customer_row(2, "A", 4),
        ])
        .unwrap();
    assert_eq!(batch.location(), BatchLocation::Memory);
    assert_eq!(batch.key_count(), 2);
    assert_eq!(batch.len(), 3);
    assert_eq!(
        read_cursor(&index, &mut batch_cursor(&[batch])),
        vec![
            customer_row(1, "A", 2),
            customer_row(2, "A", 4),
            customer_row(2, "B", -1),
        ]
    );
}

#[test]
fn cursor_consolidates_batches_without_changing_stored_rows() {
    let index = index();
    let batches = [
        index
            .build_batch([
                customer_row(1, "A", 2),
                customer_row(2, "gone", 1),
                customer_row(3, "old", 1),
                customer_row(4, "A", 1),
                customer_row(5, "negative", -3),
            ])
            .unwrap(),
        index
            .build_batch([
                customer_row(1, "A", 3),
                customer_row(2, "gone", -1),
                customer_row(3, "old", -1),
                customer_row(3, "new", 1),
                customer_row(4, "B", 2),
            ])
            .unwrap(),
        index
            .build_batch([customer_row(1, "A", -1), customer_row(4, "A", -1)])
            .unwrap(),
    ];
    assert!(batches
        .iter()
        .all(|batch| batch.location() == BatchLocation::Memory));
    let stored_before: Vec<_> = batches
        .iter()
        .map(|batch| read_cursor(&index, &mut batch.cursor()))
        .collect();
    let expected = vec![
        customer_row(1, "A", 4),
        customer_row(3, "new", 1),
        customer_row(4, "B", 2),
        customer_row(5, "negative", -3),
    ];
    let mut cursor = batch_cursor(&batches);
    assert_eq!(read_cursor(&index, &mut cursor), expected);
    cursor.rewind_keys();
    assert_eq!(read_cursor(&index, &mut cursor), expected);
    let stored_after: Vec<_> = batches
        .iter()
        .map(|batch| read_cursor(&index, &mut batch.cursor()))
        .collect();
    assert_eq!(stored_before, stored_after);
}

#[test]
fn cursor_exact_and_lower_bound_seeks_after_rewind() {
    let index = index();
    let batches = [
        index
            .build_batch([customer_row(1, "A", 1), customer_row(2, "gone", 1)])
            .unwrap(),
        index
            .build_batch([
                customer_row(1, "A", 2),
                customer_row(2, "gone", -1),
                customer_row(3, "B", 1),
            ])
            .unwrap(),
    ];
    let mut cursor = batch_cursor(&batches);
    let first = index.fold(&SourceKey::Customer(1));
    assert!(cursor.seek_key_exact(first.as_bytes().to_vec().erase(), None));
    assert_eq!(cursor_row(&index, &mut cursor), customer_row(1, "A", 3));
    let cancelled = index.fold(&SourceKey::Customer(2));
    assert!(!cursor.seek_key_exact(cancelled.as_bytes().to_vec().erase(), None));
    let after = index.fold(&SourceKey::Customer(4));
    assert!(!cursor.seek_key_exact(after.as_bytes().to_vec().erase(), None));
    cursor.rewind_keys();
    cursor.seek_key(cancelled.as_bytes().to_vec().erase());
    assert_eq!(
        read_cursor(&index, &mut cursor),
        vec![customer_row(3, "B", 1)]
    );
    cursor.rewind_keys();
    cursor.seek_key(first.as_bytes().to_vec().erase());
    assert_eq!(cursor_row(&index, &mut cursor), customer_row(1, "A", 3));
    cursor.seek_key(after.as_bytes().to_vec().erase());
    assert!(!cursor.key_valid());
}

#[test]
fn cursor_scans_interleaved_sources_and_stops_at_prefix_boundary() {
    let index = index();
    let order = (
        SourceKey::Orders(5, 2),
        SourcePayload::Orders(OrdersPayload {
            order_date: Date::from_days(1),
            ship_priority: 0,
        }),
        1,
    );
    let line = (
        SourceKey::Lineitem(5, 2, 1),
        SourcePayload::Lineitem(LineitemPayload {
            ship_date: Date::from_days(2),
            extended_price: SqlDecimal::<15, 2>::new(100, 2).unwrap(),
            discount: SqlDecimal::<4, 2>::new(5, 2).unwrap(),
        }),
        1,
    );
    let batches = [
        index
            .build_batch([customer_row(6, "other", 1), order.clone()])
            .unwrap(),
        index
            .build_batch([line.clone(), customer_row(5, "A", 1)])
            .unwrap(),
        index
            .build_batch([order.clone(), customer_row(5, "A", -1)])
            .unwrap(),
    ];
    let prefix = index.prefix_custkey(5);
    let mut cursor = batch_cursor(&batches);
    cursor.seek_key(prefix.erase());
    let mut rows = Vec::new();
    while cursor.key_valid()
        && cursor
            .key()
            .downcast_checked::<Vec<u8>>()
            .starts_with(&prefix)
    {
        while cursor.val_valid() {
            rows.push(cursor_row(&index, &mut cursor));
            cursor.step_val();
        }
        cursor.step_key();
    }
    assert_eq!(rows, vec![(order.0, order.1, 2), line]);
    assert_eq!(cursor_row(&index, &mut cursor), customer_row(6, "other", 1));
}

#[test]
fn empty_batches_and_cancelled_rows_are_skipped() {
    let index = index();
    assert!(!batch_cursor(&[]).key_valid());
    let empty = [index.build_batch([]).unwrap()];
    assert!(!batch_cursor(&empty).key_valid());
    let cancelled = [
        index.build_batch([customer_row(1, "A", 1)]).unwrap(),
        index.build_batch([customer_row(1, "A", -1)]).unwrap(),
    ];
    assert!(!batch_cursor(&cancelled).key_valid());
    let batches = [
        index.build_batch([]).unwrap(),
        index
            .build_batch([customer_row(1, "A", 1), customer_row(1, "A", -1)])
            .unwrap(),
        index.build_batch([customer_row(2, "B", -3)]).unwrap(),
    ];
    assert_eq!(batches[0].len(), 0);
    assert_eq!(batches[1].key_count(), 0);
    let mut cursor = batch_cursor(&batches);
    assert_eq!(
        read_cursor(&index, &mut cursor),
        vec![customer_row(2, "B", -3)]
    );
    cursor.rewind_keys();
    let first = index.fold(&SourceKey::Customer(1));
    cursor.seek_key(first.as_bytes().to_vec().erase());
    assert_eq!(cursor_row(&index, &mut cursor), customer_row(2, "B", -3));
}

#[test]
fn batch_rejects_weight_overflow_and_mismatched_payload() {
    let index = index();
    assert!(index
        .build_batch([customer_row(1, "A", i64::MAX), customer_row(1, "A", 1),])
        .is_err());
    assert!(index
        .build_batch([(
            SourceKey::Customer(1),
            SourcePayload::Orders(OrdersPayload {
                order_date: Date::from_days(1),
                ship_priority: 0,
            }),
            1,
        )])
        .is_err());
}

#[test]
fn exact_folded_keys_and_order() {
    let index = index();
    let customer = index.fold(&SourceKey::Customer(-1));
    let order = index.fold(&SourceKey::Orders(-1, 0));
    let line = index.fold(&SourceKey::Lineitem(-1, 0, 1));
    assert_eq!(
        customer.as_bytes(),
        &[1, 0x7f, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0, 1]
    );
    assert_eq!(
        order.as_bytes(),
        &[1, 0x7f, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 2, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 2]
    );
    assert_eq!(
        line.as_bytes(),
        &[
            1, 0x7f, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 2, 0x80, 0, 0, 0, 0, 0, 0, 0, 3,
            0x80, 0, 0, 1, 0, 3
        ]
    );
    assert!(customer < order && order < line);
    assert_eq!(index.prefix_custkey(-1), customer.as_bytes()[..9]);
    assert_eq!(index.prefix_custkey_orderkey(-1, 0), order.as_bytes()[..18]);
    assert_eq!(
        index.prefix_custkey_orderkey_then_linenumber(-1, 0),
        line.as_bytes()[..19]
    );
    for key in [
        SourceKey::Customer(i64::MIN),
        SourceKey::Orders(i64::MAX, i64::MIN),
        SourceKey::Lineitem(i64::MIN, i64::MAX, i32::MAX),
    ] {
        assert_eq!(index.unfold(index.fold(&key).as_bytes()), Ok(key));
    }
    assert!(index.fold(&SourceKey::Customer(-1)) < index.fold(&SourceKey::Customer(0)));
    assert!(index.fold(&SourceKey::Customer(0)) < index.fold(&SourceKey::Customer(1)));
}

#[test]
fn malformed_folded_keys_are_rejected() {
    let index = index();
    for key in [
        SourceKey::Customer(1),
        SourceKey::Orders(1, 2),
        SourceKey::Lineitem(1, 2, 3),
    ] {
        let bytes = index.fold(&key).as_bytes().to_vec();
        assert!(index.unfold(&bytes[..bytes.len() - 1]).is_err());
        let mut extra = bytes.clone();
        extra.push(0);
        assert!(index.unfold(&extra).is_err());
        let mut bad_index = bytes.clone();
        bad_index[bytes.len() - 2] = 0xff;
        assert!(index.unfold(&bad_index).is_err());
        let mut bad_id = bytes.clone();
        *bad_id.last_mut().unwrap() = 0xff;
        assert!(index.unfold(&bad_id).is_err());
        for offset in [0, 9, 18]
            .into_iter()
            .filter(|offset| *offset < bytes.len() - 2)
        {
            let mut bad_domain = bytes.clone();
            bad_domain[offset] = 0xff;
            assert!(index.unfold(&bad_domain).is_err());
        }
    }
}

#[test]
fn payload_round_trip_and_corruption() {
    let index = index();
    let pairs = [
        (
            SourceKey::Customer(4),
            SourcePayload::Customer(CustomerPayload {
                segment: "BUILDING".into(),
            }),
        ),
        (
            SourceKey::Orders(4, 5),
            SourcePayload::Orders(OrdersPayload {
                order_date: Date::from_days(-1),
                ship_priority: 7,
            }),
        ),
        (
            SourceKey::Lineitem(4, 5, 6),
            SourcePayload::Lineitem(LineitemPayload {
                ship_date: Date::from_days(9),
                extended_price: SqlDecimal::<15, 2>::new(12345, 2).unwrap(),
                discount: SqlDecimal::<4, 2>::new(12, 2).unwrap(),
            }),
        ),
    ];
    for (key, payload) in pairs {
        let folded = index.fold(&key);
        let encoded = index.encode_payload(&payload).unwrap();
        assert_eq!(encoded, index.encode_payload(&payload).unwrap());
        assert_eq!(
            index.decode_payload(folded.as_bytes(), encoded.as_bytes()),
            Ok(payload)
        );
        assert!(index.decode_payload(folded.as_bytes(), &[1, 2, 3]).is_err());
        assert!(index.decode_payload(&[0], encoded.as_bytes()).is_err());
    }

    let customer_key = index.fold(&SourceKey::Customer(i64::MAX));
    let customer = SourcePayload::Customer(CustomerPayload {
        segment: ("東京 café ".repeat(80)).into(),
    });
    let encoded = index.encode_payload(&customer).unwrap();
    assert_eq!(
        index.decode_payload(customer_key.as_bytes(), encoded.as_bytes()),
        Ok(customer)
    );
    let mut corrupt = encoded.as_bytes().to_vec();
    corrupt.fill(0xff);
    assert!(index
        .decode_payload(customer_key.as_bytes(), &corrupt)
        .is_err());

    let order_key = index.fold(&SourceKey::Orders(i64::MIN, i64::MAX));
    let order = SourcePayload::Orders(OrdersPayload {
        order_date: Date::from_days(i32::MIN),
        ship_priority: i32::MAX,
    });
    let encoded = index.encode_payload(&order).unwrap();
    assert_eq!(
        index.decode_payload(order_key.as_bytes(), encoded.as_bytes()),
        Ok(order)
    );
    let line_key = index.fold(&SourceKey::Lineitem(i64::MIN, i64::MAX, i32::MIN));
    let line = SourcePayload::Lineitem(LineitemPayload {
        ship_date: Date::from_days(i32::MAX),
        extended_price: SqlDecimal::<15, 2>::new(-123_456, 2).unwrap(),
        discount: SqlDecimal::<4, 2>::new(-99, 2).unwrap(),
    });
    let encoded = index.encode_payload(&line).unwrap();
    assert_eq!(
        index.decode_payload(line_key.as_bytes(), encoded.as_bytes()),
        Ok(line)
    );
}

fn column(relation: &'static str, column: &'static str) -> ColumnRef {
    ColumnRef { relation, column }
}

#[test]
fn trie_is_valid_and_derives_the_spec() {
    TRIE.validate().unwrap();
    assert_eq!(
        ["custkey", "orderkey", "linenumber"].map(|domain| TRIE.domain_tag(domain)),
        [Some(1), Some(2), Some(3)]
    );

    let placements = TRIE.placements();
    assert_eq!(
        placements
            .iter()
            .map(|placed| placed.id)
            .collect::<Vec<_>>(),
        [1, 2, 3]
    );
    assert_eq!(
        placements.iter().map(PlacedIndex::name).collect::<Vec<_>>(),
        [
            "CustomerByCustkey",
            "OrdersByCustkeyOrderkey",
            "LineitemByCustkeyOrderkeyLinenumber"
        ]
    );
    assert_eq!(
        placements
            .iter()
            .map(PlacedIndex::is_primary)
            .collect::<Vec<_>>(),
        [true, false, false]
    );
    assert_eq!(
        placements[0].key_columns(&TRIE),
        [column("Customer", "c_custkey")]
    );
    assert_eq!(
        placements[1].key_columns(&TRIE),
        [
            column("Orders", "o_custkey"),
            column("Orders", "o_orderkey")
        ]
    );
    // `custkey via Orders` takes the customer key from `Orders.o_custkey`.
    assert_eq!(
        placements[2].key_columns(&TRIE),
        [
            column("Orders", "o_custkey"),
            column("Lineitem", "l_orderkey"),
            column("Lineitem", "l_linenumber")
        ]
    );
}

#[test]
fn hard_coded_tags_and_ids_agree_with_the_trie_at_the_extremes() {
    check_against_trie::<CustomerOrdersLineitemDefinition>([
        SourceKey::Customer(i64::MIN),
        SourceKey::Customer(i64::MAX),
        SourceKey::Orders(i64::MIN, i64::MAX),
        SourceKey::Orders(i64::MAX, i64::MIN),
        SourceKey::Lineitem(i64::MIN, i64::MAX, i32::MIN),
        SourceKey::Lineitem(i64::MAX, i64::MIN, i32::MAX),
        SourceKey::Lineitem(0, 0, 0),
    ]);
}

proptest! {
    #[test]
    fn folding_round_trips_and_agrees_with_the_trie(customer: i64, order: i64, line: i32) {
        check_against_trie::<CustomerOrdersLineitemDefinition>([
            SourceKey::Customer(customer),
            SourceKey::Orders(customer, order),
            SourceKey::Lineitem(customer, order, line),
        ]);
    }

    /// Within one placement, byte order is the order of the key fields.
    #[test]
    fn folded_lineitem_order_is_field_order(
        left: (i64, i64, i32),
        right: (i64, i64, i32),
    ) {
        let fold = |(c, o, l): (i64, i64, i32)| {
            CustomerOrdersLineitemDefinition::fold(&SourceKey::Lineitem(c, o, l))
        };
        prop_assert_eq!(fold(left).cmp(&fold(right)), left.cmp(&right));
    }

    /// Every key of a customer, order, or line range starts with that range's prefix.
    #[test]
    fn prefixes_cover_exactly_their_ranges(customer: i64, order: i64, line: i32) {
        let index = index();
        let customer_prefix = index.prefix_custkey(customer);
        let order_prefix = index.prefix_custkey_orderkey(customer, order);
        let line_prefix = index.prefix_custkey_orderkey_then_linenumber(customer, order);
        let order_key = index.fold(&SourceKey::Orders(customer, order));
        let line_key = index.fold(&SourceKey::Lineitem(customer, order, line));
        prop_assert!(order_key.as_bytes().starts_with(&customer_prefix));
        prop_assert!(order_key.as_bytes().starts_with(&order_prefix));
        prop_assert!(line_key.as_bytes().starts_with(&order_prefix));
        prop_assert!(line_key.as_bytes().starts_with(&line_prefix));
        prop_assert!(!order_key.as_bytes().starts_with(&line_prefix));
    }
}

#[test]
fn prefixes_nest_and_end_at_the_expected_tags() {
    let index = index();
    let customer = index.prefix_custkey(12);
    let order = index.prefix_custkey_orderkey(12, -4);
    let line = index.prefix_custkey_orderkey_then_linenumber(12, -4);
    assert!(order.starts_with(&customer));
    assert!(line.starts_with(&order));
    assert_eq!(line.last(), Some(&3));
    assert_eq!(
        index.fold(&SourceKey::Lineitem(12, -4, 7)).as_bytes()[..line.len()],
        line
    );
}

fn test_buffer_cache() -> Option<std::sync::Arc<dbsp::storage::buffer_cache::BufferCache>> {
    thread_local! {
        static CACHE: std::sync::Arc<dbsp::storage::buffer_cache::BufferCache> =
            std::sync::Arc::new(dbsp::storage::buffer_cache::BufferCache::new(1024 * 1024));
    }
    Some(CACHE.with(Clone::clone))
}

type LayerReader = Reader<(
    &'static DynData,
    &'static DynUnit,
    (&'static DynData, &'static DynWeight, ()),
)>;

/// Reads only the file produced by `Writer2` in this fixture. That file's
/// rkyv rows have the concrete `Vec<u8>` and `ZWeight` types in `LayerReader`.
fn read_from(
    reader: &LayerReader,
    index: &CustomerOrdersLineitemIndex,
    lower_bound: &[u8],
    prefix: Option<&[u8]>,
    max_rows: usize,
) -> Vec<(SourceKey, Vec<(SourcePayload, ZWeight)>)> {
    let mut result = Vec::new();
    let target = lower_bound.to_vec();
    // SAFETY: `reader` comes directly from the matching Writer2 above. Its
    // archived rows were serialized by the factories used in this fixture.
    let mut cursor = unsafe { reader.rows().first().unwrap() };
    unsafe { cursor.advance_to_value_or_larger(target.erase()).unwrap() };
    while result.len() < max_rows {
        let Some(key) = cursor.key() else { break };
        let key = key.downcast_checked::<Vec<u8>>().clone();
        if prefix.is_some_and(|prefix| !key.starts_with(prefix)) {
            break;
        }
        let mut child = unsafe { cursor.next_column().unwrap().first().unwrap() };
        let mut children = Vec::new();
        while child.has_value() {
            let payload = child.key().unwrap().downcast_checked::<Vec<u8>>();
            let mut weight = 0_i64;
            unsafe { child.aux(weight.erase_mut()).unwrap() };
            children.push((index.decode_payload(&key, payload).unwrap(), weight));
            unsafe { child.move_next().unwrap() };
        }
        result.push((index.unfold(&key).unwrap(), children));
        unsafe { cursor.move_next().unwrap() };
    }
    result
}

#[test]
fn layer_file_weighted_children_and_seeks() {
    let index = index();
    let temp = tempdir().unwrap();
    let backend = <dyn StorageBackend>::new(
        &StorageConfig {
            path: temp.path().to_string_lossy().into_owned(),
            cache: Default::default(),
        },
        &StorageOptions::default(),
    )
    .unwrap();
    let factory0 = Factories::<DynData, DynUnit>::new::<Vec<u8>, ()>();
    let factory1 = Factories::<DynData, DynWeight>::new::<Vec<u8>, ZWeight>();
    let mut writer = Writer2::new(
        &factory0,
        &factory1,
        test_buffer_cache,
        &*backend,
        Parameters::default(),
        FilterPlan::<DynData>::decide_filter(None, 6),
    )
    .unwrap();

    let fixtures = [
        (
            SourceKey::Customer(1),
            vec![(
                SourcePayload::Customer(CustomerPayload {
                    segment: "BUILDING".into(),
                }),
                1,
            )],
        ),
        (
            SourceKey::Orders(1, 10),
            vec![(
                SourcePayload::Orders(OrdersPayload {
                    order_date: Date::from_days(1),
                    ship_priority: 0,
                }),
                1,
            )],
        ),
        (
            SourceKey::Lineitem(1, 10, 1),
            vec![(
                SourcePayload::Lineitem(LineitemPayload {
                    ship_date: Date::from_days(4),
                    extended_price: SqlDecimal::<15, 2>::new(100, 2).unwrap(),
                    discount: SqlDecimal::<4, 2>::new(5, 2).unwrap(),
                }),
                1,
            )],
        ),
        (
            SourceKey::Lineitem(1, 10, 2),
            vec![(
                SourcePayload::Lineitem(LineitemPayload {
                    ship_date: Date::from_days(5),
                    extended_price: SqlDecimal::<15, 2>::new(200, 2).unwrap(),
                    discount: SqlDecimal::<4, 2>::new(7, 2).unwrap(),
                }),
                1,
            )],
        ),
        (
            SourceKey::Orders(1, 20),
            vec![
                (
                    SourcePayload::Orders(OrdersPayload {
                        order_date: Date::from_days(1),
                        ship_priority: 0,
                    }),
                    -1,
                ),
                (
                    SourcePayload::Orders(OrdersPayload {
                        order_date: Date::from_days(2),
                        ship_priority: 0,
                    }),
                    1,
                ),
            ],
        ),
        (
            SourceKey::Customer(2),
            vec![(
                SourcePayload::Customer(CustomerPayload {
                    segment: "AUTOMOBILE".into(),
                }),
                1,
            )],
        ),
    ];
    let mut expected = Vec::new();
    for (key, payloads) in fixtures {
        let folded = index.fold(&key);
        let children: Vec<_> = payloads
            .into_iter()
            .map(|(payload, weight)| (index.encode_payload(&payload).unwrap(), weight, payload))
            .collect();
        assert!(children.windows(2).all(|pair| pair[0].0 < pair[1].0));
        let stored: Vec<_> = children
            .iter()
            .map(|(bytes, weight, _)| (bytes.clone(), *weight))
            .collect();
        write_parent(&mut writer, &folded, &stored).unwrap();
        expected.push((key, folded, children));
    }
    assert_eq!(writer.n_rows(), 6);
    let (reader, _) = writer.into_reader(BatchMetadata::default()).unwrap();
    let observed = read_from(&reader, &index, expected[4].1.as_bytes(), None, 1);
    let expected_children: Vec<_> = expected[4]
        .2
        .iter()
        .map(|(_, weight, payload)| (payload.clone(), *weight))
        .collect();
    assert_eq!(
        observed,
        vec![(SourceKey::Orders(1, 20), expected_children)]
    );

    let between = index.fold(&SourceKey::Orders(1, 15));
    assert_eq!(
        read_from(&reader, &index, between.as_bytes(), None, 1),
        observed
    );

    let prefix = index.prefix_custkey(1);
    let scanned = read_from(&reader, &index, &prefix, Some(&prefix), usize::MAX);
    assert_eq!(
        scanned.iter().map(|(key, _)| key).collect::<Vec<_>>(),
        expected[..5]
            .iter()
            .map(|(key, _, _)| key)
            .collect::<Vec<_>>()
    );
    for ((_, children), (_, _, expected_children)) in scanned.iter().zip(&expected[..5]) {
        assert_eq!(
            children,
            &expected_children
                .iter()
                .map(|(_, weight, payload)| (payload.clone(), *weight))
                .collect::<Vec<_>>()
        );
    }
    let after = index.fold(&SourceKey::Customer(3));
    assert!(read_from(&reader, &index, after.as_bytes(), None, 1).is_empty());
}
