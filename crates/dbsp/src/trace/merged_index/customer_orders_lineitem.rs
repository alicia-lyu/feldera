use rkyv::{Archive, Deserialize, Serialize, bytecheck};

use super::{
    Domain, FoldedKey, KeyField, MergedIndex, MergedIndexDefinition, PayloadBytes, Source,
};
use crate::storage::file::{Deserializer, to_bytes};

const CUSTOMER: u8 = 1;
const ORDERS: u8 = 2;
const LINEITEM: u8 = 3;

const CUSTOMER_FIELDS: &[KeyField] = &[KeyField {
    name: "customer_id",
    domain: 0,
}];
const ORDERS_FIELDS: &[KeyField] = &[
    KeyField {
        name: "customer_id",
        domain: 0,
    },
    KeyField {
        name: "order_id",
        domain: 1,
    },
];
const LINEITEM_FIELDS: &[KeyField] = &[
    KeyField {
        name: "customer_id",
        domain: 0,
    },
    KeyField {
        name: "order_id",
        domain: 1,
    },
    KeyField {
        name: "line_id",
        domain: 2,
    },
];

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum SourceKey {
    Customer(i32),
    Orders(i32, i32),
    Lineitem(i32, i32, i32),
}

pub(crate) struct CustomerOrdersLineitemDefinition;

impl MergedIndexDefinition for CustomerOrdersLineitemDefinition {
    type Key = SourceKey;

    const DOMAINS: &'static [Domain] = &[
        Domain {
            name: "customer",
            tag: 1,
        },
        Domain {
            name: "order",
            tag: 2,
        },
        Domain {
            name: "line",
            tag: 3,
        },
    ];
    const SOURCES: &'static [Source] = &[
        Source {
            name: "Customer",
            id: CUSTOMER,
            key_fields: CUSTOMER_FIELDS,
        },
        Source {
            name: "Orders",
            id: ORDERS,
            key_fields: ORDERS_FIELDS,
        },
        Source {
            name: "ExtendedLineitem",
            id: LINEITEM,
            key_fields: LINEITEM_FIELDS,
        },
    ];

    fn project(key: &SourceKey) -> (u8, Vec<i32>) {
        match *key {
            SourceKey::Customer(c) => (CUSTOMER, vec![c]),
            SourceKey::Orders(c, o) => (ORDERS, vec![c, o]),
            SourceKey::Lineitem(c, o, l) => (LINEITEM, vec![c, o, l]),
        }
    }

    fn construct(source_id: u8, fields: &[i32]) -> Option<SourceKey> {
        match (source_id, fields) {
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
        let mut prefix = self.order_prefix(customer_id, order_id);
        prefix.push(CustomerOrdersLineitemDefinition::DOMAINS[2].tag);
        prefix
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
        self.unfold(key)?;
        match key.last().copied().unwrap() {
            CUSTOMER => decode::<CustomerPayload>(bytes).map(SourcePayload::Customer),
            ORDERS => decode::<OrdersPayload>(bytes).map(SourcePayload::Orders),
            LINEITEM => decode::<LineitemPayload>(bytes).map(SourcePayload::Lineitem),
            _ => unreachable!("unfold checked the source identifier"),
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
