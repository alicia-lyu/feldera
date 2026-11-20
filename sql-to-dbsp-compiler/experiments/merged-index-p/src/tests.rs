use super::*;
use dbsp::trace::merged_index::batch::{batch_cursor, index_factories};
use dbsp::trace::merged_index::testing::check_against_trie;
use dbsp::trace::merged_index::{
    ColumnRef, FoldedKey, MergedIndexBatch, MergedIndexDefinition, PlacedIndex,
};
use dbsp::{dynamic::DowncastTrait, dynamic::DynData, trace::Cursor, DynZWeight, ZWeight};
use feldera_sqllib::{Date, SqlDecimal};
use proptest::prelude::*;

fn fold(key: &Key) -> FoldedKey {
    PartPartsuppLineitemDefinition::fold(key)
}

fn unfold(bytes: &[u8]) -> Result<Key, String> {
    PartPartsuppLineitemDefinition::unfold(bytes)
}

fn part_key(partkey: i64) -> Key {
    Key::PartByPartkey(PartByPartkeyKey { partkey })
}

fn partsupp_key(partkey: i64, suppkey: i64) -> Key {
    Key::PartsuppByPartkeySuppkey(PartsuppByPartkeySuppkeyKey { partkey, suppkey })
}

fn line_key(partkey: i64, suppkey: i64, orderkey: i64, linenumber: i32) -> Key {
    Key::LineitemByPartkeySuppkeyOrderkeyLinenumber(LineitemByPartkeySuppkeyOrderkeyLinenumberKey {
        partkey,
        suppkey,
        orderkey,
        linenumber,
    })
}

fn part_payload(part_type: &str) -> Payload {
    Payload::PartByPartkey(PartByPartkeyPayload {
        p_type: part_type.into(),
    })
}

fn partsupp_payload(available: i32) -> Payload {
    Payload::PartsuppByPartkeySuppkey(PartsuppByPartkeySuppkeyPayload {
        ps_availqty: available,
    })
}

fn lineitem_payload(ship_date: i32, price_mantissa: i128, discount_mantissa: i128) -> Payload {
    Payload::LineitemByPartkeySuppkeyOrderkeyLinenumber(
        LineitemByPartkeySuppkeyOrderkeyLinenumberPayload {
            l_shipdate: Date::from_days(ship_date),
            l_extendedprice: SqlDecimal::<15, 2>::new(price_mantissa, 2).unwrap(),
            l_discount: SqlDecimal::<4, 2>::new(discount_mantissa, 2).unwrap(),
        },
    )
}

fn read_cursor(
    cursor: &mut impl Cursor<DynData, DynData, (), DynZWeight>,
) -> Vec<(Key, Payload, ZWeight)> {
    let mut rows = Vec::new();
    while cursor.key_valid() {
        while cursor.val_valid() {
            let weight = *cursor.weight().downcast_checked::<ZWeight>();
            let key = unfold(cursor.key().downcast_checked::<Vec<u8>>()).unwrap();
            let stored = cursor.val().downcast_checked::<StoredPayload>();
            let payload = decode_payload(&key, stored).unwrap();
            rows.push((key, payload, weight));
            cursor.step_val();
        }
        cursor.step_key();
    }
    rows
}

/// One key and payload per placement, at the limits of their field types.
fn extreme_rows() -> [(Key, Payload); 3] {
    [
        (part_key(i64::MAX), part_payload(&"東京 café ".repeat(80))),
        (partsupp_key(i64::MIN, i64::MAX), partsupp_payload(i32::MIN)),
        (
            line_key(i64::MIN, i64::MAX, i64::MIN, i32::MIN),
            lineitem_payload(i32::MAX, -123_456, -99),
        ),
    ]
}

// Folded keys.

#[test]
fn exact_folded_keys() {
    const MINUS_ONE: [u8; 8] = [0x7f, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff];
    const ZERO: [u8; 8] = [0x80, 0, 0, 0, 0, 0, 0, 0];
    const ONE: [u8; 8] = [0x80, 0, 0, 0, 0, 0, 0, 1];

    let part = fold(&part_key(-1));
    let partsupp = fold(&partsupp_key(-1, 0));
    let line = fold(&line_key(-1, 0, 1, 2));

    let expected_part = [&[1][..], &MINUS_ONE, &[0, 1]].concat();
    let expected_partsupp = [&[1][..], &MINUS_ONE, &[2], &ZERO, &[0, 2]].concat();
    let expected_line = [
        &[1][..],
        &MINUS_ONE,
        &[2],
        &ZERO,
        &[3],
        &ONE,
        &[4, 0x80, 0, 0, 2],
        &[0, 3],
    ]
    .concat();
    assert_eq!(part.as_bytes(), expected_part);
    assert_eq!(partsupp.as_bytes(), expected_partsupp);
    assert_eq!(line.as_bytes(), expected_line);
    assert_eq!(
        [
            part.as_bytes().len(),
            partsupp.as_bytes().len(),
            line.as_bytes().len()
        ],
        [11, 20, 34]
    );

    for key in [
        part_key(i64::MIN),
        partsupp_key(i64::MAX, i64::MIN),
        line_key(i64::MIN, i64::MAX, i64::MIN, i32::MAX),
    ] {
        assert_eq!(unfold(fold(&key).as_bytes()), Ok(key));
    }
}

/// Within one part, the record order is the trie's pre-order: the part, then
/// each supply followed by its lines, in supplier order.
#[test]
fn record_order_is_the_tries_pre_order() {
    let in_record_order = [
        part_key(7),
        partsupp_key(7, -3),
        line_key(7, -3, 1, 1),
        line_key(7, -3, 1, 2),
        line_key(7, -3, 2, 1),
        partsupp_key(7, 4),
        line_key(7, 4, i64::MIN, 1),
        part_key(8),
    ];
    let folded: Vec<_> = in_record_order.iter().map(fold).collect();
    assert!(
        folded.windows(2).all(|pair| pair[0] < pair[1]),
        "{folded:?}"
    );
}

#[test]
fn malformed_folded_keys_are_rejected() {
    assert!(unfold(&[]).is_err());
    for key in [part_key(1), partsupp_key(1, 2), line_key(1, 2, 3, 4)] {
        let bytes = fold(&key).as_bytes().to_vec();
        assert!(unfold(&bytes[..bytes.len() - 1]).is_err());
        let mut extra = bytes.clone();
        extra.push(0);
        assert!(unfold(&extra).is_err());
        let mut bad_terminator = bytes.clone();
        bad_terminator[bytes.len() - 2] = 0xff;
        assert!(unfold(&bad_terminator).is_err());
        let mut bad_id = bytes.clone();
        *bad_id.last_mut().unwrap() = 0xff;
        assert!(unfold(&bad_id).is_err());
        // The domain tags sit at 0, 9, 18, and 27.
        for offset in [0, 9, 18, 27]
            .into_iter()
            .filter(|offset| *offset < bytes.len() - 2)
        {
            let mut bad_domain = bytes.clone();
            bad_domain[offset] = 0xff;
            assert!(unfold(&bad_domain).is_err());
        }
    }
    // A key of one placement with another placement's id is rejected.
    let mut part_as_line = fold(&part_key(1)).as_bytes().to_vec();
    *part_as_line.last_mut().unwrap() = INDEX_ID_LINEITEM_BY_PARTKEY_SUPPKEY_ORDERKEY_LINENUMBER;
    assert!(unfold(&part_as_line).is_err());
}

// The trie and the generated constants.

fn column(relation: &'static str, column: &'static str) -> ColumnRef {
    ColumnRef { relation, column }
}

#[test]
fn trie_derives_the_spec() {
    assert_eq!(
        ["partkey", "suppkey", "orderkey", "linenumber"].map(|domain| TRIE.domain_tag(domain)),
        [Some(1), Some(2), Some(3), Some(4)]
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
            "PartByPartkey",
            "PartsuppByPartkeySuppkey",
            "LineitemByPartkeySuppkeyOrderkeyLinenumber"
        ]
    );
    assert_eq!(
        placements
            .iter()
            .map(PlacedIndex::is_primary)
            .collect::<Vec<_>>(),
        [true, true, false]
    );
    assert_eq!(
        placements[0].key_columns(&TRIE),
        [column("Part", "p_partkey")]
    );
    assert_eq!(
        placements[1].key_columns(&TRIE),
        [
            column("Partsupp", "ps_partkey"),
            column("Partsupp", "ps_suppkey")
        ]
    );
    // Lineitem's own columns resolve every path domain; there is no `via`.
    assert!(placements[2].placement.via.is_empty());
    assert_eq!(
        placements[2].key_columns(&TRIE),
        [
            column("Lineitem", "l_partkey"),
            column("Lineitem", "l_suppkey"),
            column("Lineitem", "l_orderkey"),
            column("Lineitem", "l_linenumber")
        ]
    );
}

#[test]
fn domain_and_prefix_widths_follow_the_key_types() {
    let widths = TRIE.domains.iter().map(|domain| domain.width);
    // partkey, suppkey, and orderkey are i64; linenumber is i32.
    assert_eq!(widths.collect::<Vec<_>>(), [8, 8, 8, 4]);
    assert_eq!(TRIE.root_width(), 8);

    // A group is one part: a tag and an i64.
    let line = INDEX_ID_LINEITEM_BY_PARTKEY_SUPPKEY_ORDERKEY_LINENUMBER;
    assert_eq!(TRIE.prefix_width(line, 1), Ok(9));
    assert_eq!(TRIE.prefix_width(line, 2), Ok(18));
    assert_eq!(TRIE.prefix_width(line, 4), Ok(32));
    assert!(TRIE.prefix_width(line, 5).is_err());
    assert_eq!(
        TRIE.prefix_width(INDEX_ID_PARTSUPP_BY_PARTKEY_SUPPKEY, 2),
        Ok(18)
    );
}

#[test]
fn tags_and_ids_agree_with_the_trie_at_the_extremes() {
    assert_eq!(
        [TAG_PARTKEY, TAG_SUPPKEY, TAG_ORDERKEY, TAG_LINENUMBER].map(Some),
        ["partkey", "suppkey", "orderkey", "linenumber"].map(|domain| TRIE.domain_tag(domain))
    );
    assert_eq!(
        [
            INDEX_ID_PART_BY_PARTKEY,
            INDEX_ID_PARTSUPP_BY_PARTKEY_SUPPKEY,
            INDEX_ID_LINEITEM_BY_PARTKEY_SUPPKEY_ORDERKEY_LINENUMBER
        ],
        TRIE.placements()
            .iter()
            .map(|placed| placed.id)
            .collect::<Vec<_>>()[..]
    );
    assert_eq!(PartPartsuppLineitemDefinition::TRIE, TRIE);
    check_against_trie::<PartPartsuppLineitemDefinition>([
        part_key(i64::MIN),
        part_key(i64::MAX),
        partsupp_key(i64::MIN, i64::MAX),
        partsupp_key(i64::MAX, i64::MIN),
        line_key(i64::MIN, i64::MAX, i64::MIN, i32::MIN),
        line_key(i64::MAX, i64::MIN, i64::MAX, i32::MAX),
        line_key(0, 0, 0, 0),
    ]);
}

// Prefixes.

#[test]
fn prefixes_nest_and_end_at_the_expected_tags() {
    let part = prefix_partkey(12);
    let supply = prefix_partkey_suppkey(12, -4);
    let order = prefix_partkey_suppkey_orderkey(12, -4, 99);
    let lines = prefix_partkey_suppkey_orderkey_then_linenumber(12, -4, 99);
    assert!(supply.starts_with(&part));
    assert!(order.starts_with(&supply));
    assert!(lines.starts_with(&order));
    assert_eq!(prefix_partkey_then_suppkey(12).last(), Some(&TAG_SUPPKEY));
    assert_eq!(
        prefix_partkey_suppkey_then_orderkey(12, -4).last(),
        Some(&TAG_ORDERKEY)
    );
    assert_eq!(lines.last(), Some(&TAG_LINENUMBER));
    assert_eq!(
        fold(&line_key(12, -4, 99, 7)).as_bytes()[..lines.len()],
        lines
    );
}

proptest! {
    #[test]
    fn folding_round_trips_and_agrees_with_the_trie(
        part: i64,
        supplier: i64,
        order: i64,
        line: i32,
    ) {
        check_against_trie::<PartPartsuppLineitemDefinition>([
            part_key(part),
            partsupp_key(part, supplier),
            line_key(part, supplier, order, line),
        ]);
    }

    /// Within one placement, byte order is the order of the key fields.
    #[test]
    fn folded_lineitem_order_is_field_order(
        left: (i64, i64, i64, i32),
        right: (i64, i64, i64, i32),
    ) {
        let fold = |(p, s, o, l): (i64, i64, i64, i32)| fold(&line_key(p, s, o, l));
        prop_assert_eq!(fold(left).cmp(&fold(right)), left.cmp(&right));
    }

    /// A supply's lines sort after the supply and before the next supplier's
    /// supply, whatever their order and line numbers.
    #[test]
    fn lines_sort_between_their_supply_and_the_next(
        part: i64,
        supplier in i64::MIN..i64::MAX,
        order: i64,
        line: i32,
    ) {
        let line = fold(&line_key(part, supplier, order, line));
        prop_assert!(fold(&part_key(part)) < fold(&partsupp_key(part, supplier)));
        prop_assert!(fold(&partsupp_key(part, supplier)) < line);
        prop_assert!(line < fold(&partsupp_key(part, supplier + 1)));
    }

    /// Every key of a part, supply, or order range starts with that range's
    /// prefix, and no key outside it does.
    #[test]
    fn prefixes_cover_exactly_their_ranges(
        part: i64,
        supplier: i64,
        order: i64,
        line: i32,
    ) {
        let part_prefix = prefix_partkey(part);
        let supply_prefix = prefix_partkey_suppkey(part, supplier);
        let lines_prefix = prefix_partkey_suppkey_then_orderkey(part, supplier);
        let supply = fold(&partsupp_key(part, supplier));
        let line = fold(&line_key(part, supplier, order, line));
        prop_assert!(fold(&part_key(part)).as_bytes().starts_with(&part_prefix));
        prop_assert!(supply.as_bytes().starts_with(&part_prefix));
        prop_assert!(supply.as_bytes().starts_with(&supply_prefix));
        prop_assert!(line.as_bytes().starts_with(&supply_prefix));
        prop_assert!(line.as_bytes().starts_with(&lines_prefix));
        prop_assert!(!supply.as_bytes().starts_with(&lines_prefix));
    }
}

// Payloads.

#[test]
fn packed_payloads_have_golden_bytes() {
    assert_eq!(
        encode_payload(&part_payload("PROMO BRUSHED")),
        b"\x0dPROMO BRUSHED"
    );
    assert_eq!(
        encode_payload(&partsupp_payload(-2)),
        [0xfe, 0xff, 0xff, 0xff]
    );
    // A date takes 4 bytes, the price 8 (precision 15), the discount 2
    // (precision 4); nothing is padded and nothing is tagged.
    assert_eq!(
        encode_payload(&lineitem_payload(2, 100, 5)),
        [2, 0, 0, 0, 100, 0, 0, 0, 0, 0, 0, 0, 5, 0]
    );
}

#[test]
fn packed_payloads_round_trip_with_their_keys() {
    for (key, payload) in extreme_rows() {
        let bytes = encode_payload(&payload);
        assert_eq!(decode_payload(&key, &bytes).as_ref(), Ok(&payload));
        let id = *fold(&key).as_bytes().last().unwrap();
        assert_eq!(
            PartPartsuppLineitemDefinition::decode_payload(id, &bytes),
            Ok(payload)
        );
    }
}

#[test]
fn payloads_must_match_their_keys() {
    let rows = extreme_rows();
    for (i, (key, _)) in rows.iter().enumerate() {
        for (j, (_, payload)) in rows.iter().enumerate() {
            let matches = i == j;
            assert_eq!(payload.matches(key), matches, "{key:?} with {payload:?}");
            assert_eq!(
                build_batch([(key.clone(), payload.clone(), 1)]).is_ok(),
                matches,
                "{key:?} with {payload:?}"
            );
            assert_eq!(
                decode_payload(key, &encode_payload(payload)).is_ok(),
                matches,
                "{key:?} with {payload:?}"
            );
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

    // Strings: a padded length, invalid UTF-8, and a length past the input.
    let part = part_key(1);
    assert!(decode_payload(&part, b"\x01A").is_ok());
    assert!(decode_payload(&part, b"\x81\x00A").is_err());
    assert!(decode_payload(&part, b"\x01\xff").is_err());
    assert!(decode_payload(&part, b"\x02A").is_err());
}

proptest! {
    #[test]
    fn packed_payloads_round_trip(
        part_type in "\\PC{0,40}",
        available: i32,
        ship_date: i32,
        price in -(10_i128.pow(15) - 1)..=(10_i128.pow(15) - 1),
        discount in -9_999_i128..=9_999,
    ) {
        for (key, payload) in [
            (part_key(1), part_payload(&part_type)),
            (partsupp_key(1, 2), partsupp_payload(available)),
            (line_key(1, 2, 3, 4), lineitem_payload(ship_date, price, discount)),
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
        for key in [part_key(1), partsupp_key(1, 2), line_key(1, 2, 3, 4)] {
            if let Ok(payload) = decode_payload(&key, &bytes) {
                prop_assert_eq!(encode_payload(&payload), bytes.clone());
            }
        }
    }
}

// Batches.

type Row = (Key, Payload, i64);
type BatchBuilder = Box<dyn Fn(Vec<Row>) -> Result<MergedIndexBatch, String>>;

/// The two ways to build a batch: DBSP's default filters, and the index's
/// own factories, whose files get a prefix filter over the part group.
fn batch_builders() -> [BatchBuilder; 2] {
    let factories = index_factories::<StoredPayload>(&TRIE);
    [
        Box::new(build_batch),
        Box::new(move |rows| build_batch_in(&factories, rows)),
    ]
}

/// A batch holds the three placements in record order and cancels a row
/// inserted and deleted in the same batch.
#[test]
fn batch_sorts_in_record_order_and_consolidates() {
    let line = |orderkey| (line_key(5, 1, orderkey, 1), lineitem_payload(0, 100, 5));
    let rows = vec![
        (line(20).0, line(20).1, 1),
        (part_key(5), part_payload("PROMO"), 1),
        (line(10).0, line(10).1, 2),
        (partsupp_key(5, 1), partsupp_payload(3), 1),
        (line(30).0, line(30).1, 1),
        (line(30).0, line(30).1, -1),
    ];
    for build in batch_builders() {
        let batch = build(rows.clone()).unwrap();
        assert_eq!(
            read_cursor(&mut batch_cursor::<StoredPayload>(&[batch])),
            [
                (part_key(5), part_payload("PROMO"), 1),
                (partsupp_key(5, 1), partsupp_payload(3), 1),
                (line(10).0, line(10).1, 2),
                (line(20).0, line(20).1, 1),
            ]
        );
    }
}

#[test]
fn batch_rejects_weight_overflow_and_mismatched_payload() {
    let row = |weight| (part_key(1), part_payload("A"), weight);
    for build in batch_builders() {
        assert!(build(vec![row(i64::MAX), row(1)]).is_err());
        assert!(build(vec![(part_key(1), partsupp_payload(1), 1)]).is_err());
    }
}
