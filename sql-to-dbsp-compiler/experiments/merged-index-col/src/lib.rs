//! Concrete native Customer, Orders, and Lineitem merged-index definition.

pub mod q3;

use feldera_macros::{IsNone, OrdRepr};
use feldera_sqllib::{Date, SqlDecimal, SqlString};
use rkyv::{Archive, Deserialize, Serialize};
use size_of::SizeOf;

use dbsp::trace::merged_index::{
    ColumnRef, Domain, FoldedKey, KeyDecoder, KeyEncoder, KeyTrie, MergedIndexDefinition, Node,
    Payload, Placement, Via,
};

/// The domain tags and placement ids that the trie below derives.
/// `check_against_trie` in the tests ties these constants to the trie.
const CUSTKEY: u8 = 1;
const ORDERKEY: u8 = 2;
const LINENUMBER: u8 = 3;
const CUSTOMER_ID: u8 = 1;
const ORDERS_ID: u8 = 2;
const LINEITEM_ID: u8 = 3;

/// The trie of the design doc's spec for this index:
///
/// ```text
/// custkey { Customer; orderkey { Orders; linenumber { Lineitem [custkey via Orders] } } }
/// ```
pub const TRIE: KeyTrie = KeyTrie {
    name: "CustomerOrdersLineitem",
    domains: &[
        Domain {
            name: "custkey",
            columns: &[
                ColumnRef {
                    relation: "Customer",
                    column: "c_custkey",
                },
                ColumnRef {
                    relation: "Orders",
                    column: "o_custkey",
                },
            ],
        },
        Domain {
            name: "orderkey",
            columns: &[
                ColumnRef {
                    relation: "Orders",
                    column: "o_orderkey",
                },
                ColumnRef {
                    relation: "Lineitem",
                    column: "l_orderkey",
                },
            ],
        },
        Domain {
            name: "linenumber",
            columns: &[ColumnRef {
                relation: "Lineitem",
                column: "l_linenumber",
            }],
        },
    ],
    children: &[Node {
        domain: "custkey",
        placements: &[Placement {
            relation: "Customer",
            via: &[],
            payload: Payload::All,
        }],
        children: &[Node {
            domain: "orderkey",
            placements: &[Placement {
                relation: "Orders",
                via: &[],
                payload: Payload::Columns(&["o_orderdate", "o_shippriority"]),
            }],
            children: &[Node {
                domain: "linenumber",
                placements: &[Placement {
                    relation: "Lineitem",
                    via: &[Via {
                        domain: "custkey",
                        relation: "Orders",
                    }],
                    payload: Payload::Columns(&["l_shipdate", "l_extendedprice", "l_discount"]),
                }],
                children: &[],
            }],
        }],
    }],
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SourceKey {
    Customer(i64),
    Orders(i64, i64),
    Lineitem(i64, i64, i32),
}

pub struct CustomerOrdersLineitemDefinition;

impl MergedIndexDefinition for CustomerOrdersLineitemDefinition {
    type Key = SourceKey;

    const TRIE: KeyTrie = TRIE;

    fn fold(key: &SourceKey) -> FoldedKey {
        match *key {
            SourceKey::Customer(c) => KeyEncoder::new().field(CUSTKEY, &c).finish(CUSTOMER_ID),
            SourceKey::Orders(c, o) => KeyEncoder::new()
                .field(CUSTKEY, &c)
                .field(ORDERKEY, &o)
                .finish(ORDERS_ID),
            SourceKey::Lineitem(c, o, l) => KeyEncoder::new()
                .field(CUSTKEY, &c)
                .field(ORDERKEY, &o)
                .field(LINENUMBER, &l)
                .finish(LINEITEM_ID),
        }
    }

    fn unfold(bytes: &[u8]) -> Result<SourceKey, String> {
        let (placement_id, mut fields) = KeyDecoder::new(bytes)?;
        let key = match placement_id {
            CUSTOMER_ID => SourceKey::Customer(fields.field(CUSTKEY)?),
            ORDERS_ID => SourceKey::Orders(fields.field(CUSTKEY)?, fields.field(ORDERKEY)?),
            LINEITEM_ID => SourceKey::Lineitem(
                fields.field(CUSTKEY)?,
                fields.field(ORDERKEY)?,
                fields.field(LINENUMBER)?,
            ),
            _ => return Err(format!("unknown placement id {placement_id}")),
        };
        fields.finish()?;
        Ok(key)
    }
}

#[derive(
    Clone,
    Debug,
    Default,
    Eq,
    Hash,
    Ord,
    PartialEq,
    PartialOrd,
    SizeOf,
    Archive,
    Serialize,
    Deserialize,
    IsNone,
    OrdRepr,
)]
#[archive_attr(derive(Eq, Ord, PartialEq, PartialOrd))]
pub struct CustomerPayload {
    pub segment: SqlString,
}

#[derive(
    Clone,
    Debug,
    Default,
    Eq,
    Hash,
    Ord,
    PartialEq,
    PartialOrd,
    SizeOf,
    Archive,
    Serialize,
    Deserialize,
    IsNone,
    OrdRepr,
)]
#[archive_attr(derive(Eq, Ord, PartialEq, PartialOrd))]
pub struct OrdersPayload {
    pub order_date: Date,
    pub ship_priority: i32,
}

#[derive(
    Clone,
    Debug,
    Default,
    Eq,
    Hash,
    Ord,
    PartialEq,
    PartialOrd,
    SizeOf,
    Archive,
    Serialize,
    Deserialize,
    IsNone,
    OrdRepr,
)]
#[archive_attr(derive(Eq, Ord, PartialEq, PartialOrd))]
pub struct LineitemPayload {
    pub ship_date: Date,
    pub extended_price: SqlDecimal<15, 2>,
    pub discount: SqlDecimal<4, 2>,
}

#[derive(
    Clone,
    Debug,
    Eq,
    Hash,
    Ord,
    PartialEq,
    PartialOrd,
    SizeOf,
    Archive,
    Serialize,
    Deserialize,
    IsNone,
    OrdRepr,
)]
#[archive_attr(derive(Eq, Ord, PartialEq, PartialOrd))]
/// The stored value of a merged-index row. Its variant must match the source
/// index named by the row's folded key.
pub enum SourcePayload {
    Customer(CustomerPayload),
    Orders(OrdersPayload),
    Lineitem(LineitemPayload),
}

/// DBSP needs a default value; it is never stored.
impl Default for SourcePayload {
    fn default() -> Self {
        Self::Customer(CustomerPayload::default())
    }
}

impl SourcePayload {
    /// Whether this payload belongs to the source index that `key` names.
    pub fn matches(&self, key: &SourceKey) -> bool {
        matches!(
            (key, self),
            (SourceKey::Customer(..), SourcePayload::Customer(_))
                | (SourceKey::Orders(..), SourcePayload::Orders(_))
                | (SourceKey::Lineitem(..), SourcePayload::Lineitem(_))
        )
    }
}

pub struct CustomerOrdersLineitemIndex;

impl CustomerOrdersLineitemIndex {
    pub fn build_batch(
        &self,
        rows: impl IntoIterator<Item = (SourceKey, SourcePayload, i64)>,
    ) -> Result<dbsp::trace::merged_index::MergedIndexBatch, String> {
        let folded = rows
            .into_iter()
            .map(|(key, payload, weight)| {
                if !payload.matches(&key) {
                    return Err("payload does not match source key".into());
                }
                Ok((self.fold(&key), payload, weight))
            })
            .collect::<Result<Vec<_>, String>>()?;
        dbsp::trace::merged_index::batch::build_batch(folded)
    }

    pub fn fold(&self, key: &SourceKey) -> FoldedKey {
        CustomerOrdersLineitemDefinition::fold(key)
    }

    pub fn unfold(&self, key: &[u8]) -> Result<SourceKey, String> {
        CustomerOrdersLineitemDefinition::unfold(key)
    }

    /// The bytes shared by every key of one customer.
    pub fn prefix_custkey(&self, customer_id: i64) -> Vec<u8> {
        KeyEncoder::new().field(CUSTKEY, &customer_id).prefix()
    }

    /// The bytes shared by every key of one order.
    pub fn prefix_custkey_orderkey(&self, customer_id: i64, order_id: i64) -> Vec<u8> {
        KeyEncoder::new()
            .field(CUSTKEY, &customer_id)
            .field(ORDERKEY, &order_id)
            .prefix()
    }

    /// The bytes shared by every line of one order, through the line tag.
    pub fn prefix_custkey_orderkey_then_linenumber(
        &self,
        customer_id: i64,
        order_id: i64,
    ) -> Vec<u8> {
        KeyEncoder::new()
            .field(CUSTKEY, &customer_id)
            .field(ORDERKEY, &order_id)
            .then_tag(LINENUMBER)
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod storage_tests;
