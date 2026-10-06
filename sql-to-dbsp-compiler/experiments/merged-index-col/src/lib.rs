//! Concrete native Customer, Orders, and Lineitem merged-index definition.

pub mod q3;

use feldera_sqllib::{Date, SqlDecimal, SqlString};
use rkyv::{bytecheck, Archive, Deserialize, Serialize};

use dbsp::storage::file::{to_bytes, Deserializer};
use dbsp::trace::merged_index::{
    BaseRelationId, DomainByteTag, FoldedKey, KeyDomain, KeyDomainId, KeyField, KeyPrimitive,
    MergedIndex, MergedIndexDefinition, PayloadBytes, SourceByteTag, SourceIndexId,
    SourceIndexSpec,
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

#[derive(Archive, Serialize, Deserialize, Clone, Debug, Eq, PartialEq)]
#[archive_attr(derive(rkyv::CheckBytes))]
pub struct CustomerPayload {
    pub segment: SqlString,
}

#[derive(Archive, Serialize, Deserialize, Clone, Debug, Eq, PartialEq)]
#[archive_attr(derive(rkyv::CheckBytes))]
pub struct OrdersPayload {
    pub order_date: Date,
    pub ship_priority: i32,
}

#[derive(Archive, Serialize, Deserialize, Clone, Debug, Eq, PartialEq)]
#[archive_attr(derive(rkyv::CheckBytes))]
pub struct LineitemPayload {
    pub ship_date: Date,
    pub extended_price: SqlDecimal<15, 2>,
    pub discount: SqlDecimal<4, 2>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SourcePayload {
    Customer(CustomerPayload),
    Orders(OrdersPayload),
    Lineitem(LineitemPayload),
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
        let encoded = rows
            .into_iter()
            .map(|(key, payload, weight)| {
                let folded = self.fold(&key)?;
                let bytes = self.encode_payload(&payload)?;
                if self.decode_payload(folded.as_bytes(), bytes.as_bytes())? != payload {
                    return Err("payload does not match source key".into());
                }
                Ok((folded, bytes, weight))
            })
            .collect::<Result<Vec<_>, String>>()?;
        dbsp::trace::merged_index::batch::build_batch(encoded)
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

    pub fn encode_payload(&self, payload: &SourcePayload) -> Result<PayloadBytes, String> {
        let bytes = match payload {
            SourcePayload::Customer(value) => to_bytes(value),
            SourcePayload::Orders(value) => to_bytes(value),
            SourcePayload::Lineitem(value) => to_bytes(value),
        }
        .map_err(|error| format!("payload serialization failed: {error:?}"))?;
        Ok(PayloadBytes::from(bytes.as_slice().to_vec()))
    }

    pub fn decode_payload(&self, key: &[u8], bytes: &[u8]) -> Result<SourcePayload, String> {
        let (source_index, _) = CustomerOrdersLineitemDefinition::project(&self.unfold(key)?);
        match source_index {
            CUSTOMER => decode::<CustomerPayload>(bytes).map(SourcePayload::Customer),
            ORDERS => decode::<OrdersPayload>(bytes).map(SourcePayload::Orders),
            LINEITEM => decode::<LineitemPayload>(bytes).map(SourcePayload::Lineitem),
            _ => unreachable!("unfold returned a declared source index"),
        }
    }
}

fn decode<T>(bytes: &[u8]) -> Result<T, String>
where
    T: Archive,
    T::Archived: for<'a> bytecheck::CheckBytes<rkyv::validation::validators::DefaultValidator<'a>>
        + Deserialize<T, Deserializer>,
{
    let mut aligned = dbsp::storage::buffer_cache::FBuf::new();
    aligned.extend_from_slice(bytes);
    let archived = rkyv::check_archived_root::<T>(&aligned)
        .map_err(|error| format!("corrupt payload: {error}"))?;
    archived
        .deserialize(&mut Deserializer::default())
        .map_err(|error| format!("corrupt payload: {error}"))
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod storage_tests;
