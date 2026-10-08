mod data;
mod measurement;
#[cfg(feature = "merged-index")]
mod reconstruction;
#[cfg(test)]
mod verification;
mod workload;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use data::{Customer, Line, Order};
use dbsp::circuit::{
    CircuitConfig, CircuitStorageConfig, StorageCacheConfig, StorageConfig, StorageOptions,
};
use dbsp::monitor::visual_graph::CircuitNodeStyle;
use dbsp_adapters::{CircuitCatalog, RecordFormat};
use feldera_types::program_schema::SqlIdentifier;
#[cfg(feature = "merged-index")]
use merged_index_col::q3::Q3State;
#[cfg(feature = "merged-index")]
use reconstruction::ManualIndex;
use serde_json::{json, Value};
use size_of::SizeOf;

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

    #[cfg(test)]
    fn apply(self, state: &mut Reference, input: &mut Inputs) -> Result<()> {
        self.apply_optional(Some(state), input)
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

struct Inputs {
    customers: Box<dyn dbsp_adapters::DeCollectionStream>,
    orders: Box<dyn dbsp_adapters::DeCollectionStream>,
    lines: Box<dyn dbsp_adapters::DeCollectionStream>,
}

impl Inputs {
    fn new(catalog: &impl CircuitCatalog) -> Result<Self> {
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

    fn customer(&mut self, row: &Customer, insert: bool) -> Result<()> {
        Self::submit(
            &mut *self.customers,
            if insert {
                row.json()
            } else {
                json!({"c_custkey": row.key})
            },
            insert,
        )
    }
    fn order(&mut self, row: &Order, insert: bool) -> Result<()> {
        Self::submit(
            &mut *self.orders,
            if insert {
                row.json()
            } else {
                json!({"o_orderkey": row.key})
            },
            insert,
        )
    }
    fn line(&mut self, row: &Line, insert: bool) -> Result<()> {
        Self::submit(
            &mut *self.lines,
            if insert {
                row.json()
            } else {
                json!({"l_orderkey": row.order_key, "l_linenumber": row.number})
            },
            insert,
        )
    }
    fn flush(&mut self) {
        self.customers.flush();
        self.orders.flush();
        self.lines.flush();
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
    preparation: Duration,
    native: Duration,
    index: Duration,
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

#[cfg_attr(
    feature = "merged-index",
    expect(
        clippy::too_many_arguments,
        reason = "coordinates the circuit, optional validation, and external index owners"
    )
)]
fn transact_inner(
    circuit: &mut dbsp::DBSPHandle,
    catalog: &impl CircuitCatalog,
    input: &mut Inputs,
    mut state: Option<&mut Reference>,
    change: Change,
    check: Option<(&workload::BaseState, bool)>,
    stats: &mut Measurements,
    #[cfg(feature = "merged-index")] reconstruction: (&ManualIndex, &Q3State),
) -> Result<()> {
    #[cfg(feature = "merged-index")]
    let (index, q3_state) = reconstruction;
    let verification_started = Instant::now();
    let before = state
        .as_ref()
        .map(|state| state.evaluate())
        .unwrap_or_default();
    let reference_change = state.as_ref().map(|_| change.clone());
    stats.verification += verification_started.elapsed();
    let foreground_started = Instant::now();
    stats.rows += change.len();
    let preparation_started = Instant::now();
    if let Some((base, discovered)) = check {
        if discovered {
            base.validate_discovered(&change)?;
        } else {
            base.validate(&change)?;
        }
    }
    #[cfg(feature = "merged-index")]
    let (placements, index_rows) = reconstruction::prepare(&change)?;
    #[cfg(feature = "merged-index")]
    {
        stats.preparation += preparation_started.elapsed();
    }
    #[cfg(feature = "merged-index")]
    {
        let index_started = Instant::now();
        let (before_index, after_index) = index.append(index_rows)?;
        stats.index += index_started.elapsed();
        let publish_started = Instant::now();
        q3_state
            .publish(before_index, after_index, placements)
            .map_err(anyhow::Error::msg)?;
        stats.publication_cleanup += publish_started.elapsed();
    }
    #[cfg(feature = "merged-index")]
    let preparation_started = Instant::now();
    change.apply_optional(None, input)?;
    stats.preparation += preparation_started.elapsed();
    let native_started = Instant::now();
    let transaction_result = circuit.transaction();
    let latency = native_started.elapsed();
    #[cfg(feature = "merged-index")]
    let finish_result = {
        let cleanup_started = Instant::now();
        let result = q3_state.finish().map_err(anyhow::Error::msg);
        stats.publication_cleanup += cleanup_started.elapsed();
        result
    };
    transaction_result?;
    #[cfg(feature = "merged-index")]
    finish_result?;
    stats.native += latency;
    stats.latencies.push(latency);
    stats.transactions += 1;
    stats.foreground += foreground_started.elapsed();
    let verification_started = Instant::now();
    if let (Some(state), Some(change)) = (state.as_deref_mut(), reference_change) {
        // Reference maintenance belongs exclusively to independent validation.
        update_reference(state, change);
        let after = state.evaluate();
        let output = catalog
            .output_handles(&SqlIdentifier::from("V"))
            .context("missing V output")?;
        let actual_delta = read_output(&*output.delta_handle)?;
        let expected_delta = difference(&before, &after);
        if actual_delta != expected_delta {
            bail!("emitted delta differs from independent full join: actual {actual_delta:?}, expected {expected_delta:?}");
        }
        let snapshot = output
            .integrate_handle
            .as_ref()
            .context("V has no materialized snapshot")?;
        let actual_state = read_output(&**snapshot)?;
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
        let output = catalog
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
#[cfg_attr(
    feature = "merged-index",
    expect(
        clippy::too_many_arguments,
        reason = "test helper preserves existing circuit transaction fixtures"
    )
)]
fn transact(
    circuit: &mut dbsp::DBSPHandle,
    catalog: &impl CircuitCatalog,
    input: &mut Inputs,
    state: &mut Reference,
    change: Change,
    verify: bool,
    stats: &mut Measurements,
    #[cfg(feature = "merged-index")] reconstruction: (&ManualIndex, &Q3State),
) -> Result<()> {
    transact_inner(
        circuit,
        catalog,
        input,
        verify.then_some(state),
        change,
        None,
        stats,
        #[cfg(feature = "merged-index")]
        reconstruction,
    )
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
                "--workers" => {
                    config.workers = args.next().context("--workers needs number")?.parse()?
                }
                "--batch" => config.batch = args.next().context("--batch needs number")?.parse()?,
                "--runs" => config.runs = args.next().context("--runs needs number")?.parse()?,
                "--no-verify" => config.verify = false,
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

fn drain(
    circuit: &mut dbsp::DBSPHandle,
    timeout: Duration,
    #[cfg(feature = "merged-index")] index: &ManualIndex,
) -> Result<Duration> {
    let started = Instant::now();
    circuit.start_compaction()?;
    #[cfg(feature = "merged-index")]
    index.start_compaction()?;
    measurement::drain_until(timeout.saturating_sub(started.elapsed()), || {
        let complete = circuit.is_compaction_complete()?;
        #[cfg(feature = "merged-index")]
        let complete = complete && index.is_compaction_complete()?;
        Ok(complete)
    })?;
    Ok(started.elapsed())
}

/// Publish compacted batches to Catalog without changing logical input.
/// The extra transaction and mailbox cleanup belong to compaction drainage.
fn settle(
    circuit: &mut dbsp::DBSPHandle,
    catalog: &impl CircuitCatalog,
    base: &mut workload::BaseState,
    timeout: Duration,
    #[cfg(feature = "merged-index")] reconstruction: (&ManualIndex, &Q3State),
) -> Result<Duration> {
    let started = Instant::now();
    #[cfg(feature = "merged-index")]
    let (index, q3_state) = reconstruction;
    drain(
        circuit,
        timeout,
        #[cfg(feature = "merged-index")]
        index,
    )?;
    for name in ["CUSTOMER", "ORDERS", "LINEITEM", "V"] {
        let output = catalog
            .output_handles(&SqlIdentifier::from(name))
            .context("missing output")?;
        drop(
            output
                .integrate_handle
                .as_ref()
                .context("missing integrated output")?
                .take_from_all(),
        );
    }
    #[cfg(feature = "merged-index")]
    {
        let snapshot = index.snapshot()?;
        q3_state
            .publish(snapshot.clone(), snapshot, BTreeMap::new())
            .map_err(anyhow::Error::msg)?;
    }
    let transaction = circuit.transaction();
    #[cfg(feature = "merged-index")]
    let finish = q3_state.finish().map_err(anyhow::Error::msg);
    transaction?;
    #[cfg(feature = "merged-index")]
    finish?;
    *base = workload::BaseState::new(catalog)?;
    for name in ["CUSTOMER", "ORDERS", "LINEITEM", "V", "ERROR_VIEW"] {
        let output = catalog
            .output_handles(&SqlIdentifier::from(name))
            .context("missing output")?;
        drop(output.delta_handle.concat());
    }
    drain(
        circuit,
        timeout.saturating_sub(started.elapsed()),
        #[cfg(feature = "merged-index")]
        index,
    )?;
    Ok(started.elapsed())
}

fn phase_json(stats: &Measurements) -> Value {
    json!({
        "rows": stats.rows,
        "transactions": stats.transactions,
        "discovery_ms": milliseconds(stats.discovery),
        "checks_input_preparation_ms": milliseconds(stats.preparation),
        "index_ms": milliseconds(stats.index),
        "q3_ms": milliseconds(stats.native),
        "publication_cleanup_ms": milliseconds(stats.publication_cleanup),
        "foreground_ms": milliseconds(stats.foreground),
        "combined_maintenance_ms": milliseconds(stats.index + stats.native),
        "combined_maintenance_note": "index_ms + q3_ms excludes discovery, preparation and publication/cleanup",
        "verification_ms_excluded": milliseconds(stats.verification),
    })
}

/// Inspect the actual batches behind Catalog snapshots, without scanning rows.
fn storage_evidence(base: &workload::BaseState, catalog: &impl CircuitCatalog) -> Result<Value> {
    let mut evidence = base.storage_evidence();
    let reader = catalog
        .output_handles(&SqlIdentifier::from("V"))
        .context("missing V materialization")?
        .integrate_handle
        .as_ref()
        .context("missing integrated V output")?
        .concat();
    evidence.insert("V".into(), workload::batch_storage_evidence(&*reader));
    Ok(Value::Object(evidence))
}

fn run(config: &Config, run_number: usize) -> Result<()> {
    let started = Instant::now();
    let timeout = Duration::from_secs_f64(config.compaction_timeout_secs);
    let mut circuit_config =
        CircuitConfig::with_workers(config.workers).with_max_rss_bytes(config.memory_target_bytes);
    circuit_config.dev_tweaks.fetch_join = Some(config.r5);
    let storage_path = config
        .storage_path
        .as_ref()
        .map(|root| root.join(format!("run-{run_number}")));
    if let Some(path) = &storage_path {
        if path.exists() {
            bail!("storage directory already exists: {}", path.display());
        }
        std::fs::create_dir_all(path)?;
        circuit_config = circuit_config.with_storage(Some(CircuitStorageConfig::for_config(
            StorageConfig {
                path: path.to_string_lossy().into_owned(),
                cache: StorageCacheConfig::FelderaCache,
            },
            StorageOptions {
                min_storage_bytes: Some(0),
                cache_mib: config.cache_bytes.map(|bytes| bytes / (1024 * 1024)),
                compression: feldera_types::config::StorageCompression::None,
                ..StorageOptions::default()
            },
        )?));
    }
    #[cfg(feature = "merged-index")]
    let q3_state = Q3State::default();
    #[cfg(feature = "merged-index")]
    let (mut circuit, catalog) = q3_ivm_baseline::circuit(circuit_config, q3_state.clone())?;
    #[cfg(not(feature = "merged-index"))]
    let (mut circuit, catalog) = q3_ivm_baseline::circuit(circuit_config)?;
    let runtime = circuit.runtime().clone();
    runtime.with_buffer_cache(0, || {
    #[cfg(feature = "merged-index")]
    let index = ManualIndex::new(circuit.runtime());
    let sampler = storage_path
        .clone()
        .map(measurement::StorageSampler::start)
        .transpose()?;
    let mut inputs = Inputs::new(&catalog)?;
    let mut state = config.verify.then(Reference::default);
    let mut stats = Measurements::default();
    let mut base = workload::BaseState::new(&catalog)?;
    let load = workload::load_batches(
        config.customer.as_deref().context("--customer required")?,
        config.orders.as_deref().context("--orders required")?,
        config.lineitem.as_deref().context("--lineitem required")?,
        config.batch,
        |batch| {
            transact_inner(
                &mut circuit,
                &catalog,
                &mut inputs,
                state.as_mut(),
                batch,
                Some((&base, false)),
                &mut stats,
                #[cfg(feature = "merged-index")]
                (&index, &q3_state),
            )?;
            base = workload::BaseState::new(&catalog)?;
            Ok(())
        },
    )?;
    let load_drain = settle(
        &mut circuit,
        &catalog,
        &mut base,
        timeout,
        #[cfg(feature = "merged-index")]
        (&index, &q3_state),
    )?;
    let load_storage = storage_evidence(&base, &catalog)?;
    if config.r5
        && load_storage
            .as_object()
            .context("storage evidence object")?
            .values()
            .any(|e| e["file_backed"] != true)
    {
        bail!("loading left Catalog state in memory: {load_storage}");
    }
    #[cfg(feature = "merged-index")]
    if config.r5 && !index.is_file_backed()? {
        bail!("loading left the manual index in memory");
    }
    let load_peak_storage_bytes = sampler.map(measurement::StorageSampler::stop).transpose()?;
    let load_elapsed = started.elapsed();
    let load_rows = load.customer_rows + load.order_rows + load.line_rows;
    println!("run={run_number} phase=load rows={load_rows} transactions={} verified={} q3_ms={:.3} prep_ms={:.3} compaction_ms={:.3} end_to_end_ms={:.3} driver_reference_bytes={} driver_max_load_retained_rows={} peak_process_rss_bytes={} {}",
        stats.transactions, config.verify, milliseconds(stats.native), milliseconds(stats.preparation),
        milliseconds(load_drain), milliseconds(load_elapsed), state.as_ref().map_or(0, |s| s.size_of().total_bytes()), load.max_retained_rows, rss_bytes(), latency_summary(&stats.latencies));

    let refresh_list = refreshes(config)?;
    let sampler = storage_path
        .clone()
        .map(measurement::StorageSampler::start)
        .transpose()?;
    let sequence_resources = measurement::ResourceSnapshot::capture()?;
    let sequence_started = Instant::now();
    let mut reports = Vec::new();
    let mut verification_time = Duration::ZERO;
    let mut foreground_total = Duration::ZERO;
    for (number, refresh) in refresh_list.into_iter().enumerate() {
        let resources_before = measurement::ResourceSnapshot::capture()?;
        let phase_started = Instant::now();
        let mut measurement = Measurements::default();
        let (change, absent, phase, discovered) = if let Some(path) = refresh.rf2 {
            let keys: HashSet<_> = rows(&path, data::delete_order_key)?.into_iter().collect();
            #[cfg(feature = "merged-index")]
            let snapshot = index.snapshot()?;
            let (change, absent) = workload::rf2_changes(&base, &keys, |order| {
                #[cfg(feature = "merged-index")]
                {
                    reconstruction::lines(&snapshot, order)
                }
                #[cfg(not(feature = "merged-index"))]
                {
                    base.native_lines(order.key)
                }
            })?;
            measurement.discovery += phase_started.elapsed();
            (change, absent, "rf2", true)
        } else {
            let change = Change {
                orders: rows(
                    refresh
                        .rf1_orders
                        .as_deref()
                        .context("RF1 Orders required")?,
                    Order::parse,
                )?
                .into_iter()
                .map(|row| (row, true))
                .collect(),
                lines: rows(
                    refresh
                        .rf1_lines
                        .as_deref()
                        .context("RF1 Lineitem required")?,
                    Line::parse,
                )?
                .into_iter()
                .map(|row| (row, true))
                .collect(),
                ..Change::default()
            };
            measurement.preparation += phase_started.elapsed();
            (change, 0, "rf1", false)
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
            &mut circuit,
            &catalog,
            &mut inputs,
            state.as_mut(),
            change,
            Some((&base, discovered)),
            &mut measurement,
            #[cfg(feature = "merged-index")]
            (&index, &q3_state),
        )?;
        let cleanup_started = Instant::now();
        base = workload::BaseState::new(&catalog)?;
        drop(customers);
        drop(fanout);
        measurement.publication_cleanup += cleanup_started.elapsed();
        // Wall time covers file reads, discovery and all foreground cleanup.
        measurement.foreground = phase_started
            .elapsed()
            .saturating_sub(measurement.verification);
        verification_time += measurement.verification;
        foreground_total += measurement.foreground;
        let resource_delta = resources_before.delta(&measurement::ResourceSnapshot::capture()?);
        let mut report = phase_json(&measurement);
        report["refresh"] = json!(number + 1);
        report["phase"] = json!(phase);
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
    let final_drain = settle(
        &mut circuit,
        &catalog,
        &mut base,
        timeout,
        #[cfg(feature = "merged-index")]
        (&index, &q3_state),
    )?;
    let sequence_elapsed = sequence_started.elapsed().saturating_sub(verification_time);
    let resources_after = measurement::ResourceSnapshot::capture()?;
    let settled_storage_bytes = storage_path
        .as_deref()
        .map(measurement::storage_size_bytes)
        .transpose()?;
    let sampled_peak_storage_bytes = sampler.map(measurement::StorageSampler::stop).transpose()?;
    let (cache_occupancy, cache_capacity) = circuit.runtime().cache_occupancy();
    #[cfg(feature = "merged-index")]
    let manual_file_backed = index.is_file_backed()?;
    #[cfg(not(feature = "merged-index"))]
    let manual_file_backed: Option<bool> = None;
    #[cfg(feature = "merged-index")]
    if config.r5 && !manual_file_backed {
        bail!("manual merged index is not file-backed");
    }
    let profile = circuit.retrieve_profile()?;
    let graph = circuit.graph_profile()?.worker_graphs[0].to_json();
    if cfg!(feature = "merged-index") && graph.contains("shard_accumulate_trace") {
        bail!("replaced historical join trace remains in reconstructed circuit");
    }
    if let Some(path) = &config.report {
        std::fs::write(path.with_extension("profile.json"), profile.as_json())?;
        std::fs::write(path.with_extension("graph.json"), &graph)?;
    }
    let storage_evidence = storage_evidence(&base, &catalog)?;
    if config.r5
        && storage_evidence
            .as_object()
            .context("storage evidence object")?
            .values()
            .any(|e| e["file_backed"] != true)
    {
        bail!("Catalog base state or V is not file-backed: {storage_evidence}");
    }
    let report = json!({
        "status": "complete",
        "mode": if cfg!(feature = "merged-index") { "reconstructed" } else { "native" },
        "qualification": "runner determines cgroup qualification; macOS is smoke only",
        "configuration": {
            "workers": config.workers, "load_batch_target": config.batch, "verification": config.verify,
            "driver_reference_bytes": state.as_ref().map_or(0, |s| s.size_of().total_bytes()),
            "rf2_discovery_full_scans": 0,
            "cache_bytes": config.cache_bytes, "memory_pressure_target_bytes": config.memory_target_bytes,
            "cache": "feldera_cache", "compression": "none", "min_storage_bytes": 0,
            "min_step_storage_bytes": "default (unlimited; memory-pressure spill remains enabled)",
            "native_join_fetch": config.r5, "compaction_timeout_secs": config.compaction_timeout_secs,
            "durability": "no checkpoint; refresh completion is not a durability guarantee",
        },
        "load": {"rows": load_rows, "transactions": load.transactions, "max_retained_rows": load.max_retained_rows,
            "catalog_storage": load_storage, "snapshot_publication_transactions": 1,
            "sampled_peak_storage_bytes": load_peak_storage_bytes,
            "compaction_drain_ms": milliseconds(load_drain), "elapsed_ms": milliseconds(load_elapsed)},
        "refreshes": reports,
        "sequence": {
            "foreground_ms": milliseconds(foreground_total), "final_compaction_drain_ms": milliseconds(final_drain),
            "end_to_end_ms": milliseconds(sequence_elapsed), "settled_storage_bytes": settled_storage_bytes,
            "sampled_peak_storage_bytes": sampled_peak_storage_bytes, "storage_peak_sampling_ms": 10,
            "cache_occupancy_bytes": cache_occupancy, "snapshot_publication_transactions": 1, "cache_capacity_bytes": cache_capacity,
            "resources": if config.verify { json!({"unavailable": "independent validation enabled; counters omitted to exclude validation"}) } else { sequence_resources.delta(&resources_after) },
            "manual_index_file_backed": manual_file_backed,
            "catalog_storage": storage_evidence,
            "replaced_historical_join_traces_absent": if cfg!(feature = "merged-index") { Some(true) } else { None },
            "circuit_profile": serde_json::to_value(profile)?,
        },
    });
    if let Some(path) = &config.report {
        std::fs::write(path, serde_json::to_vec_pretty(&report)?)?;
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
        let capture_result = dump_runtime_circuit(&mut circuit, &directory);
        if directory.is_dir() {
            std::fs::write(
                directory.join("manifest.json"),
                serde_json::to_vec_pretty(&manifest)?,
            )?;
        }
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
        let (mut circuit, catalog) = q3_ivm_baseline::circuit(CircuitConfig::with_workers(1))?;
        let mut input = Inputs::new(&catalog)?;
        let mut state = Reference::default();
        let mut stats = Measurements::default();
        let mut step = |change| {
            transact(
                &mut circuit,
                &catalog,
                &mut input,
                &mut state,
                change,
                true,
                &mut stats,
            )
        };

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
            let (mut circuit, catalog) =
                q3_ivm_baseline::circuit(CircuitConfig::with_workers(workers))?;
            let mut input = Inputs::new(&catalog)?;
            let mut state = Reference::default();
            let mut stats = Measurements::default();
            transact(
                &mut circuit,
                &catalog,
                &mut input,
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
            dump_runtime_circuit(&mut circuit, &directory)?;
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
            assert!(dump_runtime_circuit(&mut circuit, &directory).is_err());
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
        let (mut circuit, catalog) = q3_ivm_baseline::circuit(CircuitConfig::with_workers(2))?;
        let mut input = Inputs::new(&catalog)?;
        let mut state = Reference::default();
        let mut stats = Measurements::default();
        transact(
            &mut circuit,
            &catalog,
            &mut input,
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
            let (mut circuit, catalog) =
                q3_ivm_baseline::circuit(CircuitConfig::with_workers(workers))?;
            let mut input = Inputs::new(&catalog)?;
            let mut state = Reference::default();
            let mut stats = Measurements::default();
            let base_customer = customer(2, "BUILDING");
            let base_order = order(20, 2);
            let base_line = line(20, 1);
            transact(
                &mut circuit,
                &catalog,
                &mut input,
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
                &mut circuit,
                &catalog,
                &mut input,
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
        let q3_state = Q3State::default();
        let (mut circuit, catalog) =
            q3_ivm_baseline::circuit(CircuitConfig::with_workers(1), q3_state.clone())?;
        let index = ManualIndex::new(circuit.runtime());
        let mut input = Inputs::new(&catalog)?;
        let mut state = Reference::default();
        let mut stats = Measurements::default();
        transact(
            &mut circuit,
            &catalog,
            &mut input,
            &mut state,
            Change {
                customers: vec![(customer(2), true), (customer(3), true)],
                ..Change::default()
            },
            true,
            &mut stats,
            (&index, &q3_state),
        )?;
        transact(
            &mut circuit,
            &catalog,
            &mut input,
            &mut state,
            Change {
                orders: vec![(order(20, 2), true)],
                lines: vec![(line(20, 1), true), (line(20, 2), true)],
                ..Change::default()
            },
            true,
            &mut stats,
            (&index, &q3_state),
        )?;
        transact(
            &mut circuit,
            &catalog,
            &mut input,
            &mut state,
            Change {
                orders: vec![(order(30, 3), true)],
                lines: vec![(line(30, 1), true)],
                ..Change::default()
            },
            true,
            &mut stats,
            (&index, &q3_state),
        )?;
        let keys = HashSet::from([20]);
        let deletion = rf2_changes(&state, &keys);
        transact(
            &mut circuit,
            &catalog,
            &mut input,
            &mut state,
            deletion,
            true,
            &mut stats,
            (&index, &q3_state),
        )?;
        transact(
            &mut circuit,
            &catalog,
            &mut input,
            &mut state,
            Change::default(),
            true,
            &mut stats,
            (&index, &q3_state),
        )?;
        assert_eq!(stats.view_rows, Some(1));
        assert_eq!(stats.transactions, 5);
        let profile = circuit.graph_profile()?;
        let graph = profile.worker_graphs[0].to_json();
        assert!(!graph.contains("shard_accumulate_trace"));
        assert!(graph.contains("JoinTrace"));
        Ok(())
    }

    #[test]
    fn rejects_missing_placement_and_incomplete_rf2() -> Result<()> {
        let q3_state = Q3State::default();
        let (mut circuit, catalog) =
            q3_ivm_baseline::circuit(CircuitConfig::with_workers(1), q3_state.clone())?;
        let index = ManualIndex::new(circuit.runtime());
        let mut input = Inputs::new(&catalog)?;
        let mut reference = Reference::default();
        let no_order = Change {
            lines: vec![(line(20, 1), true)],
            ..Change::default()
        };
        assert!(workload::BaseState::new(&catalog)?
            .validate(&no_order)
            .is_err());
        transact(
            &mut circuit,
            &catalog,
            &mut input,
            &mut reference,
            Change {
                customers: vec![(customer(2), true)],
                orders: vec![(order(20, 2), true)],
                lines: vec![(line(20, 1), true)],
            },
            true,
            &mut Measurements::default(),
            (&index, &q3_state),
        )?;
        let missing_line = Change {
            orders: vec![(order(20, 2), false)],
            ..Change::default()
        };
        assert!(workload::BaseState::new(&catalog)?
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
        let q3_state = Q3State::default();
        let (mut circuit, catalog) =
            q3_ivm_baseline::circuit(CircuitConfig::with_workers(1), q3_state.clone())?;
        let index = ManualIndex::new(circuit.runtime());
        let mut input = Inputs::new(&catalog)?;
        let mut reference = Reference::default();
        let mut stats = Measurements::default();
        transact(
            &mut circuit,
            &catalog,
            &mut input,
            &mut reference,
            Change {
                customers: vec![(customer(2), true)],
                ..Change::default()
            },
            true,
            &mut stats,
            (&index, &q3_state),
        )?;

        let complete = Change {
            orders: vec![(order(20, 2), true)],
            lines: vec![(line(20, 1), true), (line(20, 2), true)],
            ..Change::default()
        };
        let before = reference.evaluate();
        let (placements, rows) = reconstruction::prepare(&complete)?;
        let (before_index, after_index) = index.append(rows)?;
        q3_state
            .publish(before_index, after_index, placements)
            .map_err(anyhow::Error::msg)?;
        circuit.start_transaction()?;
        Change {
            orders: complete.orders,
            ..Change::default()
        }
        .apply(&mut reference, &mut input)?;
        assert!(!circuit.step()?);
        Change {
            lines: complete.lines,
            ..Change::default()
        }
        .apply(&mut reference, &mut input)?;
        assert!(!circuit.step()?);
        circuit.commit_transaction()?;
        q3_state.finish().map_err(anyhow::Error::msg)?;

        let output = catalog
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
        std::fs::create_dir(&path)?;
        let storage = || StorageConfig {
            path: path.to_string_lossy().into_owned(),
            cache: StorageCacheConfig::default(),
        };
        let options = || StorageOptions {
            min_storage_bytes: Some(0),
            ..StorageOptions::default()
        };
        let initial = CircuitConfig::with_workers(1).with_storage(Some(
            CircuitStorageConfig::for_config(storage(), options())?,
        ));
        let q3_state = Q3State::default();
        let (mut circuit, catalog) = q3_ivm_baseline::circuit(initial, q3_state.clone())?;
        let index = ManualIndex::new(circuit.runtime());
        let mut inputs = Inputs::new(&catalog)?;
        let mut reference = Reference::default();
        let mut stats = Measurements::default();
        transact(
            &mut circuit,
            &catalog,
            &mut inputs,
            &mut reference,
            Change {
                customers: vec![(customer(2), true)],
                ..Change::default()
            },
            true,
            &mut stats,
            (&index, &q3_state),
        )?;
        transact(
            &mut circuit,
            &catalog,
            &mut inputs,
            &mut reference,
            Change {
                orders: vec![(order(20, 2), true), (order(30, 2), true)],
                lines: vec![(line(20, 1), true), (line(30, 1), true)],
                ..Change::default()
            },
            true,
            &mut stats,
            (&index, &q3_state),
        )?;
        let restored_reference = reference.clone();
        let committer = circuit.checkpoint().prepare()?;
        let checkpoint_id = committer.uuid();
        index.save(checkpoint_id.to_string().into())?;
        let publisher = committer.commit()?;
        publisher.publish()?;
        drop(inputs);
        drop(catalog);
        drop(index);
        drop(circuit);

        let restored = CircuitConfig::with_workers(1).with_storage(Some(
            CircuitStorageConfig::for_config(storage(), options())?
                .with_init_checkpoint(Some(checkpoint_id)),
        ));
        let restored_q3 = Q3State::default();
        let (mut circuit, catalog) = q3_ivm_baseline::circuit(restored, restored_q3.clone())?;
        let restored_index = ManualIndex::new(circuit.runtime());
        restored_index.restore(checkpoint_id.to_string().into())?;
        let mut inputs = Inputs::new(&catalog)?;
        let mut reference = restored_reference;
        let mut stats = Measurements::default();
        let deletion = rf2_changes(&reference, &HashSet::from([20]));
        transact(
            &mut circuit,
            &catalog,
            &mut inputs,
            &mut reference,
            deletion,
            true,
            &mut stats,
            (&restored_index, &restored_q3),
        )?;
        assert_eq!(stats.view_rows, Some(1));
        Ok(())
    }
}

#[cfg(test)]
mod measurement_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_RUN: AtomicUsize = AtomicUsize::new(0);

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
    fn file_backed_driver_loads_complete_groups_and_discovers_repeated_refreshes() -> Result<()> {
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
        std::fs::write(&orders, "1|1|O|1.00|1996-02-29|5-LOW|Clerk|0|c|\n")?;
        std::fs::write(
            &lines,
            "1|2|3|1|5|1.00|0.04|0.02|N|O|1996-03-13|1996-02-12|1996-03-22|i|m|c|\n",
        )?;
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
        assert!(
            output["sequence"]["settled_storage_bytes"]
                .as_u64()
                .unwrap()
                > 0
        );
        for evidence in output["sequence"]["catalog_storage"]
            .as_object()
            .unwrap()
            .values()
        {
            assert_eq!(evidence["file_backed"], true);
        }
        let foreground = output["sequence"]["foreground_ms"].as_f64().unwrap();
        let total = output["sequence"]["end_to_end_ms"].as_f64().unwrap();
        assert!(total >= foreground);
        // Verification I/O and CPU counters are omitted from performance data.
        assert!(output["sequence"]["resources"]["unavailable"].is_string());
        let config = Config {
            verify: false,
            storage_path: Some(directory.join("unverified-storage")),
            report: Some(directory.join("unverified-report.json")),
            ..config
        };
        run(&config, 1)?;
        let measured: Value = serde_json::from_reader(File::open(config.report.unwrap())?)?;
        assert_eq!(measured["configuration"]["driver_reference_bytes"], 0);
        assert!(measured["sequence"]["resources"]["backend_read_operations"].is_u64());
        assert!(measured["sequence"]["resources"]["cpu_microseconds"].is_u64());
        assert!(
            measured["sequence"]["sampled_peak_storage_bytes"]
                .as_u64()
                .unwrap()
                >= measured["sequence"]["settled_storage_bytes"]
                    .as_u64()
                    .unwrap()
        );
        Ok(())
    }
}
