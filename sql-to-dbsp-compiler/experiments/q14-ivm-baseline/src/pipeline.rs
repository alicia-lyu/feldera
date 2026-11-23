//! The two chained circuits in one runtime, as in the Q3 harness's `Pipeline`.
//!
//! A transaction runs first on `base`, which holds the base relations in
//! primary-key input maps, and commits. Its net delta then goes to the view
//! side:
//!
//! * Native: the net delta's Part and Lineitem rows go to the generated view
//!   circuit's keyless tables, whole rows for deletions.
//! * Reconstructed (`merged-index`): the net delta is folded into the index's
//!   `Dlt` and appended (base maintenance); `Q14State` publishes `Old` and
//!   `Dlt`, collects once, and hands the driving deltas to the hand-written
//!   circuit's input handles.
//!
//! A pipeline can be checkpointed into an image and reopened from it, the
//! merged index included (`write_checkpoint`, `reopen`), and can evict the
//! base tables' blocks from the buffer cache after each base transaction
//! (`set_evict_base`), as the Q3 harness does.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::Arc;
#[cfg(feature = "merged-index")]
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
#[cfg(feature = "merged-index")]
use dbsp::circuit::checkpointer::Checkpointer;
#[cfg(feature = "merged-index")]
use dbsp::trace::merged_index::Pass;
use dbsp::trace::merged_index::SessionReads;
use dbsp::{
    circuit::{CircuitConfig, CircuitConstructor},
    dynamic::Erase,
    utils::Tup2,
    DBSPHandle, RootCircuit, Runtime,
};
use dbsp_adapters::{Catalog, CircuitCatalog, DeCollectionStream, RecordFormat};
use feldera_adapterlib::catalog::{SerBatchReader, SerBatchReaderHandle};
use feldera_types::checkpoint::CheckpointMetadata;
use feldera_types::memory_pressure::MemoryPressure;
use feldera_types::program_schema::SqlIdentifier;
#[cfg(feature = "merged-index")]
use merged_index_p::q14::Q14State;
use serde_json::Value;

use crate::data::{Change, Line, Part, Partsupp, Row, ViewRow};
#[cfg(feature = "merged-index")]
use crate::index::{index_rows, ManualIndex};

/// The base tables, by their catalog names.
const BASE_TABLES: [&str; 3] = ["PART", "PARTSUPP", "LINEITEM"];

/// How a deletion names its row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DeleteStyle {
    /// A primary-key table needs only the key (the base circuit's input maps).
    ByKey,
    /// A keyless Z-set needs the whole row (the native view circuit's inputs).
    #[cfg_attr(feature = "merged-index", allow(dead_code))]
    WholeRow,
}

/// The JSON input streams of one circuit's tables. The native view circuit has
/// no Partsupp, since `V` does not read it.
struct Inputs {
    parts: Box<dyn DeCollectionStream>,
    partsupps: Option<Box<dyn DeCollectionStream>>,
    lines: Box<dyn DeCollectionStream>,
    deletes: DeleteStyle,
}

impl Inputs {
    fn new(catalog: &Catalog, deletes: DeleteStyle) -> Result<Self> {
        let input = |name: &str| -> Result<Box<dyn DeCollectionStream>> {
            Ok(catalog
                .input_collection_handle(&SqlIdentifier::from(name))
                .with_context(|| format!("missing input {name}"))?
                .handle
                .configure_deserializer(RecordFormat::Json(Default::default()))?)
        };
        Ok(Self {
            parts: input("PART")?,
            partsupps: match deletes {
                DeleteStyle::ByKey => Some(input("PARTSUPP")?),
                DeleteStyle::WholeRow => None,
            },
            lines: input("LINEITEM")?,
            deletes,
        })
    }

    /// Queues `change` for the circuit's next step.
    fn push(&mut self, change: &Change) -> Result<()> {
        submit(&mut *self.parts, &change.parts, self.deletes)?;
        if let Some(partsupps) = &mut self.partsupps {
            submit(&mut **partsupps, &change.partsupps, self.deletes)?;
        }
        submit(&mut *self.lines, &change.lines, self.deletes)
    }
}

/// Queues `rows` on `stream`: insertions as whole rows, deletions as `deletes`
/// says.
fn submit<T: Row>(
    stream: &mut dyn DeCollectionStream,
    rows: &[(T, bool)],
    deletes: DeleteStyle,
) -> Result<()> {
    for (row, insert) in rows {
        let json = match (insert, deletes) {
            (false, DeleteStyle::ByKey) => row.key_json(),
            _ => row.json(),
        };
        let bytes = serde_json::to_vec(&json)?;
        if *insert {
            stream.insert(&bytes, &None)?;
        } else {
            stream.delete(&bytes, &None)?;
        }
    }
    stream.flush();
    Ok(())
}

/// Runs one transaction of `circuit`, one step per element of `steps`, with
/// `push` called before each. Returns the time spent in `push`, which the
/// caller books apart from the circuit's.
fn run_transaction(
    circuit: &mut DBSPHandle,
    steps: Vec<Change>,
    mut push: impl FnMut(&Change) -> Result<()>,
) -> Result<Duration> {
    let mut pushing = Duration::ZERO;
    circuit.start_transaction()?;
    for step in &steps {
        let started = Instant::now();
        push(step)?;
        pushing += started.elapsed();
        if circuit.step()? {
            bail!("a step of the transaction committed it");
        }
    }
    circuit.commit_transaction()?;
    Ok(pushing)
}

/// One transaction's phases, by the accounting of `experiments/CLAUDE.md`:
/// base maintenance is the base circuit plus the index's upkeep, and IVM is
/// the view's transaction delta plus the view circuit. The other phases are
/// in neither.
#[derive(Clone, Copy, Debug, Default)]
pub struct Timings {
    /// Converting the change into the base circuit's input rows.
    pub preparation: Duration,
    /// The base circuit's transaction, apart from `preparation`.
    pub base_circuit: Duration,
    /// Folding the net delta into `Dlt` and appending it, with the snapshot
    /// `Old` (reconstructed mode). Base maintenance: the index is base storage.
    pub index: Duration,
    /// Reading the net delta, the same in both modes. Harness overhead: one
    /// DBSP circuit would pass it as an in-memory stream.
    pub forward: Duration,
    /// Preparing the view circuit's transaction delta. Native: converting the
    /// net delta into the input rows. Reconstructed: building the driving
    /// deltas from `Dlt` and handing them to the input handles. The two
    /// operations differ, so the report shows this beside `ivm`.
    pub delta: Duration,
    /// The view circuit's transaction, apart from `delta`. Reconstructed, it
    /// starts with the scan session's collection of the touched groups.
    pub view: Duration,
    /// Publishing `Old` and `Dlt`, and finishing the publication
    /// (reconstructed mode).
    pub publication: Duration,
    /// Evicting the base tables' blocks from the buffer cache, after the base
    /// circuit's transaction (`Pipeline::set_evict_base`).
    pub base_eviction: Duration,
}

impl Timings {
    /// Base maintenance: `base_circuit + index`.
    pub fn base(&self) -> Duration {
        self.base_circuit + self.index
    }

    /// Incremental view maintenance: `delta + view`.
    pub fn ivm(&self) -> Duration {
        self.delta + self.view
    }
}

/// A committed transaction of both circuits.
pub struct Committed {
    /// The base circuit's net delta.
    pub net_delta: Change,
    pub timings: Timings,
    /// What the scan session read from the index (reconstructed mode).
    pub session_reads: Option<SessionReads>,
    /// The buffer cache's cost that the base eviction freed.
    pub base_evicted_bytes: usize,
}

/// The base circuit and a view circuit in one runtime, with one storage
/// directory and one buffer cache.
pub struct Pipeline {
    base: DBSPHandle,
    base_catalog: Catalog,
    base_inputs: Inputs,
    view: DBSPHandle,
    view_catalog: Catalog,
    #[cfg(not(feature = "merged-index"))]
    view_inputs: Inputs,
    #[cfg(feature = "merged-index")]
    view_inputs: q14_trie_view_circuit::TrieInputs,
    #[cfg(feature = "merged-index")]
    index: ManualIndex,
    #[cfg(feature = "merged-index")]
    q14_state: Q14State,
    #[cfg(feature = "merged-index")]
    session_pass: Pass,
    /// Whether each transaction evicts the base tables' blocks after the
    /// base circuit's transaction.
    evict_base: bool,
    /// The base Lineitem snapshot that the last transaction's eviction took,
    /// for `order_lines`; reading a base snapshot consumes it.
    base_lines: Option<Arc<dyn SerBatchReader>>,
}

impl Pipeline {
    /// Builds both circuits in one runtime, base first.
    pub fn new(config: CircuitConfig) -> Result<Self> {
        let base_constructor = CircuitConstructor::new(|circuit: &mut RootCircuit| {
            q14_base_circuit::circuit::SqlCircuit::build(circuit).map_err(anyhow::Error::from)
        });
        #[cfg(not(feature = "merged-index"))]
        let view_constructor = CircuitConstructor::new(|circuit: &mut RootCircuit| {
            q14_view_circuit::circuit::SqlCircuit::build(circuit).map_err(anyhow::Error::from)
        });
        // The constructor runs on the view circuit's one worker, which hands
        // its input handles to the driver.
        #[cfg(feature = "merged-index")]
        let q14_state = Q14State::default();
        #[cfg(feature = "merged-index")]
        let view_input_slot = Arc::new(OnceLock::new());
        #[cfg(feature = "merged-index")]
        let view_constructor = {
            let state = q14_state.clone();
            let slot = view_input_slot.clone();
            CircuitConstructor::new(move |circuit: &mut RootCircuit| {
                let (catalog, inputs) = q14_trie_view_circuit::build(circuit, state.clone())
                    .map_err(anyhow::Error::from)?;
                slot.set(inputs)
                    .map_err(|_| anyhow::anyhow!("the view circuit is built once"))?;
                Ok(catalog)
            })
        };

        let mut circuits =
            Runtime::init_circuits(config, vec![base_constructor, view_constructor])?;
        let (view, view_catalog) = circuits.pop().context("missing view circuit")?;
        let (base, base_catalog) = circuits.pop().context("missing base circuit")?;
        // A primary-key table's delta stream stays idle until enabled.
        for name in BASE_TABLES {
            base_catalog
                .output_handles(&SqlIdentifier::from(name))
                .with_context(|| format!("missing base relation {name}"))?
                .enable_count
                .enable();
        }
        let base_inputs = Inputs::new(&base_catalog, DeleteStyle::ByKey)?;
        #[cfg(not(feature = "merged-index"))]
        let view_inputs = Inputs::new(&view_catalog, DeleteStyle::WholeRow)?;
        #[cfg(feature = "merged-index")]
        let view_inputs = view_input_slot
            .get()
            .context("the view circuit did not hand over its input handles")?
            .clone();
        #[cfg(feature = "merged-index")]
        let index = ManualIndex::new(base.runtime())?;
        Ok(Self {
            base,
            base_catalog,
            base_inputs,
            view,
            view_catalog,
            view_inputs,
            #[cfg(feature = "merged-index")]
            index,
            #[cfg(feature = "merged-index")]
            q14_state,
            #[cfg(feature = "merged-index")]
            session_pass: Pass::Seek,
            evict_base: false,
            base_lines: None,
        })
    }

    /// Builds both circuits from the checkpoint `image` of the storage
    /// directory in `config`, and in reconstructed mode restores the merged
    /// index from it. Its outputs are empty until a transaction runs.
    pub fn reopen(mut config: CircuitConfig, image: &CheckpointMetadata) -> Result<Self> {
        config
            .storage
            .as_mut()
            .context("reopening an image needs file storage")?
            .init_checkpoint = Some(image.uuid);
        let pipeline = Self::new(config)?;
        #[cfg(feature = "merged-index")]
        pipeline
            .index
            .restore(Checkpointer::checkpoint_dir(image.uuid))?;
        Ok(pipeline)
    }

    /// Checkpoints both circuits, and in reconstructed mode the merged index,
    /// into one DBSP checkpoint of the runtime's storage directory, and
    /// publishes it: the image that [`Self::reopen`] reads. It must run
    /// between transactions.
    pub fn write_checkpoint(&mut self) -> Result<CheckpointMetadata> {
        let committer = DBSPHandle::checkpoint_circuits(&mut [&mut self.base, &mut self.view])?;
        // The index is state outside the circuits: it is saved into the
        // checkpoint's directory before the commit, which then protects its
        // files.
        #[cfg(feature = "merged-index")]
        self.index
            .save(Checkpointer::checkpoint_dir(committer.uuid()))?;
        Ok(committer.commit()?.publish()?)
    }

    /// Makes each transaction evict the base tables' blocks from the buffer
    /// cache once the base circuit's transaction has committed, so that the
    /// view phase finds only view state and the index there (Q3 measurement
    /// version 16). Loading does not evict.
    pub fn set_evict_base(&mut self, evict: bool) {
        self.evict_base = evict;
    }

    /// DBSP's memory pressure level now.
    pub fn memory_pressure(&self) -> MemoryPressure {
        self.base.runtime().current_memory_pressure()
    }

    /// Whether every background merge is idle: both circuits', and the
    /// index's in reconstructed mode.
    pub fn is_background_merge_idle(&mut self) -> Result<bool> {
        let idle = self.base.is_background_merge_idle()? && self.view.is_background_merge_idle()?;
        #[cfg(feature = "merged-index")]
        let idle = idle && self.index.is_background_merge_idle()?;
        Ok(idle)
    }

    /// Makes the partner reads check that every key of the circuit's delta is
    /// one of their reader's requests (reconstructed mode).
    pub fn enable_contract_check(&self) {
        #[cfg(feature = "merged-index")]
        self.q14_state.enable_contract_check();
    }

    /// How the scan session passes untouched groups (reconstructed mode).
    #[cfg(feature = "merged-index")]
    pub fn set_session_pass(&mut self, pass: Pass) {
        self.session_pass = pass;
    }

    /// Runs `change` as one transaction of each circuit, base first, and
    /// returns the base circuit's net delta with the phases' times. With
    /// `stepped`, each circuit's transaction has three steps
    /// (`Change::steps`); the view circuit takes the net delta in the same
    /// steps.
    pub fn transact(&mut self, change: Change, stepped: bool) -> Result<Committed> {
        let mut timings = Timings::default();
        self.base_lines = None;
        let steps = change.steps(stepped);
        let base_inputs = &mut self.base_inputs;
        let started = Instant::now();
        timings.preparation =
            run_transaction(&mut self.base, steps, |step| base_inputs.push(step))?;
        timings.base_circuit = started.elapsed().saturating_sub(timings.preparation);

        let started = Instant::now();
        let net_delta = self.net_delta()?;
        timings.forward = started.elapsed();

        // Before the index upkeep, which reads the index, not the base tables.
        let mut base_evicted_bytes = 0;
        if self.evict_base {
            let started = Instant::now();
            base_evicted_bytes = self.evict_base_blocks()?;
            timings.base_eviction = started.elapsed();
        }

        let session_reads = self.run_view(&net_delta, stepped, &mut timings)?;
        Ok(Committed {
            net_delta,
            timings,
            session_reads,
            base_evicted_bytes,
        })
    }

    /// Takes the base tables' snapshots, evicts every block of their files
    /// from the runtime's buffer cache in one pass, and keeps the Lineitem
    /// snapshot for `order_lines`. Returns the cache cost freed.
    fn evict_base_blocks(&mut self) -> Result<usize> {
        let mut files = HashSet::new();
        for name in BASE_TABLES {
            let snapshot = self.base_snapshot(name)?;
            files.extend(
                snapshot
                    .batches()
                    .iter()
                    .filter_map(|batch| batch.file_reader())
                    .map(|file| file.file_id()),
            );
            if name == "LINEITEM" {
                self.base_lines = Some(snapshot);
            }
        }
        Ok(self
            .base
            .runtime()
            .foreground_buffer_cache(0)
            .evict_files(&files))
    }

    /// The base relation `name` after the last transaction. Taking a base
    /// snapshot consumes it, so take one per relation per transaction.
    fn base_snapshot(&self, name: &str) -> Result<Arc<dyn SerBatchReader>> {
        Ok(self
            .base_catalog
            .output_handles(&SqlIdentifier::from(name))
            .with_context(|| format!("missing base relation {name}"))?
            .integrate_handle
            .as_ref()
            .with_context(|| format!("base relation {name} is not materialized"))?
            .concat()
            .snapshot())
    }

    #[cfg(not(feature = "merged-index"))]
    fn run_view(
        &mut self,
        net_delta: &Change,
        stepped: bool,
        timings: &mut Timings,
    ) -> Result<Option<SessionReads>> {
        let steps = net_delta.clone().steps(stepped);
        let view_inputs = &mut self.view_inputs;
        let started = Instant::now();
        timings.delta = run_transaction(&mut self.view, steps, |step| view_inputs.push(step))?;
        timings.view = started.elapsed().saturating_sub(timings.delta);
        Ok(None)
    }

    /// Appends the net delta to the index and publishes `Old` and `Dlt`. The
    /// session collects once, before the transaction; its steps share it.
    /// Returns what the session read.
    #[cfg(feature = "merged-index")]
    fn run_view(
        &mut self,
        net_delta: &Change,
        stepped: bool,
        timings: &mut Timings,
    ) -> Result<Option<SessionReads>> {
        let started = Instant::now();
        let (old, dlt) = self.index.append(index_rows(net_delta)?)?;
        timings.index = started.elapsed();

        let started = Instant::now();
        self.q14_state
            .publish(old, dlt)
            .map_err(anyhow::Error::msg)?;
        timings.publication = started.elapsed();

        // The collection's preparation of the driving deltas is `delta`; the
        // rest of it, the scan, is the view's.
        let started = Instant::now();
        let collection = self
            .base
            .runtime()
            .with_buffer_cache(0, || self.q14_state.collect(self.session_pass))
            .map_err(anyhow::Error::msg)?;
        let scan = started
            .elapsed()
            .saturating_sub(collection.delta_preparation);
        let started = Instant::now();
        self.view_inputs.set(collection.driving);
        timings.delta = collection.delta_preparation + started.elapsed();

        // The inputs are already set: each step pushes nothing.
        let started = Instant::now();
        run_transaction(&mut self.view, Change::default().steps(stepped), |_| Ok(()))?;
        timings.view = scan + started.elapsed();

        let started = Instant::now();
        let reads = self.q14_state.finish().map_err(anyhow::Error::msg)?;
        timings.publication += started.elapsed();
        Ok(Some(reads))
    }

    /// Reads, and so drains, the base circuit's delta streams: the net change
    /// of each base relation in the last transaction. An upsert shows as a
    /// deletion of the old row and an insertion of the new one.
    fn net_delta(&self) -> Result<Change> {
        Ok(Change {
            parts: self.delta_rows("PART", Part::parse)?,
            partsupps: self.delta_rows("PARTSUPP", Partsupp::parse)?,
            lines: self.delta_rows("LINEITEM", Line::parse)?,
        })
    }

    fn delta_rows<T>(
        &self,
        name: &str,
        parse: impl Fn(&Value) -> Result<T>,
    ) -> Result<Vec<(T, bool)>> {
        let handle = self
            .base_catalog
            .output_handles(&SqlIdentifier::from(name))
            .with_context(|| format!("missing base relation {name}"))?;
        read_rows(&*handle.delta_handle, Rows::MapValues, parse)?
            .into_iter()
            .map(|(row, weight)| match weight {
                1 => Ok((row, true)),
                -1 => Ok((row, false)),
                _ => bail!("{name} delta has weight {weight}; primary keys allow only +1 and -1"),
            })
            .collect()
    }

    /// The line items of the orders in `order_keys`, from the base circuit's
    /// Lineitem after the last transaction: RF2's rows. Both modes find them
    /// here, since MI_P is keyed by part and cannot find an order's line
    /// items. The reads go through the runtime's buffer cache.
    ///
    /// It uses the snapshot that the last transaction's base eviction took, or
    /// else takes one. Taking a base snapshot consumes it, so call this at
    /// most once per transaction.
    pub fn order_lines(&mut self, order_keys: &BTreeSet<i64>) -> Result<Vec<Line>> {
        let lines = match self.base_lines.take() {
            Some(lines) => lines,
            None => self.base_snapshot("LINEITEM")?,
        };
        self.base.runtime().with_buffer_cache(0, || {
            let mut cursor = lines.cursor(RecordFormat::Json(Default::default()))?;
            let mut found = Vec::new();
            for &order_key in order_keys {
                cursor.seek_key(Tup2(order_key, i32::MIN).erase());
                while cursor.key_valid() {
                    let line = Line::parse(&cursor.val_to_json()?)?;
                    if line.order_key != order_key {
                        break;
                    }
                    found.push(line);
                    cursor.step_key();
                }
            }
            Ok(found)
        })
    }

    /// The last transaction's change of `V`, and `V` after it. `V` is as
    /// large as Lineitem, so only verification reads it.
    pub fn view_output(&self) -> Result<(BTreeMap<ViewRow, i64>, BTreeMap<ViewRow, i64>)> {
        let delta = self.view_delta()?;
        let view = self.view_handles()?.1;
        let view = consolidated(read_rows(&*view, Rows::ZSetKeys, ViewRow::parse)?);
        Ok((delta, view))
    }

    /// The last transaction's change of `V`. `V`'s snapshot is left unread,
    /// and the next transaction replaces it.
    pub fn view_delta(&self) -> Result<BTreeMap<ViewRow, i64>> {
        let (delta, _) = self.view_handles()?;
        Ok(consolidated(read_rows(
            &*delta,
            Rows::ZSetKeys,
            ViewRow::parse,
        )?))
    }

    /// `V`'s delta and materialized output handles.
    fn view_handles(
        &self,
    ) -> Result<(Box<dyn SerBatchReaderHandle>, Arc<dyn SerBatchReaderHandle>)> {
        let output = self
            .view_catalog
            .output_handles(&SqlIdentifier::from("V"))
            .context("missing V output")?;
        let view = output
            .integrate_handle
            .as_ref()
            .context("V has no materialized snapshot")?;
        Ok((output.delta_handle.clone(), view.clone()))
    }
}

/// Where a batch keeps its rows.
#[derive(Clone, Copy)]
enum Rows {
    /// A primary-key map: the values.
    MapValues,
    /// A Z-set: the keys.
    ZSetKeys,
}

/// Every row of a batch with its weight.
fn read_rows<T>(
    handle: &dyn SerBatchReaderHandle,
    rows_in: Rows,
    parse: impl Fn(&Value) -> Result<T>,
) -> Result<Vec<(T, i64)>> {
    let reader = handle.concat();
    let mut cursor = reader.cursor(RecordFormat::Json(Default::default()))?;
    let mut rows = Vec::new();
    while cursor.key_valid() {
        match rows_in {
            Rows::MapValues => {
                while cursor.val_valid() {
                    rows.push((parse(&cursor.val_to_json()?)?, cursor.weight()));
                    cursor.step_val();
                }
            }
            Rows::ZSetKeys => rows.push((parse(&cursor.key_to_json()?)?, cursor.weight())),
        }
        cursor.step_key();
    }
    Ok(rows)
}

fn consolidated<T: Ord>(rows: Vec<(T, i64)>) -> BTreeMap<T, i64> {
    let mut result = BTreeMap::new();
    for (row, weight) in rows {
        *result.entry(row).or_default() += weight;
    }
    result.retain(|_, weight| *weight != 0);
    result
}
