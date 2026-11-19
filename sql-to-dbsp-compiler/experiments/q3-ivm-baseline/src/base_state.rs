//! The base tables' state after a transaction, for the refreshes: validating changes, RF2
//! discovery, and the files the base tables' runs occupy. Apart from `workload`, whose loading
//! code shapes the images, so that changing this module keeps the images valid (it is not in
//! `measure.py`'s `IMAGE_SOURCE_PATHS`).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use dbsp::dynamic::Erase;
use dbsp::utils::{Tup1, Tup2};
use dbsp_adapters::{CircuitCatalog, RecordFormat};
use feldera_adapterlib::catalog::SerBatchReader;
use feldera_types::program_schema::SqlIdentifier;
use serde_json::Value;

use crate::data::{Line, Order};
#[cfg(test)]
use crate::workload::{parse_customer, parse_line, parse_order, weighted_rows};
#[cfg(test)]
use crate::Reference;
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

    /// The files of the base tables' file runs in this snapshot.
    pub(crate) fn file_ids(&self) -> HashSet<dbsp::storage::backend::FileId> {
        [&self.customers, &self.orders, &self.lines]
            .into_iter()
            .flat_map(|reader| reader.batches())
            .filter_map(|batch| batch.file_reader())
            .map(|file| file.file_id())
            .collect()
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

    /// Whether the base relations hold exactly the rows of `reference`, each with weight one.
    #[cfg(test)]
    pub(crate) fn matches(&self, reference: &Reference) -> Result<bool> {
        fn rows<T>(
            reader: &dyn SerBatchReader,
            parse: impl Fn(&Value) -> Result<T>,
            order_by: impl Fn(&T) -> (i64, i32),
        ) -> Result<Vec<T>> {
            let mut weighted = weighted_rows(reader, parse)?;
            weighted.sort_by_key(|(row, _)| order_by(row));
            if weighted.iter().any(|(_, weight)| *weight != 1) {
                bail!("base relation has a weight other than one");
            }
            Ok(weighted.into_iter().map(|(row, _)| row).collect())
        }
        let mut customers: Vec<_> = reference.customers.values().cloned().collect();
        customers.sort_by_key(|row| row.key);
        let mut orders: Vec<_> = reference.orders.values().cloned().collect();
        orders.sort_by_key(|row| row.key);
        let mut lines: Vec<_> = reference.lines.values().cloned().collect();
        lines.sort_by_key(|row| (row.order_key, row.number));
        Ok(
            rows(&*self.customers, parse_customer, |row| (row.key, 0))? == customers
                && rows(&*self.orders, parse_order, |row| (row.key, 0))? == orders
                && rows(&*self.lines, parse_line, |row| (row.order_key, row.number))? == lines,
        )
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
    let storage_batches = batches
        .iter()
        .filter(|batch| {
            !batch.is_empty() && batch.location() == dbsp::trace::BatchLocation::Storage
        })
        .count();
    serde_json::json!({"snapshot_batch_count": batches.len(), "memory_batch_count": memory_batches,
        "storage_batch_count": storage_batches, "file_backed": memory_batches == 0})
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
