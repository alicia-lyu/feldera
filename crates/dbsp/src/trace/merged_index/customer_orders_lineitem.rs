use rkyv::{Archive, Deserialize, Serialize, bytecheck};

use super::{
    BaseRelationId, DomainByteTag, FoldedKey, KeyDomain, KeyDomainId, KeyField, KeyPrimitive,
    MergedIndex, MergedIndexDefinition, PayloadBytes, SourceByteTag, SourceIndexId,
    SourceIndexSpec,
};
use crate::storage::file::{Deserializer, to_bytes};

pub(super) const CUSTOMER: SourceIndexId = SourceIndexId("CustomerPrimary");
const ORDERS: SourceIndexId = SourceIndexId("OrdersByCustomer");
const LINEITEM: SourceIndexId = SourceIndexId("LineitemByCustomer");
pub(super) const CUSTOMER_DOMAIN: KeyDomainId = KeyDomainId("customer");
const ORDER_DOMAIN: KeyDomainId = KeyDomainId("order");
const LINE_DOMAIN: KeyDomainId = KeyDomainId("line");

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

pub(super) const CUSTOMER_KEY_DOMAIN: KeyDomain = KeyDomain {
    id: CUSTOMER_DOMAIN,
    primitive: KeyPrimitive::I32,
};
const ORDER_KEY_DOMAIN: KeyDomain = KeyDomain {
    id: ORDER_DOMAIN,
    primitive: KeyPrimitive::I32,
};
const LINE_KEY_DOMAIN: KeyDomain = KeyDomain {
    id: LINE_DOMAIN,
    primitive: KeyPrimitive::I32,
};

pub(super) const CUSTOMER_PRIMARY: SourceIndexSpec = SourceIndexSpec {
    id: CUSTOMER,
    base_relation: BaseRelationId("Customer"),
    key_fields: CUSTOMER_FIELDS,
    payload_fields: &["segment"],
};
const ORDERS_BY_CUSTOMER: SourceIndexSpec = SourceIndexSpec {
    id: ORDERS,
    base_relation: BaseRelationId("Orders"),
    key_fields: ORDERS_FIELDS,
    payload_fields: &["order_day", "ship_priority"],
};
const LINEITEM_BY_CUSTOMER: SourceIndexSpec = SourceIndexSpec {
    id: LINEITEM,
    base_relation: BaseRelationId("ExtendedLineitem"),
    key_fields: LINEITEM_FIELDS,
    payload_fields: &["ship_day", "extended_price_cents", "discount_hundredths"],
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum SourceKey {
    Customer(i32),
    Orders(i32, i32),
    Lineitem(i32, i32, i32),
}

pub(crate) struct CustomerOrdersLineitemDefinition;

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

    fn project(key: &SourceKey) -> (SourceIndexId, Vec<i32>) {
        match *key {
            SourceKey::Customer(c) => (CUSTOMER, vec![c]),
            SourceKey::Orders(c, o) => (ORDERS, vec![c, o]),
            SourceKey::Lineitem(c, o, l) => (LINEITEM, vec![c, o, l]),
        }
    }

    fn construct(source_index: SourceIndexId, fields: &[i32]) -> Option<SourceKey> {
        match (source_index, fields) {
            (CUSTOMER, [c]) => Some(SourceKey::Customer(*c)),
            (ORDERS, [c, o]) => Some(SourceKey::Orders(*c, *o)),
            (LINEITEM, [c, o, l]) => Some(SourceKey::Lineitem(*c, *o, *l)),
            _ => None,
        }
    }
}

#[derive(Archive, Serialize, Deserialize, Clone, Debug, Eq, PartialEq)]
#[archive_attr(derive(rkyv::CheckBytes))]
pub(crate) struct CustomerPayload {
    pub segment: String,
}

#[derive(Archive, Serialize, Deserialize, Clone, Debug, Eq, PartialEq)]
#[archive_attr(derive(rkyv::CheckBytes))]
pub(crate) struct OrdersPayload {
    pub order_day: i32,
    pub ship_priority: i32,
}

#[derive(Archive, Serialize, Deserialize, Clone, Debug, Eq, PartialEq)]
#[archive_attr(derive(rkyv::CheckBytes))]
pub(crate) struct LineitemPayload {
    pub ship_day: i32,
    pub extended_price_cents: i64,
    pub discount_hundredths: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum SourcePayload {
    Customer(CustomerPayload),
    Orders(OrdersPayload),
    Lineitem(LineitemPayload),
}

#[derive(Default)]
pub(crate) struct CustomerOrdersLineitemIndex {
    pub(super) base: MergedIndex<CustomerOrdersLineitemDefinition>,
}

impl CustomerOrdersLineitemIndex {
    pub fn build_batch(
        &self,
        rows: impl IntoIterator<Item = (SourceKey, SourcePayload, i64)>,
    ) -> Result<super::MergedIndexBatch, String> {
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
        super::batch::build_batch(encoded)
    }

    pub fn fold(&self, key: &SourceKey) -> Result<FoldedKey, String> {
        self.base.fold(key)
    }

    pub fn unfold(&self, key: &[u8]) -> Result<SourceKey, String> {
        self.base.unfold(key)
    }

    pub fn customer_prefix(&self, customer_id: i32) -> Vec<u8> {
        self.base.prefix(CUSTOMER, &[customer_id]).unwrap()
    }

    pub fn order_prefix(&self, customer_id: i32, order_id: i32) -> Vec<u8> {
        self.base.prefix(ORDERS, &[customer_id, order_id]).unwrap()
    }

    pub fn line_prefix(&self, customer_id: i32, order_id: i32) -> Vec<u8> {
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
        Ok(PayloadBytes(bytes.as_slice().to_vec()))
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
    let mut aligned = crate::storage::buffer_cache::FBuf::new();
    aligned.extend_from_slice(bytes);
    let archived = rkyv::check_archived_root::<T>(&aligned)
        .map_err(|error| format!("corrupt payload: {error}"))?;
    archived
        .deserialize(&mut Deserializer::default())
        .map_err(|error| format!("corrupt payload: {error}"))
}
