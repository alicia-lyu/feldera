//! Shared, deterministic verification of both Q3 builds: the RF1/RF2 transcript, and the
//! general one for changes beyond RF1/RF2.

use super::*;

#[cfg(feature = "merged-index")]
use std::collections::BTreeSet;

#[cfg(feature = "merged-index")]
use dbsp::{
    dynamic::DowncastTrait,
    trace::{BatchReader as DynamicBatchReader, Cursor, WithSnapshot},
    typed_batch::{BatchReader, SpineSnapshot, TypedBatch},
    utils::{Tup1, Tup2, Tup3, Tup4},
    DBData, OrdIndexedZSet,
};
#[cfg(feature = "merged-index")]
use feldera_sqllib::{Date, SqlDecimal, SqlString};

fn customer(key: i64, segment: &str) -> Customer {
    Customer {
        key,
        segment: segment.into(),
    }
}

fn order(key: i64, customer_key: i64) -> Order {
    Order {
        key,
        customer_key,
        date: "1996-01-02".into(),
        ship_priority: i32::try_from(key % 7).unwrap(),
    }
}

fn line(order_key: i64, number: i32) -> Line {
    Line {
        order_key,
        number,
        price_cents: 100,
        discount_hundredths: 4,
        ship_date: "1996-03-13".into(),
    }
}

fn group(order_key: i64, customer_key: i64, line_count: i32, insert: bool) -> Change {
    Change {
        orders: vec![(order(order_key, customer_key), insert)],
        lines: (1..=line_count)
            .map(|n| (line(order_key, n), insert))
            .collect(),
        ..Change::default()
    }
}

fn combine(changes: impl IntoIterator<Item = Change>) -> Change {
    let mut combined = Change::default();
    for change in changes {
        combined.customers.extend(change.customers);
        combined.orders.extend(change.orders);
        combined.lines.extend(change.lines);
    }
    combined
}

fn apply_reference(state: &mut Reference, change: &Change) {
    for (row, insert) in &change.customers {
        if *insert {
            state.customers.insert(row.key, row.clone());
        } else {
            state.customers.remove(&row.key);
        }
    }
    for (row, insert) in &change.orders {
        if *insert {
            state.orders.insert(row.key, row.clone());
        } else {
            state.orders.remove(&row.key);
        }
    }
    for (row, insert) in &change.lines {
        let key = (row.order_key, row.number);
        if *insert {
            state.lines.insert(key, row.clone());
        } else {
            state.lines.remove(&key);
        }
    }
}

#[cfg(feature = "merged-index")]
fn snapshot<K: DBData, V: DBData>(rows: Vec<(K, V, i64)>) -> SpineSnapshot<OrdIndexedZSet<K, V>> {
    let batch = OrdIndexedZSet::from_tuples(
        (),
        rows.into_iter()
            .map(|(k, v, w)| Tup2(Tup2(k, v), w))
            .collect(),
    );
    TypedBatch::new(batch.into_inner().into_ro_snapshot())
}

#[cfg(feature = "merged-index")]
fn tuples<K: DBData, V: DBData>(
    snapshot: &SpineSnapshot<OrdIndexedZSet<K, V>>,
) -> Vec<(K, V, i64)> {
    let mut cursor = snapshot.inner().cursor();
    let mut rows = Vec::new();
    while cursor.key_valid() {
        while cursor.val_valid() {
            rows.push((
                cursor.key().downcast_checked::<K>().clone(),
                cursor.val().downcast_checked::<V>().clone(),
                **cursor.weight(),
            ));
            cursor.step_val();
        }
        cursor.step_key();
    }
    rows
}

#[cfg(feature = "merged-index")]
fn date(value: &str) -> Result<Date> {
    Ok(serde_json::from_value(Value::String(value.into()))?)
}

#[cfg(feature = "merged-index")]
fn decimal(value: i64) -> Result<SqlDecimal<15, 2>> {
    SqlDecimal::new(i128::from(value), 2).context("invalid line price")
}

#[cfg(feature = "merged-index")]
fn discount(value: i64) -> Result<SqlDecimal<4, 2>> {
    SqlDecimal::new(i128::from(value), 2).context("invalid line discount")
}

/// Checks the four partner reads against brute force over the references: each reader's
/// requested slice, at the requests that the references' own driving deltas make. The check
/// collects the readers in a state of its own, from the published `Old` and `Dlt`, and leaves the
/// pipeline's for the view circuit.
#[cfg(feature = "merged-index")]
fn check_readers(
    q3: &Q3State,
    before: &Reference,
    after: &Reference,
    change: &Change,
) -> Result<()> {
    let readers = Q3State::default();
    readers.enable_contract_check();
    readers
        .publish(
            q3.old().map_err(anyhow::Error::msg)?,
            q3.dlt().map_err(anyhow::Error::msg)?,
        )
        .map_err(anyhow::Error::msg)?;
    readers.collect(Pass::Seek).map_err(anyhow::Error::msg)?;
    check_collected_readers(&readers, before, after, change)?;
    readers.finish().map_err(anyhow::Error::msg)?;
    Ok(())
}

/// The index state of a reference: what `ManualIndex` holds when it is in step with it. An order
/// is at its customer (whether or not that customer exists), and a line item is at its order's
/// customer, if its order exists; an orphan line item is in the side store, not the index.
#[cfg(feature = "merged-index")]
struct IndexState {
    customers: BTreeMap<i64, String>,
    /// `(custkey, orderkey)` to `(date, shippriority)`.
    orders: BTreeMap<(i64, i64), (String, i32)>,
    /// `(custkey, orderkey, linenumber)` to the line item.
    lines: BTreeMap<(i64, i64, i32), Line>,
}

#[cfg(feature = "merged-index")]
impl IndexState {
    fn of(reference: &Reference) -> Self {
        let owner = |order_key: i64| reference.orders.get(&order_key).map(|o| o.customer_key);
        Self {
            customers: reference
                .customers
                .iter()
                .map(|(key, row)| (*key, row.segment.clone()))
                .collect(),
            orders: reference
                .orders
                .values()
                .map(|o| ((o.customer_key, o.key), (o.date.clone(), o.ship_priority)))
                .collect(),
            lines: reference
                .lines
                .values()
                .filter_map(|l| owner(l.order_key).map(|c| ((c, l.order_key, l.number), l.clone())))
                .collect(),
        }
    }

    /// `J = O ⋈ C`: the orders whose customer exists, with the customer's segment.
    fn joined(&self) -> BTreeSet<(i64, i64, String, String, i32)> {
        self.orders
            .iter()
            .filter_map(|((c, o), (date, priority))| {
                self.customers
                    .get(c)
                    .map(|segment| (*c, *o, segment.clone(), date.clone(), *priority))
            })
            .collect()
    }
}

/// The nodes `(custkey, orderkey)` of the rows in which two sets differ.
#[cfg(feature = "merged-index")]
fn differing_nodes<K: Ord + Clone, V: PartialEq>(
    before: &BTreeMap<K, V>,
    after: &BTreeMap<K, V>,
    node: impl Fn(&K) -> (i64, i64),
) -> BTreeSet<(i64, i64)> {
    before
        .keys()
        .chain(after.keys())
        .filter(|key| before.get(*key) != after.get(*key))
        .map(node)
        .collect()
}

#[cfg(feature = "merged-index")]
fn check_collected_readers(
    q3: &Q3State,
    before: &Reference,
    after: &Reference,
    _change: &Change,
) -> Result<()> {
    // The driving deltas are the differences of the two index states, by brute force from the
    // references, independent of `prepare`: `ΔC` names the customers whose row differs, `ΔO` the
    // owners (before and after) of the orders whose entry differs, and `ΔL` the nodes of the line
    // items whose entry differs.
    let (old, new) = (IndexState::of(before), IndexState::of(after));
    let customers_of_delta_customers: BTreeSet<i64> =
        differing_nodes(&old.customers, &new.customers, |c| (*c, 0))
            .into_iter()
            .map(|(c, _)| c)
            .collect();
    let customers_of_delta_orders: BTreeSet<i64> =
        differing_nodes(&old.orders, &new.orders, |(c, o)| (*c, *o))
            .into_iter()
            .map(|(c, _)| c)
            .collect();
    let nodes_of_delta_lines = differing_nodes(&old.lines, &new.lines, |(c, o, _)| (*c, *o));
    // `ΔJ = J(after) - J(before)` has a row at the nodes of the rows in which the two differ.
    let (joined_before, joined_after) = (old.joined(), new.joined());
    let nodes_of_delta_joined: BTreeSet<(i64, i64)> = joined_before
        .symmetric_difference(&joined_after)
        .map(|(c, o, ..)| (*c, *o))
        .collect();

    let orders_delta = snapshot(
        customers_of_delta_orders
            .iter()
            .map(|c| (Tup1(*c), Tup3(0, Date::from_days(0), 0), 1))
            .collect(),
    );
    let customers_delta = snapshot(
        customers_of_delta_customers
            .iter()
            .map(|c| (Tup1(*c), Tup1(SqlString::from("request")), 1))
            .collect(),
    );
    let lines_delta = snapshot(
        nodes_of_delta_lines
            .iter()
            .map(|(c, o)| {
                (
                    Tup2(*c, *o),
                    Tup4(
                        0,
                        decimal(0).unwrap(),
                        discount(0).unwrap(),
                        Date::from_days(0),
                    ),
                    1,
                )
            })
            .collect(),
    );
    let joined_delta = snapshot(
        nodes_of_delta_joined
            .iter()
            .map(|(c, o)| {
                (
                    Tup2(*c, *o),
                    Tup3(SqlString::from("request"), Date::from_days(0), 0),
                    1,
                )
            })
            .collect(),
    );

    // C⁺: the customers of `ΔO`, after the change.
    let mut expected_customers = Vec::new();
    for c in &customers_of_delta_orders {
        if let Some(segment) = new.customers.get(c) {
            expected_customers.push((Tup1(*c), Tup1(SqlString::from(segment.as_str())), 1));
        }
    }
    expected_customers.sort();
    assert_eq!(
        tuples(
            &q3.customers_after(&orders_delta)
                .map_err(anyhow::Error::msg)?
        ),
        expected_customers
    );

    // O⁻: the orders of the customers of `ΔC`, before the change.
    let mut expected_orders = Vec::new();
    for ((c, o), (day, priority)) in &old.orders {
        if customers_of_delta_customers.contains(c) {
            expected_orders.push((Tup1(*c), Tup3(*o, date(day)?, *priority), 1));
        }
    }
    expected_orders.sort();
    assert_eq!(
        tuples(
            &q3.orders_before(&customers_delta)
                .map_err(anyhow::Error::msg)?
        ),
        expected_orders
    );

    // J⁺: the joined orders after the change, at the nodes of `ΔL`.
    let mut expected_joined = Vec::new();
    for (c, o, segment, day, priority) in &joined_after {
        if nodes_of_delta_lines.contains(&(*c, *o)) {
            expected_joined.push((
                Tup2(*c, *o),
                Tup3(SqlString::from(segment.as_str()), date(day)?, *priority),
                1,
            ));
        }
    }
    expected_joined.sort();
    assert_eq!(
        tuples(
            &q3.joined_orders_after(&lines_delta)
                .map_err(anyhow::Error::msg)?
        ),
        expected_joined
    );

    // L⁻: the lines before the change at the nodes where `ΔJ` has a row, each at the order's
    // customer then. An orphan line is in no group.
    let mut expected_lines = Vec::new();
    for ((c, o, _), row) in &old.lines {
        if nodes_of_delta_joined.contains(&(*c, *o)) {
            expected_lines.push((
                Tup2(*c, *o),
                Tup4(
                    row.number,
                    decimal(row.price_cents)?,
                    discount(row.discount_hundredths)?,
                    date(&row.ship_date)?,
                ),
                1,
            ));
        }
    }
    expected_lines.sort();
    assert_eq!(
        tuples(
            &q3.lineitems_before(&joined_delta)
                .map_err(anyhow::Error::msg)?
        ),
        expected_lines
    );
    Ok(())
}

fn verify_output(
    catalog: &impl CircuitCatalog,
    before: &BTreeMap<Joined, i64>,
    after: &BTreeMap<Joined, i64>,
    name: &str,
) -> Result<Value> {
    let output = catalog
        .output_handles(&SqlIdentifier::from("V"))
        .context("missing V output")?;
    let actual_delta = read_output(&*output.delta_handle)?;
    let expected_delta = difference(before, after);
    assert_eq!(actual_delta, expected_delta, "{name}: delta");
    let snapshot = output
        .integrate_handle
        .as_ref()
        .context("V has no materialized snapshot")?;
    let actual_view = read_output(&**snapshot)?;
    assert_eq!(&actual_view, after, "{name}: materialized V");
    Ok(json!({
        "transaction": name,
        "delta": actual_delta.into_iter().collect::<Vec<_>>(),
        "view": actual_view.into_iter().collect::<Vec<_>>(),
    }))
}

/// Publishes the net delta to the merged index and checks the readers against the references
/// before and after the change.
#[cfg(feature = "merged-index")]
fn publish(
    pipeline: &mut Pipeline,
    net_delta: &Change,
    before: &Reference,
    after: &Reference,
) -> Result<()> {
    publish_net_delta(pipeline, net_delta, after)?;
    check_readers(&pipeline.q3_state, before, after, net_delta)
}

/// One fixture of a transcript: its name, its change, and whether the circuits run it as one
/// transaction of several steps.
type Fixture = (&'static str, Change, bool);

/// Runs the fixtures in order through a fresh pipeline, checking each against the references
/// and the base relations, and returns the V transcript: each fixture's delta and view.
/// `session_pass` is the reconstructed build's scan-session pass; the native build has no
/// session.
fn replay(fixtures: Vec<Fixture>, session_pass: Pass) -> Result<Vec<Value>> {
    replay_with_image(fixtures, session_pass, None, false, None)
}

/// A scratch storage directory for one test, removed when dropped.
struct ScratchStorage(PathBuf);

impl ScratchStorage {
    fn new(name: &str) -> Result<Self> {
        let path = std::env::temp_dir().join(format!("q3-image-{name}-{}", std::process::id()));
        if path.exists() {
            std::fs::remove_dir_all(&path)?;
        }
        std::fs::create_dir_all(&path)?;
        Ok(Self(path))
    }
}

impl Drop for ScratchStorage {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Like [`replay`]. With `image_after = Some(count)`, the pipeline keeps its state in a scratch
/// storage directory, writes an image (`Pipeline::write_checkpoint`) after the first `count`
/// fixtures, and runs the rest on a pipeline reopened from that image in a new runtime, as a
/// refresh process does (`Pipeline::reopen`, then `republish`).
#[cfg_attr(not(feature = "merged-index"), allow(unused_variables))]
///
/// `index_prefix_filter` gives the merged index's files prefix filters (`--index-prefix-filter`).
/// `memtable_mib` gives the integral spines memtables (`--memtable-mib`); a memtable needs
/// storage, so it keeps the state in a scratch directory even without an image.
fn replay_with_image(
    fixtures: Vec<Fixture>,
    session_pass: Pass,
    image_after: Option<usize>,
    index_prefix_filter: bool,
    memtable_mib: Option<usize>,
) -> Result<Vec<Value>> {
    let storage = (image_after.is_some() || memtable_mib.is_some())
        .then(|| {
            // Tests run concurrently in one process: name the directory by every parameter.
            ScratchStorage::new(&format!(
                "{session_pass:?}-prefix-{index_prefix_filter}-image-{image_after:?}-memtable-{memtable_mib:?}"
            ))
        })
        .transpose()?;
    let config = || -> Result<CircuitConfig> {
        let config = CircuitConfig::with_workers(1).with_splitter_chunk_size_records(8);
        Ok(match &storage {
            None => config,
            Some(storage) => config.with_storage(Some(CircuitStorageConfig::for_config(
                StorageConfig {
                    path: storage.0.to_string_lossy().into_owned(),
                    cache: StorageCacheConfig::default(),
                },
                StorageOptions {
                    memtable_mib,
                    ..StorageOptions::default()
                },
            )?)),
        })
    };
    let prepare = |pipeline: &mut Pipeline| {
        pipeline.enable_contract_check();
        #[cfg(feature = "merged-index")]
        {
            pipeline.session_pass = session_pass;
        }
    };
    let mut pipeline = Pipeline::new(config()?, index_prefix_filter)?;
    prepare(&mut pipeline);
    let mut state = Reference::default();
    let mut transcript = Vec::new();

    for (position, (name, change, multi_step)) in fixtures.into_iter().enumerate() {
        if image_after == Some(position) {
            let image = pipeline.write_checkpoint()?;
            drop(pipeline);
            pipeline = Pipeline::reopen(config()?, &image, index_prefix_filter)?;
            prepare(&mut pipeline);
            let mut base = workload::BaseState::new(&pipeline.base.catalog)?;
            republish(&mut pipeline, &mut base)?;
            assert!(base.matches(&state)?, "{name}: the reopened base relations");
        }
        let before = state.clone();
        let before_view = before.evaluate();
        let mut expected_after = before.clone();
        apply_reference(&mut expected_after, &change);
        let after_view = expected_after.evaluate();

        let net_delta = run_base(&mut pipeline, &mut state, change.clone(), multi_step)?;
        assert_eq!(
            (
                net_delta.customers.len(),
                net_delta.orders.len(),
                net_delta.lines.len()
            ),
            (
                change.customers.len(),
                change.orders.len(),
                change.lines.len()
            ),
            "{name}: the net delta has one entry per changed row"
        );
        assert!(
            workload::BaseState::new(&pipeline.base.catalog)?.matches(&state)?,
            "{name}: base relations"
        );
        #[cfg(feature = "merged-index")]
        publish(&mut pipeline, &net_delta, &before, &expected_after)?;
        run_view(&mut pipeline, net_delta, multi_step)?;

        assert_eq!(state.evaluate(), after_view, "{name}: fixture state");
        transcript.push(verify_output(
            &pipeline.view.catalog,
            &before_view,
            &after_view,
            name,
        )?);
    }
    Ok(transcript)
}

/// Writes a transcript to the file that `variable` names, if it is set. The transcript of the
/// native build and the reconstructed build's must be byte-identical.
fn write_transcript(variable: &str, transcript: &[Value]) -> Result<()> {
    if let Some(path) = std::env::var_os(variable) {
        std::fs::write(path, serde_json::to_vec_pretty(transcript)?)?;
    }
    Ok(())
}

/// The V transcript of the two-circuit pipeline must stay byte-identical to the transcript
/// that the one-circuit program produced, and to the other mode's transcript. Each fixture is
/// also checked against the base relations, which the base circuit maintains.
#[test]
fn rf1_rf2_transcript() -> Result<()> {
    let transcript = replay(rf1_rf2_fixtures(), Pass::Seek)?;
    assert_eq!(transcript.len(), 13);
    write_transcript("Q3_VERIFICATION_TRANSCRIPT", &transcript)
}

/// A pipeline reopened from an image continues exactly where the pipeline that wrote it
/// stopped: splitting the fixtures at any point by an image and a new runtime leaves the V
/// transcript byte-identical to the single runtime's. This is the guarantee that a refresh
/// process started from a loaded image depends on.
#[test]
fn an_image_reopens_where_its_writer_stopped() -> Result<()> {
    for fixtures in [rf1_rf2_fixtures, general_fixtures] {
        let whole = serde_json::to_vec(&replay(fixtures(), Pass::Seek)?)?;
        for split in [1, fixtures().len() / 2, fixtures().len() - 1] {
            let reopened = replay_with_image(fixtures(), Pass::Seek, Some(split), false, None)?;
            assert_eq!(
                serde_json::to_vec(&reopened)?,
                whole,
                "an image after {split} fixtures"
            );
        }
    }
    Ok(())
}

/// The merged index's prefix filter (measurement version 17) leaves every transcript unchanged,
/// whole and across an image. Reopened from an image, every run is a file with a prefix filter,
/// so the group seeks after the split probe the filters and park the runs without the group.
#[cfg(feature = "merged-index")]
#[test]
fn transcripts_do_not_depend_on_the_index_prefix_filter() -> Result<()> {
    for fixtures in [rf1_rf2_fixtures, general_fixtures] {
        let whole = serde_json::to_vec(&replay(fixtures(), Pass::Seek)?)?;
        assert_eq!(
            serde_json::to_vec(&replay_with_image(
                fixtures(),
                Pass::Seek,
                None,
                true,
                None
            )?)?,
            whole
        );
        for split in [1, fixtures().len() / 2] {
            let reopened = replay_with_image(fixtures(), Pass::Seek, Some(split), true, None)?;
            assert_eq!(
                serde_json::to_vec(&reopened)?,
                whole,
                "an image after {split} fixtures, with prefix filters"
            );
        }
    }
    Ok(())
}

/// The integral spines' memtables leave every transcript unchanged, whole and across an image:
/// the image saves each memtable as a run, and the reopened pipeline fills new memtables.
#[test]
fn transcripts_do_not_depend_on_the_memtable() -> Result<()> {
    for fixtures in [rf1_rf2_fixtures, general_fixtures] {
        let whole = serde_json::to_vec(&replay(fixtures(), Pass::Seek)?)?;
        assert_eq!(
            serde_json::to_vec(&replay_with_image(
                fixtures(),
                Pass::Seek,
                None,
                false,
                Some(1)
            )?)?,
            whole
        );
        for split in [1, fixtures().len() / 2] {
            let reopened = replay_with_image(fixtures(), Pass::Seek, Some(split), false, Some(1))?;
            assert_eq!(
                serde_json::to_vec(&reopened)?,
                whole,
                "an image after {split} fixtures, with memtables"
            );
        }
    }
    Ok(())
}

/// The scan session's other pass leaves every transcript unchanged, so it matches the native
/// build's as `Seek`'s does.
#[cfg(feature = "merged-index")]
#[test]
fn transcripts_do_not_depend_on_the_session_pass() -> Result<()> {
    for fixtures in [rf1_rf2_fixtures, general_fixtures] {
        assert_eq!(
            replay(fixtures(), Pass::Step)?,
            replay(fixtures(), Pass::Seek)?
        );
    }
    Ok(())
}

fn rf1_rf2_fixtures() -> Vec<Fixture> {
    vec![
        (
            "customers",
            Change {
                customers: vec![
                    (customer(2, "BUILDING"), true),
                    (customer(3, "AUTOMOBILE"), true),
                    (customer(4, "HOUSEHOLD"), true),
                ],
                ..Change::default()
            },
            false,
        ),
        (
            "load-a",
            combine([group(20, 2, 13, true), group(21, 2, 2, true)]),
            false,
        ),
        (
            "load-b",
            combine([group(30, 3, 1, true), group(40, 4, 3, true)]),
            false,
        ),
        ("empty-after-load", Change::default(), false),
        ("empty-multi-step", Change::default(), true),
        (
            "rf1-a",
            combine([group(50, 2, 11, true), group(60, 3, 2, true)]),
            true,
        ),
        (
            "rf2-a",
            combine([group(20, 2, 13, false), group(30, 3, 1, false)]),
            false,
        ),
        ("rf1-b", group(70, 4, 9, true), false),
        (
            "rf2-b",
            combine([group(50, 2, 11, false), group(60, 3, 2, false)]),
            true,
        ),
        ("empty-between-cycles", Change::default(), false),
        (
            "rf1-c",
            combine([group(80, 3, 2, true), group(90, 2, 1, true)]),
            false,
        ),
        (
            "rf2-c",
            combine([group(70, 4, 9, false), group(80, 3, 2, false)]),
            false,
        ),
        ("empty-final", Change::default(), false),
    ]
}

/// A line item with a payload other than `line`'s, for an update.
fn repriced(order_key: i64, number: i32, price_cents: i64) -> Line {
    Line {
        price_cents,
        ..line(order_key, number)
    }
}

/// The same comparison for changes beyond RF1/RF2 (roadmap step 7): orphan line items, an
/// order that arrives for them, line items added to and updated in an existing order, a
/// customer replaced and deleted, an order that moves to another customer with and without
/// line changes, an order deleted while its lines stay, and several of these in one
/// transaction. The reconstructed build's `Dlt` must be complete for every one of them.
#[test]
fn general_transcript() -> Result<()> {
    let transcript = replay(general_fixtures(), Pass::Seek)?;
    assert_eq!(transcript.len(), 13);
    write_transcript("Q3_GENERAL_TRANSCRIPT", &transcript)
}

fn general_fixtures() -> Vec<Fixture> {
    vec![
        (
            "customers",
            Change {
                customers: vec![
                    (customer(2, "BUILDING"), true),
                    (customer(3, "AUTOMOBILE"), true),
                    (customer(4, "HOUSEHOLD"), true),
                ],
                ..Change::default()
            },
            false,
        ),
        // Lines with no order: they join nothing, and the index folds them nowhere.
        (
            "orphan-lines",
            Change {
                lines: vec![
                    (line(20, 1), true),
                    (line(20, 2), true),
                    (line(99, 1), true),
                ],
                ..Change::default()
            },
            false,
        ),
        // The order arrives for its orphan lines, with one more line item.
        (
            "order-arrives",
            Change {
                orders: vec![(order(20, 2), true)],
                lines: vec![(line(20, 3), true)],
                ..Change::default()
            },
            false,
        ),
        // A line item added to an existing order, and one updated in place.
        (
            "line-added-and-updated",
            Change {
                lines: vec![
                    (line(20, 4), true),
                    (line(20, 1), false),
                    (repriced(20, 1, 250), true),
                ],
                ..Change::default()
            },
            true,
        ),
        (
            "customer-replaced",
            Change {
                customers: vec![
                    (customer(2, "BUILDING"), false),
                    (customer(2, "MACHINERY"), true),
                ],
                ..Change::default()
            },
            false,
        ),
        // The order changes customer with no change to its line items: all of them move.
        (
            "order-moves",
            Change {
                orders: vec![(order(20, 2), false), (order(20, 3), true)],
                ..Change::default()
            },
            false,
        ),
        // It moves again while one line is deleted and another added.
        (
            "order-moves-with-line-changes",
            Change {
                orders: vec![(order(20, 3), false), (order(20, 4), true)],
                lines: vec![(line(20, 2), false), (line(20, 5), true)],
                ..Change::default()
            },
            true,
        ),
        // The order is deleted and its lines stay as orphans.
        (
            "order-deleted-lines-kept",
            Change {
                orders: vec![(order(20, 4), false)],
                ..Change::default()
            },
            false,
        ),
        // A line item of an orphan order changes while it is still an orphan.
        (
            "orphan-line-updated",
            Change {
                lines: vec![(line(99, 1), false), (repriced(99, 1, 300), true)],
                ..Change::default()
            },
            false,
        ),
        // The order returns, at another customer, and takes its orphan lines with it.
        (
            "order-returns",
            Change {
                orders: vec![(order(20, 3), true)],
                ..Change::default()
            },
            false,
        ),
        // A customer is deleted while its orders stay.
        (
            "customer-deleted",
            Change {
                customers: vec![(customer(3, "AUTOMOBILE"), false)],
                ..Change::default()
            },
            false,
        ),
        // Several kinds in one transaction: a customer replaced, two new orders with lines, and
        // an order deleted while one of its lines is deleted and the rest stay as orphans.
        (
            "mixed",
            Change {
                customers: vec![
                    (customer(4, "HOUSEHOLD"), false),
                    (customer(4, "FURNITURE"), true),
                ],
                orders: vec![
                    (order(20, 3), false),
                    (order(30, 4), true),
                    (order(31, 2), true),
                ],
                lines: vec![
                    (repriced(20, 1, 250), false),
                    (line(30, 1), true),
                    (line(31, 1), true),
                ],
            },
            true,
        ),
        ("empty", Change::default(), false),
    ]
}
