//! Tests of the memtable cursor and of the fallback batches' memtable arm.
//!
//! The cursor is checked against the cursor of a `VecIndexedWSet` or
//! `VecWSet` built from the same tuples, under random sequences of every
//! `Cursor` method.

use super::{
    FrozenMemtable, Memtable, WSetLeaf,
    test::{Leaf, Model, SMALL, indexed_batch, insert, table, wset_batch},
};
use crate::{
    DynZWeight, ZWeight,
    algebra::NegByRef,
    circuit::{CircuitConfig, CircuitStorageConfig},
    dynamic::{
        DataTrait, DowncastTrait, DynData, DynPair, DynUnit, DynWeightedPairs, Erase, LeanVec,
    },
    trace::{
        Batch, BatchLocation, BatchReader, BatchReaderFactories, Builder, Cursor,
        FallbackIndexedWSet, FallbackIndexedWSetFactories, FallbackWSet, FallbackWSetFactories,
        Filter, GroupFilter, ListMerger, MergeCursor, RunReads, VecWSetFactories,
        cursor::{CursorList, DelegatingCursor},
        test::run_in_circuit_with_storage_config,
    },
    utils::Tup2,
};
use feldera_types::config::{
    StorageCacheConfig, StorageCompression, StorageConfig, StorageOptions,
};
use proptest::{collection::vec, prelude::*};
use rand::{Rng, SeedableRng, rngs::StdRng};
use std::{collections::BTreeMap, path::Path};
use tempfile::tempdir;

type Tuple = (u64, u64, ZWeight);
type Fallback = FallbackIndexedWSet<DynData, DynData, DynZWeight>;
type FallbackFactories = FallbackIndexedWSetFactories<DynData, DynData, DynZWeight>;

fn key_box(key: u64) -> Box<DynData> {
    Box::new(key).erase_box()
}

fn frozen_of(tuples: &[Tuple]) -> FrozenMemtable<Leaf> {
    let mut table = table();
    table.insert_batch(&mut indexed_batch(tuples).cursor());
    table.freeze()
}

fn wset_frozen_of(tuples: &[(u64, ZWeight)]) -> FrozenMemtable<WSetLeaf<DynData, DynZWeight>> {
    let factories = VecWSetFactories::<DynData, DynZWeight>::new::<u64, (), ZWeight>();
    let mut table = Memtable::<WSetLeaf<DynData, DynZWeight>>::with_shape(&factories, SMALL);
    table.insert_batch(&mut wset_batch(tuples).cursor());
    table.freeze()
}

/// What a reader sees at the cursor's position.
#[derive(Debug, PartialEq, Eq)]
struct Observed {
    key: Option<u64>,
    val: Option<u64>,
    weight: Option<ZWeight>,
}

fn observe<V, C>(cursor: &mut C, read_val: &dyn Fn(&V) -> u64) -> Observed
where
    V: DataTrait + ?Sized,
    C: Cursor<DynData, V, (), DynZWeight>,
{
    let key = cursor
        .key_valid()
        .then(|| *cursor.key().downcast_checked::<u64>());
    let both = key.is_some() && cursor.val_valid();
    Observed {
        key,
        val: both.then(|| read_val(cursor.val())),
        weight: both.then(|| *cursor.weight().downcast_checked::<ZWeight>()),
    }
}

#[derive(Clone, Copy, Debug)]
enum Op {
    StepKey,
    StepKeyReverse,
    SeekKey(u64),
    SeekKeyReverse(u64),
    SeekKeyWith(u64),
    SeekKeyWithReverse(u64),
    RewindKeys,
    FastForwardKeys,
    StepVal,
    StepValReverse,
    SeekVal(u64),
    SeekValReverse(u64),
    SeekValWith(u64),
    SeekValWithReverse(u64),
    RewindVals,
    FastForwardVals,
    MapValues,
}

/// How a fuzz run reads and makes values, so that it covers both kinds of
/// batch.
struct Fuzz<V: ?Sized + 'static> {
    make_val: &'static dyn Fn(u64) -> Box<V>,
    read_val: &'static dyn Fn(&V) -> u64,
    /// The largest key and value in the data.
    max_key: u64,
    max_val: u64,
    /// Whether to use the seeks with predicates, whose probes the two cursors
    /// need not make in the same number.
    with_predicates: bool,
    /// Whether the two cursors must count the same reads.
    compare_reads: bool,
}

fn apply<V, C>(cursor: &mut C, op: Op, fuzz: &Fuzz<V>)
where
    V: DataTrait + ?Sized,
    C: Cursor<DynData, V, (), DynZWeight>,
{
    let read_val = fuzz.read_val;
    match op {
        Op::StepKey => cursor.step_key(),
        Op::StepKeyReverse => cursor.step_key_reverse(),
        Op::SeekKey(target) => cursor.seek_key(&*key_box(target)),
        Op::SeekKeyReverse(target) => cursor.seek_key_reverse(&*key_box(target)),
        Op::SeekKeyWith(target) => {
            cursor.seek_key_with(&|probe| *probe.downcast_checked::<u64>() >= target)
        }
        Op::SeekKeyWithReverse(target) => {
            cursor.seek_key_with_reverse(&|probe| *probe.downcast_checked::<u64>() <= target)
        }
        Op::RewindKeys => cursor.rewind_keys(),
        Op::FastForwardKeys => cursor.fast_forward_keys(),
        Op::StepVal => cursor.step_val(),
        Op::StepValReverse => cursor.step_val_reverse(),
        Op::SeekVal(target) => cursor.seek_val(&*(fuzz.make_val)(target)),
        Op::SeekValReverse(target) => cursor.seek_val_reverse(&*(fuzz.make_val)(target)),
        Op::SeekValWith(target) => cursor.seek_val_with(&|probe| read_val(probe) >= target),
        Op::SeekValWithReverse(target) => {
            cursor.seek_val_with_reverse(&|probe| read_val(probe) <= target)
        }
        Op::RewindVals => cursor.rewind_vals(),
        Op::FastForwardVals => cursor.fast_forward_vals(),
        Op::MapValues => cursor.map_values(&mut |_, _| {}),
    }
}

/// One operation, then what a reader sees and the reads it took.
fn step<V, C>(cursor: &mut C, op: Op, fuzz: &Fuzz<V>) -> (Observed, (u64, u64, u64, u64))
where
    V: DataTrait + ?Sized,
    C: Cursor<DynData, V, (), DynZWeight>,
{
    let before = RunReads::current();
    apply(cursor, op, fuzz);
    let observed = observe(cursor, fuzz.read_val);
    let reads = RunReads::current().since(before);
    (
        observed,
        (
            reads.seeks,
            reads.memory_key_reads,
            reads.memory_value_reads,
            reads.memory_weight_reads,
        ),
    )
}

/// Applies the same random legal operations to both cursors, and checks after
/// each that a reader sees the same.
fn run_fuzz<V, R, A>(
    reference: &mut R,
    actual: &mut A,
    rng: &mut StdRng,
    steps: usize,
    fuzz: &Fuzz<V>,
) where
    V: DataTrait + ?Sized,
    R: Cursor<DynData, V, (), DynZWeight>,
    A: Cursor<DynData, V, (), DynZWeight>,
{
    let mut keys_backward = false;
    let mut vals_backward = false;
    for _ in 0..steps {
        let key_target = rng.gen_range(0..=fuzz.max_key + 1);
        let val_target = rng.gen_range(0..=fuzz.max_val + 1);
        // The operations that the cursors' contract allows now. Both cursors
        // are asked, since `val_valid` counts the lazy read of a first value.
        let key_valid = reference.key_valid();
        assert_eq!(actual.key_valid(), key_valid);
        let val_valid = key_valid && reference.val_valid();
        assert_eq!(key_valid && actual.val_valid(), val_valid);
        let mut ops = vec![Op::RewindKeys, Op::FastForwardKeys];
        if key_valid {
            if keys_backward {
                ops.extend([Op::StepKeyReverse, Op::SeekKeyReverse(key_target)]);
                if fuzz.with_predicates {
                    ops.push(Op::SeekKeyWithReverse(key_target));
                }
            } else {
                ops.extend([Op::StepKey, Op::SeekKey(key_target)]);
                if fuzz.with_predicates {
                    ops.push(Op::SeekKeyWith(key_target));
                }
            }
            ops.extend([Op::RewindVals, Op::FastForwardVals]);
            if val_valid {
                if vals_backward {
                    ops.extend([Op::StepValReverse, Op::SeekValReverse(val_target)]);
                    if fuzz.with_predicates {
                        ops.push(Op::SeekValWithReverse(val_target));
                    }
                } else {
                    ops.extend([Op::StepVal, Op::SeekVal(val_target), Op::MapValues]);
                    if fuzz.with_predicates {
                        ops.push(Op::SeekValWith(val_target));
                    }
                }
            }
        }
        let op = ops[rng.gen_range(0..ops.len())];

        let (expected, expected_reads) = step(reference, op, fuzz);
        let (got, got_reads) = step(actual, op, fuzz);
        assert_eq!(got, expected, "after {op:?}");
        if fuzz.compare_reads {
            assert_eq!(got_reads, expected_reads, "reads of {op:?}");
        }

        // Track the directions as the contract defines them: a key move
        // resets the value direction.
        match op {
            Op::StepKey
            | Op::SeekKey(_)
            | Op::SeekKeyWith(_)
            | Op::StepKeyReverse
            | Op::SeekKeyReverse(_)
            | Op::SeekKeyWithReverse(_) => vals_backward = false,
            Op::RewindKeys => {
                keys_backward = false;
                vals_backward = false;
            }
            Op::FastForwardKeys => {
                keys_backward = true;
                vals_backward = false;
            }
            Op::RewindVals => vals_backward = false,
            Op::FastForwardVals => vals_backward = true,
            _ => {}
        }
    }
}

fn indexed_fuzz(max_key: u64, max_val: u64, with_predicates: bool) -> Fuzz<DynData> {
    Fuzz {
        make_val: &|val| Box::new(val).erase_box(),
        read_val: &|val| *val.downcast_checked::<u64>(),
        max_key,
        max_val,
        with_predicates,
        compare_reads: !with_predicates,
    }
}

fn unit_fuzz(max_key: u64, with_predicates: bool) -> Fuzz<DynUnit> {
    Fuzz {
        make_val: &|_| Box::new(()).erase_box(),
        read_val: &|_| 0,
        max_key,
        max_val: 0,
        with_predicates,
        compare_reads: !with_predicates,
    }
}

/// Few keys with many values, so that a key's values fill several leaves.
fn tuples_strategy() -> impl Strategy<Value = Vec<Tuple>> {
    vec((0..7u64, 0..90u64, -2..=2i64), 0..250)
}

fn run_indexed_fuzz(tuples: &[Tuple], seed: u64, with_predicates: bool) {
    let frozen = frozen_of(tuples);
    let reference_batch = indexed_batch(tuples);
    let mut rng = StdRng::seed_from_u64(seed);
    run_fuzz(
        &mut reference_batch.cursor(),
        &mut frozen.cursor(),
        &mut rng,
        300,
        &indexed_fuzz(7, 90, with_predicates),
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn the_cursor_matches_the_vec_cursor_under_random_operations(
        tuples in tuples_strategy(),
        seed in any::<u64>(),
    ) {
        run_indexed_fuzz(&tuples, seed, true);
    }

    #[test]
    fn the_cursor_counts_the_reads_the_vec_cursor_counts(
        tuples in tuples_strategy(),
        seed in any::<u64>(),
    ) {
        run_indexed_fuzz(&tuples, seed, false);
    }

    #[test]
    fn the_wset_cursor_matches_the_vec_wset_cursor(
        tuples in vec((0..200u64, -2..=2i64), 0..300),
        seed in any::<u64>(),
    ) {
        let frozen = wset_frozen_of(&tuples);
        let reference_batch = wset_batch(&tuples);
        let mut rng = StdRng::seed_from_u64(seed);
        // Predicates on the unit value, as the `VecWSet` cursor evaluates them.
        for with_predicates in [true, false] {
            run_fuzz(
                &mut reference_batch.cursor(),
                &mut frozen.cursor(),
                &mut rng,
                200,
                &unit_fuzz(200, with_predicates),
            );
        }
    }

    #[test]
    fn exact_seeks_find_the_keys_the_vec_cursor_finds(
        tuples in tuples_strategy(),
        mut probes in vec(0..9u64, 0..20),
    ) {
        let frozen = frozen_of(&tuples);
        let reference_batch = indexed_batch(&tuples);
        let (mut reference, mut actual) = (reference_batch.cursor(), frozen.cursor());
        // Each probe is greater than the one before, as the contract requires.
        probes.sort_unstable();
        probes.dedup();
        let fuzz = indexed_fuzz(7, 90, false);
        for probe in probes {
            let key = key_box(probe);
            prop_assert_eq!(
                actual.seek_key_exact(&*key, None),
                reference.seek_key_exact(&*key, None)
            );
            prop_assert_eq!(
                observe(&mut actual, fuzz.read_val),
                observe(&mut reference, fuzz.read_val)
            );
        }
    }
}

#[test]
fn a_key_split_between_leaves_is_one_key_with_one_run_of_values() {
    // Key 5's 100 values fill many leaves of about four items.
    let tuples: Vec<Tuple> = (0..100)
        .map(|val| (5, val, 1))
        .chain([(4, 0, 1), (6, 0, 1)])
        .collect();
    let mut table = table();
    // Insert in an order that fills the leaves from both ends.
    for &(key, val, weight) in tuples.iter().rev() {
        insert(&mut table, key, val, weight);
    }
    assert!(table.height() >= 3);
    let frozen = table.freeze();
    let fuzz = indexed_fuzz(7, 100, false);

    let mut cursor = frozen.cursor();
    let mut keys = Vec::new();
    let mut vals_of_5 = Vec::new();
    while cursor.key_valid() {
        let key = *cursor.key().downcast_checked::<u64>();
        keys.push(key);
        while cursor.val_valid() {
            if key == 5 {
                vals_of_5.push((fuzz.read_val)(cursor.val()));
            }
            cursor.step_val();
        }
        cursor.step_key();
    }
    assert_eq!(keys, [4, 5, 6]);
    assert_eq!(vals_of_5, (0..100).collect::<Vec<_>>());
    assert_eq!(frozen.totals().keys, 3);

    // Backward: keys, then the values of key 5.
    cursor.fast_forward_keys();
    cursor.step_key_reverse();
    assert_eq!(cursor.key().downcast_checked::<u64>(), &5);
    cursor.fast_forward_vals();
    let mut backward = Vec::new();
    while cursor.val_valid() {
        backward.push((fuzz.read_val)(cursor.val()));
        cursor.step_val_reverse();
    }
    assert_eq!(backward, (0..100).rev().collect::<Vec<_>>());
    cursor.step_key_reverse();
    assert_eq!(cursor.key().downcast_checked::<u64>(), &4);
    cursor.step_key_reverse();
    assert!(!cursor.key_valid());

    // Seeks that cross leaves.
    let mut cursor = frozen.cursor();
    cursor.seek_key(&*key_box(5));
    cursor.seek_val(&*(fuzz.make_val)(77));
    assert_eq!((fuzz.read_val)(cursor.val()), 77);
    cursor.seek_val_reverse(&*(fuzz.make_val)(12));
    assert_eq!((fuzz.read_val)(cursor.val()), 12);
    cursor.seek_key_reverse(&*key_box(5));
    assert_eq!(cursor.key().downcast_checked::<u64>(), &5);
    cursor.rewind_vals();
    assert_eq!((fuzz.read_val)(cursor.val()), 0);
}

#[test]
fn the_position_counts_keys() {
    // One value per key, so no key is split.
    let tuples: Vec<Tuple> = (0..60).map(|key| (key, 0, 1)).collect();
    let frozen = frozen_of(&tuples);
    let reference_batch = indexed_batch(&tuples);
    let (mut reference, mut actual) = (reference_batch.cursor(), frozen.cursor());
    while reference.key_valid() {
        assert_eq!(actual.position(), reference.position());
        reference.step_key();
        actual.step_key();
    }
    assert_eq!(actual.position(), reference.position());

    // With a split key, the offset stays within the total.
    let split = frozen_of(&(0..100).map(|val| (1, val, 1)).collect::<Vec<_>>());
    let mut cursor = split.cursor();
    let position = cursor.position().unwrap();
    assert_eq!((position.total, position.offset), (1, 0));
    cursor.step_key();
    assert_eq!(cursor.position().unwrap().offset, 1);
}

#[test]
fn an_empty_memtable_has_no_keys() {
    let frozen = table().freeze();
    let mut cursor = frozen.cursor();
    assert!(!cursor.key_valid());
    cursor.step_key();
    cursor.rewind_keys();
    cursor.fast_forward_keys();
    cursor.seek_key(&*key_box(1));
    assert!(!cursor.key_valid());
}

/// The contents of a cursor over any batch, with the weights of equal pairs
/// added.
fn contents<C: Cursor<DynData, DynData, (), DynZWeight>>(mut cursor: C) -> Model {
    let mut model = Model::new();
    while cursor.key_valid() {
        let key = *cursor.key().downcast_checked::<u64>();
        while cursor.val_valid() {
            let val = *cursor.val().downcast_checked::<u64>();
            cursor.map_times(&mut |_, weight| {
                *model.entry((key, val)).or_default() += *weight.downcast_checked::<ZWeight>();
            });
            cursor.step_val();
        }
        cursor.step_key();
    }
    model.retain(|_, weight| *weight != 0);
    model
}

fn merge_contents(
    mut cursor: Box<dyn MergeCursor<DynData, DynData, (), DynZWeight> + Send + '_>,
) -> Model {
    let mut model = Model::new();
    while cursor.key_valid() {
        let key = *cursor.key().downcast_checked::<u64>();
        while cursor.val_valid() {
            let val = *cursor.val().downcast_checked::<u64>();
            cursor.map_times(&mut |_, weight| {
                *model.entry((key, val)).or_default() += *weight.downcast_checked::<ZWeight>();
            });
            cursor.step_val();
        }
        cursor.step_key();
    }
    model
}

fn model_of_tuples(tuples: &[Tuple]) -> Model {
    let mut model = Model::new();
    for &(key, val, weight) in tuples {
        *model.entry((key, val)).or_default() += weight;
    }
    model.retain(|_, weight| *weight != 0);
    model
}

fn fallback_factories() -> FallbackFactories {
    FallbackFactories::new::<u64, u64, ZWeight>()
}

/// A fallback batch that reads a memtable of `tuples` in place.
fn memtable_run(factories: &FallbackFactories, tuples: &[Tuple]) -> Fallback {
    let mut table = Memtable::<Leaf>::with_shape(&factories.vec_indexed_wset_factory, SMALL);
    table.insert_batch(&mut indexed_batch(tuples).cursor());
    Fallback::from_memtable(factories, table.freeze())
}

/// A fallback batch of `tuples` that today's builders put in `location`.
fn built_run(factories: &FallbackFactories, tuples: &[Tuple], location: BatchLocation) -> Fallback {
    let mut erased: Box<DynWeightedPairs<DynPair<DynData, DynData>, DynZWeight>> =
        Box::new(LeanVec::from(
            tuples
                .iter()
                .map(|&(k, v, w)| Tup2(Tup2(k, v), w))
                .collect::<Vec<_>>(),
        ))
        .erase_box();
    let initial = Fallback::dyn_from_tuples(factories, (), &mut erased);
    let builder = <Fallback as Batch>::Builder::for_merge(factories, [&initial], Some(location));
    ListMerger::merge(factories, builder, vec![initial.merge_cursor(None, None)])
}

#[test]
fn a_fallback_batch_over_a_memtable_reads_like_one_over_a_vec() {
    let tuples: Vec<Tuple> = (0..400u64)
        .map(|n| (n * 7 % 23, n * 13 % 31, if n % 4 == 0 { -1 } else { 1 }))
        .collect();
    let factories = fallback_factories();
    let memtable = memtable_run(&factories, &tuples);
    let vec_run = built_run(&factories, &tuples, BatchLocation::Memory);

    assert_eq!(contents(memtable.cursor()), contents(vec_run.cursor()));
    assert_eq!(contents(memtable.cursor()), model_of_tuples(&tuples));
    assert_eq!(memtable.len(), vec_run.len());
    assert_eq!(memtable.key_count(), vec_run.key_count());
    assert_eq!(
        memtable.negative_weight_count(),
        vec_run.negative_weight_count()
    );
    assert_eq!(memtable.location(), BatchLocation::Memory);
    assert!(memtable.keys().is_none());
    assert!(memtable.approximate_byte_size() > 0);
    assert!(!memtable.is_empty());
    let (low, high) = memtable.key_bounds().unwrap();
    let (expected_low, expected_high) = vec_run.key_bounds().unwrap();
    assert_eq!((low, high), (expected_low, expected_high));

    // A clone shares the tree, and so reads the same.
    assert_eq!(
        contents(memtable.clone().cursor()),
        contents(memtable.cursor())
    );
    // Negation materializes the memtable.
    let negated = memtable.neg_by_ref();
    let expected: Model = model_of_tuples(&tuples)
        .into_iter()
        .map(|(pair, weight)| (pair, -weight))
        .collect();
    assert_eq!(contents(negated.cursor()), expected);
    // Equality compares contents.
    assert!(memtable == vec_run);
}

#[test]
fn merge_cursors_filter_a_memtable_as_they_filter_a_vec() {
    let tuples: Vec<Tuple> = (0..300u64).map(|n| (n % 17, n % 41, 1)).collect();
    let factories = fallback_factories();
    let memtable = memtable_run(&factories, &tuples);
    let vec_run = built_run(&factories, &tuples, BatchLocation::Memory);

    assert_eq!(
        merge_contents(memtable.merge_cursor(None, None)),
        merge_contents(vec_run.merge_cursor(None, None))
    );
    let key_filter = || {
        Some(Filter::new(Box::new(|key: &DynData| {
            *key.downcast_checked::<u64>() >= 5
        })))
    };
    let value_filter = || {
        Some(GroupFilter::Simple(Filter::new(Box::new(
            |val: &DynData| *val.downcast_checked::<u64>() % 2 == 0,
        ))))
    };
    type Filters = (Option<Filter<DynData>>, Option<GroupFilter<DynData>>);
    let cases: Vec<Box<dyn Fn() -> Filters>> = vec![
        Box::new(move || (key_filter(), None)),
        Box::new(move || (None, value_filter())),
        Box::new(move || (key_filter(), value_filter())),
    ];
    for case in cases {
        let (key, value) = case();
        let expected = merge_contents(vec_run.merge_cursor(key, value));
        assert!(!expected.is_empty());
        let (key, value) = case();
        assert_eq!(merge_contents(memtable.merge_cursor(key, value)), expected);
    }
    // Consuming cursors read a memtable the same way.
    let mut consumed = memtable.clone();
    assert_eq!(
        merge_contents(consumed.consuming_cursor(None, None)),
        merge_contents(vec_run.merge_cursor(None, None))
    );
}

#[test]
fn sampling_keys_of_a_memtable_follows_the_contract() {
    let tuples: Vec<Tuple> = (0..600u64).map(|n| (n % 200, n, 1)).collect();
    let factories = fallback_factories();
    let memtable = memtable_run(&factories, &tuples);
    let all_keys: Vec<u64> = (0..200).collect();
    let mut rng = StdRng::seed_from_u64(7);
    for sample_size in [10, 199, 200, 500] {
        let mut sample = factories.keys_factory().default_box();
        memtable.sample_keys(&mut rng, sample_size, &mut *sample);
        let keys: Vec<u64> = sample
            .dyn_iter()
            .map(|key| *key.downcast_checked::<u64>())
            .collect();
        assert!(keys.len() <= sample_size);
        assert!(keys.windows(2).all(|pair| pair[0] < pair[1]), "ascending");
        assert!(keys.iter().all(|key| all_keys.contains(key)));
        if sample_size >= 200 {
            assert_eq!(keys, all_keys);
        } else {
            assert!(
                keys.len() > sample_size / 2,
                "{} of {sample_size}",
                keys.len()
            );
        }
    }
}

fn storage_config(path: &Path, compression: StorageCompression) -> CircuitConfig {
    CircuitConfig::with_workers(1).with_storage(Some(
        CircuitStorageConfig::for_config(
            StorageConfig {
                path: path.to_string_lossy().into_owned(),
                cache: StorageCacheConfig::default(),
            },
            StorageOptions {
                min_storage_bytes: Some(0),
                compression,
                ..StorageOptions::default()
            },
        )
        .unwrap(),
    ))
}

fn run_with_storage(compression: StorageCompression, f: impl FnOnce() + Clone + Send + 'static) {
    let dir = tempdir().unwrap();
    run_in_circuit_with_storage_config(storage_config(dir.path(), compression), f);
}

/// The seal: the memtable's merge cursor through `ListMerger` into a file, as
/// `Spine::maybe_flush_batch` copies a batch.
fn seal(factories: &FallbackFactories, memtable: &Fallback) -> Fallback {
    let builder = <Fallback as Batch>::Builder::for_merge(
        factories,
        [memtable],
        Some(BatchLocation::Storage),
    );
    ListMerger::merge(factories, builder, vec![memtable.merge_cursor(None, None)])
}

fn check_sealed_copy_reads_like_a_built_file(compression: StorageCompression) {
    run_with_storage(compression, || {
        let tuples: Vec<Tuple> = (0..2000u64)
            .map(|n| (n * 37 % 211, n * 53 % 307, if n % 5 == 0 { -1 } else { 2 }))
            .collect();
        let factories = fallback_factories();
        let sealed = seal(&factories, &memtable_run(&factories, &tuples));
        let built = built_run(&factories, &tuples, BatchLocation::Storage);

        assert_eq!(sealed.location(), BatchLocation::Storage);
        assert_eq!(built.location(), BatchLocation::Storage);
        assert_eq!(contents(sealed.cursor()), contents(built.cursor()));
        assert_eq!(contents(sealed.cursor()), model_of_tuples(&tuples));
        assert_eq!(sealed.len(), built.len());
        assert_eq!(sealed.key_count(), built.key_count());
        assert_eq!(
            sealed.negative_weight_count(),
            built.negative_weight_count()
        );
        assert_eq!(
            sealed.membership_filter_kind(),
            built.membership_filter_kind()
        );
        assert_eq!(sealed.key_bounds(), built.key_bounds());
        // The same rows through the same writer make the same file.
        assert_eq!(
            sealed.approximate_byte_size(),
            built.approximate_byte_size()
        );
    });
}

#[test]
fn a_sealed_copy_reads_like_a_built_file_without_compression() {
    check_sealed_copy_reads_like_a_built_file(StorageCompression::None);
}

#[test]
fn a_sealed_copy_reads_like_a_built_file_with_compression() {
    check_sealed_copy_reads_like_a_built_file(StorageCompression::Zstd);
    check_sealed_copy_reads_like_a_built_file(StorageCompression::Default);
}

#[test]
fn a_persisted_memtable_round_trips() {
    run_with_storage(StorageCompression::Default, || {
        let tuples: Vec<Tuple> = (0..500u64)
            .map(|n| (n % 29, n % 43, 1 + (n % 3) as i64))
            .collect();
        let factories = fallback_factories();
        let memtable = memtable_run(&factories, &tuples);
        let file = memtable.persisted().expect("a memtable persists");
        assert_eq!(file.location(), BatchLocation::Storage);
        assert_eq!(contents(file.cursor()), model_of_tuples(&tuples));
        assert!(file.persisted().is_none(), "a file is already persisted");
        assert_eq!(file.len(), memtable.len());
        assert_eq!(file.key_count(), memtable.key_count());
    });
}

#[test]
fn memtable_vec_and_file_runs_mix_in_cursor_lists_and_list_mergers() {
    run_with_storage(StorageCompression::Default, || {
        let factories = fallback_factories();
        // Overlapping keys and values, with cancellations across the runs.
        let first: Vec<Tuple> = (0..200u64).map(|n| (n % 19, n % 23, 1)).collect();
        let second: Vec<Tuple> = (0..200u64).map(|n| (n % 17, n % 29, 2)).collect();
        let third: Vec<Tuple> = (0..200u64).map(|n| (n % 19, n % 23, -1)).collect();
        let mut everything = first.clone();
        everything.extend(&second);
        everything.extend(&third);
        let expected = model_of_tuples(&everything);

        // Assignments of the three kinds of run to the three batches.
        for kinds in [
            [0, 1, 2],
            [2, 0, 1],
            [1, 2, 0],
            [2, 2, 2],
            [2, 1, 1],
            [0, 2, 0],
        ] {
            let runs: Vec<Fallback> = [&first, &second, &third]
                .into_iter()
                .zip(kinds)
                .map(|(tuples, kind)| match kind {
                    0 => built_run(&factories, tuples, BatchLocation::Memory),
                    1 => built_run(&factories, tuples, BatchLocation::Storage),
                    _ => memtable_run(&factories, tuples),
                })
                .collect();

            let list = CursorList::new(
                factories.weight_factory(),
                runs.iter().map(|run| run.cursor()).collect::<Vec<_>>(),
            );
            assert_eq!(contents(list), expected, "cursor list of {kinds:?}");

            for location in [BatchLocation::Memory, BatchLocation::Storage] {
                let builder = <Fallback as Batch>::Builder::for_merge(
                    &factories,
                    runs.iter(),
                    Some(location),
                );
                let merged: Fallback = ListMerger::merge(
                    &factories,
                    builder,
                    runs.iter()
                        .map(|run| run.merge_cursor(None, None))
                        .collect(),
                );
                assert_eq!(merged.location(), location);
                assert_eq!(contents(merged.cursor()), expected, "merge of {kinds:?}");
            }
        }
    });
}

#[test]
fn a_wset_memtable_seals_and_persists() {
    run_with_storage(StorageCompression::Default, || {
        let factories = FallbackWSetFactories::<DynData, DynZWeight>::new::<u64, (), ZWeight>();
        let tuples: Vec<(u64, ZWeight)> = (0..1500u64)
            .map(|n| (n * 31 % 401, if n % 3 == 0 { -1 } else { 1 }))
            .collect();
        let mut table = Memtable::<WSetLeaf<DynData, DynZWeight>>::with_shape(
            &factories.vec_wset_factory,
            SMALL,
        );
        table.insert_batch(&mut wset_batch(&tuples).cursor());
        let memtable = FallbackWSet::from_memtable(&factories, table.freeze());

        let builder = <FallbackWSet<DynData, DynZWeight> as Batch>::Builder::for_merge(
            &factories,
            [&memtable],
            Some(BatchLocation::Storage),
        );
        let sealed =
            ListMerger::merge(&factories, builder, vec![memtable.merge_cursor(None, None)]);
        let persisted = memtable.persisted().unwrap();
        for file in [&sealed, &persisted] {
            assert_eq!(file.location(), BatchLocation::Storage);
            assert_eq!(file.len(), memtable.len());
            assert_eq!(file.key_count(), memtable.key_count());
            assert_eq!(
                file.negative_weight_count(),
                memtable.negative_weight_count()
            );
        }

        // The same keys and weights through the cursors.
        let weights = |mut cursor: DelegatingCursor<'_, DynData, DynUnit, (), DynZWeight>| {
            let mut weights = Vec::new();
            while cursor.key_valid() {
                weights.push((
                    *cursor.key().downcast_checked::<u64>(),
                    *cursor.weight().downcast_checked::<ZWeight>(),
                ));
                cursor.step_key();
            }
            weights
        };
        assert_eq!(weights(sealed.cursor()), weights(memtable.cursor()));
        assert_eq!(weights(persisted.cursor()), weights(memtable.cursor()));
        let mut expected = BTreeMap::new();
        for &(key, weight) in &tuples {
            *expected.entry(key).or_insert(0) += weight;
        }
        expected.retain(|_, weight| *weight != 0);
        assert_eq!(
            weights(memtable.cursor()),
            expected.into_iter().collect::<Vec<_>>()
        );
    });
}
