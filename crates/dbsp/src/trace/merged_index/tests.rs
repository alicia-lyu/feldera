use super::{batch::batch_cursor, *};
use crate::{
    DynZWeight, ZWeight,
    dynamic::{DowncastTrait, DynData, Erase},
    trace::{BatchLocation, BatchReader, Cursor},
};

const ROOT: KeyDomainId = KeyDomainId("root");
const CHILD: KeyDomainId = KeyDomainId("child");
const SOURCE: SourceIndexId = SourceIndexId("fixture");

/// A schema-independent fixture with deliberately mixed field widths and tags.
struct Definition;

impl MergedIndexDefinition for Definition {
    type Key = (i64, i64);

    const KEY_DOMAINS: &'static [KeyDomain] = &[
        KeyDomain {
            id: ROOT,
            primitive: KeyPrimitive::I64,
        },
        KeyDomain {
            id: CHILD,
            primitive: KeyPrimitive::I32,
        },
    ];
    const SOURCE_INDEX_SPECS: &'static [SourceIndexSpec] = &[SourceIndexSpec {
        id: SOURCE,
        base_relation: BaseRelationId("fixture"),
        key_fields: &[
            KeyField {
                name: "root",
                domain: ROOT,
            },
            KeyField {
                name: "child",
                domain: CHILD,
            },
        ],
        payload_fields: &["opaque"],
    }];
    const DOMAIN_BYTE_TAGS: &'static [DomainByteTag] = &[
        DomainByteTag {
            domain: ROOT,
            tag: 9,
        },
        DomainByteTag {
            domain: CHILD,
            tag: 4,
        },
    ];
    const SOURCE_BYTE_TAGS: &'static [SourceByteTag] = &[SourceByteTag {
        source_index: SOURCE,
        tag: 8,
    }];

    fn project(&(root, child): &Self::Key) -> (SourceIndexId, Vec<i64>) {
        (SOURCE, vec![root, child])
    }

    fn construct(source: SourceIndexId, fields: &[i64]) -> Option<Self::Key> {
        match (source, fields) {
            (SOURCE, [root, child]) => Some((*root, *child)),
            _ => None,
        }
    }
}

#[test]
fn mixed_width_folding_uses_definition_metadata() {
    let index = MergedIndex::<Definition>::default();
    let key = index.fold(&(-1, 1)).unwrap();
    assert_eq!(
        key.as_bytes(),
        &[
            9, 0x7f, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 4, 0x80, 0, 0, 1, 0, 8
        ]
    );
    assert_eq!(index.unfold(key.as_bytes()), Ok((-1, 1)));
    assert_eq!(index.prefix(SOURCE, &[-1]).unwrap(), key.as_bytes()[..9]);
    assert_eq!(
        index.prefix_with_next_tag(SOURCE, &[-1]).unwrap(),
        key.as_bytes()[..10]
    );
    assert_eq!(
        index.prefix(SOURCE, &[-1, 1]).unwrap(),
        key.as_bytes()[..14]
    );
    assert!(index.prefix(SOURCE, &[]).unwrap().is_empty());
    assert_eq!(index.prefix_with_next_tag(SOURCE, &[]).unwrap(), [9]);
    assert!(index.prefix(SOURCE, &[1, 2, 3]).is_err());
    assert!(index.prefix_with_next_tag(SOURCE, &[1, 2]).is_err());
    assert!(index.prefix(SourceIndexId("unknown"), &[]).is_err());
}

#[test]
fn signed_boundaries_round_trip_and_sort_in_both_domains() {
    let index = MergedIndex::<Definition>::default();
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
            let logical = (root, i64::from(child));
            let key = index.fold(&logical).unwrap();
            assert_eq!(index.unfold(key.as_bytes()), Ok(logical));
            if let Some(previous) = previous {
                assert!(previous < key);
            }
            previous = Some(key);
        }
    }
}

#[test]
fn i32_fields_reject_out_of_range_projection_and_prefix_values() {
    let index = MergedIndex::<Definition>::default();
    for value in [
        i64::MIN,
        i64::from(i32::MIN) - 1,
        i64::from(i32::MAX) + 1,
        i64::MAX,
    ] {
        assert!(index.fold(&(0, value)).is_err());
        assert!(index.prefix(SOURCE, &[0, value]).is_err());
    }
}

#[test]
fn malformed_keys_are_rejected_without_panics() {
    let index = MergedIndex::<Definition>::default();
    let bytes = index
        .fold(&(i64::MAX, i64::from(i32::MIN)))
        .unwrap()
        .as_bytes()
        .to_vec();
    for length in 0..bytes.len() {
        assert!(index.unfold(&bytes[..length]).is_err());
    }
    let mut extra = bytes.clone();
    extra.push(0);
    assert!(index.unfold(&extra).is_err());
    for position in [0, 9, 14, 15] {
        let mut corrupt = bytes.clone();
        corrupt[position] = 0xff;
        assert!(index.unfold(&corrupt).is_err());
    }
}

struct DuplicateDomainTag;

impl MergedIndexDefinition for DuplicateDomainTag {
    type Key = <Definition as MergedIndexDefinition>::Key;
    const KEY_DOMAINS: &'static [KeyDomain] = Definition::KEY_DOMAINS;
    const SOURCE_INDEX_SPECS: &'static [SourceIndexSpec] = Definition::SOURCE_INDEX_SPECS;
    const DOMAIN_BYTE_TAGS: &'static [DomainByteTag] = &[
        DomainByteTag {
            domain: ROOT,
            tag: 9,
        },
        DomainByteTag {
            domain: CHILD,
            tag: 9,
        },
    ];
    const SOURCE_BYTE_TAGS: &'static [SourceByteTag] = Definition::SOURCE_BYTE_TAGS;
    fn project(key: &Self::Key) -> (SourceIndexId, Vec<i64>) {
        Definition::project(key)
    }
    fn construct(source: SourceIndexId, fields: &[i64]) -> Option<Self::Key> {
        Definition::construct(source, fields)
    }
}

#[test]
fn ambiguous_domain_tags_are_rejected() {
    let index = MergedIndex::<DuplicateDomainTag>::default();
    assert!(index.fold(&(0, 0)).is_err());
    let valid = MergedIndex::<Definition>::default().fold(&(0, 0)).unwrap();
    assert!(index.unfold(valid.as_bytes()).is_err());
}

type Row = (Vec<u8>, Vec<u8>, ZWeight);

fn row(root: i64, payload: u8, weight: ZWeight) -> (FoldedKey, PayloadBytes, ZWeight) {
    (
        MergedIndex::<Definition>::default()
            .fold(&(root, 0))
            .unwrap(),
        vec![payload].into(),
        weight,
    )
}

fn read_cursor(cursor: &mut impl Cursor<DynData, DynData, (), DynZWeight>) -> Vec<Row> {
    let mut rows = Vec::new();
    while cursor.key_valid() {
        while cursor.val_valid() {
            rows.push((
                cursor.key().downcast_checked::<Vec<u8>>().clone(),
                cursor.val().downcast_checked::<Vec<u8>>().clone(),
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
    (key.as_bytes().to_vec(), payload.as_bytes().to_vec(), weight)
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
    let mut cursor = batch_cursor(&batches);
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
    let mut cursor = batch_cursor(&batches);
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
    let prefix = MergedIndex::<Definition>::default()
        .prefix(SOURCE, &[1])
        .unwrap();
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
    assert!(!batch_cursor(&[]).key_valid());
    let batches = [
        batch::build_batch([]).unwrap(),
        batch::build_batch([row(0, 0, 1), row(0, 0, -1)]).unwrap(),
    ];
    assert!(batches.iter().all(|batch| batch.len() == 0));
    assert!(!batch_cursor(&batches).key_valid());
    let cancelled = [
        batch::build_batch([row(0, 0, 1)]).unwrap(),
        batch::build_batch([row(0, 0, -1)]).unwrap(),
    ];
    assert!(!batch_cursor(&cancelled).key_valid());
}
