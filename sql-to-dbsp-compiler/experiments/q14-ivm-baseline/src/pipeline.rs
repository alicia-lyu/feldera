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

use std::collections::{BTreeMap, BTreeSet};
#[cfg(feature = "merged-index")]
use std::sync::{Arc, OnceLock};

use anyhow::{bail, Context, Result};
#[cfg(feature = "merged-index")]
use dbsp::trace::merged_index::Pass;
use dbsp::{
    circuit::{CircuitConfig, CircuitConstructor},
    dynamic::Erase,
    utils::Tup2,
    DBSPHandle, RootCircuit, Runtime,
};
use dbsp_adapters::{Catalog, CircuitCatalog, DeCollectionStream, RecordFormat};
use feldera_adapterlib::catalog::SerBatchReaderHandle;
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
/// `push` called before each.
fn run_transaction(
    circuit: &mut DBSPHandle,
    steps: Vec<Change>,
    mut push: impl FnMut(&Change) -> Result<()>,
) -> Result<()> {
    circuit.start_transaction()?;
    for step in &steps {
        push(step)?;
        if circuit.step()? {
            bail!("a step of the transaction committed it");
        }
    }
    circuit.commit_transaction()?;
    Ok(())
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
        })
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
    /// returns the base circuit's net delta. With `stepped`, each circuit's
    /// transaction has three steps (`Change::steps`); the view circuit takes
    /// the net delta in the same steps.
    pub fn transact(&mut self, change: Change, stepped: bool) -> Result<Change> {
        let base_inputs = &mut self.base_inputs;
        run_transaction(&mut self.base, change.steps(stepped), |step| {
            base_inputs.push(step)
        })?;
        let net_delta = self.net_delta()?;
        self.run_view(&net_delta, stepped)?;
        Ok(net_delta)
    }

    #[cfg(not(feature = "merged-index"))]
    fn run_view(&mut self, net_delta: &Change, stepped: bool) -> Result<()> {
        let view_inputs = &mut self.view_inputs;
        run_transaction(&mut self.view, net_delta.clone().steps(stepped), |step| {
            view_inputs.push(step)
        })
    }

    /// Appends the net delta to the index and publishes `Old` and `Dlt`. The
    /// session collects once, before the transaction; its steps share it.
    #[cfg(feature = "merged-index")]
    fn run_view(&mut self, net_delta: &Change, stepped: bool) -> Result<()> {
        let (old, dlt) = self.index.append(index_rows(net_delta)?)?;
        self.q14_state
            .publish(old, dlt)
            .map_err(anyhow::Error::msg)?;
        let collection = self
            .base
            .runtime()
            .with_buffer_cache(0, || self.q14_state.collect(self.session_pass))
            .map_err(anyhow::Error::msg)?;
        self.view_inputs.set(collection.driving);
        // The inputs are already set: each step pushes nothing.
        run_transaction(&mut self.view, Change::default().steps(stepped), |_| Ok(()))?;
        self.q14_state.finish().map_err(anyhow::Error::msg)?;
        Ok(())
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
    /// Taking the base snapshot consumes it, so call this at most once per
    /// transaction.
    pub fn order_lines(&self, order_keys: &BTreeSet<i64>) -> Result<Vec<Line>> {
        let lines = self
            .base_catalog
            .output_handles(&SqlIdentifier::from("LINEITEM"))
            .context("missing base relation LINEITEM")?
            .integrate_handle
            .as_ref()
            .context("LINEITEM is not materialized")?
            .concat()
            .snapshot();
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

    /// The last transaction's change of `V`, and `V` after it.
    pub fn view_output(&self) -> Result<(BTreeMap<ViewRow, i64>, BTreeMap<ViewRow, i64>)> {
        let output = self
            .view_catalog
            .output_handles(&SqlIdentifier::from("V"))
            .context("missing V output")?;
        let delta = consolidated(read_rows(
            &*output.delta_handle,
            Rows::ZSetKeys,
            ViewRow::parse,
        )?);
        let view = output
            .integrate_handle
            .as_ref()
            .context("V has no materialized snapshot")?;
        let view = consolidated(read_rows(&**view, Rows::ZSetKeys, ViewRow::parse)?);
        Ok((delta, view))
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
