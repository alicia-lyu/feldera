//! The cursor over a frozen memtable.
//!
//! The cursor stands on a *key group*: the entries of one key, which are one
//! entry of one leaf unless the key's values were split between leaves. It
//! stands on the group's first entry, and joins the later entries into one
//! sequence of values, so a reader sees the key once.
//!
//! The cursor holds its leaf by `Arc`, and finds the neighboring leaves by
//! walking from the root with a path of child indexes, since leaves are not
//! linked.

use super::{
    leaf::MemLeaf,
    tree::{FrozenMemtable, InnerNode, Node, Totals},
};
use crate::{
    dynamic::{DynVec, Factory},
    trace::{
        Cursor, Filter, GroupFilter, MergeCursor,
        cursor::{
            FilteredMergeCursor, Position, UnfilteredMergeCursor,
            run_reads::{
                count_memory_key_read, count_memory_value_read, count_memory_weight_read,
                count_seek,
            },
        },
        ord::vec::indexed_wset_batch::Landing,
    },
};
use rand::{Rng, seq::index::sample as sample_indexes};

// Reads are counted as the in-memory cursor of the same kind counts them:
// `VecIndexedWSetCursor` counts them, and `VecWSetCursor` counts none.
fn count_seek_of<L: MemLeaf>() {
    if !L::ONE_VALUE_PER_KEY {
        count_seek();
    }
}

fn count_key_read<L: MemLeaf>() {
    if !L::ONE_VALUE_PER_KEY {
        count_memory_key_read();
    }
}

fn count_value_read<L: MemLeaf>() {
    if !L::ONE_VALUE_PER_KEY {
        count_memory_value_read();
    }
}

fn count_weight_read<L: MemLeaf>() {
    if !L::ONE_VALUE_PER_KEY {
        count_memory_weight_read();
    }
}
use std::{cell::Cell, fmt, ops::Range, sync::Arc};

/// The first index of `range` at which `before` is false, given that it is
/// true on a prefix of the range.
fn partition(range: Range<usize>, before: impl Fn(usize) -> bool) -> usize {
    let (mut low, mut high) = (range.start, range.end);
    while low < high {
        let middle = low + (high - low) / 2;
        if before(middle) {
            low = middle + 1;
        } else {
            high = middle;
        }
    }
    low
}

/// A leaf node's leaf.
fn leaf_of<L: MemLeaf>(node: &Arc<Node<L>>) -> &L {
    match &**node {
        Node::Leaf(leaf) => leaf,
        Node::Inner(_) => unreachable!("a cursor stands on leaves"),
    }
}

/// Descends from the root, taking at each inner node the child that `choose` picks.
/// Returns the path of child indexes and the leaf.
fn descend<L: MemLeaf>(
    root: &Arc<Node<L>>,
    mut choose: impl FnMut(&InnerNode<L>) -> usize,
) -> (Vec<usize>, Arc<Node<L>>) {
    let mut path = Vec::new();
    let mut node = root;
    while let Node::Inner(inner) = &**node {
        let index = choose(inner);
        path.push(index);
        node = &inner.children[index];
    }
    (path, node.clone())
}

/// Moves `path` to the leaf before or after the one it names, and returns
/// that leaf. Leaves `path` alone and returns `None` at the edge of the tree.
fn adjacent<L: MemLeaf>(
    root: &Arc<Node<L>>,
    path: &mut Vec<usize>,
    forward: bool,
) -> Option<Arc<Node<L>>> {
    // The inner nodes down the path.
    let mut nodes: Vec<&InnerNode<L>> = Vec::with_capacity(path.len());
    let mut node = root;
    for &index in path.iter() {
        let Node::Inner(inner) = &**node else {
            unreachable!("a path ends at a leaf")
        };
        nodes.push(inner);
        node = &inner.children[index];
    }
    // The deepest level where the path has a sibling on the wanted side.
    for (level, inner) in nodes.iter().enumerate().rev() {
        let index = path[level];
        let sibling = if forward {
            (index + 1 < inner.children.len()).then_some(index + 1)
        } else {
            index.checked_sub(1)
        };
        let Some(sibling) = sibling else { continue };
        path.truncate(level);
        path.push(sibling);
        // Down the sibling's near edge.
        let mut node = &inner.children[sibling];
        while let Node::Inner(below) = &**node {
            let edge = if forward { 0 } else { below.children.len() - 1 };
            path.push(edge);
            node = &below.children[edge];
        }
        return Some(node.clone());
    }
    None
}

/// A later part of the current key group, in another leaf.
struct Hop<L: MemLeaf> {
    path: Vec<usize>,
    /// The leaf, whose first entry is the key.
    leaf: Arc<Node<L>>,
}

impl<L: MemLeaf> Clone for Hop<L> {
    fn clone(&self) -> Self {
        Self {
            path: self.path.clone(),
            leaf: self.leaf.clone(),
        }
    }
}

/// A cursor over a [`FrozenMemtable`].
///
/// It counts `memory_*` reads by the rules of
/// [`VecIndexedWSetCursor`](crate::trace::ord::vec::indexed_wset_batch::VecIndexedWSetCursor):
/// landing on a key reads the key, the first read of a value at a position
/// reads the value, and each weight returned reads a weight.
pub struct MemtableCursor<L: MemLeaf> {
    root: Arc<Node<L>>,
    totals: Totals,
    weight_factory: &'static dyn Factory<L::Weight>,

    /// The child index at each inner level, down to the key's leaf.
    path: Vec<usize>,
    /// The leaf of the key group's first entry.
    leaf: Arc<Node<L>>,
    entry: usize,
    key_valid: bool,

    /// The leaf of the value's chunk of the group when it is not `leaf`.
    hop: Option<Hop<L>>,
    /// The value's item in the chunk's leaf.
    item: usize,
    val_valid: bool,

    /// Whether the value at the key and value position was counted as read.
    /// Every key move clears it, and every value move sets it.
    value_counted: Cell<bool>,
}

impl<L: MemLeaf> Clone for MemtableCursor<L> {
    fn clone(&self) -> Self {
        Self {
            root: self.root.clone(),
            totals: self.totals,
            weight_factory: self.weight_factory,
            path: self.path.clone(),
            leaf: self.leaf.clone(),
            entry: self.entry,
            key_valid: self.key_valid,
            hop: self.hop.clone(),
            item: self.item,
            val_valid: self.val_valid,
            value_counted: self.value_counted.clone(),
        }
    }
}

impl<L: MemLeaf> fmt::Debug for MemtableCursor<L> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemtableCursor")
            .field("key_valid", &self.key_valid)
            .field("entry", &self.entry)
            .field("val_valid", &self.val_valid)
            .field("item", &self.item)
            .finish_non_exhaustive()
    }
}

impl<L: MemLeaf> FrozenMemtable<L> {
    pub fn cursor(&self) -> MemtableCursor<L> {
        MemtableCursor::new(self)
    }

    /// A cursor for merging, filtered as
    /// [`BatchReader::merge_cursor`](crate::trace::BatchReader::merge_cursor) filters.
    pub fn merge_cursor(
        &self,
        key_filter: Option<Filter<L::Key>>,
        value_filter: Option<GroupFilter<L::Val>>,
    ) -> Box<dyn MergeCursor<L::Key, L::Val, (), L::Weight> + Send> {
        if key_filter.is_none() && value_filter.is_none() {
            Box::new(UnfilteredMergeCursor::new(self.cursor()))
        } else if let Some(GroupFilter::Simple(filter)) = value_filter {
            Box::new(FilteredMergeCursor::new(
                self.cursor(),
                key_filter,
                Some(filter),
            ))
        } else {
            // Other forms of group filters need a snapshot of the trace.
            Box::new(FilteredMergeCursor::new(self.cursor(), key_filter, None))
        }
    }

    /// The key of the `rank`-th key entry in the order of the tree.
    fn key_at_entry(&self, mut rank: usize) -> &L::Key {
        let mut node = &*self.root;
        loop {
            match node {
                Node::Leaf(leaf) => return leaf.key(rank),
                Node::Inner(inner) => {
                    let child = inner
                        .counts
                        .iter()
                        .position(|count| {
                            if rank < count.entries {
                                true
                            } else {
                                rank -= count.entries;
                                false
                            }
                        })
                        .expect("the rank is within the counts");
                    node = &inner.children[child];
                }
            }
        }
    }

    /// Samples keys as [`BatchReader::sample_keys`](crate::trace::BatchReader::sample_keys)
    /// requires: all keys if there are no more than `sample_size`, else a
    /// uniform sample of key entries. A key split between two leaves has two
    /// entries, and so is slightly likelier to be drawn. Reads the tree without
    /// counting reads.
    pub fn sample_keys<RG: Rng>(
        &self,
        rng: &mut RG,
        sample_size: usize,
        sample: &mut DynVec<L::Key>,
    ) {
        let entries = self.root.count().entries;
        let mut push = |key: &L::Key| {
            if sample.last() != Some(key) {
                sample.push_ref(key);
            }
        };
        if self.totals.keys <= sample_size {
            for rank in 0..entries {
                push(self.key_at_entry(rank));
            }
        } else {
            let mut ranks = sample_indexes(rng, entries, sample_size).into_vec();
            ranks.sort_unstable();
            for rank in ranks {
                push(self.key_at_entry(rank));
            }
        }
    }
}

impl<L: MemLeaf> MemtableCursor<L> {
    /// Opening a cursor reads the first key, if there is one. Its first value
    /// waits for the first read of a value.
    pub fn new(frozen: &FrozenMemtable<L>) -> Self {
        let (path, leaf) = descend(&frozen.root, |_| 0);
        let mut cursor = Self {
            root: frozen.root.clone(),
            totals: frozen.totals,
            weight_factory: leaf_of(&leaf).weight_factory(),
            path,
            leaf,
            entry: 0,
            key_valid: false,
            hop: None,
            item: 0,
            val_valid: false,
            value_counted: Cell::new(false),
        };
        cursor.key_valid = cursor.leaf().entries() > 0;
        cursor.reset_vals();
        if cursor.key_valid {
            count_key_read::<L>();
        }
        cursor
    }

    fn leaf(&self) -> &L {
        leaf_of(&self.leaf)
    }

    /// The leaf and entry of the value's chunk of the group.
    fn chunk(&self) -> (&L, usize) {
        match &self.hop {
            None => (self.leaf(), self.entry),
            Some(hop) => (leaf_of(&hop.leaf), 0),
        }
    }

    fn chunk_items(&self) -> Range<usize> {
        let (leaf, entry) = self.chunk();
        leaf.items_of(entry)
    }

    /// Puts the value cursor on the key's first value.
    fn reset_vals(&mut self) {
        self.hop = None;
        self.val_valid = self.key_valid;
        self.item = if self.key_valid {
            self.leaf().items_of(self.entry).start
        } else {
            0
        };
    }

    /// Stands on `entry` of `leaf`, and on its first value.
    fn stand_on(&mut self, path: Vec<usize>, leaf: Arc<Node<L>>, entry: usize) {
        self.path = path;
        self.leaf = leaf;
        self.entry = entry;
        self.key_valid = true;
        self.reset_vals();
    }

    fn become_invalid(&mut self) {
        self.key_valid = false;
        self.reset_vals();
    }

    /// Identifies the key position, to tell whether a move changed it.
    fn key_position(&self) -> (usize, usize) {
        (Arc::as_ptr(&self.leaf) as usize, self.entry)
    }

    fn val_position(&self) -> (usize, usize) {
        let leaf = match &self.hop {
            None => &self.leaf,
            Some(hop) => &hop.leaf,
        };
        (Arc::as_ptr(leaf) as usize, self.item)
    }

    // --- Key moves, which do not count reads.

    /// Moves from the last entry of the group to the first entry of the next key.
    fn advance_key(&mut self) {
        if !self.key_valid {
            return;
        }
        let entries = self.leaf().entries();
        if self.entry + 1 < entries {
            self.entry += 1;
            self.reset_vals();
            return;
        }
        // Past the leaf's last entry: skip the leaves that continue the group.
        let mut path = self.path.clone();
        loop {
            let Some(next) = adjacent(&self.root, &mut path, true) else {
                self.become_invalid();
                return;
            };
            let next_leaf = leaf_of(&next);
            if next_leaf.key(0) != self.leaf().key(self.entry) {
                self.stand_on(path, next, 0);
                return;
            }
            if next_leaf.entries() > 1 {
                self.stand_on(path, next, 1);
                return;
            }
        }
    }

    /// Moves a cursor that stands on an entry to the first entry of its group.
    fn move_to_group_start(&mut self) {
        while self.entry == 0 {
            let mut path = self.path.clone();
            let Some(previous) = adjacent(&self.root, &mut path, false) else {
                return;
            };
            let previous_leaf = leaf_of(&previous);
            let last = previous_leaf.entries() - 1;
            if previous_leaf.key(last) != self.leaf().key(0) {
                return;
            }
            self.path = path;
            self.leaf = previous;
            self.entry = last;
        }
    }

    fn retreat_key(&mut self) {
        if !self.key_valid {
            return;
        }
        if self.entry > 0 {
            self.entry -= 1;
        } else {
            let mut path = self.path.clone();
            let Some(previous) = adjacent(&self.root, &mut path, false) else {
                self.become_invalid();
                return;
            };
            self.entry = leaf_of(&previous).entries() - 1;
            self.path = path;
            self.leaf = previous;
        }
        self.move_to_group_start();
        self.reset_vals();
    }

    fn rewind_key(&mut self) {
        let (path, leaf) = descend(&self.root, |_| 0);
        self.path = path;
        self.leaf = leaf;
        self.entry = 0;
        self.key_valid = self.leaf().entries() > 0;
        self.reset_vals();
    }

    fn fast_forward_key(&mut self) {
        let (path, leaf) = descend(&self.root, |inner| inner.children.len() - 1);
        self.path = path;
        self.leaf = leaf;
        self.key_valid = self.leaf().entries() > 0;
        if self.key_valid {
            self.entry = self.leaf().entries() - 1;
            self.move_to_group_start();
        }
        self.reset_vals();
    }

    /// Moves forward to the first key for which `before` is false, given that
    /// it is true on a prefix of the keys. Does nothing from a key that already
    /// satisfies that.
    fn seek_forward(&mut self, before: &dyn Fn(&L::Key) -> bool) {
        if !self.key_valid || !before(self.leaf().key(self.entry)) {
            return;
        }
        let leaf = self.leaf();
        let entries = leaf.entries();
        if !before(leaf.key(entries - 1)) {
            // The answer is in this leaf.
            self.entry = partition(self.entry..entries, |entry| before(leaf.key(entry)));
            self.reset_vals();
            return;
        }
        // Take the last child whose first key is still before the target; the
        // answer is in it or at the start of the next leaf.
        let (mut path, found) = descend(&self.root, |inner| {
            inner.bounds[1..].partition_point(|bound| before(&*bound.key))
        });
        let found_leaf = leaf_of(&found);
        let entry = partition(0..found_leaf.entries(), |entry| {
            before(found_leaf.key(entry))
        });
        if entry < found_leaf.entries() {
            self.stand_on(path, found, entry);
        } else if let Some(next) = adjacent(&self.root, &mut path, true) {
            self.stand_on(path, next, 0);
        } else {
            self.become_invalid();
        }
    }

    /// Moves backward to the last key for which `keep` is true, given that it
    /// is true on a prefix of the keys. Does nothing from a key that already
    /// satisfies it.
    fn seek_backward(&mut self, keep: &dyn Fn(&L::Key) -> bool) {
        if !self.key_valid || keep(self.leaf().key(self.entry)) {
            return;
        }
        let (path, found) = descend(&self.root, |inner| {
            inner.bounds[1..].partition_point(|bound| keep(&*bound.key))
        });
        let found_leaf = leaf_of(&found);
        let after = partition(0..found_leaf.entries(), |entry| keep(found_leaf.key(entry)));
        if after == 0 {
            // Not even the tree's first key satisfies it.
            self.become_invalid();
            return;
        }
        self.path = path;
        self.leaf = found;
        self.entry = after - 1;
        self.move_to_group_start();
        self.reset_vals();
    }

    // --- Value moves, which do not count reads.

    /// The chunk after the current one, if the group continues.
    fn next_chunk(&self) -> Option<Hop<L>> {
        let (leaf, entry) = self.chunk();
        if entry + 1 != leaf.entries() {
            return None;
        }
        let mut path = match &self.hop {
            None => self.path.clone(),
            Some(hop) => hop.path.clone(),
        };
        let key = self.leaf().key(self.entry);
        let next = adjacent(&self.root, &mut path, true)?;
        (leaf_of(&next).key(0) == key).then_some(Hop { path, leaf: next })
    }

    /// Moves the value cursor to the chunk before the current one, onto its
    /// last value. Returns whether there was one.
    fn move_to_previous_chunk(&mut self) -> bool {
        let Some(hop) = &self.hop else {
            return false;
        };
        let mut path = hop.path.clone();
        let previous = adjacent(&self.root, &mut path, false).expect("a hop has a leaf before it");
        if Arc::ptr_eq(&previous, &self.leaf) {
            self.hop = None;
        } else {
            self.hop = Some(Hop {
                path,
                leaf: previous,
            });
        }
        self.item = self.chunk_items().end - 1;
        true
    }

    fn move_to_next_chunk(&mut self) -> bool {
        match self.next_chunk() {
            Some(hop) => {
                self.item = leaf_of(&hop.leaf).items_of(0).start;
                self.hop = Some(hop);
                true
            }
            None => false,
        }
    }

    /// Moves forward to the first value for which `before` is false, given
    /// that it is true on a prefix of the group's values.
    fn seek_val_forward(&mut self, before: &dyn Fn(&L::Val) -> bool) {
        while self.val_valid {
            let range = self.chunk_items();
            let (leaf, _) = self.chunk();
            let found = partition(self.item..range.end, |item| before(leaf.val(item)));
            if found < range.end {
                self.item = found;
                return;
            }
            if !self.move_to_next_chunk() {
                self.item = range.end;
                self.val_valid = false;
            }
        }
    }

    /// Moves backward to the last value for which `keep` is true, given that
    /// it is true on a prefix of the group's values.
    fn seek_val_backward(&mut self, keep: &dyn Fn(&L::Val) -> bool) {
        while self.val_valid {
            let range = self.chunk_items();
            let (leaf, _) = self.chunk();
            let after = partition(range.start..self.item + 1, |item| keep(leaf.val(item)));
            if after > range.start {
                self.item = after - 1;
                return;
            }
            if !self.move_to_previous_chunk() {
                self.item = range.start;
                self.val_valid = false;
            }
        }
    }

    // --- Moves with their read counts.

    /// Moves the key cursor with `op`, and counts the key read that `landing`
    /// says `op` makes. The new key's first value has not been read.
    fn move_key(&mut self, landing: Landing, op: impl FnOnce(&mut Self)) {
        let start = self.key_position();
        op(self);
        // Every key operation puts the value cursor on the first value, even
        // a seek that stays on its key.
        self.reset_vals();
        self.value_counted.set(false);
        let end = self.key_position();
        if landing.reads(start, end, self.key_valid) {
            count_key_read::<L>();
        }
    }

    /// Counts the read of the current key's value at its first access after a
    /// key move. A key that is not valid has no value to read.
    fn read_value(&self) {
        if !self.value_counted.get() && self.key_valid && self.val_valid {
            self.value_counted.set(true);
            count_value_read::<L>();
        }
    }

    /// Moves the value cursor with `op`. The first read of a value counts
    /// before the move; `landing` says whether the move reads the value it
    /// lands on.
    fn move_val(&mut self, landing: Landing, op: impl FnOnce(&mut Self)) {
        self.read_value();
        let start = self.val_position();
        op(self);
        self.value_counted.set(true);
        if self.key_valid && landing.reads(start, self.val_position(), self.val_valid) {
            count_value_read::<L>();
        }
    }

    /// The entries before the current one, in the order of the tree. This is
    /// the position of the key unless a key is split between leaves.
    fn entries_before(&self) -> usize {
        let mut entries = self.entry;
        let mut node = &self.root;
        for &index in &self.path {
            let Node::Inner(inner) = &**node else {
                unreachable!("a path ends at a leaf")
            };
            entries += inner.counts[..index]
                .iter()
                .map(|count| count.entries)
                .sum::<usize>();
            node = &inner.children[index];
        }
        entries
    }
}

impl<L: MemLeaf> Cursor<L::Key, L::Val, (), L::Weight> for MemtableCursor<L> {
    fn weight_factory(&self) -> &'static dyn Factory<L::Weight> {
        self.weight_factory
    }

    fn key_valid(&self) -> bool {
        self.key_valid
    }

    fn val_valid(&self) -> bool {
        self.read_value();
        self.val_valid
    }

    fn key(&self) -> &L::Key {
        assert!(self.key_valid, "the cursor has no key");
        self.leaf().key(self.entry)
    }

    fn val(&self) -> &L::Val {
        assert!(self.key_valid && self.val_valid, "the cursor has no value");
        self.read_value();
        self.chunk().0.val(self.item)
    }

    fn map_times(&mut self, logic: &mut dyn FnMut(&(), &L::Weight)) {
        if self.val_valid() {
            count_weight_read::<L>();
            logic(&(), self.chunk().0.weight(self.item));
        }
    }

    fn map_times_through(&mut self, _upper: &(), logic: &mut dyn FnMut(&(), &L::Weight)) {
        self.map_times(logic)
    }

    fn weight(&mut self) -> &L::Weight {
        assert!(self.key_valid && self.val_valid, "the cursor has no weight");
        self.read_value();
        count_weight_read::<L>();
        self.chunk().0.weight(self.item)
    }

    fn weight_checked(&mut self) -> &L::Weight {
        self.weight()
    }

    fn map_values(&mut self, logic: &mut dyn FnMut(&L::Val, &L::Weight)) {
        if L::ONE_VALUE_PER_KEY {
            // As `VecWSetCursor` and `FileWSetCursor`: one call, no move.
            if self.val_valid() {
                count_weight_read::<L>();
                let (leaf, _) = self.chunk();
                logic(self.val(), leaf.weight(self.item));
            }
            return;
        }
        while self.val_valid() {
            count_weight_read::<L>();
            let (leaf, _) = self.chunk();
            logic(self.val(), leaf.weight(self.item));
            self.step_val();
        }
    }

    fn step_key(&mut self) {
        self.move_key(Landing::Always, Self::advance_key);
    }

    fn step_key_reverse(&mut self) {
        self.move_key(Landing::Always, Self::retreat_key);
    }

    fn seek_key(&mut self, key: &L::Key) {
        count_seek_of::<L>();
        self.move_key(Landing::IfMoved, |cursor| {
            cursor.seek_forward(&|probe| probe < key)
        });
    }

    fn seek_key_exact(&mut self, key: &L::Key, _hash: Option<u64>) -> bool {
        self.seek_key(key);
        self.key_valid() && self.key().eq(key)
    }

    fn seek_key_with(&mut self, predicate: &dyn Fn(&L::Key) -> bool) {
        count_seek_of::<L>();
        // Each call of the predicate reads a probe.
        let before = |key: &L::Key| {
            count_key_read::<L>();
            !predicate(key)
        };
        self.move_key(Landing::IfMoved, |cursor| cursor.seek_forward(&before));
    }

    fn seek_key_reverse(&mut self, key: &L::Key) {
        count_seek_of::<L>();
        self.move_key(Landing::Always, |cursor| {
            cursor.seek_backward(&|probe| probe <= key)
        });
    }

    fn seek_key_with_reverse(&mut self, predicate: &dyn Fn(&L::Key) -> bool) {
        count_seek_of::<L>();
        let keep = |key: &L::Key| {
            count_key_read::<L>();
            predicate(key)
        };
        self.move_key(Landing::Always, |cursor| cursor.seek_backward(&keep));
    }

    fn step_val(&mut self) {
        self.move_val(Landing::Always, |cursor| {
            if !cursor.val_valid {
                return;
            }
            let range = cursor.chunk_items();
            if cursor.item + 1 < range.end {
                cursor.item += 1;
            } else if !cursor.move_to_next_chunk() {
                cursor.item = range.end;
                cursor.val_valid = false;
            }
        });
    }

    fn step_val_reverse(&mut self) {
        self.move_val(Landing::Always, |cursor| {
            if !cursor.val_valid {
                return;
            }
            let range = cursor.chunk_items();
            if cursor.item > range.start {
                cursor.item -= 1;
            } else if !cursor.move_to_previous_chunk() {
                cursor.val_valid = false;
            }
        });
    }

    fn seek_val(&mut self, val: &L::Val) {
        self.move_val(Landing::IfMoved, |cursor| {
            cursor.seek_val_forward(&|probe| probe < val)
        });
    }

    fn seek_val_reverse(&mut self, val: &L::Val) {
        self.move_val(Landing::Always, |cursor| {
            cursor.seek_val_backward(&|probe| probe <= val)
        });
    }

    fn seek_val_with(&mut self, predicate: &dyn Fn(&L::Val) -> bool) {
        let before = |val: &L::Val| {
            count_value_read::<L>();
            !predicate(val)
        };
        self.move_val(Landing::IfMoved, |cursor| cursor.seek_val_forward(&before));
    }

    fn seek_val_with_reverse(&mut self, predicate: &dyn Fn(&L::Val) -> bool) {
        let keep = |val: &L::Val| {
            count_value_read::<L>();
            predicate(val)
        };
        self.move_val(Landing::Always, |cursor| cursor.seek_val_backward(&keep));
    }

    fn rewind_keys(&mut self) {
        self.move_key(Landing::Always, Self::rewind_key);
    }

    fn fast_forward_keys(&mut self) {
        self.move_key(Landing::Always, Self::fast_forward_key);
    }

    fn rewind_vals(&mut self) {
        self.move_val(Landing::Always, |cursor| {
            cursor.hop = None;
            cursor.val_valid = cursor.key_valid;
            if cursor.key_valid {
                cursor.item = cursor.leaf().items_of(cursor.entry).start;
            }
        });
    }

    fn fast_forward_vals(&mut self) {
        self.move_val(Landing::Always, |cursor| {
            if !cursor.key_valid {
                return;
            }
            while cursor.move_to_next_chunk() {}
            cursor.val_valid = true;
            cursor.item = cursor.chunk_items().end - 1;
        });
    }

    /// The position in entries, which differs from the position in keys only
    /// by the keys that are split between leaves.
    fn position(&self) -> Option<Position> {
        let total = self.totals.keys as u64;
        let offset = if self.key_valid {
            (self.entries_before() as u64).min(total)
        } else {
            total
        };
        Some(Position { total, offset })
    }
}
