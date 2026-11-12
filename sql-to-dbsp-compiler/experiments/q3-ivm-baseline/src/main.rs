mod data;
mod diagnostics;
mod measurement;
#[cfg(feature = "merged-index")]
mod reconstruction;
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
use dbsp::circuit::metadata::{MetaItem, CIRCUIT_CPU_TIME_SECONDS};
use dbsp::circuit::{
    CircuitConfig, CircuitConstructor, CircuitStorageConfig, GlobalNodeId, StorageCacheConfig,
    StorageConfig, StorageOptions,
};
use dbsp::monitor::visual_graph::CircuitNodeStyle;
use dbsp::storage::file::writer::{
    ELIDED_VALUE_HEADER_BLOCKS, FRONT_CODED_BLOCKS, FRONT_CODING_LAYOUT_FALLBACKS,
    FRONT_CODING_SIZE_FALLBACKS, VALUE_ELISION_LAYOUT_FALLBACKS, VALUE_ELISION_SIZE_FALLBACKS,
};
use dbsp::storage::write_attribution::WriteOwner;
use dbsp::trace::merged_index::Pass;
#[cfg(feature = "merged-index")]
use dbsp::trace::with_read_owner;
use dbsp::trace::RunReads;
use dbsp::{DBSPHandle, RootCircuit, Runtime};
use dbsp_adapters::{Catalog, CircuitCatalog, RecordFormat};
use feldera_types::program_schema::SqlIdentifier;
#[cfg(feature = "merged-index")]
use merged_index_col::q3::{Q3State, SessionReads};
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
struct Pipeline {
    base: Stage,
    view: Stage,
    /// Whether each transaction reads the circuits' CPU profiles (off while loading).
    profile_cpu: bool,
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
    /// Builds both circuits in one runtime, base first. The CPU profiler is enabled in both.
    fn new(config: CircuitConfig) -> Result<Self> {
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
        let index = ManualIndex::new(base.circuit.runtime())?;
        #[cfg(feature = "merged-index")]
        let view_inputs = view_input_slot
            .get()
            .context("the view circuit did not hand over its input handles")?
            .clone();
        Ok(Self {
            base,
            view,
            profile_cpu: false,
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

    /// The runtime that hosts both circuits.
    fn runtime(&self) -> &Runtime {
        self.base.circuit.runtime()
    }

    /// Runs `f` with the runtime's buffer cache bound to the calling thread, so that the
    /// driver's reads of the circuits' files go through the same cache as the workers' reads.
    fn in_cache<R>(&self, f: impl FnOnce() -> R) -> R {
        self.runtime().with_buffer_cache(0, f)
    }

    /// Both circuits with `workers` workers and memory-backed state.
    #[cfg(test)]
    fn with_workers(workers: usize) -> Result<Self> {
        Self::new(CircuitConfig::with_workers(workers))
    }
}

/// The view maintenance of the reconstructed build, which the driver runs on its own thread
/// with the runtime's buffer cache bound.
#[cfg(feature = "merged-index")]
impl Pipeline {
    /// Runs the transaction's scan session over the published `Old` and `Dlt`, which buffers the
    /// touched groups until the publication is finished. This is the transaction's only read of
    /// the index in the view phase.
    fn collect_groups(&self) -> Result<()> {
        self.in_cache(|| self.q3_state.collect(self.session_pass))
            .map_err(anyhow::Error::msg)
    }

    /// Hands the readers' `Dlt` batches, the driving deltas, to the view circuit's input
    /// handles: `ΔO` and `ΔL` are moved, and `ΔC` is a clone (see `Q3State::collect`).
    fn feed_driving_deltas(&self) -> Result<()> {
        let deltas = self
            .q3_state
            .take_driving_deltas()
            .map_err(anyhow::Error::msg)?;
        self.view_inputs.set(deltas);
        Ok(())
    }
}

/// Completes the net delta into the rows of the index's `Dlt` (see `reconstruction::prepare`).
/// The reads of `Old` are base maintenance, counted under the merged index's read owner; the
/// base state's reads of an order's customer are counted under the base relations'. `base`
/// names the customer of an order that the delta mentions only through its lines, or else
/// `reference` does; one of them must be given then. Returns the rows and the run reads of the
/// index.
#[cfg(feature = "merged-index")]
fn complete_net_delta(
    pipeline: &mut Pipeline,
    net_delta: &Change,
    base: Option<&workload::BaseState>,
    reference: Option<&Reference>,
) -> Result<(Vec<reconstruction::SourceRow>, RunReads)> {
    let runtime = pipeline.runtime().clone();
    runtime.with_buffer_cache(0, || {
        let reads_before = RunReads::current_for(WriteOwner::MergedIndex);
        let old = pipeline.index.snapshot()?;
        let mut base_owner = |order: i64| -> Result<Option<i64>> {
            if let Some(base) = base {
                let found = with_read_owner(WriteOwner::BaseRelation, || base.order(order))?;
                return Ok(found.map(|order| order.customer_key));
            }
            let reference = reference.context(
                "a net delta that names an order only through its lines needs the base state \
                 or a reference to say whose order it is",
            )?;
            Ok(reference.orders.get(&order).map(|order| order.customer_key))
        };
        let rows = with_read_owner(WriteOwner::MergedIndex, || {
            reconstruction::prepare(net_delta, &old, &mut pipeline.orphans, &mut base_owner)
        })?;
        let reads = RunReads::current_for(WriteOwner::MergedIndex).since(reads_before);
        Ok((rows, reads))
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
    /// Reading the base tables' net delta and feeding it to the view circuit's inputs.
    forward: Duration,
    /// Completing the net delta into the index's `Dlt` (reading `Old` for moved lines), appending
    /// it, and acquiring the snapshots (reconstructed mode). The index is base storage, so this
    /// is a part of `base_ms`.
    index: Duration,
    /// The view circuit's transaction: delta accumulation, joins and `V`. In reconstructed mode
    /// it starts with the scan session's collection of the touched groups and the push of the
    /// driving deltas into the circuit's input handles.
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
    /// Where the last transaction's published index snapshots kept their runs (reconstructed
    /// mode).
    index_layout: Option<Value>,
    /// The view worker's run reads in the view transactions, per read owner, indexed like
    /// `WriteOwner::ALL` (both modes).
    view_reads: [RunReads; WriteOwner::ALL.len()],
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

    /// Incremental view maintenance: the view circuit's transaction. The merged index's upkeep
    /// is not part of it.
    fn ivm_phase(&self) -> Duration {
        self.view_circuit
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

    // The index sees the net delta, completed into `Dlt`. Base maintenance does that and the
    // append (`index`, a part of `base_ms`), including its reads of `Old` for the lines of an
    // order that changes owner; the view circuit then takes its inputs from `Dlt`.
    #[cfg(feature = "merged-index")]
    {
        let index_started = Instant::now();
        let (index_rows, upkeep_reads) = complete_net_delta(
            pipeline,
            &net_delta,
            check.map(|(base, _)| base),
            state.as_deref(),
        )?;
        let (old, dlt) = pipeline.index.append(index_rows)?;
        stats.index += index_started.elapsed();
        stats.upkeep_reads += upkeep_reads;
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
    // The native view circuit takes the net delta as JSON rows, which is not view maintenance.
    #[cfg(not(feature = "merged-index"))]
    {
        let feed_started = Instant::now();
        net_delta.apply_optional(None, pipeline.view.inputs())?;
        stats.forward += feed_started.elapsed();
    }

    // The view worker counts its own run reads; they are read outside the transaction's timer.
    let view_reads_before = pipeline.view.circuit.run_reads()?;
    let view_started = Instant::now();
    // The IVM window of the reconstructed build starts with the scan session, which reads the
    // index once for the whole transaction and fills the readers. Handing the readers' `Dlt`
    // batches to the input handles is the counterpart of native's feed, so it is booked in
    // `forward_ms` and the window resumes with the transaction.
    #[cfg(feature = "merged-index")]
    let (transaction_result, latency) = {
        let collection_result = pipeline.collect_groups();
        let collection = view_started.elapsed();
        let feed_started = Instant::now();
        let feed_result = collection_result.and_then(|()| pipeline.feed_driving_deltas());
        stats.forward += feed_started.elapsed();
        let transaction_started = Instant::now();
        let transaction_result = feed_result.and_then(|()| {
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
    let (transaction_result, latency) = {
        let transaction_result = pipeline.view.circuit.transaction();
        (transaction_result, view_started.elapsed())
    };
    let view_reads_after = pipeline.view.circuit.run_reads()?;
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
    stats.view_circuit += latency;
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
    let (index_rows, _) = complete_net_delta(pipeline, net_delta, None, Some(reference))?;
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
        pipeline.collect_groups()?;
        pipeline.feed_driving_deltas()?;
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
    /// How the reconstructed build's scan session passes untouched groups
    /// (`--session-pass seek|step`); `None` means `Seek`. The native build has
    /// no session and ignores it.
    session_pass: Option<Pass>,
    workers: usize,
    batch: usize,
    runs: usize,
    verify: bool,
}

impl Config {
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
                "--session-pass" => {
                    config.session_pass = Some(parse_pass(
                        &args.next().context("--session-pass needs seek or step")?,
                    )?)
                }
                "--r5" => config.r5 = true,
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
        if config.r5
            && (config.workers != 1
                || config.runs != 1
                || config.storage_path.is_none()
                || config.cache_bytes.is_none()
                || config.memory_target_bytes.is_none()
                || config.report.is_none())
        {
            bail!("R5 requires one worker, one run per process, file storage, explicit cache and memory target, and --report");
        }
        if config.report.is_some() && config.runs != 1 {
            bail!("--report requires one run per process");
        }
        if config.diagnostics.is_some() && (config.runs != 1 || config.storage_path.is_none()) {
            bail!("--diagnostics requires file storage and one run per process");
        }
        Ok(config)
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Refresh {
    rf1_orders: Option<PathBuf>,
    rf1_lines: Option<PathBuf>,
    rf2: Option<PathBuf>,
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
    fn read(refresh: &Refresh) -> Result<Self> {
        if let Some(path) = &refresh.rf2 {
            return Ok(Self::Rf2(
                rows(path, data::delete_order_key)?.into_iter().collect(),
            ));
        }
        let orders = refresh
            .rf1_orders
            .as_deref()
            .context("RF1 Orders required")?;
        let lines = refresh
            .rf1_lines
            .as_deref()
            .context("RF1 Lineitem required")?;
        Ok(Self::Rf1(Change {
            orders: rows(orders, Order::parse)?
                .into_iter()
                .map(|row| (row, true))
                .collect(),
            lines: rows(lines, Line::parse)?
                .into_iter()
                .map(|row| (row, true))
                .collect(),
            ..Change::default()
        }))
    }

    /// The row counts of this refresh's input, for the report.
    fn summary(&self, number: usize) -> Value {
        match self {
            Self::Rf1(change) => json!({
                "refresh": number, "phase": "rf1",
                "orders": change.orders.len(), "lines": change.lines.len(),
            }),
            Self::Rf2(keys) => json!({"refresh": number, "phase": "rf2", "order_keys": keys.len()}),
        }
    }
}

/// The current memory-pressure level of the runtime. The level comes from the process's RSS
/// against the memory target, not from cgroup memory.
fn memory_pressure_levels(pipeline: &Pipeline) -> Result<Value> {
    Ok(json!({
        "runtime": serde_json::to_value(pipeline.runtime().current_memory_pressure())?,
    }))
}

fn refreshes(config: &Config) -> Result<Vec<Refresh>> {
    let mut refreshes = if let Some(path) = &config.refresh_list {
        let mut list: Vec<Refresh> = serde_json::from_reader(File::open(path)?)?;
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
        list
    } else {
        let mut list = Vec::new();
        if config.rf1_orders.is_some() {
            list.push(Refresh {
                rf1_orders: config.rf1_orders.clone(),
                rf1_lines: config.rf1_lines.clone(),
                rf2: None,
            });
        }
        if config.rf2.is_some() {
            list.push(Refresh {
                rf1_orders: None,
                rf1_lines: None,
                rf2: config.rf2.clone(),
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
    *base = workload::BaseState::new(&pipeline.base.catalog)?;
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
    wait_for_loading_merges(pipeline, timeout.saturating_sub(started.elapsed()))?;
    // Setup compaction is reported on its own, not as background merge wait.
    Ok((
        started.elapsed().saturating_sub(setup_compaction_elapsed),
        setup_compaction,
    ))
}

/// The phases of a refresh (since measurement version 4).
///
/// `base_ms` is base maintenance: the base circuit's transaction plus the merged index's
/// upkeep. `index_ms` is a sub-phase of `base_ms` and is never added to it again. `ivm_ms` is
/// incremental view maintenance and equals `q3_ms`, the view circuit's transaction (in
/// reconstructed mode, with the scan session's collection before it). `forward_ms` holds the
/// net delta read and the feed of the driving deltas into the view circuit's input handles:
/// native's rows, and in reconstructed mode the readers' `Dlt` batches (measurement version
/// 10). The base circuit, `forward_ms`, the index append and `q3_ms` are
/// disjoint intervals of the transaction; the base and IVM phases are always reported side by side.
fn phase_json(stats: &Measurements) -> Value {
    json!({
        "rows": stats.rows,
        "transactions": stats.transactions,
        "discovery_ms": milliseconds(stats.discovery),
        "checks_input_preparation_ms": milliseconds(stats.preparation),
        "base_ms": milliseconds(stats.base_phase()),
        "forward_ms": milliseconds(stats.forward),
        "index_ms": milliseconds(stats.index),
        "q3_ms": milliseconds(stats.view_circuit),
        "publication_cleanup_ms": milliseconds(stats.publication_cleanup),
        "foreground_ms": milliseconds(stats.foreground),
        "ivm_ms": milliseconds(stats.ivm_phase()),
        "phase_note": "base_ms = the base circuit's transaction + index_ms (the merged index's upkeep, including the reads of Old for the lines of an order that changes owner; zero in native mode); index_ms is a sub-phase of base_ms and is not added again. ivm_ms = q3_ms, the view circuit's transaction; in reconstructed mode it also includes the scan session's collection of the touched groups into the typed readers. Report base_ms and ivm_ms side by side. forward_ms (the net delta read, and the feed of the driving deltas into the view circuit's input handles: native's rows, or reconstructed's hand-off of the readers' Dlt batches, moved for orders and line items and cloned for customers), discovery, preparation and publication/cleanup are in neither.",
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
fn view_reads_json(reads: &[RunReads; WriteOwner::ALL.len()]) -> Result<Value> {
    WriteOwner::ALL
        .into_iter()
        .map(|owner| {
            Ok((
                owner.name().to_owned(),
                serde_json::to_value(reads[owner as usize])?,
            ))
        })
        .collect::<Result<serde_json::Map<_, _>>>()
        .map(Value::Object)
}

/// The merged index's run reads in the view phase (measurement version 8): the scan session's,
/// which are the only reads of the index there, under the key `session`. File runs count key,
/// value and weight decodes and in-memory runs count `memory_*` reads, apart; `index_layout`
/// says where the runs were. The session's moves are in [`session_counts_json`].
#[cfg(feature = "merged-index")]
fn index_reads_json(reads: &SessionReads) -> Result<Value> {
    Ok(json!({"session": serde_json::to_value(reads.run_reads)?}))
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
    let storage_root = config
        .storage_path
        .as_ref()
        .map(|root| root.join(format!("run-{run_number}")));
    if let Some(path) = &storage_root {
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
    let mut pipeline = Pipeline::new(pipeline_config)?;
    #[cfg(feature = "merged-index")]
    {
        pipeline.session_pass = config.session_pass();
    }
    let runtime = pipeline.runtime().clone();
    // The driver thread binds the runtime's buffer cache, as the workers do.
    runtime.with_buffer_cache(0, || {
    let sampler = storage_root
        .clone()
        .map(measurement::StorageSampler::start)
        .transpose()?;
    let mut state = config.verify.then(Reference::default);
    let mut stats = Measurements::default();
    let mut base = workload::BaseState::new(&pipeline.base.catalog)?;
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
            base = workload::BaseState::new(&pipeline.base.catalog)?;
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
    println!("run={run_number} phase=load rows={load_rows} transactions={} verified={} base_ms={:.3} forward_ms={:.3} q3_ms={:.3} prep_ms={:.3} background_merge_wait_ms={:.3} end_to_end_ms={:.3} driver_reference_bytes={} driver_max_load_retained_rows={} peak_process_rss_bytes={} {}",
        stats.transactions, config.verify, milliseconds(stats.base_phase()), milliseconds(stats.forward),
        milliseconds(stats.view_circuit), milliseconds(stats.preparation),
        milliseconds(load_drain), milliseconds(load_elapsed), state.as_ref().map_or(0, |s| s.size_of().total_bytes()), load.max_retained_rows, rss_bytes(), latency_summary(&stats.latencies));

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
            .map(|(index, prepared)| prepared.summary(index + 1))
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
    let mut memory_pressure_before = memory_pressure_levels(&pipeline)?;
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
    let mut sequence_view_reads = [RunReads::default(); WriteOwner::ALL.len()];
    for (number, prepared) in prepared_refreshes.into_iter().enumerate() {
        let resources_before = measurement::ResourceSnapshot::capture()?;
        let phase_started = Instant::now();
        let mut measurement = Measurements::default();
        let (change, absent, phase, discovered) = match prepared {
            PreparedRefresh::Rf2(keys) => {
                #[cfg(feature = "merged-index")]
                let snapshot = pipeline.index.snapshot()?;
                // Discovery is the lookup alone: the keys were read before the window.
                let (change, absent) = workload::rf2_changes(&base, &keys, |order| {
                    #[cfg(feature = "merged-index")]
                    {
                        // The index's files are read through the runtime's buffer cache,
                        // as the view circuit's readers read them.
                        pipeline.in_cache(|| reconstruction::lines(&snapshot, order))
                    }
                    #[cfg(not(feature = "merged-index"))]
                    {
                        base.native_lines(order.key)
                    }
                })?;
                measurement.discovery += phase_started.elapsed();
                (change, absent, "rf2", true)
            }
            PreparedRefresh::Rf1(change) => (change, 0, "rf1", false),
        };
        let workload_stats_started = Instant::now();
        let customers: HashSet<_> = change
            .orders
            .iter()
            .map(|(order, _)| order.customer_key)
            .collect();
        let order_count = change.orders.len();
        let mut fanout = HashMap::<i64, usize>::new();
        for (line, _) in &change.lines {
            *fanout.entry(line.order_key).or_default() += 1;
        }
        let line_count = change.lines.len();
        let customer_count = customers.len();
        let maximum_fanout = fanout.values().max().copied().unwrap_or(0);
        measurement.preparation += workload_stats_started.elapsed();
        transact_inner(
            &mut pipeline,
            state.as_mut(),
            change,
            Some((&base, discovered)),
            &mut measurement,
        )?;
        let cleanup_started = Instant::now();
        base = workload::BaseState::new(&pipeline.base.catalog)?;
        drop(customers);
        drop(fanout);
        measurement.publication_cleanup += cleanup_started.elapsed();
        // Wall time covers discovery, the transaction and all foreground cleanup; the refresh
        // files were read before the window.
        let phase_finished = Instant::now();
        measurement.foreground = phase_finished
            .duration_since(phase_started)
            .saturating_sub(measurement.verification);
        verification_time += measurement.verification;
        foreground_total += measurement.foreground;
        sequence_finished = phase_finished;
        resources_after = measurement::ResourceSnapshot::capture()?;
        window_finished_micros = dbsp::storage::diagnostics::now_micros();
        window_evidence.sample_boundary();
        let memory_pressure_after = memory_pressure_levels(&pipeline)?;
        let resource_delta = resources_before.delta(&resources_after);
        let mut report = phase_json(&measurement);
        report["refresh"] = json!(number + 1);
        report["phase"] = json!(phase);
        report["memory_pressure"] = json!({
            "before": std::mem::replace(&mut memory_pressure_before, memory_pressure_after.clone()),
            "after": memory_pressure_after,
        });
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
            report["index_upkeep_run_reads"] = upkeep_reads_json(&measurement.upkeep_reads)?;
        }
        #[cfg(not(feature = "merged-index"))]
        {
            report["index_run_reads"] = Value::Null;
            report["index_session_counts"] = Value::Null;
            report["index_upkeep_run_reads"] = Value::Null;
        }
        report["index_layout"] = json!(measurement.index_layout);
        for (total, reads) in sequence_view_reads.iter_mut().zip(measurement.view_reads) {
            *total += reads;
        }
        report["view_run_reads"] = view_reads_json(&measurement.view_reads)?;
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
    let sequence_index_upkeep_run_reads = upkeep_reads_json(&sequence_upkeep_reads)?;
    #[cfg(not(feature = "merged-index"))]
    let (sequence_index_run_reads, sequence_index_session_counts, sequence_index_upkeep_run_reads) =
        (Value::Null, Value::Null, Value::Null);
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
        "measurement_version": 11,
        "os": std::env::consts::OS,
        "mode": if cfg!(feature = "merged-index") { "reconstructed" } else { "native" },
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
            "session_pass": cfg!(feature = "merged-index").then(|| pass_name(config.session_pass())),
            "durability": "no checkpoint; refresh completion is not a durability guarantee",
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
        "load": {"rows": load_rows, "transactions": load.transactions, "max_retained_rows": load.max_retained_rows,
            "catalog_storage": load_storage, "snapshot_publication_transactions": 1,
            "sampled_peak_storage_bytes": load_peak_storage_bytes,
            "observed_storage_bytes": load_observed_storage_bytes,
            "background_merge_wait_ms": milliseconds(load_drain), "elapsed_ms": milliseconds(load_elapsed),
            "setup_full_compaction": setup_compaction},
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
            "index_upkeep_run_reads": sequence_index_upkeep_run_reads,
            "view_run_reads": view_reads_json(&sequence_view_reads)?,
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
        let (index_rows, _) =
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
        let mut pipeline = Pipeline::new(runtime_config(&config, Some(&storage))?)?;
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
        let (index_rows, _) =
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
            view_circuit: Duration::from_millis(2),
            ..Measurements::default()
        };
        let phases = phase_json(&stats);
        let phase = |name: &str| phases[name].as_f64().unwrap();
        // base_ms = 4 + 1, with index_ms reported as its sub-phase and not added again.
        assert!((phase("base_ms") - 5.0).abs() < 1e-9);
        assert!((phase("index_ms") - 1.0).abs() < 1e-9);
        assert!((phase("q3_ms") - 2.0).abs() < 1e-9);
        assert_eq!(phase("ivm_ms"), phase("q3_ms"));
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
        // is a part of `base_ms`, and the IVM figure is the view circuit alone.
        for refresh in refreshes {
            let phase = |name: &str| refresh[name].as_f64().unwrap();
            assert!(phase("base_ms") > 0.0, "{refresh}");
            assert!(phase("q3_ms") > 0.0, "{refresh}");
            assert!(phase("forward_ms") >= 0.0, "{refresh}");
            assert!(phase("index_ms") <= phase("base_ms"), "{refresh}");
            assert!((phase("ivm_ms") - phase("q3_ms")).abs() < 1e-6, "{refresh}");
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
        assert_eq!(output["measurement_version"], 11);
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
        } else {
            for refresh in refreshes {
                assert!(refresh["index_run_reads"].is_null(), "{refresh}");
                assert!(refresh["index_session_counts"].is_null(), "{refresh}");
                assert!(refresh["index_upkeep_run_reads"].is_null(), "{refresh}");
                assert!(refresh["index_layout"].is_null(), "{refresh}");
            }
            assert!(output["sequence"]["index_run_reads"].is_null());
        }
        // The view worker's reads, per owner, in both modes. Nothing the worker runs reads under
        // `merged_index`: the scan session reads the index on the driver's thread. The joins'
        // reads of the readers' batches are under `requested_slice` in reconstructed mode, and
        // none are in native mode.
        let reads_fields = [
            "seeks",
            "filter_rejections",
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
            for owner in ["query", "base_relation", "merged_index", "requested_slice"] {
                assert!(view[owner]["seeks"].is_u64(), "{refresh}");
            }
            assert_eq!(owner_total(view, "merged_index"), 0, "{refresh}");
            if !cfg!(feature = "merged-index") {
                assert_eq!(owner_total(view, "requested_slice"), 0, "{refresh}");
            }
        }
        // A refresh that changes nothing reads no slice, so the reconstructed run is checked as a
        // whole.
        if cfg!(feature = "merged-index") {
            let slice_reads: u64 = refreshes
                .iter()
                .map(|refresh| owner_total(&refresh["view_run_reads"], "requested_slice"))
                .sum();
            assert!(slice_reads > 0);
        }
        let view_seeks: u64 = refreshes
            .iter()
            .map(|refresh| {
                refresh["view_run_reads"]["query"]["seeks"]
                    .as_u64()
                    .unwrap()
            })
            .sum();
        assert_eq!(
            output["sequence"]["view_run_reads"]["query"]["seeks"],
            view_seeks
        );
        if !cfg!(feature = "merged-index") {
            // The native joins probe their traces on the view worker.
            assert!(view_seeks > 0);
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
            for side in ["before", "after"] {
                assert!(
                    refresh["memory_pressure"][side]["runtime"].is_string(),
                    "{refresh}"
                );
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
        let pipeline = Pipeline::new(runtime_config(&arm.config(), Some(&storage))?)?;
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
                // A probe that a run's filter rejects is a probe all the same.
                probes: reads.seeks + reads.filter_rejections,
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
    /// run, and confirmed by the pins below. `probes` are seeks, or filter rejections when a
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
        let [query, base_relation, merged_index, requested_slice] = stats.view_reads;
        println!(
            "native {}: pressure {pressures}; files {files}\n  reads {query:?}",
            arm.name()
        );
        assert_eq!(Totals::of(&query), tally_totals());
        assert_eq!(base_relation, RunReads::default());
        assert_eq!(merged_index, RunReads::default());
        assert_eq!(requested_slice, RunReads::default());
        Ok((query, pressures, files))
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
    /// seven probes is answered by a run's filter, which counts a filter rejection and no seek
    /// (6 seeks and 1 rejection); the test pins that split only then. The rejected probe is
    /// ΔL's o5 into J's old run, which has no entry of o5.
    #[test]
    fn worked_transaction_pressured_arm() -> Result<()> {
        let (reads, pressures, _files) = run_arm(Arm::Pressured)?;
        assert!(pressures.at_start >= feldera_types::memory_pressure::MemoryPressure::High);
        // The runs landed in files in every run observed (Critical pressure throughout). Then
        // the totals are all decodes, and the one rejected probe is not a seek. A mixed layout
        // would split the reads differently, and is reported, not forced.
        let memory_reads =
            reads.memory_key_reads + reads.memory_value_reads + reads.memory_weight_reads;
        if memory_reads == 0 {
            assert_eq!(
                reads,
                RunReads {
                    seeks: 6,
                    filter_rejections: 1,
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

                let index_before = RunReads::current_for(WriteOwner::MergedIndex);
                let total_before = RunReads::current();
                pipeline.collect_groups()?;
                let reads = RunReads::current_for(WriteOwner::MergedIndex).since(index_before);
                let other_reads = RunReads::current().since(total_before).since(reads);

                // The view circuit takes the same readers' driving deltas, and `V` must be the
                // independent join of the new state.
                pipeline.feed_driving_deltas()?;
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
    /// - No seek reaches a run, so no seek or filter rejection is counted.
    /// - The routing's own reads of the readers' in-memory driving deltas (`dC` and `dO` at c5:
    ///   two seeks, 2 key, 3 value, and 3 weight memory reads) are another owner's.
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
                seeks: 2,
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
