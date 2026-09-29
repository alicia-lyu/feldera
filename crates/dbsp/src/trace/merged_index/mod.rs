//! Index-owned folded keys and weighted layer-file rows.

use std::marker::PhantomData;

use crate::{
    algebra::ZWeight,
    dynamic::{DynData, DynUnit, DynWeight, Erase},
    storage::{backend::StorageError, file::writer::Writer2},
};

mod customer_orders_lineitem;

pub(crate) use customer_orders_lineitem::{
    CustomerOrdersLineitemIndex, CustomerPayload, LineitemPayload, OrdersPayload, SourceKey,
    SourcePayload,
};

const INDEX_TAG: u8 = 0;

/// A domain's name documents why fields of different sources share its tag.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Domain {
    pub name: &'static str,
    pub tag: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct KeyField {
    pub name: &'static str,
    pub domain: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Source {
    pub name: &'static str,
    pub id: u8,
    pub key_fields: &'static [KeyField],
}

pub(crate) trait MergedIndexDefinition {
    type Key: Clone + Eq + std::fmt::Debug;

    const DOMAINS: &'static [Domain];
    const SOURCES: &'static [Source];

    fn project(key: &Self::Key) -> (u8, Vec<i32>);
    fn construct(source_id: u8, fields: &[i32]) -> Option<Self::Key>;
}

/// A complete folded key. Its byte ordering is its query ordering.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct FoldedKey(Vec<u8>);

impl FoldedKey {
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

/// Encoded payload bytes are ordered only for file equality and merging.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct PayloadBytes(Vec<u8>);

impl PayloadBytes {
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

pub(crate) struct MergedIndex<D>(PhantomData<D>);

impl<D> Default for MergedIndex<D> {
    fn default() -> Self {
        Self(PhantomData)
    }
}

impl<D: MergedIndexDefinition> MergedIndex<D> {
    pub fn fold(&self, key: &D::Key) -> Result<FoldedKey, String> {
        let (source_id, values) = D::project(key);
        let source = Self::source(source_id)?;
        if values.len() != source.key_fields.len() {
            return Err("source key has the wrong number of fields".into());
        }
        let mut bytes = self.prefix(source_id, &values)?;
        bytes.push(INDEX_TAG);
        bytes.push(source_id);
        Ok(FoldedKey(bytes))
    }

    pub fn unfold(&self, key: &[u8]) -> Result<D::Key, String> {
        let (&source_id, body) = key
            .split_last()
            .ok_or_else(|| "empty folded key".to_string())?;
        let source = Self::source(source_id)?;
        if body.len() != source.key_fields.len() * 5 + 1 || body.last() != Some(&INDEX_TAG) {
            return Err("folded key has an invalid length or index tag".into());
        }
        let mut values = Vec::with_capacity(source.key_fields.len());
        for (position, field) in source.key_fields.iter().enumerate() {
            let offset = position * 5;
            if body[offset] != Self::domain(*field)?.tag {
                return Err("folded key has an invalid domain tag".into());
            }
            let encoded = u32::from_be_bytes(body[offset + 1..offset + 5].try_into().unwrap());
            values.push((encoded ^ 0x8000_0000) as i32);
        }
        D::construct(source_id, &values)
            .ok_or_else(|| "source definition could not construct folded key".into())
    }

    /// Returns a leading field prefix without the index tag or source id.
    pub fn prefix(&self, source_id: u8, values: &[i32]) -> Result<Vec<u8>, String> {
        let source = Self::source(source_id)?;
        if values.len() > source.key_fields.len() {
            return Err("prefix has too many fields".into());
        }
        let mut bytes = Vec::with_capacity(values.len() * 5);
        for (field, value) in source.key_fields.iter().zip(values) {
            bytes.push(Self::domain(*field)?.tag);
            bytes.extend_from_slice(&((*value as u32) ^ 0x8000_0000).to_be_bytes());
        }
        Ok(bytes)
    }

    /// Adds all children for one parent, then writes the parent exactly once.
    /// The caller supplies strictly increasing parent and child byte keys.
    pub fn write_parent(
        &self,
        writer: &mut Writer2<DynData, DynUnit, DynData, DynWeight>,
        key: &FoldedKey,
        children: &[(PayloadBytes, ZWeight)],
    ) -> Result<(), StorageError> {
        assert!(
            !children.is_empty(),
            "a layer-file parent requires children"
        );
        for (payload, weight) in children {
            writer.write1((payload.0.erase(), weight.erase()))?;
        }
        writer.write0((key.0.erase(), ().erase()))
    }

    fn source(id: u8) -> Result<&'static Source, String> {
        D::SOURCES
            .iter()
            .find(|source| source.id == id)
            .ok_or_else(|| "unknown source identifier".into())
    }

    fn domain(field: KeyField) -> Result<Domain, String> {
        let domain = *D::DOMAINS
            .get(field.domain)
            .ok_or_else(|| "source references an unknown domain".to_string())?;
        if domain.tag == INDEX_TAG {
            return Err("index tag cannot be used as a field domain".into());
        }
        Ok(domain)
    }
}

#[cfg(test)]
mod tests;
