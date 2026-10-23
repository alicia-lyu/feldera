//! Shared, deterministic RF1/RF2 verification for both generated Q3 circuits.

use super::*;

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

#[cfg(feature = "merged-index")]
fn check_readers(
    q3: &Q3State,
    before: &Reference,
    after: &Reference,
    change: &Change,
) -> Result<()> {
    // Requests come from logical deltas. In particular B is projected from
    // Customer and Orders maps, independently of the reconstructed readers.
    let customer_requests: HashSet<_> = change.customers.iter().map(|(c, _)| c.key).collect();
    let order_requests: HashSet<_> = change.orders.iter().map(|(o, _)| o.customer_key).collect();
    let line_requests: HashSet<_> = change.lines.iter().map(|(l, _)| l.order_key).collect();
    let joined_before: BTreeMap<_, _> = before
        .orders
        .values()
        .filter_map(|o| {
            before
                .customers
                .get(&o.customer_key)
                .map(|c| (o.key, (c, o)))
        })
        .collect();
    let joined_after: BTreeMap<_, _> = after
        .orders
        .values()
        .filter_map(|o| {
            after
                .customers
                .get(&o.customer_key)
                .map(|c| (o.key, (c, o)))
        })
        .collect();
    let joined_requests: HashSet<_> = joined_before
        .keys()
        .chain(joined_after.keys())
        .filter(|key| joined_before.get(key) != joined_after.get(key))
        .copied()
        .collect();

    let orders_delta = snapshot(
        order_requests
            .iter()
            .map(|c| (Tup1(*c), Tup3(0, Date::from_days(0), 0), 1))
            .collect(),
    );
    let customers_delta = snapshot(
        customer_requests
            .iter()
            .map(|c| (Tup1(*c), Tup1(SqlString::from("request")), 1))
            .collect(),
    );
    let lines_delta = snapshot(
        line_requests
            .iter()
            .map(|o| {
                (
                    Tup1(*o),
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
        joined_requests
            .iter()
            .map(|o| {
                (
                    Tup1(*o),
                    Tup4(0, SqlString::from("request"), Date::from_days(0), 0),
                    1,
                )
            })
            .collect(),
    );

    let mut expected_customers = Vec::new();
    for c in &order_requests {
        if let Some(row) = after.customers.get(c) {
            expected_customers.push((Tup1(*c), Tup1(SqlString::from(row.segment.as_str())), 1));
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

    let mut expected_orders = Vec::new();
    for row in before
        .orders
        .values()
        .filter(|o| customer_requests.contains(&o.customer_key))
    {
        expected_orders.push((
            Tup1(row.customer_key),
            Tup3(row.key, date(&row.date)?, row.ship_priority),
            1,
        ));
    }
    expected_orders.sort();
    assert_eq!(
        tuples(
            &q3.orders_before(&customers_delta)
                .map_err(anyhow::Error::msg)?
        ),
        expected_orders
    );

    let mut expected_joined = Vec::new();
    for key in &line_requests {
        if let Some((c, o)) = joined_after.get(key) {
            expected_joined.push((
                Tup1(*key),
                Tup4(
                    c.key,
                    SqlString::from(c.segment.as_str()),
                    date(&o.date)?,
                    o.ship_priority,
                ),
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

    let mut expected_lines = Vec::new();
    for row in before
        .lines
        .values()
        .filter(|l| joined_requests.contains(&l.order_key))
    {
        expected_lines.push((
            Tup1(row.order_key),
            Tup4(
                row.number,
                decimal(row.price_cents)?,
                discount(row.discount_hundredths)?,
                date(&row.ship_date)?,
            ),
            1,
        ));
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
    pipeline: &Pipeline,
    net_delta: &Change,
    before: &Reference,
    after: &Reference,
) -> Result<()> {
    publish_net_delta(pipeline, net_delta)?;
    check_readers(&pipeline.q3_state, before, after, net_delta)
}

/// The V transcript of the two-circuit pipeline must stay byte-identical to the transcript
/// that the one-circuit program produced, and to the other mode's transcript. Each fixture is
/// also checked against the base relations, which the base circuit maintains.
#[test]
fn rf1_rf2_transcript() -> Result<()> {
    let config = || CircuitConfig::with_workers(1).with_splitter_chunk_size_records(8);
    let mut pipeline = Pipeline::new(config(), config())?;
    let mut state = Reference::default();
    let mut transcript = Vec::new();

    let fixtures = vec![
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
    ];
    for (name, change, multi_step) in fixtures {
        let before = state.clone();
        let before_view = before.evaluate();
        let mut expected_after = before.clone();
        apply_reference(&mut expected_after, &change);
        let after_view = expected_after.evaluate();

        let net_delta = run_base(&mut pipeline, &mut state, change.clone(), multi_step)?;
        assert_eq!(
            (net_delta.customers.len(), net_delta.orders.len(), net_delta.lines.len()),
            (change.customers.len(), change.orders.len(), change.lines.len()),
            "{name}: the net delta has one entry per changed row"
        );
        assert!(
            workload::BaseState::new(&pipeline.base.catalog)?.matches(&state)?,
            "{name}: base relations"
        );
        #[cfg(feature = "merged-index")]
        publish(&pipeline, &net_delta, &before, &expected_after)?;
        run_view(&mut pipeline, net_delta, multi_step)?;

        assert_eq!(state.evaluate(), after_view, "{name}: fixture state");
        transcript.push(verify_output(
            &pipeline.view.catalog,
            &before_view,
            &after_view,
            name,
        )?);
    }
    assert_eq!(transcript.len(), 13);
    if let Some(path) = std::env::var_os("Q3_VERIFICATION_TRANSCRIPT") {
        std::fs::write(path, serde_json::to_vec_pretty(&transcript)?)?;
    }
    Ok(())
}
