//! Measures how many bytes a merged-index value takes, by source.
//!
//! The stored value is `SourcePayload`, whose archive is one fixed-size enum
//! for every source. These tests report each source's bytes per value row,
//! next to the byte-string form stored before values were typed, and pin the
//! sizes so that a change of encoding updates them on purpose.

use std::{
    mem::{align_of, size_of},
    sync::Arc,
};

use dbsp::{
    dynamic::DynData,
    storage::{
        backend::StorageBackend,
        buffer_cache::BufferCache,
        file::{
            block_stats::{layer_file_block_bytes, BlockBytes},
            format::BatchMetadata,
            writer::{Parameters, Writer2},
            Factories,
        },
    },
    utils::{Tup1, Tup2, Tup4},
    DBData, ZWeight,
};
use feldera_types::config::{StorageConfig, StorageOptions};
use rkyv::{to_bytes, Archived};

use super::{
    CustomerOrdersLineitemIndex, CustomerPayload, Date, LineitemPayload, OrdersPayload, SourceKey,
    SourcePayload, SqlDecimal,
};

/// Rows per single-source file.
const ROWS: i64 = 2_048;

fn customer_payload() -> CustomerPayload {
    // TPC-H market segments are at most 10 bytes long.
    CustomerPayload {
        segment: "AUTOMOBILE".into(),
    }
}

fn orders_payload() -> OrdersPayload {
    OrdersPayload {
        order_date: Date::from_days(9_000),
        ship_priority: 0,
    }
}

fn lineitem_payload() -> LineitemPayload {
    LineitemPayload {
        ship_date: Date::from_days(9_010),
        extended_price: SqlDecimal::<15, 2>::new(1_234_567, 2).unwrap(),
        discount: SqlDecimal::<4, 2>::new(5, 2).unwrap(),
    }
}

fn buffer_cache() -> Option<Arc<BufferCache>> {
    thread_local! {
        static BUFFER_CACHE: Arc<BufferCache> = Arc::new(BufferCache::new(1024 * 1024));
    }
    Some(BUFFER_CACHE.with(Arc::clone))
}

/// Bytes of the archive of one value-level item, a value and its weight.
fn item_bytes<T>(value: T) -> usize
where
    Tup2<T, ZWeight>: rkyv::Serialize<rkyv::ser::serializers::AllocSerializer<256>>,
{
    to_bytes::<_, 256>(&Tup2(value, 1)).unwrap().len()
}

/// Bytes of the archive of `payload` alone, as `encode_payload` stored it
/// before values were typed.
fn payload_archive<T>(payload: &T) -> Vec<u8>
where
    T: rkyv::Serialize<rkyv::ser::serializers::AllocSerializer<256>>,
{
    to_bytes::<_, 256>(payload).unwrap().to_vec()
}

#[test]
fn payload_item_bytes() {
    let typed = [
        (
            "customer",
            item_bytes(SourcePayload::Customer(customer_payload())),
        ),
        (
            "orders",
            item_bytes(SourcePayload::Orders(orders_payload())),
        ),
        (
            "lineitem",
            item_bytes(SourcePayload::Lineitem(lineitem_payload())),
        ),
    ];
    let byte_string = [
        ("customer", item_bytes(payload_archive(&customer_payload()))),
        ("orders", item_bytes(payload_archive(&orders_payload()))),
        ("lineitem", item_bytes(payload_archive(&lineitem_payload()))),
    ];
    let archives = [
        (
            "SourcePayload",
            size_of::<Archived<SourcePayload>>(),
            align_of::<Archived<SourcePayload>>(),
        ),
        (
            "CustomerPayload",
            size_of::<Archived<CustomerPayload>>(),
            align_of::<Archived<CustomerPayload>>(),
        ),
        (
            "OrdersPayload",
            size_of::<Archived<OrdersPayload>>(),
            align_of::<Archived<OrdersPayload>>(),
        ),
        (
            "LineitemPayload",
            size_of::<Archived<LineitemPayload>>(),
            align_of::<Archived<LineitemPayload>>(),
        ),
    ];
    for ((source, typed), (_, bytes)) in typed.iter().zip(&byte_string) {
        eprintln!("item bytes, {source}: typed {typed}, byte string {bytes}");
    }
    for (name, size, align) in archives {
        eprintln!("Archived<{name}>: size {size}, align {align}");
    }
    let native_item = item_bytes(native_lineitem_value(1));
    eprintln!(
        "native lineitem item {native_item}, Archived<Tup4<..>>: size {}, align {}",
        size_of::<Archived<NativeLineitemValue>>(),
        align_of::<Archived<NativeLineitemValue>>()
    );
    // A byte-string item is the variant's archive, padding to the 8-byte
    // alignment of the `ArchivedVec` header, the header, and the weight.
    let archive_lens = [
        payload_archive(&customer_payload()).len(),
        payload_archive(&orders_payload()).len(),
        payload_archive(&lineitem_payload()).len(),
    ];
    for (((source, item), archive), header) in byte_string
        .iter()
        .zip(archive_lens)
        .zip([size_of::<Archived<Vec<u8>>>(); 3])
    {
        let weight = size_of::<ZWeight>();
        eprintln!(
            "byte string, {source}: item {item} = archive {archive} + padding {} + header {header} + weight {weight}",
            item - archive - header - weight
        );
    }
    // The enum's archive is its root, the last bytes of `to_bytes`. Its first
    // byte is the variant's discriminant (Customer 0, Orders 1, Lineitem 2);
    // the variant's fields start at the variant's alignment.
    for payload in [
        SourcePayload::Orders(orders_payload()),
        SourcePayload::Lineitem(lineitem_payload()),
    ] {
        let bytes = to_bytes::<_, 256>(&payload).unwrap();
        let root = &bytes[bytes.len() - size_of::<Archived<SourcePayload>>()..];
        eprintln!("archived {payload:?}:");
        for row in root.chunks(16) {
            eprintln!("  {row:02x?}");
        }
    }

    // Measured on aarch64, 2026-10-19. A typed value takes the same bytes
    // whatever its source, because its archive is sized to the largest
    // variant.
    assert_eq!(typed.map(|(_, bytes)| bytes), [80, 80, 80]);
    assert_eq!(byte_string.map(|(_, bytes)| bytes), [40, 32, 72]);
    assert_eq!(
        archives.map(|(_, size, align)| (size, align)),
        [(64, 16), (16, 8), (8, 4), (48, 16)]
    );
    assert_eq!(archive_lens, [16, 8, 48]);
    assert_eq!(native_item, 64);
}

/// Writes `groups`, each a key and its values with weight 1, to a layer file
/// and accounts for its bytes.
fn layer_file<K: DBData, V: DBData>(groups: impl IntoIterator<Item = (K, Vec<V>)>) -> BlockBytes {
    let tempdir = tempfile::tempdir().unwrap();
    let backend = <dyn StorageBackend>::new(
        &StorageConfig {
            path: tempdir.path().to_string_lossy().into_owned(),
            cache: Default::default(),
        },
        &StorageOptions::default(),
    )
    .unwrap();
    let keys = Factories::<DynData, DynData>::new::<K, ()>();
    let values = Factories::<DynData, DynData>::new::<V, ZWeight>();
    let mut writer = Writer2::new(
        &keys,
        &values,
        buffer_cache,
        &*backend,
        Parameters::default().with_compression(None),
        None,
    )
    .unwrap();
    for (key, values) in groups {
        for value in &values {
            writer.write1((value, &(1 as ZWeight))).unwrap();
        }
        writer.write0((&key, &())).unwrap();
    }
    let path = tempdir.path().join(writer.path().as_ref());
    // The file is deleted when its last reader drops.
    let (_file, _filter, _bounds) = writer.close(BatchMetadata::default()).unwrap();
    layer_file_block_bytes(&path).unwrap()
}

/// The `ROWS` rows of one source, as the merged index stores them: a folded
/// key with one value, made from the row's payload by `value`.
fn merged_index_file<V: DBData>(
    row: impl Fn(i64) -> (SourceKey, SourcePayload),
    value: impl Fn(SourcePayload) -> V,
) -> BlockBytes {
    let index = CustomerOrdersLineitemIndex;
    layer_file((0..ROWS).map(|id| {
        let (key, payload) = row(id);
        assert!(payload.matches(&key));
        (index.fold(&key).as_bytes().to_vec(), vec![value(payload)])
    }))
}

/// Makes the row of one source with the given id.
type SourceRow = fn(i64) -> (SourceKey, SourcePayload);

fn customer_row(id: i64) -> (SourceKey, SourcePayload) {
    (
        SourceKey::Customer(id),
        SourcePayload::Customer(customer_payload()),
    )
}

fn orders_row(id: i64) -> (SourceKey, SourcePayload) {
    (
        SourceKey::Orders(id / 4, id),
        SourcePayload::Orders(orders_payload()),
    )
}

fn lineitem_row(id: i64) -> (SourceKey, SourcePayload) {
    (
        SourceKey::Lineitem(id / 16, id / 4, (id % 4) as i32 + 1),
        SourcePayload::Lineitem(lineitem_payload()),
    )
}

/// The archive of the payload's own variant, the byte-string value stored
/// before values were typed.
fn byte_string(payload: SourcePayload) -> Vec<u8> {
    match payload {
        SourcePayload::Customer(p) => payload_archive(&p),
        SourcePayload::Orders(p) => payload_archive(&p),
        SourcePayload::Lineitem(p) => payload_archive(&p),
    }
}

/// The native Lineitem join trace's value: line number, extended price,
/// discount, and ship date (`q3-ivm-baseline`'s generated circuit).
type NativeLineitemValue = Tup4<i32, SqlDecimal<15, 2>, SqlDecimal<4, 2>, Date>;

fn native_lineitem_value(line: i32) -> NativeLineitemValue {
    let p = lineitem_payload();
    Tup4(line, p.extended_price, p.discount, p.ship_date)
}

fn value_bytes_per_row(bytes: &BlockBytes) -> f64 {
    let values = &bytes.levels[1];
    values.data as f64 / values.rows as f64
}

#[test]
fn value_level_bytes_by_source() {
    let rows: [(&str, SourceRow); 3] = [
        ("customer", customer_row),
        ("orders", orders_row),
        ("lineitem", lineitem_row),
    ];
    let typed = rows.map(|(_, row)| merged_index_file(row, |payload| payload));
    let byte_strings = rows.map(|(_, row)| merged_index_file(row, byte_string));
    // The native trace keys lines by order: four lines per order key.
    let native = layer_file(
        (0..ROWS / 4).map(|order| (Tup1(order), (1..=4).map(native_lineitem_value).collect())),
    );
    for (((source, _), typed), byte_string) in rows.iter().zip(&typed).zip(&byte_strings) {
        eprintln!(
            "{source}: value-level data, typed {} B ({:.2} B per row), byte string {} B ({:.2} B per row)",
            typed.levels[1].data,
            value_bytes_per_row(typed),
            byte_string.levels[1].data,
            value_bytes_per_row(byte_string),
        );
    }
    eprintln!(
        "native lineitem: value-level data {} B ({:.2} B per row)",
        native.levels[1].data,
        value_bytes_per_row(&native)
    );

    // Measured on aarch64, 2026-10-19. Typed values take the same bytes
    // whatever their source; byte strings take their own variant's bytes.
    let data = |bytes: &BlockBytes| bytes.levels[1].data;
    assert_eq!(typed.each_ref().map(data), [166_400, 166_400, 166_400]);
    assert_eq!(byte_strings.each_ref().map(data), [82_944, 66_560, 148_992]);
    assert_eq!(data(&native), 132_608);
}
