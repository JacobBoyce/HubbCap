//! Content-addressed object store, shared format for working copies and hubs.
//!
//! An object on disk is `zstd(kind byte ++ payload)`, stored at
//! `objects/<first 2 hex>/<remaining hex>` where the name is the BLAKE3 hash of
//! `kind byte ++ payload`. Objects are never modified, and every read checks
//! the content against its name.

use std::fs::{self, File};
use std::io::{BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use fastcdc::v2020::StreamCDC;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::fsutil::{TEMP_PREFIX, atomic_write};
use crate::hash::Hash;
use crate::model::Blob;

/// Content-defined chunk sizes. Small files become one chunk; editing the
/// middle of a large file only changes the chunks around the edit.
pub const CHUNK_MIN: usize = 256 * 1024;
pub const CHUNK_AVG: usize = 1024 * 1024;
pub const CHUNK_MAX: usize = 4 * 1024 * 1024;

const ZSTD_LEVEL: i32 = 3;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Chunk = 1,
    Blob = 2,
    Version = 3,
    Snapshot = 4,
}

impl Kind {
    fn from_byte(byte: u8) -> Result<Kind> {
        Ok(match byte {
            1 => Kind::Chunk,
            2 => Kind::Blob,
            3 => Kind::Version,
            4 => Kind::Snapshot,
            other => bail!("unknown object kind {other}"),
        })
    }
}

#[derive(Clone, Debug)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    /// `root` is an `objects` folder; it is created on first write.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Store { root: root.into() }
    }

    fn path_of(&self, hash: &Hash) -> PathBuf {
        let hex = hash.to_hex();
        self.root.join(&hex[..2]).join(&hex[2..])
    }

    pub fn has(&self, hash: &Hash) -> bool {
        self.path_of(hash).is_file()
    }

    /// Stores an object and returns its hash. Storing the same content twice
    /// is free: the second call finds it already there.
    pub fn put(&self, kind: Kind, payload: &[u8]) -> Result<Hash> {
        let hash = Hash::of(&[&[kind as u8], payload]);
        if !self.has(&hash) {
            let mut plain = Vec::with_capacity(payload.len() + 1);
            plain.push(kind as u8);
            plain.extend_from_slice(payload);
            let packed =
                zstd::bulk::compress(&plain, ZSTD_LEVEL).context("compressing an object")?;
            atomic_write(&self.path_of(&hash), &packed)?;
        }
        Ok(hash)
    }

    /// Reads an object, refusing it if its content no longer matches its name.
    pub fn get(&self, hash: &Hash) -> Result<(Kind, Vec<u8>)> {
        let path = self.path_of(hash);
        let packed =
            fs::read(&path).with_context(|| format!("object {} is missing", hash.short()))?;
        let plain = zstd::stream::decode_all(&packed[..]).map_err(|_| {
            anyhow!(
                "object {} is corrupt (it can't be decompressed)",
                hash.short()
            )
        })?;
        if plain.is_empty() || Hash::of(&[&plain]) != *hash {
            bail!(
                "object {} is corrupt (its content doesn't match its hash)",
                hash.short()
            );
        }
        let kind = Kind::from_byte(plain[0])?;
        Ok((kind, plain[1..].to_vec()))
    }

    pub fn get_kind(&self, kind: Kind, hash: &Hash) -> Result<Vec<u8>> {
        let (found, payload) = self.get(hash)?;
        if found != kind {
            bail!(
                "object {} is a {found:?}, expected a {kind:?}",
                hash.short()
            );
        }
        Ok(payload)
    }

    pub fn put_json<T: Serialize>(&self, kind: Kind, value: &T) -> Result<Hash> {
        self.put(kind, &serde_json::to_vec(value)?)
    }

    pub fn get_json<T: DeserializeOwned>(&self, kind: Kind, hash: &Hash) -> Result<T> {
        let payload = self.get_kind(kind, hash)?;
        serde_json::from_slice(&payload)
            .with_context(|| format!("object {} has an unreadable layout", hash.short()))
    }

    /// Copies one object into another store, verifying it on the way.
    /// Returns false if the other store already had it.
    pub fn copy_to(&self, other: &Store, hash: &Hash) -> Result<bool> {
        if other.has(hash) {
            return Ok(false);
        }
        self.get(hash)?;
        let packed = fs::read(self.path_of(hash))?;
        atomic_write(&other.path_of(hash), &packed)?;
        Ok(true)
    }

    /// Chunks a file into this store and returns its blob hash.
    pub fn put_file(&self, path: &Path) -> Result<Hash> {
        blob_of_file(path, Some(self))
    }

    /// Writes a blob's full content to `out`.
    pub fn write_blob_to(&self, blob: &Hash, out: &mut impl Write) -> Result<()> {
        let blob: Blob = self.get_json(Kind::Blob, blob)?;
        for chunk in &blob.chunks {
            out.write_all(&self.get_kind(Kind::Chunk, chunk)?)?;
        }
        Ok(())
    }

    pub fn read_blob(&self, blob: &Hash) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        self.write_blob_to(blob, &mut bytes)?;
        Ok(bytes)
    }

    /// Every object hash in the store (skipping leftovers of interrupted writes).
    pub fn all_hashes(&self) -> Result<Vec<Hash>> {
        let mut hashes = Vec::new();
        if !self.root.is_dir() {
            return Ok(hashes);
        }
        for shard in fs::read_dir(&self.root)? {
            let shard = shard?;
            let prefix = shard.file_name().to_string_lossy().to_string();
            if prefix.len() != 2 || !shard.file_type()?.is_dir() {
                continue;
            }
            for entry in fs::read_dir(shard.path())? {
                let name = entry?.file_name().to_string_lossy().to_string();
                if name.starts_with(TEMP_PREFIX) {
                    continue;
                }
                hashes.push(Hash::from_hex(&format!("{prefix}{name}"))?);
            }
        }
        Ok(hashes)
    }
}

/// Computes a file's blob hash by chunking it. With `store`, the chunks and
/// blob are saved too; without, nothing is written (used by `cap status`).
pub fn blob_of_file(path: &Path, store: Option<&Store>) -> Result<Hash> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut chunks = Vec::new();
    let mut size = 0u64;
    for chunk in StreamCDC::new(BufReader::new(file), CHUNK_MIN, CHUNK_AVG, CHUNK_MAX) {
        let chunk = chunk.map_err(|e| anyhow!("reading {}: {e}", path.display()))?;
        size += chunk.length as u64;
        chunks.push(match store {
            Some(store) => store.put(Kind::Chunk, &chunk.data)?,
            None => Hash::of(&[&[Kind::Chunk as u8], &chunk.data]),
        });
    }
    let blob = Blob { size, chunks };
    match store {
        Some(store) => store.put_json(Kind::Blob, &blob),
        None => Ok(Hash::of(&[
            &[Kind::Blob as u8],
            &serde_json::to_vec(&blob)?,
        ])),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(32))]
        #[test]
        fn file_round_trips_through_the_store(data in proptest::collection::vec(any::<u8>(), 0..600_000)) {
            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join("f.bin");
            fs::write(&file, &data).unwrap();
            let store = Store::new(dir.path().join("objects"));
            let dry = blob_of_file(&file, None).unwrap();
            let blob = store.put_file(&file).unwrap();
            prop_assert_eq!(dry, blob);
            prop_assert_eq!(store.read_blob(&blob).unwrap(), data);
        }
    }

    #[test]
    fn corrupt_object_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("objects"));
        let hash = store.put(Kind::Chunk, b"hello").unwrap();
        let other = store.put(Kind::Chunk, b"world").unwrap();
        // Swap in another object's bytes under this name.
        fs::copy(store.path_of(&other), store.path_of(&hash)).unwrap();
        let err = store.get(&hash).unwrap_err().to_string();
        assert!(err.contains("corrupt"), "{err}");
    }
}
