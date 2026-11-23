mod base_state;
mod data;
mod diagnostics;
mod measurement;
#[cfg(feature = "merged-index")]
mod reconstruction;
mod run_counts;
#[cfg(test)]
mod verification;
mod window_memory;
mod workload;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::Ordering;
#[cfg(feature = "merged-index")]
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use data::{Customer, Line, Order};
#[cfg(feature = "merged-index")]
use dbsp::circuit::checkpointer::Checkpointer;
use dbsp::circuit::compaction_jobs::{CompactionJobStats, DEFAULT_COMPACTION_JOBS};
use dbsp::circuit::metadata::{MetaItem, CIRCUIT_CPU_TIME_SECONDS};
use dbsp::circuit::{
    CircuitConfig, CircuitConstructor, CircuitStorageConfig, GlobalNodeId, StorageCacheConfig,
    StorageConfig, StorageOptions,
};
use dbsp::monitor::visual_graph::CircuitNodeStyle;
use dbsp::storage::attribution::Component;
use dbsp::storage::buffer_cache::ReservationStats;
use dbsp::storage::file::writer::{
    ELIDED_VALUE_HEADER_BLOCKS, FRONT_CODED_BLOCKS, FRONT_CODING_LAYOUT_FALLBACKS,
    FRONT_CODING_SIZE_FALLBACKS, VALUE_ELISION_LAYOUT_FALLBACKS, VALUE_ELISION_SIZE_FALLBACKS,
};
use dbsp::trace::merged_index::Pass;
#[cfg(feature = "merged-index")]
use dbsp::trace::with_read_owner;
use dbsp::trace::{BlockExpansions, RunReads};
use dbsp::{DBSPHandle, RootCircuit, Runtime};
use dbsp_adapters::{Catalog, CircuitCatalog, RecordFormat};
use feldera_types::checkpoint::CheckpointMetadata;
use feldera_types::config::{CompactionMemory, MemoryModel};
use feldera_types::program_schema::SqlIdentifier;
#[cfg(feature = "merged-index")]
use merged_index_col::q3::{Collection, DrivingDeltas, Q3State, SessionReads};
#[cfg(feature = "merged-index")]
use reconstruction::{ManualIndex, OrphanLines};
use serde_json::{json, Value};
use size_of::SizeOf;

// The generated circuit crates define no allocator, because a binary links only one. These
// are the definitions that the compiler emits at the top of every generated circuit.
#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;
#[allow(non_upper_case_globals)]
#[export_name = "malloc_conf"]
pub static malloc_conf: &[u8] = b"prof:true,prof_active:true,lg_prof_sample:19\0";

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, serde::Serialize)]
struct Joined {
    customer_key: i64,
    segment: String,
    order_key: i64,
    order_date: String,
    ship_priority: i32,
    line_number: i32,
    price_cents: i64,
    discount_hundredths: i64,
    ship_date: String,
}

fn field<'a>(row: &'a Value, name: &str) -> Result<&'a Value> {
    let map = row.as_object().context("output row must be an object")?;
    map.iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value)
        .with_context(|| format!("missing output field {name}"))
}

fn integer(row: &Value, name: &str) -> Result<i64> {
    field(row, name)?
        .as_i64()
        .with_context(|| format!("{name} must be an integer"))
}

fn string(row: &Value, name: &str) -> Result<String> {
    field(row, name)?
        .as_str()
        .map(str::to_owned)
        .with_context(|| format!("{name} must be a string"))
}

fn cents(row: &Value, name: &str) -> Result<i64> {
    let value = field(row, name)?;
    let text = value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| value.to_string());
    data::decimal_hundredths(&text, false).with_context(|| format!("invalid decimal {name}"))
}

impl Joined {
    fn from_output(row: &Value) -> Result<Self> {
        Ok(Self {
            customer_key: integer(row, "c_custkey")?,
            segment: string(row, "c_mktsegment")?,
            order_key: integer(row, "o_orderkey")?,
            order_date: string(row, "o_orderdate")?,
            ship_priority: i32::try_from(integer(row, "o_shippriority")?)?,
            line_number: i32::try_from(integer(row, "l_linenumber")?)?,
            price_cents: cents(row, "l_extendedprice")?,
            discount_hundredths: cents(row, "l_discount")?,
            ship_date: string(row, "l_shipdate")?,
        })
    }

    fn new(c: &Customer, o: &Order, l: &Line) -> Self {
        Self {
            customer_key: c.key,
            segment: c.segment.clone(),
            order_key: o.key,
            order_date: o.date.clone(),
            ship_priority: o.ship_priority,
            line_number: l.number,
            price_cents: l.price_cents,
            discount_hundredths: l.discount_hundredths,
            ship_date: l.ship_date.clone(),
        }
    }
}

#[derive(Clone, Default, SizeOf)]
struct Reference {
    customers: HashMap<i64, Customer>,
    orders: HashMap<i64, Order>,
    lines: HashMap<(i64, i32), Line>,
}

impl Reference {
    fn evaluate(&self) -> BTreeMap<Joined, i64> {
        let mut result = BTreeMap::new();
        for line in self.lines.values() {
            if let Some(order) = self.orders.get(&line.order_key) {
                if let Some(customer) = self.customers.get(&order.customer_key) {
                    *result
                        .entry(Joined::new(customer, order, line))
                        .or_default() += 1;
                }
            }
        }
        result
    }
}

#[derive(Clone, Default)]
struct Change {
    customers: Vec<(Customer, bool)>,
    orders: Vec<(Order, bool)>,
    lines: Vec<(Line, bool)>,
}

impl Change {
    fn len(&self) -> usize {
        self.customers.len() + self.orders.len() + self.lines.len()
    }

    fn apply_optional(self, mut state: Option<&mut Reference>, input: &mut Inputs) -> Result<()> {
        for (row, insert) in self.customers {
            input.customer(&row, insert)?;
            if let Some(state) = state.as_deref_mut() {
                if insert {
                    state.customers.insert(row.key, row);
                } else {
                    state.customers.remove(&row.key);
                }
            }
        }
        for (row, insert) in self.orders {
            input.order(&row, insert)?;
            if let Some(state) = state.as_deref_mut() {
                if insert {
                    state.orders.insert(row.key, row);
                } else {
                    state.orders.remove(&row.key);
                }
            }
        }
        for (row, insert) in self.lines {
            input.line(&row, insert)?;
            let key = (row.order_key, row.number);
            if let Some(state) = state.as_deref_mut() {
                if insert {
                    state.lines.insert(key, row);
                } else {
                    state.lines.remove(&key);
                }
            }
        }
        input.flush();
        Ok(())
    }
}

/// How a deletion names its row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DeleteStyle {
    /// A primary-key table needs only the key (the base circuit's input maps).
    ByKey,
    /// A keyless Z-set needs the whole row, because the delete is a `-1` of that row
    /// (the view circuit's inputs).
    WholeRow,
}

/// The JSON input streams of one circuit's three tables.
struct Inputs {
    customers: Box<dyn dbsp_adapters::DeCollectionStream>,
    orders: Box<dyn dbsp_adapters::DeCollectionStream>,
    lines: Box<dyn dbsp_adapters::DeCollectionStream>,
    deletes: DeleteStyle,
}

impl Inputs {
    fn new(catalog: &impl CircuitCatalog, deletes: DeleteStyle) -> Result<Self> {
        let input = |name: &str| -> Result<Box<dyn dbsp_adapters::DeCollectionStream>> {
            Ok(catalog
                .input_collection_handle(&SqlIdentifier::from(name))
                .with_context(|| format!("missing input {name}"))?
                .handle
                .configure_deserializer(RecordFormat::Json(Default::default()))?)
        };
        Ok(Self {
            customers: input("CUSTOMER")?,
            orders: input("ORDERS")?,
            lines: input("LINEITEM")?,
            deletes,
        })
    }

    fn submit(
        stream: &mut dyn dbsp_adapters::DeCollectionStream,
        row: Value,
        insert: bool,
    ) -> Result<()> {
        let bytes = serde_json::to_vec(&row)?;
        if insert {
            stream.insert(&bytes, &None)?;
        } else {
            stream.delete(&bytes, &None)?;
        }
        Ok(())
    }

    fn whole_row(&self, insert: bool) -> bool {
        insert || self.deletes == DeleteStyle::WholeRow
    }

    fn customer(&mut self, row: &Customer, insert: bool) -> Result<()> {
        let json = if self.whole_row(insert) {
            row.json()
        } else {
            json!({"c_custkey": row.key})
        };
        Self::submit(&mut *self.customers, json, insert)
    }
    fn order(&mut self, row: &Order, insert: bool) -> Result<()> {
        let json = if self.whole_row(insert) {
            row.json()
        } else {
            json!({"o_orderkey": row.key})
        };
        Self::submit(&mut *self.orders, json, insert)
    }
    fn line(&mut self, row: &Line, insert: bool) -> Result<()> {
        let json = if self.whole_row(insert) {
            row.json()
        } else {
            json!({"l_orderkey": row.order_key, "l_linenumber": row.number})
        };
        Self::submit(&mut *self.lines, json, insert)
    }
    fn flush(&mut self) {
        self.customers.flush();
        self.orders.flush();
        self.lines.flush();
    }
}

/// One circuit with its catalog and, if it has them, the JSON input streams of its tables. The
/// view circuit of the reconstructed build has none: its inputs are typed handles, fed from the
/// transaction's `Dlt`.
struct Stage {
    circuit: dbsp::DBSPHandle,
    catalog: Catalog,
    inputs: Option<Inputs>,
}

impl Stage {
    fn new((circuit, catalog): (dbsp::DBSPHandle, Catalog), deletes: DeleteStyle) -> Result<Self> {
        let inputs = Inputs::new(&catalog, deletes)?;
        Ok(Self {
            circuit,
            catalog,
            inputs: Some(inputs),
        })
    }

    /// A circuit whose inputs are not reached through its catalog.
    #[cfg(feature = "merged-index")]
    fn without_json_inputs((circuit, catalog): (dbsp::DBSPHandle, Catalog)) -> Self {
        Self {
            circuit,
            catalog,
            inputs: None,
        }
    }

    /// The JSON input streams of the circuit's tables.
    fn inputs(&mut self) -> &mut Inputs {
        self.inputs
            .as_mut()
            .expect("this circuit's inputs are not reached through JSON streams")
    }
}

/// The two chained circuits, each timed on its own.
///
/// Both circuits run in one DBSP runtime, with one storage directory and one buffer cache. The
/// separation between them is the transaction: a transaction runs first on `base`, which holds
/// the base relations in primary-key input maps, and commits. The net change of those relations
/// is then forwarded to `view`, which maintains `V`. In reconstructed mode the same net change
/// also feeds the merged index, which stands in for the view circuit's join traces: base
/// maintenance turns the net delta into the index's `Dlt`, and the view circuit, a hand-written
/// one (`q3-trie-view-circuit`), takes its inputs from `Dlt` and reads the index's touched
/// groups through the transaction's scan session. The index is base storage, written through
/// the runtime's buffer cache, which the session's reads use too.
/// The file, in an image's checkpoint directory, that holds the merged index's side store of
/// orphan lines (`OrphanLines`).
#[cfg(feature = "merged-index")]
const ORPHAN_LINES_FILE: &str = "q3-orphan-lines.json";

struct Pipeline {
    base: Stage,
    view: Stage,
    /// Whether each transaction reads the circuits' CPU profiles (off while loading).
    profile_cpu: bool,
    /// Whether each transaction evicts the base tables' blocks after the base circuit's
    /// transaction (`evict_base_blocks`): on for the refreshes and their warm-up, off while
    /// loading, where it would only slow the load.
    evict_base: bool,
    /// The base snapshot that the last transaction's eviction took; see `take_base_state`.
    evicted_base: Option<workload::BaseState>,
    #[cfg(feature = "merged-index")]
    index: ManualIndex,
    /// Line items whose order is absent, beside the index (base maintenance).
    #[cfg(feature = "merged-index")]
    orphans: OrphanLines,
    #[cfg(feature = "merged-index")]
    q3_state: Q3State,
    /// How the view phase's scan session passes untouched groups (`--session-pass`).
    #[cfg(feature = "merged-index")]
    session_pass: Pass,
    /// The view circuit's input handles, which take each transaction's driving deltas.
    #[cfg(feature = "merged-index")]
    view_inputs: q3_trie_view_circuit::TrieInputs,
    /// The buffer cache that the view circuit's worker saw when it built the circuit, for the
    /// test that the index's thread shares it.
    #[cfg(all(test, feature = "merged-index"))]
    view_worker_cache: Arc<OnceLock<Arc<dbsp::storage::buffer_cache::BufferCache>>>,
}

impl Pipeline {
    /// Makes the partner reads check that every key of the circuit's delta is one of their
    /// reader's requests (reconstructed mode; nothing in native mode). Tests and validation
    /// runs enable it. A measured run (`--no-verify`) never does, so no measured counter
    /// includes the check's walk of the circuit's delta.
    fn enable_contract_check(&self) {
        #[cfg(feature = "merged-index")]
        self.q3_state.enable_contract_check();
    }

    /// Builds both circuits in one runtime, base first. The CPU profiler is enabled in both.
    /// `index_prefix_filter` gives the merged index's files a prefix filter over the customer
    /// group in place of the whole-key filter (`--index-prefix-filter`); native mode ignores it.
    #[cfg_attr(not(feature = "merged-index"), allow(unused_variables))]
    fn new(config: CircuitConfig, index_prefix_filter: bool) -> Result<Self> {
        #[cfg(feature = "merged-index")]
        let q3_state = Q3State::default();
        let base_constructor = CircuitConstructor::new(|circuit: &mut RootCircuit| {
            q3_base_circuit::circuit::SqlCircuit::build(circuit).map_err(anyhow::Error::from)
        });
        #[cfg(feature = "merged-index")]
        let view_state = q3_state.clone();
        #[cfg(all(test, feature = "merged-index"))]
        let view_worker_cache = Arc::new(OnceLock::new());
        #[cfg(all(test, feature = "merged-index"))]
        let probe = view_worker_cache.clone();
        // The constructor runs on the view circuit's worker, which hands its input handles to
        // the driver. The reconstructed build runs one worker, so the circuit is built once.
        #[cfg(feature = "merged-index")]
        let view_input_slot = Arc::new(OnceLock::new());
        #[cfg(feature = "merged-index")]
        let view_input_probe = view_input_slot.clone();
        #[cfg(feature = "merged-index")]
        let view_constructor = CircuitConstructor::new(move |circuit: &mut RootCircuit| {
            #[cfg(test)]
            probe
                .set(Runtime::buffer_cache().expect("the worker has a buffer cache"))
                .expect("the view circuit is built once");
            let (catalog, inputs) = q3_trie_view_circuit::build(circuit, view_state.clone())
                .map_err(anyhow::Error::from)?;
            view_input_probe
                .set(inputs)
                .map_err(|_| anyhow::anyhow!("the view circuit is built once"))?;
            Ok(catalog)
        });
        #[cfg(not(feature = "merged-index"))]
        let view_constructor = CircuitConstructor::new(|circuit: &mut RootCircuit| {
            q3_view_circuit::circuit::SqlCircuit::build(circuit).map_err(anyhow::Error::from)
        });
        let mut circuits =
            Runtime::init_circuits(config, vec![base_constructor, view_constructor])?;
        let view_circuit = circuits.pop().context("missing view circuit")?;
        let base_circuit = circuits.pop().context("missing base circuit")?;
        let mut base = Stage::new(base_circuit, DeleteStyle::ByKey)?;
        workload::enable_net_delta(&base.catalog)?;
        #[cfg(feature = "merged-index")]
        let mut view = Stage::without_json_inputs(view_circuit);
        #[cfg(not(feature = "merged-index"))]
        let mut view = Stage::new(view_circuit, DeleteStyle::WholeRow)?;
        base.circuit.enable_cpu_profiler()?;
        view.circuit.enable_cpu_profiler()?;
        // The index is secondary storage of the base relations: its upkeep is base maintenance
        // (its thread binds the runtime's buffer cache). The view circuit reads it only through
        // the snapshots that `Q3State` publishes.
        #[cfg(feature = "merged-index")]
        let index = ManualIndex::new(base.circuit.runtime(), index_prefix_filter)?;
        #[cfg(feature = "merged-index")]
        let view_inputs = view_input_slot
            .get()
            .context("the view circuit did not hand over its input handles")?
            .clone();
        Ok(Self {
            base,
            view,
            profile_cpu: false,
            evict_base: false,
            evicted_base: None,
            #[cfg(feature = "merged-index")]
            index,
            #[cfg(feature = "merged-index")]
            orphans: OrphanLines::default(),
            #[cfg(feature = "merged-index")]
            q3_state,
            #[cfg(feature = "merged-index")]
            session_pass: Pass::Seek,
            #[cfg(feature = "merged-index")]
            view_inputs,
            #[cfg(all(test, feature = "merged-index"))]
            view_worker_cache,
        })
    }

    /// Checkpoints both circuits, and in reconstructed mode the merged index, into one DBSP
    /// checkpoint of the runtime's storage directory, and publishes it. This is the image that
    /// a refresh process reopens ([`Self::reopen`]). It must run between transactions.
    fn write_checkpoint(&mut self) -> Result<CheckpointMetadata> {
        let committer =
            DBSPHandle::checkpoint_circuits(&mut [&mut self.base.circuit, &mut self.view.circuit])?;
        // The index and its side store of orphan lines are state outside the circuits: they
        // are saved into the checkpoint's directory before the commit, which then protects
        // their files.
        #[cfg(feature = "merged-index")]
        {
            let directory = Checkpointer::checkpoint_dir(committer.uuid());
            self.index.save(directory.clone())?;
            self.runtime()
                .backend()?
                .write_json(&directory.join(ORPHAN_LINES_FILE), &self.orphans)?
                .commit()?;
        }
        Ok(committer.commit()?.publish()?)
    }

    /// Builds both circuits from the checkpoint `image` of the storage directory in `config`,
    /// and in reconstructed mode restores the merged index from it. Like a freshly loaded
    /// pipeline, it needs [`republish`] before its first refresh.
    fn reopen(
        mut config: CircuitConfig,
        image: &CheckpointMetadata,
        index_prefix_filter: bool,
    ) -> Result<Self> {
        config
            .storage
            .as_mut()
            .context("reopening an image needs file storage")?
            .init_checkpoint = Some(image.uuid);
        #[cfg_attr(not(feature = "merged-index"), allow(unused_mut))]
        let mut pipeline = Self::new(config, index_prefix_filter)?;
        #[cfg(feature = "merged-index")]
        {
            let directory = Checkpointer::checkpoint_dir(image.uuid);
            pipeline.index.restore(directory.clone())?;
            pipeline.orphans = pipeline
                .runtime()
                .backend()?
                .read_json(&directory.join(ORPHAN_LINES_FILE))?;
        }
        Ok(pipeline)
    }

    /// The runtime that hosts both circuits.
    fn runtime(&self) -> &Runtime {
        self.base.circuit.runtime()
    }

    /// Takes the transaction's base snapshot, evicts every block of the base tables' current
    /// files (CUSTOMER, ORDERS, LINEITEM) from the runtime's buffer cache in one pass, and keeps
    /// the snapshot for the caller's next `BaseState`. Returns the cache cost freed. Both
    /// circuits and the merged index share that cache.
    fn evict_base_blocks(&mut self) -> Result<usize> {
        let base = workload::BaseState::new(&self.base.catalog)?;
        let freed = self
            .runtime()
            .foreground_buffer_cache(0)
            .evict_files(&base.file_ids());
        self.evicted_base = Some(base);
        Ok(freed)
    }

    /// The base state after the last transaction: the snapshot its eviction took, or else a
    /// new one. A base snapshot may be taken once per transaction, since reading one consumes
    /// it.
    fn take_base_state(&mut self) -> Result<workload::BaseState> {
        match self.evicted_base.take() {
            Some(base) => Ok(base),
            None => workload::BaseState::new(&self.base.catalog),
        }
    }

    /// Runs `f` with the runtime's buffer cache bound to the calling thread, so that the
    /// driver's reads of the circuits' files go through the same cache as the workers' reads.
    fn in_cache<R>(&self, f: impl FnOnce() -> R) -> R {
        self.runtime().with_buffer_cache(0, f)
    }

    /// Both circuits with `workers` workers and memory-backed state.
    #[cfg(test)]
    fn with_workers(workers: usize) -> Result<Self> {
        Self::new(CircuitConfig::with_workers(workers), false)
    }
}

/// The view maintenance of the reconstructed build, which the driver runs on its own thread
/// with the runtime's buffer cache bound.
#[cfg(feature = "merged-index")]
impl Pipeline {
    /// Runs the transaction's scan session over the published `Old` and `Dlt`, which seals the
    /// readers of the touched groups until the publication is finished. This is the
    /// transaction's only read of the index in the view phase. Returns the driving deltas, and
    /// the time the collection spent preparing them from `Dlt` (phase A), which belongs to
    /// `delta_ms`, not to the scan's `q3_ms`.
    fn collect_groups(&self) -> Result<Collection> {
        self.in_cache(|| self.q3_state.collect(self.session_pass))
            .map_err(anyhow::Error::msg)
    }

    /// Hands the prepared transaction delta to the view circuit: the part of `delta_ms` after
    /// the collection (phase A's part is in the `Collection` that `collect_groups` returned).
    fn build_view_delta(&self, driving: DrivingDeltas) -> Result<()> {
        self.feed_driving_deltas(driving)
    }

    /// Hands the driving deltas, which the collection moved out of `Q3State`, to the view
    /// circuit's input handles.
    fn feed_driving_deltas(&self, driving: DrivingDeltas) -> Result<()> {
        self.view_inputs.set(driving);
        Ok(())
    }
}

/// Completes the net delta into the rows of the index's `Dlt` (see `reconstruction::prepare`).
/// The reads of `Old` are base maintenance, counted under the merged index's read owner; the
/// base state's reads of an order's customer are counted under the base relations'. `base`
/// names the customer of an order that the delta mentions only through its lines, or else
/// `reference` does; one of them must be given then. Returns the rows, and the run reads and block
/// expansions of the index.
#[cfg(feature = "merged-index")]
fn complete_net_delta(
    pipeline: &mut Pipeline,
    net_delta: &Change,
    base: Option<&workload::BaseState>,
    reference: Option<&Reference>,
) -> Result<(Vec<reconstruction::SourceRow>, RunReads, BlockExpansions)> {
    let runtime = pipeline.runtime().clone();
    runtime.with_buffer_cache(0, || {
        let reads_before = RunReads::current_for(Component::MergedIndex);
        let expansions_before = BlockExpansions::current_for(Component::MergedIndex);
        let old = pipeline.index.snapshot()?;
        let mut base_owner = |order: i64| -> Result<Option<i64>> {
            if let Some(base) = base {
                let found = with_read_owner(Component::BaseRelation, || base.order(order))?;
                return Ok(found.map(|order| order.customer_key));
            }
            let reference = reference.context(
                "a net delta that names an order only through its lines needs the base state \
                 or a reference to say whose order it is",
            )?;
            Ok(reference.orders.get(&order).map(|order| order.customer_key))
        };
        let rows = with_read_owner(Component::MergedIndex, || {
            reconstruction::prepare(net_delta, &old, &mut pipeline.orphans, &mut base_owner)
        })?;
        let reads = RunReads::current_for(Component::MergedIndex).since(reads_before);
        let expansions =
            BlockExpansions::current_for(Component::MergedIndex).since(expansions_before);
        Ok((rows, reads, expansions))
    })
}

/// A circuit's cumulative CPU time inside steps, from the CPU profiler. The profile is read
/// outside the phase timers.
fn profiled_cpu(circuit: &mut DBSPHandle) -> Result<Duration> {
    let profile = circuit.retrieve_profile()?;
    let worker = profile
        .worker_profiles
        .first()
        .context("the circuit has no worker profile")?;
    match worker
        .attribute_profile(&CIRCUIT_CPU_TIME_SECONDS)
        .get(&GlobalNodeId::root())
    {
        Some(MetaItem::Duration(cpu)) => Ok(*cpu),
        other => bail!("the root circuit's CPU time is {other:?}, not a duration"),
    }
}

fn read_output(
    handle: &dyn feldera_adapterlib::catalog::SerBatchReaderHandle,
) -> Result<BTreeMap<Joined, i64>> {
    let reader = handle.concat();
    let mut cursor = reader.cursor(RecordFormat::Json(Default::default()))?;
    let mut result = BTreeMap::new();
    while cursor.key_valid() {
        let row = Joined::from_output(&cursor.key_to_json()?)?;
        let weight = cursor.weight();
        *result.entry(row).or_default() += weight;
        cursor.step_key();
    }
    result.retain(|_, weight| *weight != 0);
    Ok(result)
}

fn difference(
    before: &BTreeMap<Joined, i64>,
    after: &BTreeMap<Joined, i64>,
) -> BTreeMap<Joined, i64> {
    let mut delta = BTreeMap::new();
    for (row, weight) in before {
        *delta.entry(row.clone()).or_default() -= weight;
    }
    for (row, weight) in after {
        *delta.entry(row.clone()).or_default() += weight;
    }
    delta.retain(|_, weight| *weight != 0);
    delta
}

#[derive(Default)]
struct Measurements {
    /// Reading refresh files, validating, and loading the change into the base circuit.
    preparation: Duration,
    /// The base circuit's transaction alone. Reported inside `base_ms`; see `base_phase`.
    base_circuit: Duration,
    /// Reading the base tables' net delta (`workload::net_delta`), the same operation in both
    /// modes. Not part of `ivm_ms`; see `delta`.
    forward: Duration,
    /// Evicting the base tables' blocks from the buffer cache after the base circuit's
    /// transaction (measurement version 16; decision of 2026-11-18), in neither `base_ms` nor
    /// `ivm_ms`: neither view reads the base tables, whose upkeep is assumed amortized over the
    /// views an index serves.
    base_eviction: Duration,
    /// The cache cost those evictions freed.
    base_evicted_bytes: usize,
    /// Preparing the view circuit's transaction delta, which is view work (measurement version
    /// 11) and a part of `ivm_ms`. Native: converting the net delta into the input handles'
    /// rows. Reconstructed: building the driving deltas from `Dlt` and handing them to the
    /// input handles (`Pipeline::build_view_delta`). The two operations are not identical, so
    /// the report shows `delta_ms` beside `ivm_ms`.
    delta: Duration,
    /// Completing the net delta into the index's `Dlt` (reading `Old` for moved lines), appending
    /// it, and acquiring the snapshots (reconstructed mode). The index is base storage, so this
    /// is a part of `base_ms`.
    index: Duration,
    /// The view circuit's transaction: delta accumulation, joins and `V`. In reconstructed mode
    /// it starts with the scan session's collection of the touched groups. The transaction
    /// delta's preparation (`delta`) is not in it.
    view_circuit: Duration,
    discovery: Duration,
    publication_cleanup: Duration,
    foreground: Duration,
    verification: Duration,
    transactions: usize,
    rows: usize,
    latencies: Vec<Duration>,
    delta_positive: Option<i64>,
    delta_negative: Option<i64>,
    view_rows: Option<i64>,
    /// What the scan session read from the merged index in the view transactions: its moves and
    /// the run reads they cost, counted on the driver thread (reconstructed mode).
    #[cfg(feature = "merged-index")]
    session_reads: SessionReads,
    /// The run reads of base maintenance on the merged index: `prepare` reading `Old` for the
    /// lines of an order whose owner changes (reconstructed mode).
    #[cfg(feature = "merged-index")]
    upkeep_reads: RunReads,
    /// The blocks that `prepare`'s reads of `Old` expanded from their disk encoding, on the
    /// merged index's owner (reconstructed mode; measurement version 12).
    #[cfg(feature = "merged-index")]
    upkeep_expansions: BlockExpansions,
    /// Where the last transaction's published index snapshots kept their runs (reconstructed
    /// mode).
    index_layout: Option<Value>,
    /// The view worker's run reads in the view transactions, per read owner, indexed like
    /// `Component::ALL` (both modes).
    view_reads: [RunReads; Component::ALL.len()],
    /// The blocks the view worker expanded from their disk encoding on cache misses in the view
    /// transactions, per read owner (both modes; measurement version 12). A storage-level step,
    /// apart from `view_reads`' item decodes.
    view_expansions: [BlockExpansions; Component::ALL.len()],
    /// The CPU profiler's step CPU time of each circuit in the measured transactions, a
    /// cross-check of the phase timers. `None` while the pipeline is not profiling.
    profiled_cpu: Option<ProfiledCpu>,
}

/// The CPU time that the profiler attributes to each circuit's steps.
#[derive(Clone, Copy, Default)]
struct ProfiledCpu {
    base: Duration,
    view: Duration,
}

impl Measurements {
    /// Base maintenance: the base circuit's transaction plus the merged index's upkeep. The
    /// two are separate intervals of the transaction (the net-delta read and the row
    /// conversion fall between them), so this is their sum.
    fn base_phase(&self) -> Duration {
        self.base_circuit + self.index
    }

    /// Incremental view maintenance: preparing the view circuit's transaction delta, then the
    /// view circuit's transaction (with the scan session's collection in reconstructed mode).
    /// The merged index's upkeep and the net-delta read are not part of it.
    fn ivm_phase(&self) -> Duration {
        self.delta + self.view_circuit
    }
}

/// Applies `change` to the base circuit, forwards the base tables' net delta to the view
/// circuit (and, in reconstructed mode, to the merged index), and runs the view circuit.
fn transact_inner(
    pipeline: &mut Pipeline,
    mut state: Option<&mut Reference>,
    change: Change,
    check: Option<(&workload::BaseState, bool)>,
    stats: &mut Measurements,
) -> Result<()> {
    let verification_started = Instant::now();
    let before = state
        .as_ref()
        .map(|state| state.evaluate())
        .unwrap_or_default();
    let reference_change = state.as_ref().map(|_| change.clone());
    stats.verification += verification_started.elapsed();
    let foreground_started = Instant::now();
    stats.rows += change.len();

    // Check the change, then load it into the base circuit's input maps.
    let preparation_started = Instant::now();
    if let Some((base, discovered)) = check {
        if discovered {
            base.validate_discovered(&change)?;
        } else {
            base.validate(&change)?;
        }
    }
    change.apply_optional(None, pipeline.base.inputs())?;
    stats.preparation += preparation_started.elapsed();

    // The profiler's cumulative CPU time before the transactions; both circuits are idle.
    let cpu_before = if pipeline.profile_cpu {
        Some(ProfiledCpu {
            base: profiled_cpu(&mut pipeline.base.circuit)?,
            view: profiled_cpu(&mut pipeline.view.circuit)?,
        })
    } else {
        None
    };
    let base_started = Instant::now();
    pipeline.base.circuit.transaction()?;
    stats.base_circuit += base_started.elapsed();
    let base_cpu_after = cpu_before
        .map(|_| profiled_cpu(&mut pipeline.base.circuit))
        .transpose()?;

    // The base circuit's delta streams hold the net change of each base table: an upsert that
    // replaces a row appears as a deletion of the old row and an insertion of the new one.
    let forward_started = Instant::now();
    let net_delta = workload::net_delta(&pipeline.base.catalog)?;
    stats.forward += forward_started.elapsed();

    // The base circuit used the cache normally, its merges included; now its tables' blocks
    // leave the cache, so the view phase finds only view state there. Before the index upkeep,
    // which reads the index, not the base tables.
    if pipeline.evict_base {
        let eviction_started = Instant::now();
        stats.base_evicted_bytes += pipeline.evict_base_blocks()?;
        stats.base_eviction += eviction_started.elapsed();
    }

    // The index sees the net delta, completed into `Dlt`. Base maintenance does that and the
    // append (`index`, a part of `base_ms`), including its reads of `Old` for the lines of an
    // order that changes owner; the view circuit then takes its inputs from `Dlt`.
    #[cfg(feature = "merged-index")]
    {
        let index_started = Instant::now();
        let (index_rows, upkeep_reads, upkeep_expansions) = complete_net_delta(
            pipeline,
            &net_delta,
            check.map(|(base, _)| base),
            state.as_deref(),
        )?;
        let (old, dlt) = pipeline.index.append(index_rows)?;
        stats.index += index_started.elapsed();
        stats.upkeep_reads += upkeep_reads;
        stats.upkeep_expansions += upkeep_expansions;
        let publish_started = Instant::now();
        // The after reads see `Old`'s runs and `Dlt`, which is placed like an operator's output.
        stats.index_layout = Some(json!({
            "before": reconstruction::runs_layout(old.batches()),
            "after": reconstruction::runs_layout(old.batches().iter().chain([&dlt])),
        }));
        pipeline
            .q3_state
            .publish(old, dlt)
            .map_err(anyhow::Error::msg)?;
        stats.publication_cleanup += publish_started.elapsed();
    }
    // The native view circuit takes the net delta as JSON rows; converting them into its input
    // handles is view work (`delta_ms`).
    #[cfg(not(feature = "merged-index"))]
    let delta = {
        let delta_started = Instant::now();
        net_delta.apply_optional(None, pipeline.view.inputs())?;
        delta_started.elapsed()
    };
    #[cfg(feature = "merged-index")]
    let mut delta = Duration::ZERO;

    // The view worker counts its own run reads; they are read outside the transaction's timer.
    let view_reads_before = pipeline.view.circuit.run_reads()?;
    let view_expansions_before = pipeline.view.circuit.block_expansions()?;
    let view_started = Instant::now();
    // The reconstructed view phase starts with the scan session, which reads the index once for
    // the whole transaction and fills the readers (`q3_ms`). Preparing the transaction delta
    // from `Dlt` is the counterpart of native's row conversion, so it is booked in `delta_ms`
    // and the `q3_ms` window resumes with the transaction.
    #[cfg(feature = "merged-index")]
    let (transaction_result, view_time) = {
        let collection_result = pipeline.collect_groups();
        // Phase A (the driving deltas from `Dlt`) runs inside the collection; it moves from the
        // scan's `q3_ms` to `delta_ms`.
        let delta_preparation = collection_result
            .as_ref()
            .map_or(Duration::ZERO, |collection| collection.delta_preparation);
        let collection = view_started.elapsed().saturating_sub(delta_preparation);
        delta += delta_preparation;
        let delta_started = Instant::now();
        let delta_result =
            collection_result.and_then(|collection| pipeline.build_view_delta(collection.driving));
        delta += delta_started.elapsed();
        let transaction_started = Instant::now();
        let transaction_result = delta_result.and_then(|()| {
            pipeline
                .view
                .circuit
                .transaction()
                .map_err(anyhow::Error::from)
        });
        (
            transaction_result,
            collection + transaction_started.elapsed(),
        )
    };
    #[cfg(not(feature = "merged-index"))]
    let (transaction_result, view_time) = {
        let transaction_result = pipeline.view.circuit.transaction();
        (transaction_result, view_started.elapsed())
    };
    // The latency of a view transaction is its `ivm_ms`: the delta's preparation and the rest.
    let latency = delta + view_time;
    let view_reads_after = pipeline.view.circuit.run_reads()?;
    let view_expansions_after = pipeline.view.circuit.block_expansions()?;
    if let (Some(before), Some(base_after)) = (cpu_before, base_cpu_after) {
        let view_after = profiled_cpu(&mut pipeline.view.circuit)?;
        let spent = stats.profiled_cpu.get_or_insert_with(ProfiledCpu::default);
        spent.base += base_after.saturating_sub(before.base);
        spent.view += view_after.saturating_sub(before.view);
    }
    for ((total, after), before) in stats
        .view_reads
        .iter_mut()
        .zip(view_reads_after)
        .zip(view_reads_before)
    {
        *total += after.since(before);
    }
    for ((total, after), before) in stats
        .view_expansions
        .iter_mut()
        .zip(view_expansions_after)
        .zip(view_expansions_before)
    {
        *total += after.since(before);
    }
    #[cfg(feature = "merged-index")]
    let finish_result = {
        let cleanup_started = Instant::now();
        let result = pipeline.q3_state.finish().map_err(anyhow::Error::msg);
        stats.publication_cleanup += cleanup_started.elapsed();
        result
    };
    transaction_result?;
    #[cfg(feature = "merged-index")]
    {
        stats.session_reads += finish_result?;
    }
    stats.delta += delta;
    stats.view_circuit += view_time;
    stats.latencies.push(latency);
    stats.transactions += 1;
    stats.foreground += foreground_started.elapsed();
    let verification_started = Instant::now();
    if let (Some(state), Some(change)) = (state.as_deref_mut(), reference_change) {
        // Reference maintenance belongs exclusively to independent validation.
        update_reference(state, change);
        let after = state.evaluate();
        let output = pipeline
            .view
            .catalog
            .output_handles(&SqlIdentifier::from("V"))
            .context("missing V output")?;
        let actual_delta = pipeline.in_cache(|| read_output(&*output.delta_handle))?;
        let expected_delta = difference(&before, &after);
        if actual_delta != expected_delta {
            bail!("emitted delta differs from independent full join: actual {actual_delta:?}, expected {expected_delta:?}");
        }
        let snapshot = output
            .integrate_handle
            .as_ref()
            .context("V has no materialized snapshot")?;
        let actual_state = pipeline.in_cache(|| read_output(&**snapshot))?;
        if actual_state != after {
            bail!("materialized V differs from independent full join: actual {actual_state:?}, expected {after:?}");
        }
        *stats.delta_positive.get_or_insert(0) += actual_delta
            .values()
            .filter(|weight| **weight > 0)
            .sum::<i64>();
        *stats.delta_negative.get_or_insert(0) += actual_delta
            .values()
            .filter(|weight| **weight < 0)
            .map(|weight| -*weight)
            .sum::<i64>();
        stats.view_rows = Some(after.values().sum());
    } else {
        let cleanup_started = Instant::now();
        let output = pipeline
            .view
            .catalog
            .output_handles(&SqlIdentifier::from("V"))
            .context("missing V output")?;
        let _ = output.delta_handle.concat();
        let cleanup = cleanup_started.elapsed();
        stats.publication_cleanup += cleanup;
        stats.foreground += cleanup;
    }
    if state.is_some() {
        stats.verification += verification_started.elapsed();
    }
    Ok(())
}

fn update_reference(state: &mut Reference, change: Change) {
    for (row, insert) in change.customers {
        if insert {
            state.customers.insert(row.key, row);
        } else {
            state.customers.remove(&row.key);
        }
    }
    for (row, insert) in change.orders {
        if insert {
            state.orders.insert(row.key, row);
        } else {
            state.orders.remove(&row.key);
        }
    }
    for (row, insert) in change.lines {
        let key = (row.order_key, row.number);
        if insert {
            state.lines.insert(key, row);
        } else {
            state.lines.remove(&key);
        }
    }
}

#[cfg(test)]
fn transact(
    pipeline: &mut Pipeline,
    state: &mut Reference,
    change: Change,
    verify: bool,
    stats: &mut Measurements,
) -> Result<()> {
    transact_inner(pipeline, verify.then_some(state), change, None, stats)
}

/// Splits a change into the three steps of a multi-step transaction: customers and orders,
/// then each half of the lines.
#[cfg(test)]
fn steps_of(change: &Change) -> [Change; 3] {
    let middle = change.lines.len() / 2;
    [
        Change {
            customers: change.customers.clone(),
            orders: change.orders.clone(),
            ..Change::default()
        },
        Change {
            lines: change.lines[..middle].to_vec(),
            ..Change::default()
        },
        Change {
            lines: change.lines[middle..].to_vec(),
            ..Change::default()
        },
    ]
}

/// Runs `change` on the base circuit, as one transaction of three steps when `stepped`, and
/// returns the base tables' net delta.
#[cfg(test)]
fn run_base(
    pipeline: &mut Pipeline,
    state: &mut Reference,
    change: Change,
    stepped: bool,
) -> Result<Change> {
    if stepped {
        pipeline.base.circuit.start_transaction()?;
        for step in steps_of(&change) {
            step.apply_optional(Some(state), pipeline.base.inputs())?;
            assert!(!pipeline.base.circuit.step()?);
        }
        pipeline.base.circuit.commit_transaction()?;
    } else {
        change.apply_optional(Some(state), pipeline.base.inputs())?;
        pipeline.base.circuit.transaction()?;
    }
    workload::net_delta(&pipeline.base.catalog)
}

/// Completes the net delta into the index's `Dlt`, appends it, and publishes `Old` and `Dlt`,
/// as `transact_inner` does before the view circuit runs. `reference` names the customer of an
/// order that the net delta mentions only through its lines.
#[cfg(all(test, feature = "merged-index"))]
fn publish_net_delta(
    pipeline: &mut Pipeline,
    net_delta: &Change,
    reference: &Reference,
) -> Result<()> {
    let (index_rows, _, _) = complete_net_delta(pipeline, net_delta, None, Some(reference))?;
    let (old, dlt) = pipeline.index.append(index_rows)?;
    pipeline
        .q3_state
        .publish(old, dlt)
        .map_err(anyhow::Error::msg)
}

/// Runs the view circuit's transaction, as one of three steps when `stepped`. In reconstructed
/// mode the caller has published `Old` and `Dlt`: the session fills the readers, the driving
/// deltas reach the input handles before the first step, and the publication is finished
/// here. The joins' accumulators hold the deltas until the commit, where the joins read their
/// partners, so the steps share the one collection.
#[cfg(test)]
fn run_view(pipeline: &mut Pipeline, net_delta: Change, stepped: bool) -> Result<()> {
    #[cfg(feature = "merged-index")]
    {
        drop(net_delta);
        let collection = pipeline.collect_groups()?;
        pipeline.build_view_delta(collection.driving)?;
        if stepped {
            pipeline.view.circuit.start_transaction()?;
            for _ in 0..3 {
                assert!(!pipeline.view.circuit.step()?);
            }
            pipeline.view.circuit.commit_transaction()?;
        } else {
            pipeline.view.circuit.transaction()?;
        }
        pipeline.q3_state.finish().map_err(anyhow::Error::msg)?;
    }
    #[cfg(not(feature = "merged-index"))]
    if stepped {
        pipeline.view.circuit.start_transaction()?;
        for step in steps_of(&net_delta) {
            step.apply_optional(None, pipeline.view.inputs())?;
            assert!(!pipeline.view.circuit.step()?);
        }
        pipeline.view.circuit.commit_transaction()?;
    } else {
        net_delta.apply_optional(None, pipeline.view.inputs())?;
        pipeline.view.circuit.transaction()?;
    }
    Ok(())
}

/// Like `transact`, but each circuit runs the change as one transaction of three steps: the
/// base circuit takes the change, and the view circuit takes the net delta in the same steps.
/// The merged index is published once, and every step of the view transaction shares it.
#[cfg(all(test, feature = "merged-index"))]
fn transact_in_steps(pipeline: &mut Pipeline, state: &mut Reference, change: Change) -> Result<()> {
    let net_delta = run_base(pipeline, state, change, true)?;
    publish_net_delta(pipeline, &net_delta, state)?;
    run_view(pipeline, net_delta, true)
}

fn latency_summary(latencies: &[Duration]) -> String {
    if latencies.is_empty() {
        return "min_ms=0.000 median_ms=0.000 p95_ms=0.000 max_ms=0.000".to_owned();
    }
    let mut values = latencies.to_vec();
    values.sort();
    let ms = |value: Duration| value.as_secs_f64() * 1000.0;
    let percentile =
        |numerator: usize| values[(values.len() * numerator).div_ceil(100).saturating_sub(1)];
    format!(
        "min_ms={:.3} median_ms={:.3} p95_ms={:.3} max_ms={:.3}",
        ms(values[0]),
        ms(percentile(50)),
        ms(percentile(95)),
        ms(*values.last().unwrap())
    )
}

fn rows<T>(path: &Path, parse: impl Fn(&str) -> Result<T>) -> Result<Vec<T>> {
    BufReader::new(File::open(path).with_context(|| format!("opening {}", path.display()))?)
        .lines()
        .enumerate()
        .map(|(line, text)| {
            parse(&text?).with_context(|| format!("{}:{}", path.display(), line + 1))
        })
        .collect()
}

#[cfg(test)]
fn rf2_changes(state: &Reference, keys: &HashSet<i64>) -> Change {
    let mut change = Change {
        orders: state
            .orders
            .values()
            .filter(|order| keys.contains(&order.key))
            .cloned()
            .map(|order| (order, false))
            .collect(),
        lines: state
            .lines
            .values()
            .filter(|line| keys.contains(&line.order_key))
            .cloned()
            .map(|line| (line, false))
            .collect(),
        ..Change::default()
    };
    change.orders.sort_by_key(|(order, _)| order.key);
    change
        .lines
        .sort_by_key(|(line, _)| (line.order_key, line.number));
    change
}

fn rss_bytes() -> u64 {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } != 0 {
        return 0;
    }
    let usage = unsafe { usage.assume_init() };
    #[cfg(target_os = "macos")]
    {
        usage.ru_maxrss as u64
    }
    #[cfg(not(target_os = "macos"))]
    {
        usage.ru_maxrss as u64 * 1024
    }
}

/// The scan session's pass named on the command line.
fn parse_pass(name: &str) -> Result<Pass> {
    match name {
        "seek" => Ok(Pass::Seek),
        "step" => Ok(Pass::Step),
        other => bail!("--session-pass must be seek or step, not {other}"),
    }
}

fn parse_compaction_memory(value: &str) -> Result<CompactionMemory> {
    match value {
        "pool" => Ok(CompactionMemory::Pool),
        "charge" => Ok(CompactionMemory::Charge),
        other => bail!("--compaction-memory must be pool or charge, not {other}"),
    }
}

/// The name of a compaction memory in the report, as `--compaction-memory` spells it.
fn compaction_memory_name(compaction_memory: CompactionMemory) -> &'static str {
    match compaction_memory {
        CompactionMemory::Pool => "pool",
        CompactionMemory::Charge => "charge",
    }
}

fn parse_memory_model(value: &str) -> Result<MemoryModel> {
    match value {
        "rss" => Ok(MemoryModel::Rss),
        "budgets" => Ok(MemoryModel::Budgets),
        other => bail!("--memory-model must be rss or budgets, not {other}"),
    }
}

/// The name of a memory model in the report, as `--memory-model` spells it.
fn memory_model_name(memory_model: MemoryModel) -> &'static str {
    match memory_model {
        MemoryModel::Rss => "rss",
        MemoryModel::Budgets => "budgets",
    }
}

/// The name of a pass in the report, as `--session-pass` spells it.
fn pass_name(pass: Pass) -> &'static str {
    match pass {
        Pass::Seek => "seek",
        Pass::Step => "step",
    }
}

fn milliseconds(elapsed: Duration) -> f64 {
    elapsed.as_secs_f64() * 1000.0
}

#[derive(Default)]
struct Config {
    customer: Option<PathBuf>,
    orders: Option<PathBuf>,
    lineitem: Option<PathBuf>,
    rf1_orders: Option<PathBuf>,
    rf1_lines: Option<PathBuf>,
    rf2: Option<PathBuf>,
    storage_path: Option<PathBuf>,
    refresh_list: Option<PathBuf>,
    report: Option<PathBuf>,
    cache_bytes: Option<usize>,
    memory_target_bytes: Option<u64>,
    compaction_timeout_secs: f64,
    r5: bool,
    dump_runtime_circuit: Option<PathBuf>,
    diagnostics: Option<PathBuf>,
    /// Fully compact the circuit and the merged index after loading, before
    /// the measured refreshes, so both modes start from one run per spine.
    setup_full_compaction: bool,
    /// Front-code byte-string keys in storage files (the merged index's key
    /// level); see the DBSP dev tweak of the same name.
    front_code_byte_keys: bool,
    /// Drop the redundant `ArchivedVec` header of byte-string values in
    /// storage files (the merged index's value level); see the DBSP dev
    /// tweak of the same name.
    elide_value_headers: bool,
    /// Give the merged index's files a prefix filter over the customer group, in place of the
    /// whole-key filter, and let group seeks skip the runs it rules out
    /// (`--index-prefix-filter`, measurement version 17; an A/B arm, off by default). Native
    /// mode has no index and ignores it.
    index_prefix_filter: bool,
    /// The integral spines' memtable budget in MiB (`--memtable-mib`, measurement version 18);
    /// `None` leaves memtables off. The budget is a soft cap whose memtables count in RSS.
    memtable_mib: Option<usize>,
    /// How DBSP decides where a new run goes (`--memory-model rss|budgets`, measurement
    /// version 20): `rss` is the default, the pressure levels on RSS; `budgets` places runs by
    /// the cache and memtable budgets and needs `--memtable-mib`. Images are built under `rss`.
    memory_model: MemoryModel,
    /// How compaction's memory is bounded under `--memory-model budgets` (`--compaction-memory
    /// pool|charge`, measurement version 22): `None` is the default, `pool`. Setting it needs
    /// `budgets`, and images are built under `rss`.
    compaction_memory: Option<CompactionMemory>,
    /// The pool's limit on the merges open at once (`--compaction-jobs`, measurement version
    /// 22); `None` is DBSP's default of 2. Needs `budgets` and `pool`, and at least 1.
    compaction_jobs: Option<usize>,
    /// How the reconstructed build's scan session passes untouched groups
    /// (`--session-pass seek|step`); `None` means `Seek`. The native build has
    /// no session and ignores it.
    session_pass: Option<Pass>,
    /// Load the base files, then write the loaded state as an image to this new directory and
    /// stop (`--write-image`): the load program. See [`ImageManifest`].
    write_image: Option<PathBuf>,
    /// Reopen the image in this directory and run only the refreshes (`--from-image`): the
    /// refresh program. The directory is the run's own copy of an image; its `storage/` is the
    /// runtime's storage directory and changes as the refreshes run.
    from_image: Option<PathBuf>,
    /// Refresh list applied, untimed and unreported, after reopening the image and before the
    /// window (`--warm-up`). Without it the window starts with an empty buffer cache.
    warm_up: Option<PathBuf>,
    workers: usize,
    batch: usize,
    runs: usize,
    verify: bool,
}

impl Config {
    /// How compaction's memory is bounded in this run: `None` under `rss`, which bounds nothing,
    /// else the chosen memory, `pool` by default.
    fn effective_compaction_memory(&self) -> Option<CompactionMemory> {
        (self.memory_model == MemoryModel::Budgets)
            .then(|| self.compaction_memory.unwrap_or_default())
    }

    /// The limit on the merges open at once in this run: `None` unless the pool is on.
    fn effective_compaction_jobs(&self) -> Option<usize> {
        (self.effective_compaction_memory() == Some(CompactionMemory::Pool))
            .then(|| self.compaction_jobs.unwrap_or(DEFAULT_COMPACTION_JOBS))
    }

    fn parse() -> Result<Self> {
        Self::from_args(std::env::args().skip(1))
    }

    fn session_pass(&self) -> Pass {
        self.session_pass.unwrap_or(Pass::Seek)
    }

    fn from_args(arguments: impl IntoIterator<Item = String>) -> Result<Self> {
        let mut config = Self {
            workers: 1,
            batch: 10_000,
            runs: 3,
            verify: true,
            compaction_timeout_secs: 300.0,
            ..Self::default()
        };
        let mut args = arguments.into_iter();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--customer" => {
                    config.customer = Some(args.next().context("--customer needs path")?.into())
                }
                "--orders" => {
                    config.orders = Some(args.next().context("--orders needs path")?.into())
                }
                "--lineitem" => {
                    config.lineitem = Some(args.next().context("--lineitem needs path")?.into())
                }
                "--rf1-orders" => {
                    config.rf1_orders = Some(args.next().context("--rf1-orders needs path")?.into())
                }
                "--rf1-lines" => {
                    config.rf1_lines = Some(args.next().context("--rf1-lines needs path")?.into())
                }
                "--rf2" => config.rf2 = Some(args.next().context("--rf2 needs path")?.into()),
                "--storage-path" => {
                    config.storage_path =
                        Some(args.next().context("--storage-path needs path")?.into())
                }
                "--dump-runtime-circuit" => {
                    config.dump_runtime_circuit = Some(
                        args.next()
                            .context("--dump-runtime-circuit needs path")?
                            .into(),
                    )
                }
                "--diagnostics" => {
                    config.diagnostics = Some(
                        args.next()
                            .context("--diagnostics needs a new directory")?
                            .into(),
                    )
                }
                "--workers" => {
                    config.workers = args.next().context("--workers needs number")?.parse()?
                }
                "--batch" => config.batch = args.next().context("--batch needs number")?.parse()?,
                "--runs" => config.runs = args.next().context("--runs needs number")?.parse()?,
                "--no-verify" => config.verify = false,
                "--setup-full-compaction" => config.setup_full_compaction = true,
                "--front-code-byte-keys" => config.front_code_byte_keys = true,
                "--elide-value-headers" => config.elide_value_headers = true,
                "--index-prefix-filter" => config.index_prefix_filter = true,
                "--memtable-mib" => {
                    config.memtable_mib = Some(
                        args.next()
                            .context("--memtable-mib needs number")?
                            .parse()?,
                    )
                }
                "--memory-model" => {
                    config.memory_model = parse_memory_model(
                        &args.next().context("--memory-model needs rss or budgets")?,
                    )?
                }
                "--compaction-memory" => {
                    config.compaction_memory = Some(parse_compaction_memory(
                        &args
                            .next()
                            .context("--compaction-memory needs pool or charge")?,
                    )?)
                }
                "--compaction-jobs" => {
                    config.compaction_jobs = Some(
                        args.next()
                            .context("--compaction-jobs needs number")?
                            .parse()?,
                    )
                }
                "--session-pass" => {
                    config.session_pass = Some(parse_pass(
                        &args.next().context("--session-pass needs seek or step")?,
                    )?)
                }
                "--r5" => config.r5 = true,
                "--write-image" => {
                    config.write_image = Some(
                        args.next()
                            .context("--write-image needs a new directory")?
                            .into(),
                    )
                }
                "--from-image" => {
                    config.from_image = Some(
                        args.next()
                            .context("--from-image needs an image directory")?
                            .into(),
                    )
                }
                "--warm-up" => {
                    config.warm_up = Some(
                        args.next()
                            .context("--warm-up needs a refresh list")?
                            .into(),
                    )
                }
                "--refresh-list" => {
                    config.refresh_list =
                        Some(args.next().context("--refresh-list needs path")?.into())
                }
                "--report" => {
                    config.report = Some(args.next().context("--report needs path")?.into())
                }
                "--cache-bytes" => {
                    config.cache_bytes =
                        Some(args.next().context("--cache-bytes needs number")?.parse()?)
                }
                "--memory-target-bytes" => {
                    config.memory_target_bytes = Some(
                        args.next()
                            .context("--memory-target-bytes needs number")?
                            .parse()?,
                    )
                }
                "--compaction-timeout-secs" => {
                    config.compaction_timeout_secs = args
                        .next()
                        .context("--compaction-timeout-secs needs number")?
                        .parse()?
                }
                other => bail!("unknown argument {other}"),
            }
        }
        if config.workers == 0 || config.batch == 0 || config.runs == 0 {
            bail!("workers, batch, and runs must be positive");
        }
        #[cfg(feature = "merged-index")]
        if config.workers != 1 {
            bail!("merged-index reconstruction supports exactly one worker");
        }
        if config.rf1_orders.is_some() != config.rf1_lines.is_some() {
            bail!("RF1 orders and lines must be provided together");
        }
        if Duration::try_from_secs_f64(config.compaction_timeout_secs).is_err()
            || config.compaction_timeout_secs <= 0.0
        {
            bail!("compaction timeout must be finite and positive");
        }
        if config
            .cache_bytes
            .is_some_and(|n| n == 0 || n % (1024 * 1024) != 0)
        {
            bail!("cache capacity must be a positive whole number of MiB");
        }
        if config.cache_bytes.is_some_and(|n| n < 2 * 1024 * 1024) {
            bail!("cache capacity must be at least 2 MiB: each of the two circuits gets a share");
        }
        if config.memory_target_bytes == Some(0) {
            bail!("memory pressure target must be positive");
        }
        if config.refresh_list.is_some() && (config.rf1_orders.is_some() || config.rf2.is_some()) {
            bail!("refresh list cannot be combined with individual refresh arguments");
        }
        if config.memory_model == MemoryModel::Budgets && config.memtable_mib.is_none() {
            bail!("--memory-model budgets needs --memtable-mib: the memtable is its write budget");
        }
        if config.write_image.is_some() && config.memory_model != MemoryModel::Rss {
            bail!("--write-image loads under --memory-model rss: images are built under rss");
        }
        if (config.compaction_memory.is_some() || config.compaction_jobs.is_some())
            && config.memory_model != MemoryModel::Budgets
        {
            bail!(
                "--compaction-memory and --compaction-jobs need --memory-model budgets: they bound \
                 compaction under the budgets that model has, and images, built under rss, have none"
            );
        }
        if config.compaction_jobs.is_some()
            && config.compaction_memory == Some(CompactionMemory::Charge)
        {
            bail!("--compaction-jobs limits --compaction-memory pool; the charge has no job limit");
        }
        if config.compaction_jobs == Some(0) {
            bail!("--compaction-jobs must be at least 1: a pool of no permits starts no merge");
        }
        if config.write_image.is_some() && config.from_image.is_some() {
            bail!("--write-image and --from-image are the load and the refresh program; pass one");
        }
        if config.write_image.is_some()
            && (config.r5
                || config.runs != 1
                || config.storage_path.is_some()
                || config.report.is_some()
                || config.refresh_list.is_some()
                || config.rf1_orders.is_some()
                || config.rf2.is_some())
        {
            bail!("--write-image loads only: it takes the base files and one run, and writes its storage and manifest into the image directory");
        }
        if config.from_image.is_some()
            && (config.runs != 1
                || config.storage_path.is_some()
                || config.customer.is_some()
                || config.orders.is_some()
                || config.lineitem.is_some()
                || config.verify)
        {
            bail!("--from-image refreshes only: it takes no base files and no --storage-path (the image holds the state), one run, and --no-verify (verification runs in the single-process mode)");
        }
        if config.warm_up.is_some() && config.from_image.is_none() {
            bail!("--warm-up applies to a refresh program started --from-image");
        }
        if config.r5
            && (config.workers != 1
                || config.runs != 1
                || (config.storage_path.is_none() && config.from_image.is_none())
                || config.cache_bytes.is_none()
                || config.memory_target_bytes.is_none()
                || config.report.is_none())
        {
            bail!("R5 requires one worker, one run per process, file storage, explicit cache and memory target, and --report");
        }
        if config.report.is_some() && config.runs != 1 {
            bail!("--report requires one run per process");
        }
        // A refresh program's storage is its copy of the image.
        if config.diagnostics.is_some()
            && (config.runs != 1 || (config.storage_path.is_none() && config.from_image.is_none()))
        {
            bail!("--diagnostics requires file storage and one run per process");
        }
        Ok(config)
    }
}

/// The settings that shape an image's stored state. A refresh program reopens an image only
/// with the same settings, so that its writes use the image's storage format.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ImageSettings {
    mode: String,
    workers: usize,
    front_code_byte_keys: bool,
    elide_value_headers: bool,
    /// Whether the merged index's files carry prefix filters; images written before the option
    /// existed have none.
    #[serde(default)]
    index_prefix_filter: bool,
}

impl ImageSettings {
    fn of(config: &Config) -> Self {
        Self {
            mode: mode_name().to_owned(),
            workers: config.workers,
            front_code_byte_keys: config.front_code_byte_keys,
            elide_value_headers: config.elide_value_headers,
            index_prefix_filter: config.index_prefix_filter,
        }
    }
}

/// `image.json`, beside an image's `storage/` directory: what the load program loaded and the
/// checkpoint that holds it. Loading is not measured; `load` is its record, not a measurement.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ImageManifest {
    /// [`ImageManifest::FORMAT`] when written.
    format: u32,
    settings: ImageSettings,
    /// The published runtime checkpoint of both circuits (and the merged index and its orphan
    /// lines in reconstructed mode).
    checkpoint: CheckpointMetadata,
    /// The base files loaded, as given.
    inputs: BTreeMap<String, PathBuf>,
    /// The load report: rows, transactions, setup compaction, storage after loading.
    load: Value,
}

impl ImageManifest {
    /// The image format. Bump it by hand whenever driver code outside `measure.py`'s image
    /// sources digest (`IMAGE_SOURCE_PATHS`) changes what an image holds: the load loop,
    /// `Pipeline::write_checkpoint`, `Pipeline::reopen`, or `republish`. The digest covers DBSP's
    /// storage format, the circuits, the merged index, `reconstruction.rs`, and `workload.rs`; a
    /// change elsewhere in the driver (measurement, reporting) keeps the images valid.
    const FORMAT: u32 = 1;
    const FILE: &'static str = "image.json";
    const STORAGE: &'static str = "storage";

    fn read(image: &Path) -> Result<Self> {
        let path = image.join(Self::FILE);
        let manifest: Self = serde_json::from_slice(
            &std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?,
        )
        .with_context(|| format!("parsing {}", path.display()))?;
        if manifest.format != Self::FORMAT {
            bail!(
                "{}: unsupported image format {}",
                path.display(),
                manifest.format
            );
        }
        Ok(manifest)
    }

    /// Fails unless the image was loaded with the settings that `config` refreshes with.
    fn check(&self, config: &Config) -> Result<()> {
        let wanted = ImageSettings::of(config);
        if self.settings != wanted {
            bail!(
                "the image was written with {:?}, but this refresh runs with {wanted:?}",
                self.settings
            );
        }
        Ok(())
    }
}

/// `native` or `reconstructed`, the build's mode.
fn mode_name() -> &'static str {
    if cfg!(feature = "merged-index") {
        "reconstructed"
    } else {
        "native"
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Refresh {
    rf1_orders: Option<PathBuf>,
    rf1_lines: Option<PathBuf>,
    rf2: Option<PathBuf>,
    /// The Order count of each consecutive transaction that this refresh is cut into, in file
    /// order (`prepare_inputs.py --transaction-rows`; measurement version 14). Absent: the
    /// whole refresh is one transaction.
    #[serde(default)]
    transaction_orders: Option<Vec<usize>>,
}

impl Refresh {
    /// The files that this refresh reads.
    fn files(&self) -> impl Iterator<Item = &Path> {
        [&self.rf1_orders, &self.rf1_lines, &self.rf2]
            .into_iter()
            .flatten()
            .map(PathBuf::as_path)
    }
}

/// A refresh's input files, parsed into memory before the measurement window opens, so that
/// the window times neither file reads nor parsing.
enum PreparedRefresh {
    /// RF1: the inserted rows, as the `Change` that the loop applies.
    Rf1(Change),
    /// RF2: the order keys to delete. The rows to delete are still discovered inside the
    /// window, by looking these keys up.
    Rf2(HashSet<i64>),
}

impl PreparedRefresh {
    /// The transactions of `refresh`, in order: one, or one per entry of its
    /// `transaction_orders`. Each file is read once.
    fn read(refresh: &Refresh) -> Result<Vec<Self>> {
        if let Some(path) = &refresh.rf2 {
            let keys = rows(path, data::delete_order_key)?;
            let counts = transaction_counts(refresh, keys.len())?;
            let mut keys = keys.into_iter();
            return Ok(counts
                .into_iter()
                .map(|count| Self::Rf2(keys.by_ref().take(count).collect()))
                .collect());
        }
        let orders_path = refresh
            .rf1_orders
            .as_deref()
            .context("RF1 Orders required")?;
        let lines_path = refresh
            .rf1_lines
            .as_deref()
            .context("RF1 Lineitem required")?;
        let orders = rows(orders_path, Order::parse)?;
        let lines = rows(lines_path, Line::parse)?;
        if refresh.transaction_orders.is_none() {
            return Ok(vec![Self::Rf1(Change {
                orders: orders.into_iter().map(|row| (row, true)).collect(),
                lines: lines.into_iter().map(|row| (row, true)).collect(),
                ..Change::default()
            })]);
        }
        // Each Lineitem goes to the transaction of its Order, which must be in this refresh.
        let counts = transaction_counts(refresh, orders.len())?;
        let mut transaction_of = HashMap::with_capacity(orders.len());
        let mut changes = Vec::with_capacity(counts.len());
        let mut orders = orders.into_iter();
        for (index, count) in counts.into_iter().enumerate() {
            let mut change = Change::default();
            for order in orders.by_ref().take(count) {
                transaction_of.insert(order.key, index);
                change.orders.push((order, true));
            }
            changes.push(change);
        }
        for line in lines {
            let index = *transaction_of.get(&line.order_key).with_context(|| {
                format!(
                    "{}: Lineitem of Order {} has no Order in {}, so it belongs to no transaction",
                    lines_path.display(),
                    line.order_key,
                    orders_path.display()
                )
            })?;
            changes[index].lines.push((line, true));
        }
        Ok(changes.into_iter().map(Self::Rf1).collect())
    }

    /// The row counts of a refresh's input, for the report.
    fn summary(transactions: &[Self], number: usize) -> Value {
        let (mut orders, mut lines, mut keys) = (0, 0, 0);
        for transaction in transactions {
            match transaction {
                Self::Rf1(change) => {
                    orders += change.orders.len();
                    lines += change.lines.len();
                }
                Self::Rf2(order_keys) => keys += order_keys.len(),
            }
        }
        match transactions.first() {
            Some(Self::Rf2(_)) => json!({"refresh": number, "phase": "rf2", "order_keys": keys,
                "transactions": transactions.len()}),
            _ => json!({"refresh": number, "phase": "rf1", "orders": orders, "lines": lines,
                "transactions": transactions.len()}),
        }
    }
}

/// The Order count of each transaction of `refresh`, which holds `orders` Orders: its
/// `transaction_orders`, which must be positive and cover them exactly, or one transaction.
fn transaction_counts(refresh: &Refresh, orders: usize) -> Result<Vec<usize>> {
    let Some(counts) = &refresh.transaction_orders else {
        return Ok(vec![orders]);
    };
    if counts.is_empty() || counts.contains(&0) {
        bail!("transaction_orders must be a non-empty list of positive counts");
    }
    let covered: usize = counts.iter().sum();
    if covered != orders {
        bail!("transaction_orders cover {covered} Orders, but the refresh holds {orders}");
    }
    Ok(counts.clone())
}

/// Each transaction's latency in a refresh (measurement version 14): the foreground wall time
/// (`total`, the per-transaction counterpart of a refresh's `foreground_ms`), and its base and
/// IVM phases (`base_ms` and `ivm_ms` of one transaction).
struct TransactionLatencies {
    total: Vec<Duration>,
    base: Vec<Duration>,
    ivm: Vec<Duration>,
}

impl TransactionLatencies {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            total: Vec::with_capacity(capacity),
            base: Vec::with_capacity(capacity),
            ivm: Vec::with_capacity(capacity),
        }
    }

    fn push(&mut self, total: Duration, base: Duration, ivm: Duration) {
        self.total.push(total);
        self.base.push(base);
        self.ivm.push(ivm);
    }

    /// Percentiles (nearest rank) and the per-transaction values, in milliseconds.
    fn json(&self) -> Value {
        let measure = |values: &[Duration]| {
            let milliseconds: Vec<f64> = values.iter().copied().map(milliseconds).collect();
            let mut sorted = milliseconds.clone();
            sorted.sort_by(f64::total_cmp);
            let rank = |percent: usize| {
                sorted
                    .get((sorted.len() * percent).div_ceil(100).saturating_sub(1))
                    .copied()
            };
            json!({
                "p50": rank(50), "p95": rank(95), "p99": rank(99), "max": sorted.last(),
                "mean": (!sorted.is_empty()).then(|| sorted.iter().sum::<f64>() / sorted.len() as f64),
                "values": milliseconds,
            })
        };
        json!({
            "transactions": self.total.len(),
            "total_ms": measure(&self.total),
            "base_ms": measure(&self.base),
            "ivm_ms": measure(&self.ivm),
            "note": "One entry per transaction of this refresh, in order. total_ms is the transaction's foreground wall time (discovery, the base and view transactions, cleanup; validation excluded), so the refresh's foreground_ms is their sum plus the refresh's own bookkeeping. base_ms and ivm_ms are the transaction's base_ms and ivm_ms. Percentiles are nearest-rank.",
        })
    }
}

/// The current memory-pressure level of the runtime. The level comes from the process's RSS
/// against the memory target, not from cgroup memory.
/// The runtime's memory state at a refresh boundary: its pressure level, and the parts of RSS
/// that a budget holds, so the rest of RSS is measured rather than inferred.
fn memory_state(pipeline: &Pipeline) -> Result<Value> {
    let runtime = pipeline.runtime();
    let (cache_occupancy, _) = runtime.cache_occupancy();
    Ok(json!({
        "runtime": serde_json::to_value(runtime.current_memory_pressure())?,
        "rss_bytes": dbsp::utils::process_rss_bytes(),
        "cache_occupancy_bytes": cache_occupancy,
        "memtable_bytes": runtime.memtable_live_bytes(),
    }))
}

/// What the compaction-memory reservations cost during one refresh (measurement version 21):
/// how many waited and for how long, and the most bytes pinned at once. All zero unless
/// `--compaction-memory charge` is on. `before` and `after` are the cumulative counters at the
/// refresh's two ends; `after.peak_pinned_bytes` is the peak since the refresh began.
fn reservations_json(before: ReservationStats, after: ReservationStats) -> Value {
    json!({
        "waits": after.waits - before.waits,
        "wait_ms": milliseconds(after.wait - before.wait),
        "peak_pinned_bytes": after.peak_pinned_bytes,
    })
}

/// What the compaction pool cost during one refresh (measurement version 22): how many merges
/// waited for a permit and for how long, and the most merges open at once. The waits are zero
/// unless the pool is on (`budgets`, `--compaction-memory pool`); the peak is counted under every
/// memory model. `before` and `after` are the cumulative counters at the refresh's two ends;
/// `after.peak_open_merges` is the peak since the refresh began.
fn compaction_jobs_json(before: CompactionJobStats, after: CompactionJobStats) -> Value {
    json!({
        "waits": after.waits - before.waits,
        "wait_ms": milliseconds(after.wait - before.wait),
        "peak_open_merges": after.peak_open_merges,
    })
}

/// The refreshes of the refresh list at `path`, with relative file names resolved against the
/// list's directory.
fn refresh_list_at(path: &Path) -> Result<Vec<Refresh>> {
    let mut list: Vec<Refresh> = serde_json::from_reader(
        File::open(path).with_context(|| format!("opening {}", path.display()))?,
    )?;
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    for refresh in &mut list {
        for source in [
            &mut refresh.rf1_orders,
            &mut refresh.rf1_lines,
            &mut refresh.rf2,
        ]
        .into_iter()
        .flatten()
        {
            if source.is_relative() {
                *source = directory.join(&*source);
            }
        }
    }
    Ok(list)
}

/// The change that a prepared refresh applies, the Orders an RF2 names that are absent, the
/// refresh's phase, and whether its rows were discovered from the base state (RF2). An RF2's
/// discovery reads the base state and, in reconstructed mode, the merged index.
fn refresh_change(
    pipeline: &Pipeline,
    base: &workload::BaseState,
    prepared: PreparedRefresh,
) -> Result<(Change, usize, &'static str, bool)> {
    Ok(match prepared {
        PreparedRefresh::Rf2(keys) => {
            #[cfg(feature = "merged-index")]
            let snapshot = pipeline.index.snapshot()?;
            #[cfg(not(feature = "merged-index"))]
            let _ = pipeline;
            let (change, absent) = workload::rf2_changes(base, &keys, |order| {
                #[cfg(feature = "merged-index")]
                {
                    // The index's files are read through the runtime's buffer cache, as the
                    // view circuit's readers read them.
                    pipeline.in_cache(|| reconstruction::lines(&snapshot, order))
                }
                #[cfg(not(feature = "merged-index"))]
                {
                    base.native_lines(order.key)
                }
            })?;
            (change, absent, "rf2", true)
        }
        PreparedRefresh::Rf1(change) => (change, 0, "rf1", false),
    })
}

fn refreshes(config: &Config) -> Result<Vec<Refresh>> {
    let mut refreshes = if let Some(path) = &config.refresh_list {
        refresh_list_at(path)?
    } else {
        let mut list = Vec::new();
        if config.rf1_orders.is_some() {
            list.push(Refresh {
                rf1_orders: config.rf1_orders.clone(),
                rf1_lines: config.rf1_lines.clone(),
                rf2: None,
                transaction_orders: None,
            });
        }
        if config.rf2.is_some() {
            list.push(Refresh {
                rf1_orders: None,
                rf1_lines: None,
                rf2: config.rf2.clone(),
                transaction_orders: None,
            });
        }
        list
    };
    for refresh in &mut refreshes {
        if !((refresh.rf1_orders.is_some() && refresh.rf1_lines.is_some() && refresh.rf2.is_none())
            || (refresh.rf1_orders.is_none()
                && refresh.rf1_lines.is_none()
                && refresh.rf2.is_some()))
        {
            bail!("each refresh requires RF1 Orders/Lineitem paths or one RF2 path");
        }
    }
    Ok(refreshes)
}

/// Classify only operator names whose state behavior is established by the
/// corresponding DBSP implementation. The label's location and profile lines
/// are annotations and are not part of the operator name.
fn runtime_operator_style(name: &str) -> CircuitNodeStyle {
    match name {
        // crates/dbsp/src/operator/dynamic/accumulate_trace.rs.
        "AccumulateTraceAppend" | "AccumulateUntimedTraceAppend" => CircuitNodeStyle::OtherState,
        // The delayed accumulated trace remains available across clock cycles.
        "AccumulateDelayTrace" => CircuitNodeStyle::Accumulation,
        // crates/dbsp/src/operator/dynamic/trace.rs.
        "TraceAppend" | "UntimedTraceAppend" => CircuitNodeStyle::OtherState,
        // crates/dbsp/src/operator/dynamic/accumulator.rs.
        "Accumulator" => CircuitNodeStyle::OtherState,
        // crates/dbsp/src/operator/accumulator.rs.
        "AccumulateApply2" => CircuitNodeStyle::OtherState,
        // crates/dbsp/src/operator/dynamic/concat.rs.
        "AccumulateConcat" => CircuitNodeStyle::OtherState,
        // crates/dbsp/src/operator/non_incremental.rs.
        "ImportAccumulator" => CircuitNodeStyle::OtherState,
        // crates/dbsp/src/operator/dynamic/join.rs.
        "JoinTrace" => CircuitNodeStyle::OtherState,
        // crates/dbsp/src/operator/dynamic/{trace,accumulate_trace}.rs.
        "Z1 (trace)" => CircuitNodeStyle::OtherState,
        // crates/dbsp/src/operator/z1.rs.
        "Z^-1" | "Z^-1 (nested)" => CircuitNodeStyle::OtherState,
        // crates/dbsp/src/operator/transaction_z1.rs.
        "Transaction Z^-1" => CircuitNodeStyle::OtherState,
        _ => CircuitNodeStyle::Unclassified,
    }
}

fn runtime_cluster_style(name: &str) -> Option<CircuitNodeStyle> {
    match name {
        // crates/dbsp/src/operator/dynamic/accumulate_trace.rs: both paths
        // integrate updates into a retained trace.
        "accumulate_integrate_trace" | "shard_accumulate_integrate_trace" => {
            Some(CircuitNodeStyle::Accumulation)
        }
        // Timed retained-trace regions remain other state under the explicit
        // integrate-region highlight convention.
        "accumulate_trace" | "shard_accumulate_trace" => Some(CircuitNodeStyle::OtherState),
        _ => None,
    }
}

fn runtime_node_styles(graph_json: &str) -> Result<BTreeMap<String, CircuitNodeStyle>> {
    fn visit_cluster(
        cluster: &Value,
        styles: &mut BTreeMap<String, CircuitNodeStyle>,
    ) -> Result<()> {
        let id = cluster
            .get("id")
            .and_then(Value::as_str)
            .context("cluster has no id")?;
        let label = cluster
            .get("label")
            .and_then(Value::as_str)
            .context("cluster has no label")?;
        let name = label.split("\\l").next().unwrap_or(label);
        let name = name.split(" @ ").next().unwrap_or(name);
        if let Some(style) = runtime_cluster_style(name) {
            styles.insert(id.to_owned(), style);
        }
        let nodes = cluster
            .get("nodes")
            .and_then(Value::as_array)
            .context("runtime graph cluster has no nodes array")?;
        for node in nodes {
            if let Some(simple) = node.get("Simple") {
                let id = simple
                    .get("id")
                    .and_then(Value::as_str)
                    .context("node has no id")?;
                let label = simple
                    .get("label")
                    .and_then(Value::as_str)
                    .context("node has no label")?;
                let name = label.split("\\l").next().unwrap_or(label);
                let name = name.split(" @ ").next().unwrap_or(name);
                styles.insert(id.to_owned(), runtime_operator_style(name));
            } else if let Some(child) = node.get("Cluster") {
                visit_cluster(child, styles)?;
            } else {
                bail!("unexpected runtime graph node: {node}");
            }
        }
        Ok(())
    }

    let graph: Value = serde_json::from_str(graph_json).context("invalid runtime graph JSON")?;
    let root = graph
        .get("nodes")
        .context("runtime graph has no root cluster")?;
    let mut styles = BTreeMap::new();
    visit_cluster(root, &mut styles)?;
    Ok(styles)
}

fn render_svg(dot_path: &Path) -> Result<()> {
    let svg_path = dot_path.with_extension("svg");
    let status = Command::new("dot")
        .arg("-Tsvg")
        .arg(dot_path)
        .arg("-o")
        .arg(&svg_path)
        .status()
        .with_context(|| format!("running Graphviz for {}", dot_path.display()))?;
    if !status.success() {
        bail!("Graphviz failed for {} with {status}", dot_path.display());
    }
    Ok(())
}

fn dump_runtime_circuit(circuit: &mut dbsp::DBSPHandle, directory: &Path) -> Result<()> {
    std::fs::create_dir(directory)
        .with_context(|| format!("creating new capture directory {}", directory.display()))?;
    let profile = circuit
        .graph_profile()
        .context("capturing runtime graph profile")?;
    for (graph, worker) in profile.worker_graphs.iter().zip(profile.worker_offset..) {
        let raw_dot = directory.join(format!("worker-{worker}.dot"));
        let graph_json = graph.to_json();
        let styles = runtime_node_styles(&graph_json)?;
        std::fs::write(&raw_dot, graph.to_dot())?;
        std::fs::write(directory.join(format!("worker-{worker}.json")), graph_json)?;
        let styled_dot = directory.join(format!("worker-{worker}-styled.dot"));
        std::fs::write(&styled_dot, graph.to_dot_with_styles(&styles))?;
    }
    for worker in profile.worker_offset..profile.worker_offset + profile.worker_graphs.len() {
        let raw_dot = directory.join(format!("worker-{worker}.dot"));
        let styled_dot = directory.join(format!("worker-{worker}-styled.dot"));
        render_svg(&raw_dot)?;
        render_svg(&styled_dot)?;
    }
    Ok(())
}

fn wait_for_loading_merges(pipeline: &mut Pipeline, timeout: Duration) -> Result<Duration> {
    let started = Instant::now();
    measurement::drain_until(timeout.saturating_sub(started.elapsed()), || {
        let complete = pipeline.base.circuit.is_background_merge_idle()?
            && pipeline.view.circuit.is_background_merge_idle()?;
        #[cfg(feature = "merged-index")]
        let complete = complete && pipeline.index.is_background_merge_idle()?;
        Ok(complete)
    })?;
    Ok(started.elapsed())
}

/// Requests full compaction of both circuits and the merged index, waits for compaction to
/// finish, and reports what that setup cost.
fn setup_full_compaction(pipeline: &mut Pipeline, timeout: Duration) -> Result<Value> {
    let before = measurement::ResourceSnapshot::capture()?;
    let started = Instant::now();
    pipeline.base.circuit.start_compaction()?;
    pipeline.view.circuit.start_compaction()?;
    #[cfg(feature = "merged-index")]
    pipeline.index.start_compaction()?;
    let wait = measurement::drain_until(timeout, || {
        let complete = pipeline.base.circuit.is_compaction_complete()?
            && pipeline.view.circuit.is_compaction_complete()?;
        #[cfg(feature = "merged-index")]
        let complete = complete && pipeline.index.is_compaction_complete()?;
        Ok(complete)
    })?;
    let elapsed = started.elapsed();
    let after = measurement::ResourceSnapshot::capture()?;
    Ok(json!({
        "wait_for_compaction_ms": milliseconds(wait),
        "elapsed_ms": milliseconds(elapsed),
        "resources": before.delta(&after),
    }))
}

/// Publishes the current batches of both circuits to their catalogs without changing the
/// logical input. Loading waits for normally scheduled merges. It requests full compaction
/// only when `full_compaction` is set, and then before publication, so the published snapshots
/// hold the compacted runs. Returns the time spent outside setup compaction and the setup
/// compaction's cost, if any.
fn finish_loading(
    pipeline: &mut Pipeline,
    base: &mut workload::BaseState,
    timeout: Duration,
    full_compaction: bool,
) -> Result<(Duration, Option<Value>)> {
    let started = Instant::now();
    wait_for_loading_merges(pipeline, timeout)?;
    let setup_compaction_started = Instant::now();
    let setup_compaction = full_compaction
        .then(|| setup_full_compaction(pipeline, timeout.saturating_sub(started.elapsed())))
        .transpose()?;
    let setup_compaction_elapsed = setup_compaction_started.elapsed();
    republish(pipeline, base)?;
    wait_for_loading_merges(pipeline, timeout.saturating_sub(started.elapsed()))?;
    // Setup compaction is reported on its own, not as background merge wait.
    Ok((
        started.elapsed().saturating_sub(setup_compaction_elapsed),
        setup_compaction,
    ))
}

/// Publishes the current batches of both circuits to their catalogs without changing the
/// logical input, with one empty transaction of each circuit (and, in reconstructed mode, an
/// empty publication of the merged index), then refreshes `base` and drains the delta outputs.
/// A loaded pipeline and a pipeline reopened from an image both need it before their first
/// refresh: after a restore, the output snapshots are empty until a transaction runs.
fn republish(pipeline: &mut Pipeline, base: &mut workload::BaseState) -> Result<()> {
    for (stage, names) in [
        (
            &pipeline.base,
            ["CUSTOMER", "ORDERS", "LINEITEM"].as_slice(),
        ),
        (&pipeline.view, ["V"].as_slice()),
    ] {
        for name in names {
            let output = stage
                .catalog
                .output_handles(&SqlIdentifier::from(*name))
                .context("missing output")?;
            drop(
                output
                    .integrate_handle
                    .as_ref()
                    .context("missing integrated output")?
                    .take_from_all(),
            );
        }
    }
    #[cfg(feature = "merged-index")]
    {
        let snapshot = pipeline.index.snapshot()?;
        pipeline
            .q3_state
            .publish(snapshot, reconstruction::empty_run()?)
            .map_err(anyhow::Error::msg)?;
        // The joins' readers run even on empty deltas, and need the (empty) collection.
        pipeline.collect_groups()?;
    }
    let base_transaction = pipeline.base.circuit.transaction();
    let view_transaction = pipeline.view.circuit.transaction();
    #[cfg(feature = "merged-index")]
    let finish = pipeline.q3_state.finish().map_err(anyhow::Error::msg);
    base_transaction?;
    view_transaction?;
    #[cfg(feature = "merged-index")]
    finish?;
    *base = pipeline.take_base_state()?;
    for (stage, names) in [
        (
            &pipeline.base,
            ["CUSTOMER", "ORDERS", "LINEITEM", "ERROR_VIEW"].as_slice(),
        ),
        (&pipeline.view, ["V", "ERROR_VIEW"].as_slice()),
    ] {
        for name in names {
            // The reconstructed build's view circuit has no `ERROR_VIEW`.
            let Some(output) = stage.catalog.output_handles(&SqlIdentifier::from(*name)) else {
                if *name == "ERROR_VIEW" {
                    continue;
                }
                bail!("missing output {name}");
            };
            drop(output.delta_handle.concat());
        }
    }
    Ok(())
}

/// The phases of a refresh (since measurement version 4).
///
/// `base_ms` is base maintenance: the base circuit's transaction plus the merged index's
/// upkeep. `index_ms` is a sub-phase of `base_ms` and is never added to it again. `ivm_ms` is
/// incremental view maintenance: `delta_ms + q3_ms`. `delta_ms` prepares the view circuit's
/// transaction delta (native's conversion of the net delta into input rows; reconstructed's
/// building and hand-off of the driving deltas from `Dlt`), and `q3_ms` is the view circuit's
/// transaction (in reconstructed mode, with the scan session's collection before it).
/// `forward_ms` holds only the net-delta read (measurement version 11). The base circuit,
/// `forward_ms`, the index append, `delta_ms` and `q3_ms` are disjoint intervals of the
/// transaction; the base and IVM phases are always reported side by side.
fn phase_json(stats: &Measurements) -> Value {
    json!({
        "rows": stats.rows,
        "transactions": stats.transactions,
        "discovery_ms": milliseconds(stats.discovery),
        "checks_input_preparation_ms": milliseconds(stats.preparation),
        "base_ms": milliseconds(stats.base_phase()),
        "forward_ms": milliseconds(stats.forward),
        "base_eviction_ms": milliseconds(stats.base_eviction),
        "base_evicted_bytes": stats.base_evicted_bytes,
        "index_ms": milliseconds(stats.index),
        "delta_ms": milliseconds(stats.delta),
        "q3_ms": milliseconds(stats.view_circuit),
        "publication_cleanup_ms": milliseconds(stats.publication_cleanup),
        "foreground_ms": milliseconds(stats.foreground),
        "ivm_ms": milliseconds(stats.ivm_phase()),
        "phase_note": "base_ms = the base circuit's transaction + index_ms (the merged index's upkeep, including the reads of Old for the lines of an order that changes owner; zero in native mode); index_ms is a sub-phase of base_ms and is not added again. ivm_ms = delta_ms + q3_ms. delta_ms prepares the view circuit's transaction delta: in native mode, the conversion of the net delta into the input handles' rows; in reconstructed mode, the hand-off of the readers' Dlt batches (the driving deltas, moved for orders and line items and cloned for customers) to the input handles. The two operations are not identical, so compare the modes with and without delta_ms. q3_ms = the view circuit's transaction; in reconstructed mode it also includes the scan session's collection of the touched groups into the typed readers, which builds the readers' driving deltas and so is not yet in delta_ms. forward_ms is only the net delta read (workload::net_delta), the same operation in both modes, and is in neither ivm_ms nor q3_ms. Report base_ms and ivm_ms side by side. Discovery, preparation and publication/cleanup are in none of them.",
        "verification_ms_excluded": milliseconds(stats.verification),
        "profiled_cpu_ms": stats.profiled_cpu.map(|cpu| json!({
            "base": milliseconds(cpu.base),
            "q3": milliseconds(cpu.view),
            "note": "The CPU profiler's step CPU time of each circuit in this refresh's transactions: a cross-check of base_ms and q3_ms, not a phase. base excludes the merged index's upkeep, which runs on its own thread.",
        })),
    })
}

/// The view worker's run reads per read owner (measurement version 11): in native mode the
/// operators' own reads, the join traces' partner reads among them, are under `query`. In
/// reconstructed mode `query` holds the operators' own reads (the deltas'), and the joins' reads
/// of the readers' batches are under `requested_slice`. Nothing the worker runs reads under
/// `merged_index`: the scan session reads the index on the driver's thread. The fields are those
/// of `RunReads`: file runs' decodes and in-memory runs' reads count apart.
fn view_reads_json(reads: &[RunReads; Component::ALL.len()]) -> Result<Value> {
    per_owner_json(reads)
}

/// An object with one entry per read owner, keyed by the owner's name.
fn per_owner_json<T: serde::Serialize>(values: &[T; Component::ALL.len()]) -> Result<Value> {
    Component::ALL
        .into_iter()
        .map(|owner| {
            Ok((
                owner.name().to_owned(),
                serde_json::to_value(&values[owner as usize])?,
            ))
        })
        .collect::<Result<serde_json::Map<_, _>>>()
        .map(Value::Object)
}

/// The blocks expanded from their disk encoding on cache misses (measurement version 12), per
/// thread that read and, for the view worker and the driver, per read owner:
///
/// - `view_worker`: the view circuit's worker, per owner, in both modes. In native mode its
///   `query` entry holds the join traces' expansions along with the operators' own.
/// - `driver`: the thread that collects the readers (reconstructed mode), per owner; `null` in
///   native mode. Its `merged_index` entry is the scan session's expansions of `Old` and `Dlt`.
/// - `index_upkeep`: base maintenance's `prepare` reads of `Old` (reconstructed mode); `null` in
///   native mode.
///
/// A block expansion rebuilds a prefix-truncated or an elided data block; the fields are
/// those of `BlockExpansions`. Other threads, such as the index's own, are not counted.
fn block_expansions_json(
    view_worker: &[BlockExpansions; Component::ALL.len()],
    driver: Option<&[BlockExpansions; Component::ALL.len()]>,
    index_upkeep: Option<&BlockExpansions>,
) -> Result<Value> {
    Ok(json!({
        "view_worker": per_owner_json(view_worker)?,
        "driver": driver.map(per_owner_json).transpose()?,
        "index_upkeep": index_upkeep.map(serde_json::to_value).transpose()?,
    }))
}

/// The merged index's run reads in the view phase (measurement version 8): the scan session's,
/// which are the only reads of the index there, under the key `session`. File runs count key,
/// value and weight decodes and in-memory runs count `memory_*` reads, apart; `index_layout`
/// says where the runs were. The session's moves are in [`session_counts_json`].
#[cfg(feature = "merged-index")]
fn index_reads_json(reads: &SessionReads) -> Result<Value> {
    Ok(json!({"session": serde_json::to_value(reads.run_reads)?}))
}

/// Every run read the driver's thread made while collecting the readers (measurement version
/// 11), per read owner: the index's, which are also in `index_run_reads`, and the routing's reads
/// of the driving deltas under `requested_slice`. No other owner reads there. The summarizer adds
/// the `requested_slice` part to the view worker's.
#[cfg(feature = "merged-index")]
fn driver_reads_json(reads: &SessionReads) -> Result<Value> {
    view_reads_json(&reads.driver_reads)
}

/// What the scan session did, summed over the view transactions: groups handed out, logical
/// seeks and steps over `Old`, positions reached, and payloads decoded from `Old` and `Dlt`.
#[cfg(feature = "merged-index")]
fn session_counts_json(reads: &SessionReads) -> Result<Value> {
    Ok(serde_json::to_value(reads.counts)?)
}

/// The merged index's run reads in base maintenance: `prepare`'s reads of `Old` for the lines of
/// an order whose owner changes. They belong to the base phase, so they stay apart from
/// `index_run_reads`, which is the view phase's.
#[cfg(feature = "merged-index")]
fn upkeep_reads_json(reads: &RunReads) -> Result<Value> {
    Ok(serde_json::to_value(reads)?)
}

/// Inspects the actual batches behind the catalogs' snapshots, without scanning rows: the base
/// relations from the base circuit and `V` from the view circuit.
fn storage_evidence(base: &workload::BaseState, pipeline: &Pipeline) -> Result<Value> {
    let mut evidence = base.storage_evidence();
    let reader = pipeline
        .view
        .catalog
        .output_handles(&SqlIdentifier::from("V"))
        .context("missing V materialization")?
        .integrate_handle
        .as_ref()
        .context("missing integrated V output")?
        .concat();
    evidence.insert("V".into(), workload::batch_storage_evidence(&*reader));
    Ok(Value::Object(evidence))
}

/// The whole buffer cache in MiB, rounded down. Both circuits share it.
fn cache_mib(cache_bytes: Option<usize>) -> Option<usize> {
    cache_bytes.map(|bytes| bytes / (1024 * 1024))
}

/// The storage options of the runtime. The harness leaves DBSP's run placement at its defaults
/// (user decision of 2026-11-09): memory pressure, from the cgroup and RSS targets, decides what
/// reaches disk, as in a pipeline. `min_storage_bytes` and `min_step_storage_bytes` stay unset.
fn storage_options(config: &Config) -> StorageOptions {
    StorageOptions {
        cache_mib: cache_mib(config.cache_bytes),
        compression: feldera_types::config::StorageCompression::None,
        memtable_mib: config.memtable_mib,
        memory_model: config.memory_model,
        compaction_memory: config.compaction_memory.unwrap_or_default(),
        compaction_jobs: config.compaction_jobs,
        ..StorageOptions::default()
    }
}

/// The `min_storage_bytes` that the runtime applies: the option if set, else DBSP's default.
/// At low memory pressure a merge output below it stays in memory and inserted batches stay in
/// memory; moderate pressure spills every merge output, and high pressure every inserted batch
/// (`Runtime::min_merge_storage_bytes` and `Runtime::min_insert_storage_bytes`).
fn resolved_min_storage_bytes(options: &StorageOptions) -> usize {
    options
        .min_storage_bytes
        .unwrap_or(Runtime::DEFAULT_MIN_STORAGE_BYTES)
}

/// The configuration of the runtime that hosts both circuits: one storage directory, one buffer
/// cache, and the memory-pressure target.
fn runtime_config(config: &Config, storage_path: Option<&Path>) -> Result<CircuitConfig> {
    let mut circuit_config =
        CircuitConfig::with_workers(config.workers).with_max_rss_bytes(config.memory_target_bytes);
    // Native joins probe their traces through run cursors, as the reconstructed readers do, so
    // both modes' run reads are counted alike (user decision of 2026-11-04).
    circuit_config.dev_tweaks.fetch_join = Some(false);
    circuit_config.dev_tweaks.front_code_byte_keys = Some(config.front_code_byte_keys);
    circuit_config.dev_tweaks.elide_value_headers = Some(config.elide_value_headers);
    if let Some(path) = storage_path {
        std::fs::create_dir_all(path)?;
        circuit_config = circuit_config.with_storage(Some(CircuitStorageConfig::for_config(
            StorageConfig {
                path: path.to_string_lossy().into_owned(),
                cache: StorageCacheConfig::FelderaCache,
            },
            storage_options(config),
        )?));
    }
    Ok(circuit_config)
}

fn run(config: &Config, run_number: usize) -> Result<()> {
    let started = Instant::now();
    let timeout = Duration::from_secs_f64(config.compaction_timeout_secs);
    // The load program writes its storage into a new image; the refresh program runs in its
    // copy of one; otherwise each run gets a new directory under --storage-path.
    let image = config
        .from_image
        .as_deref()
        .map(ImageManifest::read)
        .transpose()?;
    if let Some(image) = &image {
        image.check(config)?;
    }
    let storage_root = if let Some(directory) = &config.from_image {
        let path = directory.join(ImageManifest::STORAGE);
        if !path.is_dir() {
            bail!("image has no storage directory: {}", path.display());
        }
        Some(path)
    } else if let Some(directory) = &config.write_image {
        if directory.exists() {
            bail!("image directory already exists: {}", directory.display());
        }
        Some(directory.join(ImageManifest::STORAGE))
    } else {
        config
            .storage_path
            .as_ref()
            .map(|root| root.join(format!("run-{run_number}")))
    };
    if let (None, Some(path)) = (&config.from_image, &storage_root) {
        if path.exists() {
            bail!("storage directory already exists: {}", path.display());
        }
        std::fs::create_dir_all(path)?;
    }
    // Both circuits share one runtime, and so its storage directory and its buffer cache.
    let pipeline_config = runtime_config(config, storage_root.as_deref())?;
    // Diagnostics must be on before any spine exists, so that all are registered.
    let diagnostics = config
        .diagnostics
        .as_deref()
        .map(|directory| {
            diagnostics::Diagnostics::start(
                directory,
                if cfg!(feature = "merged-index") {
                    "reconstructed"
                } else {
                    "native"
                },
            )
        })
        .transpose()?;
    let mut pipeline = match &image {
        Some(image) => Pipeline::reopen(
            pipeline_config,
            &image.checkpoint,
            config.index_prefix_filter,
        )?,
        None => Pipeline::new(pipeline_config, config.index_prefix_filter)?,
    };
    if config.verify {
        pipeline.enable_contract_check();
    }
    #[cfg(feature = "merged-index")]
    {
        pipeline.session_pass = config.session_pass();
    }
    let runtime = pipeline.runtime().clone();
    // The driver thread binds the runtime's buffer cache, as the workers do.
    runtime.with_buffer_cache(0, || {
    let mut state = config.verify.then(Reference::default);
    let mut base = pipeline.take_base_state()?;
    // The refresh program reopens a loaded image instead of loading; it only republishes.
    let load_report = if image.is_some() {
        republish(&mut pipeline, &mut base)?;
        Value::Null
    } else {
    let sampler = storage_root
        .clone()
        .map(measurement::StorageSampler::start)
        .transpose()?;
    let mut stats = Measurements::default();
    let load = workload::load_batches(
        config.customer.as_deref().context("--customer required")?,
        config.orders.as_deref().context("--orders required")?,
        config.lineitem.as_deref().context("--lineitem required")?,
        config.batch,
        |batch| {
            transact_inner(
                &mut pipeline,
                state.as_mut(),
                batch,
                Some((&base, false)),
                &mut stats,
            )?;
            base = pipeline.take_base_state()?;
            Ok(())
        },
    )?;
    let (load_drain, setup_compaction) = finish_loading(
        &mut pipeline,
        &mut base,
        timeout,
        config.setup_full_compaction,
    )?;
    let load_storage = storage_evidence(&base, &pipeline)?;
    let load_observed_storage_bytes = storage_root
        .as_deref()
        .map(measurement::storage_size_bytes)
        .transpose()?;
    let load_peak_storage_bytes = sampler.map(measurement::StorageSampler::stop).transpose()?;
    let load_elapsed = started.elapsed();
    let load_rows = load.customer_rows + load.order_rows + load.line_rows;
    println!("run={run_number} phase=load rows={load_rows} transactions={} verified={} base_ms={:.3} forward_ms={:.3} delta_ms={:.3} q3_ms={:.3} prep_ms={:.3} background_merge_wait_ms={:.3} end_to_end_ms={:.3} driver_reference_bytes={} driver_max_load_retained_rows={} peak_process_rss_bytes={} {}",
        stats.transactions, config.verify, milliseconds(stats.base_phase()), milliseconds(stats.forward),
        milliseconds(stats.delta), milliseconds(stats.view_circuit), milliseconds(stats.preparation),
        milliseconds(load_drain), milliseconds(load_elapsed), state.as_ref().map_or(0, |s| s.size_of().total_bytes()), load.max_retained_rows, rss_bytes(), latency_summary(&stats.latencies));
    json!({"rows": load_rows, "transactions": load.transactions, "max_retained_rows": load.max_retained_rows,
        "catalog_storage": load_storage, "snapshot_publication_transactions": 1,
        "sampled_peak_storage_bytes": load_peak_storage_bytes,
        "observed_storage_bytes": load_observed_storage_bytes,
        "background_merge_wait_ms": milliseconds(load_drain), "elapsed_ms": milliseconds(load_elapsed),
        "setup_full_compaction": setup_compaction})
    };

    // The load program stops here: it checkpoints the loaded state and writes the manifest.
    if let Some(directory) = &config.write_image {
        let checkpoint = pipeline.write_checkpoint()?;
        let manifest = ImageManifest {
            format: ImageManifest::FORMAT,
            settings: ImageSettings::of(config),
            checkpoint,
            inputs: [
                ("customer", &config.customer),
                ("orders", &config.orders),
                ("lineitem", &config.lineitem),
            ]
            .into_iter()
            .filter_map(|(name, path)| Some((name.to_owned(), path.clone()?)))
            .collect(),
            load: load_report,
        };
        std::fs::write(
            directory.join(ImageManifest::FILE),
            serde_json::to_vec_pretty(&manifest)?,
        )?;
        println!(
            "run={run_number} phase=image directory={} checkpoint={}",
            directory.display(),
            manifest.checkpoint.uuid
        );
        return Ok(());
    }

    // An optional warm-up, untimed and unreported, so that the window can start with the
    // buffer cache that these refreshes leave instead of an empty one.
    // From here on (the warm-up and the window), each transaction evicts the base tables' blocks.
    pipeline.evict_base = true;
    let warm_up = match &config.warm_up {
        None => Value::Null,
        Some(list) => {
            let started = Instant::now();
            let refreshes = refresh_list_at(list)?;
            for refresh in &refreshes {
                for prepared in PreparedRefresh::read(refresh)? {
                    let (change, ..) = refresh_change(&pipeline, &base, prepared)?;
                    transact_inner(&mut pipeline, None, change, Some((&base, false)), &mut Measurements::default())?;
                    base = pipeline.take_base_state()?;
                }
            }
            json!({"refresh_list": list, "refreshes": refreshes.len(), "elapsed_ms": milliseconds(started.elapsed())})
        }
    };

    // Parse every refresh file now, so that the window reads no input file. The loop consumes
    // each prepared refresh by value and drops it once applied.
    let refresh_list = refreshes(config)?;
    let rss_before_pre_read = window_memory::current_rss_bytes();
    let prepared_refreshes = refresh_list
        .iter()
        .map(PreparedRefresh::read)
        .collect::<Result<Vec<_>>>()?;
    let refresh_inputs = json!({
        "per_refresh": prepared_refreshes
            .iter()
            .enumerate()
            .map(|(index, prepared)| PreparedRefresh::summary(prepared, index + 1))
            .collect::<Vec<_>>(),
        "vm_rss_before_pre_read_bytes": rss_before_pre_read,
        "vm_rss_after_pre_read_bytes": window_memory::current_rss_bytes(),
        "note": "Every refresh file is parsed into memory before the window; the loop drops each refresh's rows after applying it. VmRSS is Linux only (null elsewhere). The memory is ordinary process memory, equal in both modes; it counts toward the RSS-based pressure level and the window peak.",
    });
    // Evict the page cache of every input file the driver read, so that the window starts
    // without them cached.
    let input_paths: Vec<PathBuf> = [&config.customer, &config.orders, &config.lineitem]
        .into_iter()
        .flatten()
        .cloned()
        .chain(
            refresh_list
                .iter()
                .flat_map(Refresh::files)
                .map(Path::to_path_buf),
        )
        .collect();
    let inputs_evicted = window_memory::evict_inputs(&input_paths);
    drop(refresh_list);
    let sampler = storage_root
        .clone()
        .map(measurement::StorageSampler::start)
        .transpose()?;
    let sequence_resources = measurement::ResourceSnapshot::capture()?;
    let window_started_micros = dbsp::storage::diagnostics::now_micros();
    let mut window_evidence = window_memory::WindowMemory::start();
    // The buffer cache is left warm (there is no API to clear it); its state at the start of
    // the window is recorded beside its state at the end.
    let (cache_occupancy_at_start, _) = pipeline.runtime().cache_occupancy();
    pipeline.profile_cpu = true;
    let mut memory_pressure_before = memory_state(&pipeline)?;
    let sequence_started = Instant::now();
    let mut sequence_finished = sequence_started;
    let mut resources_after = sequence_resources;
    let mut window_finished_micros = window_started_micros;
    let mut reports = Vec::new();
    let mut verification_time = Duration::ZERO;
    let mut foreground_total = Duration::ZERO;
    #[cfg(feature = "merged-index")]
    let mut sequence_session_reads = SessionReads::default();
    #[cfg(feature = "merged-index")]
    let mut sequence_upkeep_reads = RunReads::default();
    let mut sequence_view_reads = [RunReads::default(); Component::ALL.len()];
    let mut sequence_view_expansions = [BlockExpansions::default(); Component::ALL.len()];
    #[cfg(feature = "merged-index")]
    let mut sequence_driver_expansions = [BlockExpansions::default(); Component::ALL.len()];
    #[cfg(feature = "merged-index")]
    let mut sequence_upkeep_expansions = BlockExpansions::default();
    for (number, transactions) in prepared_refreshes.into_iter().enumerate() {
        // The resource, pressure and window-evidence samples are taken around the refresh, not
        // around each of its transactions, so a refresh cut into small transactions does not
        // pay them per transaction (measurement version 14).
        let resources_before = measurement::ResourceSnapshot::capture()?;
        let reservations_before = pipeline.runtime().compaction_reservation_stats();
        pipeline.runtime().reset_peak_pinned_bytes();
        let compaction_jobs_before = pipeline.runtime().compaction_job_stats();
        pipeline.runtime().reset_peak_open_merges();
        let phase_started = Instant::now();
        let mut measurement = Measurements::default();
        let mut customers = HashSet::new();
        let mut fanout = HashMap::<i64, usize>::new();
        let (mut order_count, mut line_count, mut absent) = (0, 0, 0);
        let mut phase = "rf1";
        let mut latencies = TransactionLatencies::with_capacity(transactions.len());
        let mut run_counts = run_counts::RunCountSampler::start(transactions.len());
        for prepared in transactions {
            let transaction_started = Instant::now();
            let (base_before, ivm_before, verification_before) = (
                measurement.base_phase(),
                measurement.ivm_phase(),
                measurement.verification,
            );
            // Discovery is the RF2 lookup alone: the keys were read before the window.
            let (change, transaction_absent, transaction_phase, discovered) =
                refresh_change(&pipeline, &base, prepared)?;
            if discovered {
                measurement.discovery += transaction_started.elapsed();
            }
            let workload_stats_started = Instant::now();
            customers.extend(change.orders.iter().map(|(order, _)| order.customer_key));
            order_count += change.orders.len();
            for (line, _) in &change.lines {
                *fanout.entry(line.order_key).or_default() += 1;
            }
            line_count += change.lines.len();
            measurement.preparation += workload_stats_started.elapsed();
            transact_inner(
                &mut pipeline,
                state.as_mut(),
                change,
                Some((&base, discovered)),
                &mut measurement,
            )?;
            let cleanup_started = Instant::now();
            base = pipeline.take_base_state()?;
            measurement.publication_cleanup += cleanup_started.elapsed();
            absent += transaction_absent;
            phase = transaction_phase;
            // A transaction's latency is its foreground wall time, as a whole refresh's is.
            latencies.push(
                transaction_started
                    .elapsed()
                    .saturating_sub(measurement.verification - verification_before),
                measurement.base_phase() - base_before,
                measurement.ivm_phase() - ivm_before,
            );
            // After the transaction's latency is taken; its time leaves the refresh's below.
            run_counts.sample();
        }
        let customer_count = customers.len();
        let maximum_fanout = fanout.values().max().copied().unwrap_or(0);
        drop(customers);
        drop(fanout);
        // Wall time covers discovery, the transactions and all foreground cleanup; the refresh
        // files were read before the window.
        let phase_finished = Instant::now();
        measurement.foreground = phase_finished
            .duration_since(phase_started)
            .saturating_sub(measurement.verification)
            .saturating_sub(run_counts.sampling);
        verification_time += measurement.verification;
        foreground_total += measurement.foreground;
        sequence_finished = phase_finished;
        resources_after = measurement::ResourceSnapshot::capture()?;
        window_finished_micros = dbsp::storage::diagnostics::now_micros();
        window_evidence.sample_boundary();
        let memory_pressure_after = memory_state(&pipeline)?;
        let resource_delta = resources_before.delta(&resources_after);
        let mut report = phase_json(&measurement);
        report["refresh"] = json!(number + 1);
        report["phase"] = json!(phase);
        report["memory_pressure"] = json!({
            "before": std::mem::replace(&mut memory_pressure_before, memory_pressure_after.clone()),
            "after": memory_pressure_after,
        });
        report["transaction_latency"] = latencies.json();
        report["compaction_reservations"] = reservations_json(
            reservations_before,
            pipeline.runtime().compaction_reservation_stats(),
        );
        report["compaction_jobs"] = compaction_jobs_json(
            compaction_jobs_before,
            pipeline.runtime().compaction_job_stats(),
        );
        report["run_counts_sampling_ms"] = json!(milliseconds(run_counts.sampling));
        report["run_counts"] = run_counts.json();
        report["absent_orders"] = json!(absent);
        report["affected_customers"] = json!(customer_count);
        report["affected_orders"] = json!(order_count);
        report["line_count"] = json!(line_count);
        report["line_fanout_max"] = json!(maximum_fanout);
        report["line_fanout_mean"] = json!(if order_count == 0 {
            0.0
        } else {
            line_count as f64 / order_count as f64
        });
        report["resources"] = if config.verify {
            json!({"unavailable": "independent validation enabled; counters omitted to exclude validation"})
        } else {
            resource_delta
        };
        // The view worker counts its own run reads, and the driver counts the index's (the scan
        // session's in the view phase, and `prepare`'s in the base phase); validation on the
        // driver adds none of them.
        #[cfg(feature = "merged-index")]
        {
            sequence_session_reads += measurement.session_reads;
            sequence_upkeep_reads += measurement.upkeep_reads;
            report["index_run_reads"] = index_reads_json(&measurement.session_reads)?;
            report["index_session_counts"] = session_counts_json(&measurement.session_reads)?;
            report["driver_run_reads"] = driver_reads_json(&measurement.session_reads)?;
            report["index_upkeep_run_reads"] = upkeep_reads_json(&measurement.upkeep_reads)?;
        }
        #[cfg(not(feature = "merged-index"))]
        {
            report["index_run_reads"] = Value::Null;
            report["index_session_counts"] = Value::Null;
            report["driver_run_reads"] = Value::Null;
            report["index_upkeep_run_reads"] = Value::Null;
        }
        report["index_layout"] = json!(measurement.index_layout);
        for (total, reads) in sequence_view_reads.iter_mut().zip(measurement.view_reads) {
            *total += reads;
        }
        report["view_run_reads"] = view_reads_json(&measurement.view_reads)?;
        for (total, expansions) in sequence_view_expansions
            .iter_mut()
            .zip(measurement.view_expansions)
        {
            *total += expansions;
        }
        #[cfg(feature = "merged-index")]
        {
            sequence_driver_expansions
                .iter_mut()
                .zip(measurement.session_reads.driver_expansions)
                .for_each(|(total, expansions)| *total += expansions);
            sequence_upkeep_expansions += measurement.upkeep_expansions;
            report["block_expansions"] = block_expansions_json(
                &measurement.view_expansions,
                Some(&measurement.session_reads.driver_expansions),
                Some(&measurement.upkeep_expansions),
            )?;
        }
        #[cfg(not(feature = "merged-index"))]
        {
            report["block_expansions"] =
                block_expansions_json(&measurement.view_expansions, None, None)?;
        }
        println!(
            "run={run_number} refresh={} phase={phase} {}",
            number + 1,
            report
        );
        reports.push(report);
    }
    // The window ends with the last refresh; read its memory figures before anything else runs.
    let window_memory_fields = window_evidence.finish();
    #[cfg(feature = "merged-index")]
    let sequence_index_run_reads = index_reads_json(&sequence_session_reads)?;
    #[cfg(feature = "merged-index")]
    let sequence_index_session_counts = session_counts_json(&sequence_session_reads)?;
    #[cfg(feature = "merged-index")]
    let sequence_driver_run_reads = driver_reads_json(&sequence_session_reads)?;
    #[cfg(not(feature = "merged-index"))]
    let sequence_driver_run_reads = Value::Null;
    #[cfg(feature = "merged-index")]
    let sequence_index_upkeep_run_reads = upkeep_reads_json(&sequence_upkeep_reads)?;
    #[cfg(not(feature = "merged-index"))]
    let (sequence_index_run_reads, sequence_index_session_counts, sequence_index_upkeep_run_reads) =
        (Value::Null, Value::Null, Value::Null);
    #[cfg(feature = "merged-index")]
    let sequence_block_expansions = block_expansions_json(
        &sequence_view_expansions,
        Some(&sequence_driver_expansions),
        Some(&sequence_upkeep_expansions),
    )?;
    #[cfg(not(feature = "merged-index"))]
    let sequence_block_expansions = block_expansions_json(&sequence_view_expansions, None, None)?;
    let sequence_elapsed = sequence_finished
        .duration_since(sequence_started)
        .saturating_sub(verification_time);
    let observed_storage_bytes = storage_root
        .as_deref()
        .map(measurement::storage_size_bytes)
        .transpose()?;
    let sampled_peak_storage_bytes = sampler.map(measurement::StorageSampler::stop).transpose()?;
    let (cache_occupancy, cache_capacity) = pipeline.runtime().cache_occupancy();
    #[cfg(feature = "merged-index")]
    let manual_storage = Some(pipeline.index.storage_evidence()?);
    #[cfg(not(feature = "merged-index"))]
    let manual_storage: Option<Value> = None;
    let manual_file_backed = manual_storage.as_ref().map(|evidence| &evidence["file_backed"]);
    let base_profile = pipeline.base.circuit.retrieve_profile()?;
    let q3_profile = pipeline.view.circuit.retrieve_profile()?;
    let base_graph = pipeline.base.circuit.graph_profile()?.worker_graphs[0].to_json();
    let q3_graph = pipeline.view.circuit.graph_profile()?.worker_graphs[0].to_json();
    if cfg!(feature = "merged-index") && q3_graph.contains("shard_accumulate_trace") {
        bail!("replaced historical join trace remains in reconstructed circuit");
    }
    if let Some(path) = &config.report {
        std::fs::write(path.with_extension("base.profile.json"), base_profile.as_json())?;
        std::fs::write(path.with_extension("q3.profile.json"), q3_profile.as_json())?;
        std::fs::write(path.with_extension("base.graph.json"), &base_graph)?;
        std::fs::write(path.with_extension("q3.graph.json"), &q3_graph)?;
    }
    let storage_evidence = if config.verify {
        let mut evidence = base.storage_evidence();
        evidence.insert("V".into(), json!({"unavailable": "validation consumed the V output snapshot"}));
        Value::Object(evidence)
    } else {
        storage_evidence(&base, &pipeline)?
    };
    // The scan reads storage but writes none. It runs after the window's
    // counters, the disk usage and the cache occupancy have been read.
    if let Some(diagnostics) = &diagnostics {
        diagnostics.write_run_contents(
            &[storage_root
                .as_deref()
                .context("--diagnostics requires file storage")?],
        )?;
    }
    let mut report = json!({
        "status": "complete",
        "measurement_version": 22,
        "os": std::env::consts::OS,
        "mode": mode_name(),
        "qualification": "runner determines cgroup qualification; macOS is smoke only",
        "configuration": {
            "workers": config.workers, "load_batch_target": config.batch, "verification": config.verify,
            "driver_reference_bytes": state.as_ref().map_or(0, |s| s.size_of().total_bytes()),
            "rf2_discovery_full_scans": 0,
            "cache_bytes": config.cache_bytes, "memory_pressure_target_bytes": config.memory_target_bytes,
            "runtime": {
                "storage_path": storage_root, "cache_mib": cache_mib(config.cache_bytes),
                "note": "One runtime hosts the base and the view circuit, with one storage directory and one buffer cache of cache_bytes. The phases are told apart by transaction boundaries (base commits before the view transaction starts), with the CPU profiler's per-circuit step CPU time as a cross-check.",
            },
            "cache": "feldera_cache", "compression": "none",
            "evict_base_after_transaction": true,
            "filter_residency": match storage_options(config).filter_residency {
                feldera_types::config::FilterResidency::Storage => "storage",
                feldera_types::config::FilterResidency::Memory => "memory",
            },
            "min_storage_bytes": resolved_min_storage_bytes(&storage_options(config)),
            "min_storage_bytes_source": "DBSP default; the harness sets no value",
            "min_step_storage_bytes": "default (unlimited; memory-pressure spill remains enabled)",
            "run_placement": "default: at low memory pressure a merge output below min_storage_bytes stays in memory and inserted batches stay in memory; moderate pressure spills every merge output, high pressure every inserted batch",
            "native_join_fetch": false, "compaction_timeout_secs": config.compaction_timeout_secs,
            "compaction_policy": if config.setup_full_compaction {
                "setup full compaction after loading, before the refreshes; no compaction requested after any refresh; no final wait"
            } else {
                "no forced compaction; loading waits for normal background merges only; no final wait"
            },
            "setup_full_compaction": config.setup_full_compaction,
            "front_code_byte_keys": config.front_code_byte_keys,
            "elide_value_headers": config.elide_value_headers,
            "index_prefix_filter": config.index_prefix_filter,
            "memtable_mib": config.memtable_mib,
            "memory_model": memory_model_name(config.memory_model),
            "compaction_memory": config.effective_compaction_memory().map(compaction_memory_name),
            "compaction_jobs": config.effective_compaction_jobs(),
            "session_pass": cfg!(feature = "merged-index").then(|| pass_name(config.session_pass())),
            "durability": "no checkpoint is taken during the refreshes; an image (--write-image) is the checkpoint the refresh program starts from; refresh completion is not a durability guarantee",
            "diagnostics": config.diagnostics,
        },
        // Process-wide writer counters: this process runs one measurement.
        "front_coding": {
            "blocks": FRONT_CODED_BLOCKS.load(Ordering::Relaxed),
            "layout_fallbacks": FRONT_CODING_LAYOUT_FALLBACKS.load(Ordering::Relaxed),
            "size_fallbacks": FRONT_CODING_SIZE_FALLBACKS.load(Ordering::Relaxed),
        },
        "value_header_elision": {
            "blocks": ELIDED_VALUE_HEADER_BLOCKS.load(Ordering::Relaxed),
            "layout_fallbacks": VALUE_ELISION_LAYOUT_FALLBACKS.load(Ordering::Relaxed),
            "size_fallbacks": VALUE_ELISION_SIZE_FALLBACKS.load(Ordering::Relaxed),
        },
        "load": load_report,
        "image": image.as_ref().map(|image| json!({
            "directory": config.from_image, "settings": image.settings,
            "checkpoint": image.checkpoint, "inputs": image.inputs, "load": image.load,
        })),
        "warm_up": warm_up,
        "refresh_inputs": refresh_inputs,
        "inputs_evicted": inputs_evicted,
        "refreshes": reports,
        "sequence": {
            "foreground_ms": milliseconds(foreground_total),
            "end_to_end_ms": milliseconds(sequence_elapsed), "observed_storage_bytes": observed_storage_bytes,
            "sampled_peak_storage_bytes": sampled_peak_storage_bytes, "storage_peak_sampling_ms": 10,
            // The one buffer cache of the runtime.
            "cache_occupancy_bytes": cache_occupancy,
            "cache_occupancy_at_window_start_bytes": cache_occupancy_at_start,
            "snapshot_publication_transactions": 0,
            "cache_capacity_bytes": cache_capacity,
            "window_started_micros": window_started_micros, "window_finished_micros": window_finished_micros,
            "resources": if config.verify { json!({"unavailable": "independent validation enabled; counters omitted to exclude validation"}) } else { sequence_resources.delta(&resources_after) },
            "manual_index_file_backed": manual_file_backed,
            "index_run_reads": sequence_index_run_reads,
            "index_session_counts": sequence_index_session_counts,
            "driver_run_reads": sequence_driver_run_reads,
            "index_upkeep_run_reads": sequence_index_upkeep_run_reads,
            "view_run_reads": view_reads_json(&sequence_view_reads)?,
            "block_expansions": sequence_block_expansions,
            "manual_index_storage": manual_storage,
            "completion_boundary": "last_refresh_complete",
            "catalog_storage": storage_evidence,
            "replaced_historical_join_traces_absent": if cfg!(feature = "merged-index") { Some(true) } else { None },
            "circuit_profile": {
                "base": serde_json::to_value(base_profile)?,
                "q3": serde_json::to_value(q3_profile)?,
            },
        },
    });
    report["sequence"]
        .as_object_mut()
        .context("sequence is an object")?
        .extend(window_memory_fields);
    if let Some(path) = &config.report {
        std::fs::write(path, serde_json::to_vec_pretty(&report)?)?;
    }
    if let Some(diagnostics) = &diagnostics {
        diagnostics.finish(diagnostics::Window {
            started_micros: window_started_micros,
            finished_micros: window_finished_micros,
        })?;
    }
    if let Some(root) = &config.dump_runtime_circuit {
        let directory = root.join(format!("run-{run_number}"));
        let revision = Command::new("git")
            .args(["-C", env!("CARGO_MANIFEST_DIR"), "rev-parse", "HEAD"])
            .output()
            .context("reading source revision")?;
        if !revision.status.success() {
            bail!("git rev-parse HEAD failed with {}", revision.status);
        }
        let source_status = Command::new("git")
            .args(["-C", env!("CARGO_MANIFEST_DIR"), "status", "--short"])
            .output()
            .context("reading source worktree status")?;
        if !source_status.status.success() {
            bail!("git status --short failed with {}", source_status.status);
        }
        let manifest = json!({
            "revision": String::from_utf8(revision.stdout)?.trim().to_owned(),
            "worktree_status": String::from_utf8(source_status.stdout)?,
            "command": std::env::args().collect::<Vec<_>>(),
            "configuration": {
                "workers": config.workers,
                "batch": config.batch,
                "runs": config.runs,
                "verify": config.verify,
                "storage_mode": if config.storage_path.is_some() { "file" } else { "memory" },
                "storage_path": config.storage_path,
                "customer": config.customer,
                "orders": config.orders,
                "lineitem": config.lineitem,
                "rf1_orders": config.rf1_orders,
                "rf1_lines": config.rf1_lines,
                "rf2": config.rf2,
            },
            "capture_stage": "after final refresh report and outside phase timers",
            "run": run_number,
        });
        // One capture per circuit: `run-N/base` and `run-N/q3`.
        std::fs::create_dir_all(&directory)?;
        let capture_result = dump_runtime_circuit(&mut pipeline.base.circuit, &directory.join("base"))
            .and_then(|()| dump_runtime_circuit(&mut pipeline.view.circuit, &directory.join("q3")));
        std::fs::write(
            directory.join("manifest.json"),
            serde_json::to_vec_pretty(&manifest)?,
        )?;
        capture_result?;
    }
    Ok(())
    })
}

fn main() -> Result<()> {
    let config = Config::parse()?;
    if let Some(root) = &config.dump_runtime_circuit {
        std::fs::create_dir_all(root)?;
        for run_number in 1..=config.runs {
            let directory = root.join(format!("run-{run_number}"));
            if directory.exists() {
                bail!("capture directory already exists: {}", directory.display());
            }
        }
    }
    for run_number in 1..=config.runs {
        run(&config, run_number)?;
    }
    Ok(())
}

#[cfg(all(test, not(feature = "merged-index")))]
mod tests {
    use super::*;

    fn customer(key: i64, segment: &str) -> Customer {
        Customer {
            key,
            segment: segment.to_owned(),
        }
    }

    fn order(key: i64, customer_key: i64) -> Order {
        Order {
            key,
            customer_key,
            date: "1996-01-02".to_owned(),
            ship_priority: 0,
        }
    }

    fn line(order_key: i64, number: i32) -> Line {
        Line {
            order_key,
            number,
            price_cents: 100,
            discount_hundredths: 4,
            ship_date: "1996-03-13".to_owned(),
        }
    }

    #[test]
    fn refresh_discovery_keeps_related_records_together() {
        let mut state = Reference::default();
        state.orders.insert(20, order(20, 2));
        state.orders.insert(30, order(30, 3));
        state.lines.insert((20, 1), line(20, 1));
        state.lines.insert((20, 2), line(20, 2));
        state.lines.insert((30, 1), line(30, 1));
        let keys = [data::delete_order_key("20|").unwrap(), 20, 99]
            .into_iter()
            .collect();
        let change = rf2_changes(&state, &keys);
        assert_eq!(change.orders.len(), 1);
        assert_eq!(change.lines.len(), 2);
        assert!(change.lines.iter().all(|(line, _)| line.order_key == 20));
    }

    #[test]
    fn cli_rejects_invalid_refresh_and_counts() {
        let parse = |args: &[&str]| Config::from_args(args.iter().map(|arg| (*arg).to_owned()));
        assert!(parse(&["--workers", "0"]).is_err());
        assert!(parse(&["--batch", "0"]).is_err());
        assert!(parse(&["--runs", "0"]).is_err());
        assert!(parse(&["--rf1-orders", "orders.tbl.u1"]).is_err());
        assert!(parse(&["--rf1-lines", "lineitem.tbl.u1"]).is_err());
        assert!(parse(&["--bogus"]).is_err());
        assert!(parse(&["--dump-runtime-circuit"]).is_err());
        assert!(parse(&["--session-pass"]).is_err());
        assert!(parse(&["--session-pass", "Seek"]).is_err());
        assert_eq!(parse(&[]).unwrap().session_pass(), Pass::Seek);
        // The memory model defaults to rss; budgets needs the memtable budget and no image.
        assert_eq!(parse(&[]).unwrap().memory_model, MemoryModel::Rss);
        assert!(parse(&["--memory-model"]).is_err());
        assert!(parse(&["--memory-model", "Budgets"]).is_err());
        assert_eq!(
            parse(&["--memory-model", "rss"]).unwrap().memory_model,
            MemoryModel::Rss
        );
        assert!(parse(&["--memory-model", "budgets"]).is_err());
        assert_eq!(
            parse(&["--memory-model", "budgets", "--memtable-mib", "64"])
                .unwrap()
                .memory_model,
            MemoryModel::Budgets
        );
        assert!(parse(&[
            "--write-image",
            "i",
            "--memory-model",
            "budgets",
            "--memtable-mib",
            "64"
        ])
        .is_err());
        // Compaction memory defaults to the pool of two jobs under budgets, and to nothing
        // under rss; setting it needs the budgets model.
        let budgets = ["--memory-model", "budgets", "--memtable-mib", "64"];
        let with_budgets = |extra: &[&str]| parse(&[&budgets[..], extra].concat());
        let unset = parse(&[]).unwrap();
        assert_eq!(unset.effective_compaction_memory(), None);
        assert_eq!(unset.effective_compaction_jobs(), None);
        let pool = with_budgets(&[]).unwrap();
        assert_eq!(
            pool.effective_compaction_memory(),
            Some(CompactionMemory::Pool)
        );
        assert_eq!(
            pool.effective_compaction_jobs(),
            Some(DEFAULT_COMPACTION_JOBS)
        );
        let one_job = with_budgets(&["--compaction-jobs", "1"]).unwrap();
        assert_eq!(one_job.effective_compaction_jobs(), Some(1));
        let charge = with_budgets(&["--compaction-memory", "charge"]).unwrap();
        assert_eq!(
            charge.effective_compaction_memory(),
            Some(CompactionMemory::Charge)
        );
        assert_eq!(charge.effective_compaction_jobs(), None);
        for refused in [
            &["--compaction-memory", "charge"][..],
            &["--compaction-memory", "pool"],
            &["--compaction-jobs", "2"],
            &["--memtable-mib", "64", "--compaction-memory", "charge"],
        ] {
            assert!(parse(refused).is_err(), "{refused:?}");
        }
        assert!(with_budgets(&["--compaction-memory", "other"]).is_err());
        assert!(with_budgets(&["--compaction-jobs", "0"]).is_err());
        assert!(with_budgets(&["--compaction-jobs", "x"]).is_err());
        // The charge has no job limit.
        assert!(
            with_budgets(&["--compaction-memory", "charge", "--compaction-jobs", "2"]).is_err()
        );
        assert!(with_budgets(&["--compaction-memory", "pool", "--compaction-jobs", "2"]).is_ok());
        // Images are built under rss, without compaction memory.
        assert!(with_budgets(&["--write-image", "i"]).is_err());
        assert!(parse(&["--write-image", "i", "--compaction-jobs", "2"]).is_err());
        // The load and the refresh program take disjoint inputs.
        assert!(parse(&["--write-image", "i", "--from-image", "j", "--no-verify"]).is_err());
        assert!(parse(&["--write-image", "i", "--refresh-list", "r.json"]).is_err());
        assert!(parse(&["--write-image", "i", "--storage-path", "s"]).is_err());
        assert!(parse(&["--write-image", "i", "--runs", "1"]).is_ok());
        // A refresh program records diagnostics of its image copy's storage.
        assert!(parse(&[
            "--from-image",
            "i",
            "--no-verify",
            "--runs",
            "1",
            "--diagnostics",
            "d"
        ])
        .is_ok());
        assert!(parse(&["--diagnostics", "d", "--runs", "1"]).is_err());
        assert!(
            parse(&["--from-image", "i", "--runs", "1"]).is_err(),
            "verification needs the single process"
        );
        assert!(parse(&[
            "--from-image",
            "i",
            "--runs",
            "1",
            "--no-verify",
            "--customer",
            "c"
        ])
        .is_err());
        assert!(parse(&[
            "--from-image",
            "i",
            "--runs",
            "1",
            "--no-verify",
            "--storage-path",
            "s"
        ])
        .is_err());
        assert!(parse(&["--from-image", "i", "--runs", "1", "--no-verify"]).is_ok());
        assert!(parse(&["--warm-up", "w.json", "--no-verify"]).is_err());
        assert!(parse(&[
            "--from-image",
            "i",
            "--runs",
            "1",
            "--no-verify",
            "--warm-up",
            "w.json"
        ])
        .is_ok());
        for name in ["seek", "step"] {
            let pass = parse(&["--session-pass", name]).unwrap().session_pass();
            assert_eq!(pass_name(pass), name);
        }
        let config = parse(&[
            "--workers",
            "2",
            "--batch",
            "13",
            "--runs",
            "1",
            "--rf1-orders",
            "orders.tbl.u1",
            "--rf1-lines",
            "lineitem.tbl.u1",
            "--dump-runtime-circuit",
            "graphs",
        ])
        .unwrap();
        assert_eq!((config.workers, config.batch, config.runs), (2, 13, 1));
        assert_eq!(
            config.dump_runtime_circuit.as_deref(),
            Some(Path::new("graphs"))
        );
    }

    #[test]
    fn runtime_classification_is_explicit_and_preserves_node_ids() -> Result<()> {
        let graph = json!({
            "nodes": {"id":"root", "label":"", "nodes": [
                {"Simple":{"id":"n1", "label":"AccumulateTraceAppend @ file.rs:1:1\\lrecords: 4"}},
                {"Cluster":{"id":"integral", "label":"shard_accumulate_integrate_trace @ trace.rs:1:1", "nodes": [
                    {"Simple":{"id":"n2", "label":"JoinTrace @ join.rs:2:1"}},
                    {"Simple":{"id":"n3", "label":"FutureOperator"}},
                    {"Simple":{"id":"n4", "label":"TraceAppend"}},
                    {"Simple":{"id":"n5", "label":"InputUpsert"}},
                    {"Simple":{"id":"n6", "label":"AccumulateDelayTrace"}}
                ]}},
                {"Cluster":{"id":"trace", "label":"shard_accumulate_trace", "nodes": []}},
                {"Cluster":{"id":"other", "label":"join", "nodes": []}}
            ]},
            "edges": []
        });
        let styles = runtime_node_styles(&graph.to_string())?;
        assert_eq!(styles.get("n1"), Some(&CircuitNodeStyle::OtherState));
        assert_eq!(styles.get("n2"), Some(&CircuitNodeStyle::OtherState));
        assert_eq!(styles.get("n3"), Some(&CircuitNodeStyle::Unclassified));
        assert_eq!(styles.get("n4"), Some(&CircuitNodeStyle::OtherState));
        assert_eq!(styles.get("n5"), Some(&CircuitNodeStyle::Unclassified));
        assert_eq!(styles.get("n6"), Some(&CircuitNodeStyle::Accumulation));
        assert_eq!(
            styles.get("integral"),
            Some(&CircuitNodeStyle::Accumulation)
        );
        assert_eq!(styles.get("trace"), Some(&CircuitNodeStyle::OtherState));
        assert!(!styles.contains_key("root"));
        assert!(!styles.contains_key("other"));
        assert!(runtime_node_styles("not JSON").is_err());
        assert!(runtime_node_styles("{}").is_err());
        Ok(())
    }

    #[test]
    fn runtime_capture_writes_each_worker_without_overwriting() -> Result<()> {
        for workers in [1, 2] {
            let mut pipeline = Pipeline::with_workers(workers)?;
            let mut state = Reference::default();
            let mut stats = Measurements::default();
            transact(
                &mut pipeline,
                &mut state,
                Change {
                    customers: vec![(customer(2, "BUILDING"), true)],
                    orders: vec![(order(20, 2), true)],
                    lines: vec![(line(20, 1), true)],
                },
                true,
                &mut stats,
            )?;
            let directory = std::env::temp_dir().join(format!(
                "q3-runtime-capture-{}-{workers}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)?
                    .as_nanos()
            ));
            dump_runtime_circuit(&mut pipeline.view.circuit, &directory)?;
            dump_runtime_circuit(&mut pipeline.base.circuit, &directory.join("base"))?;
            for worker in 0..workers {
                for extension in ["dot", "json", "svg"] {
                    assert!(directory
                        .join(format!("worker-{worker}.{extension}"))
                        .exists());
                }
                assert!(directory
                    .join(format!("worker-{worker}-styled.dot"))
                    .exists());
                assert!(directory
                    .join(format!("worker-{worker}-styled.svg"))
                    .exists());
            }
            assert!(dump_runtime_circuit(&mut pipeline.view.circuit, &directory).is_err());
        }
        Ok(())
    }

    #[test]
    fn graphviz_failure_is_reported() -> Result<()> {
        let path = std::env::temp_dir().join(format!(
            "q3-invalid-dot-{}-{}.dot",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ));
        std::fs::write(&path, "digraph { invalid ->")?;
        assert!(render_svg(&path).is_err());
        Ok(())
    }

    #[test]
    fn two_worker_circuit_matches_reference() -> Result<()> {
        let mut pipeline = Pipeline::with_workers(2)?;
        let mut state = Reference::default();
        let mut stats = Measurements::default();
        transact(
            &mut pipeline,
            &mut state,
            Change {
                customers: vec![(customer(2, "BUILDING"), true)],
                orders: vec![(order(20, 2), true)],
                lines: vec![(line(20, 1), true)],
            },
            true,
            &mut stats,
        )?;
        assert_eq!(state.evaluate().len(), 1);
        Ok(())
    }

    #[test]
    fn simultaneous_three_source_changes_match_reference() -> Result<()> {
        for workers in [1, 2] {
            let mut pipeline = Pipeline::with_workers(workers)?;
            let mut state = Reference::default();
            let mut stats = Measurements::default();
            let base_customer = customer(2, "BUILDING");
            let base_order = order(20, 2);
            let base_line = line(20, 1);
            transact(
                &mut pipeline,
                &mut state,
                Change {
                    customers: vec![(base_customer.clone(), true)],
                    orders: vec![(base_order.clone(), true)],
                    lines: vec![(base_line.clone(), true)],
                },
                true,
                &mut stats,
            )?;

            let changed_customer = customer(2, "AUTOMOBILE");
            let mut changed_order = base_order.clone();
            changed_order.date = "1996-02-03".to_owned();
            let mut changed_line = base_line.clone();
            changed_line.price_cents = 275;
            transact(
                &mut pipeline,
                &mut state,
                Change {
                    customers: vec![(base_customer, false), (changed_customer, true)],
                    orders: vec![(base_order, false), (changed_order, true)],
                    lines: vec![(base_line, false), (changed_line, true)],
                },
                true,
                &mut stats,
            )?;
            assert_eq!(stats.view_rows, Some(1));
        }
        Ok(())
    }
}

/// Deltas beyond RF1/RF2, in both builds: a pipeline's `V` must equal the independent join after
/// every change, whatever the build. In the reconstructed build this exercises the index's
/// complete `Dlt` (owner changes, orphan lines) through the hand-written view circuit.
#[cfg(test)]
mod general_delta_tests {
    use super::*;

    fn customer(key: i64, segment: &str) -> Customer {
        Customer {
            key,
            segment: segment.to_owned(),
        }
    }

    fn order(key: i64, customer_key: i64) -> Order {
        Order {
            key,
            customer_key,
            date: "1996-01-02".to_owned(),
            ship_priority: 0,
        }
    }

    fn line(order_key: i64, number: i32) -> Line {
        Line {
            order_key,
            number,
            price_cents: 100,
            discount_hundredths: 4,
            ship_date: "1996-03-13".to_owned(),
        }
    }

    #[test]
    fn native_deltas_match_full_join_across_updates() -> Result<()> {
        let mut pipeline = Pipeline::with_workers(1)?;
        let mut state = Reference::default();
        let mut stats = Measurements::default();
        let mut step = |change| transact(&mut pipeline, &mut state, change, true, &mut stats);

        // Orphan lines and unmatched orders produce no joined row.
        step(Change {
            orders: vec![(order(20, 2), true)],
            lines: vec![(line(20, 1), true), (line(99, 1), true)],
            ..Change::default()
        })?;
        step(Change {
            customers: vec![(customer(2, "BUILDING"), true)],
            ..Change::default()
        })?;
        // Distinct line identities retain multiplicity when payloads are identical.
        step(Change {
            lines: vec![(line(20, 2), true)],
            ..Change::default()
        })?;
        // Replacement and key move, with the associated order and line changes together.
        step(Change {
            customers: vec![
                (customer(2, "BUILDING"), false),
                (customer(2, "AUTOMOBILE"), true),
            ],
            ..Change::default()
        })?;
        let mut changed_line = line(20, 1);
        changed_line.price_cents = 250;
        changed_line.discount_hundredths = 7;
        changed_line.ship_date = "1996-04-01".to_owned();
        step(Change {
            lines: vec![(line(20, 1), false), (changed_line.clone(), true)],
            ..Change::default()
        })?;
        // A stable order identity can move to another customer without line updates.
        step(Change {
            customers: vec![(customer(3, "HOUSEHOLD"), true)],
            orders: vec![(order(20, 2), false), (order(20, 3), true)],
            ..Change::default()
        })?;
        step(Change {
            orders: vec![(order(20, 3), false), (order(21, 3), true)],
            lines: vec![
                (changed_line, false),
                (line(20, 2), false),
                (line(21, 1), true),
                (line(21, 2), true),
            ],
            ..Change::default()
        })?;
        // Simultaneous deletion and insertion, then RF2 style removal.
        step(Change {
            orders: vec![(order(21, 3), false), (order(30, 2), true)],
            lines: vec![
                (line(21, 1), false),
                (line(21, 2), false),
                (line(30, 1), true),
            ],
            ..Change::default()
        })?;
        step(Change {
            orders: vec![(order(30, 2), false)],
            lines: vec![(line(30, 1), false)],
            ..Change::default()
        })?;
        step(Change::default())?;
        assert_eq!(stats.transactions, 10);
        Ok(())
    }

    /// A small deterministic generator, so that a failing seed reproduces.
    struct Generator(u64);

    impl Generator {
        /// A number below `bound`.
        fn below(&mut self, bound: u64) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (self.0 >> 33) % bound
        }
    }

    /// A random change of the primary-key tables, valid against `state`: at most one operation
    /// per key. A key that exists is deleted or replaced (an order replaced may change its
    /// customer, which moves its line items), and a key that does not exist is inserted. Line
    /// items may have no order, and orders may have no customer.
    fn random_change(generator: &mut Generator, state: &Reference) -> Change {
        let mut change = Change::default();
        for key in 1..=3 {
            let old = state.customers.get(&key);
            let segment = ["AUTOMOBILE", "BUILDING", "MACHINERY"][generator.below(3) as usize];
            match (generator.below(4), old) {
                (0, Some(old)) => change.customers.push((old.clone(), false)),
                (0, None) => change.customers.push((customer(key, segment), true)),
                (1, Some(old)) if old.segment != segment => {
                    change.customers.push((old.clone(), false));
                    change.customers.push((customer(key, segment), true));
                }
                _ => {}
            }
        }
        for key in 1..=5 {
            let old = state.orders.get(&key);
            let mut new = order(key, 1 + generator.below(3) as i64);
            new.ship_priority = generator.below(3) as i32;
            match (generator.below(4), old) {
                (0, Some(old)) => change.orders.push((old.clone(), false)),
                (0, None) => change.orders.push((new, true)),
                (1, Some(old)) if *old != new => {
                    change.orders.push((old.clone(), false));
                    change.orders.push((new, true));
                }
                _ => {}
            }
        }
        for order_key in 1..=5 {
            for number in 1..=3 {
                let old = state.lines.get(&(order_key, number));
                let mut new = line(order_key, number);
                new.price_cents = 100 + 50 * generator.below(3) as i64;
                match (generator.below(4), old) {
                    (0, Some(old)) => change.lines.push((old.clone(), false)),
                    (0, None) => change.lines.push((new, true)),
                    (1, Some(old)) if *old != new => {
                        change.lines.push((old.clone(), false));
                        change.lines.push((new, true));
                    }
                    _ => {}
                }
            }
        }
        change
    }

    /// Random changes of every kind, in sequence from several seeds: after each, `V` and its
    /// delta equal the independent join of the reference (`transact` checks them), and, in the
    /// reconstructed build, the index and the view circuit have taken the same change by way of
    /// `prepare`'s complete `Dlt`.
    #[test]
    fn random_general_changes_match_the_reference_join() -> Result<()> {
        // Orders that moved to another customer while they had line items, and line items
        // inserted or kept with no order: the hard cases must occur.
        let (mut moves_with_lines, mut orphan_lines_seen) = (0, 0);
        for seed in 0..8 {
            let mut generator = Generator(seed);
            let mut pipeline = Pipeline::with_workers(1)?;
            let mut state = Reference::default();
            let mut stats = Measurements::default();
            for _ in 0..30 {
                let change = random_change(&mut generator, &state);
                moves_with_lines += change
                    .orders
                    .iter()
                    .filter(|(new, insert)| {
                        *insert
                            && state.orders.get(&new.key).is_some_and(|old| {
                                old.customer_key != new.customer_key
                                    && state.lines.keys().any(|(order, _)| *order == new.key)
                            })
                    })
                    .count();
                transact(&mut pipeline, &mut state, change, true, &mut stats)
                    .with_context(|| format!("seed {seed}, transaction {}", stats.transactions))?;
                orphan_lines_seen += state
                    .lines
                    .keys()
                    .filter(|(order, _)| !state.orders.contains_key(order))
                    .count();
            }
            assert_eq!(stats.transactions, 30);
        }
        assert!(moves_with_lines > 0, "no order moved with its line items");
        assert!(orphan_lines_seen > 0, "no line item was ever an orphan");
        Ok(())
    }
}

#[cfg(all(test, feature = "merged-index"))]
mod reconstructed_tests {
    use super::*;

    fn customer(key: i64) -> Customer {
        Customer {
            key,
            segment: "BUILDING".to_owned(),
        }
    }

    fn order(key: i64, customer_key: i64) -> Order {
        Order {
            key,
            customer_key,
            date: "1996-01-02".to_owned(),
            ship_priority: 0,
        }
    }

    fn line(order_key: i64, number: i32) -> Line {
        Line {
            order_key,
            number,
            price_cents: 100,
            discount_hundredths: 4,
            ship_date: "1996-03-13".to_owned(),
        }
    }

    #[test]
    fn rf1_rf2_match_full_join() -> Result<()> {
        let mut pipeline = Pipeline::with_workers(1)?;
        let mut state = Reference::default();
        let mut stats = Measurements::default();
        transact(
            &mut pipeline,
            &mut state,
            Change {
                customers: vec![(customer(2), true), (customer(3), true)],
                ..Change::default()
            },
            true,
            &mut stats,
        )?;
        transact(
            &mut pipeline,
            &mut state,
            Change {
                orders: vec![(order(20, 2), true)],
                lines: vec![(line(20, 1), true), (line(20, 2), true)],
                ..Change::default()
            },
            true,
            &mut stats,
        )?;
        transact(
            &mut pipeline,
            &mut state,
            Change {
                orders: vec![(order(30, 3), true)],
                lines: vec![(line(30, 1), true)],
                ..Change::default()
            },
            true,
            &mut stats,
        )?;
        let keys = HashSet::from([20]);
        let deletion = rf2_changes(&state, &keys);
        transact(&mut pipeline, &mut state, deletion, true, &mut stats)?;
        transact(
            &mut pipeline,
            &mut state,
            Change::default(),
            true,
            &mut stats,
        )?;
        assert_eq!(stats.view_rows, Some(1));
        assert_eq!(stats.transactions, 5);
        // The view circuit has no historical join traces, and the base circuit has no joins.
        let graph = pipeline.view.circuit.graph_profile()?.worker_graphs[0].to_json();
        assert!(!graph.contains("shard_accumulate_trace"));
        assert!(graph.contains("JoinTrace"));
        let base_graph = pipeline.base.circuit.graph_profile()?.worker_graphs[0].to_json();
        assert!(!base_graph.contains("JoinTrace"));
        Ok(())
    }

    #[test]
    fn grouped_load_keeps_oversized_order_and_lines_together() -> Result<()> {
        let batches = reconstruction::grouped_load(
            Change {
                customers: vec![(customer(2), true)],
                orders: vec![(order(20, 2), true), (order(30, 2), true)],
                lines: vec![
                    (line(20, 1), true),
                    (line(20, 2), true),
                    (line(30, 1), true),
                ],
            },
            2,
        )?;
        assert_eq!(batches.len(), 3);
        assert_eq!(batches[1].orders[0].0.key, 20);
        assert_eq!(batches[1].lines.len(), 2);
        assert_eq!(batches[2].orders[0].0.key, 30);
        Ok(())
    }

    #[test]
    fn published_views_cover_multiple_transaction_steps() -> Result<()> {
        let mut pipeline = Pipeline::with_workers(1)?;
        let mut reference = Reference::default();
        let mut stats = Measurements::default();
        transact(
            &mut pipeline,
            &mut reference,
            Change {
                customers: vec![(customer(2), true)],
                ..Change::default()
            },
            true,
            &mut stats,
        )?;

        let complete = Change {
            orders: vec![(order(20, 2), true)],
            lines: vec![(line(20, 1), true), (line(20, 2), true)],
            ..Change::default()
        };
        let before = reference.evaluate();
        // One publication covers the steps of the view circuit's transaction.
        transact_in_steps(&mut pipeline, &mut reference, complete)?;

        let output = pipeline
            .view
            .catalog
            .output_handles(&SqlIdentifier::from("V"))
            .context("missing V output")?;
        let expected = reference.evaluate();
        assert_eq!(
            read_output(&*output.delta_handle)?,
            difference(&before, &expected)
        );
        assert_eq!(
            read_output(
                &**output
                    .integrate_handle
                    .as_ref()
                    .context("V has no materialized snapshot")?
            )?,
            expected
        );
        Ok(())
    }

    /// An append returns `Old`, the snapshot before it, and `Dlt`, the run it appended. With no
    /// memory pressure the run is built in memory.
    #[test]
    fn append_returns_old_and_the_appended_run() -> Result<()> {
        use dbsp::trace::{BatchLocation, BatchReader};

        let mut pipeline = Pipeline::with_workers(1)?;
        let refresh = Change {
            customers: vec![(customer(2), true)],
            orders: vec![(order(20, 2), true)],
            lines: vec![(line(20, 1), true), (line(20, 2), true)],
        };
        let mut reference = Reference::default();
        let net_delta = run_base(&mut pipeline, &mut reference, refresh, false)?;
        let (index_rows, _, _) =
            complete_net_delta(&mut pipeline, &net_delta, None, Some(&reference))?;
        let (old, dlt) = pipeline.index.append(index_rows)?;
        let rows = |snapshot: &reconstruction::Snapshot| -> usize {
            snapshot.batches().iter().map(|run| run.len()).sum()
        };
        assert_eq!(rows(&old), 0);
        // One customer, one order, and two line items.
        assert_eq!(dlt.len(), 4);
        assert_eq!(rows(&pipeline.index.snapshot()?), 4);
        assert_eq!(dlt.location(), BatchLocation::Memory);
        Ok(())
    }

    /// What the index's thread and the view worker make of the buffer cache once the index's
    /// thread has written a file run.
    struct IndexFileRun {
        /// Whether the two threads use the same cache object.
        same_cache: bool,
        /// The blocks of the file run, and how many of them the view worker's cache holds.
        blocks: usize,
        blocks_in_view_cache: usize,
        /// The view worker's cache: `(used, capacity)`.
        occupancy: (usize, usize),
    }

    /// Appends one refresh to the index, so that the index's thread writes the file run, and
    /// reports where the run's blocks landed.
    ///
    /// The index inserts its run into the spine on its own thread, and the spine writes it to a
    /// file there only under memory pressure. A target far below the process's size puts the
    /// runtime under pressure for good, and the test waits for the level to rise before it
    /// appends. The index then holds one run, so nothing merges it, and no background thread
    /// writes a file that the check could mistake for the index thread's. The check is by direct
    /// lookup of each block, so it neither reads from storage nor counts a hit or a miss.
    fn append_one_file_run_to_the_index() -> Result<IndexFileRun> {
        use dbsp::trace::{Batch, BatchLocation, BatchReader};
        use feldera_types::memory_pressure::MemoryPressure;

        let storage = std::env::temp_dir().join(format!(
            "q3-index-cache-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ));
        let config = Config {
            workers: 1,
            // Room for far more than the few kilobytes of the run.
            cache_bytes: Some(64 * 1024 * 1024),
            memory_target_bytes: Some(1024 * 1024),
            ..Config::default()
        };
        let mut pipeline = Pipeline::new(runtime_config(&config, Some(&storage))?, false)?;
        pipeline.enable_contract_check();
        measurement::drain_until(Duration::from_secs(30), || {
            Ok(pipeline.runtime().current_memory_pressure() >= MemoryPressure::High)
        })
        .context("the runtime did not come under memory pressure")?;

        let refresh = Change {
            customers: vec![(customer(2), true)],
            orders: vec![(order(20, 2), true), (order(30, 2), true)],
            lines: vec![
                (line(20, 1), true),
                (line(20, 2), true),
                (line(30, 1), true),
            ],
        };
        let mut reference = Reference::default();
        let net_delta = run_base(&mut pipeline, &mut reference, refresh, false)?;
        let (index_rows, _, _) =
            complete_net_delta(&mut pipeline, &net_delta, None, Some(&reference))?;
        pipeline.index.append(index_rows)?;
        let runs: Vec<_> = pipeline
            .index
            .snapshot()?
            .batches()
            .iter()
            .filter(|run| !run.is_empty())
            .cloned()
            .collect();
        assert_eq!(runs.len(), 1, "the index holds more than the appended run");
        assert_eq!(
            runs[0].location(),
            BatchLocation::Storage,
            "the appended run is not in a file"
        );

        let file = runs[0].file_reader().context("a file run has a file")?;
        let locations = runs[0]
            .file_block_locations()?
            .context("a file run has block locations")?;
        let index_cache = pipeline.index.buffer_cache()?;
        let view_cache = pipeline
            .view_worker_cache
            .get()
            .context("the view circuit was not built")?
            .clone();
        Ok(IndexFileRun {
            same_cache: Arc::ptr_eq(&index_cache, &view_cache),
            blocks: locations.len(),
            blocks_in_view_cache: locations
                .iter()
                .filter(|location| view_cache.get(&*file, **location).is_some())
                .count(),
            occupancy: view_cache.occupancy(),
        })
    }

    /// The index's thread writes its file runs through the buffer cache that the view worker
    /// reads: the two threads hold the same cache object, and every block of the file run is in
    /// the worker's cache. The cache has room to spare, so no block was evicted.
    #[test]
    fn index_file_runs_land_in_the_view_workers_cache() -> Result<()> {
        let run = append_one_file_run_to_the_index()?;
        assert!(run.same_cache, "the index thread has another cache");
        assert!(run.blocks > 0, "the file run has no block");
        assert_eq!(
            run.blocks_in_view_cache, run.blocks,
            "a block of the file run is missing from the view worker's cache"
        );
        let (used, capacity) = run.occupancy;
        assert!(used < capacity, "the cache is full: {used} of {capacity}");
        Ok(())
    }

    /// The control of the test above, and the check that production relies on: a `ManualIndex`
    /// thread that is not on the runtime's buffer cache is refused at start, so that no run can
    /// write the index through another cache. The unbound thread is test-only.
    #[test]
    fn manual_index_refuses_a_thread_off_the_runtimes_cache() -> Result<()> {
        let pipeline = Pipeline::with_workers(1)?;
        let refusal = match ManualIndex::without_cache_binding(pipeline.runtime()) {
            Ok(_) => bail!("an index thread off the runtime's cache was accepted"),
            Err(error) => error,
        };
        assert_eq!(
            refusal.to_string(),
            "merged-index thread is not on the runtime's buffer cache"
        );
        Ok(())
    }
}

#[cfg(test)]
mod measurement_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_RUN: AtomicUsize = AtomicUsize::new(0);

    #[test]
    fn index_upkeep_is_a_part_of_the_base_phase_and_not_of_ivm() {
        let stats = Measurements {
            base_circuit: Duration::from_millis(4),
            index: Duration::from_millis(1),
            forward: Duration::from_millis(7),
            delta: Duration::from_millis(3),
            view_circuit: Duration::from_millis(2),
            ..Measurements::default()
        };
        let phases = phase_json(&stats);
        let phase = |name: &str| phases[name].as_f64().unwrap();
        // base_ms = 4 + 1, with index_ms reported as its sub-phase and not added again.
        assert!((phase("base_ms") - 5.0).abs() < 1e-9);
        assert!((phase("index_ms") - 1.0).abs() < 1e-9);
        assert!((phase("q3_ms") - 2.0).abs() < 1e-9);
        // ivm_ms = delta_ms + q3_ms, and forward_ms is in neither.
        assert!((phase("delta_ms") - 3.0).abs() < 1e-9);
        assert!((phase("ivm_ms") - 5.0).abs() < 1e-9);
        assert!((phase("forward_ms") - 7.0).abs() < 1e-9);
        assert!(phases.get("combined_maintenance_ms").is_none());
    }

    fn directory() -> Result<PathBuf> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/r5-tests")
            .join(format!(
                "{}-{}-{}",
                std::process::id(),
                cfg!(feature = "merged-index"),
                NEXT_RUN.fetch_add(1, Ordering::Relaxed)
            ));
        std::fs::create_dir_all(&path)?;
        Ok(path)
    }

    #[test]
    fn r5_rejects_unconfigured_storage_and_invalid_budgets() {
        for args in [
            vec!["--r5"],
            vec!["--cache-bytes", "1"],
            vec!["--compaction-timeout-secs", "NaN"],
            vec!["--memory-target-bytes", "0"],
            vec!["--refresh-list", "list.json", "--rf2", "delete.tbl"],
        ] {
            assert!(Config::from_args(args.into_iter().map(str::to_owned)).is_err());
        }
    }

    /// The load program writes an image, and the refresh program, started from a copy of it,
    /// reports the same refreshes as the single-process driver on the same inputs. A refresh
    /// program with other storage settings refuses the image.
    #[test]
    fn a_refresh_cut_into_transactions_reads_each_file_once_and_keeps_orders_whole() -> Result<()> {
        let directory = directory()?;
        let order = |key: i64| format!("{key}|1|O|1.00|1996-02-29|5-LOW|Clerk|0|c|\n");
        let line = |key: i64, number: i64| {
            format!(
                "{key}|2|3|{number}|5|1.00|0.04|0.02|N|O|1996-03-13|1996-02-12|1996-03-22|i|m|c|\n"
            )
        };
        std::fs::write(
            directory.join("orders.tbl"),
            [101, 102, 103].map(order).concat(),
        )?;
        std::fs::write(
            directory.join("lines.tbl"),
            [line(101, 1), line(102, 1), line(102, 2), line(103, 1)].concat(),
        )?;
        std::fs::write(directory.join("delete.tbl"), "7|\n8|\n9|\n")?;
        let refresh = |counts: Option<Vec<usize>>| Refresh {
            rf1_orders: Some(directory.join("orders.tbl")),
            rf1_lines: Some(directory.join("lines.tbl")),
            rf2: None,
            transaction_orders: counts,
        };
        let sizes = |prepared: Vec<PreparedRefresh>| -> Vec<(Vec<i64>, Vec<i64>)> {
            prepared
                .into_iter()
                .map(|transaction| match transaction {
                    PreparedRefresh::Rf1(change) => (
                        change.orders.iter().map(|(order, _)| order.key).collect(),
                        change
                            .lines
                            .iter()
                            .map(|(line, _)| line.order_key)
                            .collect(),
                    ),
                    PreparedRefresh::Rf2(_) => panic!("an RF1 refresh gave an RF2 transaction"),
                })
                .collect()
        };
        assert_eq!(
            sizes(PreparedRefresh::read(&refresh(Some(vec![2, 1])))?),
            vec![
                (vec![101, 102], vec![101, 102, 102]),
                (vec![103], vec![103])
            ]
        );
        assert_eq!(
            sizes(PreparedRefresh::read(&refresh(None))?),
            vec![(vec![101, 102, 103], vec![101, 102, 102, 103])]
        );
        for counts in [vec![2], vec![2, 2], vec![3, 0], vec![]] {
            let error = PreparedRefresh::read(&refresh(Some(counts.clone())))
                .err()
                .with_context(|| format!("{counts:?} was accepted"))?;
            assert!(error.to_string().contains("transaction_orders"), "{error}");
        }
        // A Lineitem whose Order is not in the cut refresh belongs to no transaction.
        std::fs::write(
            directory.join("lines.tbl"),
            [line(101, 1), line(104, 1)].concat(),
        )?;
        let error = PreparedRefresh::read(&refresh(Some(vec![3])))
            .err()
            .context("an orphan Lineitem was accepted")?;
        assert!(
            error.to_string().contains("belongs to no transaction"),
            "{error}"
        );
        let rf2 = Refresh {
            rf1_orders: None,
            rf1_lines: None,
            rf2: Some(directory.join("delete.tbl")),
            transaction_orders: Some(vec![1, 2]),
        };
        let keys: Vec<Vec<i64>> = PreparedRefresh::read(&rf2)?
            .into_iter()
            .map(|transaction| match transaction {
                PreparedRefresh::Rf2(keys) => {
                    let mut keys: Vec<i64> = keys.into_iter().collect();
                    keys.sort();
                    keys
                }
                PreparedRefresh::Rf1(_) => panic!("an RF2 refresh gave an RF1 transaction"),
            })
            .collect();
        assert_eq!(keys, vec![vec![7], vec![8, 9]]);
        Ok(())
    }

    #[test]
    fn a_refresh_cut_into_transactions_is_verified_after_each_one_and_timed_per_transaction(
    ) -> Result<()> {
        let directory = directory()?;
        let customers = directory.join("customer.tbl");
        let orders = directory.join("orders.tbl");
        let lines = directory.join("lineitem.tbl");
        std::fs::write(
            &customers,
            "1|n|a|1|p|1.00|BUILDING|comment|\n2|n|a|1|p|1.00|BUILDING|comment|\n",
        )?;
        let order = |key: i64, customer: i64| {
            format!("{key}|{customer}|O|1.00|1996-02-29|5-LOW|Clerk|0|c|\n")
        };
        let line = |key: i64, number: i64| {
            format!(
                "{key}|2|3|{number}|5|1.00|0.04|0.02|N|O|1996-03-13|1996-02-12|1996-03-22|i|m|c|\n"
            )
        };
        let keys: Vec<i64> = (1..=20).collect();
        std::fs::write(
            &orders,
            keys.iter()
                .map(|&key| order(key, 1 + key % 2))
                .collect::<String>(),
        )?;
        std::fs::write(
            &lines,
            keys.iter().map(|&key| line(key, 1)).collect::<String>(),
        )?;
        std::fs::write(
            directory.join("insert-orders.tbl"),
            (101..=105)
                .map(|key| order(key, 1 + key % 2))
                .collect::<String>(),
        )?;
        std::fs::write(
            directory.join("insert-lines.tbl"),
            (101..=105)
                .flat_map(|key| (1..=1 + key % 2).map(move |number| line(key, number)))
                .collect::<String>(),
        )?;
        std::fs::write(directory.join("delete.tbl"), "2|\n3|\n4|\n5|\n")?;
        let list = directory.join("refreshes.json");
        std::fs::write(
            &list,
            serde_json::to_vec(&json!([
                {"rf1_orders": "insert-orders.tbl", "rf1_lines": "insert-lines.tbl",
                 "transaction_orders": [2, 1, 2]},
                {"rf2": "delete.tbl", "transaction_orders": [3, 1]}
            ]))?,
        )?;
        let report = directory.join("report.json");
        // Independent validation checks V's delta against the reference after every transaction.
        run(
            &Config {
                customer: Some(customers),
                orders: Some(orders),
                lineitem: Some(lines),
                refresh_list: Some(list),
                report: Some(report.clone()),
                storage_path: Some(directory.join("storage")),
                workers: 1,
                runs: 1,
                batch: 8,
                cache_bytes: Some(8 * 1024 * 1024),
                compaction_timeout_secs: 30.0,
                verify: true,
                ..Config::default()
            },
            1,
        )?;
        let output: Value = serde_json::from_reader(File::open(&report)?)?;
        let refreshes = output["refreshes"].as_array().context("refreshes")?;
        for (refresh, phase, transactions) in [(&refreshes[0], "rf1", 3), (&refreshes[1], "rf2", 2)]
        {
            assert_eq!(refresh["phase"], phase);
            assert_eq!(refresh["transactions"], transactions);
            let latency = &refresh["transaction_latency"];
            assert_eq!(latency["transactions"], transactions);
            for field in ["total_ms", "base_ms", "ivm_ms"] {
                let values = latency[field]["values"].as_array().context("values")?;
                assert_eq!(values.len(), transactions, "{field}");
                let max = latency[field]["max"].as_f64().context("max")?;
                assert!(
                    values.iter().all(|value| value.as_f64().unwrap() <= max),
                    "{field}"
                );
                assert!(latency[field]["p50"].as_f64().unwrap() <= max, "{field}");
            }
        }
        assert_eq!(refreshes[0]["affected_orders"], 5);
        assert_eq!(refreshes[1]["affected_orders"], 4);
        assert_eq!(refreshes[1]["absent_orders"], 0);
        Ok(())
    }

    #[test]
    fn a_refresh_program_from_an_image_matches_the_single_process_driver() -> Result<()> {
        let directory = directory()?;
        let customers = directory.join("customer.tbl");
        let orders = directory.join("orders.tbl");
        let lines = directory.join("lineitem.tbl");
        std::fs::write(&customers, "1|n|a|1|p|1.00|BUILDING|comment|\n")?;
        let keys: Vec<i64> = (1..=40).collect();
        std::fs::write(
            &orders,
            keys.iter()
                .map(|key| format!("{key}|1|O|1.00|1996-02-29|5-LOW|Clerk|0|c|\n"))
                .collect::<String>(),
        )?;
        std::fs::write(&lines, keys.iter().map(|key| {
            format!("{key}|2|3|1|5|1.00|0.04|0.02|N|O|1996-03-13|1996-02-12|1996-03-22|i|m|c|\n")
        }).collect::<String>())?;
        std::fs::write(
            directory.join("insert-orders.tbl"),
            "50|1|O|1.00|1996-02-29|5-LOW|Clerk|0|c|\n",
        )?;
        std::fs::write(
            directory.join("insert-lines.tbl"),
            "50|2|3|1|5|1.00|0.04|0.02|N|O|1996-03-13|1996-02-12|1996-03-22|i|m|c|\n",
        )?;
        std::fs::write(directory.join("delete.tbl"), "3|\n50|\n")?;
        let list = directory.join("refreshes.json");
        std::fs::write(
            &list,
            serde_json::to_vec(&json!([
                {"rf1_orders": "insert-orders.tbl", "rf1_lines": "insert-lines.tbl"},
                {"rf2": "delete.tbl"}
            ]))?,
        )?;
        let common = || Config {
            workers: 1,
            runs: 1,
            batch: 8,
            cache_bytes: Some(8 * 1024 * 1024),
            compaction_timeout_secs: 30.0,
            verify: false,
            ..Config::default()
        };
        let refresh_rows = |report: &Path| -> Result<Vec<(Value, Value)>> {
            let output: Value = serde_json::from_reader(File::open(report)?)?;
            Ok(output["refreshes"]
                .as_array()
                .context("refreshes")?
                .iter()
                .map(|refresh| (refresh["phase"].clone(), refresh["absent_orders"].clone()))
                .collect())
        };

        // The single-process driver.
        let single_report = directory.join("single.json");
        run(
            &Config {
                customer: Some(customers.clone()),
                orders: Some(orders.clone()),
                lineitem: Some(lines.clone()),
                refresh_list: Some(list.clone()),
                report: Some(single_report.clone()),
                storage_path: Some(directory.join("single-storage")),
                ..common()
            },
            1,
        )?;

        // The load program, then the refresh program on a copy of its image.
        let image = directory.join("image");
        run(
            &Config {
                customer: Some(customers),
                orders: Some(orders),
                lineitem: Some(lines),
                write_image: Some(image.clone()),
                ..common()
            },
            1,
        )?;
        let manifest = ImageManifest::read(&image)?;
        assert_eq!(manifest.settings.mode, mode_name());
        assert_eq!(manifest.load["rows"], 81);
        let copy = directory.join("image-copy");
        let copied = Command::new("cp")
            .arg("-a")
            .arg(&image)
            .arg(&copy)
            .status()?;
        assert!(copied.success());
        let image_report = directory.join("from-image.json");
        let from_image = || Config {
            refresh_list: Some(list.clone()),
            report: Some(image_report.clone()),
            from_image: Some(copy.clone()),
            ..common()
        };
        run(&from_image(), 1)?;
        assert_eq!(refresh_rows(&image_report)?, refresh_rows(&single_report)?);
        let output: Value = serde_json::from_reader(File::open(&image_report)?)?;
        assert!(output["load"].is_null());
        assert_eq!(output["image"]["settings"]["mode"], mode_name());
        assert_eq!(output["measurement_version"], 22);
        // The refreshes evicted the base tables' blocks after each base transaction.
        assert_eq!(
            output["configuration"]["evict_base_after_transaction"],
            true
        );
        let evicted: u64 = output["refreshes"]
            .as_array()
            .context("refreshes")?
            .iter()
            .map(|refresh| refresh["base_evicted_bytes"].as_u64().unwrap_or(0))
            .sum();
        assert!(evicted > 0, "no base-table block was evicted");
        // The original image is untouched by the refreshes of its copy.
        assert_eq!(
            ImageManifest::read(&image)?.checkpoint.uuid,
            manifest.checkpoint.uuid
        );

        // An image of another format is refused when it is read.
        let other_format = directory.join("image-other-format");
        std::fs::create_dir(&other_format)?;
        let mut manifest_json: Value =
            serde_json::from_slice(&std::fs::read(image.join(ImageManifest::FILE))?)?;
        manifest_json["format"] = json!(ImageManifest::FORMAT + 1);
        std::fs::write(
            other_format.join(ImageManifest::FILE),
            serde_json::to_vec(&manifest_json)?,
        )?;
        let error = ImageManifest::read(&other_format).unwrap_err();
        assert!(
            error.to_string().contains("unsupported image format"),
            "{error}"
        );

        // Other storage settings than the image's are refused before anything runs.
        let error = run(
            &Config {
                front_code_byte_keys: true,
                ..from_image()
            },
            1,
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("the image was written with"),
            "{error}"
        );
        Ok(())
    }

    #[test]
    fn storage_backed_driver_preserves_runs_and_discovers_repeated_refreshes() -> Result<()> {
        let directory = directory()?;
        let customers = directory.join("customer.tbl");
        let orders = directory.join("orders.tbl");
        let lines = directory.join("lineitem.tbl");
        let inserted_orders = directory.join("insert-orders.tbl");
        let inserted_lines = directory.join("insert-lines.tbl");
        let deletes = directory.join("delete.tbl");
        let empty = directory.join("empty.tbl");
        let list = directory.join("refreshes.json");
        let report = directory.join("report.json");
        std::fs::write(&customers, "1|n|a|1|p|1.00|BUILDING|comment|\n")?;
        let base_keys: Vec<_> = std::iter::once(1).chain(1001..=1032).collect();
        std::fs::write(
            &orders,
            base_keys
                .iter()
                .map(|key| format!("{key}|1|O|1.00|1996-02-29|5-LOW|Clerk|0|c|\n"))
                .collect::<String>(),
        )?;
        std::fs::write(&lines, base_keys.iter().map(|key| {
            format!("{key}|2|3|1|5|1.00|0.04|0.02|N|O|1996-03-13|1996-02-12|1996-03-22|i|m|c|\n")
        }).collect::<String>())?;
        std::fs::write(&inserted_orders, "2|1|O|1.00|1996-02-29|5-LOW|Clerk|0|c|\n")?;
        std::fs::write(
            &inserted_lines,
            (1..=130)
                .map(|n| {
                    format!(
                        "2|2|3|{n}|5|1.00|0.04|0.02|N|O|1996-03-13|1996-02-12|1996-03-22|i|m|c|\n"
                    )
                })
                .collect::<String>(),
        )?;
        std::fs::write(&deletes, "2|\n2|\n999|\n")?;
        std::fs::write(&empty, "")?;
        std::fs::write(
            &list,
            serde_json::to_vec(&json!([
                {"rf1_orders": "insert-orders.tbl", "rf1_lines": "insert-lines.tbl"},
                {"rf2": "delete.tbl"}, {"rf2": "delete.tbl"},
                {"rf1_orders": "insert-orders.tbl", "rf1_lines": "insert-lines.tbl"},
                {"rf2": "delete.tbl"}, {"rf2": "empty.tbl"}
            ]))?,
        )?;
        let config = Config {
            customer: Some(customers),
            orders: Some(orders),
            lineitem: Some(lines),
            refresh_list: Some(list),
            report: Some(report.clone()),
            storage_path: Some(directory.join("storage")),
            workers: 1,
            runs: 1,
            batch: 2,
            cache_bytes: Some(8 * 1024 * 1024),
            // Far below the process's size: with default placement only memory pressure puts runs in
            // files at this tiny scale, and a target this low keeps the runtime under pressure.
            memory_target_bytes: Some(1024 * 1024),
            compaction_timeout_secs: 10.0,
            verify: true,
            r5: true,
            ..Config::default()
        };
        run(&config, 1)?;
        let output: Value = serde_json::from_reader(File::open(report)?)?;
        let refreshes = output["refreshes"].as_array().unwrap();
        assert_eq!(refreshes.len(), 6);
        assert_eq!(refreshes[0]["line_fanout_max"], 130);
        assert_eq!(refreshes[1]["absent_orders"], 1);
        assert_eq!(refreshes[2]["absent_orders"], 2);
        assert_eq!(refreshes[5]["rows"], 0);
        // Measurement version 4 books the merged index's upkeep in the base phase: `index_ms`
        // is a part of `base_ms`. Measurement version 11 books the transaction delta's
        // preparation in `delta_ms`, inside the IVM figure: `ivm_ms = delta_ms + q3_ms`.
        for refresh in refreshes {
            let phase = |name: &str| refresh[name].as_f64().unwrap();
            assert!(phase("base_ms") > 0.0, "{refresh}");
            assert!(phase("q3_ms") > 0.0, "{refresh}");
            assert!(phase("forward_ms") >= 0.0, "{refresh}");
            assert!(phase("index_ms") <= phase("base_ms"), "{refresh}");
            // Both modes time the transaction delta's preparation inside the IVM window.
            assert!(phase("delta_ms") >= 0.0, "{refresh}");
            assert!(phase("ivm_ms") >= phase("delta_ms"), "{refresh}");
            assert!(
                (phase("ivm_ms") - phase("delta_ms") - phase("q3_ms")).abs() < 1e-6,
                "{refresh}"
            );
            assert!(
                refresh.get("combined_maintenance_ms").is_none(),
                "{refresh}"
            );
            if !cfg!(feature = "merged-index") {
                assert_eq!(phase("index_ms"), 0.0);
            }
        }
        // One runtime, with one storage directory and the whole cache.
        let runtime = &output["configuration"]["runtime"];
        assert_eq!(runtime["cache_mib"].as_u64().unwrap(), 8);
        assert!(runtime["storage_path"].is_string());
        assert!(output["configuration"].get("circuits").is_none());
        assert!(output["sequence"].get("circuits").is_none());
        assert!(output["sequence"]["cache_capacity_bytes"].as_u64().unwrap() > 0);
        // The CPU profiler's per-circuit time is reported for every refresh.
        for refresh in refreshes {
            let cpu = &refresh["profiled_cpu_ms"];
            assert!(cpu["base"].as_f64().unwrap() >= 0.0, "{refresh}");
            assert!(cpu["q3"].as_f64().unwrap() >= 0.0, "{refresh}");
        }
        assert!(
            output["sequence"]["observed_storage_bytes"]
                .as_u64()
                .unwrap()
                > 0
        );
        assert_eq!(output["measurement_version"], 22);
        let session_pass = &output["configuration"]["session_pass"];
        if cfg!(feature = "merged-index") {
            assert_eq!(session_pass, "seek");
        } else {
            assert!(session_pass.is_null());
        }
        // Version 8 reports the merged index's run reads of the view phase as the scan
        // session's (`index_run_reads.session`, with the session's moves in
        // `index_session_counts`), and the reads of base maintenance apart
        // (`index_upkeep_run_reads`), and where the index's runs were.
        let key_reads = |reads: &Value| -> u64 {
            ["key_decodes", "memory_key_reads"]
                .iter()
                .map(|field| reads[field].as_u64().unwrap())
                .sum()
        };
        if cfg!(feature = "merged-index") {
            for refresh in refreshes {
                let layout = &refresh["index_layout"];
                for snapshot in ["before", "after"] {
                    for location in ["file", "memory"] {
                        assert!(layout[snapshot][location]["runs"].is_u64(), "{refresh}");
                        assert!(layout[snapshot][location]["bytes"].is_u64(), "{refresh}");
                    }
                }
                assert!(
                    refresh["index_upkeep_run_reads"]["seeks"].is_u64(),
                    "{refresh}"
                );
            }
            let per_refresh =
                |field: &dyn Fn(&Value) -> u64| -> u64 { refreshes.iter().map(field).sum() };
            let session_key_reads =
                per_refresh(&|refresh| key_reads(&refresh["index_run_reads"]["session"]));
            let groups =
                per_refresh(&|refresh| refresh["index_session_counts"]["groups"].as_u64().unwrap());
            // The refreshes that change the base touch groups, and the session reads keys.
            assert!(groups > 0);
            assert!(session_key_reads > 0);
            let sequence = &output["sequence"];
            assert_eq!(
                key_reads(&sequence["index_run_reads"]["session"]),
                session_key_reads
            );
            assert_eq!(
                sequence["index_session_counts"]["groups"].as_u64().unwrap(),
                groups
            );
            // The driver thread's reads while collecting, per owner: the index's, the routing's
            // under `requested_slice`, and none for any other owner.
            for refresh in refreshes {
                let driver = &refresh["driver_run_reads"];
                assert_eq!(
                    driver["merged_index"], refresh["index_run_reads"]["session"],
                    "{refresh}"
                );
                for owner in ["query", "base_relation", "partner_trace"] {
                    for field in ["seeks", "memory_key_reads", "key_decodes"] {
                        assert_eq!(driver[owner][field].as_u64().unwrap(), 0, "{refresh}");
                    }
                }
            }
        } else {
            for refresh in refreshes {
                assert!(refresh["index_run_reads"].is_null(), "{refresh}");
                assert!(refresh["index_session_counts"].is_null(), "{refresh}");
                assert!(refresh["driver_run_reads"].is_null(), "{refresh}");
                assert!(refresh["index_upkeep_run_reads"].is_null(), "{refresh}");
                assert!(refresh["index_layout"].is_null(), "{refresh}");
            }
            assert!(output["sequence"]["index_run_reads"].is_null());
        }
        // The view worker's reads, per owner, in both modes. Nothing the worker runs reads under
        // `merged_index`: the scan session reads the index on the driver's thread. The joins'
        // reads of the readers' batches are under `requested_slice` in reconstructed mode, and
        // none are in native mode.
        // `filtered_probes` counts exact probes that a file run's filters (key range, then
        // Roaring bitmap or Bloom filter) answer with no seek.
        let reads_fields = [
            "seeks",
            "filtered_probes",
            "filtered_prefix_probes",
            "key_decodes",
            "value_decodes",
            "weight_decodes",
            "memory_key_reads",
            "memory_value_reads",
            "memory_weight_reads",
        ];
        let owner_total = |view: &Value, owner: &str| -> u64 {
            reads_fields
                .iter()
                .map(|field| view[owner][field].as_u64().unwrap())
                .sum()
        };
        for refresh in refreshes {
            let view = &refresh["view_run_reads"];
            for owner in [
                "query",
                "base_relation",
                "merged_index",
                "requested_slice",
                "partner_trace",
            ] {
                assert!(view[owner]["seeks"].is_u64(), "{refresh}");
            }
            assert_eq!(owner_total(view, "merged_index"), 0, "{refresh}");
            // The partner side is the trace's in native mode and the slice's in reconstructed.
            let absent = if cfg!(feature = "merged-index") {
                "partner_trace"
            } else {
                "requested_slice"
            };
            assert_eq!(owner_total(view, absent), 0, "{refresh}");
        }
        // A refresh that changes nothing reads no partner, so each mode is checked as a whole.
        let partner_owner = if cfg!(feature = "merged-index") {
            "requested_slice"
        } else {
            "partner_trace"
        };
        let partner_reads: u64 = refreshes
            .iter()
            .map(|refresh| owner_total(&refresh["view_run_reads"], partner_owner))
            .sum();
        assert!(partner_reads > 0);
        let view_seeks: u64 = refreshes
            .iter()
            .map(|refresh| {
                refresh["view_run_reads"]["partner_trace"]["seeks"]
                    .as_u64()
                    .unwrap()
            })
            .sum();
        assert_eq!(
            output["sequence"]["view_run_reads"]["partner_trace"]["seeks"],
            view_seeks
        );
        if !cfg!(feature = "merged-index") {
            // The native joins probe their traces on the view worker.
            assert!(view_seeks > 0);
        }
        // Block expansions (version 12): the view worker's per owner in both modes, the driver
        // thread's and base maintenance's in the reconstructed mode only. The smoke run is
        // written in no disk encoding, so it expands nothing; the sequence sums the refreshes.
        let expansion_fields = [
            "prefix_truncated_blocks",
            "prefix_truncated_encoded_bytes",
            "prefix_truncated_expanded_bytes",
            "elided_blocks",
            "elided_encoded_bytes",
            "elided_expanded_bytes",
        ];
        let owners = [
            "query",
            "base_relation",
            "merged_index",
            "requested_slice",
            "partner_trace",
        ];
        for place in refreshes
            .iter()
            .map(|refresh| &refresh["block_expansions"])
            .chain([&output["sequence"]["block_expansions"]])
        {
            for owner in owners {
                for field in expansion_fields {
                    assert!(place["view_worker"][owner][field].is_u64(), "{place}");
                }
            }
            if cfg!(feature = "merged-index") {
                for owner in owners {
                    for field in expansion_fields {
                        assert!(place["driver"][owner][field].is_u64(), "{place}");
                    }
                }
                for field in expansion_fields {
                    assert_eq!(place["index_upkeep"][field], 0, "{place}");
                }
            } else {
                assert!(place["driver"].is_null(), "{place}");
                assert!(place["index_upkeep"].is_null(), "{place}");
            }
        }
        assert_eq!(
            output["sequence"]["completion_boundary"],
            "last_refresh_complete"
        );
        assert_eq!(output["sequence"]["snapshot_publication_transactions"], 0);
        assert!(output["sequence"]
            .get("final_compaction_drain_ms")
            .is_none());
        assert!(output["sequence"].get("settled_storage_bytes").is_none());
        assert!(output["sequence"].get("background_merge_wait_ms").is_none());
        // The runtime is under memory pressure, so the loaded runs are in files.
        assert!(
            output["load"]["catalog_storage"]["CUSTOMER"]["storage_batch_count"]
                .as_u64()
                .unwrap()
                > 0
        );
        let foreground = output["sequence"]["foreground_ms"].as_f64().unwrap();
        let total = output["sequence"]["end_to_end_ms"].as_f64().unwrap();
        assert!(total >= foreground);
        // The refresh files are parsed before the window, and each refresh's row counts are
        // reported.
        let inputs = output["refresh_inputs"]["per_refresh"].as_array().unwrap();
        assert_eq!(inputs.len(), refreshes.len());
        assert_eq!(inputs[0]["phase"], "rf1");
        assert!(inputs[0]["orders"].as_u64().unwrap() > 0);
        assert_eq!(inputs[1]["phase"], "rf2");
        assert!(inputs[1]["order_keys"].as_u64().unwrap() > 0);
        // The window figures are Linux readings: present there, `null` elsewhere. Pressure is
        // sampled for both runtimes around every refresh.
        let sequence = &output["sequence"];
        assert_eq!(output["os"], std::env::consts::OS);
        assert_eq!(
            sequence["window_peak_rss_bytes"].is_u64(),
            cfg!(target_os = "linux")
        );
        assert_eq!(
            sequence["window_end_rss_bytes"].is_u64(),
            cfg!(target_os = "linux")
        );
        assert_eq!(
            output["refresh_inputs"]["vm_rss_after_pre_read_bytes"].is_u64(),
            cfg!(target_os = "linux")
        );
        assert_eq!(
            output["inputs_evicted"]["evicted"],
            cfg!(target_os = "linux")
        );
        if !cfg!(target_os = "linux") {
            assert!(sequence["cgroup"].is_null());
        }
        for refresh in refreshes {
            // The charge is off in this run: no reservation was made, and none waited.
            let reservations = &refresh["compaction_reservations"];
            assert_eq!(reservations["waits"], 0, "{refresh}");
            assert_eq!(reservations["wait_ms"], 0.0, "{refresh}");
            assert_eq!(reservations["peak_pinned_bytes"], 0, "{refresh}");
            // Nor is there a pool, under rss: no permit wait, though the open merges are counted.
            let jobs = &refresh["compaction_jobs"];
            assert_eq!(jobs["waits"], 0, "{refresh}");
            assert_eq!(jobs["wait_ms"], 0.0, "{refresh}");
            assert!(jobs["peak_open_merges"].is_u64(), "{refresh}");
            for side in ["before", "after"] {
                let state = &refresh["memory_pressure"][side];
                assert!(state["runtime"].is_string(), "{refresh}");
                assert!(state["cache_occupancy_bytes"].is_u64(), "{refresh}");
                // This driver runs with the memtable off.
                assert!(state["memtable_bytes"].is_null(), "{refresh}");
            }
        }
        assert!(sequence["cache_occupancy_at_window_start_bytes"]
            .as_u64()
            .is_some());
        // Verification I/O and CPU counters are omitted from performance data.
        assert!(output["sequence"]["resources"]["unavailable"].is_string());
        // The other pass collects the same groups and decodes the same payloads; only how it
        // passes the untouched groups differs, and verification checks V under it too.
        #[cfg(feature = "merged-index")]
        let config = {
            let config = Config {
                session_pass: Some(Pass::Step),
                storage_path: Some(directory.join("step-storage")),
                report: Some(directory.join("step-report.json")),
                ..config
            };
            run(&config, 1)?;
            let stepped: Value =
                serde_json::from_reader(File::open(config.report.as_ref().unwrap())?)?;
            assert_eq!(stepped["configuration"]["session_pass"], "step");
            for field in ["groups", "payload_decodes_old", "payload_decodes_dlt"] {
                assert_eq!(
                    stepped["sequence"]["index_session_counts"][field],
                    output["sequence"]["index_session_counts"][field],
                    "{field}"
                );
            }
            Config {
                session_pass: None,
                ..config
            }
        };
        let config = Config {
            verify: false,
            storage_path: Some(directory.join("unverified-storage")),
            report: Some(directory.join("unverified-report.json")),
            ..config
        };
        run(&config, 1)?;
        let measured: Value =
            serde_json::from_reader(File::open(config.report.as_ref().unwrap())?)?;
        assert_eq!(measured["configuration"]["driver_reference_bytes"], 0);
        assert!(measured["sequence"]["resources"]["backend_read_operations"].is_u64());
        assert!(measured["sequence"]["resources"]["cpu_microseconds"].is_u64());
        // Each file is attributed to one owner, so the owners add up to the totals; the native
        // program has no merged index, and so no merged-index writes.
        let owners = measured["sequence"]["resources"]["backend_write_by_owner"]
            .as_object()
            .unwrap();
        let owner_bytes: u64 = owners.values().map(|o| o["bytes"].as_u64().unwrap()).sum();
        assert_eq!(
            owner_bytes,
            measured["sequence"]["resources"]["backend_write_bytes"]
                .as_u64()
                .unwrap()
        );
        if !cfg!(feature = "merged-index") {
            assert_eq!(owners["merged_index"]["bytes"], 0);
            assert_eq!(owners["merged_index"]["operations"], 0);
        }
        assert!(
            measured["sequence"]["sampled_peak_storage_bytes"]
                .as_u64()
                .unwrap()
                >= measured["sequence"]["observed_storage_bytes"]
                    .as_u64()
                    .unwrap()
        );
        // A large final RF1 leaves a below-threshold RAM run alongside the
        // loaded file-backed state. Completion must not force it into one file.
        let single_list = directory.join("single-refresh.json");
        std::fs::write(
            &single_list,
            serde_json::to_vec(&json!([
                {"rf1_orders": "insert-orders.tbl", "rf1_lines": "insert-lines.tbl"}
            ]))?,
        )?;
        let config = Config {
            refresh_list: Some(single_list),
            storage_path: Some(directory.join("single-refresh-storage")),
            report: Some(directory.join("single-refresh-report.json")),
            ..config
        };
        run(&config, 1)?;
        let single: Value = serde_json::from_reader(File::open(config.report.as_ref().unwrap())?)?;
        let lines = &single["sequence"]["catalog_storage"]["LINEITEM"];
        // Default placement under memory pressure puts the runs in files. Whether a run that
        // arrived before the first pressure sample stays in memory is a race, so the test pins
        // only that files exist.
        assert!(lines["storage_batch_count"].as_u64().unwrap() > 0);
        #[cfg(feature = "merged-index")]
        assert!(
            single["sequence"]["manual_index_storage"]["storage_batch_count"]
                .as_u64()
                .unwrap()
                > 0
        );
        let empty_list = directory.join("no-refreshes.json");
        std::fs::write(&empty_list, "[]")?;
        let config = Config {
            refresh_list: Some(empty_list),
            storage_path: Some(directory.join("no-refreshes-storage")),
            report: Some(directory.join("no-refreshes-report.json")),
            ..config
        };
        run(&config, 1)?;
        let empty: Value = serde_json::from_reader(File::open(config.report.unwrap())?)?;
        assert_eq!(empty["sequence"]["foreground_ms"], 0.0);
        assert_eq!(empty["sequence"]["end_to_end_ms"], 0.0);
        assert_eq!(empty["sequence"]["resources"]["cpu_microseconds"], 0);
        assert_eq!(empty["sequence"]["snapshot_publication_transactions"], 0);
        assert!(empty["load"]["setup_full_compaction"].is_null());

        // Setup full compaction leaves each relation in at most one run before
        // the refreshes, and reports its cost apart from the sequence.
        let config = Config {
            refresh_list: Some(directory.join("refreshes.json")),
            storage_path: Some(directory.join("setup-compaction-storage")),
            report: Some(directory.join("setup-compaction-report.json")),
            setup_full_compaction: true,
            ..config
        };
        run(&config, 1)?;
        let compacted: Value = serde_json::from_reader(File::open(config.report.unwrap())?)?;
        assert_eq!(compacted["configuration"]["setup_full_compaction"], true);
        for (name, relation) in compacted["load"]["catalog_storage"].as_object().unwrap() {
            let runs = relation["memory_batch_count"].as_u64().unwrap()
                + relation["storage_batch_count"].as_u64().unwrap();
            assert!(runs <= 1, "{name} has {runs} runs after setup compaction");
        }
        let setup = &compacted["load"]["setup_full_compaction"];
        assert!(setup["wait_for_compaction_ms"].as_f64().unwrap() >= 0.0);
        let resources = &setup["resources"];
        let owner_bytes: u64 = resources["backend_write_by_owner"]
            .as_object()
            .unwrap()
            .values()
            .map(|owner| owner["bytes"].as_u64().unwrap())
            .sum();
        assert_eq!(
            owner_bytes,
            resources["backend_write_bytes"].as_u64().unwrap()
        );
        assert_eq!(compacted["refreshes"].as_array().unwrap().len(), 6);

        // Front coding applies only to the merged index's byte-string keys,
        // and every eligible block has the layout the encoder understands.
        // The counters are process-wide, so only their signs are checked.
        let config = Config {
            storage_path: Some(directory.join("front-coded-storage")),
            report: Some(directory.join("front-coded-report.json")),
            front_code_byte_keys: true,
            ..config
        };
        run(&config, 1)?;
        let front_coded: Value = serde_json::from_reader(File::open(config.report.unwrap())?)?;
        assert_eq!(front_coded["configuration"]["front_code_byte_keys"], true);
        let coding = &front_coded["front_coding"];
        assert_eq!(coding["layout_fallbacks"], 0);
        if cfg!(feature = "merged-index") {
            assert!(coding["blocks"].as_u64().unwrap() > 0);
        }
        // Value-header elision is a separate switch: with only key front
        // coding on, no value block is elided.
        assert_eq!(front_coded["value_header_elision"]["blocks"], 0);

        // Elision applies to the merged index's byte-string values.
        let config = Config {
            storage_path: Some(directory.join("elided-storage")),
            report: Some(directory.join("elided-report.json")),
            front_code_byte_keys: false,
            elide_value_headers: true,
            ..config
        };
        run(&config, 1)?;
        let elided: Value = serde_json::from_reader(File::open(config.report.unwrap())?)?;
        assert_eq!(elided["configuration"]["elide_value_headers"], true);
        assert_eq!(elided["value_header_elision"]["layout_fallbacks"], 0);

        // The diagnostics scan finds each run's file in the storage directory of the circuit
        // that wrote it, whichever of the two that is.
        let config = Config {
            storage_path: Some(directory.join("diagnosed-storage")),
            report: Some(directory.join("diagnosed-report.json")),
            diagnostics: Some(directory.join("diagnostics")),
            elide_value_headers: false,
            ..config
        };
        run(&config, 1)?;
        let contents: Value = serde_json::from_reader(File::open(
            directory.join("diagnostics").join("run-contents.json"),
        )?)?;
        assert!(!contents["spines"].as_array().unwrap().is_empty());
        Ok(())
    }
}

/// The base circuit's net delta, which the view circuit and the merged index consume.
#[cfg(test)]
mod net_delta_tests {
    use super::*;

    fn customer(key: i64, segment: &str) -> Customer {
        Customer {
            key,
            segment: segment.to_owned(),
        }
    }

    fn order(key: i64, customer_key: i64) -> Order {
        Order {
            key,
            customer_key,
            date: "1996-01-02".to_owned(),
            ship_priority: 3,
        }
    }

    fn line(order_key: i64, number: i32) -> Line {
        Line {
            order_key,
            number,
            price_cents: 12345,
            discount_hundredths: 4,
            ship_date: "1996-03-13".to_owned(),
        }
    }

    /// Applies `change` to the base circuit alone and returns its net delta.
    fn base_delta(pipeline: &mut Pipeline, change: Change) -> Result<Change> {
        change.apply_optional(None, pipeline.base.inputs())?;
        pipeline.base.circuit.transaction()?;
        workload::net_delta(&pipeline.base.catalog)
    }

    #[test]
    fn net_delta_carries_complete_rows_for_inserts_and_deletes() -> Result<()> {
        let mut pipeline = Pipeline::with_workers(1)?;
        let inserted = Change {
            customers: vec![(customer(2, "BUILDING"), true)],
            orders: vec![(order(20, 2), true)],
            lines: vec![(line(20, 1), true), (line(20, 2), true)],
        };
        let delta = base_delta(&mut pipeline, inserted.clone())?;
        assert_eq!(delta.customers, inserted.customers);
        assert_eq!(delta.orders, inserted.orders);
        assert_eq!(delta.lines, inserted.lines);

        // The driver deletes by key, but the delta carries the complete rows that went away.
        let deleted = Change {
            orders: vec![(order(20, 2), false)],
            lines: vec![(line(20, 1), false), (line(20, 2), false)],
            ..Change::default()
        };
        let delta = base_delta(&mut pipeline, deleted.clone())?;
        assert_eq!(delta.orders, deleted.orders);
        assert_eq!(delta.lines, deleted.lines);
        assert!(delta.customers.is_empty());

        // Reading drains the delta streams.
        assert_eq!(workload::net_delta(&pipeline.base.catalog)?.len(), 0);
        Ok(())
    }

    #[test]
    fn net_delta_reports_a_replaced_row_as_a_deletion_and_an_insertion() -> Result<()> {
        let mut pipeline = Pipeline::with_workers(1)?;
        base_delta(
            &mut pipeline,
            Change {
                customers: vec![(customer(2, "BUILDING"), true)],
                ..Change::default()
            },
        )?;
        let delta = base_delta(
            &mut pipeline,
            Change {
                customers: vec![(customer(2, "AUTOMOBILE"), true)],
                ..Change::default()
            },
        )?;
        assert_eq!(delta.customers.len(), 2);
        assert!(delta.customers.contains(&(customer(2, "BUILDING"), false)));
        assert!(delta.customers.contains(&(customer(2, "AUTOMOBILE"), true)));
        Ok(())
    }

    #[test]
    fn repeating_an_upsert_leaves_no_net_delta() -> Result<()> {
        let mut pipeline = Pipeline::with_workers(1)?;
        let row = Change {
            customers: vec![(customer(2, "BUILDING"), true)],
            ..Change::default()
        };
        base_delta(&mut pipeline, row.clone())?;
        // The same upsert changes nothing, so nothing is forwarded.
        assert_eq!(base_delta(&mut pipeline, row)?.len(), 0);
        Ok(())
    }

    #[test]
    fn view_inputs_delete_by_whole_row_and_base_inputs_by_key() -> Result<()> {
        let mut pipeline = Pipeline::with_workers(1)?;
        assert_eq!(pipeline.base.inputs().deletes, DeleteStyle::ByKey);
        assert!(!pipeline.base.inputs().whole_row(false));
        assert!(pipeline.base.inputs().whole_row(true));
        // The reconstructed build's view circuit takes whole rows from `Dlt`, not JSON.
        if cfg!(feature = "merged-index") {
            assert!(pipeline.view.inputs.is_none());
        } else {
            assert_eq!(pipeline.view.inputs().deletes, DeleteStyle::WholeRow);
            assert!(pipeline.view.inputs().whole_row(false));
        }
        Ok(())
    }
}

/// What the two worked-transaction tests share: the rows, and the two arms with one config and
/// one memory-pressure mechanism for both sides of the comparison (user decision of
/// 2026-11-10). `native_worked_transaction_tests` (native build) and
/// `session_worked_transaction_tests` (`merged-index` build) both start from `start`, so their
/// configs cannot drift apart.
#[cfg(test)]
mod worked_transaction {
    use super::*;
    use feldera_types::memory_pressure::MemoryPressure;

    pub(super) fn customer(key: i64, segment: &str) -> Customer {
        Customer {
            key,
            segment: segment.to_owned(),
        }
    }

    pub(super) fn order(key: i64, customer_key: i64) -> Order {
        Order {
            key,
            customer_key,
            date: "1996-01-02".to_owned(),
            ship_priority: 0,
        }
    }

    pub(super) fn line(order_key: i64, number: i32) -> Line {
        Line {
            order_key,
            number,
            price_cents: 100,
            discount_hundredths: 4,
            ship_date: "1996-03-13".to_owned(),
        }
    }

    /// The old state: c5 with orders o3 and o4 (lines l7 and l8 under o3, l9 under o4), and
    /// customers c6 and c7.
    pub(super) fn load_change() -> Change {
        Change {
            customers: vec![
                (customer(5, "BUILDING"), true),
                (customer(6, "BUILDING"), true),
                (customer(7, "BUILDING"), true),
            ],
            orders: vec![(order(3, 5), true), (order(4, 5), true)],
            lines: vec![(line(3, 7), true), (line(3, 8), true), (line(4, 9), true)],
        }
    }

    /// The transaction: c5 replaced, and o5 added with l10 and l11.
    pub(super) fn worked_change() -> Change {
        Change {
            customers: vec![
                (customer(5, "BUILDING"), false),
                (customer(5, "AUTOMOBILE"), true),
            ],
            orders: vec![(order(5, 5), true)],
            lines: vec![(line(5, 10), true), (line(5, 11), true)],
        }
    }

    /// The memory-pressure configuration of a run. Nothing relieves the pressure by hand, and
    /// the runs land wherever the runtime puts them.
    #[derive(Clone, Copy, Debug)]
    pub(super) enum Arm {
        /// No memory target: the pressure stays low.
        Default,
        /// A target far below the process's size, which keeps the runtime under pressure for
        /// good (it is sampled, so the level is reported, not assumed).
        Pressured,
    }

    impl Arm {
        pub(super) fn name(self) -> &'static str {
            match self {
                Arm::Default => "default",
                Arm::Pressured => "pressured",
            }
        }

        /// The one config of both sides.
        pub(super) fn config(self) -> Config {
            Config {
                workers: 1,
                cache_bytes: Some(64 * 1024 * 1024),
                memory_target_bytes: matches!(self, Arm::Pressured).then_some(1024 * 1024),
                ..Config::default()
            }
        }
    }

    /// The pipeline of `arm` with storage in a fresh directory, which is returned. Under
    /// `Pressured` it waits until the runtime reports high pressure or more.
    pub(super) fn start(arm: Arm) -> Result<(Pipeline, PathBuf)> {
        let storage = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/worked-transaction")
            .join(format!(
                "{}-{}-{}",
                std::process::id(),
                if cfg!(feature = "merged-index") {
                    "reconstructed"
                } else {
                    "native"
                },
                arm.name()
            ));
        let pipeline = Pipeline::new(runtime_config(&arm.config(), Some(&storage))?, false)?;
        pipeline.enable_contract_check();
        if matches!(arm, Arm::Pressured) {
            measurement::drain_until(Duration::from_secs(30), || {
                Ok(pipeline.runtime().current_memory_pressure() >= MemoryPressure::High)
            })
            .context("the runtime did not come under memory pressure")?;
        }
        Ok((pipeline, storage))
    }

    /// The files under `path`.
    pub(super) fn files_under(path: &Path) -> Result<usize> {
        let mut files = 0;
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            files += if entry.file_type()?.is_dir() {
                files_under(&entry.path())?
            } else {
                1
            };
        }
        Ok(files)
    }

    /// The memory pressure at the points of a run.
    #[derive(Clone, Copy, Debug)]
    pub(super) struct Pressures {
        pub(super) at_start: MemoryPressure,
        pub(super) after_load: MemoryPressure,
        pub(super) after_transaction: MemoryPressure,
    }

    /// The files in the storage directory at the points of a run. They show whether the runs
    /// are in files.
    #[derive(Clone, Copy, Debug)]
    pub(super) struct FileCounts {
        pub(super) at_start: usize,
        pub(super) after_load: usize,
        pub(super) after_transaction: usize,
    }

    impl std::fmt::Display for Pressures {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(
                f,
                "{:?} at start, {:?} after the load, {:?} after the transaction",
                self.at_start, self.after_load, self.after_transaction
            )
        }
    }

    impl std::fmt::Display for FileCounts {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(
                f,
                "{} at start, {} after the load, {} after the transaction",
                self.at_start, self.after_load, self.after_transaction
            )
        }
    }

    /// Loads the old state through `transact`, then runs `worked`, the worked transaction, and
    /// returns what it returns with what the run observed around it. The two sides run the
    /// transaction differently (see their modules), but share the config, the load, and the
    /// observations.
    pub(super) fn run_load_and_transaction<T>(
        pipeline: &mut Pipeline,
        storage: &Path,
        worked: impl FnOnce(&mut Pipeline, &mut Reference) -> Result<T>,
    ) -> Result<(T, Pressures, FileCounts)> {
        let pressure = |pipeline: &Pipeline| pipeline.runtime().current_memory_pressure();
        let at_start = (pressure(pipeline), files_under(storage)?);
        let mut state = Reference::default();
        transact(
            pipeline,
            &mut state,
            load_change(),
            true,
            &mut Measurements::default(),
        )?;
        let after_load = (pressure(pipeline), files_under(storage)?);
        let result = worked(pipeline, &mut state)?;
        let after_transaction = (pressure(pipeline), files_under(storage)?);
        Ok((
            result,
            Pressures {
                at_start: at_start.0,
                after_load: after_load.0,
                after_transaction: after_transaction.0,
            },
            FileCounts {
                at_start: at_start.1,
                after_load: after_load.1,
                after_transaction: after_transaction.1,
            },
        ))
    }

    /// A read count's memory reads and decodes together: what the cursors read, wherever the
    /// runs are.
    #[derive(Debug, PartialEq, Eq)]
    pub(super) struct Totals {
        pub(super) probes: u64,
        pub(super) keys: u64,
        pub(super) values: u64,
        pub(super) weights: u64,
    }

    impl Totals {
        pub(super) fn of(reads: &RunReads) -> Self {
            Self {
                // A probe that a run's filters answer is a probe all the same.
                probes: reads.seeks + reads.filtered_probes,
                keys: reads.memory_key_reads + reads.key_decodes,
                values: reads.memory_value_reads + reads.value_decodes,
                weights: reads.memory_weight_reads + reads.weight_decodes,
            }
        }
    }
}

/// The worked transaction of `docs/design/merged-index/shared-traversal.md` in native mode: the
/// view worker's run reads, to compare with the scan session's on the same rows
/// (`session_worked_transaction_tests`). It runs in the native build, since the view circuit of
/// the `merged-index` build reads the index.
///
/// Both arms of `worked_transaction::Arm` pin what the cursors read in all, wherever the runtime
/// put the runs. Each test prints the pressure level, the files, and where the reads went
/// (memory reads or decodes), so a run's layout can be read from the output (`--nocapture`).
#[cfg(all(test, not(feature = "merged-index")))]
mod native_worked_transaction_tests {
    use super::worked_transaction::*;
    use super::*;

    /// What one operator of the view circuit reads, derived per operator by a scout with no code
    /// run, and confirmed by the pins below. `probes` are seeks, or filtered probes when a
    /// file run's filter answers the probe.
    struct OperatorReads {
        probes: u64,
        keys: u64,
        values: u64,
        weights: u64,
    }

    /// The view circuit is two incremental joins, `J = C ⋈ O` (keyed by custkey) and
    /// `V = L ⋈ J` (keyed by orderkey), each `Δa ⋈ b' + a ⋈ Δb` over the integrated traces. The
    /// "after" trace of each pair is the old run plus the transaction's delta run (two runs); the
    /// "before" trace is the old run alone. The transaction is `ΔC = {-c5 BUILDING, +c5
    /// AUTOMOBILE}`, `ΔO = {o5}`, `ΔL = {l10, l11}`. Old J holds c5's orders o3 and o4, so `ΔJ`
    /// has entries of o3 and o4 (the retracted and the inserted c5, which cancel) and of o5.
    ///
    /// Values and weights exceed keys for four reasons: each delta value makes a full pass over
    /// the trace's values and then rewinds; `CursorList` reads a weight at each value landing,
    /// for its zero-weight skip; a seek that stays on the same key counts that key's value
    /// again; and J's `plus` reads every output row.
    ///
    /// Rows of the table, in the order of the circuit:
    ///
    /// | Operator | Probes | Key | Value | Weight |
    /// | --- | --- | --- | --- | --- |
    /// | `ΔO ⋈ C` after the insert | 2 | 3 | 8 | 6 |
    /// | `ΔC ⋈ O` before the insert | 1 | 2 | 8 | 8 |
    /// | J's `plus`, merging the two join outputs | 0 | 3 | 5 | 5 |
    /// | `ΔL ⋈ J` after the insert | 2 | 4 | 7 | 6 |
    /// | `ΔJ ⋈ L` before the insert (swapped: L drives and ΔJ is sought) | 2 | 4 | 13 | 13 |
    ///
    /// J's `plus`, and the sought `ΔJ`, compute the view change from the circuit's own
    /// intermediate results; the rest read the old state and the transaction's delta.
    const OPERATOR_TALLY: [OperatorReads; 5] = [
        OperatorReads {
            probes: 2,
            keys: 3,
            values: 8,
            weights: 6,
        },
        OperatorReads {
            probes: 1,
            keys: 2,
            values: 8,
            weights: 8,
        },
        OperatorReads {
            probes: 0,
            keys: 3,
            values: 5,
            weights: 5,
        },
        OperatorReads {
            probes: 2,
            keys: 4,
            values: 7,
            weights: 6,
        },
        OperatorReads {
            probes: 2,
            keys: 4,
            values: 13,
            weights: 13,
        },
    ];

    /// The tally's sums: what native reads in all.
    fn tally_totals() -> Totals {
        OPERATOR_TALLY.iter().fold(
            Totals {
                probes: 0,
                keys: 0,
                values: 0,
                weights: 0,
            },
            |sum, operator| Totals {
                probes: sum.probes + operator.probes,
                keys: sum.keys + operator.keys,
                values: sum.values + operator.values,
                weights: sum.weights + operator.weights,
            },
        )
    }

    /// Runs `arm`, checks that the cursors read the tally's totals, and prints what the run
    /// observed. Returns the reads and the observations.
    fn run_arm(arm: Arm) -> Result<(RunReads, Pressures, FileCounts)> {
        let (mut pipeline, storage) = start(arm)?;
        let (stats, pressures, files) =
            run_load_and_transaction(&mut pipeline, &storage, |pipeline, state| {
                let mut stats = Measurements::default();
                transact(pipeline, state, worked_change(), true, &mut stats)?;
                Ok(stats)
            })?;
        let [query, base_relation, merged_index, requested_slice, partner_trace] = stats.view_reads;
        // The DBSP operators' reads: the joins' probes of their traces (`partner_trace`) and
        // everything else they read (`query`), the deltas among it.
        let mut operators = query;
        operators += partner_trace;
        println!(
            "native {}: pressure {pressures}; files {files}\n  query {query:?}\n  partner trace {partner_trace:?}",
            arm.name()
        );
        assert_eq!(Totals::of(&operators), tally_totals());
        // The partner traces are read, and apart from the deltas.
        assert!(Totals::of(&partner_trace).keys > 0);
        assert!(Totals::of(&query).keys > 0);
        assert_eq!(base_relation, RunReads::default());
        assert_eq!(merged_index, RunReads::default());
        assert_eq!(requested_slice, RunReads::default());
        Ok((operators, pressures, files))
    }

    #[test]
    fn the_tally_adds_up_to_the_pinned_totals() {
        assert_eq!(
            tally_totals(),
            Totals {
                probes: 7,
                keys: 16,
                values: 41,
                weights: 38
            }
        );
    }

    /// Default placement at low memory pressure: every run is in memory, so the reads are
    /// memory reads and nothing is decoded. The seven probes are seeks; the tally's sums are 7
    /// seeks and 16 key, 41 value, and 38 weight reads.
    #[test]
    fn worked_transaction_default_arm() -> Result<()> {
        let (reads, _pressures, files) = run_arm(Arm::Default)?;
        assert_eq!(files.after_transaction, files.at_start);
        assert_eq!(
            reads,
            RunReads {
                seeks: 7,
                memory_key_reads: 16,
                memory_value_reads: 41,
                memory_weight_reads: 38,
                ..RunReads::default()
            }
        );
        Ok(())
    }

    /// A 1 MiB target, with the wait for high pressure. The totals are the tally's, as in the
    /// default arm, because the cursors make the same probes and passes. What differs is where
    /// the reads go, which the test prints and does not force: the runtime's pressure puts the
    /// runs wherever it does. When every run is in a file, the reads are decodes, and one of the
    /// seven probes is answered by a run's filter, which counts a filtered probe and no seek
    /// (6 seeks and 1 filtered probe); the test pins that split only then. The filtered probe is
    /// ΔL's o5 into J's old run, which has no entry of o5.
    #[test]
    fn worked_transaction_pressured_arm() -> Result<()> {
        let (reads, pressures, _files) = run_arm(Arm::Pressured)?;
        assert!(pressures.at_start >= feldera_types::memory_pressure::MemoryPressure::High);
        // The runs landed in files in every run observed (Critical pressure throughout). Then
        // the totals are all decodes, and the one filtered probe is not a seek. A mixed layout
        // would split the reads differently, and is reported, not forced.
        let memory_reads =
            reads.memory_key_reads + reads.memory_value_reads + reads.memory_weight_reads;
        if memory_reads == 0 {
            assert_eq!(
                reads,
                RunReads {
                    seeks: 6,
                    filtered_probes: 1,
                    key_decodes: 16,
                    value_decodes: 41,
                    weight_decodes: 38,
                    ..RunReads::default()
                }
            );
        }
        Ok(())
    }
}

/// The scan session on the worked transaction of `docs/design/merged-index/shared-traversal.md`,
/// under the same arms as `native_worked_transaction_tests`: the same rows, the same config
/// (`worked_transaction::Arm::config`), the same memory-pressure mechanism, and the same load
/// through `transact`. The index's runs are placed by that mechanism too: `Old` is
/// the index's snapshot before the transaction (`ManualIndex` keeps it in a `Spine`, whose
/// inserts consult `min_insert_storage_bytes`), and `Dlt` is built by `build_batch` with no
/// fixed location, so it follows `min_step_storage_bytes` like native's delta batches.
///
/// The transaction runs on the base circuit, the index, and now the view circuit too (`run_arm`):
/// the c5 replacement is a change outside RF1/RF2, which the hand-written circuit takes from
/// `Dlt` like any other. The session runs on the test thread, bound to the runtime's buffer
/// cache, as the driver runs it. The tests print the pressure level, the files, and the layout
/// of `Old` and `Dlt`, and do not assert a layout. They pin what the session does (its
/// `SessionCounts`), what the cursors read in all, wherever the runs are, and the split of those
/// reads between memory reads and decodes by the layout that the run observed. They also check
/// `V` against the independent join.
#[cfg(all(test, feature = "merged-index"))]
mod session_worked_transaction_tests {
    use super::worked_transaction::*;
    use super::*;
    use dbsp::trace::merged_index::SessionCounts;
    use dbsp::trace::{BatchLocation, BatchReader};

    /// A run's location and its number of entries.
    type RunShape = (BatchLocation, usize);

    struct Observed {
        pressures: Pressures,
        files: FileCounts,
        /// The nonempty runs of `Old`.
        old_runs: Vec<RunShape>,
        dlt: RunShape,
        counts: SessionCounts,
        /// The session's reads for the merged index, and for every other owner.
        reads: RunReads,
        other_reads: RunReads,
    }

    /// Runs the arm's transactions, then the capture of `Dlt` and the session over the last
    /// transaction's `Old`, and the view circuit on the driving deltas the owners built.
    fn run_arm(arm: Arm) -> Result<Observed> {
        let (mut pipeline, storage) = start(arm)?;
        let ((old_runs, dlt_shape, session, reads, other_reads), pressures, files) =
            run_load_and_transaction(&mut pipeline, &storage, |pipeline, state| {
                let before = state.evaluate();
                let net_delta = run_base(pipeline, state, worked_change(), true)?;
                publish_net_delta(pipeline, &net_delta, state)?;
                let old = pipeline.q3_state.old().map_err(anyhow::Error::msg)?;
                let dlt = pipeline.q3_state.dlt().map_err(anyhow::Error::msg)?;
                let old_runs: Vec<RunShape> = old
                    .batches()
                    .iter()
                    .filter(|run| !run.is_empty())
                    .map(|run| (run.location(), run.len()))
                    .collect();
                let dlt_shape = (dlt.location(), dlt.len());

                let index_before = RunReads::current_for(Component::MergedIndex);
                let total_before = RunReads::current();
                let collection = pipeline.collect_groups()?;
                let reads = RunReads::current_for(Component::MergedIndex).since(index_before);
                let other_reads = RunReads::current().since(total_before).since(reads);

                // The view circuit takes the same readers' driving deltas, and `V` must be the
                // independent join of the new state.
                pipeline.build_view_delta(collection.driving)?;
                pipeline.view.circuit.transaction()?;
                let session = pipeline.q3_state.finish().map_err(anyhow::Error::msg)?;
                let output = pipeline
                    .view
                    .catalog
                    .output_handles(&SqlIdentifier::from("V"))
                    .context("missing V output")?;
                let after = state.evaluate();
                assert_eq!(
                    pipeline.in_cache(|| read_output(&*output.delta_handle))?,
                    difference(&before, &after)
                );
                Ok((old_runs, dlt_shape, session, reads, other_reads))
            })?;
        assert_eq!(session.run_reads, reads);

        println!(
            "session {}: pressure {pressures}; files {files}\n  Old runs {old_runs:?}, Dlt {dlt_shape:?}\n  reads {reads:?}\n  counts {:?}",
            arm.name(),
            session.counts
        );
        Ok(Observed {
            pressures,
            files,
            old_runs,
            dlt: dlt_shape,
            counts: session.counts,
            reads,
            other_reads,
        })
    }

    /// `keys` key, `values` value, and `weights` weight reads of a run in `location`: decodes
    /// for a file, memory reads for a run in memory.
    fn run_reads(location: BatchLocation, keys: u64, values: u64, weights: u64) -> RunReads {
        match location {
            BatchLocation::Storage => RunReads {
                key_decodes: keys,
                value_decodes: values,
                weight_decodes: weights,
                ..RunReads::default()
            },
            BatchLocation::Memory => RunReads {
                memory_key_reads: keys,
                memory_value_reads: values,
                memory_weight_reads: weights,
                ..RunReads::default()
            },
        }
    }

    /// Checks the observation against the derivation below, whatever the layout.
    ///
    /// Derivation. `Old` is the load's one run of eight keys in index order: c5, o3, l7, l8,
    /// o4, l9, c6, c7. `Dlt` is the transaction's net delta: c5 (-BUILDING, +AUTOMOBILE; one key,
    /// two entries), o5, l10, and l11, four keys and five entries. The capture reads `Dlt` once:
    /// one touched group, c5; `dC` (c5's two entries) drives `O-`, `dO` (o5) drives `C+`, and
    /// `dL` (l10 and l11) drives `J+`, so all five entries are decoded. The session scans c5's
    /// group and every entry of it is kept: `C+` keeps c5, `O-` keeps o3 and o4 (`dC` requests
    /// c5), and `L-` keeps l7, l8, and l9 (`dJ` has a row at (5, 3), (5, 4), and (5, 5)).
    ///
    /// - `Old` opens on c5, the smallest key, so the seek to c5's prefix moves nothing: no
    ///   logical seek. Six steps reach o3, l7, l8, o4, l9, and c6, which ends the group, so
    ///   there are 7 positions (c7 is never reached), 6 payload decodes of `Old`, and 5 of
    ///   `Dlt`. The session opens one cursor, over `Old`.
    /// - `Old`'s run: 7 key reads (the positions), and 6 value and 6 weight reads (the six
    ///   entries taken; c6 only ends the group). `Dlt`'s run is read whole by the capture: 4 key
    ///   reads (the step past l11 lands on nothing), 5 value, and 5 weight reads. A run in a
    ///   file counts them as decodes, and a run in memory as memory reads. In all: 11 key, 11
    ///   value, and 11 weight reads.
    /// - No seek reaches a run, so no seek or filtered probe is counted.
    /// - The routing's reads of the driving deltas it merges forward alongside the scan (`dC` and
    ///   `dO` at c5: no seek, 2 key reads for the two cursors' first keys, and 3 value and 3
    ///   weight memory reads) are the `requested_slice` owner's, and nothing is read under
    ///   `query`.
    fn check(observed: &Observed) {
        assert_eq!(
            observed.counts,
            SessionCounts {
                groups: 1,
                cursor_opens: 1,
                logical_seeks: 0,
                logical_steps: 6,
                positions: 7,
                payload_decodes_old: 6,
                payload_decodes_dlt: 5,
                cancelled: 0,
            }
        );
        assert_eq!(
            observed.other_reads,
            RunReads {
                memory_key_reads: 2,
                memory_value_reads: 3,
                memory_weight_reads: 3,
                ..RunReads::default()
            }
        );
        assert_eq!(observed.old_runs.len(), 1, "Old is not one run");
        assert_eq!(observed.old_runs[0].1, 8);
        assert_eq!(observed.dlt.1, 5);

        let mut expected = run_reads(observed.old_runs[0].0, 7, 6, 6);
        expected += run_reads(observed.dlt.0, 4, 5, 5);
        assert_eq!(observed.reads, expected);
        assert_eq!(
            Totals::of(&observed.reads),
            Totals {
                probes: 0,
                keys: 11,
                values: 11,
                weights: 11
            }
        );
    }

    /// Default placement at low memory pressure: the index's runs are all in memory, so the
    /// reads are memory reads and nothing is decoded.
    #[test]
    fn session_worked_transaction_default_arm() -> Result<()> {
        let observed = run_arm(Arm::Default)?;
        check(&observed);
        assert_eq!(observed.old_runs[0].0, BatchLocation::Memory);
        assert_eq!(observed.dlt.0, BatchLocation::Memory);
        assert_eq!(observed.files.after_transaction, observed.files.at_start);
        Ok(())
    }

    /// A 1 MiB target, with the wait for high pressure, as the native test. The layout of `Old`
    /// and `Dlt` is whatever that pressure produces; it is printed, and the reads are checked
    /// against it, not against a layout chosen here.
    #[test]
    fn session_worked_transaction_pressured_arm() -> Result<()> {
        let observed = run_arm(Arm::Pressured)?;
        assert!(
            observed.pressures.at_start >= feldera_types::memory_pressure::MemoryPressure::High
        );
        check(&observed);
        Ok(())
    }
}
