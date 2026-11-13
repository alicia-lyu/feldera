//! Query-specific partner reads for the Q3 view circuit.
//!
//! The view is `V = L ⋈ (O ⋈ C)`. The caller maintains the secondary index,
//! publishes the transaction's `Old` (the snapshot before it) and `Dlt` (its
//! signed index changes), and [`Q3State::collect`] reads them once each:
//!
//! 1. A pass over `Dlt` ([`capture`]) lists the touched customers and builds
//!    the driving deltas `ΔC`, `ΔO`, and `ΔL` once each, into the hand-off
//!    structure [`DrivingDeltas`], and each reader's requests.
//! 2. One scan session visits the touched customers' groups of `Old`. It
//!    decides from the folded key alone whether a reader keeps an entry, and
//!    reads, decodes, and unfolds a kept entry once.
//!
//! The four partner reads of the delta rules are four [`Reader`]s. Each holds
//! the request prefixes of its driving delta and keeps only the rows that it
//! requests; the deltas are not the readers': the routing borrows `ΔC` and
//! `ΔO` for the rows it needs (a requested customer's `ΔC` rows are copied
//! into `C⁺`'s slice), and the driver moves all three to the circuit:
//!
//! | Reader | Requests of | Keeps |
//! | --- | --- | --- |
//! | `C⁺` ([`CustomersAfter`]) | `ΔO` | the customers of `ΔO`, after: `Old`'s row and `ΔC`'s rows |
//! | `O⁻` ([`OrdersBefore`]) | `ΔC` | the orders of each customer in `ΔC`, before |
//! | `J⁺` ([`JoinedAfter`]) | `ΔL` | `J`'s rows at the `(custkey, orderkey)` of `ΔL`, after |
//! | `L⁻` ([`LineitemsBefore`]) | `ΔJ`, as requests only | the line items of each order in `ΔJ`, before |
//!
//! The folded keys put a customer first, then each of its orders followed by
//! its line items, so one forward pass over a group knows both terms of `ΔJ`
//! (`ΔO ⋈ C⁺` and `O⁻ ⋈ ΔC`) and `J⁺`'s rows before an order's line items
//! arrive. `J⁺` and `ΔJ` are therefore computed at routing: `J⁺`'s rows are
//! pushed into its batch, and `ΔJ`'s keys are `L⁻`'s requests. `ΔJ` is not
//! handed to the circuit, which computes its own through its join. The routing
//! of the three stored readers is [`route`]; this module adds only what `J`
//! needs (a hook of the generic reader machinery, which has no notion of a
//! join).
//!
//! The circuit's driving deltas are the batches that `capture` built, moved out
//! ([`Q3State::take_driving_deltas`]); no after read shares them. A partner
//! read errors on a delta key outside its reader's requests: every delta key
//! lies in a request, and a request outside the touched groups breaks that
//! contract.

use std::{
    ops::AddAssign,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, MutexGuard, OnceLock,
    },
    time::{Duration, Instant},
};

use dbsp::{
    circuit::RootCircuit,
    dynamic::DowncastTrait,
    storage::write_attribution::WriteOwner,
    trace::{
        merged_index::{
            capture, route, Capture, Delta, DeltaCursor, DeltaReader, DrivingDelta,
            MergedIndexBatch, OrderedKey, Pass, Position, Reader, ReaderDefinition, ScanSession,
            SessionCounts, StoredReader,
        },
        with_read_owner, BatchReader as DynamicBatchReader, Cursor, RunReads,
        SpineSnapshot as DynamicSnapshot,
    },
    typed_batch::{BatchReader, SpineSnapshot},
    utils::{Tup1, Tup2, Tup3, Tup4},
    DBData, OrdIndexedZSet, OrdZSet, Stream, ZWeight,
};
use feldera_sqllib::{Date, SqlDecimal, SqlString};

use super::{
    prefix_custkey, prefix_custkey_orderkey, CustomerOrdersLineitemDefinition, Key, Payload,
    INDEX_ID_CUSTOMER_BY_CUSTKEY, INDEX_ID_LINEITEM_BY_CUSTKEY_ORDERKEY_LINENUMBER,
    INDEX_ID_ORDERS_BY_CUSTKEY_ORDERKEY, TAG_CUSTKEY, TAG_ORDERKEY,
};

// A *vector* is DBSP's unit of pipelining: a weighted, key-sorted set of rows
// that flows between operators. It is the counterpart of an X100 vector, not
// of a Volcano row.

/// The customers, indexed by `custkey`.
pub type CustomerByCustkeyVector = OrdIndexedZSet<Tup1<i64>, Tup1<SqlString>>;
/// The orders, indexed by their customer: `(orderkey, orderdate, shippriority)`.
pub type OrdersByCustkeyVector = OrdIndexedZSet<Tup1<i64>, Tup3<i64, Date, i32>>;
/// The joined orders `J`, indexed by `(custkey, orderkey)`, the node at which
/// `V` reads them: `(mktsegment, orderdate, shippriority)`.
pub type JoinedByCustkeyOrderkeyVector = OrdIndexedZSet<Tup2<i64, i64>, Tup3<SqlString, Date, i32>>;
/// The line items, indexed by `(custkey, orderkey)`.
pub type LineitemByCustkeyOrderkeyVector =
    OrdIndexedZSet<Tup2<i64, i64>, Tup4<i32, SqlDecimal<15, 2>, SqlDecimal<4, 2>, Date>>;

type LineValue = Tup4<i32, SqlDecimal<15, 2>, SqlDecimal<4, 2>, Date>;

pub type SourceSnapshot = DynamicSnapshot<MergedIndexBatch>;

type Definition = CustomerOrdersLineitemDefinition;

/// Run reads per read owner, indexed like [`WriteOwner::ALL`].
pub type OwnerReads = [RunReads; WriteOwner::ALL.len()];

/// What one transaction's view maintenance read from the index: the scan
/// session's moves, and the run reads they cost on the thread that collected.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SessionReads {
    pub counts: SessionCounts,
    /// The reads of the index, under the merged index's read owner (`Dlt`'s, the capture's,
    /// included).
    pub run_reads: RunReads,
    /// Every read the collecting thread made, per owner. Besides the index's, they are the
    /// routing's reads of the driving deltas it merges alongside the scan, under the
    /// requested-slice owner. No other owner reads here.
    pub driver_reads: OwnerReads,
}

impl AddAssign for SessionReads {
    fn add_assign(&mut self, other: Self) {
        self.counts += other.counts;
        self.run_reads += other.run_reads;
        for (total, reads) in self.driver_reads.iter_mut().zip(other.driver_reads) {
            *total += reads;
        }
    }
}

/// The driving deltas of one transaction, `ΔC`, `ΔO`, and `ΔL`: the batches
/// that `capture` built from `Dlt`, indexed by the joins' keys. Every row
/// carries its trie keys.
pub struct DrivingDeltas {
    pub customers: CustomerByCustkeyVector,
    pub orders: OrdersByCustkeyVector,
    pub lines: LineitemByCustkeyOrderkeyVector,
}

// ---- The four reader definitions: the static part of each partner read ----

/// `C⁺`: the customers after the transaction, driven by `ΔO`. A path read of
/// the Customer stream at `custkey`.
pub struct CustomersAfter;

impl ReaderDefinition for CustomersAfter {
    type Index = Definition;
    type Key = Tup1<i64>;
    type Val = Tup1<SqlString>;
    type DeltaKey = Tup1<i64>;
    type DeltaVal = Tup3<i64, Date, i32>;
    const PARTNER_STREAM: Option<u8> = Some(INDEX_ID_CUSTOMER_BY_CUSTKEY);
    const REQUEST_DEPTH: usize = 1;
    const DRIVING_DELTA: DrivingDelta = DrivingDelta::Stream(INDEX_ID_ORDERS_BY_CUSTKEY_ORDERKEY);

    fn unfold(key: &Key, payload: &Payload) -> Result<(Self::Key, Self::Val), String> {
        match (key, payload) {
            (Key::CustomerByCustkey(key), Payload::CustomerByCustkey(payload)) => {
                Ok((Tup1(key.custkey), Tup1(payload.c_mktsegment.clone())))
            }
            other => Err(format!("expected a Customer entry, found {other:?}")),
        }
    }

    fn unfold_delta(
        key: &Key,
        payload: &Payload,
    ) -> Result<(Self::DeltaKey, Self::DeltaVal), String> {
        unfold_order_by_custkey(key, payload)
    }
}

/// `O⁻`: the orders before the transaction, driven by `ΔC`. A range read of
/// the Orders stream below the customer.
pub struct OrdersBefore;

impl ReaderDefinition for OrdersBefore {
    type Index = Definition;
    type Key = Tup1<i64>;
    type Val = Tup3<i64, Date, i32>;
    type DeltaKey = Tup1<i64>;
    type DeltaVal = Tup1<SqlString>;
    const PARTNER_STREAM: Option<u8> = Some(INDEX_ID_ORDERS_BY_CUSTKEY_ORDERKEY);
    const REQUEST_DEPTH: usize = 1;
    const DRIVING_DELTA: DrivingDelta = DrivingDelta::Stream(INDEX_ID_CUSTOMER_BY_CUSTKEY);

    fn unfold(key: &Key, payload: &Payload) -> Result<(Self::Key, Self::Val), String> {
        unfold_order_by_custkey(key, payload)
    }

    fn unfold_delta(
        key: &Key,
        payload: &Payload,
    ) -> Result<(Self::DeltaKey, Self::DeltaVal), String> {
        CustomersAfter::unfold(key, payload)
    }
}

/// `J⁺`: the joined orders after the transaction, driven by `ΔL`. `J` is not
/// stored: its rows are built at routing, from the customer and the order of
/// each requested `(custkey, orderkey)` after the transaction.
pub struct JoinedAfter;

impl ReaderDefinition for JoinedAfter {
    type Index = Definition;
    type Key = Tup2<i64, i64>;
    type Val = Tup3<SqlString, Date, i32>;
    type DeltaKey = Tup2<i64, i64>;
    type DeltaVal = LineValue;
    const PARTNER_STREAM: Option<u8> = None;
    const REQUEST_DEPTH: usize = 2;
    const DRIVING_DELTA: DrivingDelta =
        DrivingDelta::Stream(INDEX_ID_LINEITEM_BY_CUSTKEY_ORDERKEY_LINENUMBER);

    fn unfold_delta(
        key: &Key,
        payload: &Payload,
    ) -> Result<(Self::DeltaKey, Self::DeltaVal), String> {
        unfold_line(key, payload)
    }
}

/// `L⁻`: the line items before the transaction, driven by `ΔJ`. A range read
/// of the Lineitem stream below the order. Its driving delta is a join: only
/// the requests exist, and the routing computes them.
pub struct LineitemsBefore;

impl ReaderDefinition for LineitemsBefore {
    type Index = Definition;
    type Key = Tup2<i64, i64>;
    type Val = LineValue;
    type DeltaKey = Tup2<i64, i64>;
    type DeltaVal = Tup3<SqlString, Date, i32>;
    const PARTNER_STREAM: Option<u8> = Some(INDEX_ID_LINEITEM_BY_CUSTKEY_ORDERKEY_LINENUMBER);
    const REQUEST_DEPTH: usize = 2;
    const DRIVING_DELTA: DrivingDelta = DrivingDelta::Join;

    fn unfold(key: &Key, payload: &Payload) -> Result<(Self::Key, Self::Val), String> {
        unfold_line(key, payload)
    }
}

/// A row of a reader's batch, or why the entry is not one.
type Unfolded<K, V> = Result<(K, V), String>;

fn unfold_order_by_custkey(
    key: &Key,
    payload: &Payload,
) -> Unfolded<Tup1<i64>, Tup3<i64, Date, i32>> {
    match (key, payload) {
        (Key::OrdersByCustkeyOrderkey(key), Payload::OrdersByCustkeyOrderkey(payload)) => Ok((
            Tup1(key.custkey),
            Tup3(key.orderkey, payload.o_orderdate, payload.o_shippriority),
        )),
        other => Err(format!("expected an Orders entry, found {other:?}")),
    }
}

fn unfold_line(key: &Key, payload: &Payload) -> Result<(Tup2<i64, i64>, LineValue), String> {
    match (key, payload) {
        (
            Key::LineitemByCustkeyOrderkeyLinenumber(key),
            Payload::LineitemByCustkeyOrderkeyLinenumber(payload),
        ) => Ok((
            Tup2(key.custkey, key.orderkey),
            Tup4(
                key.linenumber,
                payload.l_extendedprice,
                payload.l_discount,
                payload.l_shipdate,
            ),
        )),
        other => Err(format!("expected a Lineitem entry, found {other:?}")),
    }
}

// ---- Routing: what J needs beyond the generic reader machinery ----

const WEIGHT_OVERFLOW: &str = "Q3 reconstructed join weight overflow";

/// The rows of one customer's or one order's side of `J`: values with their
/// weights.
type Weighted<V> = Vec<(V, ZWeight)>;

/// Sums the weights of equal values and drops the zero sums, in value order.
fn consolidate<V: Ord>(mut rows: Weighted<V>) -> Result<Weighted<V>, String> {
    rows.sort_by(|left, right| left.0.cmp(&right.0));
    let mut merged: Weighted<V> = Vec::with_capacity(rows.len());
    for (value, weight) in rows {
        match merged.last_mut() {
            Some((last, total)) if *last == value => {
                *total = total.checked_add(weight).ok_or(WEIGHT_OVERFLOW)?;
            }
            _ => merged.push((value, weight)),
        }
    }
    merged.retain(|(_, weight)| *weight != 0);
    Ok(merged)
}

fn multiply(left: ZWeight, right: ZWeight) -> Result<ZWeight, String> {
    left.checked_mul(right)
        .ok_or_else(|| WEIGHT_OVERFLOW.to_owned())
}

/// The length of a group prefix: the customer field's tag and value.
const GROUP_WIDTH: usize = 1 + <i64 as OrderedKey>::WIDTH;

/// The customer of a group prefix.
fn custkey_of_group(group: &[u8]) -> Result<i64, String> {
    match group {
        [tag, value @ ..] if *tag == TAG_CUSTKEY => <i64 as OrderedKey>::decode_ordered(value)
            .ok_or_else(|| format!("group prefix {group:?} has a malformed custkey")),
        _ => Err(format!(
            "group prefix {group:?} does not start with the custkey tag"
        )),
    }
}

/// The order of a key below an order node: the customer field, then the
/// orderkey field.
fn orderkey_of(key: &[u8]) -> Result<i64, String> {
    match key.get(GROUP_WIDTH..GROUP_WIDTH + 1 + <i64 as OrderedKey>::WIDTH) {
        Some([tag, value @ ..]) if *tag == TAG_ORDERKEY => {
            <i64 as OrderedKey>::decode_ordered(value)
                .ok_or_else(|| format!("key {key:?} has a malformed orderkey"))
        }
        _ => Err(format!("key {key:?} is not below an order node")),
    }
}

fn segment_of(payload: &Payload) -> Result<SqlString, String> {
    match payload {
        Payload::CustomerByCustkey(payload) => Ok(payload.c_mktsegment.clone()),
        other => Err(format!("expected a Customer payload, found {other:?}")),
    }
}

fn order_day_and_priority(payload: &Payload) -> Result<Tup2<Date, i32>, String> {
    match payload {
        Payload::OrdersByCustkeyOrderkey(payload) => {
            Ok(Tup2(payload.o_orderdate, payload.o_shippriority))
        }
        other => Err(format!("expected an Orders payload, found {other:?}")),
    }
}

/// One touched customer's group as the routing sees it.
struct Group {
    custkey: i64,
    prefix: Vec<u8>,
    /// `ΔC`'s rows of this customer, and `ΔO`'s rows of it by ascending
    /// orderkey, which `next_delta_order` walks.
    delta_customers: Weighted<Tup1<SqlString>>,
    delta_orders: Weighted<Tup3<i64, Date, i32>>,
    next_delta_order: usize,
    /// Set once the customer's step is done. `customer_after` is the
    /// customer's rows after the transaction, when someone needs them.
    customer_done: bool,
    customer_after: Weighted<SqlString>,
}

/// The four readers, the two driving deltas whose rows the routing reads, and the routing that
/// computes `J⁺` and `ΔJ`. The deltas are borrowed: the caller of `capture` owns them.
struct Routing<'r> {
    /// `ΔC`, which `O⁻` requests, and `ΔO`, which `C⁺` requests.
    delta_customers: DeltaCursor<'r, OrdersBefore>,
    delta_orders: DeltaCursor<'r, CustomersAfter>,
    customers: &'r mut Reader<CustomersAfter>,
    orders: &'r mut Reader<OrdersBefore>,
    joined: &'r mut Reader<JoinedAfter>,
    lines: &'r mut Reader<LineitemsBefore>,
}

impl Routing<'_> {
    /// Scans every touched group of `session`.
    fn scan(&mut self, session: &mut ScanSession<'_, Definition>) -> Result<(), String> {
        while let Some(prefix) = session.next_group() {
            let mut group = self.begin(prefix)?;
            while session.key().is_some() {
                {
                    let mut position = Position::at(session);
                    self.hook(&mut group, &mut position)?;
                    let readers: &mut [&mut dyn StoredReader<Definition>] =
                        &mut [&mut *self.customers, &mut *self.orders, &mut *self.lines];
                    route(&mut position, readers)?;
                }
                session.step();
            }
            self.end(&mut group)?;
        }
        Ok(())
    }

    fn begin(&mut self, prefix: &[u8]) -> Result<Group, String> {
        let custkey = custkey_of_group(prefix)?;
        // `ΔC` and `ΔO` have rows in this group only if `C⁺` and `O⁻` request it.
        let delta_customers = if self.orders.requests_mut().has(prefix) {
            self.delta_customers.rows_at(&Tup1(custkey))
        } else {
            Vec::new()
        };
        let delta_orders = if self.customers.requests_mut().has(prefix) {
            self.delta_orders.rows_at(&Tup1(custkey))
        } else {
            Vec::new()
        };
        Ok(Group {
            custkey,
            prefix: prefix.to_vec(),
            delta_customers,
            delta_orders,
            next_delta_order: 0,
            customer_done: false,
            customer_after: Vec::new(),
        })
    }

    /// What the join adds at a position of `Old`, before the stored readers
    /// route it: the customer step at the customer's entry, and the order step
    /// at each order, including the orders that only `ΔO` mentions, which
    /// have no entry in `Old` and are stepped in key order between entries.
    fn hook(
        &mut self,
        group: &mut Group,
        position: &mut Position<'_, '_, Definition>,
    ) -> Result<(), String> {
        let key = position.key().ok_or("a routed position has a key")?;
        let stream = *key.last().ok_or("a folded key is not empty")?;
        match stream {
            INDEX_ID_CUSTOMER_BY_CUSTKEY => self.customer(group, Some(position)),
            INDEX_ID_ORDERS_BY_CUSTKEY_ORDERKEY => {
                let order = orderkey_of(key)?;
                self.customer_if_absent(group)?;
                self.orders_before(group, order, false)?;
                self.order(group, order, Some(position))
            }
            INDEX_ID_LINEITEM_BY_CUSTKEY_ORDERKEY_LINENUMBER => {
                // A line item may follow no order entry of `Old`, when `ΔO` inserts its order.
                let order = orderkey_of(key)?;
                self.customer_if_absent(group)?;
                self.orders_before(group, order, true)
            }
            other => Err(format!("unknown index stream {other}")),
        }
    }

    /// The end of a group: the customer and the orders that no entry of
    /// `Old` carried.
    fn end(&mut self, group: &mut Group) -> Result<(), String> {
        self.customer_if_absent(group)?;
        self.orders_before(group, i64::MAX, true)
    }

    fn customer_if_absent(&mut self, group: &mut Group) -> Result<(), String> {
        if group.customer_done {
            Ok(())
        } else {
            self.customer(group, None)
        }
    }

    /// The customer's step: computes its rows after the transaction (`Old`'s
    /// plus `ΔC`'s) if `ΔO` or `ΔL` is in the group, and gives `ΔC`'s rows to
    /// `C⁺`. `old` is the customer's entry, or `None` if `Old` has none.
    fn customer(
        &mut self,
        group: &mut Group,
        old: Option<&mut Position<'_, '_, Definition>>,
    ) -> Result<(), String> {
        group.customer_done = true;
        let delta_orders_here = !group.delta_orders.is_empty();
        let lines_here = self.joined.requests_mut().has_below(&group.prefix);
        if !delta_orders_here && !lines_here {
            return Ok(());
        }

        let mut rows: Weighted<SqlString> = Vec::new();
        let has_old = old.is_some();
        if let Some(position) = old {
            for (payload, weight) in position.decoded()? {
                rows.push((segment_of(payload)?, *weight));
            }
        }
        for (Tup1(segment), weight) in &group.delta_customers {
            rows.push((segment.clone(), *weight));
            if delta_orders_here {
                // `C⁺`'s slice is `Old`'s row, which `route` pushes, and `ΔC`'s rows: the three rows
                // of one entry at most.
                self.customers
                    .push_row(Tup1(group.custkey), Tup1(segment.clone()), *weight)?;
            }
        }
        if delta_orders_here && !has_old {
            // `Old` has no entry of this customer for `route` to add to the open entry and
            // close, so the entry of `ΔC`'s rows ends here.
            self.customers.end_entry()?;
        }
        group.customer_after = consolidate(rows)?;
        Ok(())
    }

    /// Steps the orders that only `ΔO` mentions: those with an orderkey below
    /// `order`, or through it when `inclusive`.
    fn orders_before(
        &mut self,
        group: &mut Group,
        order: i64,
        inclusive: bool,
    ) -> Result<(), String> {
        while let Some((Tup3(next, ..), _)) = group.delta_orders.get(group.next_delta_order) {
            if *next > order || (*next == order && !inclusive) {
                break;
            }
            let next = *next;
            self.order(group, next, None)?;
        }
        Ok(())
    }

    /// The order step at `(group, order)`: builds `J⁺`'s rows if `ΔL` is at
    /// the order, and requests the order's line items for `L⁻` if `ΔJ` has a
    /// row at it. `old` is the order's entry in `Old`, or `None` if `Old` has
    /// none. Orders arrive in ascending order, each once.
    fn order(
        &mut self,
        group: &mut Group,
        order: i64,
        old: Option<&mut Position<'_, '_, Definition>>,
    ) -> Result<(), String> {
        let prefix = prefix_custkey_orderkey(group.custkey, order);
        let delta_here = take_delta_orders(group, order)?;
        let lines_here = self.joined.requests_mut().has(&prefix);
        let customer_delta_here = !group.delta_customers.is_empty();

        let mut order_before: Weighted<Tup2<Date, i32>> = Vec::new();
        if let (Some(position), true) = (old, customer_delta_here || lines_here) {
            for (payload, weight) in position.decoded()? {
                order_before.push((order_day_and_priority(payload)?, *weight));
            }
        }

        if lines_here {
            // `J⁺`: the customer after, times the order after.
            let order_after = consolidate(
                order_before
                    .iter()
                    .chain(&delta_here)
                    .map(|(value, weight)| (*value, *weight))
                    .collect(),
            )?;
            for (segment, customer_weight) in &group.customer_after {
                for (Tup2(date, priority), order_weight) in &order_after {
                    self.joined.push_ordered_row(
                        Tup2(group.custkey, order),
                        Tup3(segment.clone(), *date, *priority),
                        multiply(*customer_weight, *order_weight)?,
                    )?;
                }
            }
        }

        // `ΔJ` at the order: `ΔO ⋈ C⁺` plus `O⁻ ⋈ ΔC`. It has a row there only
        // if the two terms do not cancel.
        let mut delta_joined: Weighted<Tup3<SqlString, Date, i32>> = Vec::new();
        for (Tup2(date, priority), delta_weight) in &delta_here {
            for (segment, customer_weight) in &group.customer_after {
                delta_joined.push((
                    Tup3(segment.clone(), *date, *priority),
                    multiply(*delta_weight, *customer_weight)?,
                ));
            }
        }
        for (Tup2(date, priority), order_weight) in &order_before {
            for (Tup1(segment), delta_weight) in &group.delta_customers {
                delta_joined.push((
                    Tup3(segment.clone(), *date, *priority),
                    multiply(*order_weight, *delta_weight)?,
                ));
            }
        }
        if !consolidate(delta_joined)?.is_empty() {
            self.lines.request(&prefix)?;
        }
        Ok(())
    }
}

/// Takes `ΔO`'s rows of `order` out of the group's walk, as `(date, priority)`
/// values. The walk is at the first row of `order` or below it, so a row of a
/// smaller order left behind is an error.
fn take_delta_orders(group: &mut Group, order: i64) -> Result<Weighted<Tup2<Date, i32>>, String> {
    let mut rows = Vec::new();
    while let Some((Tup3(next, date, priority), weight)) =
        group.delta_orders.get(group.next_delta_order)
    {
        match (*next).cmp(&order) {
            std::cmp::Ordering::Less => {
                return Err(format!(
                    "order {next} of ΔO was left behind by the walk at order {order}"
                ));
            }
            std::cmp::Ordering::Greater => break,
            std::cmp::Ordering::Equal => {
                rows.push((Tup2(*date, *priority), *weight));
                group.next_delta_order += 1;
            }
        }
    }
    Ok(rows)
}

// ---- The state of one transaction ----

/// The readers of one transaction, after collection, and what collecting
/// them read.
struct CollectedReaders {
    customers: Reader<CustomersAfter>,
    orders: Reader<OrdersBefore>,
    joined: Reader<JoinedAfter>,
    lines: Reader<LineitemsBefore>,
    reads: SessionReads,
    /// Until [`Q3State::take_driving_deltas`] hands them over.
    driving: Option<DrivingDeltas>,
}

/// Checks that a delta key's request prefix is one of `reader`'s requests.
fn require_requested<R: ReaderDefinition>(
    reader: &Reader<R>,
    prefix: &[u8],
    what: &str,
    reader_name: &str,
) -> Result<(), String> {
    if reader.requests().contains(prefix) {
        Ok(())
    } else {
        Err(format!(
            "{what} is outside the requests of {reader_name} ({} requests): its driving delta has no row for it",
            reader.requests().len()
        ))
    }
}

struct ReadViews {
    old: SourceSnapshot,
    dlt: Arc<MergedIndexBatch>,
    /// Set once by [`Q3State::collect`]. The readers' builders are not `Sync`,
    /// so the readers sit behind a lock that the joins' callbacks take.
    collected: OnceLock<Mutex<CollectedReaders>>,
}

/// One externally prepared transaction's index views and readers, shared by
/// the view circuit's source and joins.
///
/// Publish `Old`, the index snapshot before the transaction, and `Dlt`, the
/// run of the transaction's signed index changes, after index maintenance and
/// before the view transaction. [`Self::collect`] then fills the readers; the
/// driver takes the driving deltas for the circuit's input handles
/// ([`Self::take_driving_deltas`]), and the joins take the partner reads.
/// Keep the publication unchanged until `DBSPHandle::transaction` or the
/// equivalent multi-step commit has completed, then call [`Self::finish`]: the
/// readers are held until then, since `J` and `V` are separate operators and
/// `V` flushes only after `J` has passed every customer. The caller must
/// restore the index, base state, and the view circuit at the same completed
/// transaction.
///
/// The partner reads can check the contract that every key of the circuit's delta is one of
/// their reader's requests ([`Self::enable_contract_check`]). The check walks the circuit's
/// delta, so it is off by default and the measured driver never enables it; tests and the
/// harness's validation runs do.
#[derive(Clone, Default)]
pub struct Q3State {
    views: Arc<Mutex<Option<Arc<ReadViews>>>>,
    check_contract: Arc<AtomicBool>,
}

impl Q3State {
    /// Makes every partner read check its contract (all clones of this state share the
    /// setting): each key of the circuit's delta must be one of its reader's requests, or the
    /// read errors. The check reads the circuit's delta once more, so a measured run must not
    /// enable it.
    pub fn enable_contract_check(&self) {
        self.check_contract.store(true, Ordering::Relaxed);
    }

    /// Checks `keys` (the keys of a circuit delta, read only if the check is on) against `reader`.
    fn check_requested<R: ReaderDefinition, K: DBData, V: DBData>(
        &self,
        reader: &Reader<R>,
        delta: &SpineSnapshot<OrdIndexedZSet<K, V>>,
        prefix: impl Fn(&K) -> (Vec<u8>, String),
        reader_name: &str,
    ) -> Result<(), String> {
        if !self.check_contract.load(Ordering::Relaxed) {
            return Ok(());
        }
        for key in requested_keys(delta) {
            let (prefix, what) = prefix(&key);
            require_requested(reader, &prefix, &what, reader_name)?;
        }
        Ok(())
    }

    pub fn publish(&self, old: SourceSnapshot, dlt: Arc<MergedIndexBatch>) -> Result<(), String> {
        let mut views = self.views.lock().map_err(|_| "Q3 state lock poisoned")?;
        if views.is_some() {
            return Err("finish the previous Q3 transaction before publishing another".into());
        }
        *views = Some(Arc::new(ReadViews {
            old,
            dlt,
            collected: OnceLock::new(),
        }));
        Ok(())
    }

    /// Reads `Dlt` and `Old` once each and fills the readers, which are kept
    /// until [`Self::finish`]. The index is read here, on the calling thread
    /// under the merged index's read owner, and nowhere else; the caller binds
    /// the runtime's buffer cache to this thread.
    ///
    /// Returns the time spent preparing the driving deltas (the capture of
    /// `Dlt` and the sealing of `ΔC`, `ΔO`, and `ΔL`), which the harness books
    /// as the preparation of the view's transaction delta, apart from the scan.
    pub fn collect(&self, pass: Pass) -> Result<Duration, String> {
        let views = self.views()?;
        let before = WriteOwner::ALL.map(RunReads::current_for);
        let (collected, driving, counts, delta_preparation) =
            with_read_owner(WriteOwner::MergedIndex, || {
                let mut customers = Reader::<CustomersAfter>::new()?;
                let mut orders = Reader::<OrdersBefore>::new()?;
                let mut joined = Reader::<JoinedAfter>::new()?;
                let mut lines = Reader::<LineitemsBefore>::new()?;

                // The driving deltas: `ΔO` of `C⁺`, `ΔC` of `O⁻`, and `ΔL` of `J⁺`. `L⁻`'s is a
                // join, so it has requests only. The readers keep the requests, not the deltas.
                let mut delta_orders = Delta::<CustomersAfter>::new()?;
                let mut delta_customers = Delta::<OrdersBefore>::new()?;
                let mut delta_lines = Delta::<JoinedAfter>::new()?;
                let drivers: &mut [&mut dyn DeltaReader<Definition>] = &mut [
                    &mut Capture::new(&mut customers, &mut delta_orders),
                    &mut Capture::new(&mut orders, &mut delta_customers),
                    &mut Capture::new(&mut joined, &mut delta_lines),
                ];
                let capture_started = Instant::now();
                let captured = capture::<Definition>(&views.dlt, drivers)?;
                let mut delta_preparation = capture_started.elapsed();

                let mut session =
                    ScanSession::<Definition>::new(&views.old, &captured.touched, pass);
                Routing {
                    delta_customers: delta_customers.cursor()?,
                    delta_orders: delta_orders.cursor()?,
                    customers: &mut customers,
                    orders: &mut orders,
                    joined: &mut joined,
                    lines: &mut lines,
                }
                .scan(&mut session)?;
                let counts = SessionCounts {
                    payload_decodes_dlt: captured.payload_decodes_dlt,
                    ..session.counts()
                };
                drop(session);

                for reader in [
                    customers.seal_partner(),
                    orders.seal_partner(),
                    joined.seal_partner(),
                    lines.seal_partner(),
                ] {
                    reader?;
                }
                // The deltas are moved out, not cloned: no after read shares them.
                let seal_started = Instant::now();
                let driving = DrivingDeltas {
                    customers: delta_customers.into_batch()?,
                    orders: delta_orders.into_batch()?,
                    lines: delta_lines.into_batch()?,
                };
                delta_preparation += seal_started.elapsed();
                Ok::<_, String>((
                    (customers, orders, joined, lines),
                    driving,
                    counts,
                    delta_preparation,
                ))
            })?;
        let after = WriteOwner::ALL.map(RunReads::current_for);
        let driver_reads: OwnerReads = std::array::from_fn(|i| after[i].since(before[i]));
        let run_reads = driver_reads[WriteOwner::MergedIndex as usize];

        let (customers, orders, joined, lines) = collected;
        views
            .collected
            .set(Mutex::new(CollectedReaders {
                customers,
                orders,
                joined,
                lines,
                reads: SessionReads {
                    counts,
                    run_reads,
                    driver_reads,
                },
                driving: Some(driving),
            }))
            .map_err(|_| "the published views were already collected".to_owned())?;
        Ok(delta_preparation)
    }

    /// Release the readers and published handles after all transaction
    /// consumers finish, and return what collecting them read from the index
    /// (nothing, if the transaction never collected).
    pub fn finish(&self) -> Result<SessionReads, String> {
        let mut views = self.views.lock().map_err(|_| "Q3 state lock poisoned")?;
        let views = views
            .take()
            .ok_or("no Q3 transaction views were published")?;
        match views.collected.get() {
            Some(collected) => Ok(collected
                .lock()
                .map_err(|_| "Q3 readers lock poisoned")?
                .reads),
            None => Ok(SessionReads::default()),
        }
    }

    /// The published `Old` snapshot.
    pub fn old(&self) -> Result<SourceSnapshot, String> {
        Ok(self.views()?.old.clone())
    }

    /// The published `Dlt` run.
    pub fn dlt(&self) -> Result<Arc<MergedIndexBatch>, String> {
        Ok(self.views()?.dlt.clone())
    }

    fn views(&self) -> Result<Arc<ReadViews>, String> {
        self.views
            .lock()
            .map_err(|_| "Q3 state lock poisoned")?
            .clone()
            .ok_or_else(|| "publish merged-index views before Q3 maintenance".into())
    }

    /// Hands over the transaction's driving deltas, once, for the view
    /// circuit's input handles.
    pub fn take_driving_deltas(&self) -> Result<DrivingDeltas, String> {
        let views = self.views()?;
        let mut collected = views.collected_readers()?;
        collected
            .driving
            .take()
            .ok_or_else(|| "the driving deltas were already taken".to_owned())
    }

    /// `C⁺`: the customers after the transaction, driven by `ΔO`: for each
    /// customer of `ΔO`, `Old`'s row and `ΔC`'s rows, consolidated.
    pub fn customers_after(
        &self,
        delta: &SpineSnapshot<OrdersByCustkeyVector>,
    ) -> Result<SpineSnapshot<CustomerByCustkeyVector>, String> {
        let views = self.views()?;
        let collected = views.collected_readers()?;
        self.check_requested(
            &collected.customers,
            delta,
            |Tup1(customer)| (prefix_custkey(*customer), format!("customer {customer}")),
            "C⁺",
        )?;
        collected.customers.partner()
    }

    /// `O⁻`: the orders before the transaction, driven by `ΔC`: every order
    /// of each customer of `ΔC`.
    pub fn orders_before(
        &self,
        delta: &SpineSnapshot<CustomerByCustkeyVector>,
    ) -> Result<SpineSnapshot<OrdersByCustkeyVector>, String> {
        let views = self.views()?;
        let collected = views.collected_readers()?;
        self.check_requested(
            &collected.orders,
            delta,
            |Tup1(customer)| (prefix_custkey(*customer), format!("customer {customer}")),
            "O⁻",
        )?;
        collected.orders.partner()
    }

    /// `J⁺`: the joined orders at the requested `(custkey, orderkey)` nodes
    /// after the transaction, driven by `ΔL`. `J` is not stored: the rows were
    /// built at routing, from the customer after times the order after.
    pub fn joined_orders_after(
        &self,
        delta: &SpineSnapshot<LineitemByCustkeyOrderkeyVector>,
    ) -> Result<SpineSnapshot<JoinedByCustkeyOrderkeyVector>, String> {
        let views = self.views()?;
        let collected = views.collected_readers()?;
        self.check_requested(
            &collected.joined,
            delta,
            |Tup2(customer, order)| {
                (
                    prefix_custkey_orderkey(*customer, *order),
                    format!("order ({customer}, {order})"),
                )
            },
            "J⁺",
        )?;
        collected.joined.partner()
    }

    /// `L⁻`: the line items before the transaction, driven by `ΔJ`: every
    /// line item of each order that `ΔJ` has a row at.
    pub fn lineitems_before(
        &self,
        delta: &SpineSnapshot<JoinedByCustkeyOrderkeyVector>,
    ) -> Result<SpineSnapshot<LineitemByCustkeyOrderkeyVector>, String> {
        let views = self.views()?;
        let collected = views.collected_readers()?;
        self.check_requested(
            &collected.lines,
            delta,
            |Tup2(customer, order)| {
                (
                    prefix_custkey_orderkey(*customer, *order),
                    format!("order ({customer}, {order})"),
                )
            },
            "L⁻",
        )?;
        collected.lines.partner()
    }
}

impl ReadViews {
    fn collected_readers(&self) -> Result<MutexGuard<'_, CollectedReaders>, String> {
        self.collected
            .get()
            .ok_or_else(|| "collect the touched groups before reading them".to_owned())?
            .lock()
            .map_err(|_| "Q3 readers lock poisoned".to_owned())
    }
}

/// The keys of a driving delta, in order. This is a walk of the circuit's delta, like the join's
/// own delta side, so its reads are the operators' (`query`), not the requested slice's, though
/// the join runs the reader callback under that owner.
fn requested_keys<K: DBData, V: DBData>(delta: &SpineSnapshot<OrdIndexedZSet<K, V>>) -> Vec<K> {
    with_read_owner(WriteOwner::Query, || {
        let mut cursor = delta.inner().cursor();
        let mut keys = Vec::new();
        while cursor.key_valid() {
            keys.push(cursor.key().downcast_checked::<K>().clone());
            cursor.step_key();
        }
        keys
    })
}

/// `J = O ⋈ C` on `custkey`, with both historical operands read from the
/// collected readers. `J`'s rows are indexed by `(custkey, orderkey)`, the node
/// at which `V` reads them.
pub fn join_orders_customers<F, It>(
    orders: &Stream<RootCircuit, OrdersByCustkeyVector>,
    customers: &Stream<RootCircuit, CustomerByCustkeyVector>,
    state: Q3State,
    join: F,
) -> Stream<RootCircuit, JoinedByCustkeyOrderkeyVector>
where
    F: Fn(&Tup1<i64>, &Tup3<i64, Date, i32>, &Tup1<SqlString>) -> It + Clone + 'static,
    It: IntoIterator<Item = (Tup2<i64, i64>, Tup3<SqlString, Date, i32>)> + 'static,
{
    let before = state.clone();
    orders.join_index_with_snapshot_readers(
        customers,
        move |delta| {
            state
                .customers_after(delta)
                .expect("Q3 C+ reconstruction failed")
        },
        move |delta| {
            before
                .orders_before(delta)
                .expect("Q3 O- reconstruction failed")
        },
        join,
    )
}

/// `V = L ⋈ J` on `(custkey, orderkey)`, preserving the native delta formula.
pub fn join_lineitems_orders<F, OV>(
    lines: &Stream<RootCircuit, LineitemByCustkeyOrderkeyVector>,
    orders: &Stream<RootCircuit, JoinedByCustkeyOrderkeyVector>,
    state: Q3State,
    join: F,
) -> Stream<RootCircuit, OrdZSet<OV>>
where
    OV: DBData,
    F: Fn(
            &Tup2<i64, i64>,
            &Tup4<i32, SqlDecimal<15, 2>, SqlDecimal<4, 2>, Date>,
            &Tup3<SqlString, Date, i32>,
        ) -> OV
        + Clone
        + 'static,
{
    let before = state.clone();
    lines.join_with_snapshot_readers(
        orders,
        move |delta| {
            state
                .joined_orders_after(delta)
                .expect("Q3 J+ reconstruction failed")
        },
        move |delta| {
            before
                .lineitems_before(delta)
                .expect("Q3 L- reconstruction failed")
        },
        join,
    )
}

#[cfg(test)]
mod tests;
