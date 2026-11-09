use super::*;
use dbsp::trace::merged_index::batch::batch_cursor;
use dbsp::trace::merged_index::testing::check_against_trie;
use dbsp::trace::merged_index::{
    write_parent, ColumnRef, FoldedKey, MergedIndexDefinition, PlacedIndex,
};
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

fn fold(key: &Key) -> FoldedKey {
    CustomerOrdersLineitemDefinition::fold(key)
}

fn unfold(bytes: &[u8]) -> Result<Key, String> {
    CustomerOrdersLineitemDefinition::unfold(bytes)
}

fn customer_key(custkey: i64) -> Key {
    Key::CustomerByCustkey(CustomerByCustkeyKey { custkey })
}

fn order_key(custkey: i64, orderkey: i64) -> Key {
    Key::OrdersByCustkeyOrderkey(OrdersByCustkeyOrderkeyKey { custkey, orderkey })
}

fn line_key(custkey: i64, orderkey: i64, linenumber: i32) -> Key {
    Key::LineitemByCustkeyOrderkeyLinenumber(LineitemByCustkeyOrderkeyLinenumberKey {
        custkey,
        orderkey,
        linenumber,
    })
}

fn customer_payload(segment: &str) -> Payload {
    Payload::CustomerByCustkey(CustomerByCustkeyPayload {
        c_mktsegment: segment.into(),
    })
}

fn orders_payload(order_date: i32, ship_priority: i32) -> Payload {
    Payload::OrdersByCustkeyOrderkey(OrdersByCustkeyOrderkeyPayload {
        o_orderdate: Date::from_days(order_date),
        o_shippriority: ship_priority,
    })
}

fn lineitem_payload(ship_date: i32, price_mantissa: i128, discount_mantissa: i128) -> Payload {
    Payload::LineitemByCustkeyOrderkeyLinenumber(LineitemByCustkeyOrderkeyLinenumberPayload {
        l_shipdate: Date::from_days(ship_date),
        l_extendedprice: SqlDecimal::<15, 2>::new(price_mantissa, 2).unwrap(),
        l_discount: SqlDecimal::<4, 2>::new(discount_mantissa, 2).unwrap(),
    })
}

fn customer_row(custkey: i64, segment: &str, weight: i64) -> (Key, Payload, i64) {
    (customer_key(custkey), customer_payload(segment), weight)
}

fn cursor_row(
    cursor: &mut impl Cursor<DynData, DynData, (), DynZWeight>,
) -> (Key, Payload, ZWeight) {
    let weight = *cursor.weight().downcast_checked::<ZWeight>();
    let key = cursor.key().downcast_checked::<Vec<u8>>();
    let key = unfold(key).unwrap();
    let stored = cursor.val().downcast_checked::<StoredPayload>();
    let payload = decode_payload(&key, stored).unwrap();
    (key, payload, weight)
}

fn read_cursor(
    cursor: &mut impl Cursor<DynData, DynData, (), DynZWeight>,
) -> Vec<(Key, Payload, ZWeight)> {
    let mut rows = Vec::new();
    while cursor.key_valid() {
        while cursor.val_valid() {
            rows.push(cursor_row(cursor));
            cursor.step_val();
        }
        cursor.step_key();
    }
    rows
}

#[test]
fn memory_batch_sorts_and_consolidates_within_batch() {
    let batch = build_batch([
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
        read_cursor(&mut batch_cursor::<StoredPayload>(&[batch])),
        vec![
            customer_row(1, "A", 2),
            customer_row(2, "A", 4),
            customer_row(2, "B", -1),
        ]
    );
}

#[test]
fn cursor_consolidates_batches_without_changing_stored_rows() {
    let batches = [
        build_batch([
            customer_row(1, "A", 2),
            customer_row(2, "gone", 1),
            customer_row(3, "old", 1),
            customer_row(4, "A", 1),
            customer_row(5, "negative", -3),
        ])
        .unwrap(),
        build_batch([
            customer_row(1, "A", 3),
            customer_row(2, "gone", -1),
            customer_row(3, "old", -1),
            customer_row(3, "new", 1),
            customer_row(4, "B", 2),
        ])
        .unwrap(),
        build_batch([customer_row(1, "A", -1), customer_row(4, "A", -1)]).unwrap(),
    ];
    assert!(batches
        .iter()
        .all(|batch| batch.location() == BatchLocation::Memory));
    let stored_before: Vec<_> = batches
        .iter()
        .map(|batch| read_cursor(&mut batch.cursor()))
        .collect();
    let expected = vec![
        customer_row(1, "A", 4),
        customer_row(3, "new", 1),
        customer_row(4, "B", 2),
        customer_row(5, "negative", -3),
    ];
    let mut cursor = batch_cursor::<StoredPayload>(&batches);
    assert_eq!(read_cursor(&mut cursor), expected);
    cursor.rewind_keys();
    assert_eq!(read_cursor(&mut cursor), expected);
    let stored_after: Vec<_> = batches
        .iter()
        .map(|batch| read_cursor(&mut batch.cursor()))
        .collect();
    assert_eq!(stored_before, stored_after);
}

#[test]
fn cursor_exact_and_lower_bound_seeks_after_rewind() {
    let batches = [
        build_batch([customer_row(1, "A", 1), customer_row(2, "gone", 1)]).unwrap(),
        build_batch([
            customer_row(1, "A", 2),
            customer_row(2, "gone", -1),
            customer_row(3, "B", 1),
        ])
        .unwrap(),
    ];
    let mut cursor = batch_cursor::<StoredPayload>(&batches);
    let first = fold(&customer_key(1));
    assert!(cursor.seek_key_exact(first.as_bytes().to_vec().erase(), None));
    assert_eq!(cursor_row(&mut cursor), customer_row(1, "A", 3));
    let cancelled = fold(&customer_key(2));
    assert!(!cursor.seek_key_exact(cancelled.as_bytes().to_vec().erase(), None));
    let after = fold(&customer_key(4));
    assert!(!cursor.seek_key_exact(after.as_bytes().to_vec().erase(), None));
    cursor.rewind_keys();
    cursor.seek_key(cancelled.as_bytes().to_vec().erase());
    assert_eq!(read_cursor(&mut cursor), vec![customer_row(3, "B", 1)]);
    cursor.rewind_keys();
    cursor.seek_key(first.as_bytes().to_vec().erase());
    assert_eq!(cursor_row(&mut cursor), customer_row(1, "A", 3));
    cursor.seek_key(after.as_bytes().to_vec().erase());
    assert!(!cursor.key_valid());
}

#[test]
fn cursor_scans_interleaved_sources_and_stops_at_prefix_boundary() {
    let order = (order_key(5, 2), orders_payload(1, 0), 1);
    let line = (line_key(5, 2, 1), lineitem_payload(2, 100, 5), 1);
    let batches = [
        build_batch([customer_row(6, "other", 1), order.clone()]).unwrap(),
        build_batch([line.clone(), customer_row(5, "A", 1)]).unwrap(),
        build_batch([order.clone(), customer_row(5, "A", -1)]).unwrap(),
    ];
    let prefix = prefix_custkey(5);
    let mut cursor = batch_cursor::<StoredPayload>(&batches);
    cursor.seek_key(prefix.erase());
    let mut rows = Vec::new();
    while cursor.key_valid()
        && cursor
            .key()
            .downcast_checked::<Vec<u8>>()
            .starts_with(&prefix)
    {
        while cursor.val_valid() {
            rows.push(cursor_row(&mut cursor));
            cursor.step_val();
        }
        cursor.step_key();
    }
    assert_eq!(rows, vec![(order.0, order.1, 2), line]);
    assert_eq!(cursor_row(&mut cursor), customer_row(6, "other", 1));
}

#[test]
fn empty_batches_and_cancelled_rows_are_skipped() {
    assert!(!batch_cursor::<StoredPayload>(&[]).key_valid());
    let empty = [build_batch([]).unwrap()];
    assert!(!batch_cursor::<StoredPayload>(&empty).key_valid());
    let cancelled = [
        build_batch([customer_row(1, "A", 1)]).unwrap(),
        build_batch([customer_row(1, "A", -1)]).unwrap(),
    ];
    assert!(!batch_cursor::<StoredPayload>(&cancelled).key_valid());
    let batches = [
        build_batch([]).unwrap(),
        build_batch([customer_row(1, "A", 1), customer_row(1, "A", -1)]).unwrap(),
        build_batch([customer_row(2, "B", -3)]).unwrap(),
    ];
    assert_eq!(batches[0].len(), 0);
    assert_eq!(batches[1].key_count(), 0);
    let mut cursor = batch_cursor::<StoredPayload>(&batches);
    assert_eq!(read_cursor(&mut cursor), vec![customer_row(2, "B", -3)]);
    cursor.rewind_keys();
    let first = fold(&customer_key(1));
    cursor.seek_key(first.as_bytes().to_vec().erase());
    assert_eq!(cursor_row(&mut cursor), customer_row(2, "B", -3));
}

#[test]
fn batch_rejects_weight_overflow_and_mismatched_payload() {
    assert!(build_batch([customer_row(1, "A", i64::MAX), customer_row(1, "A", 1),]).is_err());
    assert!(build_batch([(customer_key(1), orders_payload(1, 0), 1,)]).is_err());
}

#[test]
fn exact_folded_keys_and_order() {
    let customer = fold(&customer_key(-1));
    let order = fold(&order_key(-1, 0));
    let line = fold(&line_key(-1, 0, 1));
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
    assert_eq!(prefix_custkey(-1), customer.as_bytes()[..9]);
    assert_eq!(prefix_custkey_orderkey(-1, 0), order.as_bytes()[..18]);
    assert_eq!(
        prefix_custkey_orderkey_then_linenumber(-1, 0),
        line.as_bytes()[..19]
    );
    for key in [
        customer_key(i64::MIN),
        order_key(i64::MAX, i64::MIN),
        line_key(i64::MIN, i64::MAX, i32::MAX),
    ] {
        assert_eq!(unfold(fold(&key).as_bytes()), Ok(key));
    }
    assert!(fold(&customer_key(-1)) < fold(&customer_key(0)));
    assert!(fold(&customer_key(0)) < fold(&customer_key(1)));
}

#[test]
fn malformed_folded_keys_are_rejected() {
    assert!(unfold(&[]).is_err());
    for key in [customer_key(1), order_key(1, 2), line_key(1, 2, 3)] {
        let bytes = fold(&key).as_bytes().to_vec();
        assert!(unfold(&bytes[..bytes.len() - 1]).is_err());
        let mut extra = bytes.clone();
        extra.push(0);
        assert!(unfold(&extra).is_err());
        let mut bad_index = bytes.clone();
        bad_index[bytes.len() - 2] = 0xff;
        assert!(unfold(&bad_index).is_err());
        let mut bad_id = bytes.clone();
        *bad_id.last_mut().unwrap() = 0xff;
        assert!(unfold(&bad_id).is_err());
        for offset in [0, 9, 18]
            .into_iter()
            .filter(|offset| *offset < bytes.len() - 2)
        {
            let mut bad_domain = bytes.clone();
            bad_domain[offset] = 0xff;
            assert!(unfold(&bad_domain).is_err());
        }
    }
}

#[test]
fn typed_payloads_round_trip_and_must_match_their_keys() {
    let rows = [
        (
            customer_key(i64::MAX),
            customer_payload(&"東京 café ".repeat(80)),
        ),
        (
            order_key(i64::MIN, i64::MAX),
            orders_payload(i32::MIN, i32::MAX),
        ),
        (
            line_key(i64::MIN, i64::MAX, i32::MIN),
            lineitem_payload(i32::MAX, -123_456, -99),
        ),
    ];
    let batch = build_batch(rows.iter().cloned().map(|(key, payload)| (key, payload, 1))).unwrap();
    let mut stored = read_cursor(&mut batch_cursor::<StoredPayload>(&[batch]));
    stored.sort_by_key(|(key, ..)| fold(key));
    let mut expected: Vec<_> = rows
        .iter()
        .cloned()
        .map(|(key, payload)| (key, payload, 1))
        .collect();
    expected.sort_by_key(|(key, ..)| fold(key));
    assert_eq!(stored, expected);

    // Each payload variant belongs to exactly one source index.
    for (i, (key, _)) in rows.iter().enumerate() {
        for (j, (_, payload)) in rows.iter().enumerate() {
            let matches = i == j;
            assert_eq!(payload.matches(key), matches, "{key:?} with {payload:?}");
            assert_eq!(
                build_batch([(key.clone(), payload.clone(), 1)]).is_ok(),
                matches,
                "{key:?} with {payload:?}"
            );
        }
    }
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
fn domain_widths_follow_the_key_types() {
    let widths = TRIE.domains.iter().map(|domain| domain.width);
    // custkey and orderkey are i64; linenumber is i32.
    assert_eq!(widths.collect::<Vec<_>>(), [8, 8, 4]);
    assert_eq!(TRIE.root_width(), Some(8));
}

#[test]
fn tags_and_ids_agree_with_the_trie_at_the_extremes() {
    assert_eq!(
        [TAG_CUSTKEY, TAG_ORDERKEY, TAG_LINENUMBER].map(Some),
        ["custkey", "orderkey", "linenumber"].map(|domain| TRIE.domain_tag(domain))
    );
    assert_eq!(
        [
            INDEX_ID_CUSTOMER_BY_CUSTKEY,
            INDEX_ID_ORDERS_BY_CUSTKEY_ORDERKEY,
            INDEX_ID_LINEITEM_BY_CUSTKEY_ORDERKEY_LINENUMBER
        ],
        TRIE.placements()
            .iter()
            .map(|placed| placed.id)
            .collect::<Vec<_>>()[..]
    );
    assert_eq!(CustomerOrdersLineitemDefinition::TRIE, TRIE);
    check_against_trie::<CustomerOrdersLineitemDefinition>([
        customer_key(i64::MIN),
        customer_key(i64::MAX),
        order_key(i64::MIN, i64::MAX),
        order_key(i64::MAX, i64::MIN),
        line_key(i64::MIN, i64::MAX, i32::MIN),
        line_key(i64::MAX, i64::MIN, i32::MAX),
        line_key(0, 0, 0),
    ]);
}

proptest! {
    #[test]
    fn folding_round_trips_and_agrees_with_the_trie(customer: i64, order: i64, line: i32) {
        check_against_trie::<CustomerOrdersLineitemDefinition>([
            customer_key(customer),
            order_key(customer, order),
            line_key(customer, order, line),
        ]);
    }

    /// Within one placement, byte order is the order of the key fields.
    #[test]
    fn folded_lineitem_order_is_field_order(
        left: (i64, i64, i32),
        right: (i64, i64, i32),
    ) {
        let fold = |(c, o, l): (i64, i64, i32)| {
            CustomerOrdersLineitemDefinition::fold(&line_key(c, o, l))
        };
        prop_assert_eq!(fold(left).cmp(&fold(right)), left.cmp(&right));
    }

    /// Every key of a customer, order, or line range starts with that range's prefix.
    #[test]
    fn prefixes_cover_exactly_their_ranges(customer: i64, order: i64, line: i32) {
        let customer_prefix = prefix_custkey(customer);
        let order_prefix = prefix_custkey_orderkey(customer, order);
        let line_prefix = prefix_custkey_orderkey_then_linenumber(customer, order);
        let order_key = fold(&order_key(customer, order));
        let line_key = fold(&line_key(customer, order, line));
        prop_assert!(order_key.as_bytes().starts_with(&customer_prefix));
        prop_assert!(order_key.as_bytes().starts_with(&order_prefix));
        prop_assert!(line_key.as_bytes().starts_with(&order_prefix));
        prop_assert!(line_key.as_bytes().starts_with(&line_prefix));
        prop_assert!(!order_key.as_bytes().starts_with(&line_prefix));
    }
}

#[test]
fn prefixes_nest_and_end_at_the_expected_tags() {
    let customer = prefix_custkey(12);
    let order = prefix_custkey_orderkey(12, -4);
    let line = prefix_custkey_orderkey_then_linenumber(12, -4);
    assert!(order.starts_with(&customer));
    assert!(line.starts_with(&order));
    assert_eq!(line.last(), Some(&3));
    assert_eq!(fold(&line_key(12, -4, 7)).as_bytes()[..line.len()], line);
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
/// rkyv rows have the concrete `Vec<u8>`, `StoredPayload`, and `ZWeight` types in `LayerReader`.
fn read_from(
    reader: &LayerReader,
    lower_bound: &[u8],
    prefix: Option<&[u8]>,
    max_rows: usize,
) -> Vec<(Key, Vec<(Payload, ZWeight)>)> {
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
        let key = unfold(&key).unwrap();
        let mut child = unsafe { cursor.next_column().unwrap().first().unwrap() };
        let mut children = Vec::new();
        while child.has_value() {
            let stored = child.key().unwrap().downcast_checked::<StoredPayload>();
            let payload = decode_payload(&key, stored).unwrap();
            let mut weight = 0_i64;
            unsafe { child.aux(weight.erase_mut()).unwrap() };
            children.push((payload, weight));
            unsafe { child.move_next().unwrap() };
        }
        result.push((key, children));
        unsafe { cursor.move_next().unwrap() };
    }
    result
}

#[test]
fn layer_file_weighted_children_and_seeks() {
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
    let factory1 = Factories::<DynData, DynWeight>::new::<StoredPayload, ZWeight>();
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
        (customer_key(1), vec![(customer_payload("BUILDING"), 1)]),
        (order_key(1, 10), vec![(orders_payload(1, 0), 1)]),
        (line_key(1, 10, 1), vec![(lineitem_payload(4, 100, 5), 1)]),
        (line_key(1, 10, 2), vec![(lineitem_payload(5, 200, 7), 1)]),
        (
            order_key(1, 20),
            vec![(orders_payload(1, 0), -1), (orders_payload(2, 0), 1)],
        ),
        (customer_key(2), vec![(customer_payload("AUTOMOBILE"), 1)]),
    ];
    let mut expected = Vec::new();
    for (key, payloads) in fixtures {
        let folded = fold(&key);
        let children: Vec<_> = payloads
            .into_iter()
            .map(|(payload, weight)| (encode_payload(&payload), weight, payload))
            .collect();
        assert!(children.windows(2).all(|pair| pair[0].0 < pair[1].0));
        let stored: Vec<_> = children
            .iter()
            .map(|(stored, weight, _)| (stored.clone(), *weight))
            .collect();
        write_parent(&mut writer, &folded, &stored).unwrap();
        expected.push((key, folded, children));
    }
    assert_eq!(writer.n_rows(), 6);
    let (reader, _) = writer.into_reader(BatchMetadata::default()).unwrap();
    let observed = read_from(&reader, expected[4].1.as_bytes(), None, 1);
    let expected_children: Vec<_> = expected[4]
        .2
        .iter()
        .map(|(_, weight, payload)| (payload.clone(), *weight))
        .collect();
    assert_eq!(observed, vec![(order_key(1, 20), expected_children)]);

    let between = fold(&order_key(1, 15));
    assert_eq!(read_from(&reader, between.as_bytes(), None, 1), observed);

    let prefix = prefix_custkey(1);
    let scanned = read_from(&reader, &prefix, Some(&prefix), usize::MAX);
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
    let after = fold(&customer_key(3));
    assert!(read_from(&reader, after.as_bytes(), None, 1).is_empty());
}

/// One key and payload per source, at the limits of their field types.
fn extreme_rows() -> [(Key, Payload); 3] {
    [
        (
            customer_key(i64::MAX),
            customer_payload(&"東京 café ".repeat(80)),
        ),
        (
            order_key(i64::MIN, i64::MAX),
            orders_payload(i32::MIN, i32::MAX),
        ),
        (
            line_key(i64::MIN, i64::MAX, i32::MIN),
            lineitem_payload(i32::MAX, -123_456, -99),
        ),
    ]
}

#[test]
fn packed_payloads_have_golden_bytes() {
    let customer = customer_payload("BUILDING");
    assert_eq!(encode_payload(&customer), b"\x08BUILDING");

    let orders = orders_payload(1, -2);
    assert_eq!(
        encode_payload(&orders),
        [1, 0, 0, 0, 0xfe, 0xff, 0xff, 0xff]
    );

    // A date takes 4 bytes, the price 8 (precision 15), the discount 2
    // (precision 4); nothing is padded and nothing is tagged.
    let lineitem = lineitem_payload(2, 100, 5);
    assert_eq!(
        encode_payload(&lineitem),
        [2, 0, 0, 0, 100, 0, 0, 0, 0, 0, 0, 0, 5, 0]
    );
}

#[test]
fn packed_payloads_round_trip_with_their_keys() {
    for (key, payload) in extreme_rows() {
        let bytes = encode_payload(&payload);
        assert_eq!(decode_payload(&key, &bytes).as_ref(), Ok(&payload));
    }
    // The limits of each narrowed decimal round-trip.
    let key = line_key(1, 2, 3);
    for price in [-(10_i128.pow(15) - 1), 0, 10_i128.pow(15) - 1] {
        for discount in [-9_999, 0, 9_999] {
            let payload = lineitem_payload(0, price, discount);
            let bytes = encode_payload(&payload);
            assert_eq!(decode_payload(&key, &bytes), Ok(payload));
        }
    }
}

#[test]
fn packed_payload_decoding_needs_the_matching_key() {
    let rows = extreme_rows();
    for (i, (key, _)) in rows.iter().enumerate() {
        for (j, (_, payload)) in rows.iter().enumerate() {
            let result = decode_payload(key, &encode_payload(payload));
            assert_eq!(result.is_ok(), i == j, "{key:?} with {payload:?}");
        }
    }
}

#[test]
fn packed_payloads_reject_truncated_trailing_and_out_of_range_bytes() {
    for (key, payload) in extreme_rows() {
        let bytes = encode_payload(&payload);
        for len in 0..bytes.len() {
            assert!(
                decode_payload(&key, &bytes[..len]).is_err(),
                "{key:?} decoded from {len} of {} bytes",
                bytes.len()
            );
        }
        let mut trailing = bytes;
        trailing.push(0);
        assert!(decode_payload(&key, &trailing).is_err(), "{key:?}");
    }

    let key = line_key(1, 2, 3);
    let packed = |price: i64, discount: i16| {
        let mut bytes = vec![0, 0, 0, 0];
        bytes.extend_from_slice(&price.to_le_bytes());
        bytes.extend_from_slice(&discount.to_le_bytes());
        bytes
    };
    assert!(decode_payload(&key, &packed(0, 0)).is_ok());
    // The narrow integers hold these mantissas, but the declared precisions
    // do not: precision 4 ends at 9,999 and precision 15 at 10^15 - 1.
    for discount in [10_000, -10_000, i16::MAX, i16::MIN] {
        assert!(decode_payload(&key, &packed(0, discount)).is_err());
    }
    for price in [10_i64.pow(15), -(10_i64.pow(15)), i64::MAX, i64::MIN] {
        assert!(decode_payload(&key, &packed(price, 0)).is_err());
    }

    // Strings: a padded length, invalid UTF-8, and a length past the input.
    let customer = customer_key(1);
    assert!(decode_payload(&customer, b"\x01A").is_ok());
    assert!(decode_payload(&customer, b"\x81\x00A").is_err());
    assert!(decode_payload(&customer, b"\x01\xff").is_err());
    assert!(decode_payload(&customer, b"\x02A").is_err());
}

proptest! {
    #[test]
    fn packed_lineitems_round_trip(
        ship_date: i32,
        price in -(10_i128.pow(15) - 1)..=(10_i128.pow(15) - 1),
        discount in -9_999_i128..=9_999,
    ) {
        let key = line_key(1, 2, 3);
        let payload = lineitem_payload(ship_date, price, discount);
        prop_assert_eq!(decode_payload(&key, &encode_payload(&payload)), Ok(payload));
    }

    #[test]
    fn packed_orders_and_customers_round_trip(
        order_date: i32,
        ship_priority: i32,
        segment in "\\PC{0,40}",
    ) {
        let orders = orders_payload(order_date, ship_priority);
        let customer = customer_payload(&segment);
        for (key, payload) in [
            (order_key(1, 2), orders),
            (customer_key(1), customer),
        ] {
            prop_assert_eq!(decode_payload(&key, &encode_payload(&payload)), Ok(payload));
        }
    }

    /// Whatever bytes decode must be exactly the encoding of what they
    /// decode to, so a payload has one stored form.
    #[test]
    fn packed_payload_decoding_is_canonical(
        bytes in proptest::collection::vec(any::<u8>(), 0..24),
    ) {
        for key in [
            customer_key(1),
            order_key(1, 2),
            line_key(1, 2, 3),
        ] {
            if let Ok(payload) = decode_payload(&key, &bytes) {
                prop_assert_eq!(encode_payload(&payload), bytes.clone());
            }
        }
    }
}
