//! Bounded base loading and operational checks against maintained base tables.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Lines};
use std::path::Path;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use dbsp::dynamic::Erase;
use dbsp::utils::{Tup1, Tup2};
use dbsp_adapters::{CircuitCatalog, RecordFormat};
use feldera_adapterlib::catalog::SerBatchReader;
use feldera_types::program_schema::SqlIdentifier;
use serde_json::Value;

use crate::data::{Customer, Line, Order};
use crate::{cents, integer, string, Change};

pub(crate) struct BaseState {
    customers: Arc<dyn SerBatchReader>,
    orders: Arc<dyn SerBatchReader>,
    lines: Arc<dyn SerBatchReader>,
}

impl BaseState {
    pub(crate) fn new(catalog: &impl CircuitCatalog) -> Result<Self> {
        let snapshot = |name: &str| -> Result<Arc<dyn SerBatchReader>> {
            Ok(catalog
                .output_handles(&SqlIdentifier::from(name))
                .with_context(|| format!("missing base relation {name}"))?
                .integrate_handle
                .as_ref()
                .with_context(|| format!("base relation {name} is not materialized"))?
                .concat()
                .snapshot())
        };
        Ok(Self {
            customers: snapshot("CUSTOMER")?,
            orders: snapshot("ORDERS")?,
            lines: snapshot("LINEITEM")?,
        })
    }

    pub(crate) fn storage_evidence(&self) -> serde_json::Map<String, Value> {
        [
            ("CUSTOMER", &self.customers),
            ("ORDERS", &self.orders),
            ("LINEITEM", &self.lines),
        ]
        .into_iter()
        .map(|(name, reader)| (name.to_owned(), batch_storage_evidence(&**reader)))
        .collect()
    }

    fn keyed_row(
        reader: &dyn SerBatchReader,
        key: &dbsp::dynamic::DynData,
    ) -> Result<Option<Value>> {
        let mut cursor = reader.cursor(RecordFormat::Json(Default::default()))?;
        if !cursor.seek_key_exact(key) {
            return Ok(None);
        }
        if !cursor.val_valid() || cursor.weight() != 1 {
            bail!("base relation has invalid primary-key multiplicity");
        }
        let row = cursor.val_to_json()?;
        cursor.step_val();
        if cursor.val_valid() {
            bail!("base relation has multiple values for one primary key");
        }
        Ok(Some(row))
    }

    pub(crate) fn customer_exists(&self, key: i64) -> Result<bool> {
        Ok(Self::keyed_row(&*self.customers, Tup1(key).erase())?.is_some())
    }

    pub(crate) fn order(&self, key: i64) -> Result<Option<Order>> {
        Self::keyed_row(&*self.orders, Tup1(key).erase())?
            .map(|row| {
                Ok(Order {
                    key,
                    customer_key: integer(&row, "o_custkey")?,
                    date: string(&row, "o_orderdate")?,
                    ship_priority: i32::try_from(integer(&row, "o_shippriority")?)?,
                })
            })
            .transpose()
    }

    pub(crate) fn native_lines(&self, order_key: i64) -> Result<Vec<Line>> {
        let mut cursor = self.lines.cursor(RecordFormat::Json(Default::default()))?;
        cursor.seek_key(Tup2(order_key, i32::MIN).erase());
        let mut lines = Vec::new();
        while cursor.key_valid() {
            let key = cursor.key_to_json()?;
            if integer(&key, "l_orderkey")? != order_key {
                break;
            }
            if !cursor.val_valid() || cursor.weight() != 1 {
                bail!("Lineitem has invalid primary-key multiplicity");
            }
            let val = cursor.val_to_json()?;
            lines.push(Line {
                order_key,
                number: i32::try_from(integer(&key, "l_linenumber")?)?,
                price_cents: cents(&val, "l_extendedprice")?,
                discount_hundredths: cents(&val, "l_discount")?,
                ship_date: string(&val, "l_shipdate")?,
            });
            cursor.step_val();
            if cursor.val_valid() {
                bail!("Lineitem has multiple values for one primary key");
            }
            cursor.step_key();
        }
        Ok(lines)
    }

    /// Check the restricted refresh before either maintenance owner is changed.
    pub(crate) fn validate(&self, change: &Change) -> Result<()> {
        self.validate_inner(change, false)
    }

    /// Validate an RF2 delta assembled by `rf2_changes` from this snapshot.
    /// Discovery already established its current payloads and complete line ranges.
    pub(crate) fn validate_discovered(&self, change: &Change) -> Result<()> {
        if !change.customers.is_empty()
            || change.orders.iter().any(|(_, insert)| *insert)
            || change.lines.iter().any(|(_, insert)| *insert)
        {
            bail!("discovered RF2 must contain only deletions");
        }
        self.validate_inner(change, true)
    }

    fn validate_inner(&self, change: &Change, discovered_deletions: bool) -> Result<()> {
        let mut customers = HashSet::new();
        let mut orders = HashMap::new();
        let mut lines = HashMap::<i64, HashMap<i32, &Line>>::new();
        for (row, insert) in &change.customers {
            if !insert
                || !self.orders.is_empty()
                || !customers.insert(row.key)
                || self.customer_exists(row.key)?
            {
                bail!("only initial, distinct Customer inserts are supported");
            }
        }
        for (row, insert) in &change.orders {
            if orders.insert(row.key, *insert).is_some() {
                bail!("duplicate or replacement Orders delta for {}", row.key);
            }
            if *insert {
                if self.order(row.key)?.is_some()
                    || !(customers.contains(&row.customer_key)
                        || self.customer_exists(row.customer_key)?)
                {
                    bail!(
                        "RF1 Order {} exists or references a missing Customer",
                        row.key
                    );
                }
            } else if !discovered_deletions && self.order(row.key)?.as_ref() != Some(row) {
                bail!("RF2 Order {} is missing or has changed payload", row.key);
            }
        }
        for (row, insert) in &change.lines {
            let order_direction = orders.get(&row.order_key).with_context(|| {
                format!(
                    "Lineitem ({}, {}) has no matching Orders delta",
                    row.order_key, row.number
                )
            })?;
            if insert != order_direction {
                bail!(
                    "Lineitem ({}, {}) direction differs from Orders",
                    row.order_key,
                    row.number
                );
            }
            if lines
                .entry(row.order_key)
                .or_default()
                .insert(row.number, row)
                .is_some()
            {
                bail!(
                    "duplicate Lineitem identity ({}, {})",
                    row.order_key,
                    row.number
                );
            }
        }
        for (row, insert) in &change.orders {
            let changed = lines.get(&row.key);
            if *insert {
                if !self.native_lines(row.key)?.is_empty() {
                    bail!("RF1 Order {} has existing Lineitems", row.key);
                }
            } else if !discovered_deletions {
                let current_lines = self.native_lines(row.key)?;
                if current_lines.len() != changed.map_or(0, HashMap::len)
                    || current_lines.iter().any(|line| {
                        changed.and_then(|m| m.get(&line.number)).copied() != Some(line)
                    })
                {
                    bail!(
                        "RF2 Order {} must delete every current Lineitem with its current payload",
                        row.key
                    );
                }
            }
        }
        Ok(())
    }
}

pub(crate) fn batch_storage_evidence(reader: &dyn SerBatchReader) -> Value {
    let batches = reader.batches();
    let memory_batches = batches
        .iter()
        .filter(|batch| {
            !batch.is_empty() && batch.location() != dbsp::trace::BatchLocation::Storage
        })
        .count();
    serde_json::json!({"snapshot_batch_count": batches.len(), "memory_batch_count": memory_batches,
        "file_backed": memory_batches == 0})
}

pub(crate) fn rf2_changes(
    state: &BaseState,
    keys: &HashSet<i64>,
    mut lines_for_order: impl FnMut(&Order) -> Result<Vec<Line>>,
) -> Result<(Change, usize)> {
    let mut keys: Vec<_> = keys.iter().copied().collect();
    keys.sort_unstable();
    let mut change = Change::default();
    let mut absent = 0;
    for key in keys {
        if let Some(order) = state.order(key)? {
            let lines = lines_for_order(&order)?;
            change
                .lines
                .extend(lines.into_iter().map(|line| (line, false)));
            change.orders.push((order, false));
        } else {
            absent += 1;
        }
    }
    Ok((change, absent))
}

#[derive(Debug, Default)]
pub(crate) struct LoadStats {
    pub(crate) customer_rows: usize,
    pub(crate) order_rows: usize,
    pub(crate) line_rows: usize,
    pub(crate) transactions: usize,
    pub(crate) max_retained_rows: usize,
}

fn rows(path: &Path) -> Result<Lines<BufReader<File>>> {
    Ok(
        BufReader::new(File::open(path).with_context(|| format!("open {}", path.display()))?)
            .lines(),
    )
}

fn next_row<T>(
    input: &mut Lines<BufReader<File>>,
    parse: impl FnOnce(&str) -> Result<T>,
) -> Result<Option<T>> {
    input.next().map(|line| parse(&line?)).transpose()
}

fn emit_batch(
    batch: &mut Change,
    stats: &mut LoadStats,
    emit: &mut impl FnMut(Change) -> Result<()>,
) -> Result<()> {
    if batch.len() != 0 {
        emit(std::mem::take(batch))?;
        stats.transactions += 1;
    }
    Ok(())
}

pub(crate) fn load_batches(
    customer_path: &Path,
    orders_path: &Path,
    lineitem_path: &Path,
    target: usize,
    mut emit: impl FnMut(Change) -> Result<()>,
) -> Result<LoadStats> {
    if target == 0 {
        bail!("load batch target must be positive");
    }
    let mut stats = LoadStats::default();
    let mut batch = Change::default();
    for row in rows(customer_path)? {
        batch.customers.push((Customer::parse(&row?)?, true));
        stats.customer_rows += 1;
        stats.max_retained_rows = stats.max_retained_rows.max(batch.len());
        if batch.len() >= target {
            emit_batch(&mut batch, &mut stats, &mut emit)?;
        }
    }
    emit_batch(&mut batch, &mut stats, &mut emit)?;

    let mut orders = rows(orders_path)?;
    let mut lines = rows(lineitem_path)?;
    let mut next_line = next_row(&mut lines, Line::parse)?;
    let mut previous_order = None;
    for source in &mut orders {
        let order = Order::parse(&source?)?;
        if previous_order.is_some_and(|key| order.key <= key) {
            bail!("unsupported Orders input ordering: keys must increase strictly");
        }
        previous_order = Some(order.key);
        if next_line
            .as_ref()
            .is_some_and(|line| line.order_key < order.key)
        {
            bail!("unsupported Lineitem input ordering or orphan: order keys must follow Orders");
        }
        let mut group = Change::default();
        group.orders.push((order, true));
        while next_line
            .as_ref()
            .is_some_and(|line| line.order_key == group.orders[0].0.key)
        {
            group.lines.push((next_line.take().unwrap(), true));
            next_line = next_row(&mut lines, Line::parse)?;
            if next_line
                .as_ref()
                .is_some_and(|line| line.order_key < group.orders[0].0.key)
            {
                bail!(
                    "unsupported Lineitem input ordering: order keys must be grouped and ascending"
                );
            }
            stats.max_retained_rows = stats
                .max_retained_rows
                .max(batch.len() + group.len() + usize::from(next_line.is_some()));
        }
        if batch.len() != 0 && batch.len() + group.len() > target {
            emit_batch(&mut batch, &mut stats, &mut emit)?;
        }
        stats.order_rows += 1;
        stats.line_rows += group.lines.len();
        batch.orders.extend(group.orders);
        batch.lines.extend(group.lines);
        stats.max_retained_rows = stats
            .max_retained_rows
            .max(batch.len() + usize::from(next_line.is_some()));
        if batch.len() >= target {
            emit_batch(&mut batch, &mut stats, &mut emit)?;
        }
    }
    if next_line.is_some() {
        bail!("Lineitem input contains an Order absent from Orders");
    }
    emit_batch(&mut batch, &mut stats, &mut emit)?;
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "merged-index")]
    use crate::{reconstruction, ManualIndex, Q3State};
    use crate::{Inputs, Reference};
    use dbsp::circuit::CircuitConfig;

    #[cfg(feature = "merged-index")]
    fn submit_test_change(
        circuit: &mut dbsp::DBSPHandle,
        input: &mut Inputs,
        reference: &mut Reference,
        change: Change,
        index: &ManualIndex,
        q3: &Q3State,
    ) -> Result<()> {
        let (placements, rows) = reconstruction::prepare(&change)?;
        let (before, after) = index.append(rows)?;
        q3.publish(before, after, placements)
            .map_err(anyhow::Error::msg)?;
        change.apply(reference, input)?;
        circuit.transaction()?;
        q3.finish().map_err(anyhow::Error::msg)
    }

    #[cfg(not(feature = "merged-index"))]
    fn submit_test_change(
        circuit: &mut dbsp::DBSPHandle,
        input: &mut Inputs,
        reference: &mut Reference,
        change: Change,
    ) -> Result<()> {
        change.apply(reference, input)?;
        circuit.transaction()?;
        Ok(())
    }
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

    fn input_files(
        orders: &str,
        lines: &str,
    ) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "q3-workload-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        let customers = dir.join("customer.tbl");
        let orders_path = dir.join("orders.tbl");
        let lines_path = dir.join("lineitem.tbl");
        fs::write(&customers, "1|n|a|1|p|1.00|BUILDING|comment|\n").unwrap();
        fs::write(&orders_path, orders).unwrap();
        fs::write(&lines_path, lines).unwrap();
        (customers, orders_path, lines_path)
    }

    fn order(key: i64) -> String {
        format!("{key}|1|O|1.00|1996-02-29|5-LOW|Clerk|0|c|\n")
    }

    fn line(order: i64, number: i32) -> String {
        format!(
            "{order}|2|3|{number}|5|1.00|0.04|0.02|N|O|1996-03-13|1996-02-12|1996-03-22|i|m|c|\n"
        )
    }

    #[test]
    fn loader_preserves_large_group_and_bounds_other_retention() {
        let mut order_text = String::new();
        let mut line_text = String::new();
        for key in 1..=20 {
            order_text.push_str(&order(key));
            let count = if key == 10 { 31 } else { 1 };
            for number in 1..=count {
                line_text.push_str(&line(key, number));
            }
        }
        let (customers, orders, lines) = input_files(&order_text, &line_text);
        let mut batches = Vec::new();
        let stats = load_batches(&customers, &orders, &lines, 5, |change| {
            batches.push(change);
            Ok(())
        })
        .unwrap();
        assert_eq!(
            (stats.customer_rows, stats.order_rows, stats.line_rows),
            (1, 20, 50)
        );
        assert_eq!(stats.transactions, batches.len());
        assert!(stats.max_retained_rows <= 38);
        assert!(batches.iter().any(|batch| batch.orders.len() == 1
            && batch.orders[0].0.key == 10
            && batch.lines.len() == 31));
        assert!(batches
            .iter()
            .all(|batch| batch.customers.is_empty() || batch.orders.is_empty()));
    }

    #[test]
    fn loader_rejects_unsupported_ordering_and_orphans() {
        let cases = [
            (
                format!("{}{}", order(2), order(1)),
                String::new(),
                "Orders input ordering",
            ),
            (
                format!("{}{}", order(1), order(2)),
                format!("{}{}", line(2, 1), line(1, 1)),
                "Lineitem input ordering",
            ),
            (order(1), line(2, 1), "absent from Orders"),
        ];
        for (orders, lines, expected) in cases {
            let (customer_path, order_path, line_path) = input_files(&orders, &lines);
            let error =
                load_batches(&customer_path, &order_path, &line_path, 2, |_| Ok(())).unwrap_err();
            assert!(error.to_string().contains(expected), "{error:#}");
        }
    }

    #[test]
    fn base_snapshot_supports_current_deletion_and_rejects_stale_rows() -> Result<()> {
        #[cfg(feature = "merged-index")]
        let q3 = Q3State::default();
        #[cfg(feature = "merged-index")]
        let (mut circuit, catalog) =
            q3_ivm_baseline::circuit(CircuitConfig::with_workers(1), q3.clone())?;
        #[cfg(not(feature = "merged-index"))]
        let (mut circuit, catalog) = q3_ivm_baseline::circuit(CircuitConfig::with_workers(1))?;
        #[cfg(feature = "merged-index")]
        let index = ManualIndex::new(circuit.runtime());
        let mut input = Inputs::new(&catalog)?;
        let mut reference = Reference::default();
        let customer = Customer {
            key: 1,
            segment: "BUILDING".into(),
        };
        let order = Order {
            key: 7,
            customer_key: 1,
            date: "1996-02-29".into(),
            ship_priority: 0,
        };
        let first = Line {
            order_key: 7,
            number: 1,
            price_cents: 100,
            discount_hundredths: 4,
            ship_date: "1996-03-13".into(),
        };
        let second = Line {
            number: 2,
            ..first.clone()
        };
        let customers = Change {
            customers: vec![(customer, true)],
            ..Change::default()
        };
        BaseState::new(&catalog)?.validate(&customers)?;
        #[cfg(feature = "merged-index")]
        submit_test_change(
            &mut circuit,
            &mut input,
            &mut reference,
            customers,
            &index,
            &q3,
        )?;
        #[cfg(not(feature = "merged-index"))]
        submit_test_change(&mut circuit, &mut input, &mut reference, customers)?;
        let state = BaseState::new(&catalog)?;
        assert_eq!(state.storage_evidence()["CUSTOMER"]["file_backed"], false);
        let load = Change {
            orders: vec![(order.clone(), true)],
            lines: vec![(first.clone(), true), (second.clone(), true)],
            ..Change::default()
        };
        state.validate(&load)?;
        #[cfg(feature = "merged-index")]
        submit_test_change(&mut circuit, &mut input, &mut reference, load, &index, &q3)?;
        #[cfg(not(feature = "merged-index"))]
        submit_test_change(&mut circuit, &mut input, &mut reference, load)?;
        let state = BaseState::new(&catalog)?;
        assert_eq!(state.order(7)?, Some(order.clone()));
        assert_eq!(state.native_lines(7)?, vec![first.clone(), second.clone()]);
        assert!(state.validate(&Change::default()).is_ok());
        assert!(state
            .validate(&Change {
                orders: vec![(order.clone(), true)],
                ..Change::default()
            })
            .is_err());
        assert!(state
            .validate(&Change {
                orders: vec![(order.clone(), false), (order.clone(), false)],
                ..Change::default()
            })
            .is_err());
        assert!(state
            .validate(&Change {
                orders: vec![(order.clone(), false)],
                lines: vec![(first.clone(), true)],
                ..Change::default()
            })
            .is_err());
        assert!(state
            .validate(&Change {
                orders: vec![(
                    Order {
                        key: 8,
                        customer_key: 999,
                        ..order.clone()
                    },
                    true
                )],
                ..Change::default()
            })
            .is_err());
        assert!(state
            .validate(&Change {
                customers: vec![(
                    Customer {
                        key: 2,
                        segment: "BUILDING".into()
                    },
                    true
                )],
                ..Change::default()
            })
            .is_err());

        let requested = HashSet::from([7, 999]);
        let (delete, absent) = rf2_changes(&state, &requested, |row| state.native_lines(row.key))?;
        assert_eq!(absent, 1);
        state.validate(&delete)?;
        state.validate_discovered(&delete)?;
        let mut incomplete = delete.clone();
        incomplete.lines.pop();
        assert!(state.validate(&incomplete).is_err());
        let mut stale = delete.clone();
        stale.lines[0].0.price_cents += 1;
        assert!(state.validate(&stale).is_err());
        #[cfg(feature = "merged-index")]
        submit_test_change(
            &mut circuit,
            &mut input,
            &mut reference,
            delete,
            &index,
            &q3,
        )?;
        #[cfg(not(feature = "merged-index"))]
        submit_test_change(&mut circuit, &mut input, &mut reference, delete)?;
        let state = BaseState::new(&catalog)?;
        assert!(state.order(7)?.is_none());
        assert!(state.native_lines(7)?.is_empty());
        let (repeat, absent) = rf2_changes(&state, &HashSet::from([7]), |row| {
            state.native_lines(row.key)
        })?;
        assert_eq!(absent, 1);
        assert_eq!(repeat.len(), 0);
        Ok(())
    }
}
