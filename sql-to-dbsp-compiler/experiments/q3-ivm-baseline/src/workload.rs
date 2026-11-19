//! Bounded base loading and the base tables' net delta. This module shapes the images (it is in
//! `measure.py`'s `IMAGE_SOURCE_PATHS`); the base tables' state for the refreshes is in
//! `base_state`.

use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader, Lines};
use std::path::Path;

use anyhow::{bail, Context, Result};
use dbsp_adapters::{CircuitCatalog, RecordFormat};
use feldera_adapterlib::catalog::SerBatchReader;
use feldera_types::program_schema::SqlIdentifier;
use serde_json::Value;

use crate::data::{Customer, Line, Order};
use crate::{cents, integer, string, Change};

pub(crate) use crate::base_state::{batch_storage_evidence, rf2_changes, BaseState};

/// Switches on the delta streams of the base relations. A primary-key table's delta accumulator
/// stays idle until an output connector attaches, so without this call `net_delta` reads
/// nothing. Call it before the first transaction.
pub(crate) fn enable_net_delta(catalog: &impl CircuitCatalog) -> Result<()> {
    for name in ["CUSTOMER", "ORDERS", "LINEITEM"] {
        catalog
            .output_handles(&SqlIdentifier::from(name))
            .with_context(|| format!("missing base relation {name}"))?
            .enable_count
            .enable();
    }
    Ok(())
}

/// Reads, and so drains, the base circuit's delta streams: the net change that the last base
/// transaction made to each base relation. The base relations are primary-key input maps, so
/// an upsert that replaces a row shows as a deletion of the old row and an insertion of the new
/// one, and a deletion carries the complete old row.
pub(crate) fn net_delta(catalog: &impl CircuitCatalog) -> Result<Change> {
    Ok(Change {
        customers: delta_rows(catalog, "CUSTOMER", parse_customer)?,
        orders: delta_rows(catalog, "ORDERS", parse_order)?,
        lines: delta_rows(catalog, "LINEITEM", parse_line)?,
    })
}

pub(crate) fn parse_customer(row: &Value) -> Result<Customer> {
    Ok(Customer {
        key: integer(row, "c_custkey")?,
        segment: string(row, "c_mktsegment")?,
    })
}

pub(crate) fn parse_order(row: &Value) -> Result<Order> {
    Ok(Order {
        key: integer(row, "o_orderkey")?,
        customer_key: integer(row, "o_custkey")?,
        date: string(row, "o_orderdate")?,
        ship_priority: i32::try_from(integer(row, "o_shippriority")?)?,
    })
}

pub(crate) fn parse_line(row: &Value) -> Result<Line> {
    Ok(Line {
        order_key: integer(row, "l_orderkey")?,
        number: i32::try_from(integer(row, "l_linenumber")?)?,
        price_cents: cents(row, "l_extendedprice")?,
        discount_hundredths: cents(row, "l_discount")?,
        ship_date: string(row, "l_shipdate")?,
    })
}

/// Every (value, weight) pair of a batch, in key order.
pub(crate) fn weighted_rows<T>(
    reader: &dyn SerBatchReader,
    parse: impl Fn(&Value) -> Result<T>,
) -> Result<Vec<(T, i64)>> {
    let mut cursor = reader.cursor(RecordFormat::Json(Default::default()))?;
    let mut rows = Vec::new();
    while cursor.key_valid() {
        while cursor.val_valid() {
            rows.push((parse(&cursor.val_to_json()?)?, cursor.weight()));
            cursor.step_val();
        }
        cursor.step_key();
    }
    Ok(rows)
}

/// The rows of one relation's delta stream, each with `true` for an insertion and `false` for a
/// deletion. Primary keys make every weight `+1` or `-1`; any other weight is an error.
fn delta_rows<T>(
    catalog: &impl CircuitCatalog,
    name: &str,
    parse: impl Fn(&Value) -> Result<T>,
) -> Result<Vec<(T, bool)>> {
    let reader = catalog
        .output_handles(&SqlIdentifier::from(name))
        .with_context(|| format!("missing base relation {name}"))?
        .delta_handle
        .concat();
    weighted_rows(&*reader, parse)?
        .into_iter()
        .map(|(row, weight)| match weight {
            1 => Ok((row, true)),
            -1 => Ok((row, false)),
            _ => bail!("{name} delta has weight {weight}; primary keys allow only +1 and -1"),
        })
        .collect()
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
    use crate::{run_base, run_view, Pipeline, Reference};

    /// Runs `change` through both circuits and, in reconstructed mode, the merged index.
    fn submit_test_change(
        pipeline: &mut Pipeline,
        reference: &mut Reference,
        change: Change,
    ) -> Result<()> {
        let net_delta = run_base(pipeline, reference, change, false)?;
        #[cfg(feature = "merged-index")]
        crate::publish_net_delta(pipeline, &net_delta, reference)?;
        run_view(pipeline, net_delta, false)
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
        let mut pipeline = Pipeline::with_workers(1)?;
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
        BaseState::new(&pipeline.base.catalog)?.validate(&customers)?;
        submit_test_change(&mut pipeline, &mut reference, customers)?;
        // Reading a snapshot consumes it, so each transaction gets one `BaseState`.
        let state = BaseState::new(&pipeline.base.catalog)?;
        assert!(state.matches(&reference)?);
        assert_eq!(state.storage_evidence()["CUSTOMER"]["file_backed"], false);
        let load = Change {
            orders: vec![(order.clone(), true)],
            lines: vec![(first.clone(), true), (second.clone(), true)],
            ..Change::default()
        };
        state.validate(&load)?;
        submit_test_change(&mut pipeline, &mut reference, load)?;
        // Reading a snapshot consumes it, so each transaction gets one `BaseState`.
        let state = BaseState::new(&pipeline.base.catalog)?;
        assert!(state.matches(&reference)?);
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
        submit_test_change(&mut pipeline, &mut reference, delete)?;
        // Reading a snapshot consumes it, so each transaction gets one `BaseState`.
        let state = BaseState::new(&pipeline.base.catalog)?;
        assert!(state.matches(&reference)?);
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
