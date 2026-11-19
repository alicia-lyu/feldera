//! The scan session: one forward pass over a merged index per transaction.
//!
//! View maintenance needs, for each *touched group* (for Q3, a customer that
//! the transaction's delta mentions), the old entries of that group that some
//! reader keeps. [`ScanSession`] is a cursor over the touched groups' ranges of
//! `Old`: for each group it positions once, then the caller scans the group's
//! range contiguously, entry by entry, and decides from the folded key alone
//! whether to read an entry's payload. The pseudocode is `alg:session` and
//! `alg:ivm-buffered` of the write-up, and the rules are in
//! `docs/design/merged-index/shared-traversal.md`.
//!
//! # Inputs
//!
//! * `Old`, a pinned snapshot of the index before the transaction, read
//!   through [`RunMerge`].
//! * The touched groups, as a sorted list of group prefixes (the root domain's
//!   tag and value, `1 + root width` bytes). The session never sees `Dlt`:
//!   whoever builds the list reads `Dlt` once, before the session
//!   ([`reader::capture`](super::reader::capture)).
//! * The index definition `D` ([`MergedIndexDefinition`]), which gives the
//!   payload decoder (`D::decode_payload`).
//!
//! # Moves
//!
//! * [`ScanSession::next_group`] positions `Old` at the first key at or past
//!   the next touched group: one logical seek under [`Pass::Seek`], steps under
//!   [`Pass::Step`] (the untouched groups in between are passed, not read).
//! * [`ScanSession::key`] is the folded key at the position while it lies in
//!   the group, and nothing at the position that ends the group. It reads no
//!   value or weight.
//! * [`ScanSession::take_and_decode`] reads the position's payloads, sums their
//!   weights across runs, and decodes them. A caller calls it only for an entry
//!   that some reader keeps, so a payload nobody keeps costs no value read, no
//!   weight read, and no decode.
//! * [`ScanSession::step`] moves to the next position. The group's range is
//!   scanned with no skips: skipping the subtrees that no reader requests is a
//!   deferred refinement (`deferred.md`).
//!
//! `Old` entries and the delta's entries stay apart and unconsolidated by the
//! session, since a join is bilinear (`prop:overlay`).
//!
//! # Knob
//!
//! [`Pass`] chooses between two ways of doing the same work, for A/B tests:
//! seek against step, for the untouched groups between two touched ones. A
//! group read has no exact-key probe: a run whose filter rejects the group's
//! root key can still hold entries below it, so the run must be sought anyway
//! (decision of 2026-11-09).

use std::{marker::PhantomData, ops::AddAssign};

use serde::Serialize;

use crate::{
    ZWeight,
    dynamic::DowncastTrait,
    trace::{BatchReader, SpineSnapshot},
};

use super::{INDEX_TAG, MergedIndexBatch, MergedIndexDefinition, RunMerge};

/// How the session passes entries that it keeps nothing from: the untouched
/// groups between two touched ones.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Pass {
    /// One logical seek to the first key of the next touched group.
    Seek,
    /// Steps through them, one position at a time.
    Step,
}

/// What the session did. The storage costs of these moves are in
/// [`RunReads`](crate::trace::RunReads), which the cursors keep.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub struct SessionCounts {
    /// Groups the session positioned on.
    pub groups: u64,
    /// Logical cursors opened: one over `Old`, which is one run cursor per
    /// run. The delta is read before the session, so the session opens no
    /// cursor on it.
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
    /// Payloads of `Old` decoded: one per entry that a reader kept.
    pub payload_decodes_old: u64,
    /// Payloads of `Dlt` decoded: one per delta entry that a reader's driving
    /// delta took. The session does not read `Dlt`; the capture before it
    /// does, and fills this count in when it hands the session's counts on.
    pub payload_decodes_dlt: u64,
    /// Values of `Old` whose weights cancelled across runs. They cost a
    /// value and a weight decode each and are not kept.
    pub cancelled: u64,
}

impl AddAssign for SessionCounts {
    /// Adds the counts of another session, for the totals of several transactions.
    fn add_assign(&mut self, other: Self) {
        self.groups += other.groups;
        self.cursor_opens += other.cursor_opens;
        self.logical_seeks += other.logical_seeks;
        self.logical_steps += other.logical_steps;
        self.positions += other.positions;
        self.payload_decodes_old += other.payload_decodes_old;
        self.payload_decodes_dlt += other.payload_decodes_dlt;
        self.cancelled += other.cancelled;
    }
}

/// A cursor over the touched groups of `Old` of the index that `D` defines;
/// see the [module docs](self).
pub struct ScanSession<'a, D: MergedIndexDefinition> {
    old: RunMerge<'a>,
    pass: Pass,

    /// The touched groups, ascending, and how many the session has entered.
    touched: &'a [Vec<u8>],
    entered: usize,

    counts: SessionCounts,
    definition: PhantomData<D>,
}

impl<'a, D: MergedIndexDefinition> ScanSession<'a, D> {
    /// Opens the session's cursor over `old`. `touched` lists the group
    /// prefixes to visit, strictly ascending.
    pub fn new(
        old: &'a SpineSnapshot<MergedIndexBatch>,
        touched: &'a [Vec<u8>],
        pass: Pass,
    ) -> Self {
        debug_assert!(
            touched.windows(2).all(|pair| pair[0] < pair[1]),
            "touched groups must be strictly ascending"
        );
        // Group seeks probe the prefix the index's spine builds its filters over
        // (`batch::index_factories`); a spine without one is sought run by run.
        let old = RunMerge::with_group_prefix(old, old.factories().filter_spec().prefix);
        let mut counts = SessionCounts {
            cursor_opens: 1,
            ..SessionCounts::default()
        };
        counts.positions += u64::from(old.key().is_some());
        Self {
            old,
            pass,
            touched,
            entered: 0,
            counts,
            definition: PhantomData,
        }
    }

    /// What the session has done so far.
    pub fn counts(&self) -> SessionCounts {
        SessionCounts {
            cancelled: self.old.cancelled(),
            ..self.counts
        }
    }

    /// Enters the next touched group: brings `Old` to the first key at or past
    /// the group's prefix (the untouched groups in between are never read) and
    /// returns the prefix, or `None` once every touched group is done.
    pub fn next_group(&mut self) -> Option<&'a [u8]> {
        let group = self.touched.get(self.entered)?;
        self.entered += 1;
        self.counts.groups += 1;
        match self.pass {
            Pass::Seek => self.seek_old(group),
            Pass::Step => {
                while self.old.key().is_some_and(|key| key < group.as_slice()) {
                    self.step();
                }
            }
        }
        Some(group)
    }

    /// The folded key at the position, if it lies in the current group; `None`
    /// at the position that ends the group, whose key has been read and
    /// nothing more.
    pub fn key(&self) -> Option<&[u8]> {
        let group = self
            .entered
            .checked_sub(1)
            .map(|last| &self.touched[last])?;
        self.old.key().filter(|key| key.starts_with(group))
    }

    /// Reads the position's payloads: each distinct payload with its weight
    /// summed over the runs, decoded by `D::decode_payload` from the stream
    /// id, the last byte of the key. Counts one decode per payload. A second
    /// call at the same position finds nothing.
    pub fn take_and_decode(&mut self) -> Result<Vec<(D::Payload, ZWeight)>, String> {
        let key = self
            .key()
            .ok_or("take_and_decode outside a group's range")?;
        let (_, stream_id) = split_key(key)?;
        let mut taken = Vec::new();
        let payload_decodes = &mut self.counts.payload_decodes_old;
        self.old.take(|payload, weight| {
            *payload_decodes += 1;
            let payload = D::decode_payload(stream_id, payload.downcast_checked::<Vec<u8>>())?;
            taken.push((payload, weight));
            Ok::<(), String>(())
        })?;
        Ok(taken)
    }

    /// Steps `Old` past its current position.
    pub fn step(&mut self) {
        self.counts.logical_steps += 1;
        self.old.step();
        self.counts.positions += u64::from(self.old.key().is_some());
    }

    /// Seeks `Old` to the group `target`, skipping the runs that cannot hold
    /// it (`RunMerge::seek_group`), and counting only a seek that moves some
    /// run.
    fn seek_old(&mut self, target: &[u8]) {
        if self.old.seek_group(target) {
            self.counts.logical_seeks += 1;
            self.counts.positions += u64::from(self.old.key().is_some());
        }
    }
}

/// Splits a folded key `<node prefix> 0 <id>` into the length of the node
/// prefix, which is the key without its trailing `[0, id]`, and the id.
pub fn split_key(key: &[u8]) -> Result<(usize, u8), String> {
    match key {
        [.., terminator, id] if *terminator == INDEX_TAG => Ok((key.len() - 2, *id)),
        _ => Err(format!(
            "malformed folded key {key:?}: no terminator and id"
        )),
    }
}

#[cfg(test)]
mod tests;
