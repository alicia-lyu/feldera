//! Disk-only encodings of data blocks that hold byte strings.
//!
//! Two independent transforms live here. Each changes only the bytes on
//! disk: the reader expands an encoded block back to the exact bytes the
//! writer built before any other code sees it, so cursors, seeks, and the
//! buffer cache work on ordinary data blocks.
//!
//! - **Front coding** applies to keys. Keys in a layer file are sorted, so
//!   neighbouring keys often share a long prefix: the merged index folds
//!   customer, order, and line identifiers into one byte string, and every
//!   line of an order repeats its first 19 bytes. A front-coded data block
//!   stores each key as the length of the prefix it shares with the previous
//!   key in the block, followed by the remaining suffix.
//! - **Value-header elision** applies to values. A `Vec<u8>` value is stored
//!   with a 16-byte `ArchivedVec` root, which is redundant on disk: its
//!   relative pointer always points at the bytes just before it, and its
//!   length follows from the item's bounds. An elided data block stores each
//!   item as the length of its bytes, the bytes, and the weight.
//!
//! # Applicability
//!
//! With rkyv's `size_64` layout, an archived `Vec<u8>` is the bytes, zero
//! padding to an 8-byte boundary, and a 16-byte root (a native-endian `i64`
//! relative pointer followed by a native-endian `u64` length).
//!
//! - Front coding understands one item layout: a key of type `Vec<u8>`
//!   paired with an auxiliary value whose archive is empty, which is the key
//!   level of an indexed batch. The item is the archived `Vec<u8>`.
//! - Value-header elision understands a `Vec<u8>` paired with an `i64`
//!   weight, which is the value level of an indexed batch. The item is the
//!   archived `Vec<u8>` followed by the 8-byte weight.
//!
//! [`encode`] and [`encode_values`] check the layout for every item, and
//! check that decoding reproduces the original block, before they return an
//! encoded block. Any other block is left as it is.
//!
//! # Encoded block format
//!
//! Both transforms share one format apart from the magic and the per-item
//! entries. All integers in the header are little-endian `u32`s.
//!
//! | Offset | Contents |
//! |---|---|
//! | 0 | checksum of bytes 4.. (the block's own checksum) |
//! | 4 | the transform's magic |
//! | 8 | length of the decoded block |
//! | 12 | checksum of the decoded block |
//! | 16 | end of the decoded block's items |
//! | 20 | length of the copied tail |
//! | 24 | bytes 8..32 of the decoded block's header |
//! | 48 | the per-item entries |
//! | ... | the decoded block from the end of its items, without trailing zeros |
//!
//! The per-item entries are:
//!
//! - **Front coding** ([`FRONT_CODED_DATA_BLOCK_MAGIC`]): the shared-prefix
//!   length and the suffix length (LEB128), then the suffix.
//! - **Value-header elision** ([`ELIDED_VALUE_HEADERS_DATA_BLOCK_MAGIC`]):
//!   the length of the bytes (LEB128), the bytes, and the weight as it was
//!   stored (8 bytes).
//!
//! The tail holds the value map and row groups. Bytes of the decoded block
//! after the tail are zero.

use crc32c::crc32c;

use super::format::{
    DATA_BLOCK_MAGIC, ELIDED_VALUE_HEADERS_DATA_BLOCK_MAGIC, FRONT_CODED_DATA_BLOCK_MAGIC,
};
use crate::storage::buffer_cache::FBuf;

/// Length of a data block's header, after which its items start.
const DATA_HEADER_LEN: usize = 32;

/// Length of the header of an encoded block.
const ENCODED_HEADER_LEN: usize = 48;

/// Length of an archived `Vec<u8>`: a relative pointer and a length.
const ROOT_LEN: usize = 16;

/// Alignment of an archived `Vec<u8>`.
const ROOT_ALIGN: usize = 8;

/// Length of an archived `i64` weight, which follows the root of a value.
const WEIGHT_LEN: usize = 8;

/// The two item layouts that this module understands.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Layout {
    /// A `Vec<u8>` key with an empty auxiliary archive.
    Keys,
    /// A `Vec<u8>` value with an `i64` weight.
    Values,
}

impl Layout {
    fn magic(self) -> [u8; 4] {
        match self {
            Layout::Keys => FRONT_CODED_DATA_BLOCK_MAGIC,
            Layout::Values => ELIDED_VALUE_HEADERS_DATA_BLOCK_MAGIC,
        }
    }

    fn from_magic(magic: &[u8]) -> Option<Self> {
        [Layout::Keys, Layout::Values]
            .into_iter()
            .find(|layout| layout.magic() == magic)
    }

    /// The length of the archive that follows an item's bytes and padding.
    fn archive_len(self) -> usize {
        match self {
            Layout::Keys => ROOT_LEN,
            Layout::Values => ROOT_LEN + WEIGHT_LEN,
        }
    }
}

/// Front-codes the keys of the data block `decoded`, whose checksum is
/// already in its first four bytes.
///
/// Returns `None`, and the caller writes `decoded` as it is, when the block
/// does not have the item layout described in the [module](self)
/// documentation. The encoded block may be longer than `decoded`; the caller
/// decides whether it saves space. Its own checksum is left as zero for the
/// caller to fill in.
pub(super) fn encode(decoded: &[u8]) -> Option<FBuf> {
    encode_layout(decoded, Layout::Keys)
}

/// Elides the `ArchivedVec` roots of the values of the data block `decoded`,
/// whose checksum is already in its first four bytes.
///
/// The contract is that of [`encode`].
pub(super) fn encode_values(decoded: &[u8]) -> Option<FBuf> {
    encode_layout(decoded, Layout::Values)
}

fn encode_layout(decoded: &[u8], layout: Layout) -> Option<FBuf> {
    let header = DecodedHeader::parse(decoded)?;
    let mut entries = Vec::with_capacity(decoded.len() / 2);
    let mut cursor = DATA_HEADER_LEN;
    let mut previous: &[u8] = &[];
    for index in 0..header.n_values {
        let root = header.item_offset(decoded, index)?;
        let bytes = item_bytes(decoded, cursor, root)?;
        match layout {
            Layout::Keys => {
                let shared = common_prefix_len(previous, bytes);
                write_leb128(&mut entries, shared);
                write_leb128(&mut entries, bytes.len() - shared);
                entries.extend_from_slice(&bytes[shared..]);
                previous = bytes;
            }
            Layout::Values => {
                let weight =
                    decoded.get(root + ROOT_LEN..root.checked_add(layout.archive_len())?)?;
                write_leb128(&mut entries, bytes.len());
                entries.extend_from_slice(bytes);
                entries.extend_from_slice(weight);
            }
        }
        cursor = root.checked_add(layout.archive_len())?;
    }
    let items_end = cursor;
    if items_end > header.tail_start {
        return None;
    }
    let tail = trim_trailing_zeros(&decoded[items_end..]);

    let mut encoded = FBuf::with_capacity(ENCODED_HEADER_LEN + entries.len() + tail.len());
    encoded.extend_from_slice(&[0; 4]);
    encoded.extend_from_slice(&layout.magic());
    for field in [decoded.len(), 0, items_end, tail.len()] {
        encoded.extend_from_slice(&u32::try_from(field).ok()?.to_le_bytes());
    }
    encoded[12..16].copy_from_slice(&decoded[..4]);
    encoded.extend_from_slice(&decoded[8..DATA_HEADER_LEN]);
    encoded.extend_from_slice(&entries);
    encoded.extend_from_slice(tail);

    // Keep the encoding only if it decodes to the original.
    match decode(&encoded) {
        Ok(roundtrip) if roundtrip.as_slice() == decoded => Some(encoded),
        _ => None,
    }
}

/// Expands the encoded block `encoded`, of either transform, back to the
/// data block it was made from, and verifies that block's checksum.
///
/// `encoded` may carry padding after its tail. Returns a description of the
/// first inconsistency found; it never panics on malformed input.
pub(super) fn decode(encoded: &[u8]) -> Result<FBuf, String> {
    if encoded.len() < ENCODED_HEADER_LEN {
        return Err(format!(
            "{} bytes is shorter than the header",
            encoded.len()
        ));
    }
    let layout = Layout::from_magic(&encoded[4..8]).ok_or("bad magic")?;
    let field = |offset: usize| {
        u32::from_le_bytes(encoded[offset..offset + 4].try_into().unwrap()) as usize
    };
    let (decoded_len, decoded_checksum, items_end, tail_len) =
        (field(8), field(12) as u32, field(16), field(20));
    let n_values = field(24);
    if decoded_len > super::reader::MAX_DECOMPRESSED_LEN {
        return Err(format!("decoded length {decoded_len} is too large"));
    }
    if !(DATA_HEADER_LEN <= items_end && items_end <= decoded_len)
        || tail_len > decoded_len - items_end
    {
        return Err(format!(
            "items end {items_end} and tail of {tail_len} bytes do not fit in {decoded_len} bytes"
        ));
    }

    let mut decoded = FBuf::with_capacity(decoded_len);
    decoded.resize(decoded_len, 0);
    decoded[..4].copy_from_slice(&decoded_checksum.to_le_bytes());
    decoded[4..8].copy_from_slice(&DATA_BLOCK_MAGIC);
    decoded[8..DATA_HEADER_LEN].copy_from_slice(&encoded[24..ENCODED_HEADER_LEN]);

    let mut input = ENCODED_HEADER_LEN;
    let mut cursor = DATA_HEADER_LEN;
    let mut previous = DATA_HEADER_LEN..DATA_HEADER_LEN;
    for index in 0..n_values {
        // Where this item's bytes come from. Front coding copies the shared
        // prefix from the previous item; elision has no shared prefix.
        let (shared, suffix_len) = match layout {
            Layout::Keys => (
                read_leb128(encoded, &mut input)?,
                read_leb128(encoded, &mut input)?,
            ),
            Layout::Values => (0, read_leb128(encoded, &mut input)?),
        };
        if shared > previous.len() {
            return Err(format!(
                "key {index} shares more than the previous key's length"
            ));
        }
        let suffix = encoded
            .get(input..)
            .and_then(|rest| rest.get(..suffix_len))
            .ok_or_else(|| format!("item {index}'s bytes run past the block"))?;
        input += suffix_len;
        let len = shared
            .checked_add(suffix_len)
            .ok_or_else(|| format!("item {index} is too long"))?;
        let root = cursor
            .checked_add(len)
            .map(|end| end.next_multiple_of(ROOT_ALIGN))
            .filter(|root| {
                root.checked_add(layout.archive_len())
                    .is_some_and(|end| end <= items_end)
            })
            .ok_or_else(|| format!("item {index} runs past the end of the items"))?;

        decoded.copy_within(previous.start..previous.start + shared, cursor);
        decoded[cursor + shared..cursor + len].copy_from_slice(suffix);
        let relative = cursor as i64 - root as i64;
        decoded[root..root + 8].copy_from_slice(&relative.to_ne_bytes());
        decoded[root + 8..root + ROOT_LEN].copy_from_slice(&(len as u64).to_ne_bytes());
        if layout == Layout::Values {
            let weight = encoded
                .get(input..)
                .and_then(|rest| rest.get(..WEIGHT_LEN))
                .ok_or_else(|| format!("item {index}'s weight runs past the block"))?;
            decoded[root + ROOT_LEN..root + ROOT_LEN + WEIGHT_LEN].copy_from_slice(weight);
            input += WEIGHT_LEN;
        }
        previous = cursor..cursor + len;
        cursor = root + layout.archive_len();
    }
    if cursor != items_end {
        return Err(format!("items end at {cursor}, not at {items_end}"));
    }
    let tail = encoded
        .get(input..)
        .and_then(|rest| rest.get(..tail_len))
        .ok_or("the tail runs past the block")?;
    decoded[items_end..items_end + tail_len].copy_from_slice(tail);

    let checksum = crc32c(&decoded[4..]);
    if checksum != decoded_checksum {
        return Err(format!(
            "decoded checksum {checksum:#x} does not match {decoded_checksum:#x}"
        ));
    }
    Ok(decoded)
}

/// The fields of a data block's header that the encoder needs.
struct DecodedHeader {
    n_values: usize,
    /// Where the value map and row groups start; items must end before it.
    tail_start: usize,
    /// Where the items are.
    value_map: ValueMap,
}

/// A data block's value map: the offset of each item's root.
enum ValueMap {
    /// An array of `width`-byte little-endian offsets starting at `offset`.
    Array { offset: usize, width: usize },
    /// Item `i` is at `start + stride * i`.
    Stride { start: usize, stride: usize },
}

impl DecodedHeader {
    fn parse(block: &[u8]) -> Option<Self> {
        if block.len() < DATA_HEADER_LEN || block[4..8] != DATA_BLOCK_MAGIC {
            return None;
        }
        let field = |offset: usize| {
            u32::from_le_bytes(block[offset..offset + 4].try_into().unwrap()) as usize
        };
        let (n_values, value_map_ofs, row_groups_ofs) = (field(8), field(12), field(16));
        let value_map = match decode_varint_width(block[20])? {
            Some(width) => ValueMap::Array {
                offset: value_map_ofs,
                width,
            },
            None => {
                let pair = block.get(value_map_ofs..value_map_ofs.checked_add(8)?)?;
                ValueMap::Stride {
                    start: u32::from_le_bytes(pair[..4].try_into().unwrap()) as usize,
                    stride: u32::from_le_bytes(pair[4..].try_into().unwrap()) as usize,
                }
            }
        };
        // Row groups, when present, follow the value map; either may be
        // first in a future layout, so items must end before both.
        let tail_start = if row_groups_ofs == 0 {
            value_map_ofs
        } else {
            value_map_ofs.min(row_groups_ofs)
        };
        (n_values > 0).then_some(Self {
            n_values,
            tail_start,
            value_map,
        })
    }

    /// The offset of item `index`'s root.
    fn item_offset(&self, block: &[u8], index: usize) -> Option<usize> {
        match self.value_map {
            ValueMap::Array { offset, width } => {
                let start = offset.checked_add(index.checked_mul(width)?)?;
                let raw = block.get(start..start.checked_add(width)?)?;
                let mut bytes = [0u8; 8];
                bytes[..width].copy_from_slice(raw);
                usize::try_from(u64::from_le_bytes(bytes)).ok()
            }
            ValueMap::Stride { start, stride } => start.checked_add(stride.checked_mul(index)?),
        }
    }
}

/// Decodes the serialized `Option<Varint>` of a value map: `Some(None)` for
/// the stride form, `Some(Some(width))` for an array of `width`-byte
/// integers, and `None` for a value the encoder does not know.
fn decode_varint_width(tag: u8) -> Option<Option<usize>> {
    if tag == 0 {
        return Some(None);
    }
    <super::format::Varint as num_traits::FromPrimitive>::from_u8(tag)
        .map(|varint| Some(varint.len()))
}

/// Returns the bytes of the archived `Vec<u8>` whose root is at `root`,
/// checking that they start at `cursor` and are followed only by zero padding
/// up to the root.
fn item_bytes(block: &[u8], cursor: usize, root: usize) -> Option<&[u8]> {
    let fields = block.get(root..root.checked_add(ROOT_LEN)?)?;
    let relative = i64::from_ne_bytes(fields[..8].try_into().unwrap());
    let len = usize::try_from(u64::from_ne_bytes(fields[8..].try_into().unwrap())).ok()?;
    let start = usize::try_from((root as i64).checked_add(relative)?).ok()?;
    let end = start.checked_add(len)?;
    let well_placed = start == cursor
        && end <= root
        && end.next_multiple_of(ROOT_ALIGN) == root
        && block[end..root].iter().all(|byte| *byte == 0);
    well_placed.then(|| &block[start..end])
}

fn common_prefix_len(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b).take_while(|(x, y)| x == y).count()
}

fn trim_trailing_zeros(bytes: &[u8]) -> &[u8] {
    let len = bytes
        .iter()
        .rposition(|byte| *byte != 0)
        .map_or(0, |last| last + 1);
    &bytes[..len]
}

pub(crate) fn write_leb128(out: &mut Vec<u8>, mut value: usize) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

pub(crate) fn read_leb128(bytes: &[u8], position: &mut usize) -> Result<usize, String> {
    let mut value = 0usize;
    for shift in (0..usize::BITS).step_by(7) {
        let byte = *bytes.get(*position).ok_or("a length runs past the block")?;
        *position += 1;
        value |= usize::from(byte & 0x7f)
            .checked_shl(shift)
            .ok_or("a length overflows")?;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err("a length overflows".into())
}
