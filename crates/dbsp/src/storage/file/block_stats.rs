//! Space accounting for layer files: how many bytes each kind of block uses.
//!
//! The analysis reads the file with plain file I/O, not through a storage
//! backend or the buffer cache, so it changes none of the backend's counters
//! and none of the cache's contents. It supports files without compression.

use std::collections::HashSet;
use std::io::Cursor;
use std::os::unix::fs::FileExt;
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use binrw::BinRead;
use serde::Serialize;

use super::format::{FileTrailer, IndexBlockHeader, NodeType, Varint};

/// Every layer file ends with a trailer block of exactly this size.
const TRAILER_BYTES: u64 = 512;

/// Offsets and sizes of child blocks are stored in units of this many bytes.
const BLOCK_UNIT_SHIFT: u32 = 9;

/// Bytes of the blocks in one level of a layer file's nested rows.
///
/// Level 0 holds the keys; each later level holds the rows grouped under the
/// level before it (for an indexed batch, level 1 holds the values).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct LevelBlockBytes {
    /// Data blocks of this level.
    pub data: u64,
    /// Index blocks of this level's tree.
    pub index: u64,
    /// Number of data blocks of this level.
    pub data_blocks: u64,
    /// Rows stored in this level.
    pub rows: u64,
}

/// Bytes of a layer file by block type.
///
/// Together they make up the file. Blocks are whole multiples of 512 bytes,
/// so each figure includes the padding of its blocks.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct BlockBytes {
    /// Data and index blocks, by level.
    pub levels: Vec<LevelBlockBytes>,
    /// The membership filter block, if any.
    pub filter: u64,
    /// The trailer block.
    pub trailer: u64,
    /// Bytes that no block of the file's trees, filter, or trailer covers.
    /// Zero in a well-formed file.
    pub unreferenced: u64,
}

impl BlockBytes {
    /// Bytes of data blocks, over all levels.
    pub fn data(&self) -> u64 {
        self.levels.iter().map(|level| level.data).sum()
    }

    /// Bytes of index blocks, over all levels.
    pub fn index(&self) -> u64 {
        self.levels.iter().map(|level| level.index).sum()
    }

    /// The file size.
    pub fn total(&self) -> u64 {
        self.data() + self.index() + self.filter + self.trailer + self.unreferenced
    }
}

/// Accounts for the bytes of the layer file at `path`.
pub fn layer_file_block_bytes(path: &Path) -> Result<BlockBytes> {
    let file = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let size = file
        .metadata()
        .with_context(|| format!("stat {}", path.display()))?
        .len();
    block_bytes(size, |offset, len| {
        let mut buffer = vec![0; len];
        file.read_exact_at(&mut buffer, offset)?;
        Ok(buffer)
    })
    .with_context(|| format!("analyze {}", path.display()))
}

/// Accounts for the bytes of a layer file of `size` bytes whose contents
/// `read(offset, len)` returns.
pub fn block_bytes(
    size: u64,
    read: impl Fn(u64, usize) -> std::io::Result<Vec<u8>>,
) -> Result<BlockBytes> {
    ensure!(
        size >= TRAILER_BYTES && size.is_multiple_of(512),
        "a layer file is a whole number of 512-byte blocks and holds a trailer; this one has {size} bytes"
    );
    let trailer_bytes = read(size - TRAILER_BYTES, TRAILER_BYTES as usize)?;
    let trailer =
        FileTrailer::read_le(&mut Cursor::new(&trailer_bytes)).context("parse the file trailer")?;
    ensure!(
        trailer.compression.is_none(),
        "block accounting does not support compressed files"
    );

    let mut bytes = BlockBytes {
        trailer: TRAILER_BYTES,
        levels: trailer
            .columns
            .iter()
            .map(|column| LevelBlockBytes {
                rows: column.n_rows,
                ..LevelBlockBytes::default()
            })
            .collect(),
        ..BlockBytes::default()
    };
    let mut visited = HashSet::new();
    let mut pending = Vec::new();
    for (level, column) in trailer.columns.iter().enumerate() {
        if column.n_rows > 0 {
            pending.push((
                level,
                column.node_offset,
                column.node_size,
                column.node_type,
            ));
        }
    }
    while let Some((level, offset, node_size, node_type)) = pending.pop() {
        ensure!(
            offset
                .checked_add(u64::from(node_size))
                .is_some_and(|end| end <= size - TRAILER_BYTES),
            "block at {offset} of {node_size} bytes lies outside the file"
        );
        ensure!(
            visited.insert(offset),
            "block at {offset} is referenced twice"
        );
        let level_bytes = &mut bytes.levels[level];
        match node_type {
            NodeType::Data => {
                level_bytes.data += u64::from(node_size);
                level_bytes.data_blocks += 1;
            }
            NodeType::Index => {
                level_bytes.index += u64::from(node_size);
                let block = read(offset, node_size as usize)?;
                pending.extend(
                    index_block_children(&block)
                        .with_context(|| format!("parse the index block at {offset}"))?
                        .into_iter()
                        .map(|(offset, size, node_type)| (level, offset, size, node_type)),
                );
            }
        }
    }

    let filter = if trailer.has_filter64() {
        trailer.filter_size64
    } else if trailer.filter_offset != 0 {
        u64::from(trailer.filter_size)
    } else {
        0
    };
    bytes.filter = filter;

    let accounted = bytes.data() + bytes.index() + bytes.filter + bytes.trailer;
    match size.checked_sub(accounted) {
        Some(unreferenced) => bytes.unreferenced = unreferenced,
        None => bail!("blocks cover {accounted} bytes of a {size}-byte file"),
    }
    Ok(bytes)
}

/// The (offset, size, type) of each child that an index block points to.
fn index_block_children(block: &[u8]) -> Result<Vec<(u64, u32, NodeType)>> {
    let header = IndexBlockHeader::read_le(&mut Cursor::new(block))?;
    let n_children = usize::from(header.n_children);
    let offsets = read_varints(
        block,
        header.child_offset_varint,
        header.child_offsets_offset,
        n_children,
    )?;
    let sizes = read_varints(
        block,
        header.child_size_varint,
        header.child_sizes_offset,
        n_children,
    )?;
    offsets
        .into_iter()
        .zip(sizes)
        .map(|(offset, size)| {
            let size = u32::try_from(size << BLOCK_UNIT_SHIFT)
                .context("child block size exceeds 4 GiB")?;
            Ok((offset << BLOCK_UNIT_SHIFT, size, header.child_type))
        })
        .collect()
}

/// Reads `count` little-endian integers of width `varint` starting at `start`.
fn read_varints(block: &[u8], varint: Varint, start: u32, count: usize) -> Result<Vec<u64>> {
    let width = varint.len();
    let start = start as usize;
    let end = count
        .checked_mul(width)
        .and_then(|len| len.checked_add(start))
        .filter(|end| *end <= block.len())
        .context("an array in an index block runs past the block")?;
    Ok(block[start..end]
        .chunks_exact(width)
        .map(|raw| {
            let mut value = [0u8; 8];
            value[..width].copy_from_slice(raw);
            u64::from_le_bytes(value)
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use feldera_types::config::{StorageConfig, StorageOptions};
    use tempfile::tempdir;

    use super::*;
    use crate::{
        dynamic::{DynData, Erase},
        storage::{
            backend::StorageBackend,
            buffer_cache::BufferCache,
            file::{
                Factories, FilterPlan,
                format::{BatchMetadata, Compression},
                writer::{Parameters, Writer1, Writer2},
            },
        },
    };

    fn buffer_cache() -> Option<Arc<BufferCache>> {
        thread_local! {
            static CACHE: Arc<BufferCache> = Arc::new(BufferCache::new(1024 * 1024));
        }
        Some(CACHE.with(Arc::clone))
    }

    /// Writes a one-column file of `rows` rows and returns its analysis along
    /// with the file's size on disk.
    fn write_and_analyze(rows: i64, parameters: Parameters) -> (BlockBytes, u64) {
        let directory = tempdir().unwrap();
        let backend = <dyn StorageBackend>::new(
            &StorageConfig {
                path: directory.path().to_string_lossy().into_owned(),
                cache: Default::default(),
            },
            &StorageOptions::default(),
        )
        .unwrap();
        let factories = Factories::<DynData, DynData>::new::<i64, ()>();
        let mut writer = Writer1::new(
            &factories,
            buffer_cache,
            &*backend,
            parameters,
            FilterPlan::<DynData>::decide_filter(None, rows as usize),
        )
        .unwrap();
        for row in 0..rows {
            writer.write0((row.erase(), ().erase())).unwrap();
        }
        let (reader, _filters) = writer.into_reader(BatchMetadata::default()).unwrap();
        let path = directory.path().join(reader.path().as_ref());
        let size_on_disk = std::fs::metadata(&path).unwrap().len();
        (layer_file_block_bytes(&path).unwrap(), size_on_disk)
    }

    #[test]
    fn block_bytes_sum_to_the_file_size_and_leave_nothing_unreferenced() {
        // Small blocks force several data blocks and an index tree.
        let parameters = Parameters::default().with_compression(None);
        for rows in [1, 100, 20_000] {
            let (bytes, size) = write_and_analyze(rows, parameters.clone());
            assert_eq!(bytes.total(), size, "{rows} rows");
            assert_eq!(bytes.unreferenced, 0, "{rows} rows");
            assert_eq!(bytes.trailer, 512);
            assert!(bytes.data() > 0, "{rows} rows");
            assert!(bytes.filter > 0, "{rows} rows");
            assert_eq!(bytes.levels.len(), 1);
            assert_eq!(bytes.levels[0].rows, rows as u64);
        }
        let (bytes, _) = write_and_analyze(20_000, parameters);
        assert!(
            bytes.index() > 0,
            "a file with many data blocks has index blocks"
        );
    }

    #[test]
    fn levels_split_a_two_level_file_and_sum_to_the_totals() {
        const KEYS: i64 = 2_000;
        const VALUES_PER_KEY: i64 = 4;
        let directory = tempdir().unwrap();
        let backend = <dyn StorageBackend>::new(
            &StorageConfig {
                path: directory.path().to_string_lossy().into_owned(),
                cache: Default::default(),
            },
            &StorageOptions::default(),
        )
        .unwrap();
        let keys = Factories::<DynData, DynData>::new::<i64, ()>();
        let values = Factories::<DynData, DynData>::new::<i64, i64>();
        let mut writer = Writer2::new(
            &keys,
            &values,
            buffer_cache,
            &*backend,
            Parameters::default().with_compression(None),
            None,
        )
        .unwrap();
        for key in 0..KEYS {
            for value in 0..VALUES_PER_KEY {
                writer.write1((value.erase(), 1i64.erase())).unwrap();
            }
            writer.write0((key.erase(), ().erase())).unwrap();
        }
        let (file, _, _) = writer.close(BatchMetadata::default()).unwrap();
        let path = directory.path().join(file.path().as_ref());

        let bytes = layer_file_block_bytes(&path).unwrap();
        assert_eq!(bytes.total(), std::fs::metadata(&path).unwrap().len());
        assert_eq!(bytes.levels.len(), 2);
        let [keys_level, values_level] = [bytes.levels[0], bytes.levels[1]];
        assert_eq!(keys_level.rows, KEYS as u64);
        assert_eq!(values_level.rows, (KEYS * VALUES_PER_KEY) as u64);
        assert!(
            values_level.data > keys_level.data,
            "four values per key take more room than the keys"
        );
    }

    #[test]
    fn compressed_files_are_rejected() {
        let parameters = Parameters::default().with_compression(Some(Compression::Lz4));
        let directory = tempdir().unwrap();
        let backend = <dyn StorageBackend>::new(
            &StorageConfig {
                path: directory.path().to_string_lossy().into_owned(),
                cache: Default::default(),
            },
            &StorageOptions::default(),
        )
        .unwrap();
        let factories = Factories::<DynData, DynData>::new::<i64, ()>();
        let mut writer = Writer1::new(
            &factories,
            buffer_cache,
            &*backend,
            parameters,
            FilterPlan::<DynData>::decide_filter(None, 10),
        )
        .unwrap();
        for row in 0..10i64 {
            writer.write0((row.erase(), ().erase())).unwrap();
        }
        let (reader, _) = writer.into_reader(BatchMetadata::default()).unwrap();
        let path = directory.path().join(reader.path().as_ref());
        let error = layer_file_block_bytes(&path).unwrap_err();
        assert!(format!("{error:#}").contains("compressed"), "{error:#}");
    }

    #[test]
    fn a_truncated_file_is_an_error() {
        let error = block_bytes(100, |_, _| unreachable!()).unwrap_err();
        assert!(error.to_string().contains("512-byte"), "{error}");
    }
}
