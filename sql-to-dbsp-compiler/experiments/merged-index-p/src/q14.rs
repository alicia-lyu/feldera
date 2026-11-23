//! Query-specific partner reads for the Q14 view circuit.
//!
//! The view is `V = L ⋈ P` on `partkey`. Its delta rule has two partner
//! reads, `ΔV = ΔL ⋈ P⁺ + L⁻ ⋈ ΔP`, each a [`Reader`]:
//!
//! | Reader | Requests of | Keeps |
//! | --- | --- | --- |
//! | `P⁺` ([`PartsAfter`]) | `ΔL` | the parts of `ΔL`, after: `Old`'s row and `ΔP`'s rows |
//! | `L⁻` ([`LineitemsBefore`]) | `ΔP` | the line items of each part in `ΔP`, before |
//!
//! Both requests are part groups (depth 1), so `P⁺` is a path read and `L⁻` a
//! range read of the group. The caller maintains the index and publishes a
//! transaction's `Old` and `Dlt`; [`Q14State::collect`] reads each once, as
//! `Q3State` does for Q3:
//!
//! 1. [`capture`] reads `Dlt` once: the touched parts, the driving deltas `ΔP`
//!    and `ΔL` (moved to the caller in [`DrivingDeltas`]), and each reader's
//!    requests.
//! 2. One scan session visits the touched parts' groups of `Old`, and
//!    [`route`] copies the entries each reader keeps.
//!
//! `L⁻` is `Old`'s rows only, which `route` copies. `P⁺` is `Old`'s row plus
//! `ΔP`'s rows, and `route` copies only `Old`'s: the routing hook adds `ΔP`'s
//! rows of a part that `ΔL` requests, in the entry of `Old`'s part row, or in
//! an entry of their own if `Old` has none (a part inserted in this
//! transaction). That is the whole hook: Q14 has one join, so nothing is
//! computed at routing. A Partsupp-only change touches its part's group,
//! which the scan enters and nobody keeps anything from.
//!
//! Only `L⁻` reads past a group's first entry, so a group that `L⁻` does not
//! request is left after it (`Routing::scan`): for a refresh that changes only
//! line items, the session reads one entry per touched part.
//!
//! The query-independent state (the publication, the sealed readers, the
//! contract check, and the session reads) is DBSP's
//! [`TransactionViews`](dbsp::trace::merged_index::TransactionViews).

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use dbsp::{
    circuit::RootCircuit,
    trace::merged_index::{
        capture, measure_collection, route, Capture, Delta, DeltaCursor, DeltaReader, DrivingDelta,
        MergedIndexBatch, OrderedKey, Pass, Position, Published, Reader, ReaderDefinition,
        ScanSession, SealedReader, SessionCounts, SessionReads, SourceSnapshot, StoredReader,
        TransactionViews, Weighted,
    },
    typed_batch::SpineSnapshot,
    utils::{Tup1, Tup6},
    DBData, OrdIndexedZSet, OrdZSet, Stream,
};
use feldera_sqllib::{Date, SqlDecimal, SqlString};

use super::{
    prefix_partkey, Key, PartPartsuppLineitemDefinition, Payload,
    INDEX_ID_LINEITEM_BY_PARTKEY_SUPPKEY_ORDERKEY_LINENUMBER, INDEX_ID_PART_BY_PARTKEY,
    TAG_PARTKEY,
};

type Definition = PartPartsuppLineitemDefinition;

/// The parts, indexed by `partkey`: `(p_type)`.
pub type PartByPartkeyVector = OrdIndexedZSet<Tup1<i64>, Tup1<SqlString>>;

/// A line item's value in [`LineitemByPartkeyVector`]: `(l_suppkey,
/// l_orderkey, l_linenumber, l_extendedprice, l_discount, l_shipdate)`.
///
/// It starts with the trie's path fields below the part, in trie order, so
/// that the rows of a part arrive in value order as the index holds them, as
/// a [`Reader`]'s builder requires: without `l_suppkey`, the lines of a later
/// supplier could carry a smaller orderkey.
pub type LineValue = Tup6<i64, i64, i32, SqlDecimal<15, 2>, SqlDecimal<4, 2>, Date>;

/// The line items, indexed by `partkey`, with [`LineValue`]s.
pub type LineitemByPartkeyVector = OrdIndexedZSet<Tup1<i64>, LineValue>;

/// The driving deltas of one transaction, `ΔP` and `ΔL`: the batches that
/// `capture` built from `Dlt`, indexed by `partkey`.
pub struct DrivingDeltas {
    pub parts: PartByPartkeyVector,
    pub lines: LineitemByPartkeyVector,
}

/// What [`Q14State::collect`] hands the driver.
pub struct Collection {
    /// The driving deltas, for the view circuit's input handles. They leave
    /// `Q14State` here, once per transaction: collecting again errors.
    pub driving: DrivingDeltas,
    /// The time spent preparing the driving deltas (the capture of `Dlt` and
    /// their sealing), apart from the scan.
    pub delta_preparation: Duration,
}

// ---- The two reader definitions ----

/// `P⁺`: the parts after the transaction, driven by `ΔL`. A path read of the
/// Part stream at `partkey`.
pub struct PartsAfter;

impl ReaderDefinition for PartsAfter {
    type Index = Definition;
    type Key = Tup1<i64>;
    type Val = Tup1<SqlString>;
    type DeltaKey = Tup1<i64>;
    type DeltaVal = LineValue;
    const PARTNER_STREAM: Option<u8> = Some(INDEX_ID_PART_BY_PARTKEY);
    const REQUEST_DEPTH: usize = 1;
    const DRIVING_DELTA: DrivingDelta =
        DrivingDelta::Stream(INDEX_ID_LINEITEM_BY_PARTKEY_SUPPKEY_ORDERKEY_LINENUMBER);

    fn unfold(key: &Key, payload: &Payload) -> Result<(Self::Key, Self::Val), String> {
        match (key, payload) {
            (Key::PartByPartkey(key), Payload::PartByPartkey(payload)) => {
                Ok((Tup1(key.partkey), Tup1(payload.p_type.clone())))
            }
            other => Err(format!("expected a Part entry, found {other:?}")),
        }
    }

    fn unfold_delta(
        key: &Key,
        payload: &Payload,
    ) -> Result<(Self::DeltaKey, Self::DeltaVal), String> {
        unfold_line(key, payload)
    }
}

/// `L⁻`: the line items before the transaction, driven by `ΔP`. A range read
/// of the Lineitem stream below the part.
pub struct LineitemsBefore;

impl ReaderDefinition for LineitemsBefore {
    type Index = Definition;
    type Key = Tup1<i64>;
    type Val = LineValue;
    type DeltaKey = Tup1<i64>;
    type DeltaVal = Tup1<SqlString>;
    const PARTNER_STREAM: Option<u8> =
        Some(INDEX_ID_LINEITEM_BY_PARTKEY_SUPPKEY_ORDERKEY_LINENUMBER);
    const REQUEST_DEPTH: usize = 1;
    const DRIVING_DELTA: DrivingDelta = DrivingDelta::Stream(INDEX_ID_PART_BY_PARTKEY);

    fn unfold(key: &Key, payload: &Payload) -> Result<(Self::Key, Self::Val), String> {
        unfold_line(key, payload)
    }

    fn unfold_delta(
        key: &Key,
        payload: &Payload,
    ) -> Result<(Self::DeltaKey, Self::DeltaVal), String> {
        PartsAfter::unfold(key, payload)
    }
}

fn unfold_line(key: &Key, payload: &Payload) -> Result<(Tup1<i64>, LineValue), String> {
    match (key, payload) {
        (
            Key::LineitemByPartkeySuppkeyOrderkeyLinenumber(key),
            Payload::LineitemByPartkeySuppkeyOrderkeyLinenumber(payload),
        ) => Ok((
            Tup1(key.partkey),
            Tup6(
                key.suppkey,
                key.orderkey,
                key.linenumber,
                payload.l_extendedprice,
                payload.l_discount,
                payload.l_shipdate,
            ),
        )),
        other => Err(format!("expected a Lineitem entry, found {other:?}")),
    }
}

/// The part of a group prefix.
fn partkey_of_group(group: &[u8]) -> Result<i64, String> {
    match group {
        [tag, value @ ..] if *tag == TAG_PARTKEY => <i64 as OrderedKey>::decode_ordered(value)
            .ok_or_else(|| format!("group prefix {group:?} has a malformed partkey")),
        _ => Err(format!(
            "group prefix {group:?} does not start with the partkey tag"
        )),
    }
}

// ---- Routing: the part step that P⁺ needs beyond the generic machinery ----

/// One touched part's group as the routing sees it.
struct Group {
    partkey: i64,
    /// `ΔP`'s rows of this part, if `P⁺` requests it; else none.
    delta_parts: Weighted<Tup1<SqlString>>,
    /// Set once the part's step is done.
    part_done: bool,
}

/// The two readers and `ΔP`, whose rows the part step adds to `P⁺`. The delta
/// is borrowed: the caller of `capture` owns it.
struct Routing<'r> {
    delta_parts: DeltaCursor<'r, LineitemsBefore>,
    parts: &'r mut Reader<PartsAfter>,
    lines: &'r mut Reader<LineitemsBefore>,
}

impl Routing<'_> {
    /// Scans every touched group of `session`.
    ///
    /// Only `L⁻` keeps entries past a group's first: `P⁺` keeps the part's
    /// row, whose key (the group prefix, the terminator, the part's stream
    /// id) sorts below every child's, since the terminator is below every
    /// tag. So a group that `L⁻` does not request is left after its first
    /// entry, and the session moves on to the next group without reading the
    /// rest of this one.
    fn scan(&mut self, session: &mut ScanSession<'_, Definition>) -> Result<(), String> {
        while let Some(prefix) = session.next_group() {
            let whole_group = self.lines.requests().contains(prefix);
            let mut group = self.begin(prefix)?;
            while let Some(key) = session.key() {
                let is_part = key.last() == Some(&INDEX_ID_PART_BY_PARTKEY);
                {
                    let mut position = Position::at(session);
                    if is_part {
                        self.part(&mut group, true)?;
                    } else {
                        self.part_if_absent(&mut group)?;
                    }
                    let readers: &mut [&mut dyn StoredReader<Definition>] =
                        &mut [&mut *self.parts, &mut *self.lines];
                    route(&mut position, readers)?;
                }
                if !whole_group {
                    break;
                }
                session.step();
            }
            self.part_if_absent(&mut group)?;
        }
        Ok(())
    }

    fn begin(&mut self, prefix: &[u8]) -> Result<Group, String> {
        let partkey = partkey_of_group(prefix)?;
        // `ΔP` has rows in this group only if `L⁻` requests it, and they
        // matter only if `P⁺` does.
        let delta_parts =
            if self.parts.requests_mut().has(prefix) && self.lines.requests_mut().has(prefix) {
                self.delta_parts.rows_at(&Tup1(partkey))
            } else {
                Vec::new()
            };
        Ok(Group {
            partkey,
            delta_parts,
            part_done: false,
        })
    }

    fn part_if_absent(&mut self, group: &mut Group) -> Result<(), String> {
        if group.part_done {
            Ok(())
        } else {
            self.part(group, false)
        }
    }

    /// The part's step: gives `ΔP`'s rows to `P⁺`. `in_old` says whether
    /// `Old` has an entry of the part, which `route` then adds to the open
    /// entry and closes; otherwise the entry of `ΔP`'s rows ends here.
    fn part(&mut self, group: &mut Group, in_old: bool) -> Result<(), String> {
        group.part_done = true;
        if group.delta_parts.is_empty() {
            return Ok(());
        }
        // `P⁺`'s slice is `Old`'s row and `ΔP`'s rows: the three rows of one
        // entry at most.
        for (Tup1(part_type), weight) in &group.delta_parts {
            self.parts
                .push_row(Tup1(group.partkey), Tup1(part_type.clone()), *weight)?;
        }
        if !in_old {
            self.parts.end_entry()?;
        }
        Ok(())
    }
}

// ---- The state of one transaction ----

/// The readers of one transaction, sealed by [`Q14State::collect`].
struct CollectedReaders {
    parts: SealedReader<PartsAfter>,
    lines: SealedReader<LineitemsBefore>,
}

/// One externally prepared transaction's index views and readers, shared by
/// the view circuit's join. The order of a transaction is `Q3State`'s:
/// [`publish`](Self::publish), [`collect`](Self::collect), the join's
/// partner reads, then [`finish`](Self::finish) once the view transaction
/// has committed.
///
/// The partner reads can check that every key of the circuit's delta is one
/// of its reader's requests ([`Self::enable_contract_check`]); a measured run
/// must not enable it.
#[derive(Clone)]
pub struct Q14State {
    views: TransactionViews<CollectedReaders>,
}

impl Default for Q14State {
    fn default() -> Self {
        Self {
            views: TransactionViews::new("Q14"),
        }
    }
}

impl Q14State {
    /// Makes every partner read check its contract (all clones share the
    /// setting).
    pub fn enable_contract_check(&self) {
        self.views.enable_contract_check();
    }

    /// Publishes a transaction's `Old` and `Dlt`.
    pub fn publish(&self, old: SourceSnapshot, dlt: Arc<MergedIndexBatch>) -> Result<(), String> {
        self.views.publish(old, dlt)
    }

    /// Reads `Dlt` and `Old` once each and fills the readers, which are
    /// sealed and kept until [`Self::finish`]. The index is read here, on the
    /// calling thread under the merged index's read owner, and nowhere else.
    ///
    /// Returns the driving deltas, which leave `Q14State` here, and the time
    /// spent preparing them. A second `collect` of one publication errors.
    pub fn collect(&self, pass: Pass) -> Result<Collection, String> {
        let views = self.views()?;
        let (collected, driver_reads, driver_expansions) = measure_collection(|| {
            let mut parts = Reader::<PartsAfter>::new()?;
            let mut lines = Reader::<LineitemsBefore>::new()?;

            // The driving deltas: `ΔL` of `P⁺`, and `ΔP` of `L⁻`.
            let mut delta_lines = Delta::<PartsAfter>::new()?;
            let mut delta_parts = Delta::<LineitemsBefore>::new()?;
            let drivers: &mut [&mut dyn DeltaReader<Definition>] = &mut [
                &mut Capture::new(&mut parts, &mut delta_lines),
                &mut Capture::new(&mut lines, &mut delta_parts),
            ];
            let capture_started = Instant::now();
            let captured = capture::<Definition>(&views.dlt, drivers)?;
            let mut delta_preparation = capture_started.elapsed();

            let mut session = ScanSession::<Definition>::new(&views.old, &captured.touched, pass);
            Routing {
                delta_parts: delta_parts.cursor()?,
                parts: &mut parts,
                lines: &mut lines,
            }
            .scan(&mut session)?;
            let counts = SessionCounts {
                payload_decodes_dlt: captured.payload_decodes_dlt,
                ..session.counts()
            };
            drop(session);

            let readers = CollectedReaders {
                parts: parts.into_sealed()?,
                lines: lines.into_sealed()?,
            };
            // The deltas are moved out, not cloned.
            let seal_started = Instant::now();
            let driving = DrivingDeltas {
                parts: delta_parts.into_batch()?,
                lines: delta_lines.into_batch()?,
            };
            delta_preparation += seal_started.elapsed();
            Ok::<_, String>((readers, driving, counts, delta_preparation))
        });
        let (readers, driving, counts, delta_preparation) = collected?;
        views.set_collected(
            readers,
            SessionReads::new(counts, driver_reads, driver_expansions),
        )?;
        Ok(Collection {
            driving,
            delta_preparation,
        })
    }

    /// Releases the readers and published views after the view transaction,
    /// and returns what collecting them read (nothing, if never collected).
    pub fn finish(&self) -> Result<SessionReads, String> {
        self.views.finish()
    }

    fn views(&self) -> Result<Arc<Published<CollectedReaders>>, String> {
        self.views.published()
    }

    /// `P⁺`: the parts after the transaction, driven by `ΔL`: for each part
    /// of `ΔL`, `Old`'s row and `ΔP`'s rows, consolidated.
    pub fn parts_after(
        &self,
        delta: &SpineSnapshot<LineitemByPartkeyVector>,
    ) -> Result<SpineSnapshot<PartByPartkeyVector>, String> {
        self.views
            .partner(|readers| &readers.parts, delta, part_request, "P⁺")
    }

    /// `L⁻`: the line items before the transaction, driven by `ΔP`: every
    /// line item of each part of `ΔP`.
    pub fn lineitems_before(
        &self,
        delta: &SpineSnapshot<PartByPartkeyVector>,
    ) -> Result<SpineSnapshot<LineitemByPartkeyVector>, String> {
        self.views
            .partner(|readers| &readers.lines, delta, part_request, "L⁻")
    }
}

/// The request prefix of a part group, and its description for errors.
fn part_request(&Tup1(part): &Tup1<i64>) -> (Vec<u8>, String) {
    (prefix_partkey(part), format!("part {part}"))
}

/// `V = L ⋈ P` on `partkey`, with both historical operands read from the
/// collected readers: `ΔV = ΔL ⋈ P⁺ + L⁻ ⋈ ΔP`.
pub fn join_lineitems_parts<F, OV>(
    lines: &Stream<RootCircuit, LineitemByPartkeyVector>,
    parts: &Stream<RootCircuit, PartByPartkeyVector>,
    state: Q14State,
    join: F,
) -> Stream<RootCircuit, OrdZSet<OV>>
where
    OV: DBData,
    F: Fn(&Tup1<i64>, &LineValue, &Tup1<SqlString>) -> OV + Clone + 'static,
{
    let before = state.clone();
    lines.join_with_snapshot_readers(
        parts,
        move |delta| {
            state
                .parts_after(delta)
                .expect("Q14 P+ reconstruction failed")
        },
        move |delta| {
            before
                .lineitems_before(delta)
                .expect("Q14 L- reconstruction failed")
        },
        join,
    )
}

#[cfg(test)]
mod tests;
