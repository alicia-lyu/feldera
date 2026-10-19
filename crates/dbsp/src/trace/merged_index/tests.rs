use super::{batch::batch_cursor, testing::check_against_trie, *};
use crate::{
    DynZWeight, ZWeight,
    dynamic::{DowncastTrait, DynData, Erase},
    trace::{BatchLocation, BatchReader, Cursor},
};

const FIXTURE_DOMAINS: &[Domain] = &[
    Domain {
        name: "root",
        columns: &[ColumnRef {
            relation: "Fixture",
            column: "f_root",
        }],
    },
    Domain {
        name: "child",
        columns: &[ColumnRef {
            relation: "Fixture",
            column: "f_child",
        }],
    },
];

/// A schema-independent fixture: two domains, one placement, mixed field widths.
const FIXTURE_TRIE: KeyTrie = KeyTrie {
    name: "Fixture",
    domains: FIXTURE_DOMAINS,
    children: &[Node {
        domain: "root",
        placements: &[],
        children: &[Node {
            domain: "child",
            placements: &[Placement {
                relation: "Fixture",
                via: &[],
                payload: Payload::All,
            }],
            children: &[],
        }],
    }],
};

struct Definition;

impl MergedIndexDefinition for Definition {
    type Key = (i64, i32);

    const TRIE: KeyTrie = FIXTURE_TRIE;

    fn fold(&(root, child): &Self::Key) -> FoldedKey {
        KeyEncoder::new().field(1, &root).field(2, &child).finish(1)
    }

    fn unfold(bytes: &[u8]) -> Result<Self::Key, String> {
        let (id, mut decoder) = KeyDecoder::new(bytes)?;
        if id != 1 {
            return Err(format!("unknown placement id {id}"));
        }
        let key = (decoder.field(1)?, decoder.field(2)?);
        decoder.finish()?;
        Ok(key)
    }
}

#[test]
fn mixed_width_folding_writes_tags_values_terminator_and_id() {
    let key = Definition::fold(&(-1, 1));
    assert_eq!(
        key.as_bytes(),
        &[
            1, 0x7f, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 2, 0x80, 0, 0, 1, 0, 1
        ]
    );
    assert_eq!(Definition::unfold(key.as_bytes()), Ok((-1, 1)));
    assert_eq!(
        KeyEncoder::new().field(1, &(-1i64)).prefix(),
        key.as_bytes()[..9]
    );
    assert_eq!(
        KeyEncoder::new().field(1, &(-1i64)).then_tag(2),
        key.as_bytes()[..10]
    );
    assert_eq!(
        KeyEncoder::new()
            .field(1, &(-1i64))
            .field(2, &1i32)
            .prefix(),
        key.as_bytes()[..14]
    );
    assert!(KeyEncoder::new().prefix().is_empty());
    assert_eq!(KeyEncoder::new().then_tag(1), [1]);
}

#[test]
fn signed_boundaries_round_trip_and_sort() {
    let roots = [
        i64::MIN,
        i64::from(i32::MIN) - 1,
        -1,
        0,
        1,
        i64::from(i32::MAX) + 1,
        i64::MAX,
    ];
    let children = [i32::MIN, -1, 0, 1, i32::MAX];
    let mut previous = None;
    for root in roots {
        for child in children {
            let logical = (root, child);
            let key = Definition::fold(&logical);
            assert_eq!(Definition::unfold(key.as_bytes()), Ok(logical));
            if let Some(previous) = previous {
                assert!(previous < key);
            }
            previous = Some(key);
        }
    }
}

#[test]
fn malformed_keys_are_rejected_without_panics() {
    let bytes = Definition::fold(&(i64::MAX, i32::MIN)).as_bytes().to_vec();
    for length in 0..bytes.len() {
        assert!(Definition::unfold(&bytes[..length]).is_err());
    }
    let mut extra = bytes.clone();
    extra.push(0);
    assert!(Definition::unfold(&extra).is_err());
    // The first tag, the second tag, the terminator, and the placement id.
    for position in [0, 9, 14, 15] {
        let mut corrupt = bytes.clone();
        corrupt[position] = 0xff;
        assert!(Definition::unfold(&corrupt).is_err());
    }
}

#[test]
fn decoder_reports_each_kind_of_malformed_key() {
    assert!(KeyDecoder::new(&[]).is_err());
    assert!(KeyDecoder::new(&[1]).is_err());
    assert!(KeyDecoder::new(&[1, 1]).is_err(), "terminator must be 0");

    let (id, mut decoder) = KeyDecoder::new(&[1, 0x80, 0, 0, 0, 0, 1]).unwrap();
    assert_eq!(id, 1);
    assert!(decoder.field::<i32>(2).is_err(), "wrong tag");
    assert!(decoder.field::<i64>(1).is_err(), "truncated field");
    assert_eq!(decoder.field::<i32>(1), Ok(0));
    assert!(decoder.field::<i32>(2).is_err(), "no field left");
    assert!(decoder.finish().is_ok());

    let (_, decoder) = KeyDecoder::new(&[1, 0x80, 0, 0, 0, 0, 1]).unwrap();
    assert!(decoder.finish().is_err(), "unread bytes");

    let (_, mut decoder) = KeyDecoder::new(&[1, 2, 0, 1]).unwrap();
    assert!(decoder.field::<bool>(1).is_err(), "non-canonical bool");
}

#[test]
fn fixture_agrees_with_its_trie() {
    Definition::TRIE.validate().unwrap();
    check_against_trie::<Definition>([(i64::MIN, i32::MIN), (0, 0), (i64::MAX, i32::MAX)]);
}

/// Folds with the wrong placement id.
struct WrongId;

impl MergedIndexDefinition for WrongId {
    type Key = (i64, i32);
    const TRIE: KeyTrie = FIXTURE_TRIE;
    fn fold(&(root, child): &Self::Key) -> FoldedKey {
        KeyEncoder::new().field(1, &root).field(2, &child).finish(2)
    }
    fn unfold(bytes: &[u8]) -> Result<Self::Key, String> {
        let (_, mut decoder) = KeyDecoder::new(bytes)?;
        let key = (decoder.field(1)?, decoder.field(2)?);
        decoder.finish()?;
        Ok(key)
    }
}

/// Folds with swapped tags, so the tags no longer match the trie's path.
struct WrongTags;

impl MergedIndexDefinition for WrongTags {
    type Key = (i64, i32);
    const TRIE: KeyTrie = FIXTURE_TRIE;
    fn fold(&(root, child): &Self::Key) -> FoldedKey {
        KeyEncoder::new().field(2, &root).field(1, &child).finish(1)
    }
    fn unfold(bytes: &[u8]) -> Result<Self::Key, String> {
        let (_, mut decoder) = KeyDecoder::new(bytes)?;
        let key = (decoder.field(2)?, decoder.field(1)?);
        decoder.finish()?;
        Ok(key)
    }
}

#[test]
#[should_panic(expected = "absent from the trie")]
fn check_against_trie_rejects_a_placement_id_outside_the_trie() {
    check_against_trie::<WrongId>([(1, 2)]);
}

#[test]
#[should_panic(expected = "does not carry the tags")]
fn check_against_trie_rejects_tags_that_differ_from_the_trie() {
    check_against_trie::<WrongTags>([(1, 2)]);
}

// The trie of COL, as the design doc's spec writes it.
const COL_TRIE: KeyTrie = KeyTrie {
    name: "CustomerOrdersLineitem",
    domains: &[
        Domain {
            name: "custkey",
            columns: &[
                ColumnRef {
                    relation: "Customer",
                    column: "c_custkey",
                },
                ColumnRef {
                    relation: "Orders",
                    column: "o_custkey",
                },
            ],
        },
        Domain {
            name: "orderkey",
            columns: &[
                ColumnRef {
                    relation: "Orders",
                    column: "o_orderkey",
                },
                ColumnRef {
                    relation: "Lineitem",
                    column: "l_orderkey",
                },
            ],
        },
        Domain {
            name: "linenumber",
            columns: &[ColumnRef {
                relation: "Lineitem",
                column: "l_linenumber",
            }],
        },
    ],
    children: &[Node {
        domain: "custkey",
        placements: &[Placement {
            relation: "Customer",
            via: &[],
            payload: Payload::All,
        }],
        children: &[Node {
            domain: "orderkey",
            placements: &[Placement {
                relation: "Orders",
                via: &[],
                payload: Payload::Columns(&["o_orderdate", "o_shippriority"]),
            }],
            children: &[Node {
                domain: "linenumber",
                placements: &[Placement {
                    relation: "Lineitem",
                    via: &[Via {
                        domain: "custkey",
                        relation: "Orders",
                    }],
                    payload: Payload::Columns(&["l_shipdate", "l_extendedprice", "l_discount"]),
                }],
                children: &[],
            }],
        }],
    }],
};

fn column(relation: &'static str, column: &'static str) -> ColumnRef {
    ColumnRef { relation, column }
}

#[test]
fn col_trie_derives_tags_ids_names_and_key_columns() {
    COL_TRIE.validate().unwrap();
    assert_eq!(COL_TRIE.domain_tag("custkey"), Some(1));
    assert_eq!(COL_TRIE.domain_tag("orderkey"), Some(2));
    assert_eq!(COL_TRIE.domain_tag("linenumber"), Some(3));
    assert_eq!(COL_TRIE.domain_tag("unknown"), None);

    let placements = COL_TRIE.placements();
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
    assert_eq!(placements[2].path, ["custkey", "orderkey", "linenumber"]);
    assert_eq!(
        placements
            .iter()
            .map(PlacedIndex::is_primary)
            .collect::<Vec<_>>(),
        [true, false, false]
    );
    assert_eq!(
        placements[0].key_columns(&COL_TRIE),
        [column("Customer", "c_custkey")]
    );
    assert_eq!(
        placements[1].key_columns(&COL_TRIE),
        [
            column("Orders", "o_custkey"),
            column("Orders", "o_orderkey")
        ]
    );
    // `custkey via Orders` takes the customer from `Orders.o_custkey`.
    assert_eq!(
        placements[2].key_columns(&COL_TRIE),
        [
            column("Orders", "o_custkey"),
            column("Lineitem", "l_orderkey"),
            column("Lineitem", "l_linenumber")
        ]
    );
    assert_eq!(
        COL_TRIE.placement("OrdersByCustkeyOrderkey"),
        Some(placements[1].clone())
    );
    assert_eq!(COL_TRIE.placement("OrdersByCustkey"), None);
}

#[test]
fn tags_follow_first_appearance_and_ids_follow_visit_order() {
    const SIBLINGS: KeyTrie = KeyTrie {
        name: "Siblings",
        domains: &[
            Domain {
                name: "c",
                columns: &[ColumnRef {
                    relation: "R",
                    column: "r_c",
                }],
            },
            Domain {
                name: "a",
                columns: &[ColumnRef {
                    relation: "R",
                    column: "r_a",
                }],
            },
            Domain {
                name: "b",
                columns: &[ColumnRef {
                    relation: "R",
                    column: "r_b",
                }],
            },
        ],
        // a { c { R } }  b { c { R } }  in written order.
        children: &[
            Node {
                domain: "a",
                placements: &[],
                children: &[Node {
                    domain: "c",
                    placements: &[Placement {
                        relation: "R",
                        via: &[],
                        payload: Payload::All,
                    }],
                    children: &[],
                }],
            },
            Node {
                domain: "b",
                placements: &[],
                children: &[Node {
                    domain: "c",
                    placements: &[Placement {
                        relation: "R",
                        via: &[],
                        payload: Payload::All,
                    }],
                    children: &[],
                }],
            },
        ],
    };
    // Tags: a = 1, c = 2, b = 3. A repeated domain keeps its first tag.
    assert_eq!(SIBLINGS.domain_tag("a"), Some(1));
    assert_eq!(SIBLINGS.domain_tag("c"), Some(2));
    assert_eq!(SIBLINGS.domain_tag("b"), Some(3));
    let placements = SIBLINGS.placements();
    assert_eq!(placements[0].path, ["a", "c"]);
    assert_eq!(placements[1].path, ["b", "c"]);
    assert_eq!(placements[1].id, 2);
    assert_eq!(placements[1].name(), "RByBC");
}

#[test]
fn names_are_camel_case_across_underscores() {
    let placed = PlacedIndex {
        id: 1,
        path: vec!["order_key", "line_number"],
        placement: &Placement {
            relation: "Lineitem",
            via: &[],
            payload: Payload::All,
        },
    };
    assert_eq!(placed.name(), "LineitemByOrderKeyLineNumber");
}

// One failing case per validation rule.

const EMPTY_PLACEMENT: Placement = Placement {
    relation: "Fixture",
    via: &[],
    payload: Payload::All,
};

fn fixture_trie(children: &'static [Node]) -> KeyTrie {
    KeyTrie {
        children,
        ..FIXTURE_TRIE
    }
}

fn assert_invalid(trie: KeyTrie, expected_message: &str) {
    let error = trie.validate().unwrap_err();
    assert!(
        error.contains(expected_message),
        "`{error}` should contain `{expected_message}`"
    );
}

#[test]
fn validate_rejects_a_node_domain_that_is_not_declared() {
    assert_invalid(
        fixture_trie(&[Node {
            domain: "undeclared",
            placements: &[],
            children: &[],
        }]),
        "`undeclared` is not declared",
    );
}

#[test]
fn validate_rejects_a_via_domain_that_is_not_declared() {
    assert_invalid(
        fixture_trie(&[Node {
            domain: "root",
            placements: &[Placement {
                relation: "Fixture",
                via: &[Via {
                    domain: "undeclared",
                    relation: "Other",
                }],
                payload: Payload::All,
            }],
            children: &[],
        }]),
        "via domain `undeclared` is not declared",
    );
}

#[test]
fn validate_rejects_a_domain_declared_twice() {
    const TWICE: &[Domain] = &[FIXTURE_DOMAINS[0], FIXTURE_DOMAINS[0]];
    assert_invalid(
        KeyTrie {
            domains: TWICE,
            ..FIXTURE_TRIE
        },
        "declared twice",
    );
}

#[test]
fn validate_rejects_sibling_nodes_with_the_same_domain() {
    assert_invalid(
        fixture_trie(&[
            Node {
                domain: "root",
                placements: &[EMPTY_PLACEMENT],
                children: &[],
            },
            Node {
                domain: "root",
                placements: &[],
                children: &[],
            },
        ]),
        "sibling nodes repeat domain `root`",
    );
}

#[test]
fn validate_rejects_a_domain_that_repeats_on_a_path() {
    assert_invalid(
        fixture_trie(&[Node {
            domain: "root",
            placements: &[],
            children: &[Node {
                domain: "root",
                placements: &[],
                children: &[],
            }],
        }]),
        "repeats on the path",
    );
}

#[test]
fn validate_rejects_a_relation_placed_twice_at_a_node() {
    assert_invalid(
        fixture_trie(&[Node {
            domain: "root",
            placements: &[EMPTY_PLACEMENT, EMPTY_PLACEMENT],
            children: &[],
        }]),
        "placed twice",
    );
}

#[test]
fn validate_rejects_more_than_255_domains() {
    let domains: Vec<Domain> = (0..256)
        .map(|position| Domain {
            name: Box::leak(format!("d{position}").into_boxed_str()),
            columns: &[],
        })
        .collect();
    assert_invalid(
        KeyTrie {
            domains: Box::leak(domains.into_boxed_slice()),
            ..FIXTURE_TRIE
        },
        "more than 255 domains",
    );
}

#[test]
fn validate_rejects_more_than_255_placements() {
    let placements: Vec<Placement> = (0..256).map(|_| EMPTY_PLACEMENT).collect();
    let nodes = vec![Node {
        domain: "root",
        placements: Box::leak(placements.into_boxed_slice()),
        children: &[],
    }];
    assert_invalid(
        fixture_trie(Box::leak(nodes.into_boxed_slice())),
        "more than 255 placements",
    );
}

#[test]
fn validate_rejects_a_path_domain_that_does_not_resolve() {
    // `Other` has no column in `root` and no via for it.
    assert_invalid(
        fixture_trie(&[Node {
            domain: "root",
            placements: &[Placement {
                relation: "Other",
                via: &[],
                payload: Payload::All,
            }],
            children: &[],
        }]),
        "no column in domain `root` and no via",
    );
}

#[test]
fn validate_rejects_a_via_relation_without_a_column_in_the_domain() {
    assert_invalid(
        fixture_trie(&[Node {
            domain: "root",
            placements: &[Placement {
                relation: "Other",
                via: &[Via {
                    domain: "root",
                    relation: "Missing",
                }],
                payload: Payload::All,
            }],
            children: &[],
        }]),
        "via relation `Missing` has no column",
    );
}

#[test]
fn validate_rejects_a_via_for_a_domain_the_relation_already_has() {
    assert_invalid(
        fixture_trie(&[Node {
            domain: "root",
            placements: &[Placement {
                relation: "Fixture",
                via: &[Via {
                    domain: "root",
                    relation: "Fixture",
                }],
                payload: Payload::All,
            }],
            children: &[],
        }]),
        "own column in domain `root` and also a via",
    );
}

#[test]
fn validate_rejects_a_via_domain_off_the_path() {
    assert_invalid(
        fixture_trie(&[Node {
            domain: "root",
            placements: &[Placement {
                relation: "Fixture",
                via: &[Via {
                    domain: "child",
                    relation: "Fixture",
                }],
                payload: Payload::All,
            }],
            children: &[],
        }]),
        "is not on the path",
    );
}

#[test]
fn validate_rejects_two_vias_for_one_domain() {
    assert_invalid(
        fixture_trie(&[Node {
            domain: "root",
            placements: &[Placement {
                relation: "Other",
                via: &[
                    Via {
                        domain: "root",
                        relation: "Fixture",
                    },
                    Via {
                        domain: "root",
                        relation: "Fixture",
                    },
                ],
                payload: Payload::All,
            }],
            children: &[],
        }]),
        "two vias",
    );
}

#[test]
fn validate_rejects_two_columns_of_one_relation_in_a_domain() {
    const TWO_COLUMNS: &[Domain] = &[Domain {
        name: "root",
        columns: &[
            ColumnRef {
                relation: "Fixture",
                column: "f_a",
            },
            ColumnRef {
                relation: "Fixture",
                column: "f_b",
            },
        ],
    }];
    assert_invalid(
        KeyTrie {
            domains: TWO_COLUMNS,
            children: &[Node {
                domain: "root",
                placements: &[EMPTY_PLACEMENT],
                children: &[],
            }],
            ..FIXTURE_TRIE
        },
        "two columns in domain `root`",
    );
}

/// A stored row: folded key bytes, typed payload, weight.
type Row = (Vec<u8>, u8, ZWeight);

fn row(root: i64, payload: u8, weight: ZWeight) -> (FoldedKey, u8, ZWeight) {
    (Definition::fold(&(root, 0)), payload, weight)
}

fn read_cursor(cursor: &mut impl Cursor<DynData, DynData, (), DynZWeight>) -> Vec<Row> {
    let mut rows = Vec::new();
    while cursor.key_valid() {
        while cursor.val_valid() {
            rows.push((
                cursor.key().downcast_checked::<Vec<u8>>().clone(),
                *cursor.val().downcast_checked::<u8>(),
                *cursor.weight().downcast_checked::<ZWeight>(),
            ));
            cursor.step_val();
        }
        cursor.step_key();
    }
    rows
}

fn bytes_row(root: i64, payload: u8, weight: ZWeight) -> Row {
    let (key, payload, weight) = row(root, payload, weight);
    (key.as_bytes().to_vec(), payload, weight)
}

#[test]
fn memory_batch_sorts_and_consolidates_signed_pairs() {
    let batch = batch::build_batch([
        row(2, 2, 1),
        row(1, 1, 3),
        row(2, 2, -2),
        row(1, 1, -1),
        row(3, 3, 1),
        row(3, 3, -1),
        row(2, 1, 4),
        row(4, 4, 0),
    ])
    .unwrap();
    assert_eq!(batch.location(), BatchLocation::Memory);
    assert_eq!(batch.key_count(), 2);
    assert_eq!(batch.len(), 3);
    assert_eq!(
        read_cursor(&mut batch.cursor()),
        vec![bytes_row(1, 1, 2), bytes_row(2, 1, 4), bytes_row(2, 2, -1)]
    );
}

#[test]
fn batch_rejects_signed_weight_overflow() {
    assert!(batch::build_batch([row(0, 0, i64::MAX), row(0, 0, 1)]).is_err());
    assert!(batch::build_batch([row(0, 0, i64::MIN), row(0, 0, -1)]).is_err());
}

#[test]
fn consolidated_cursor_preserves_batches_and_handles_replacements() {
    let batches = [
        batch::build_batch([row(1, 1, 2), row(2, 2, 1), row(3, 3, 1), row(5, 5, -3)]).unwrap(),
        batch::build_batch([row(1, 1, 3), row(2, 2, -1), row(3, 3, -1), row(3, 4, 1)]).unwrap(),
        batch::build_batch([row(1, 1, -1)]).unwrap(),
    ];
    let stored_before: Vec<_> = batches
        .iter()
        .map(|batch| read_cursor(&mut batch.cursor()))
        .collect();
    let expected = vec![bytes_row(1, 1, 4), bytes_row(3, 4, 1), bytes_row(5, 5, -3)];
    let mut cursor = batch_cursor::<u8>(&batches);
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
fn consolidated_cursor_seeks_and_stops_at_prefix_boundary() {
    let batches = [
        batch::build_batch([row(1, 1, 1), row(2, 2, 1), row(3, 3, 1)]).unwrap(),
        batch::build_batch([row(1, 1, 2), row(2, 2, -1)]).unwrap(),
    ];
    let mut cursor = batch_cursor::<u8>(&batches);
    let first = bytes_row(1, 1, 3);
    assert!(cursor.seek_key_exact(first.0.erase(), None));
    assert_eq!(*cursor.weight().downcast_checked::<ZWeight>(), 3);
    // An exact lookup does not establish a forward key traversal.
    cursor.rewind_keys();
    cursor.seek_key(first.0.erase());
    assert_eq!(
        read_cursor(&mut cursor),
        vec![first.clone(), bytes_row(3, 3, 1)]
    );
    cursor.rewind_keys();
    let cancelled = bytes_row(2, 2, 0);
    assert!(!cursor.seek_key_exact(cancelled.0.erase(), None));
    cursor.rewind_keys();
    cursor.seek_key(cancelled.0.erase());
    assert_eq!(read_cursor(&mut cursor), vec![bytes_row(3, 3, 1)]);
    cursor.rewind_keys();
    let prefix = KeyEncoder::new().field(1, &1i64).prefix();
    cursor.seek_key(prefix.erase());
    assert!(
        cursor
            .key()
            .downcast_checked::<Vec<u8>>()
            .starts_with(&prefix)
    );
    cursor.step_key();
    assert!(
        !cursor
            .key()
            .downcast_checked::<Vec<u8>>()
            .starts_with(&prefix)
    );
    let past_end = bytes_row(4, 4, 0);
    cursor.seek_key(past_end.0.erase());
    assert!(!cursor.key_valid());
}

#[test]
fn empty_and_cancelled_batches_have_no_cursor_rows() {
    assert!(!batch_cursor::<u8>(&[]).key_valid());
    let batches = [
        batch::build_batch::<u8>([]).unwrap(),
        batch::build_batch([row(0, 0, 1), row(0, 0, -1)]).unwrap(),
    ];
    assert!(batches.iter().all(|batch| batch.len() == 0));
    assert!(!batch_cursor::<u8>(&batches).key_valid());
    let cancelled = [
        batch::build_batch([row(0, 0, 1)]).unwrap(),
        batch::build_batch([row(0, 0, -1)]).unwrap(),
    ];
    assert!(!batch_cursor::<u8>(&cancelled).key_valid());
}
