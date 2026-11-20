//! Tests of the memtable's tree and leaves, against a model that is a
//! `BTreeMap` from `(key, value)` to weight.

use super::{FrozenMemtable, IndexedLeaf, MemLeaf, Memtable, TreeShape, WSetLeaf};
use crate::{
    DynZWeight, ZWeight,
    dynamic::{DowncastTrait, DynData, DynPair, DynUnit, DynWeightedPairs, Erase, LeanVec},
    trace::{
        Batch, BatchReader, BatchReaderFactories, VecIndexedWSet, VecIndexedWSetFactories, VecWSet,
        VecWSetFactories,
    },
    utils::Tup2,
};
use proptest::{collection::vec, prelude::*};
use std::collections::{BTreeMap, BTreeSet};

pub(super) type Leaf = IndexedLeaf<DynData, DynData, DynZWeight>;
pub(super) type Table = Memtable<Leaf>;
pub(super) type Model = BTreeMap<(u64, u64), ZWeight>;

/// Small nodes, so that a few dozen items make a tree several levels high:
/// a leaf holds about four items of 24 bytes plus an offset.
pub(super) const SMALL: TreeShape = TreeShape {
    leaf_bytes: 128,
    fanout: 3,
};

pub(super) fn factories() -> VecIndexedWSetFactories<DynData, DynData, DynZWeight> {
    VecIndexedWSetFactories::new::<u64, u64, ZWeight>()
}

pub(super) fn table() -> Table {
    Memtable::with_shape(&factories(), SMALL)
}

pub(super) fn insert(table: &mut Table, key: u64, val: u64, weight: ZWeight) {
    table.insert(key.erase(), val.erase(), weight.erase());
}

/// The model of `items`.
fn model_of(items: &[(Box<DynData>, Box<DynData>, Box<DynZWeight>)]) -> Model {
    items
        .iter()
        .map(|(key, val, weight)| {
            (
                (
                    *key.downcast_checked::<u64>(),
                    *val.downcast_checked::<u64>(),
                ),
                *weight.downcast_checked::<ZWeight>(),
            )
        })
        .collect()
}

fn apply_to_model(model: &mut Model, key: u64, val: u64, weight: ZWeight) {
    let sum = model.entry((key, val)).or_default();
    *sum += weight;
    if *sum == 0 {
        model.remove(&(key, val));
    }
}

/// Checks the tree's invariants and that it holds the model.
fn assert_holds(table: &Table, model: &Model) {
    assert_eq!(&model_of(&table.validate()), model);
}

#[test]
fn a_new_memtable_is_empty() {
    let table = table();
    assert_eq!(table.height(), 1);
    assert_eq!(table.totals().tuples, 0);
    assert!(table.freeze().key_bounds().is_none());
    assert!(table.validate().is_empty());
}

#[test]
fn inserts_in_every_order_make_a_tree_of_several_levels() {
    for order in 0..3 {
        let mut table = table();
        let mut model = Model::new();
        let mut keys: Vec<u64> = (0..200).collect();
        match order {
            0 => {}
            1 => keys.reverse(),
            _ => keys.sort_by_key(|key| key.wrapping_mul(2654435761) % 1009),
        }
        for key in keys {
            insert(&mut table, key, key * 10, 1);
            apply_to_model(&mut model, key, key * 10, 1);
        }
        assert_holds(&table, &model);
        assert!(table.height() >= 4, "height {}", table.height());
        assert_eq!(table.totals().tuples, 200);
        assert_eq!(table.totals().keys, 200);
    }
}

#[test]
fn a_cancelling_weight_removes_the_item_and_then_the_key() {
    let mut table = table();
    let mut model = Model::new();
    for (key, val, weight) in [(1, 1, 2), (1, 2, 3), (2, 1, -4), (3, 1, 5)] {
        insert(&mut table, key, val, weight);
        apply_to_model(&mut model, key, val, weight);
    }
    assert_eq!(table.totals().keys, 3);
    assert_eq!(table.totals().negative_weights, 1);

    // Adding to a weight changes it in place.
    insert(&mut table, 1, 1, 5);
    assert_eq!(model_of(&table.validate())[&(1, 1)], 7);

    // Cancelling one of key 1's values leaves the key; cancelling both removes it.
    insert(&mut table, 1, 1, -7);
    assert_eq!(table.totals().keys, 3);
    insert(&mut table, 1, 2, -3);
    assert_eq!(table.totals().keys, 2);
    // A weight that turns positive stops counting as negative.
    insert(&mut table, 2, 1, 10);
    assert_eq!(table.totals().negative_weights, 0);

    for (key, val, weight) in [(1, 1, 5), (1, 1, -7), (1, 2, -3), (2, 1, 10)] {
        apply_to_model(&mut model, key, val, weight);
    }
    model.remove(&(1, 1));
    assert_holds(&table, &model);

    // A zero weight changes nothing.
    insert(&mut table, 9, 9, 0);
    assert_holds(&table, &model);

    // Removing everything leaves an empty tree of one level.
    for (&(key, val), &weight) in &model.clone() {
        insert(&mut table, key, val, -weight);
    }
    assert_eq!(table.totals(), super::Totals::default());
    assert_eq!(table.height(), 1);
}

#[test]
fn the_values_of_one_key_split_across_leaves_and_count_the_key_once() {
    let mut table = table();
    let mut model = Model::new();
    // Interleave two keys' values from both ends, so that the splits fall inside keys.
    for val in 0..120 {
        for (key, v) in [(5, val), (5, 239 - val), (9, val)] {
            insert(&mut table, key, v, 1);
            apply_to_model(&mut model, key, v, 1);
        }
    }
    assert_holds(&table, &model);
    assert!(table.height() >= 3);
    assert_eq!(table.totals().keys, 2);
    assert_eq!(table.totals().tuples, 360);
    // The repeated key entries are in the bytes, so they exceed the minimum.
    assert!(table.totals().bytes > 360 * 16 + 2 * 8);

    // Cancel the values of key 5 one by one, from the middle outward: the
    // key is counted once until the last one goes.
    let mut middle: Vec<u64> = (0..240).collect();
    middle.sort_by_key(|val| val.abs_diff(120));
    for (index, val) in middle.iter().enumerate() {
        insert(&mut table, 5, *val, -1);
        apply_to_model(&mut model, 5, *val, -1);
        let expected_keys = if index + 1 < 240 { 2 } else { 1 };
        assert_eq!(table.totals().keys, expected_keys, "after {index}");
    }
    assert_holds(&table, &model);
}

#[test]
fn a_leaf_splits_inside_a_key_and_repeats_it() {
    let mut leaf = Leaf::new(&factories(), &Default::default());
    let mut model = Model::new();
    for (key, val) in [(1, 1), (1, 2), (2, 1), (2, 2), (2, 3), (2, 4), (3, 1)] {
        let slot = leaf.search(key.erase(), val.erase());
        assert!(!slot.found);
        leaf.insert(slot, key.erase(), val.erase(), 1i64.erase());
        apply_to_model(&mut model, key, val, 1);
    }
    // Key 1: items 0 and 1. Key 2: items 2..6. Key 3: item 6. The middle is
    // item 3, which key 2 holds, and the nearest boundary is at item 2.
    let (right, inside) = {
        let mut copy = leaf.clone();
        let (right, inside) = copy.split_off();
        assert_eq!((copy.items(), right.items()), (2, 5));
        (right, inside)
    };
    assert!(!inside);
    assert_eq!(right.key(0).downcast_checked::<u64>(), &2);

    // A leaf of a single key splits inside it, and both halves name the key.
    let mut single = Leaf::new(&factories(), &Default::default());
    for val in 0..6u64 {
        let slot = single.search(7u64.erase(), val.erase());
        single.insert(slot, 7u64.erase(), val.erase(), 1i64.erase());
    }
    let (right, inside) = single.split_off();
    assert!(inside);
    assert_eq!((single.entries(), single.items()), (1, 3));
    assert_eq!((right.entries(), right.items()), (1, 3));
    assert_eq!(single.key(0), right.key(0));
    assert_eq!(single.val(2).downcast_checked::<u64>(), &2);
    assert_eq!(right.val(0).downcast_checked::<u64>(), &3);
    assert_eq!(single.items_of(0), 0..3);
    assert_eq!(right.items_of(0), 0..3);
}

#[test]
fn strings_are_moved_and_dropped_without_error() {
    let factories =
        VecIndexedWSetFactories::<DynData, DynData, DynZWeight>::new::<String, String, ZWeight>();
    let mut table = Memtable::<IndexedLeaf<_, _, _>>::with_shape(&factories, SMALL);
    let mut model = BTreeMap::<(String, String), ZWeight>::new();
    for index in 0..300u64 {
        let key = format!("key {:03}", index * 37 % 41);
        let val = format!("a rather long value, number {}", index * 91 % 53);
        let weight = if index % 5 == 0 { -1 } else { 1 };
        table.insert(key.erase(), val.erase(), weight.erase());
        let sum = model.entry((key.clone(), val.clone())).or_default();
        *sum += weight;
        if *sum == 0 {
            model.remove(&(key, val));
        }
    }
    let items = table.validate();
    let actual: BTreeMap<(String, String), ZWeight> = items
        .iter()
        .map(|(k, v, w)| {
            (
                (
                    k.downcast_checked::<String>().clone(),
                    v.downcast_checked::<String>().clone(),
                ),
                *w.downcast_checked::<ZWeight>(),
            )
        })
        .collect();
    assert_eq!(actual, model);
    drop(table);
}

#[test]
fn a_wset_memtable_holds_keys_and_weights() {
    let factories = VecWSetFactories::<DynData, DynZWeight>::new::<u64, (), ZWeight>();
    let mut table = Memtable::<WSetLeaf<DynData, DynZWeight>>::with_shape(&factories, SMALL);
    let mut model = BTreeMap::<u64, ZWeight>::new();
    for index in 0..400u64 {
        let key = index * 7919 % 101;
        let weight = if index % 3 == 0 { -2 } else { 1 };
        table.insert(key.erase(), ().erase(), weight.erase());
        let sum = model.entry(key).or_default();
        *sum += weight;
        if *sum == 0 {
            model.remove(&key);
        }
    }
    let items = table.validate();
    let actual: BTreeMap<u64, ZWeight> = items
        .iter()
        .map(|(k, _, w)| {
            (
                *k.downcast_checked::<u64>(),
                *w.downcast_checked::<ZWeight>(),
            )
        })
        .collect();
    assert_eq!(actual, model);
    assert_eq!(table.totals().keys, model.len());
    assert_eq!(
        table.totals().negative_weights as usize,
        model.values().filter(|&&w| w < 0).count()
    );
    assert!(table.height() >= 3);
}

/// A batch of `(key, value, weight)` tuples, as the indexed W-set that the
/// spine would insert.
pub(super) fn indexed_batch(
    tuples: &[(u64, u64, ZWeight)],
) -> VecIndexedWSet<DynData, DynData, DynZWeight> {
    let mut erased: Box<DynWeightedPairs<DynPair<DynData, DynData>, DynZWeight>> =
        Box::new(LeanVec::from(
            tuples
                .iter()
                .map(|&(k, v, w)| Tup2(Tup2(k, v), w))
                .collect::<Vec<_>>(),
        ))
        .erase_box();
    VecIndexedWSet::dyn_from_tuples(&factories(), (), &mut erased)
}

pub(super) fn wset_batch(tuples: &[(u64, ZWeight)]) -> VecWSet<DynData, DynZWeight> {
    let mut erased: Box<DynWeightedPairs<DynPair<DynData, DynUnit>, DynZWeight>> =
        Box::new(LeanVec::from(
            tuples
                .iter()
                .map(|&(k, w)| Tup2(Tup2(k, ()), w))
                .collect::<Vec<_>>(),
        ))
        .erase_box();
    VecWSet::dyn_from_tuples(
        &VecWSetFactories::<DynData, DynZWeight>::new::<u64, (), ZWeight>(),
        (),
        &mut erased,
    )
}

#[test]
fn a_wset_batch_inserts_through_its_cursor() {
    let factories = VecWSetFactories::<DynData, DynZWeight>::new::<u64, (), ZWeight>();
    let mut table = Memtable::<WSetLeaf<DynData, DynZWeight>>::with_shape(&factories, SMALL);
    table.insert_batch(&mut wset_batch(&[(1, 1), (5, 2), (9, -1)]).cursor());
    table.insert_batch(&mut wset_batch(&[(5, -2), (6, 1), (9, 1)]).cursor());
    let items = table.validate();
    let keys: Vec<u64> = items
        .iter()
        .map(|(k, _, _)| *k.downcast_checked::<u64>())
        .collect();
    assert_eq!(keys, [1, 6]);
}

/// A sorted batch's tuples interleave with what the tree holds, and
/// a later batch cancels some of them.
#[test]
fn sorted_batches_interleave_and_cancel() {
    let mut table = table();
    let mut model = Model::new();
    let batches: Vec<Vec<(u64, u64, ZWeight)>> = (0..12u64)
        .map(|round| {
            (0..40u64)
                .map(|index| {
                    let key = (index * 5 + round * 3) % 61;
                    let val = (index + round) % 4;
                    let weight = if (index + round) % 7 == 0 { -1 } else { 1 };
                    (key, val, weight)
                })
                .collect()
        })
        .collect();
    for batch in &batches {
        for &(key, val, weight) in batch {
            apply_to_model(&mut model, key, val, weight);
        }
        table.insert_batch(&mut indexed_batch(batch).cursor());
        assert_holds(&table, &model);
    }
    assert!(table.height() >= 3);
}

#[test]
fn a_frozen_memtable_does_not_see_later_inserts() {
    let mut table = table();
    let mut model = Model::new();
    for key in 0..50 {
        insert(&mut table, key, key, 1);
        apply_to_model(&mut model, key, key, 1);
    }
    let frozen: FrozenMemtable<Leaf> = table.freeze();
    let frozen_model = model.clone();
    assert!(frozen.shares_root_with(&table.freeze()));

    // Inserts, cancellations and enough new keys to split nodes.
    for key in 0..120 {
        let (k, v, w) = (key * 3 % 80, key, if key % 4 == 0 { -1 } else { 1 });
        insert(&mut table, k, v, w);
        apply_to_model(&mut model, k, v, w);
        insert(&mut table, key % 50, key % 50, -1);
        apply_to_model(&mut model, key % 50, key % 50, -1);
    }
    assert_holds(&table, &model);
    assert_ne!(model, frozen_model);
    assert_eq!(model_of(&frozen.items()), frozen_model);
    assert_eq!(frozen.totals().tuples, frozen_model.len());
    assert!(!frozen.shares_root_with(&table.freeze()));

    // A second frozen memtable sees the later state, and the first is unchanged.
    let later = table.freeze();
    insert(&mut table, 1000, 1, 1);
    assert_eq!(model_of(&later.items()), model);
    assert_eq!(model_of(&frozen.items()), frozen_model);
}

#[test]
fn nodes_are_copied_only_while_a_frozen_memtable_is_held() {
    let mut table = table();
    for key in 0..100 {
        insert(&mut table, key, 0, 1);
    }
    assert_eq!(table.node_copies(), 0, "nothing shares the nodes");

    let frozen = table.freeze();
    insert(&mut table, 50, 1, 1);
    let copies = table.node_copies();
    // The path from the root to the leaf: every level is copied.
    assert_eq!(copies, table.height() as u64);

    // The path is now unshared, so the same path copies nothing more.
    insert(&mut table, 50, 2, 1);
    assert_eq!(table.node_copies(), copies);

    // An insert elsewhere copies that path's nodes below the shared top.
    insert(&mut table, 5, 1, 1);
    assert!(table.node_copies() > copies);
    let copies = table.node_copies();

    drop(frozen);
    for key in 0..100 {
        insert(&mut table, key, 3, 1);
    }
    assert_eq!(table.node_copies(), copies, "no frozen memtable is held");
    table.validate();
}

#[test]
fn a_cursor_style_holder_of_the_root_also_forces_copies() {
    let mut table = table();
    insert(&mut table, 1, 1, 1);
    let held = table.freeze();
    insert(&mut table, 2, 1, 1);
    assert_eq!(table.node_copies(), 1);
    assert_eq!(held.items().len(), 1);
}

fn batch_strategy() -> impl Strategy<Value = Vec<(u64, u64, ZWeight)>> {
    // Few keys and values, so that tuples collide and cancel and keys
    // gather enough values to split.
    vec((0..24u64, 0..40u64, -2..=2i64), 0..60)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn the_memtable_matches_the_model(batches in vec(batch_strategy(), 1..12)) {
        let mut table = table();
        let mut model = Model::new();
        for tuples in batches {
            for &(key, val, weight) in &tuples {
                apply_to_model(&mut model, key, val, weight);
            }
            table.insert_batch(&mut indexed_batch(&tuples).cursor());
            assert_holds(&table, &model);
        }
        // Totals are checked by `validate` against the items; check the model too.
        let totals = table.totals();
        prop_assert_eq!(totals.tuples, model.len());
        prop_assert_eq!(totals.keys, model.keys().map(|(k, _)| k).collect::<BTreeSet<_>>().len());
        prop_assert_eq!(totals.negative_weights as usize, model.values().filter(|&&w| w < 0).count());
    }

    #[test]
    fn a_frozen_memtable_keeps_its_contents(
        before in vec(batch_strategy(), 1..5),
        after in vec(batch_strategy(), 1..5),
    ) {
        let mut table = table();
        let mut model = Model::new();
        for tuples in before {
            for &(key, val, weight) in &tuples {
                apply_to_model(&mut model, key, val, weight);
            }
            table.insert_batch(&mut indexed_batch(&tuples).cursor());
        }
        let frozen = table.freeze();
        let frozen_model = model.clone();
        let copies = table.node_copies();
        for tuples in after {
            for &(key, val, weight) in &tuples {
                apply_to_model(&mut model, key, val, weight);
            }
            table.insert_batch(&mut indexed_batch(&tuples).cursor());
        }
        assert_holds(&table, &model);
        prop_assert_eq!(model_of(&frozen.items()), frozen_model);
        prop_assert!(table.node_copies() >= copies);
    }

    #[test]
    fn single_inserts_match_batch_inserts(tuples in batch_strategy()) {
        let mut one_by_one = table();
        let mut batched = table();
        for &(key, val, weight) in &tuples {
            insert(&mut one_by_one, key, val, weight);
        }
        batched.insert_batch(&mut indexed_batch(&tuples).cursor());
        prop_assert_eq!(model_of(&one_by_one.validate()), model_of(&batched.validate()));
        prop_assert_eq!(one_by_one.totals().tuples, batched.totals().tuples);
        prop_assert_eq!(one_by_one.totals().keys, batched.totals().keys);
    }
}

#[test]
fn the_leaf_split_target_is_a_parameter_with_an_8_kib_default() {
    assert_eq!(TreeShape::default().leaf_bytes, 8 * 1024);
    let heights: Vec<usize> = [TreeShape::default(), TreeShape::with_leaf_bytes(1024)]
        .into_iter()
        .map(|shape| {
            let mut table = Memtable::<Leaf>::with_shape(&factories(), shape);
            for key in 0..2000u64 {
                insert(&mut table, key * 7919 % 2003, key, 1);
            }
            table.validate();
            table.height()
        })
        .collect();
    // 2000 items of 32 bytes are about 64 KiB: 8 leaves of 8 KiB, 64 of 1 KiB.
    assert_eq!(heights[0], 2);
    assert!(heights[1] > heights[0], "{heights:?}");
}

#[test]
fn leaf_memory_counts_old_copies_until_the_snapshot_goes() {
    let mut table = table();
    let counter = table.leaf_memory().clone();
    for key in 0..100 {
        insert(&mut table, key, 0, 1);
    }
    // These elements have no heap bytes, so the leaves' inline bytes are all.
    assert_eq!(table.live_leaf_bytes(), table.current_inline_leaf_bytes());
    assert_eq!(table.copy_nanos(), 0, "nothing was copied");

    // An insert while a frozen memtable is held copies a leaf, and the old
    // copy stays alive in the snapshot.
    let frozen = table.freeze();
    let before = table.live_leaf_bytes();
    insert(&mut table, 50, 1, 1);
    assert!(table.live_leaf_bytes() > before + 16);
    assert!(table.live_leaf_bytes() > table.current_inline_leaf_bytes());
    assert!(table.copy_nanos() > 0);

    // Dropping the snapshot releases the old copies.
    drop(frozen);
    assert_eq!(table.live_leaf_bytes(), table.current_inline_leaf_bytes());
    let nanos = table.copy_nanos();
    insert(&mut table, 50, 2, 1);
    assert_eq!(table.copy_nanos(), nanos, "no copy, no time");

    // Removals shrink the leaves' share.
    let before = table.live_leaf_bytes();
    for key in 0..100 {
        insert(&mut table, key, 0, -1);
    }
    assert!(table.live_leaf_bytes() < before);
    assert_eq!(table.live_leaf_bytes(), table.current_inline_leaf_bytes());

    // Dropping the memtable gives back everything.
    drop(table);
    assert_eq!(counter.load(std::sync::atomic::Ordering::Relaxed), 0);
}

#[test]
fn leaf_memory_includes_the_heap_bytes_of_elements() {
    let factories =
        VecIndexedWSetFactories::<DynData, DynData, DynZWeight>::new::<String, String, ZWeight>();
    let mut table = Memtable::<IndexedLeaf<_, _, _>>::with_shape(&factories, SMALL);
    let counter = table.leaf_memory().clone();
    for index in 0..60u64 {
        let key = format!("key {index:03}");
        let val = "v".repeat(100);
        table.insert(key.erase(), val.erase(), 1i64.erase());
    }
    // Each value has 100 heap bytes beyond its inline size.
    assert!(table.live_leaf_bytes() >= table.current_inline_leaf_bytes() + 60 * 100);
    drop(table);
    assert_eq!(counter.load(std::sync::atomic::Ordering::Relaxed), 0);
}
