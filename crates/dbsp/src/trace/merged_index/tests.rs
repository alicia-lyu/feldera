use super::customer_orders_lineitem::{
    CUSTOMER, CUSTOMER_DOMAIN, CUSTOMER_KEY_DOMAIN, CUSTOMER_PRIMARY,
    CustomerOrdersLineitemDefinition,
};
use super::memory_batch::batch_cursor;
use super::*;
use crate::{
    DynZWeight,
    dynamic::{DowncastTrait, DynData, DynUnit, DynWeight, Erase},
    storage::{
        backend::StorageBackend,
        file::{Factories, FilterPlan, format::BatchMetadata, reader::Reader, writer::Parameters},
    },
    trace::{BatchReader, Cursor},
};
use feldera_types::config::{StorageConfig, StorageOptions};
use tempfile::tempdir;

fn index() -> CustomerOrdersLineitemIndex {
    CustomerOrdersLineitemIndex::default()
}

fn customer_row(id: i32, segment: &str, weight: i64) -> (SourceKey, SourcePayload, i64) {
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
    let first = index.fold(&SourceKey::Customer(1)).unwrap();
    assert!(cursor.seek_key_exact(first.0.erase(), None));
    assert_eq!(cursor_row(&index, &mut cursor), customer_row(1, "A", 3));
    let cancelled = index.fold(&SourceKey::Customer(2)).unwrap();
    assert!(!cursor.seek_key_exact(cancelled.0.erase(), None));
    let after = index.fold(&SourceKey::Customer(4)).unwrap();
    assert!(!cursor.seek_key_exact(after.0.erase(), None));
    cursor.rewind_keys();
    cursor.seek_key(cancelled.0.erase());
    assert_eq!(
        read_cursor(&index, &mut cursor),
        vec![customer_row(3, "B", 1)]
    );
    cursor.rewind_keys();
    cursor.seek_key(first.0.erase());
    assert_eq!(cursor_row(&index, &mut cursor), customer_row(1, "A", 3));
    cursor.seek_key(after.0.erase());
    assert!(!cursor.key_valid());
}

#[test]
fn cursor_scans_interleaved_sources_and_stops_at_prefix_boundary() {
    let index = index();
    let order = (
        SourceKey::Orders(5, 2),
        SourcePayload::Orders(OrdersPayload {
            order_day: 1,
            ship_priority: 0,
        }),
        1,
    );
    let line = (
        SourceKey::Lineitem(5, 2, 1),
        SourcePayload::Lineitem(LineitemPayload {
            ship_day: 2,
            extended_price_cents: 100,
            discount_hundredths: 5,
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
    let prefix = index.customer_prefix(5);
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
    let first = index.fold(&SourceKey::Customer(1)).unwrap();
    cursor.seek_key(first.0.erase());
    assert_eq!(cursor_row(&index, &mut cursor), customer_row(2, "B", -3));
}

#[test]
fn batch_rejects_weight_overflow_and_mismatched_payload() {
    let index = index();
    assert!(
        index
            .build_batch([customer_row(1, "A", i64::MAX), customer_row(1, "A", 1),])
            .is_err()
    );
    assert!(
        index
            .build_batch([(
                SourceKey::Customer(1),
                SourcePayload::Orders(OrdersPayload {
                    order_day: 1,
                    ship_priority: 0,
                }),
                1,
            )])
            .is_err()
    );
}

#[test]
fn exact_folded_keys_and_order() {
    let index = index();
    let customer = index.fold(&SourceKey::Customer(-1)).unwrap();
    let order = index.fold(&SourceKey::Orders(-1, 0)).unwrap();
    let line = index.fold(&SourceKey::Lineitem(-1, 0, 1)).unwrap();
    assert_eq!(customer.as_bytes(), &[1, 0x7f, 0xff, 0xff, 0xff, 0, 1]);
    assert_eq!(
        order.as_bytes(),
        &[1, 0x7f, 0xff, 0xff, 0xff, 2, 0x80, 0, 0, 0, 0, 2]
    );
    assert_eq!(
        line.as_bytes(),
        &[
            1, 0x7f, 0xff, 0xff, 0xff, 2, 0x80, 0, 0, 0, 3, 0x80, 0, 0, 1, 0, 3
        ]
    );
    assert!(customer < order && order < line);
    assert_eq!(index.customer_prefix(-1), customer.as_bytes()[..5]);
    assert_eq!(index.order_prefix(-1, 0), order.as_bytes()[..10]);
    assert_eq!(index.line_prefix(-1, 0), line.as_bytes()[..11]);
    for key in [
        SourceKey::Customer(i32::MIN),
        SourceKey::Orders(i32::MAX, i32::MIN),
        SourceKey::Lineitem(i32::MIN, i32::MAX, i32::MAX),
    ] {
        assert_eq!(index.unfold(index.fold(&key).unwrap().as_bytes()), Ok(key));
    }
    assert!(
        index.fold(&SourceKey::Customer(-1)).unwrap()
            < index.fold(&SourceKey::Customer(0)).unwrap()
    );
    assert!(
        index.fold(&SourceKey::Customer(0)).unwrap() < index.fold(&SourceKey::Customer(1)).unwrap()
    );
}

#[test]
fn malformed_folded_keys_are_rejected() {
    let index = index();
    for key in [
        SourceKey::Customer(1),
        SourceKey::Orders(1, 2),
        SourceKey::Lineitem(1, 2, 3),
    ] {
        let bytes = index.fold(&key).unwrap().as_bytes().to_vec();
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
        for offset in (0..bytes.len() - 2).step_by(5) {
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
                order_day: -1,
                ship_priority: 7,
            }),
        ),
        (
            SourceKey::Lineitem(4, 5, 6),
            SourcePayload::Lineitem(LineitemPayload {
                ship_day: 9,
                extended_price_cents: 12345,
                discount_hundredths: 12,
            }),
        ),
    ];
    for (key, payload) in pairs {
        let folded = index.fold(&key).unwrap();
        let encoded = index.encode_payload(&payload).unwrap();
        assert_eq!(encoded, index.encode_payload(&payload).unwrap());
        assert_eq!(
            index.decode_payload(folded.as_bytes(), encoded.as_bytes()),
            Ok(payload)
        );
        assert!(index.decode_payload(folded.as_bytes(), &[1, 2, 3]).is_err());
        assert!(index.decode_payload(&[0], encoded.as_bytes()).is_err());
    }
}

struct Alternative;

impl MergedIndexDefinition for Alternative {
    type Key = SourceKey;
    const KEY_DOMAINS: &'static [KeyDomain] = &[CUSTOMER_KEY_DOMAIN];
    const SOURCE_INDEX_SPECS: &'static [SourceIndexSpec] = &[CUSTOMER_PRIMARY];
    const DOMAIN_BYTE_TAGS: &'static [DomainByteTag] = &[DomainByteTag {
        domain: CUSTOMER_DOMAIN,
        tag: 9,
    }];
    const SOURCE_BYTE_TAGS: &'static [SourceByteTag] = &[SourceByteTag {
        source_index: CUSTOMER,
        tag: 8,
    }];

    fn project(key: &Self::Key) -> (SourceIndexId, Vec<i32>) {
        match key {
            SourceKey::Customer(id) => (CUSTOMER, vec![*id]),
            _ => panic!("alternative definition only owns Customer"),
        }
    }

    fn construct(id: SourceIndexId, fields: &[i32]) -> Option<Self::Key> {
        match (id, fields) {
            (CUSTOMER, [customer]) => Some(SourceKey::Customer(*customer)),
            _ => None,
        }
    }
}

#[test]
fn generic_base_uses_definition_domains() {
    let alternative = MergedIndex::<Alternative>::default();
    let key = SourceKey::Customer(7);
    let bytes = alternative.fold(&key).unwrap();
    assert_eq!(bytes.as_bytes()[0], 9);
    assert_eq!(bytes.as_bytes().last(), Some(&8));
    assert_eq!(alternative.unfold(bytes.as_bytes()), Ok(key.clone()));
    assert_ne!(bytes, index().fold(&key).unwrap());
}

#[test]
fn q3_descriptors_keep_logical_fields_separate_from_tags() {
    let specs = CustomerOrdersLineitemDefinition::SOURCE_INDEX_SPECS;
    assert_eq!(
        specs.iter().map(|spec| spec.id.0).collect::<Vec<_>>(),
        ["CustomerPrimary", "OrdersByCustomer", "LineitemByCustomer"]
    );
    assert_eq!(
        specs
            .iter()
            .map(|spec| spec.base_relation.0)
            .collect::<Vec<_>>(),
        ["Customer", "Orders", "ExtendedLineitem"]
    );
    assert_eq!(
        CustomerOrdersLineitemDefinition::KEY_DOMAINS
            .iter()
            .map(|domain| (domain.id.0, domain.primitive))
            .collect::<Vec<_>>(),
        [
            ("customer", KeyPrimitive::I32),
            ("order", KeyPrimitive::I32),
            ("line", KeyPrimitive::I32)
        ]
    );
    for spec in specs {
        for field in spec.key_fields {
            assert!(
                CustomerOrdersLineitemDefinition::KEY_DOMAINS
                    .iter()
                    .any(|domain| domain.id == field.domain)
            );
            assert!(!spec.payload_fields.contains(&field.name));
        }
        assert!(
            spec.payload_fields
                .iter()
                .all(|field| !spec.key_fields.iter().any(|key| key.name == *field))
        );
    }
    assert_eq!(
        specs[0]
            .key_fields
            .iter()
            .map(|field| field.domain.0)
            .collect::<Vec<_>>(),
        ["customer"]
    );
    assert_eq!(
        specs[1]
            .key_fields
            .iter()
            .map(|field| field.domain.0)
            .collect::<Vec<_>>(),
        ["customer", "order"]
    );
    assert_eq!(
        specs[2]
            .key_fields
            .iter()
            .map(|field| field.domain.0)
            .collect::<Vec<_>>(),
        ["customer", "order", "line"]
    );
    assert_eq!(specs[0].payload_fields, ["segment"]);
    assert_eq!(specs[1].payload_fields, ["order_day", "ship_priority"]);
    assert_eq!(
        specs[2].payload_fields,
        ["ship_day", "extended_price_cents", "discount_hundredths"]
    );

    let index = index();
    let customer = index.customer_prefix(12);
    let order = index.order_prefix(12, -4);
    let line = index.line_prefix(12, -4);
    assert!(order.starts_with(&customer));
    assert!(line.starts_with(&order));
    assert_eq!(line.last(), Some(&3));
    assert_eq!(
        index
            .fold(&SourceKey::Lineitem(12, -4, 7))
            .unwrap()
            .as_bytes()[..line.len()],
        line
    );
}

fn test_buffer_cache() -> Option<std::sync::Arc<crate::storage::buffer_cache::BufferCache>> {
    thread_local! {
        static CACHE: std::sync::Arc<crate::storage::buffer_cache::BufferCache> =
            std::sync::Arc::new(crate::storage::buffer_cache::BufferCache::new(1024 * 1024));
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
                    order_day: 1,
                    ship_priority: 0,
                }),
                1,
            )],
        ),
        (
            SourceKey::Lineitem(1, 10, 1),
            vec![(
                SourcePayload::Lineitem(LineitemPayload {
                    ship_day: 4,
                    extended_price_cents: 100,
                    discount_hundredths: 5,
                }),
                1,
            )],
        ),
        (
            SourceKey::Lineitem(1, 10, 2),
            vec![(
                SourcePayload::Lineitem(LineitemPayload {
                    ship_day: 5,
                    extended_price_cents: 200,
                    discount_hundredths: 7,
                }),
                1,
            )],
        ),
        (
            SourceKey::Orders(1, 20),
            vec![
                (
                    SourcePayload::Orders(OrdersPayload {
                        order_day: 1,
                        ship_priority: 0,
                    }),
                    -1,
                ),
                (
                    SourcePayload::Orders(OrdersPayload {
                        order_day: 2,
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
        let folded = index.fold(&key).unwrap();
        let children: Vec<_> = payloads
            .into_iter()
            .map(|(payload, weight)| (index.encode_payload(&payload).unwrap(), weight, payload))
            .collect();
        assert!(children.windows(2).all(|pair| pair[0].0 < pair[1].0));
        let stored: Vec<_> = children
            .iter()
            .map(|(bytes, weight, _)| (bytes.clone(), *weight))
            .collect();
        index
            .base
            .write_parent(&mut writer, &folded, &stored)
            .unwrap();
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

    let between = index.fold(&SourceKey::Orders(1, 15)).unwrap();
    assert_eq!(
        read_from(&reader, &index, between.as_bytes(), None, 1),
        observed
    );

    let prefix = index.customer_prefix(1);
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
    let after = index.fold(&SourceKey::Customer(3)).unwrap();
    assert!(read_from(&reader, &index, after.as_bytes(), None, 1).is_empty());
}
