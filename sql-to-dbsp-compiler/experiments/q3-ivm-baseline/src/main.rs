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
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use data::{Customer, Line, Order};
use dbsp::circuit::{
    CircuitConfig, CircuitStorageConfig, StorageCacheConfig, StorageConfig, StorageOptions,
};
use dbsp::monitor::visual_graph::CircuitNodeStyle;
use dbsp::storage::file::writer::{
    ELIDED_VALUE_HEADER_BLOCKS, FRONT_CODED_BLOCKS, FRONT_CODING_LAYOUT_FALLBACKS,
    FRONT_CODING_SIZE_FALLBACKS, VALUE_ELISION_LAYOUT_FALLBACKS, VALUE_ELISION_SIZE_FALLBACKS,
};
use dbsp_adapters::{Catalog, CircuitCatalog, RecordFormat};
use feldera_types::program_schema::SqlIdentifier;
#[cfg(feature = "merged-index")]
use merged_index_col::q3::Q3State;
#[cfg(feature = "merged-index")]
use reconstruction::ManualIndex;
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

/// One circuit with its catalog and the input streams of its tables.
struct Stage {
    circuit: dbsp::DBSPHandle,
    catalog: Catalog,
    inputs: Inputs,
}

impl Stage {
    fn new((circuit, catalog): (dbsp::DBSPHandle, Catalog), deletes: DeleteStyle) -> Result<Self> {
        let inputs = Inputs::new(&catalog, deletes)?;
        Ok(Self {
            circuit,
            catalog,
            inputs,
        })
    }

    /// Runs `f` with this stage's buffer cache bound to the calling thread, so that reads of
    /// this circuit's files are charged to its cache and not to the other circuit's.
    fn in_cache<R>(&self, f: impl FnOnce() -> R) -> R {
        self.circuit.runtime().with_buffer_cache(0, f)
    }
}

/// The two chained circuits, each timed on its own.
///
/// A transaction runs first on `base`, which holds the base relations in primary-key input
/// maps. The net change of those relations is then forwarded to `view`, which maintains `V`.
/// In reconstructed mode the same net change also feeds the merged index, which lives in the
/// base runtime and stands in for the view circuit's join traces; the view circuit reads it
/// through published snapshots.
struct Pipeline {
    base: Stage,
    view: Stage,
    #[cfg(feature = "merged-index")]
    index: ManualIndex,
    #[cfg(feature = "merged-index")]
    q3_state: Q3State,
}

impl Pipeline {
    /// Builds both circuits; each has its own runtime, and so its own storage and buffer cache.
    fn new(base_config: CircuitConfig, view_config: CircuitConfig) -> Result<Self> {
        let base = Stage::new(q3_base_circuit::circuit(base_config)?, DeleteStyle::ByKey)?;
        workload::enable_net_delta(&base.catalog)?;
        #[cfg(feature = "merged-index")]
        let q3_state = Q3State::default();
        #[cfg(feature = "merged-index")]
        let view_circuit = q3_view_circuit::circuit(view_config, q3_state.clone())?;
        #[cfg(not(feature = "merged-index"))]
        let view_circuit = q3_view_circuit::circuit(view_config)?;
        let view = Stage::new(view_circuit, DeleteStyle::WholeRow)?;
        // The index is secondary storage of the base relations, so it belongs to the base
        // runtime (thread, buffer cache, storage directory) and its upkeep is base maintenance.
        // The view circuit reads it only through the snapshots that `Q3State` publishes.
        #[cfg(feature = "merged-index")]
        let index = ManualIndex::new(base.circuit.runtime());
        Ok(Self {
            base,
            view,
            #[cfg(feature = "merged-index")]
            index,
            #[cfg(feature = "merged-index")]
            q3_state,
        })
    }

    /// Both circuits with `workers` workers and memory-backed state.
    #[cfg(test)]
    fn with_workers(workers: usize) -> Result<Self> {
        Self::new(
            CircuitConfig::with_workers(workers),
            CircuitConfig::with_workers(workers),
        )
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
    /// Appending the net delta to the merged index and acquiring its snapshots (reconstructed
    /// mode). The index is base storage, so this is a part of `base_ms`.
    index: Duration,
    /// The view circuit's transaction: delta accumulation, joins and `V`.
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
    change.apply_optional(None, &mut pipeline.base.inputs)?;
    stats.preparation += preparation_started.elapsed();

    let base_started = Instant::now();
    pipeline.base.circuit.transaction()?;
    stats.base_circuit += base_started.elapsed();

    // The base circuit's delta streams hold the net change of each base table: an upsert that
    // replaces a row appears as a deletion of the old row and an insertion of the new one.
    let forward_started = Instant::now();
    let net_delta = workload::net_delta(&pipeline.base.catalog)?;
    stats.forward += forward_started.elapsed();

    // The index and the view circuit's joins see the same net delta. The index append is base
    // maintenance (`index`, a part of `base_ms`); the row conversion stays in preparation.
    #[cfg(feature = "merged-index")]
    {
        let preparation_started = Instant::now();
        let (placements, index_rows) = reconstruction::prepare(&net_delta)?;
        stats.preparation += preparation_started.elapsed();
        let index_started = Instant::now();
        let (before_index, after_index) = pipeline.index.append(index_rows)?;
        stats.index += index_started.elapsed();
        let publish_started = Instant::now();
        pipeline
            .q3_state
            .publish(before_index, after_index, placements)
            .map_err(anyhow::Error::msg)?;
        stats.publication_cleanup += publish_started.elapsed();
    }
    let feed_started = Instant::now();
    net_delta.apply_optional(None, &mut pipeline.view.inputs)?;
    stats.forward += feed_started.elapsed();

    let view_started = Instant::now();
    let transaction_result = pipeline.view.circuit.transaction();
    let latency = view_started.elapsed();
    #[cfg(feature = "merged-index")]
    let finish_result = {
        let cleanup_started = Instant::now();
        let result = pipeline.q3_state.finish().map_err(anyhow::Error::msg);
        stats.publication_cleanup += cleanup_started.elapsed();
        result
    };
    transaction_result?;
    #[cfg(feature = "merged-index")]
    finish_result?;
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
        let actual_delta = pipeline
            .view
            .in_cache(|| read_output(&*output.delta_handle))?;
        let expected_delta = difference(&before, &after);
        if actual_delta != expected_delta {
            bail!("emitted delta differs from independent full join: actual {actual_delta:?}, expected {expected_delta:?}");
        }
        let snapshot = output
            .integrate_handle
            .as_ref()
            .context("V has no materialized snapshot")?;
        let actual_state = pipeline.view.in_cache(|| read_output(&**snapshot))?;
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
            step.apply_optional(Some(state), &mut pipeline.base.inputs)?;
            assert!(!pipeline.base.circuit.step()?);
        }
        pipeline.base.circuit.commit_transaction()?;
    } else {
        change.apply_optional(Some(state), &mut pipeline.base.inputs)?;
        pipeline.base.circuit.transaction()?;
    }
    workload::net_delta(&pipeline.base.catalog)
}

/// Appends the net delta to the merged index and publishes the before and after snapshots,
/// as `transact_inner` does before the view circuit runs.
#[cfg(all(test, feature = "merged-index"))]
fn publish_net_delta(pipeline: &Pipeline, net_delta: &Change) -> Result<()> {
    let (placements, index_rows) = reconstruction::prepare(net_delta)?;
    let (before, after) = pipeline.index.append(index_rows)?;
    pipeline
        .q3_state
        .publish(before, after, placements)
        .map_err(anyhow::Error::msg)
}

/// Runs the net delta on the view circuit, as one transaction of three steps when `stepped`.
/// In reconstructed mode the caller has published the delta, and the publication is finished
/// here.
#[cfg(test)]
fn run_view(pipeline: &mut Pipeline, net_delta: Change, stepped: bool) -> Result<()> {
    if stepped {
        pipeline.view.circuit.start_transaction()?;
        for step in steps_of(&net_delta) {
            step.apply_optional(None, &mut pipeline.view.inputs)?;
            assert!(!pipeline.view.circuit.step()?);
        }
        pipeline.view.circuit.commit_transaction()?;
    } else {
        net_delta.apply_optional(None, &mut pipeline.view.inputs)?;
        pipeline.view.circuit.transaction()?;
    }
    #[cfg(feature = "merged-index")]
    pipeline.q3_state.finish().map_err(anyhow::Error::msg)?;
    Ok(())
}

/// Like `transact`, but each circuit runs the change as one transaction of three steps: the
/// base circuit takes the change, and the view circuit takes the net delta in the same steps.
/// The merged index is published once, and every step of the view transaction shares it.
#[cfg(all(test, feature = "merged-index"))]
fn transact_in_steps(pipeline: &mut Pipeline, state: &mut Reference, change: Change) -> Result<()> {
    let net_delta = run_base(pipeline, state, change, true)?;
    publish_net_delta(pipeline, &net_delta)?;
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
    workers: usize,
    batch: usize,
    runs: usize,
    verify: bool,
}

impl Config {
    fn parse() -> Result<Self> {
        Self::from_args(std::env::args().skip(1))
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

/// The current memory-pressure level of each circuit's runtime. The level comes from the
/// process's RSS against the memory target, not from cgroup memory.
fn memory_pressure_levels(pipeline: &Pipeline) -> Result<Value> {
    Ok(json!({
        "base_runtime": serde_json::to_value(pipeline.base.circuit.runtime().current_memory_pressure())?,
        "q3_runtime": serde_json::to_value(pipeline.view.circuit.runtime().current_memory_pressure())?,
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
            .publish(snapshot.clone(), snapshot, BTreeMap::new())
            .map_err(anyhow::Error::msg)?;
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
            let output = stage
                .catalog
                .output_handles(&SqlIdentifier::from(*name))
                .context("missing output")?;
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

/// The phases of a refresh (measurement version 4).
///
/// `base_ms` is base maintenance: the base circuit's transaction plus the merged index's
/// upkeep. `index_ms` is a sub-phase of `base_ms` and is never added to it again. `ivm_ms` is
/// incremental view maintenance and equals `q3_ms`, the view circuit's transaction. The
/// base circuit, `forward_ms`, the index append and `q3_ms` are disjoint intervals of the
/// transaction; the base and IVM phases are always reported side by side.
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
        "phase_note": "base_ms = the base circuit's transaction + index_ms (the merged index's upkeep, zero in native mode); index_ms is a sub-phase of base_ms and is not added again. ivm_ms = q3_ms, the view circuit's transaction. Report base_ms and ivm_ms side by side. forward_ms (net delta read and feed), discovery, preparation and publication/cleanup are in neither.",
        "verification_ms_excluded": milliseconds(stats.verification),
    })
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

/// Splits the buffer cache between the two circuits' runtimes: the base circuit takes half the
/// MiB rounded down, the view circuit the rest.
fn split_cache_mib(cache_bytes: Option<usize>) -> (Option<usize>, Option<usize>) {
    match cache_bytes {
        Some(bytes) => {
            let mib = bytes / (1024 * 1024);
            (Some(mib / 2), Some(mib - mib / 2))
        }
        None => (None, None),
    }
}

/// The configuration of one circuit. Both circuits get the same worker count and memory-pressure
/// target; they differ in storage directory and buffer cache.
fn circuit_config(
    config: &Config,
    storage_path: Option<&Path>,
    cache_mib: Option<usize>,
) -> Result<CircuitConfig> {
    let mut circuit_config =
        CircuitConfig::with_workers(config.workers).with_max_rss_bytes(config.memory_target_bytes);
    circuit_config.dev_tweaks.fetch_join = Some(config.r5);
    circuit_config.dev_tweaks.front_code_byte_keys = Some(config.front_code_byte_keys);
    circuit_config.dev_tweaks.elide_value_headers = Some(config.elide_value_headers);
    if let Some(path) = storage_path {
        std::fs::create_dir_all(path)?;
        circuit_config = circuit_config.with_storage(Some(CircuitStorageConfig::for_config(
            StorageConfig {
                path: path.to_string_lossy().into_owned(),
                cache: StorageCacheConfig::FelderaCache,
            },
            StorageOptions {
                min_storage_bytes: Some(0),
                cache_mib,
                compression: feldera_types::config::StorageCompression::None,
                ..StorageOptions::default()
            },
        )?));
    }
    Ok(circuit_config)
}

/// The two circuits' storage directories under one run's storage root.
const BASE_STORAGE_DIRECTORY: &str = "base";
const Q3_STORAGE_DIRECTORY: &str = "q3";

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
    // Each circuit has its own runtime, and a storage directory is locked by one runtime.
    let base_storage = storage_root
        .as_ref()
        .map(|root| root.join(BASE_STORAGE_DIRECTORY));
    let q3_storage = storage_root
        .as_ref()
        .map(|root| root.join(Q3_STORAGE_DIRECTORY));
    let (base_cache_mib, q3_cache_mib) = split_cache_mib(config.cache_bytes);
    let base_config = circuit_config(config, base_storage.as_deref(), base_cache_mib)?;
    let q3_config = circuit_config(config, q3_storage.as_deref(), q3_cache_mib)?;
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
    let mut pipeline = Pipeline::new(base_config, q3_config)?;
    let base_runtime = pipeline.base.circuit.runtime().clone();
    // The driver thread reads base state; reads of the view circuit's state bind its cache.
    base_runtime.with_buffer_cache(0, || {
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
    let (base_cache_occupancy_at_start, _) = pipeline.base.circuit.runtime().cache_occupancy();
    let (q3_cache_occupancy_at_start, _) = pipeline.view.circuit.runtime().cache_occupancy();
    let mut memory_pressure_before = memory_pressure_levels(&pipeline)?;
    let sequence_started = Instant::now();
    let mut sequence_finished = sequence_started;
    let mut resources_after = sequence_resources;
    let mut window_finished_micros = window_started_micros;
    let mut reports = Vec::new();
    let mut verification_time = Duration::ZERO;
    let mut foreground_total = Duration::ZERO;
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
                        // The index belongs to the base runtime, so its files are read
                        // through the base buffer cache.
                        pipeline
                            .base
                            .in_cache(|| reconstruction::lines(&snapshot, order))
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
        println!(
            "run={run_number} refresh={} phase={phase} {}",
            number + 1,
            report
        );
        reports.push(report);
    }
    // The window ends with the last refresh; read its memory figures before anything else runs.
    let window_memory_fields = window_evidence.finish();
    let sequence_elapsed = sequence_finished
        .duration_since(sequence_started)
        .saturating_sub(verification_time);
    let observed_storage_bytes = storage_root
        .as_deref()
        .map(measurement::storage_size_bytes)
        .transpose()?;
    let observed = |path: &Option<PathBuf>| {
        path.as_deref()
            .map(measurement::storage_size_bytes)
            .transpose()
    };
    let base_storage_bytes = observed(&base_storage)?;
    let q3_storage_bytes = observed(&q3_storage)?;
    let sampled_peak_storage_bytes = sampler.map(measurement::StorageSampler::stop).transpose()?;
    let (base_cache_occupancy, base_cache_capacity) = pipeline.base.circuit.runtime().cache_occupancy();
    let (q3_cache_occupancy, q3_cache_capacity) = pipeline.view.circuit.runtime().cache_occupancy();
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
            &[base_storage.as_deref(), q3_storage.as_deref()]
                .into_iter()
                .collect::<Option<Vec<_>>>()
                .context("--diagnostics requires file storage")?,
        )?;
    }
    let mut report = json!({
        "status": "complete",
        "measurement_version": 4,
        "os": std::env::consts::OS,
        "mode": if cfg!(feature = "merged-index") { "reconstructed" } else { "native" },
        "qualification": "runner determines cgroup qualification; macOS is smoke only",
        "configuration": {
            "workers": config.workers, "load_batch_target": config.batch, "verification": config.verify,
            "driver_reference_bytes": state.as_ref().map_or(0, |s| s.size_of().total_bytes()),
            "rf2_discovery_full_scans": 0,
            "cache_bytes": config.cache_bytes, "memory_pressure_target_bytes": config.memory_target_bytes,
            "circuits": {
                "base": {"storage_path": base_storage, "cache_mib": base_cache_mib},
                "q3": {"storage_path": q3_storage, "cache_mib": q3_cache_mib},
                "note": "Two runtimes, each with one storage directory and one buffer cache. cache_bytes is split between them; both get the same memory_pressure_target_bytes, because RSS is process-wide.",
            },
            "cache": "feldera_cache", "compression": "none", "min_storage_bytes": 0,
            "min_step_storage_bytes": "default (unlimited; memory-pressure spill remains enabled)",
            "native_join_fetch": config.r5, "compaction_timeout_secs": config.compaction_timeout_secs,
            "compaction_policy": if config.setup_full_compaction {
                "setup full compaction after loading, before the refreshes; no compaction requested after any refresh; no final wait"
            } else {
                "no forced compaction; loading waits for normal background merges only; no final wait"
            },
            "setup_full_compaction": config.setup_full_compaction,
            "front_code_byte_keys": config.front_code_byte_keys,
            "elide_value_headers": config.elide_value_headers,
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
            // The sums over both runtimes; the breakdown is under `circuits`.
            "cache_occupancy_bytes": base_cache_occupancy + q3_cache_occupancy,
            "cache_occupancy_at_window_start_bytes": base_cache_occupancy_at_start + q3_cache_occupancy_at_start,
            "snapshot_publication_transactions": 0,
            "cache_capacity_bytes": base_cache_capacity + q3_cache_capacity,
            "circuits": {
                "base": {
                    "observed_storage_bytes": base_storage_bytes,
                    "cache_occupancy_bytes": base_cache_occupancy,
                    "cache_occupancy_at_window_start_bytes": base_cache_occupancy_at_start,
                    "cache_capacity_bytes": base_cache_capacity,
                },
                "q3": {
                    "observed_storage_bytes": q3_storage_bytes,
                    "cache_occupancy_bytes": q3_cache_occupancy,
                    "cache_occupancy_at_window_start_bytes": q3_cache_occupancy_at_start,
                    "cache_capacity_bytes": q3_cache_capacity,
                },
            },
            "window_started_micros": window_started_micros, "window_finished_micros": window_finished_micros,
            "resources": if config.verify { json!({"unavailable": "independent validation enabled; counters omitted to exclude validation"}) } else { sequence_resources.delta(&resources_after) },
            "manual_index_file_backed": manual_file_backed,
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
    fn rejects_missing_placement_and_incomplete_rf2() -> Result<()> {
        let mut pipeline = Pipeline::with_workers(1)?;
        let mut reference = Reference::default();
        let no_order = Change {
            lines: vec![(line(20, 1), true)],
            ..Change::default()
        };
        assert!(workload::BaseState::new(&pipeline.base.catalog)?
            .validate(&no_order)
            .is_err());
        transact(
            &mut pipeline,
            &mut reference,
            Change {
                customers: vec![(customer(2), true)],
                orders: vec![(order(20, 2), true)],
                lines: vec![(line(20, 1), true)],
            },
            true,
            &mut Measurements::default(),
        )?;
        let missing_line = Change {
            orders: vec![(order(20, 2), false)],
            ..Change::default()
        };
        assert!(workload::BaseState::new(&pipeline.base.catalog)?
            .validate(&missing_line)
            .is_err());
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

    #[test]
    fn checkpoint_restores_q3_and_manual_index_at_completed_boundary() -> Result<()> {
        let path = std::env::temp_dir().join(format!(
            "q3-reconstructed-checkpoint-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ));
        // A storage directory belongs to one runtime, so each circuit gets its own.
        let base_directory = path.join("base");
        let q3_directory = path.join("q3");
        std::fs::create_dir_all(&base_directory)?;
        std::fs::create_dir_all(&q3_directory)?;
        let config = |directory: &Path, checkpoint| -> Result<CircuitConfig> {
            let storage = StorageConfig {
                path: directory.to_string_lossy().into_owned(),
                cache: StorageCacheConfig::default(),
            };
            let options = StorageOptions {
                min_storage_bytes: Some(0),
                ..StorageOptions::default()
            };
            Ok(CircuitConfig::with_workers(1).with_storage(Some(
                CircuitStorageConfig::for_config(storage, options)?
                    .with_init_checkpoint(checkpoint),
            )))
        };
        let mut pipeline =
            Pipeline::new(config(&base_directory, None)?, config(&q3_directory, None)?)?;
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
        transact(
            &mut pipeline,
            &mut reference,
            Change {
                orders: vec![(order(20, 2), true), (order(30, 2), true)],
                lines: vec![(line(20, 1), true), (line(30, 1), true)],
                ..Change::default()
            },
            true,
            &mut stats,
        )?;
        let restored_reference = reference.clone();
        let base_committer = pipeline.base.circuit.checkpoint().prepare()?;
        let q3_committer = pipeline.view.circuit.checkpoint().prepare()?;
        let (base_checkpoint, q3_checkpoint) = (base_committer.uuid(), q3_committer.uuid());
        // The index lives in the base runtime, so its files belong to the base checkpoint.
        pipeline.index.save(base_checkpoint.to_string().into())?;
        base_committer.commit()?.publish()?;
        q3_committer.commit()?.publish()?;
        drop(pipeline);

        let mut pipeline = Pipeline::new(
            config(&base_directory, Some(base_checkpoint))?,
            config(&q3_directory, Some(q3_checkpoint))?,
        )?;
        pipeline.index.restore(base_checkpoint.to_string().into())?;
        let mut reference = restored_reference;
        let mut stats = Measurements::default();
        let deletion = rf2_changes(&reference, &HashSet::from([20]));
        transact(&mut pipeline, &mut reference, deletion, true, &mut stats)?;
        assert_eq!(stats.view_rows, Some(1));
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
            memory_target_bytes: Some(512 * 1024 * 1024),
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
        // Two runtimes, each with its own storage directory and its share of the cache.
        let circuits = &output["configuration"]["circuits"];
        assert_eq!(
            circuits["base"]["cache_mib"].as_u64().unwrap()
                + circuits["q3"]["cache_mib"].as_u64().unwrap(),
            8
        );
        assert_ne!(
            circuits["base"]["storage_path"],
            circuits["q3"]["storage_path"]
        );
        let by_circuit = &output["sequence"]["circuits"];
        assert!(
            by_circuit["base"]["observed_storage_bytes"]
                .as_u64()
                .unwrap()
                > 0
        );
        assert!(by_circuit["q3"]["observed_storage_bytes"].as_u64().unwrap() > 0);
        assert_eq!(
            output["sequence"]["cache_capacity_bytes"].as_u64().unwrap(),
            by_circuit["base"]["cache_capacity_bytes"].as_u64().unwrap()
                + by_circuit["q3"]["cache_capacity_bytes"].as_u64().unwrap()
        );
        assert!(
            output["sequence"]["observed_storage_bytes"]
                .as_u64()
                .unwrap()
                > 0
        );
        assert_eq!(output["measurement_version"], 4);
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
        assert!(
            output["load"]["catalog_storage"]["CUSTOMER"]["memory_batch_count"]
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
                for runtime in ["base_runtime", "q3_runtime"] {
                    assert!(
                        refresh["memory_pressure"][side][runtime].is_string(),
                        "{refresh}"
                    );
                }
            }
        }
        assert!(sequence["cache_occupancy_at_window_start_bytes"]
            .as_u64()
            .is_some());
        // Verification I/O and CPU counters are omitted from performance data.
        assert!(output["sequence"]["resources"]["unavailable"].is_string());
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
        assert!(lines["storage_batch_count"].as_u64().unwrap() > 0);
        assert!(lines["memory_batch_count"].as_u64().unwrap() > 0);
        assert_eq!(lines["file_backed"], false);
        #[cfg(feature = "merged-index")]
        assert!(
            single["sequence"]["manual_index_storage"]["memory_batch_count"]
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
        change.apply_optional(None, &mut pipeline.base.inputs)?;
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
        let pipeline = Pipeline::with_workers(1)?;
        assert_eq!(pipeline.base.inputs.deletes, DeleteStyle::ByKey);
        assert_eq!(pipeline.view.inputs.deletes, DeleteStyle::WholeRow);
        assert!(!pipeline.base.inputs.whole_row(false));
        assert!(pipeline.base.inputs.whole_row(true));
        assert!(pipeline.view.inputs.whole_row(false));
        Ok(())
    }
}
