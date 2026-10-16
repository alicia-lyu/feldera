//! Concrete native Customer, Orders, and Lineitem merged-index definition.

pub mod q3;

use feldera_macros::{IsNone, OrdRepr};
use feldera_sqllib::{Date, SqlDecimal, SqlString};
use rkyv::{Archive, Deserialize, Serialize};
use size_of::SizeOf;

use dbsp::trace::merged_index::{
    BaseRelationId, DomainByteTag, FoldedKey, KeyDomain, KeyDomainId, KeyField, KeyPrimitive,
    MergedIndex, MergedIndexDefinition, SourceByteTag, SourceIndexId, SourceIndexSpec,
};

pub const CUSTOMER: SourceIndexId = SourceIndexId("CustomerPrimary");
pub const ORDERS: SourceIndexId = SourceIndexId("OrdersByCustomer");
pub const LINEITEM: SourceIndexId = SourceIndexId("LineitemByCustomer");
pub const CUSTOMER_DOMAIN: KeyDomainId = KeyDomainId("customer");
pub const ORDER_DOMAIN: KeyDomainId = KeyDomainId("order");
pub const LINE_DOMAIN: KeyDomainId = KeyDomainId("line");

const CUSTOMER_FIELDS: &[KeyField] = &[KeyField {
    name: "customer_id",
    domain: CUSTOMER_DOMAIN,
}];
const ORDERS_FIELDS: &[KeyField] = &[
    KeyField {
        name: "customer_id",
        domain: CUSTOMER_DOMAIN,
    },
    KeyField {
        name: "order_id",
        domain: ORDER_DOMAIN,
    },
];
const LINEITEM_FIELDS: &[KeyField] = &[
    KeyField {
        name: "customer_id",
        domain: CUSTOMER_DOMAIN,
    },
    KeyField {
        name: "order_id",
        domain: ORDER_DOMAIN,
    },
    KeyField {
        name: "line_id",
        domain: LINE_DOMAIN,
    },
];

pub const CUSTOMER_KEY_DOMAIN: KeyDomain = KeyDomain {
    id: CUSTOMER_DOMAIN,
    primitive: KeyPrimitive::I64,
};
pub const ORDER_KEY_DOMAIN: KeyDomain = KeyDomain {
    id: ORDER_DOMAIN,
    primitive: KeyPrimitive::I64,
};
pub const LINE_KEY_DOMAIN: KeyDomain = KeyDomain {
    id: LINE_DOMAIN,
    primitive: KeyPrimitive::I32,
};

pub const CUSTOMER_PRIMARY: SourceIndexSpec = SourceIndexSpec {
    id: CUSTOMER,
    base_relation: BaseRelationId("Customer"),
    key_fields: CUSTOMER_FIELDS,
    payload_fields: &["segment"],
};
pub const ORDERS_BY_CUSTOMER: SourceIndexSpec = SourceIndexSpec {
    id: ORDERS,
    base_relation: BaseRelationId("Orders"),
    key_fields: ORDERS_FIELDS,
    payload_fields: &["order_date", "ship_priority"],
};
pub const LINEITEM_BY_CUSTOMER: SourceIndexSpec = SourceIndexSpec {
    id: LINEITEM,
    base_relation: BaseRelationId("ExtendedLineitem"),
    key_fields: LINEITEM_FIELDS,
    payload_fields: &["ship_date", "extended_price", "discount"],
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

    const KEY_DOMAINS: &'static [KeyDomain] =
        &[CUSTOMER_KEY_DOMAIN, ORDER_KEY_DOMAIN, LINE_KEY_DOMAIN];
    const SOURCE_INDEX_SPECS: &'static [SourceIndexSpec] =
        &[CUSTOMER_PRIMARY, ORDERS_BY_CUSTOMER, LINEITEM_BY_CUSTOMER];

    const DOMAIN_BYTE_TAGS: &'static [DomainByteTag] = &[
        DomainByteTag {
            domain: CUSTOMER_DOMAIN,
            tag: 1,
        },
        DomainByteTag {
            domain: ORDER_DOMAIN,
            tag: 2,
        },
        DomainByteTag {
            domain: LINE_DOMAIN,
            tag: 3,
        },
    ];

    const SOURCE_BYTE_TAGS: &'static [SourceByteTag] = &[
        SourceByteTag {
            source_index: CUSTOMER,
            tag: 1,
        },
        SourceByteTag {
            source_index: ORDERS,
            tag: 2,
        },
        SourceByteTag {
            source_index: LINEITEM,
            tag: 3,
        },
    ];

    fn project(key: &SourceKey) -> (SourceIndexId, Vec<i64>) {
        match *key {
            SourceKey::Customer(c) => (CUSTOMER, vec![c]),
            SourceKey::Orders(c, o) => (ORDERS, vec![c, o]),
            SourceKey::Lineitem(c, o, l) => (LINEITEM, vec![c, o, i64::from(l)]),
        }
    }

    fn construct(source_index: SourceIndexId, fields: &[i64]) -> Option<SourceKey> {
        match (source_index, fields) {
            (CUSTOMER, [c]) => Some(SourceKey::Customer(*c)),
            (ORDERS, [c, o]) => Some(SourceKey::Orders(*c, *o)),
            (LINEITEM, [c, o, l]) => Some(SourceKey::Lineitem(*c, *o, (*l).try_into().ok()?)),
            _ => None,
        }
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

#[derive(Default)]
pub struct CustomerOrdersLineitemIndex {
    pub base: MergedIndex<CustomerOrdersLineitemDefinition>,
}

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
                Ok((self.fold(&key)?, payload, weight))
            })
            .collect::<Result<Vec<_>, String>>()?;
        dbsp::trace::merged_index::batch::build_batch(folded)
    }

    pub fn fold(&self, key: &SourceKey) -> Result<FoldedKey, String> {
        self.base.fold(key)
    }

    pub fn unfold(&self, key: &[u8]) -> Result<SourceKey, String> {
        self.base.unfold(key)
    }

    pub fn customer_prefix(&self, customer_id: i64) -> Vec<u8> {
        self.base.prefix(CUSTOMER, &[customer_id]).unwrap()
    }

    pub fn order_prefix(&self, customer_id: i64, order_id: i64) -> Vec<u8> {
        self.base.prefix(ORDERS, &[customer_id, order_id]).unwrap()
    }

    pub fn line_prefix(&self, customer_id: i64, order_id: i64) -> Vec<u8> {
        self.base
            .prefix_with_next_tag(LINEITEM, &[customer_id, order_id])
            .unwrap()
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod storage_tests;
