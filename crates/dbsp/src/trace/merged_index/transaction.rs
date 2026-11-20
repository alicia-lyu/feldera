//! The query-independent state of one transaction's partner reads.
//!
//! An index's query-specific module (Q3's is `merged-index-col/src/q3.rs`)
//! defines its readers and its routing. What every such module also needs is
//! here:
//!
//! - [`TransactionViews`]: the publication of `Old` and `Dlt`, the collected
//!   readers, sealed once per transaction, and the partner reads that the
//!   joins make, with the optional contract check
//!   ([`TransactionViews::enable_contract_check`]);
//! - [`SessionReads`] and [`measure_collection`]: what collecting the readers
//!   read, per read owner;
//! - [`Weighted`], [`consolidate`], and [`multiply_weights`]: the weight
//!   arithmetic of a routing hook that combines rows.
//!
//! The order of one transaction: [`publish`](TransactionViews::publish),
//! then [`Published::set_collected`] (by the module's `collect`), then the
//! partner reads ([`TransactionViews::partner`]), then
//! [`finish`](TransactionViews::finish).

use std::{
    ops::AddAssign,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
};

use crate::{
    DBData, ZWeight,
    dynamic::DowncastTrait,
    storage::attribution::Component,
    trace::{
        BatchReader as DynBatchReader, BlockExpansions, Cursor, RunReads,
        SpineSnapshot as DynamicSnapshot, with_read_owner,
    },
    typed_batch::{BatchReader, OrdIndexedZSet, SpineSnapshot},
};

use super::{MergedIndexBatch, ReaderDefinition, SealedReader, SessionCounts};

/// A snapshot of the index: `Old`, the index before the transaction.
pub type SourceSnapshot = DynamicSnapshot<MergedIndexBatch>;

/// Run reads per read owner, indexed like [`Component::ALL`].
pub type OwnerReads = [RunReads; Component::ALL.len()];

/// Block expansions per read owner, indexed like [`Component::ALL`].
pub type OwnerExpansions = [BlockExpansions; Component::ALL.len()];

/// What one transaction's view maintenance read from the index: the scan
/// session's moves, and the run reads they cost on the thread that collected.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SessionReads {
    pub counts: SessionCounts,
    /// The reads of the index, under the merged index's read owner (`Dlt`'s,
    /// the capture's, included).
    pub run_reads: RunReads,
    /// Every read the collecting thread made, per owner. Besides the index's,
    /// they are the routing's reads of the driving deltas it merges alongside
    /// the scan, under the requested-slice owner. No other owner reads here.
    pub driver_reads: OwnerReads,
    /// The blocks the collecting thread expanded from their disk encoding on
    /// cache misses, per owner (measurement version 12). They are a
    /// storage-level step apart from the item decodes in `driver_reads`.
    pub driver_expansions: OwnerExpansions,
}

impl SessionReads {
    /// The reads of a collection that [`measure_collection`] measured, with
    /// the session's `counts`. `run_reads` is the merged index's share of
    /// `driver_reads`.
    pub fn new(
        counts: SessionCounts,
        driver_reads: OwnerReads,
        driver_expansions: OwnerExpansions,
    ) -> Self {
        Self {
            counts,
            run_reads: driver_reads[Component::MergedIndex as usize],
            driver_reads,
            driver_expansions,
        }
    }
}

impl AddAssign for SessionReads {
    fn add_assign(&mut self, other: Self) {
        self.counts += other.counts;
        self.run_reads += other.run_reads;
        for (total, reads) in self.driver_reads.iter_mut().zip(other.driver_reads) {
            *total += reads;
        }
        for (total, expansions) in self
            .driver_expansions
            .iter_mut()
            .zip(other.driver_expansions)
        {
            *total += expansions;
        }
    }
}

/// Runs `collect` on this thread under the merged index's read owner, and
/// returns its result with every read the thread made meanwhile and every
/// block it expanded, per owner. Reads that `collect` charges to another
/// owner (the routing's reads of the driving deltas) count under that owner.
pub fn measure_collection<T>(collect: impl FnOnce() -> T) -> (T, OwnerReads, OwnerExpansions) {
    let reads_before = Component::ALL.map(RunReads::current_for);
    let expansions_before = Component::ALL.map(BlockExpansions::current_for);
    let result = with_read_owner(Component::MergedIndex, collect);
    let reads_after = Component::ALL.map(RunReads::current_for);
    let expansions_after = Component::ALL.map(BlockExpansions::current_for);
    (
        result,
        std::array::from_fn(|i| reads_after[i].since(reads_before[i])),
        std::array::from_fn(|i| expansions_after[i].since(expansions_before[i])),
    )
}

const WEIGHT_OVERFLOW: &str = "merged-index join weight overflow";

/// Values with their weights, such as the rows of one node before or after
/// the transaction.
pub type Weighted<V> = Vec<(V, ZWeight)>;

/// Sums the weights of equal values and drops the zero sums, in value order.
/// Errors if a sum overflows.
pub fn consolidate<V: Ord>(mut rows: Weighted<V>) -> Result<Weighted<V>, String> {
    rows.sort_by(|left, right| left.0.cmp(&right.0));
    let mut merged: Weighted<V> = Vec::with_capacity(rows.len());
    for (value, weight) in rows {
        match merged.last_mut() {
            Some((last, total)) if *last == value => {
                *total = total.checked_add(weight).ok_or(WEIGHT_OVERFLOW)?;
            }
            _ => merged.push((value, weight)),
        }
    }
    merged.retain(|(_, weight)| *weight != 0);
    Ok(merged)
}

/// The weight of a joined row: the product of its operands' weights. Errors
/// if it overflows.
pub fn multiply_weights(left: ZWeight, right: ZWeight) -> Result<ZWeight, String> {
    left.checked_mul(right)
        .ok_or_else(|| WEIGHT_OVERFLOW.to_owned())
}

/// The published views of one transaction, and, once collected, its readers.
pub struct Published<C> {
    /// The index before the transaction.
    pub old: SourceSnapshot,
    /// The run of the transaction's signed index changes.
    pub dlt: Arc<MergedIndexBatch>,
    /// Set once by the module's `collect`, and read-only after that: the
    /// driver thread sets it, and the worker thread's partner reads and the
    /// driver's `finish` read it.
    collected: OnceLock<Collected<C>>,
}

struct Collected<C> {
    readers: C,
    reads: SessionReads,
}

impl<C> Published<C> {
    /// Stores the sealed `readers` and what collecting them read. Errors if
    /// this publication was already collected.
    pub fn set_collected(&self, readers: C, reads: SessionReads) -> Result<(), String> {
        self.collected
            .set(Collected { readers, reads })
            .map_err(|_| "the published views were already collected".to_owned())
    }

    /// The collected readers. Errors before the collection.
    pub fn collected(&self) -> Result<&C, String> {
        self.collected
            .get()
            .map(|collected| &collected.readers)
            .ok_or_else(|| "collect the touched groups before reading them".to_owned())
    }
}

/// One externally prepared transaction's index views and readers, shared by
/// the view circuit's sources and joins. `C` holds the query's sealed readers.
///
/// Publish `Old`, the index snapshot before the transaction, and `Dlt`, the
/// run of the transaction's signed index changes, after index maintenance and
/// before the view transaction. The query's `collect` then fills and seals
/// the readers ([`Published::set_collected`]), and the joins take the partner
/// reads ([`partner`](Self::partner)). Keep the publication unchanged until
/// the view transaction has committed, then call [`finish`](Self::finish):
/// the readers are held until then, since a later join may flush after an
/// earlier one has passed every group.
///
/// The partner reads can check the contract that every key of the circuit's
/// delta is one of its reader's requests
/// ([`enable_contract_check`](Self::enable_contract_check)). The check walks
/// the circuit's delta, so it is off by default and a measured driver never
/// enables it; tests and validation runs do.
///
/// Clones share the publication and the contract setting.
pub struct TransactionViews<C> {
    /// The query's name, for errors.
    name: &'static str,
    published: Arc<Mutex<Option<Arc<Published<C>>>>>,
    check_contract: Arc<AtomicBool>,
}

impl<C> Clone for TransactionViews<C> {
    fn clone(&self) -> Self {
        Self {
            name: self.name,
            published: Arc::clone(&self.published),
            check_contract: Arc::clone(&self.check_contract),
        }
    }
}

impl<C> TransactionViews<C> {
    /// No publication, with the contract check off. `name` names the query
    /// in errors.
    pub fn new(name: &'static str) -> Self {
        Self {
            name,
            published: Arc::default(),
            check_contract: Arc::default(),
        }
    }

    /// Makes every partner read check its contract (all clones share the
    /// setting): each key of the circuit's delta must be one of its reader's
    /// requests, or the read errors. The check reads the circuit's delta once
    /// more, so a measured run must not enable it.
    pub fn enable_contract_check(&self) {
        self.check_contract.store(true, Ordering::Relaxed);
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Option<Arc<Published<C>>>>, String> {
        self.published
            .lock()
            .map_err(|_| format!("{} state lock poisoned", self.name))
    }

    /// Publishes a transaction's `old` and `dlt`. Errors if the previous
    /// transaction has not finished.
    pub fn publish(&self, old: SourceSnapshot, dlt: Arc<MergedIndexBatch>) -> Result<(), String> {
        let mut published = self.lock()?;
        if published.is_some() {
            return Err(format!(
                "finish the previous {} transaction before publishing another",
                self.name
            ));
        }
        *published = Some(Arc::new(Published {
            old,
            dlt,
            collected: OnceLock::new(),
        }));
        Ok(())
    }

    /// The current publication. Errors if none is published.
    pub fn published(&self) -> Result<Arc<Published<C>>, String> {
        self.lock()?.clone().ok_or_else(|| {
            format!(
                "publish merged-index views before {} maintenance",
                self.name
            )
        })
    }

    /// Releases the readers and the published views after every consumer of
    /// the transaction has finished, and returns what collecting them read
    /// (nothing, if the transaction never collected). Errors if nothing is
    /// published.
    pub fn finish(&self) -> Result<SessionReads, String> {
        let published = self
            .lock()?
            .take()
            .ok_or_else(|| format!("no {} transaction views were published", self.name))?;
        Ok(published
            .collected
            .get()
            .map_or_else(SessionReads::default, |collected| collected.reads))
    }

    /// The partner batch of the reader that `reader` selects from the
    /// collected readers, for a join whose delta is `delta`. With the
    /// contract check on, each key of `delta` must be a request of the
    /// reader: `prefix` gives the key's request prefix and a description for
    /// the error, and `reader_name` names the reader.
    pub fn partner<R, K, V>(
        &self,
        reader: impl Fn(&C) -> &SealedReader<R>,
        delta: &SpineSnapshot<OrdIndexedZSet<K, V>>,
        prefix: impl Fn(&K) -> (Vec<u8>, String),
        reader_name: &str,
    ) -> Result<SpineSnapshot<OrdIndexedZSet<R::Key, R::Val>>, String>
    where
        R: ReaderDefinition,
        K: DBData,
        V: DBData,
    {
        let published = self.published()?;
        let reader = reader(published.collected()?);
        if self.check_contract.load(Ordering::Relaxed) {
            for key in requested_keys(delta) {
                let (prefix, what) = prefix(&key);
                require_requested(reader, &prefix, &what, reader_name)?;
            }
        }
        Ok(reader.partner())
    }
}

/// Checks that a delta key's request prefix is one of `reader`'s requests.
pub fn require_requested<R: ReaderDefinition>(
    reader: &SealedReader<R>,
    prefix: &[u8],
    what: &str,
    reader_name: &str,
) -> Result<(), String> {
    if reader.requests().contains(prefix) {
        Ok(())
    } else {
        Err(format!(
            "{what} is outside the requests of {reader_name} ({} requests): its driving delta has no row for it",
            reader.requests().len()
        ))
    }
}

/// The keys of a driving delta, in order. This is a walk of the circuit's
/// delta, like the join's own delta side, so its reads are the operators'
/// (`Query`), not the requested slice's, though the join runs the reader
/// callback under that owner.
fn requested_keys<K: DBData, V: DBData>(delta: &SpineSnapshot<OrdIndexedZSet<K, V>>) -> Vec<K> {
    with_read_owner(Component::Query, || {
        let mut cursor = delta.inner().cursor();
        let mut keys = Vec::new();
        while cursor.key_valid() {
            keys.push(cursor.key().downcast_checked::<K>().clone());
            cursor.step_key();
        }
        keys
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        DynZWeight,
        dynamic::DynData,
        trace::{BatchReaderFactories, ord::OrdIndexedWSetFactories},
    };

    #[test]
    fn consolidate_sums_equal_values_and_drops_zeros() {
        let rows = vec![("b", 2), ("a", 1), ("b", -2), ("c", 3), ("a", 4)];
        assert_eq!(consolidate(rows).unwrap(), vec![("a", 5), ("c", 3)]);
        assert_eq!(consolidate(Vec::<(u8, ZWeight)>::new()).unwrap(), vec![]);
    }

    #[test]
    fn weight_arithmetic_errors_on_overflow() {
        assert!(consolidate(vec![(1, ZWeight::MAX), (1, 1)]).is_err());
        assert_eq!(multiply_weights(-3, 4), Ok(-12));
        assert!(multiply_weights(ZWeight::MAX, 2).is_err());
    }

    #[test]
    fn session_reads_take_the_index_share_and_add() {
        let mut driver_reads = OwnerReads::default();
        driver_reads[Component::MergedIndex as usize].seeks = 3;
        driver_reads[Component::RequestedSlice as usize].seeks = 5;
        let reads = SessionReads::new(
            SessionCounts::default(),
            driver_reads,
            OwnerExpansions::default(),
        );
        assert_eq!(reads.run_reads.seeks, 3);
        let mut total = reads;
        total += reads;
        assert_eq!(total.run_reads.seeks, 6);
        assert_eq!(
            total.driver_reads[Component::RequestedSlice as usize].seeks,
            10
        );
    }

    fn empty_views() -> (SourceSnapshot, Arc<MergedIndexBatch>) {
        let factories =
            OrdIndexedWSetFactories::<DynData, DynData, DynZWeight>::new::<Vec<u8>, u8, ZWeight>();
        let dlt = super::super::batch::build_batch::<u8>(Vec::new()).unwrap();
        (DynamicSnapshot::with_batches(&factories, Vec::new()), Arc::new(dlt))
    }

    #[test]
    fn a_publication_is_collected_once_and_finished_once() {
        let views = TransactionViews::<u8>::new("Test");
        assert!(views.published().err().unwrap().contains("publish merged-index views"));
        assert!(views.finish().unwrap_err().contains("no Test transaction"));

        let (old, dlt) = empty_views();
        views.publish(old.clone(), dlt.clone()).unwrap();
        let clone = views.clone();
        assert!(clone
            .publish(old.clone(), dlt.clone())
            .unwrap_err()
            .contains("finish the previous Test transaction"));
        let published = clone.published().unwrap();
        assert!(published.collected().err().unwrap().contains("collect"));

        let mut reads = SessionReads::default();
        reads.counts.groups = 2;
        published.set_collected(7, reads).unwrap();
        assert!(published
            .set_collected(8, reads)
            .unwrap_err()
            .contains("already collected"));
        assert_eq!(views.published().unwrap().collected(), Ok(&7));

        assert_eq!(views.finish(), Ok(reads));
        assert!(clone.finish().is_err());
        // A finished transaction lets the next one publish; one never
        // collected reads nothing.
        views.publish(old, dlt).unwrap();
        assert_eq!(views.finish(), Ok(SessionReads::default()));
    }
}
