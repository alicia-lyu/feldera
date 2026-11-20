//! A B+-tree with copy-on-write nodes, after Rodeh, "B-trees, Shadowing, and
//! Clones" (ACM Transactions on Storage, 2008).
//!
//! The tree holds `(key, value, weight)` items in the order of `(key, value)`.
//! An inner node keeps, for each child, a copy of the child's first
//! `(key, value)` and the child's counts. A search descends by those bounds.
//!
//! An insert makes the nodes on its path mutable with [`Arc::make_mut`], which
//! copies a node only if some other handle still shares it. A
//! [`FrozenMemtable`] is such a handle: the root and the totals. So an insert
//! copies nothing unless a frozen memtable is held, and then it copies the nodes
//! of its path, once per node until the next freeze.

use super::leaf::{Bound, MemLeaf, Slot};
use crate::{
    DynZWeight, ZWeight,
    algebra::ZRingValue,
    dynamic::{ClonableTrait, Weight, WeightTrait},
    trace::Cursor,
};
use size_of::SizeOf;
use std::{
    any::TypeId,
    cmp::Ordering,
    fmt,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering as AtomicOrdering},
    },
    time::Instant,
};

/// The size limits of the tree's nodes.
#[derive(Clone, Copy, Debug)]
pub struct TreeShape {
    /// A leaf splits when the inline bytes of its elements pass this.
    pub leaf_bytes: usize,
    /// An inner node splits when it has more children than this.
    pub fanout: usize,
}

impl Default for TreeShape {
    fn default() -> Self {
        Self {
            leaf_bytes: Self::DEFAULT_LEAF_BYTES,
            fanout: 64,
        }
    }
}

impl TreeShape {
    /// The leaf split target of a memtable made without a shape.
    pub const DEFAULT_LEAF_BYTES: usize = 8 * 1024;

    /// The default shape with leaves that split at `leaf_bytes`.
    pub fn with_leaf_bytes(leaf_bytes: usize) -> Self {
        Self {
            leaf_bytes,
            ..Self::default()
        }
    }
}

/// The exact totals of a memtable's contents.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Totals {
    /// The `(key, value)` items.
    pub tuples: usize,
    /// The distinct keys. A key split between two leaves counts once.
    pub keys: usize,
    /// The items with a negative weight. Counted only for `ZWeight`, as the
    /// batch builders do.
    pub negative_weights: u64,
    /// The `size_of().total_bytes()` of every stored key entry, value, and
    /// weight. A key repeated by a split counts once per entry.
    pub bytes: usize,
}

/// The sizes of the subtree under one child.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Count {
    pub entries: usize,
    pub items: usize,
}

pub(super) enum Node<L: MemLeaf> {
    Inner(InnerNode<L>),
    Leaf(L),
}

pub(super) struct InnerNode<L: MemLeaf> {
    pub children: Vec<Arc<Node<L>>>,
    /// The first `(key, value)` of each child.
    pub bounds: Vec<Bound<L::Key, L::Val>>,
    pub counts: Vec<Count>,
}

impl<L: MemLeaf> Clone for InnerNode<L> {
    fn clone(&self) -> Self {
        Self {
            children: self.children.clone(),
            bounds: self.bounds.clone(),
            counts: self.counts.clone(),
        }
    }
}

impl<L: MemLeaf> Clone for Node<L> {
    fn clone(&self) -> Self {
        match self {
            Node::Inner(inner) => Node::Inner(inner.clone()),
            Node::Leaf(leaf) => Node::Leaf(leaf.clone()),
        }
    }
}

impl<L: MemLeaf> Node<L> {
    pub(super) fn count(&self) -> Count {
        match self {
            Node::Leaf(leaf) => Count {
                entries: leaf.entries(),
                items: leaf.items(),
            },
            Node::Inner(inner) => inner
                .counts
                .iter()
                .fold(Count::default(), |sum, count| Count {
                    entries: sum.entries + count.entries,
                    items: sum.items + count.items,
                }),
        }
    }

    /// The first `(key, value)` of a non-empty subtree.
    fn first_bound(&self) -> Bound<L::Key, L::Val> {
        match self {
            Node::Leaf(leaf) => leaf.first_bound(),
            Node::Inner(inner) => inner.bounds[0].clone(),
        }
    }

    /// The leftmost or rightmost leaf of this subtree.
    pub(super) fn edge_leaf(&self, rightmost: bool) -> &L {
        let mut node = self;
        loop {
            match node {
                Node::Leaf(leaf) => return leaf,
                Node::Inner(inner) => {
                    node = if rightmost {
                        inner.children.last().expect("inner nodes are not empty")
                    } else {
                        &inner.children[0]
                    }
                }
            }
        }
    }
}

impl<L: MemLeaf> InnerNode<L> {
    /// The child that holds, or would hold, `(key, val)`: the last one whose
    /// bound does not exceed it, or the first.
    pub(super) fn route(&self, key: &L::Key, val: &L::Val) -> usize {
        self.bounds[1..].partition_point(|bound| bound.cmp_to(key, val) != Ordering::Greater)
    }

    /// Updates this node after its child `index` changed by `outcome`.
    fn absorb(&mut self, index: usize, outcome: Outcome<L>, fanout: usize) -> Outcome<L> {
        let mut result = Outcome {
            d_entries: outcome.d_entries,
            d_items: outcome.d_items,
            ..Outcome::default()
        };
        if outcome.emptied {
            self.children.remove(index);
            self.bounds.remove(index);
            self.counts.remove(index);
            result.first_changed = index == 0;
            result.emptied = self.children.is_empty();
        } else {
            if outcome.first_changed {
                self.bounds[index] = self.children[index].first_bound();
                result.first_changed = index == 0;
            }
            if let Some(sibling) = outcome.split {
                // Recount the two halves. A split inside a key repeats that key
                // as the sibling's first entry, so the delta passed up comes from
                // the recount, not from `outcome`.
                let before = self.counts[index];
                let (left, right) = (self.children[index].count(), sibling.count());
                result.d_entries =
                    (left.entries + right.entries) as isize - before.entries as isize;
                result.d_items = (left.items + right.items) as isize - before.items as isize;
                self.counts[index] = left;
                self.bounds.insert(index + 1, sibling.first_bound());
                self.counts.insert(index + 1, right);
                self.children.insert(index + 1, sibling);
            } else {
                let count = &mut self.counts[index];
                count.entries = count.entries.checked_add_signed(outcome.d_entries).unwrap();
                count.items = count.items.checked_add_signed(outcome.d_items).unwrap();
            }
        }
        if self.children.len() > fanout {
            let middle = self.children.len() / 2;
            result.split = Some(Arc::new(Node::Inner(Self {
                children: self.children.split_off(middle),
                bounds: self.bounds.split_off(middle),
                counts: self.counts.split_off(middle),
            })));
        }
        result
    }
}

/// What a change to a subtree tells its parent.
struct Outcome<L: MemLeaf> {
    d_entries: isize,
    d_items: isize,
    /// The subtree's first `(key, value)` changed.
    first_changed: bool,
    /// The subtree has no items left, and its parent should drop it.
    emptied: bool,
    /// The subtree split, and this is the new right sibling.
    split: Option<Arc<Node<L>>>,
}

impl<L: MemLeaf> Default for Outcome<L> {
    fn default() -> Self {
        Self {
            d_entries: 0,
            d_items: 0,
            first_changed: false,
            emptied: false,
            split: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    /// Insert a new item.
    Insert,
    /// Add to the weight of an existing item, which stays non-zero.
    Update,
    /// Remove an existing item whose weight the delta cancels.
    Remove,
}

/// Everything an insert decides, from a read-only look at the tree.
struct Plan {
    action: Action,
    slot: Slot,
    key_delta: isize,
    negative_delta: i64,
    bytes_delta: isize,
}

/// The mutable accumulators of one insert.
struct Applied {
    node_copies: u64,
    /// The time spent copying those nodes.
    copy_nanos: u64,
    /// Bytes that splits inside a key added by repeating the key.
    repeated_key_bytes: usize,
    /// The bytes of an inserted item, measured on the stored clones.
    inserted_bytes: isize,
}

fn is_negative<R: WeightTrait + ?Sized>(weight: &R) -> bool {
    TypeId::of::<R>() == TypeId::of::<DynZWeight>()
        && !unsafe { weight.downcast::<ZWeight>() }.ge0()
}

fn bytes_of<T: SizeOf + ?Sized>(element: &T) -> isize {
    element.size_of().total_bytes() as isize
}

/// Makes `node` mutable, copying it if a frozen memtable or a cursor shares it.
fn make_mut<'a, L: MemLeaf>(node: &'a mut Arc<Node<L>>, applied: &mut Applied) -> &'a mut Node<L> {
    if Arc::strong_count(node) == 1 {
        return Arc::get_mut(node).expect("the node has no other handle");
    }
    let start = Instant::now();
    let copy = Arc::make_mut(node);
    applied.node_copies += 1;
    applied.copy_nanos += start.elapsed().as_nanos() as u64;
    copy
}

/// An immutable handle to a memtable's contents at one moment. It shares the
/// tree's nodes, so making it takes constant time, and later inserts into the
/// memtable copy any node it holds before changing it.
pub struct FrozenMemtable<L: MemLeaf> {
    pub(super) factories: L::Factories,
    pub(super) root: Arc<Node<L>>,
    pub(super) totals: Totals,
}

impl<L: MemLeaf> Clone for FrozenMemtable<L> {
    fn clone(&self) -> Self {
        Self {
            factories: self.factories.clone(),
            root: self.root.clone(),
            totals: self.totals,
        }
    }
}

impl<L: MemLeaf> SizeOf for FrozenMemtable<L> {
    fn size_of_children(&self, context: &mut size_of::Context) {
        // Only approximate, as for the batches that hold their elements.
        context.add(self.totals.bytes);
    }
}

impl<L: MemLeaf> fmt::Debug for FrozenMemtable<L> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FrozenMemtable")
            .field("totals", &self.totals)
            .finish_non_exhaustive()
    }
}

impl<L: MemLeaf> FrozenMemtable<L> {
    pub fn totals(&self) -> Totals {
        self.totals
    }

    pub fn is_empty(&self) -> bool {
        self.totals.tuples == 0
    }

    /// The smallest and largest key.
    pub fn key_bounds(&self) -> Option<(&L::Key, &L::Key)> {
        if self.is_empty() {
            return None;
        }
        let first = self.root.edge_leaf(false);
        let last = self.root.edge_leaf(true);
        Some((first.key(0), last.key(last.entries() - 1)))
    }

    /// Whether `self` and `other` share their root, and so every node.
    pub fn shares_root_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.root, &other.root)
    }
}

/// The mutable memtable: a B+-tree with copy-on-write nodes.
pub struct Memtable<L: MemLeaf> {
    factories: L::Factories,
    shape: TreeShape,
    root: Arc<Node<L>>,
    totals: Totals,
    node_copies: u64,
    copy_nanos: u64,
    /// The bytes of every live leaf, including the old copies that a frozen
    /// memtable or a cursor still holds.
    leaf_memory: Arc<AtomicUsize>,
}

impl<L: MemLeaf> fmt::Debug for Memtable<L> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Memtable")
            .field("totals", &self.totals)
            .field("node_copies", &self.node_copies)
            .finish_non_exhaustive()
    }
}

impl<L: MemLeaf> Memtable<L> {
    pub fn new(factories: &L::Factories) -> Self {
        Self::with_shape(factories, TreeShape::default())
    }

    pub fn with_shape(factories: &L::Factories, shape: TreeShape) -> Self {
        assert!(
            shape.fanout >= 2,
            "an inner node needs at least two children"
        );
        let leaf_memory = Arc::new(AtomicUsize::new(0));
        Self {
            factories: factories.clone(),
            shape,
            root: Arc::new(Node::Leaf(L::new(factories, &leaf_memory))),
            totals: Totals::default(),
            node_copies: 0,
            copy_nanos: 0,
            leaf_memory,
        }
    }

    pub fn totals(&self) -> Totals {
        self.totals
    }

    /// How many nodes inserts have copied because a frozen memtable or a cursor
    /// shared them.
    pub fn node_copies(&self) -> u64 {
        self.node_copies
    }

    /// The time inserts have spent copying shared nodes, in nanoseconds.
    pub fn copy_nanos(&self) -> u64 {
        self.copy_nanos
    }

    /// The bytes of every live leaf: elements, offsets and the heap bytes of
    /// the elements. A leaf that an insert replaced counts until the frozen
    /// memtable or cursor that holds it is dropped.
    pub fn live_leaf_bytes(&self) -> usize {
        self.leaf_memory.load(AtomicOrdering::Relaxed)
    }

    /// The counter behind [`Self::live_leaf_bytes`], for a budget that reads
    /// it without the memtable.
    pub fn leaf_memory(&self) -> &Arc<AtomicUsize> {
        &self.leaf_memory
    }

    /// Makes a frozen memtable of the current contents.
    pub fn freeze(&self) -> FrozenMemtable<L> {
        FrozenMemtable {
            factories: self.factories.clone(),
            root: self.root.clone(),
            totals: self.totals,
        }
    }

    /// The number of levels, a leaf-only tree having one.
    pub fn height(&self) -> usize {
        let mut node = &*self.root;
        let mut height = 1;
        while let Node::Inner(inner) = node {
            node = &inner.children[0];
            height += 1;
        }
        height
    }

    /// Adds `weight` to the weight of `(key, val)`, inserting it if it is new,
    /// and removing it if its weight becomes zero.
    pub fn insert(&mut self, key: &L::Key, val: &L::Val, weight: &L::Weight) {
        self.insert_with_finger(&mut Vec::new(), key, val, weight);
    }

    /// Inserts every item of the batch that `cursor` navigates, which is sorted.
    ///
    /// Successive items of a sorted batch mostly land in the leaf of the
    /// previous one, so the path to that leaf is kept as a finger and tried first.
    pub fn insert_batch<C>(&mut self, cursor: &mut C)
    where
        C: Cursor<L::Key, L::Val, (), L::Weight>,
    {
        let mut finger = Vec::new();
        while cursor.key_valid() {
            // `map_values` needs the cursor mutably, so it cannot lend the key.
            let key = dyn_clone::clone_box(cursor.key());
            cursor.map_values(&mut |val, weight| {
                self.insert_with_finger(&mut finger, &key, val, weight);
            });
            cursor.step_key();
        }
    }

    fn insert_with_finger(
        &mut self,
        finger: &mut Vec<usize>,
        key: &L::Key,
        val: &L::Val,
        weight: &L::Weight,
    ) {
        if weight.is_zero() {
            return;
        }
        let plan = self.plan(finger, key, val, weight);
        let mut applied = Applied {
            node_copies: 0,
            copy_nanos: 0,
            repeated_key_bytes: 0,
            inserted_bytes: 0,
        };
        let outcome = Self::apply(
            &mut self.root,
            finger,
            &plan,
            (key, val, weight),
            self.shape,
            &mut applied,
        );
        self.finish_root(outcome);

        self.node_copies += applied.node_copies;
        self.copy_nanos += applied.copy_nanos;
        let totals = &mut self.totals;
        totals.tuples = totals
            .tuples
            .checked_add_signed(match plan.action {
                Action::Insert => 1,
                Action::Update => 0,
                Action::Remove => -1,
            })
            .unwrap();
        totals.keys = totals.keys.checked_add_signed(plan.key_delta).unwrap();
        totals.negative_weights = totals
            .negative_weights
            .checked_add_signed(plan.negative_delta)
            .unwrap();
        totals.bytes = totals
            .bytes
            .checked_add_signed(
                plan.bytes_delta + applied.inserted_bytes + applied.repeated_key_bytes as isize,
            )
            .unwrap();
    }

    /// Looks at the tree, without changing it, to decide what the insert does and
    /// how it changes the totals. Leaves the path to the leaf in `path`.
    fn plan(&self, path: &mut Vec<usize>, key: &L::Key, val: &L::Val, weight: &L::Weight) -> Plan {
        let leaf = self.find_leaf(path, key, val);
        let slot = leaf.search(key, val);
        if !slot.found {
            // A new item. Its key is new to the tree unless the leaf has it, or
            // the key continues in the neighboring leaf.
            let new_key = !slot.key_found
                && !self.key_continues(path, key, slot.entry == 0, slot.entry == leaf.entries());
            return Plan {
                action: Action::Insert,
                slot,
                key_delta: isize::from(new_key),
                negative_delta: i64::from(is_negative(weight)),
                // Measured on the stored clones, once the leaf holds them.
                bytes_delta: 0,
            };
        }

        let stored = leaf.weight(slot.item);
        let mut sum = leaf.new_weight();
        stored.clone_to(&mut sum);
        sum.add_assign(weight);
        if !sum.is_zero() {
            return Plan {
                action: Action::Update,
                slot,
                key_delta: 0,
                negative_delta: i64::from(is_negative(&*sum)) - i64::from(is_negative(stored)),
                bytes_delta: 0,
            };
        }

        // The weight cancels, so the item goes, and with it the key's entry if
        // this was the entry's only value.
        let entry_goes = leaf.items_of(slot.entry).len() == 1;
        let key_goes = entry_goes
            && !self.key_continues(path, key, slot.entry == 0, slot.entry + 1 == leaf.entries());
        let key_bytes = if entry_goes {
            bytes_of(leaf.key(slot.entry))
        } else {
            0
        };
        Plan {
            action: Action::Remove,
            slot,
            key_delta: -isize::from(key_goes),
            negative_delta: -i64::from(is_negative(stored)),
            bytes_delta: -(key_bytes + bytes_of(leaf.val(slot.item)) + bytes_of(stored)),
        }
    }

    /// Whether `key` is also an entry of a neighboring leaf of `leaf`, which is
    /// at `path`. Only the entry at a leaf's start or end can continue in a
    /// neighbor.
    fn key_continues(&self, path: &[usize], key: &L::Key, at_start: bool, at_end: bool) -> bool {
        let in_previous = at_start
            && self.adjacent_leaf(path, false).is_some_and(|previous| {
                previous.entries() > 0 && previous.key(previous.entries() - 1) == key
            });
        let in_next = at_end
            && self
                .adjacent_leaf(path, true)
                .is_some_and(|next| next.entries() > 0 && next.key(0) == key);
        in_previous || in_next
    }

    /// The leaf before or after the leaf at `path`.
    fn adjacent_leaf(&self, path: &[usize], next: bool) -> Option<&L> {
        // The inner nodes down the path.
        let mut nodes = Vec::with_capacity(path.len());
        let mut node = &*self.root;
        for &index in path {
            let Node::Inner(inner) = node else {
                unreachable!("a path ends at a leaf")
            };
            nodes.push(inner);
            node = &inner.children[index];
        }
        // The deepest inner node where the path is not at the edge.
        for (inner, &index) in nodes.iter().zip(path).rev() {
            if next && index + 1 < inner.children.len() {
                return Some(inner.children[index + 1].edge_leaf(false));
            }
            if !next && index > 0 {
                return Some(inner.children[index - 1].edge_leaf(true));
            }
        }
        None
    }

    /// The leaf that holds, or would hold, `(key, val)`. `path` is the finger: a
    /// path to try first. It is replaced by the path to the leaf found.
    fn find_leaf(&self, path: &mut Vec<usize>, key: &L::Key, val: &L::Val) -> &L {
        if let Some(leaf) = self.leaf_covering(path, key, val) {
            return leaf;
        }
        path.clear();
        let mut node = &*self.root;
        loop {
            match node {
                Node::Leaf(leaf) => return leaf,
                Node::Inner(inner) => {
                    let index = inner.route(key, val);
                    path.push(index);
                    node = &inner.children[index];
                }
            }
        }
    }

    /// The leaf at `path`, if the path is valid in the current tree and the leaf
    /// covers `(key, val)`: it starts at or before, and the next leaf starts after.
    fn leaf_covering(&self, path: &[usize], key: &L::Key, val: &L::Val) -> Option<&L> {
        let mut node = &*self.root;
        let mut lower: Option<&Bound<L::Key, L::Val>> = None;
        let mut upper: Option<&Bound<L::Key, L::Val>> = None;
        for &index in path {
            let Node::Inner(inner) = node else {
                return None;
            };
            if index >= inner.children.len() {
                return None;
            }
            if index > 0 {
                lower = Some(&inner.bounds[index]);
            }
            if index + 1 < inner.children.len() {
                upper = Some(&inner.bounds[index + 1]);
            }
            node = &inner.children[index];
        }
        let Node::Leaf(leaf) = node else {
            return None;
        };
        let after_lower = lower.is_none_or(|bound| bound.cmp_to(key, val) != Ordering::Greater);
        let before_upper = upper.is_none_or(|bound| bound.cmp_to(key, val) == Ordering::Greater);
        (after_lower && before_upper).then_some(leaf)
    }

    /// Changes the leaf at the end of `path`, making the nodes on the path
    /// mutable, and fixes the bounds, counts and splits on the way back up.
    fn apply(
        node: &mut Arc<Node<L>>,
        path: &[usize],
        plan: &Plan,
        item: (&L::Key, &L::Val, &L::Weight),
        shape: TreeShape,
        applied: &mut Applied,
    ) -> Outcome<L> {
        match make_mut(node, applied) {
            Node::Inner(inner) => {
                let index = path[0];
                let outcome = Self::apply(
                    &mut inner.children[index],
                    &path[1..],
                    plan,
                    item,
                    shape,
                    applied,
                );
                inner.absorb(index, outcome, shape.fanout)
            }
            Node::Leaf(leaf) => Self::apply_to_leaf(leaf, plan, item, shape, applied),
        }
    }

    fn apply_to_leaf(
        leaf: &mut L,
        plan: &Plan,
        (key, val, weight): (&L::Key, &L::Val, &L::Weight),
        shape: TreeShape,
        applied: &mut Applied,
    ) -> Outcome<L> {
        let (entries, items) = (leaf.entries() as isize, leaf.items() as isize);
        let slot = plan.slot;
        match plan.action {
            Action::Insert => {
                leaf.insert(slot, key, val, weight);
                applied.inserted_bytes =
                    bytes_of(leaf.val(slot.item)) + bytes_of(leaf.weight(slot.item));
                if !slot.key_found {
                    applied.inserted_bytes += bytes_of(leaf.key(slot.entry));
                }
            }
            Action::Update => leaf.weight_mut(slot.item).add_assign(weight),
            Action::Remove => {
                leaf.remove(slot.entry, slot.item);
            }
        }
        leaf.account();
        let mut outcome = Outcome {
            d_entries: leaf.entries() as isize - entries,
            d_items: leaf.items() as isize - items,
            first_changed: slot.item == 0 && plan.action != Action::Update,
            emptied: leaf.items() == 0,
            split: None,
        };
        if leaf.items() >= 2 && leaf.inline_bytes() > shape.leaf_bytes {
            let (right, inside_key) = leaf.split_off();
            if inside_key {
                applied.repeated_key_bytes += bytes_of(right.key(0)) as usize;
            }
            outcome.split = Some(Arc::new(Node::Leaf(right)));
        }
        outcome
    }

    /// Grows the tree at the top for a root split, and shrinks it for a root
    /// left with one child or none.
    fn finish_root(&mut self, outcome: Outcome<L>) {
        if let Some(right) = outcome.split {
            let left = self.root.clone();
            let new_root = Node::Inner(InnerNode {
                bounds: vec![left.first_bound(), right.first_bound()],
                counts: vec![left.count(), right.count()],
                children: vec![left, right],
            });
            self.root = Arc::new(new_root);
            return;
        }
        if outcome.emptied {
            if matches!(*self.root, Node::Inner(_)) {
                self.root = Arc::new(Node::Leaf(L::new(&self.factories, &self.leaf_memory)));
            }
            return;
        }
        loop {
            let only_child = match &*self.root {
                Node::Inner(inner) if inner.children.len() == 1 => inner.children[0].clone(),
                _ => break,
            };
            self.root = only_child;
        }
    }
}

/// The items under `node` in order, as owned copies.
#[cfg(test)]
pub(super) fn collect_items<L: MemLeaf>(
    node: &Node<L>,
    items: &mut Vec<(Box<L::Key>, Box<L::Val>, Box<L::Weight>)>,
) {
    use dyn_clone::clone_box;

    match node {
        Node::Leaf(leaf) => {
            for entry in 0..leaf.entries() {
                for item in leaf.items_of(entry) {
                    items.push((
                        clone_box(leaf.key(entry)),
                        clone_box(leaf.val(item)),
                        clone_box(leaf.weight(item)),
                    ));
                }
            }
        }
        Node::Inner(inner) => {
            for child in &inner.children {
                collect_items(child, items);
            }
        }
    }
}

#[cfg(test)]
impl<L: MemLeaf> FrozenMemtable<L> {
    pub(super) fn items(&self) -> Vec<(Box<L::Key>, Box<L::Val>, Box<L::Weight>)> {
        let mut items = Vec::new();
        collect_items(&self.root, &mut items);
        items
    }
}

#[cfg(test)]
impl<L: MemLeaf> Memtable<L> {
    /// The inline bytes of the leaves of the current tree, summed by a walk.
    pub(super) fn current_inline_leaf_bytes(&self) -> usize {
        fn walk<L: MemLeaf>(node: &Node<L>) -> usize {
            match node {
                Node::Leaf(leaf) => leaf.inline_bytes(),
                Node::Inner(inner) => inner.children.iter().map(|child| walk(child)).sum(),
            }
        }
        walk(&self.root)
    }

    /// Checks every structural invariant and that the totals are exact, and
    /// returns the items in order.
    pub(super) fn validate(&self) -> Vec<(Box<L::Key>, Box<L::Val>, Box<L::Weight>)> {
        /// What the walk adds up from the leaves.
        #[derive(Default)]
        struct Walk {
            leaf_depth: Option<usize>,
            bytes: isize,
            leaf_count: usize,
        }

        fn walk<L: MemLeaf>(
            node: &Node<L>,
            depth: usize,
            fanout: usize,
            state: &mut Walk,
            items: &mut Vec<(Box<L::Key>, Box<L::Val>, Box<L::Weight>)>,
        ) -> Count {
            use dyn_clone::clone_box;

            match node {
                Node::Leaf(leaf) => {
                    assert_eq!(
                        *state.leaf_depth.get_or_insert(depth),
                        depth,
                        "uneven depth"
                    );
                    state.leaf_count += 1;
                    let mut seen_items = 0;
                    for entry in 0..leaf.entries() {
                        let range = leaf.items_of(entry);
                        assert_eq!(range.start, seen_items, "offsets are contiguous");
                        assert!(!range.is_empty(), "an entry has an item");
                        if entry > 0 {
                            assert!(leaf.key(entry - 1) < leaf.key(entry), "keys ascend");
                        }
                        state.bytes += bytes_of(leaf.key(entry));
                        for item in range {
                            assert!(!leaf.weight(item).is_zero(), "no zero weights");
                            state.bytes += bytes_of(leaf.val(item)) + bytes_of(leaf.weight(item));
                            items.push((
                                clone_box(leaf.key(entry)),
                                clone_box(leaf.val(item)),
                                clone_box(leaf.weight(item)),
                            ));
                            seen_items += 1;
                        }
                    }
                    assert_eq!(seen_items, leaf.items());
                    Count {
                        entries: leaf.entries(),
                        items: leaf.items(),
                    }
                }
                Node::Inner(inner) => {
                    assert!(!inner.children.is_empty() && inner.children.len() <= fanout);
                    assert!(inner.children.len() >= 2 || depth > 0, "no one-child root");
                    assert_eq!(inner.children.len(), inner.bounds.len());
                    assert_eq!(inner.children.len(), inner.counts.len());
                    let mut total = Count::default();
                    for (index, child) in inner.children.iter().enumerate() {
                        let first_item = items.len();
                        let count = walk(child, depth + 1, fanout, state, items);
                        assert!(count.items > 0, "no empty children");
                        assert_eq!(count, inner.counts[index], "counts are exact");
                        let (key, val, _) = &items[first_item];
                        assert_eq!(
                            inner.bounds[index].cmp_to(key, val),
                            Ordering::Equal,
                            "a bound is the child's first item"
                        );
                        total.entries += count.entries;
                        total.items += count.items;
                    }
                    total
                }
            }
        }

        let mut state = Walk::default();
        let mut items = Vec::new();
        walk(&self.root, 0, self.shape.fanout, &mut state, &mut items);
        for pair in items.windows(2) {
            let ordering = pair[0]
                .0
                .cmp(&pair[1].0)
                .then_with(|| pair[0].1.cmp(&pair[1].1));
            assert_eq!(ordering, Ordering::Less, "items ascend");
        }

        // The totals are what the items add up to, with a key split between
        // leaves counted once.
        let mut keys = 0;
        for (index, (key, _, _)) in items.iter().enumerate() {
            if index == 0 || *items[index - 1].0 != **key {
                keys += 1;
            }
        }
        let negative_weights = items
            .iter()
            .filter(|(_, _, weight)| is_negative(&**weight))
            .count();
        assert_eq!(
            self.totals,
            Totals {
                tuples: items.len(),
                keys,
                negative_weights: negative_weights as u64,
                bytes: state.bytes as usize,
            }
        );
        items
    }
}
