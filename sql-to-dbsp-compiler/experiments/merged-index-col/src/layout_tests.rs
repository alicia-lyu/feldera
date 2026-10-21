//! Measures how many bytes the merged index's stored value takes, by source.
//!
//! The stored value is the packed bytes of `encode_payload` in a `Vec<u8>`,
//! written to a layer file of one source's rows, both plain and with
//! value-header elision, a disk-only transform. Native Lineitem values are
//! the baseline. The test reports each source's bytes per value row and pins
//! the sizes, so that a change of encoding updates them on purpose.
//!
//! The typed payload archive and the `rkyv` byte-string measurements were
//! retired at Step 5; their last figures are recorded in
//! `docs/design/merged-index/col-storage.md`.

use std::sync::Arc;

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
    trace::merged_index::MergedIndexDefinition,
    utils::{Tup1, Tup4},
    DBData, ZWeight,
};
use feldera_types::config::{StorageConfig, StorageOptions};

use super::{
    encode_payload, CustomerByCustkeyKey, CustomerByCustkeyPayload,
    CustomerOrdersLineitemDefinition, Key, LineitemByCustkeyOrderkeyLinenumberKey,
    LineitemByCustkeyOrderkeyLinenumberPayload, OrdersByCustkeyOrderkeyKey,
    OrdersByCustkeyOrderkeyPayload, Payload,
};
use feldera_sqllib::{Date, SqlDecimal};

/// Rows per single-source file.
const ROWS: i64 = 2_048;

fn customer_payload() -> CustomerByCustkeyPayload {
    // TPC-H market segments are at most 10 bytes long.
    CustomerByCustkeyPayload {
        c_mktsegment: "AUTOMOBILE".into(),
    }
}

fn orders_payload() -> OrdersByCustkeyOrderkeyPayload {
    OrdersByCustkeyOrderkeyPayload {
        o_orderdate: Date::from_days(9_000),
        o_shippriority: 0,
    }
}

fn lineitem_payload() -> LineitemByCustkeyOrderkeyLinenumberPayload {
    LineitemByCustkeyOrderkeyLinenumberPayload {
        l_shipdate: Date::from_days(9_010),
        l_extendedprice: SqlDecimal::<15, 2>::new(1_234_567, 2).unwrap(),
        l_discount: SqlDecimal::<4, 2>::new(5, 2).unwrap(),
    }
}

fn buffer_cache() -> Option<Arc<BufferCache>> {
    thread_local! {
        static BUFFER_CACHE: Arc<BufferCache> = Arc::new(BufferCache::new(1024 * 1024));
    }
    Some(BUFFER_CACHE.with(Arc::clone))
}

/// Writes `groups`, each a key and its values with weight 1, to a layer file
/// and accounts for its bytes.
fn layer_file<K: DBData, V: DBData>(
    groups: impl IntoIterator<Item = (K, Vec<V>)>,
    parameters: Parameters,
) -> BlockBytes {
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
    let mut writer =
        Writer2::new(&keys, &values, buffer_cache, &*backend, parameters, None).unwrap();
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
/// key with one value, the packed bytes of the row's payload.
fn merged_index_file(row: SourceRow, parameters: Parameters) -> BlockBytes {
    layer_file(
        (0..ROWS).map(|id| {
            let (key, payload) = row(id);
            assert!(payload.matches(&key));
            (
                CustomerOrdersLineitemDefinition::fold(&key)
                    .as_bytes()
                    .to_vec(),
                vec![encode_payload(&payload)],
            )
        }),
        parameters,
    )
}

/// Makes the row of one source with the given id.
type SourceRow = fn(i64) -> (Key, Payload);

fn customer_row(id: i64) -> (Key, Payload) {
    (
        Key::CustomerByCustkey(CustomerByCustkeyKey { custkey: id }),
        Payload::CustomerByCustkey(customer_payload()),
    )
}

fn orders_row(id: i64) -> (Key, Payload) {
    (
        Key::OrdersByCustkeyOrderkey(OrdersByCustkeyOrderkeyKey {
            custkey: id / 4,
            orderkey: id,
        }),
        Payload::OrdersByCustkeyOrderkey(orders_payload()),
    )
}

fn lineitem_row(id: i64) -> (Key, Payload) {
    (
        Key::LineitemByCustkeyOrderkeyLinenumber(LineitemByCustkeyOrderkeyLinenumberKey {
            custkey: id / 16,
            orderkey: id / 4,
            linenumber: (id % 4) as i32 + 1,
        }),
        Payload::LineitemByCustkeyOrderkeyLinenumber(lineitem_payload()),
    )
}

/// The native Lineitem join trace's value: line number, extended price,
/// discount, and ship date (`q3-ivm-baseline`'s generated circuit).
type NativeLineitemValue = Tup4<i32, SqlDecimal<15, 2>, SqlDecimal<4, 2>, Date>;

fn native_lineitem_value(line: i32) -> NativeLineitemValue {
    let p = lineitem_payload();
    Tup4(line, p.l_extendedprice, p.l_discount, p.l_shipdate)
}

/// Writer parameters without compression, so that the file takes the bytes
/// of its blocks.
fn plain_file() -> Parameters {
    Parameters::default().with_compression(None)
}

/// The same, with value-header elision.
fn elided_file() -> Parameters {
    plain_file().with_elide_value_headers(true)
}

/// The native Lineitem join trace keys lines by order: four lines per order
/// key.
fn native_lineitem_file() -> BlockBytes {
    layer_file(
        (0..ROWS / 4).map(|order| (Tup1(order), (1..=4).map(native_lineitem_value).collect())),
        plain_file(),
    )
}

fn value_bytes_per_row(bytes: &BlockBytes) -> f64 {
    let values = &bytes.levels[1];
    values.data as f64 / values.rows as f64
}

/// Value-level data bytes of the same 2,048 rows of each source, packed, with
/// and without value-header elision, against the native Lineitem value. Each
/// figure is the data-block bytes of a layer file written with no
/// compression, read back from the file, not computed.
#[test]
fn packed_value_bytes_against_native_lineitem() {
    let rows: [(&str, SourceRow); 3] = [
        ("customer", customer_row),
        ("orders", orders_row),
        ("lineitem", lineitem_row),
    ];
    let data = |bytes: &BlockBytes| bytes.levels[1].data;
    let measure =
        |parameters: fn() -> Parameters| rows.map(|(_, row)| merged_index_file(row, parameters()));
    let packed_plain = measure(plain_file);
    let packed_elided = measure(elided_file);
    let native = native_lineitem_file();

    for (index, (source, _)) in rows.iter().enumerate() {
        eprintln!(
            "{source}: value-level data bytes per row: packed {:.2}, packed elided {:.2}",
            value_bytes_per_row(&packed_plain[index]),
            value_bytes_per_row(&packed_elided[index]),
        );
    }
    eprintln!(
        "native lineitem: value-level data bytes per row {:.2}",
        value_bytes_per_row(&native)
    );
    for (name, files) in [("packed", &packed_plain), ("packed elided", &packed_elided)] {
        eprintln!("{name}: {:?}", files.each_ref().map(data));
    }
    eprintln!("native: {}", data(&native));

    // Measured on aarch64, 2026-10-19 and 2026-10-20.
    assert_eq!(packed_plain.each_ref().map(data), PIN_PACKED);
    assert_eq!(packed_elided.each_ref().map(data), PIN_PACKED_ELIDED);
    assert_eq!(data(&native), 132_608);
}

const PIN_PACKED: [u64; 3] = [82_944, 66_560, 82_944];
const PIN_PACKED_ELIDED: [u64; 3] = [46_592, 37_376, 51_712];
