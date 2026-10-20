//! Roadmap Step 4 exit checks: the module that the SQL compiler generates from
//! `col.mi` (`generated.rs`) agrees byte for byte with the hand-written
//! definition in `lib.rs`.

use super::*;
use crate::generated;
use dbsp::trace::merged_index::testing::check_against_trie;
use dbsp::trace::merged_index::MergedIndexDefinition;
use feldera_sqllib::{Date, SqlDecimal};
use proptest::prelude::*;

/// The generated key equal to a hand-written one.
fn generated_key(key: &SourceKey) -> generated::Key {
    match *key {
        SourceKey::Customer(custkey) => {
            generated::Key::CustomerByCustkey(generated::CustomerByCustkeyKey { custkey })
        }
        SourceKey::Orders(custkey, orderkey) => {
            generated::Key::OrdersByCustkeyOrderkey(generated::OrdersByCustkeyOrderkeyKey {
                custkey,
                orderkey,
            })
        }
        SourceKey::Lineitem(custkey, orderkey, linenumber) => {
            generated::Key::LineitemByCustkeyOrderkeyLinenumber(
                generated::LineitemByCustkeyOrderkeyLinenumberKey {
                    custkey,
                    orderkey,
                    linenumber,
                },
            )
        }
    }
}

/// The generated payload equal to a hand-written one.
fn generated_payload(payload: &SourcePayload) -> generated::Payload {
    match payload {
        SourcePayload::Customer(p) => {
            generated::Payload::CustomerByCustkey(generated::CustomerByCustkeyPayload {
                c_mktsegment: p.segment.clone(),
            })
        }
        SourcePayload::Orders(p) => {
            generated::Payload::OrdersByCustkeyOrderkey(generated::OrdersByCustkeyOrderkeyPayload {
                o_orderdate: p.order_date,
                o_shippriority: p.ship_priority,
            })
        }
        SourcePayload::Lineitem(p) => generated::Payload::LineitemByCustkeyOrderkeyLinenumber(
            generated::LineitemByCustkeyOrderkeyLinenumberPayload {
                l_shipdate: p.ship_date,
                l_extendedprice: p.extended_price,
                l_discount: p.discount,
            },
        ),
    }
}

/// Both definitions fold `key` to the same bytes, and each unfolds the other's.
fn assert_same_folded_key(key: &SourceKey) {
    let hand_written = CustomerOrdersLineitemDefinition::fold(key);
    let generated = generated::CustomerOrdersLineitemDefinition::fold(&generated_key(key));
    assert_eq!(generated.as_bytes(), hand_written.as_bytes(), "{key:?}");
    assert_eq!(
        generated::CustomerOrdersLineitemDefinition::unfold(hand_written.as_bytes()),
        Ok(generated_key(key))
    );
    assert_eq!(
        CustomerOrdersLineitemDefinition::unfold(generated.as_bytes()).as_ref(),
        Ok(key)
    );
}

/// Both modules pack `payload` to the same bytes, and each decodes the other's.
fn assert_same_payload_bytes(key: &SourceKey, payload: &SourcePayload) {
    let hand_written = encode_payload(payload);
    let generated = generated::encode_payload(&generated_payload(payload));
    assert_eq!(generated, hand_written, "{payload:?}");
    assert_eq!(
        generated::decode_payload(&generated_key(key), &hand_written),
        Ok(generated_payload(payload))
    );
    assert_eq!(decode_payload(key, &generated).as_ref(), Ok(payload));
}

fn customer(segment: &str) -> SourcePayload {
    SourcePayload::Customer(CustomerPayload {
        segment: segment.into(),
    })
}

fn orders(order_date: i32, ship_priority: i32) -> SourcePayload {
    SourcePayload::Orders(OrdersPayload {
        order_date: Date::from_days(order_date),
        ship_priority,
    })
}

fn lineitem(ship_date: i32, price_mantissa: i128, discount_mantissa: i128) -> SourcePayload {
    SourcePayload::Lineitem(LineitemPayload {
        ship_date: Date::from_days(ship_date),
        extended_price: SqlDecimal::<15, 2>::new(price_mantissa, 2).unwrap(),
        discount: SqlDecimal::<4, 2>::new(discount_mantissa, 2).unwrap(),
    })
}

/// Keys at the limits of their field types.
fn extreme_keys() -> Vec<SourceKey> {
    vec![
        SourceKey::Customer(i64::MIN),
        SourceKey::Customer(i64::MAX),
        SourceKey::Orders(i64::MIN, i64::MAX),
        SourceKey::Orders(i64::MAX, i64::MIN),
        SourceKey::Lineitem(i64::MIN, i64::MAX, i32::MIN),
        SourceKey::Lineitem(i64::MAX, i64::MIN, i32::MAX),
        SourceKey::Lineitem(0, 0, 0),
    ]
}

#[test]
fn generated_and_hand_written_tries_are_equal() {
    assert_eq!(generated::TRIE, TRIE);
    assert_eq!(generated::TRIE.validate(), Ok(()));
}

#[test]
fn generated_constants_agree_with_the_trie() {
    check_against_trie::<generated::CustomerOrdersLineitemDefinition>(
        extreme_keys().iter().map(generated_key),
    );
    assert_eq!(generated::TAG_CUSTKEY, CUSTKEY);
    assert_eq!(generated::TAG_ORDERKEY, ORDERKEY);
    assert_eq!(generated::TAG_LINENUMBER, LINENUMBER);
    assert_eq!(generated::INDEX_ID_CUSTOMER_BY_CUSTKEY, CUSTOMER_ID);
    assert_eq!(generated::INDEX_ID_ORDERS_BY_CUSTKEY_ORDERKEY, ORDERS_ID);
    assert_eq!(
        generated::INDEX_ID_LINEITEM_BY_CUSTKEY_ORDERKEY_LINENUMBER,
        LINEITEM_ID
    );
}

#[test]
fn folded_keys_are_identical_at_the_extremes() {
    for key in extreme_keys() {
        assert_same_folded_key(&key);
    }
}

#[test]
fn generated_unfold_rejects_what_the_hand_written_one_rejects() {
    let valid = CustomerOrdersLineitemDefinition::fold(&SourceKey::Orders(1, 2));
    let bytes = valid.as_bytes();
    let mut unknown_id = bytes.to_vec();
    *unknown_id.last_mut().unwrap() = 9;
    let mut bad_tag = bytes.to_vec();
    bad_tag[0] = 7;
    for malformed in [
        &bytes[..bytes.len() - 1],
        &unknown_id[..],
        &bad_tag[..],
        &[][..],
    ] {
        assert!(CustomerOrdersLineitemDefinition::unfold(malformed).is_err());
        assert!(generated::CustomerOrdersLineitemDefinition::unfold(malformed).is_err());
    }
}

#[test]
fn payload_bytes_are_identical_at_the_limits() {
    let limit = 10_i128.pow(15) - 1;
    let rows = [
        (SourceKey::Customer(1), customer("")),
        (SourceKey::Customer(1), customer(&"東京 café ".repeat(80))),
        (SourceKey::Orders(1, 2), orders(i32::MIN, i32::MAX)),
        (SourceKey::Orders(1, 2), orders(i32::MAX, i32::MIN)),
        (
            SourceKey::Lineitem(1, 2, 3),
            lineitem(i32::MIN, limit, 9_999),
        ),
        (
            SourceKey::Lineitem(1, 2, 3),
            lineitem(i32::MAX, -limit, -9_999),
        ),
    ];
    for (key, payload) in &rows {
        assert_same_payload_bytes(key, payload);
    }
}

#[test]
fn payloads_match_the_same_keys() {
    let keys = [
        SourceKey::Customer(1),
        SourceKey::Orders(1, 2),
        SourceKey::Lineitem(1, 2, 3),
    ];
    let payloads = [customer("BUILDING"), orders(1, 2), lineitem(1, 2, 3)];
    for key in &keys {
        for payload in &payloads {
            assert_eq!(
                generated_payload(payload).matches(&generated_key(key)),
                payload.matches(key),
                "{key:?} {payload:?}"
            );
        }
    }
}

proptest! {
    #[test]
    fn folded_keys_are_identical(custkey: i64, orderkey: i64, linenumber: i32) {
        assert_same_folded_key(&SourceKey::Customer(custkey));
        assert_same_folded_key(&SourceKey::Orders(custkey, orderkey));
        assert_same_folded_key(&SourceKey::Lineitem(custkey, orderkey, linenumber));
    }

    #[test]
    fn prefixes_are_identical(custkey: i64, orderkey: i64) {
        let index = CustomerOrdersLineitemIndex;
        prop_assert_eq!(generated::prefix_custkey(custkey), index.prefix_custkey(custkey));
        prop_assert_eq!(
            generated::prefix_custkey_orderkey(custkey, orderkey),
            index.prefix_custkey_orderkey(custkey, orderkey)
        );
        prop_assert_eq!(
            generated::prefix_custkey_orderkey_then_linenumber(custkey, orderkey),
            index.prefix_custkey_orderkey_then_linenumber(custkey, orderkey)
        );
    }

    #[test]
    fn payload_bytes_are_identical(
        segment in "\\PC{0,40}",
        order_date: i32,
        ship_priority: i32,
        ship_date: i32,
        price in -(10_i128.pow(15) - 1)..=(10_i128.pow(15) - 1),
        discount in -9_999_i128..=9_999,
    ) {
        assert_same_payload_bytes(&SourceKey::Customer(1), &customer(&segment));
        assert_same_payload_bytes(&SourceKey::Orders(1, 2), &orders(order_date, ship_priority));
        assert_same_payload_bytes(&SourceKey::Lineitem(1, 2, 3), &lineitem(ship_date, price, discount));
    }
}
