//! The leaves of the memtable's B+-tree.
//!
//! A leaf is a small sorted array of `(key, value, weight)` items, in the
//! column layout of [`VecIndexedWSet`](crate::trace::VecIndexedWSet): the keys
//! of the leaf, a local offset per key, and the values and weights. The offsets
//! count from the leaf's first value, so an insert shifts only its own leaf.
//!
//! A leaf has *key entries*. A key's values may be split between two leaves; the
//! key is then an entry of both, and the cursor joins the two.

use crate::{
    dynamic::{DataTrait, DynUnit, DynVec, Factory, WeightTrait},
    trace::ord::{VecIndexedWSetFactories, VecWSetFactories},
};
use dyn_clone::clone_box;
use size_of::SizeOf;
use std::{
    cmp::Ordering,
    mem::size_of,
    ops::Range,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering as AtomicOrdering},
    },
};

/// The heap bytes of an element: what its `size_of` reports beyond `inline`.
fn heap_of<T: SizeOf + ?Sized>(element: &T, inline: usize) -> usize {
    element.size_of().total_bytes().saturating_sub(inline)
}

/// A leaf's share of the tree's leaf memory. A shared counter holds the sum
/// over all live leaves. A leaf adds its bytes when it is made or cloned,
/// follows its own changes, and gives its bytes back when it is dropped, so
/// the old copies that a snapshot still holds count until the snapshot goes.
pub struct LeafMemory {
    counter: Arc<AtomicUsize>,
    bytes: usize,
}

impl LeafMemory {
    pub fn new(counter: &Arc<AtomicUsize>) -> Self {
        Self {
            counter: counter.clone(),
            bytes: 0,
        }
    }

    pub fn counter(&self) -> &Arc<AtomicUsize> {
        &self.counter
    }

    /// Sets the leaf's bytes, and the counter with it.
    fn set(&mut self, bytes: usize) {
        if bytes >= self.bytes {
            self.counter
                .fetch_add(bytes - self.bytes, AtomicOrdering::Relaxed);
        } else {
            self.counter
                .fetch_sub(self.bytes - bytes, AtomicOrdering::Relaxed);
        }
        self.bytes = bytes;
    }
}

impl Clone for LeafMemory {
    fn clone(&self) -> Self {
        self.counter.fetch_add(self.bytes, AtomicOrdering::Relaxed);
        Self {
            counter: self.counter.clone(),
            bytes: self.bytes,
        }
    }
}

impl Drop for LeafMemory {
    fn drop(&mut self) {
        self.counter.fetch_sub(self.bytes, AtomicOrdering::Relaxed);
    }
}

/// A copy of the first `(key, value)` of a node, kept by the node's parent to
/// route searches.
pub struct Bound<K: ?Sized, V: ?Sized> {
    pub key: Box<K>,
    pub val: Box<V>,
}

impl<K: DataTrait + ?Sized, V: DataTrait + ?Sized> Bound<K, V> {
    pub fn new(key: &K, val: &V) -> Self {
        Self {
            key: clone_box(key),
            val: clone_box(val),
        }
    }

    /// Compares this bound to a `(key, value)` pair.
    pub fn cmp_to(&self, key: &K, val: &V) -> Ordering {
        (*self.key).cmp(key).then_with(|| (*self.val).cmp(val))
    }
}

impl<K: DataTrait + ?Sized, V: DataTrait + ?Sized> Clone for Bound<K, V> {
    fn clone(&self) -> Self {
        Self::new(&self.key, &self.val)
    }
}

/// Where a `(key, value)` pair is, or would go, in a leaf.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Slot {
    /// The key entry holding the key, or the one the key's entry would take.
    pub entry: usize,
    /// The item holding the value, or the one the value would take.
    pub item: usize,
    /// Whether `entry` holds the key.
    pub key_found: bool,
    /// Whether `item` holds the value (which implies `key_found`).
    pub found: bool,
}

/// What a leaf type must provide to the tree.
pub trait MemLeaf: Clone + Send + Sync + 'static {
    type Key: DataTrait + ?Sized;
    type Val: DataTrait + ?Sized;
    type Weight: WeightTrait + ?Sized;
    type Factories: Clone + Send + Sync + 'static;

    /// Whether each key has exactly one value, the unit value of a W-set.
    /// Cursors over such leaves follow the W-set cursors' contract, under
    /// which `map_values` leaves the cursor where it is.
    const ONE_VALUE_PER_KEY: bool;

    /// An empty leaf, whose memory counts in `memory`.
    fn new(factories: &Self::Factories, memory: &Arc<AtomicUsize>) -> Self;

    /// Brings the leaf's share of the leaf memory up to date. The tree calls
    /// it after every change of the leaf.
    fn account(&mut self);

    /// The number of key entries.
    fn entries(&self) -> usize;

    /// The number of `(key, value)` items.
    fn items(&self) -> usize;

    fn key(&self, entry: usize) -> &Self::Key;

    /// The items of a key entry.
    fn items_of(&self, entry: usize) -> Range<usize>;

    fn val(&self, item: usize) -> &Self::Val;

    fn weight(&self, item: usize) -> &Self::Weight;

    fn weight_mut(&mut self, item: usize) -> &mut Self::Weight;

    /// Finds where `(key, val)` is, or would go.
    fn search(&self, key: &Self::Key, val: &Self::Val) -> Slot;

    /// Inserts an item that `search` reported as absent, at `slot`.
    fn insert(&mut self, slot: Slot, key: &Self::Key, val: &Self::Val, weight: &Self::Weight);

    /// Removes `item` of `entry`, and the entry if it was the entry's last
    /// item. Returns whether the entry was removed.
    fn remove(&mut self, entry: usize, item: usize) -> bool;

    /// The bytes of the leaf's elements, which decide when it splits.
    fn inline_bytes(&self) -> usize;

    /// Moves the upper part of the leaf into a new leaf. The leaf must hold at
    /// least two items. Returns the new leaf and whether the split fell inside
    /// one key's values, in which case the key is the first entry of the new
    /// leaf and also the last entry of this one.
    fn split_off(&mut self) -> (Self, bool);

    /// A copy of the first `(key, value)`. The leaf must not be empty.
    fn first_bound(&self) -> Bound<Self::Key, Self::Val>;

    fn weight_factory(&self) -> &'static dyn Factory<Self::Weight>;

    /// A new weight, to hold the sum of a stored weight and a delta.
    fn new_weight(&self) -> Box<Self::Weight> {
        self.weight_factory().default_box()
    }
}

/// The first item whose pair is not less than `(key, val)`, in the layout of
/// keys with local offsets.
fn search_columns<K, V>(
    keys: &DynVec<K>,
    offs: &[usize],
    vals: &DynVec<V>,
    key: &K,
    val: &V,
) -> Slot
where
    K: DataTrait + ?Sized,
    V: DataTrait + ?Sized,
{
    let entry = keys.advance_to(0, keys.len(), key);
    if entry == keys.len() || keys.index(entry) != key {
        return Slot {
            entry,
            item: offs[entry],
            key_found: false,
            found: false,
        };
    }
    let (start, end) = (offs[entry], offs[entry + 1]);
    let item = start + vals.advance_to(start, end, val);
    Slot {
        entry,
        item,
        key_found: true,
        found: item < end && vals.index(item) == val,
    }
}

/// Where to split a leaf of `items` items: a key boundary near the middle if
/// there is one, else the middle of the only key.
fn split_point(offs: &[usize], items: usize) -> usize {
    let middle = items / 2;
    // The boundary at or below the middle, and the one above it.
    let below = offs.partition_point(|&off| off <= middle) - 1;
    let candidates = [offs[below], offs[below + 1]];
    candidates
        .into_iter()
        .filter(|&boundary| 0 < boundary && boundary < items)
        .min_by_key(|&boundary| boundary.abs_diff(middle))
        .unwrap_or(middle)
}

/// A leaf of an indexed W-set: keys, local offsets, values and weights.
pub struct IndexedLeaf<K, V, R>
where
    K: DataTrait + ?Sized,
    V: DataTrait + ?Sized,
    R: WeightTrait + ?Sized,
{
    factories: VecIndexedWSetFactories<K, V, R>,
    keys: Box<DynVec<K>>,
    /// `offs[e]..offs[e + 1]` are the items of entry `e`, so `offs` is one
    /// longer than `keys`.
    offs: Vec<usize>,
    vals: Box<DynVec<V>>,
    diffs: Box<DynVec<R>>,
    /// The heap bytes of the elements, beyond their inline size.
    heap_bytes: usize,
    memory: LeafMemory,
}

impl<K, V, R> Clone for IndexedLeaf<K, V, R>
where
    K: DataTrait + ?Sized,
    V: DataTrait + ?Sized,
    R: WeightTrait + ?Sized,
{
    fn clone(&self) -> Self {
        Self {
            factories: self.factories.clone(),
            keys: self.keys.clone(),
            offs: self.offs.clone(),
            vals: self.vals.clone(),
            diffs: self.diffs.clone(),
            heap_bytes: self.heap_bytes,
            memory: self.memory.clone(),
        }
    }
}

impl<K, V, R> IndexedLeaf<K, V, R>
where
    K: DataTrait + ?Sized,
    V: DataTrait + ?Sized,
    R: WeightTrait + ?Sized,
{
    /// Adds up the heap bytes of every element.
    fn recompute_heap(&mut self) {
        let layer = &self.factories.layer_factories;
        let (key_size, val_size, weight_size) = (
            layer.key.size_of(),
            layer.child.key.size_of(),
            layer.child.diff.size_of(),
        );
        self.heap_bytes = self
            .keys
            .dyn_iter()
            .map(|key| heap_of(key, key_size))
            .sum::<usize>()
            + self
                .vals
                .dyn_iter()
                .map(|val| heap_of(val, val_size))
                .sum::<usize>()
            + self
                .diffs
                .dyn_iter()
                .map(|weight| heap_of(weight, weight_size))
                .sum::<usize>();
    }
}

impl<K, V, R> MemLeaf for IndexedLeaf<K, V, R>
where
    K: DataTrait + ?Sized,
    V: DataTrait + ?Sized,
    R: WeightTrait + ?Sized,
{
    const ONE_VALUE_PER_KEY: bool = false;

    type Key = K;
    type Val = V;
    type Weight = R;
    type Factories = VecIndexedWSetFactories<K, V, R>;

    fn new(factories: &Self::Factories, memory: &Arc<AtomicUsize>) -> Self {
        let layer = &factories.layer_factories;
        let mut leaf = Self {
            factories: factories.clone(),
            keys: layer.keys.default_box(),
            offs: vec![0],
            vals: layer.child.keys.default_box(),
            diffs: layer.child.diffs.default_box(),
            heap_bytes: 0,
            memory: LeafMemory::new(memory),
        };
        leaf.account();
        leaf
    }

    fn account(&mut self) {
        let bytes = self.inline_bytes() + self.heap_bytes;
        self.memory.set(bytes);
    }

    fn entries(&self) -> usize {
        self.keys.len()
    }

    fn items(&self) -> usize {
        self.vals.len()
    }

    fn key(&self, entry: usize) -> &K {
        self.keys.index(entry)
    }

    fn items_of(&self, entry: usize) -> Range<usize> {
        self.offs[entry]..self.offs[entry + 1]
    }

    fn val(&self, item: usize) -> &V {
        self.vals.index(item)
    }

    fn weight(&self, item: usize) -> &R {
        self.diffs.index(item)
    }

    fn weight_mut(&mut self, item: usize) -> &mut R {
        self.diffs.index_mut(item)
    }

    fn search(&self, key: &K, val: &V) -> Slot {
        search_columns(&*self.keys, &self.offs, &*self.vals, key, val)
    }

    fn insert(&mut self, slot: Slot, key: &K, val: &V, weight: &R) {
        debug_assert!(!slot.found);
        if !slot.key_found {
            // The new entry has the one item at `slot.item`, and ends there.
            self.keys.insert_ref(slot.entry, key);
            self.offs.insert(slot.entry + 1, slot.item);
        }
        for off in &mut self.offs[slot.entry + 1..] {
            *off += 1;
        }
        self.vals.insert_ref(slot.item, val);
        self.diffs.insert_ref(slot.item, weight);

        // Measure the stored clones: a clone's heap can differ from the
        // caller's, as a `String` drops its spare capacity.
        let layer = &self.factories.layer_factories;
        self.heap_bytes += heap_of(self.vals.index(slot.item), layer.child.key.size_of())
            + heap_of(self.diffs.index(slot.item), layer.child.diff.size_of());
        if !slot.key_found {
            self.heap_bytes += heap_of(self.keys.index(slot.entry), layer.key.size_of());
        }
    }

    fn remove(&mut self, entry: usize, item: usize) -> bool {
        let layer = &self.factories.layer_factories;
        let mut freed = heap_of(self.vals.index(item), layer.child.key.size_of())
            + heap_of(self.diffs.index(item), layer.child.diff.size_of());
        self.vals.remove_at(item);
        self.diffs.remove_at(item);
        for off in &mut self.offs[entry + 1..] {
            *off -= 1;
        }
        let emptied = self.offs[entry] == self.offs[entry + 1];
        if emptied {
            freed += heap_of(self.keys.index(entry), layer.key.size_of());
            self.keys.remove_at(entry);
            self.offs.remove(entry + 1);
        }
        self.heap_bytes -= freed;
        emptied
    }

    fn inline_bytes(&self) -> usize {
        let layer = &self.factories.layer_factories;
        self.keys.len() * layer.key.size_of()
            + self.offs.len() * size_of::<usize>()
            + self.vals.len() * (layer.child.key.size_of() + layer.child.diff.size_of())
    }

    fn split_off(&mut self) -> (Self, bool) {
        debug_assert!(self.items() >= 2);
        let items = self.items();
        let at = split_point(&self.offs, items);

        // The entry holding item `at`. If `at` is inside its items, the entry
        // stays in this leaf and is repeated in the new one.
        let entry = self.offs.partition_point(|&off| off <= at) - 1;
        let inside = self.offs[entry] < at;

        let mut right = Self::new(&self.factories, self.memory.counter());
        if inside {
            right.keys.push_ref(self.keys.index(entry));
        }
        let first_moved_entry = if inside { entry + 1 } else { entry };
        let entries = self.keys.len();
        right
            .keys
            .append_range(&mut *self.keys, first_moved_entry, entries);
        right.offs = std::iter::once(0)
            .chain(self.offs[entry + 1..].iter().map(|&off| off - at))
            .collect();
        right.vals.append_range(&mut *self.vals, at, items);
        right.diffs.append_range(&mut *self.diffs, at, items);

        // This leaf now ends at item `at`.
        let kept_entries = if inside { entry + 1 } else { entry };
        self.keys.truncate(kept_entries);
        self.offs.truncate(kept_entries + 1);
        self.offs[kept_entries] = at;
        self.vals.truncate(at);
        self.diffs.truncate(at);
        self.recompute_heap();
        right.recompute_heap();
        self.account();
        right.account();
        (right, inside)
    }

    fn first_bound(&self) -> Bound<K, V> {
        Bound::new(self.keys.index(0), self.vals.index(0))
    }

    fn weight_factory(&self) -> &'static dyn Factory<R> {
        self.factories.layer_factories.child.diff
    }
}

/// A leaf of a W-set: keys and weights, with the unit as every value.
pub struct WSetLeaf<K, R>
where
    K: DataTrait + ?Sized,
    R: WeightTrait + ?Sized,
{
    factories: VecWSetFactories<K, R>,
    keys: Box<DynVec<K>>,
    diffs: Box<DynVec<R>>,
    /// The heap bytes of the elements, beyond their inline size.
    heap_bytes: usize,
    memory: LeafMemory,
}

impl<K, R> Clone for WSetLeaf<K, R>
where
    K: DataTrait + ?Sized,
    R: WeightTrait + ?Sized,
{
    fn clone(&self) -> Self {
        Self {
            factories: self.factories.clone(),
            keys: self.keys.clone(),
            diffs: self.diffs.clone(),
            heap_bytes: self.heap_bytes,
            memory: self.memory.clone(),
        }
    }
}

impl<K, R> WSetLeaf<K, R>
where
    K: DataTrait + ?Sized,
    R: WeightTrait + ?Sized,
{
    /// Adds up the heap bytes of every element.
    fn recompute_heap(&mut self) {
        let layer = &self.factories.layer_factories;
        let (key_size, weight_size) = (layer.key.size_of(), layer.diff.size_of());
        self.heap_bytes = self
            .keys
            .dyn_iter()
            .map(|key| heap_of(key, key_size))
            .sum::<usize>()
            + self
                .diffs
                .dyn_iter()
                .map(|weight| heap_of(weight, weight_size))
                .sum::<usize>();
    }
}

impl<K, R> MemLeaf for WSetLeaf<K, R>
where
    K: DataTrait + ?Sized,
    R: WeightTrait + ?Sized,
{
    const ONE_VALUE_PER_KEY: bool = true;

    type Key = K;
    type Val = DynUnit;
    type Weight = R;
    type Factories = VecWSetFactories<K, R>;

    fn new(factories: &Self::Factories, memory: &Arc<AtomicUsize>) -> Self {
        let mut leaf = Self {
            factories: factories.clone(),
            keys: factories.layer_factories.keys.default_box(),
            diffs: factories.layer_factories.diffs.default_box(),
            heap_bytes: 0,
            memory: LeafMemory::new(memory),
        };
        leaf.account();
        leaf
    }

    fn account(&mut self) {
        let bytes = self.inline_bytes() + self.heap_bytes;
        self.memory.set(bytes);
    }

    fn entries(&self) -> usize {
        self.keys.len()
    }

    fn items(&self) -> usize {
        self.keys.len()
    }

    fn key(&self, entry: usize) -> &K {
        self.keys.index(entry)
    }

    fn items_of(&self, entry: usize) -> Range<usize> {
        entry..entry + 1
    }

    fn val(&self, _item: usize) -> &DynUnit {
        &()
    }

    fn weight(&self, item: usize) -> &R {
        self.diffs.index(item)
    }

    fn weight_mut(&mut self, item: usize) -> &mut R {
        self.diffs.index_mut(item)
    }

    fn search(&self, key: &K, _val: &DynUnit) -> Slot {
        let entry = self.keys.advance_to(0, self.keys.len(), key);
        let found = entry < self.keys.len() && self.keys.index(entry) == key;
        Slot {
            entry,
            item: entry,
            key_found: found,
            found,
        }
    }

    fn insert(&mut self, slot: Slot, key: &K, _val: &DynUnit, weight: &R) {
        debug_assert!(!slot.found);
        self.keys.insert_ref(slot.entry, key);
        self.diffs.insert_ref(slot.entry, weight);
        // Measure the stored clones, as the indexed leaf does.
        let layer = &self.factories.layer_factories;
        self.heap_bytes += heap_of(self.keys.index(slot.entry), layer.key.size_of())
            + heap_of(self.diffs.index(slot.entry), layer.diff.size_of());
    }

    fn remove(&mut self, entry: usize, item: usize) -> bool {
        debug_assert_eq!(entry, item);
        let layer = &self.factories.layer_factories;
        self.heap_bytes -= heap_of(self.keys.index(entry), layer.key.size_of())
            + heap_of(self.diffs.index(entry), layer.diff.size_of());
        self.keys.remove_at(entry);
        self.diffs.remove_at(entry);
        true
    }

    fn inline_bytes(&self) -> usize {
        let layer = &self.factories.layer_factories;
        self.keys.len() * (layer.key.size_of() + layer.diff.size_of())
    }

    fn split_off(&mut self) -> (Self, bool) {
        debug_assert!(self.items() >= 2);
        let (len, at) = (self.keys.len(), self.keys.len() / 2);
        let mut right = Self::new(&self.factories, self.memory.counter());
        right.keys.append_range(&mut *self.keys, at, len);
        right.diffs.append_range(&mut *self.diffs, at, len);
        self.keys.truncate(at);
        self.diffs.truncate(at);
        self.recompute_heap();
        right.recompute_heap();
        self.account();
        right.account();
        (right, false)
    }

    fn first_bound(&self) -> Bound<K, DynUnit> {
        Bound {
            key: clone_box(self.keys.index(0)),
            val: Box::new(()),
        }
    }

    fn weight_factory(&self) -> &'static dyn Factory<R> {
        self.factories.layer_factories.diff
    }
}
