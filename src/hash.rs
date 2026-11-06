//! Content hashes (BLAKE3), shown and stored as lowercase hex.

use std::fmt;

use anyhow::{Context, Result};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A 32-byte BLAKE3 hash that names an object by its content.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Hash([u8; 32]);

impl Hash {
    /// Hashes the concatenation of `parts`.
    pub fn of(parts: &[&[u8]]) -> Self {
        let mut hasher = blake3::Hasher::new();
        for part in parts {
            hasher.update(part);
        }
        Hash(*hasher.finalize().as_bytes())
    }

    pub fn to_hex(&self) -> String {
        blake3::Hash::from(self.0).to_hex().to_string()
    }

    pub fn from_hex(text: &str) -> Result<Self> {
        let parsed = blake3::Hash::from_hex(text)
            .with_context(|| format!("'{text}' is not a valid object hash"))?;
        Ok(Hash(*parsed.as_bytes()))
    }

    /// First 10 hex characters, for messages.
    pub fn short(&self) -> String {
        self.to_hex()[..10].to_string()
    }
}

impl fmt::Display for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Hash({})", self.short())
    }
}

impl Serialize for Hash {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for Hash {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Hash::from_hex(&text).map_err(serde::de::Error::custom)
    }
}
