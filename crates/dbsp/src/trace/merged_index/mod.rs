//! Index-owned folded keys and weighted layer-file rows.

use std::marker::PhantomData;

use crate::{
    algebra::ZWeight,
    dynamic::{DynData, DynUnit, DynWeight, Erase},
    storage::{backend::StorageError, file::writer::Writer2},
};

mod customer_orders_lineitem;
mod memory_batch;

pub(crate) use memory_batch::MergedIndexBatch;

pub(crate) use customer_orders_lineitem::{
    CustomerOrdersLineitemIndex, CustomerPayload, LineitemPayload, OrdersPayload, SourceKey,
    SourcePayload,
};

const INDEX_TAG: u8 = 0;

/// A logical base relation, independent of how this index stores its rows.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BaseRelationId(pub &'static str);

/// A selected access path for a base relation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SourceIndexId(pub &'static str);

/// A shared logical domain for an ordered key field.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct KeyDomainId(pub &'static str);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum KeyPrimitive {
    I32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct KeyDomain {
    pub id: KeyDomainId,
    pub primitive: KeyPrimitive,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct KeyField {
    pub name: &'static str,
    pub domain: KeyDomainId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SourceIndexSpec {
    pub id: SourceIndexId,
    pub base_relation: BaseRelationId,
    pub key_fields: &'static [KeyField],
    pub payload_fields: &'static [&'static str],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DomainByteTag {
    pub domain: KeyDomainId,
    pub tag: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SourceByteTag {
    pub source_index: SourceIndexId,
    pub tag: u8,
}

pub(crate) trait MergedIndexDefinition {
    type Key: Clone + Eq + std::fmt::Debug;

    const KEY_DOMAINS: &'static [KeyDomain];
    const SOURCE_INDEX_SPECS: &'static [SourceIndexSpec];
    const DOMAIN_BYTE_TAGS: &'static [DomainByteTag];
    const SOURCE_BYTE_TAGS: &'static [SourceByteTag];

    fn project(key: &Self::Key) -> (SourceIndexId, Vec<i32>);
    fn construct(source_index: SourceIndexId, fields: &[i32]) -> Option<Self::Key>;
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
        let (source_index, values) = D::project(key);
        let source = Self::source(source_index)?;
        if values.len() != source.key_fields.len() {
            return Err("source key has the wrong number of fields".into());
        }
        let mut bytes = self.prefix(source_index, &values)?;
        bytes.push(INDEX_TAG);
        bytes.push(Self::source_tag(source_index)?);
        Ok(FoldedKey(bytes))
    }

    pub fn unfold(&self, key: &[u8]) -> Result<D::Key, String> {
        let (&source_tag, body) = key
            .split_last()
            .ok_or_else(|| "empty folded key".to_string())?;
        let source_index = Self::source_index_for_tag(source_tag)?;
        let source = Self::source(source_index)?;
        if body.len() != source.key_fields.len() * 5 + 1 || body.last() != Some(&INDEX_TAG) {
            return Err("folded key has an invalid length or index tag".into());
        }
        let mut values = Vec::with_capacity(source.key_fields.len());
        for (position, field) in source.key_fields.iter().enumerate() {
            let offset = position * 5;
            if body[offset] != Self::domain_tag(field.domain)? {
                return Err("folded key has an invalid domain tag".into());
            }
            let encoded = u32::from_be_bytes(body[offset + 1..offset + 5].try_into().unwrap());
            values.push((encoded ^ 0x8000_0000) as i32);
        }
        D::construct(source_index, &values)
            .ok_or_else(|| "source definition could not construct folded key".into())
    }

    /// Returns a leading field prefix without the index tag or source id.
    pub fn prefix(&self, source_index: SourceIndexId, values: &[i32]) -> Result<Vec<u8>, String> {
        let source = Self::source(source_index)?;
        if values.len() > source.key_fields.len() {
            return Err("prefix has too many fields".into());
        }
        let mut bytes = Vec::with_capacity(values.len() * 5);
        for (field, value) in source.key_fields.iter().zip(values) {
            bytes.push(Self::domain_tag(field.domain)?);
            bytes.extend_from_slice(&((*value as u32) ^ 0x8000_0000).to_be_bytes());
        }
        Ok(bytes)
    }

    /// A prefix ending after the tag of the next field, but before its value.
    pub fn prefix_with_next_tag(
        &self,
        source_index: SourceIndexId,
        values: &[i32],
    ) -> Result<Vec<u8>, String> {
        let source = Self::source(source_index)?;
        let next_field = source
            .key_fields
            .get(values.len())
            .ok_or_else(|| "prefix has no next field".to_string())?;
        let mut bytes = self.prefix(source_index, values)?;
        bytes.push(Self::domain_tag(next_field.domain)?);
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

    fn source(id: SourceIndexId) -> Result<&'static SourceIndexSpec, String> {
        D::SOURCE_INDEX_SPECS
            .iter()
            .find(|source| source.id == id)
            .ok_or_else(|| "unknown source-index identifier".into())
    }

    fn source_tag(source_index: SourceIndexId) -> Result<u8, String> {
        let mut bindings = D::SOURCE_BYTE_TAGS
            .iter()
            .filter(|binding| binding.source_index == source_index);
        let tag = bindings
            .next()
            .ok_or_else(|| "source index has no byte tag".to_string())?
            .tag;
        if tag == INDEX_TAG
            || bindings.next().is_some()
            || D::SOURCE_BYTE_TAGS
                .iter()
                .any(|binding| binding.tag == tag && binding.source_index != source_index)
        {
            return Err("source index has an invalid byte-tag binding".into());
        }
        Ok(tag)
    }

    fn source_index_for_tag(tag: u8) -> Result<SourceIndexId, String> {
        let mut bindings = D::SOURCE_BYTE_TAGS
            .iter()
            .filter(|binding| binding.tag == tag);
        let source_index = bindings
            .next()
            .ok_or_else(|| "unknown terminal source tag".to_string())?
            .source_index;
        if tag == INDEX_TAG || bindings.next().is_some() {
            return Err("ambiguous terminal source tag".into());
        }
        Ok(source_index)
    }

    fn domain_tag(id: KeyDomainId) -> Result<u8, String> {
        let domain = D::KEY_DOMAINS
            .iter()
            .find(|domain| domain.id == id)
            .ok_or_else(|| "source references an unknown key domain".to_string())?;
        if domain.primitive != KeyPrimitive::I32 {
            return Err("unsupported key-domain primitive".into());
        }
        let mut bindings = D::DOMAIN_BYTE_TAGS
            .iter()
            .filter(|binding| binding.domain == id);
        let tag = bindings
            .next()
            .ok_or_else(|| "key domain has no byte tag".to_string())?
            .tag;
        if tag == INDEX_TAG
            || bindings.next().is_some()
            || D::DOMAIN_BYTE_TAGS
                .iter()
                .any(|binding| binding.tag == tag && binding.domain != id)
        {
            return Err("key domain has an invalid byte-tag binding".into());
        }
        Ok(tag)
    }
}

#[cfg(test)]
mod tests;
