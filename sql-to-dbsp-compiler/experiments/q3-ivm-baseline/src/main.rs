mod data;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use data::{Customer, Line, Order};
use dbsp::circuit::{
    CircuitConfig, CircuitStorageConfig, StorageCacheConfig, StorageConfig, StorageOptions,
};
use dbsp_adapters::{CircuitCatalog, RecordFormat};
use feldera_types::program_schema::SqlIdentifier;
use serde_json::{json, Value};
use size_of::SizeOf;

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
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

#[derive(Default, SizeOf)]
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

#[derive(Default)]
struct Change {
    customers: Vec<(Customer, bool)>,
    orders: Vec<(Order, bool)>,
    lines: Vec<(Line, bool)>,
}

impl Change {
    fn len(&self) -> usize {
        self.customers.len() + self.orders.len() + self.lines.len()
    }

    fn apply(self, state: &mut Reference, input: &mut Inputs) -> Result<()> {
        for (row, insert) in self.customers {
            input.customer(&row, insert)?;
            if insert {
                state.customers.insert(row.key, row);
            } else {
                state.customers.remove(&row.key);
            }
        }
        for (row, insert) in self.orders {
            input.order(&row, insert)?;
            if insert {
                state.orders.insert(row.key, row);
            } else {
                state.orders.remove(&row.key);
            }
        }
        for (row, insert) in self.lines {
            input.line(&row, insert)?;
            let key = (row.order_key, row.number);
            if insert {
                state.lines.insert(key, row);
            } else {
                state.lines.remove(&key);
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
    transactions: usize,
    rows: usize,
    latencies: Vec<Duration>,
    delta_positive: Option<i64>,
    delta_negative: Option<i64>,
    view_rows: Option<i64>,
}

fn transact(
    circuit: &mut dbsp::DBSPHandle,
    catalog: &impl CircuitCatalog,
    input: &mut Inputs,
    state: &mut Reference,
    change: Change,
    verify: bool,
    stats: &mut Measurements,
) -> Result<()> {
    let before = if verify {
        state.evaluate()
    } else {
        BTreeMap::new()
    };
    stats.rows += change.len();
    let preparation_started = Instant::now();
    change.apply(state, input)?;
    stats.preparation += preparation_started.elapsed();
    let native_started = Instant::now();
    circuit.transaction()?;
    let latency = native_started.elapsed();
    stats.native += latency;
    stats.latencies.push(latency);
    stats.transactions += 1;
    if verify {
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
        // Keep the output mailbox bounded without including deserialization in native time.
        let output = catalog
            .output_handles(&SqlIdentifier::from("V"))
            .context("missing V output")?;
        let _ = output.delta_handle.concat();
    }
    Ok(())
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

fn throughput(rows: usize, elapsed: Duration) -> f64 {
    if elapsed.is_zero() {
        0.0
    } else {
        rows as f64 / elapsed.as_secs_f64()
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
                "--workers" => {
                    config.workers = args.next().context("--workers needs number")?.parse()?
                }
                "--batch" => config.batch = args.next().context("--batch needs number")?.parse()?,
                "--runs" => config.runs = args.next().context("--runs needs number")?.parse()?,
                "--no-verify" => config.verify = false,
                other => bail!("unknown argument {other}"),
            }
        }
        if config.workers == 0 || config.batch == 0 || config.runs == 0 {
            bail!("workers, batch, and runs must be positive");
        }
        if config.rf1_orders.is_some() != config.rf1_lines.is_some() {
            bail!("RF1 orders and lines must be provided together");
        }
        Ok(config)
    }
}

fn run(config: &Config, run_number: usize) -> Result<()> {
    let started = Instant::now();
    let mut circuit_config = CircuitConfig::with_workers(config.workers);
    if let Some(root) = &config.storage_path {
        let path = root.join(format!("run-{run_number}"));
        if path.exists() {
            bail!("storage directory already exists: {}", path.display());
        }
        circuit_config = circuit_config.with_storage(Some(CircuitStorageConfig::for_config(
            StorageConfig {
                path: path.to_string_lossy().into_owned(),
                cache: StorageCacheConfig::default(),
            },
            StorageOptions {
                min_storage_bytes: Some(0),
                ..StorageOptions::default()
            },
        )?));
    }
    let (mut circuit, catalog) = q3_ivm_baseline::circuit(circuit_config)?;
    let mut inputs = Inputs::new(&catalog)?;
    let mut state = Reference::default();
    let mut stats = Measurements::default();

    let parse_started = Instant::now();
    let customers = rows(
        config.customer.as_deref().context("--customer required")?,
        Customer::parse,
    )?;
    let orders = rows(
        config.orders.as_deref().context("--orders required")?,
        Order::parse,
    )?;
    let lines = rows(
        config.lineitem.as_deref().context("--lineitem required")?,
        Line::parse,
    )?;
    let load = Change {
        customers: customers.into_iter().map(|row| (row, true)).collect(),
        orders: orders.into_iter().map(|row| (row, true)).collect(),
        lines: lines.into_iter().map(|row| (row, true)).collect(),
    };
    let load_rows = load.len();
    // Submit all related records in each transaction; the final partial batch is still stepped.
    let mut customers = load.customers.into_iter();
    let mut orders = load.orders.into_iter();
    let mut lines = load.lines.into_iter();
    stats.preparation += parse_started.elapsed();
    while customers.len() + orders.len() + lines.len() > 0 {
        let batch_started = Instant::now();
        let mut batch = Change::default();
        for _ in 0..config.batch {
            if let Some(row) = customers.next() {
                batch.customers.push(row);
            } else if let Some(row) = orders.next() {
                batch.orders.push(row);
            } else if let Some(row) = lines.next() {
                batch.lines.push(row);
            }
        }
        stats.preparation += batch_started.elapsed();
        transact(
            &mut circuit,
            &catalog,
            &mut inputs,
            &mut state,
            batch,
            config.verify,
            &mut stats,
        )?;
    }
    println!("run={run_number} phase=load rows={load_rows} transactions={} delta_plus={:?} delta_minus={:?} view_rows={:?} verified={} native_ms={:.3} native_rows_per_s={:.1} prep_ms={:.3} end_to_end_ms={:.3} driver_reference_bytes={} peak_process_rss_bytes={} {}",
        stats.transactions, stats.delta_positive, stats.delta_negative, stats.view_rows, config.verify,
        milliseconds(stats.native), throughput(load_rows, stats.native),
        milliseconds(stats.preparation), milliseconds(started.elapsed()), state.size_of().total_bytes(), rss_bytes(), latency_summary(&stats.latencies));

    if let (Some(order_path), Some(line_path)) = (&config.rf1_orders, &config.rf1_lines) {
        let phase = Instant::now();
        let mut change = Change::default();
        let mut measurement = Measurements::default();
        change.orders = rows(order_path, Order::parse)?
            .into_iter()
            .map(|row| (row, true))
            .collect();
        change.lines = rows(line_path, Line::parse)?
            .into_iter()
            .map(|row| (row, true))
            .collect();
        measurement.preparation += phase.elapsed();
        let count = change.len();
        transact(
            &mut circuit,
            &catalog,
            &mut inputs,
            &mut state,
            change,
            config.verify,
            &mut measurement,
        )?;
        println!("run={run_number} phase=rf1 rows={count} transactions=1 delta_plus={:?} delta_minus={:?} view_rows={:?} verified={} native_ms={:.3} native_rows_per_s={:.1} prep_ms={:.3} end_to_end_ms={:.3} driver_reference_bytes={} peak_process_rss_bytes={} {}",
            measurement.delta_positive, measurement.delta_negative, measurement.view_rows, config.verify,
            milliseconds(measurement.native), throughput(count, measurement.native),
            milliseconds(measurement.preparation), milliseconds(phase.elapsed()), state.size_of().total_bytes(), rss_bytes(), latency_summary(&measurement.latencies));
    }
    if let Some(path) = &config.rf2 {
        let phase = Instant::now();
        let keys: HashSet<_> = rows(path, data::delete_order_key)?.into_iter().collect();
        let change = rf2_changes(&state, &keys);
        let count = change.len();
        let mut measurement = Measurements::default();
        measurement.preparation += phase.elapsed();
        transact(
            &mut circuit,
            &catalog,
            &mut inputs,
            &mut state,
            change,
            config.verify,
            &mut measurement,
        )?;
        println!("run={run_number} phase=rf2 rows={count} transactions=1 delta_plus={:?} delta_minus={:?} view_rows={:?} verified={} native_ms={:.3} native_rows_per_s={:.1} prep_ms={:.3} end_to_end_ms={:.3} driver_reference_bytes={} peak_process_rss_bytes={} {}",
            measurement.delta_positive, measurement.delta_negative, measurement.view_rows, config.verify,
            milliseconds(measurement.native), throughput(count, measurement.native),
            milliseconds(measurement.preparation), milliseconds(phase.elapsed()), state.size_of().total_bytes(), rss_bytes(), latency_summary(&measurement.latencies));
    }
    Ok(())
}

fn main() -> Result<()> {
    let config = Config::parse()?;
    for run_number in 1..=config.runs {
        run(&config, run_number)?;
    }
    Ok(())
}

#[cfg(test)]
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
        ])
        .unwrap();
        assert_eq!((config.workers, config.batch, config.runs), (2, 13, 1));
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
