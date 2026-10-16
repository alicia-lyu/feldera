//! Schema-independent folded keys, byte-backed batches, and weighted layer-file rows.
//! Concrete source definitions and payload codecs belong to client crates.

use std::marker::PhantomData;

use crate::{
    algebra::ZWeight,
    dynamic::{DynData, DynUnit, DynWeight, Erase},
    storage::{backend::StorageError, file::writer::Writer2},
};

pub mod batch;
pub mod ordered;

pub use batch::MergedIndexBatch;
pub use ordered::OrderedKey;

const INDEX_TAG: u8 = 0;

/// A logical base relation, independent of how this index stores its rows.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BaseRelationId(pub &'static str);

/// A selected access path for a base relation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SourceIndexId(pub &'static str);

/// A shared logical domain for an ordered key field.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KeyDomainId(pub &'static str);

/// The signed integer width used by a key domain in folded bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeyPrimitive {
    I32,
    I64,
}

impl KeyPrimitive {
    fn width(self) -> usize {
        match self {
            Self::I32 => 4,
            Self::I64 => 8,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KeyDomain {
    pub id: KeyDomainId,
    pub primitive: KeyPrimitive,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KeyField {
    pub name: &'static str,
    pub domain: KeyDomainId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SourceIndexSpec {
    pub id: SourceIndexId,
    pub base_relation: BaseRelationId,
    pub key_fields: &'static [KeyField],
    pub payload_fields: &'static [&'static str],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DomainByteTag {
    pub domain: KeyDomainId,
    pub tag: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SourceByteTag {
    pub source_index: SourceIndexId,
    pub tag: u8,
}

/// Static source metadata and typed projection for one merged index.
/// Domain and source tags must be unique within their respective namespaces; zero
/// is reserved for the terminal index tag. Field values use `i64` at this API
/// boundary, and folding checks that values in `I32` domains fit without truncation.
pub trait MergedIndexDefinition {
    type Key: Clone + Eq + std::fmt::Debug;

    const KEY_DOMAINS: &'static [KeyDomain];
    const SOURCE_INDEX_SPECS: &'static [SourceIndexSpec];
    const DOMAIN_BYTE_TAGS: &'static [DomainByteTag];
    const SOURCE_BYTE_TAGS: &'static [SourceByteTag];

    fn project(key: &Self::Key) -> (SourceIndexId, Vec<i64>);
    fn construct(source_index: SourceIndexId, fields: &[i64]) -> Option<Self::Key>;
}

/// A complete folded key. Its byte ordering is its query ordering.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct FoldedKey(Vec<u8>);

impl FoldedKey {
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

/// Encoded payload bytes are ordered only for file equality and merging.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct PayloadBytes(Vec<u8>);

/// Wraps bytes produced by a client-owned payload codec.
/// Validation and source-schema selection remain the codec's responsibility.
impl From<Vec<u8>> for PayloadBytes {
    fn from(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }
}

impl PayloadBytes {
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

/// Folding and prefix operations governed by a static index definition.
pub struct MergedIndex<D>(PhantomData<D>);

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
        let primitives = source
            .key_fields
            .iter()
            .map(|field| Self::primitive(field.domain))
            .collect::<Result<Vec<_>, _>>()?;
        let expected_length = primitives
            .iter()
            .map(|primitive| 1 + primitive.width())
            .sum::<usize>()
            + 1;
        if body.len() != expected_length || body.last() != Some(&INDEX_TAG) {
            return Err("folded key has an invalid length or index tag".into());
        }
        let mut values = Vec::with_capacity(source.key_fields.len());
        let mut offset = 0;
        for (field, primitive) in source.key_fields.iter().zip(primitives) {
            if body[offset] != Self::domain_tag(field.domain)? {
                return Err("folded key has an invalid domain tag".into());
            }
            offset += 1;
            let value = match primitive {
                KeyPrimitive::I32 => {
                    let encoded = u32::from_be_bytes(body[offset..offset + 4].try_into().unwrap());
                    i64::from((encoded ^ 0x8000_0000) as i32)
                }
                KeyPrimitive::I64 => {
                    let encoded = u64::from_be_bytes(body[offset..offset + 8].try_into().unwrap());
                    (encoded ^ 0x8000_0000_0000_0000) as i64
                }
            };
            values.push(value);
            offset += primitive.width();
        }
        D::construct(source_index, &values)
            .ok_or_else(|| "source definition could not construct folded key".into())
    }

    /// Returns a leading field prefix without the index tag or source id.
    pub fn prefix(&self, source_index: SourceIndexId, values: &[i64]) -> Result<Vec<u8>, String> {
        let source = Self::source(source_index)?;
        if values.len() > source.key_fields.len() {
            return Err("prefix has too many fields".into());
        }
        let mut bytes = Vec::new();
        for (field, value) in source.key_fields.iter().zip(values) {
            bytes.push(Self::domain_tag(field.domain)?);
            match Self::primitive(field.domain)? {
                KeyPrimitive::I32 => {
                    let value = i32::try_from(*value)
                        .map_err(|_| "key field is outside its I32 domain".to_string())?;
                    bytes.extend_from_slice(&((value as u32) ^ 0x8000_0000).to_be_bytes());
                }
                KeyPrimitive::I64 => {
                    bytes.extend_from_slice(
                        &((*value as u64) ^ 0x8000_0000_0000_0000).to_be_bytes(),
                    );
                }
            }
        }
        Ok(bytes)
    }

    /// A prefix ending after the tag of the next field, but before its value.
    pub fn prefix_with_next_tag(
        &self,
        source_index: SourceIndexId,
        values: &[i64],
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

    fn primitive(id: KeyDomainId) -> Result<KeyPrimitive, String> {
        D::KEY_DOMAINS
            .iter()
            .find(|domain| domain.id == id)
            .map(|domain| domain.primitive)
            .ok_or_else(|| "source references an unknown key domain".to_string())
    }

    fn domain_tag(id: KeyDomainId) -> Result<u8, String> {
        Self::primitive(id)?;
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
