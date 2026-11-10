//! The scan session: one forward pass over a merged index per transaction.
//!
//! View maintenance needs, for each *touched group* (for Q3, a customer that
//! the transaction's delta mentions), the old entries of that group, and the
//! delta's entries for it. [`ScanSession`] hands them to the caller group by
//! group, in index order, and reads the index so that each old entry is read
//! once and untouched groups are passed, not read. The pseudocode is
//! `alg:session`, `alg:skip`, and `alg:ivm-buffered` of the write-up, and the
//! rules are in `docs/design/merged-index/shared-traversal.md`.
//!
//! # Inputs
//!
//! * `Old`, a pinned snapshot of the index before the transaction, read
//!   through [`RunMerge`].
//! * `Dlt`, the transaction's signed changes to the index, one in-memory run,
//!   read once by one forward cursor.
//! * The trie, which gives the width of the root domain. A group is the first
//!   `1 + root width` bytes of a key: the root domain's tag and value.
//!
//! The groups are inferred from `Dlt`: a touched group is one that has a `Dlt`
//! entry, so the session needs no list of requests.
//!
//! # What the session buffers
//!
//! For each group, [`ScanSession::maintain`] collects the entries of the
//! requested streams (index ids) into a [`GroupBuffers`], calls the caller's
//! `on_group` with them, and releases them. `Old` and `Dlt` entries stay
//! apart and unconsolidated, since a join is bilinear (`prop:overlay`).
//! A position of `Old` whose stream is not requested costs a key decode and
//! nothing else, as does the position that ends a group.
//!
//! # Knob
//!
//! [`Pass`] chooses between two ways of doing the same work, for A/B tests:
//! seek against step, for untouched groups and rejected subtrees. Both buffer
//! the same entries. A group read has no exact-key probe: a run whose filter
//! rejects the group's root key can still hold entries below it, so the run
//! must be sought anyway (decision of 2026-11-09).

use std::collections::{BTreeMap, BTreeSet};

use crate::{
    ZWeight,
    dynamic::DowncastTrait,
    trace::{BatchReader, Cursor, SpineSnapshot},
};

use super::{INDEX_TAG, KeyTrie, MergedIndexBatch, RunMerge, prefix_successor};

type DeltaCursor<'a> = <MergedIndexBatch as BatchReader>::Cursor<'a>;

/// How the session passes entries that it buffers nothing from: the untouched
/// groups between two touched ones, and the rest of a rejected subtree.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Pass {
    /// One logical seek to the first key past them.
    Seek,
    /// Steps through them, one position at a time.
    Step,
}

/// One entry of the index: a folded key, its decoded payload, and its weight.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Entry<P> {
    pub key: Vec<u8>,
    pub payload: P,
    pub weight: ZWeight,
}

impl<P> Entry<P> {
    /// The id of the entry's stream: the last byte of its folded key.
    pub fn stream_id(&self) -> u8 {
        *self.key.last().expect("a folded key is not empty")
    }
}

/// The entries of one stream in one group, in key order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StreamBuffer<P> {
    /// Entries of the index before the transaction.
    pub old: Vec<Entry<P>>,
    /// Entries of the transaction's delta.
    pub dlt: Vec<Entry<P>>,
}

// Not derived: a derive would require `P: Default`.
impl<P> Default for StreamBuffer<P> {
    fn default() -> Self {
        Self {
            old: Vec::new(),
            dlt: Vec::new(),
        }
    }
}

/// A group's buffers, one per requested stream that has an entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GroupBuffers<P> {
    streams: BTreeMap<u8, StreamBuffer<P>>,
}

impl<P> Default for GroupBuffers<P> {
    fn default() -> Self {
        Self {
            streams: BTreeMap::new(),
        }
    }
}

impl<P> GroupBuffers<P> {
    /// The buffer of stream `id`, or `None` if it has no entry in this group.
    pub fn stream(&self, id: u8) -> Option<&StreamBuffer<P>> {
        self.streams.get(&id)
    }

    /// The old entries of stream `id`, in key order.
    pub fn old(&self, id: u8) -> &[Entry<P>] {
        self.stream(id).map_or(&[], |buffer| &buffer.old)
    }

    /// The delta's entries of stream `id`, in key order.
    pub fn dlt(&self, id: u8) -> &[Entry<P>] {
        self.stream(id).map_or(&[], |buffer| &buffer.dlt)
    }

    /// The nonempty buffers, by ascending stream id.
    pub fn streams(&self) -> impl Iterator<Item = (u8, &StreamBuffer<P>)> {
        self.streams.iter().map(|(&id, buffer)| (id, buffer))
    }

    fn stream_mut(&mut self, id: u8) -> &mut StreamBuffer<P> {
        self.streams.entry(id).or_default()
    }
}

/// What the session did. The storage costs of these moves are in
/// [`RunReads`](crate::trace::RunReads), which the cursors keep.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SessionCounts {
    /// Groups handed to `on_group`.
    pub groups: u64,
    /// Logical cursors opened: one over `Old` and one over `Dlt`. `Old`'s
    /// logical cursor is one run cursor per run.
    pub cursor_opens: u64,
    /// Seeks over `Old` that moved some run. A seek that finds `Old` already
    /// at or past its target moves nothing and counts nothing. One logical
    /// seek costs one run seek for each run that it moves.
    pub logical_seeks: u64,
    /// Steps over `Old`, past one position each.
    pub logical_steps: u64,
    /// Positions of `Old` that the session reached: its first key, and each
    /// key a step or seek landed on. Each costs one key decode in a
    /// file run.
    pub positions: u64,
    /// Payloads of `Old` decoded: one per buffered old entry.
    pub payload_decodes_old: u64,
    /// Payloads of `Dlt` decoded: one per buffered delta entry.
    pub payload_decodes_dlt: u64,
    /// Values of `Old` whose weights cancelled across runs. They cost a
    /// value and a weight decode each and are not buffered.
    pub cancelled: u64,
}

/// A forward pass over `Old` and `Dlt`; see the [module docs](self).
pub struct ScanSession<'a> {
    old: RunMerge<'a>,
    dlt: DeltaCursor<'a>,
    pass: Pass,

    /// The length of a group's prefix: the root domain's tag and value.
    group_width: usize,

    counts: SessionCounts,
}

impl<'a> ScanSession<'a> {
    /// Opens the session's cursors over `old` and `dlt`. Fails if the trie has
    /// no single root width (see [`KeyTrie::root_width`]), since then a key's
    /// group cannot be read from its first bytes.
    pub fn new(
        trie: &KeyTrie,
        old: &'a SpineSnapshot<MergedIndexBatch>,
        dlt: &'a MergedIndexBatch,
        pass: Pass,
    ) -> Result<Self, String> {
        let root_width = trie.root_width().ok_or_else(|| {
            format!(
                "trie `{}` does not have exactly one top-level node, so a key's group is not \
                 its first bytes",
                trie.name
            )
        })?;
        let old = RunMerge::new(old);
        let mut counts = SessionCounts {
            cursor_opens: 2,
            ..SessionCounts::default()
        };
        counts.positions += u64::from(old.key().is_some());
        Ok(Self {
            old,
            dlt: dlt.cursor(),
            pass,
            group_width: 1 + root_width,
            counts,
        })
    }

    /// What the session has done so far.
    pub fn counts(&self) -> SessionCounts {
        SessionCounts {
            cancelled: self.old.cancelled(),
            ..self.counts
        }
    }

    /// Hands the caller each touched group, in index order, with the entries
    /// of the streams in `inputs` (index ids). Reads `Dlt` once, so a second
    /// call finds no group.
    ///
    /// * `decode(key, payload bytes)` makes a payload of the caller's type
    ///   from the folded key and the stored bytes. It runs once per buffered
    ///   entry, and its error stops the session.
    /// * `reject`, if given, judges each buffered *old* entry. When some entry
    ///   of an old key is rejected, the entry stays in its buffer, and the
    ///   session passes the rest of the key's node and everything below it in
    ///   `Old` (`Skip`), because no consumer needs them. `Dlt` entries are
    ///   never rejected: the delta is read whole.
    /// * `on_group(group prefix, buffers)` runs once per group. The buffers
    ///   are released when it returns.
    pub fn maintain<P>(
        &mut self,
        inputs: &BTreeSet<u8>,
        mut decode: impl FnMut(&[u8], &[u8]) -> Result<P, String>,
        mut reject: Option<&mut dyn FnMut(&Entry<P>) -> bool>,
        mut on_group: impl FnMut(&[u8], &GroupBuffers<P>) -> Result<(), String>,
    ) -> Result<(), String> {
        while self.dlt.key_valid() {
            let group = self.next_group()?;
            self.counts.groups += 1;
            self.pass_to(&group);

            let mut buffers = GroupBuffers::default();
            self.collect_old(&group, inputs, &mut decode, &mut reject, &mut buffers)?;
            self.collect_delta(&group, inputs, &mut decode, &mut buffers)?;
            on_group(&group, &buffers)?;
        }
        Ok(())
    }

    /// The group of the delta's next key.
    fn next_group(&self) -> Result<Vec<u8>, String> {
        let key = self.dlt.key().downcast_checked::<Vec<u8>>();
        key.get(..self.group_width)
            .map(<[u8]>::to_vec)
            .ok_or_else(|| format!("delta key {key:?} is shorter than a group prefix"))
    }

    /// Brings `Old` to the first key at or past `group`. The untouched groups
    /// in between are never buffered.
    fn pass_to(&mut self, group: &[u8]) {
        match self.pass {
            Pass::Seek => self.seek_old(group),
            Pass::Step => {
                while self.old.key().is_some_and(|key| key < group) {
                    self.step_old();
                }
            }
        }
    }

    /// `Collect`: buffers the entries of `Old` under `group` whose stream is in
    /// `inputs`, stepping through every position of the group. Ends on the
    /// first position past the group, whose key it has read and nothing more.
    fn collect_old<P>(
        &mut self,
        group: &[u8],
        inputs: &BTreeSet<u8>,
        decode: &mut impl FnMut(&[u8], &[u8]) -> Result<P, String>,
        reject: &mut Option<&mut dyn FnMut(&Entry<P>) -> bool>,
        buffers: &mut GroupBuffers<P>,
    ) -> Result<(), String> {
        loop {
            let Some(position) = self.old.key().filter(|key| key.starts_with(group)) else {
                return Ok(());
            };
            let (node_len, id) = split_key(position)?;
            if !inputs.contains(&id) {
                self.step_old();
                continue;
            }

            let key = position.to_vec();
            let mut taken = Vec::new();
            let payload_decodes = &mut self.counts.payload_decodes_old;
            self.old.take(|payload, weight| {
                *payload_decodes += 1;
                let payload = decode(&key, payload.downcast_checked::<Vec<u8>>())?;
                taken.push(Entry {
                    key: key.clone(),
                    payload,
                    weight,
                });
                Ok::<(), String>(())
            })?;

            let rejected = reject
                .as_mut()
                .is_some_and(|reject| taken.iter().any(reject));
            if !taken.is_empty() {
                buffers.stream_mut(id).old.extend(taken);
            }
            if rejected {
                // The skip ends on a key outside the node, so no step follows.
                self.skip(&key[..node_len]);
            } else {
                self.step_old();
            }
        }
    }

    /// `Skip`: passes the rest of the node with prefix `node` in `Old`. The
    /// position is a key of the node.
    fn skip(&mut self, node: &[u8]) {
        match self.pass {
            Pass::Seek => match prefix_successor(node) {
                Some(successor) => self.seek_old(&successor),
                None => {
                    // The node's range runs to the end of the index.
                    self.counts.logical_seeks += 1;
                    self.old.exhaust();
                }
            },
            Pass::Step => {
                while self.old.key().is_some_and(|key| key.starts_with(node)) {
                    self.step_old();
                }
            }
        }
    }

    /// Buffers the delta's entries under `group` whose stream is in `inputs`.
    /// The delta is sorted, so they are the next ones in its order.
    fn collect_delta<P>(
        &mut self,
        group: &[u8],
        inputs: &BTreeSet<u8>,
        decode: &mut impl FnMut(&[u8], &[u8]) -> Result<P, String>,
        buffers: &mut GroupBuffers<P>,
    ) -> Result<(), String> {
        while self.dlt.key_valid() {
            let key = self.dlt.key().downcast_checked::<Vec<u8>>().clone();
            if !key.starts_with(group) {
                break;
            }
            let (_, id) = split_key(&key)?;
            if inputs.contains(&id) {
                while self.dlt.val_valid() {
                    let weight = *self.dlt.weight().downcast_checked::<ZWeight>();
                    self.counts.payload_decodes_dlt += 1;
                    let payload = decode(&key, self.dlt.val().downcast_checked::<Vec<u8>>())?;
                    buffers.stream_mut(id).dlt.push(Entry {
                        key: key.clone(),
                        payload,
                        weight,
                    });
                    self.dlt.step_val();
                }
            }
            self.dlt.step_key();
        }
        Ok(())
    }

    /// Steps `Old` past its current position.
    fn step_old(&mut self) {
        self.counts.logical_steps += 1;
        self.old.step();
        self.counts.positions += u64::from(self.old.key().is_some());
    }

    /// Seeks `Old` to `target`, counting only a seek that moves some run.
    fn seek_old(&mut self, target: &[u8]) {
        if self.old.seek(target) {
            self.counts.logical_seeks += 1;
            self.counts.positions += u64::from(self.old.key().is_some());
        }
    }
}

/// Splits a folded key `<node prefix> 0 <id>` into the length of the node
/// prefix, which is the key without its trailing `[0, id]`, and the id.
fn split_key(key: &[u8]) -> Result<(usize, u8), String> {
    match key {
        [.., terminator, id] if *terminator == INDEX_TAG => Ok((key.len() - 2, *id)),
        _ => Err(format!(
            "malformed folded key {key:?}: no terminator and id"
        )),
    }
}

#[cfg(test)]
mod tests;
