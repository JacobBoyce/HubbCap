//! The structured objects HubbCap stores. All are immutable once written.

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::hash::Hash;

/// A file's content: the ordered chunks that make it up.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Blob {
    pub size: u64,
    pub chunks: Vec<Hash>,
}

/// One saved state of one file. `blob: None` records a deletion.
/// Two or more parents means this version collapsed a superposition.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Version {
    pub path: String,
    pub blob: Option<Hash>,
    pub parents: Vec<Hash>,
    pub author: String,
    pub machine: String,
    pub time: i64,
}

impl Version {
    /// "jacob@desktop": who made this version and where.
    pub fn who(&self) -> String {
        format!("{}@{}", self.author, self.machine)
    }
}

/// Everything one `cap done` saved. A message makes it a checkpoint.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub versions: Vec<Hash>,
    pub parent: Option<Hash>,
    pub author: String,
    pub machine: String,
    pub time: i64,
    pub message: Option<String>,
}

/// Seconds since the Unix epoch.
pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// "just now", "5 minutes", "3 days": how long ago `time` was.
pub fn age(time: i64) -> String {
    let secs = (now() - time).max(0);
    let (n, unit) = match secs {
        0..=59 => return "just now".to_string(),
        60..=3_599 => (secs / 60, "minute"),
        3_600..=86_399 => (secs / 3_600, "hour"),
        _ => (secs / 86_400, "day"),
    };
    let plural = if n == 1 { "" } else { "s" };
    format!("{n} {unit}{plural} ago")
}
