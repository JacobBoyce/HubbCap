//! The hub: the shared store everyone syncs to. For now it is a plain folder
//! (a local path or a network share).
//!
//! ```text
//! hub/
//!   hub.toml                    marks the folder as a hub
//!   objects/                    same format as a working copy's objects
//!   heads/<hash of path>.json   the current version(s) of each file
//!   machines/<user>@<machine>   each machine's latest snapshot
//!   lock                        locked for a moment while heads change
//! ```
//!
//! A file with more than one head is in superposition. The hub only ever adds
//! a version and removes the versions it replaces (its parents), so nobody is
//! told to merge first and no version wins because it arrived first.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::fsutil::atomic_write;
use crate::hash::Hash;
use crate::model::Version;
use crate::store::{Kind, Store};

const MARKER: &str = "hub.toml";
const MARKER_TEXT: &str = "# This folder is a HubbCap hub. Don't edit it by hand.\nformat = 1\n";
const LOCK_WAIT: Duration = Duration::from_secs(30);

pub struct Hub {
    pub root: PathBuf,
    pub store: Store,
}

#[derive(Serialize, Deserialize, Default)]
struct HeadsFile {
    path: String,
    heads: Vec<Hash>,
}

/// What happened to the file when a version reached the hub.
#[derive(Debug, PartialEq, Eq)]
pub enum Applied {
    /// The file has exactly one current version.
    Clean,
    /// The file now has this many current versions.
    Superposition(usize),
    /// Someone already shared identical content; that version stands for ours.
    SameAs(Hash),
}

impl Hub {
    fn at(root: &Path) -> Hub {
        Hub {
            root: root.to_path_buf(),
            store: Store::new(root.join("objects")),
        }
    }

    /// Opens an existing hub, or creates one in a missing or empty folder.
    pub fn create_or_open(root: &Path) -> Result<Hub> {
        if root.join(MARKER).is_file() {
            return Ok(Hub::at(root));
        }
        if root.exists() && fs::read_dir(root)?.next().is_some() {
            bail!(
                "{} already has files in it and isn't a hub. Pick an empty or new folder for the hub.",
                root.display()
            );
        }
        fs::create_dir_all(root.join("objects"))
            .with_context(|| format!("creating the hub at {}", root.display()))?;
        fs::create_dir_all(root.join("heads"))?;
        fs::create_dir_all(root.join("machines"))?;
        atomic_write(&root.join(MARKER), MARKER_TEXT.as_bytes())?;
        Ok(Hub::at(root))
    }

    /// Opens an existing hub; fails if the folder can't be reached.
    pub fn open(root: &Path) -> Result<Hub> {
        if !root.join(MARKER).is_file() {
            bail!("can't reach the hub at {}", root.display());
        }
        Ok(Hub::at(root))
    }

    fn heads_path(&self, path: &str) -> PathBuf {
        let name = Hash::of(&[path.as_bytes()]).to_hex();
        self.root.join("heads").join(format!("{name}.json"))
    }

    pub fn heads(&self, path: &str) -> Result<Vec<Hash>> {
        let file = self.heads_path(path);
        match fs::read(&file) {
            Ok(bytes) => Ok(serde_json::from_slice::<HeadsFile>(&bytes)
                .with_context(|| format!("reading {}", file.display()))?
                .heads),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e).with_context(|| format!("reading {}", file.display())),
        }
    }

    fn set_heads(&self, path: &str, heads: Vec<Hash>) -> Result<()> {
        let file = HeadsFile {
            path: path.to_string(),
            heads,
        };
        atomic_write(&self.heads_path(path), &serde_json::to_vec(&file)?)
    }

    /// Every file the hub knows, with its current versions.
    pub fn all_heads(&self) -> Result<Vec<(String, Vec<Hash>)>> {
        let mut all = Vec::new();
        let dir = self.root.join("heads");
        if !dir.is_dir() {
            return Ok(all);
        }
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.ends_with(".json") {
                continue;
            }
            let file: HeadsFile = serde_json::from_slice(&fs::read(entry.path())?)
                .with_context(|| format!("reading {}", entry.path().display()))?;
            all.push((file.path, file.heads));
        }
        all.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(all)
    }

    pub fn version(&self, hash: &Hash) -> Result<Version> {
        self.store.get_json(Kind::Version, hash)
    }

    /// Records a version that is already in the hub's store. The caller must
    /// hold the lock. `parents` are the versions it replaces (its own parents,
    /// with aliases resolved). Safe to repeat after an interruption.
    pub fn apply(&self, hash: Hash, version: &Version, parents: &[Hash]) -> Result<Applied> {
        let mut heads = self.heads(&version.path)?;
        if !heads.contains(&hash) {
            heads.retain(|h| !parents.contains(h));
            let mut identical = None;
            for head in &heads {
                if self.version(head)?.blob == version.blob {
                    identical = Some(*head);
                    break;
                }
            }
            if let Some(same) = identical {
                self.set_heads(&version.path, heads)?;
                return Ok(Applied::SameAs(same));
            }
            heads.push(hash);
            self.set_heads(&version.path, heads.clone())?;
        }
        Ok(match heads.len() {
            1 => Applied::Clean,
            n => Applied::Superposition(n),
        })
    }

    pub fn set_machine_snapshot(&self, who: &str, snapshot: &Hash) -> Result<()> {
        atomic_write(
            &self.root.join("machines").join(who),
            snapshot.to_hex().as_bytes(),
        )
    }

    /// Takes the hub's lock, waiting up to 30 seconds for another `cap`.
    ///
    /// This is an operating-system file lock, so if `cap` crashes or is
    /// killed the system releases it at once; nothing is ever left stuck.
    pub fn lock(&self) -> Result<HubLock> {
        let path = self.root.join("lock");
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("opening the hub lock at {}", path.display()))?;
        let started = Instant::now();
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(HubLock { _file: file }),
                Err(TryLockError::WouldBlock) => {
                    if started.elapsed() > LOCK_WAIT {
                        bail!("the hub is busy (another cap has been working on it for a while)");
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(TryLockError::Error(e)) => {
                    return Err(e)
                        .with_context(|| format!("locking the hub at {}", path.display()));
                }
            }
        }
    }
}

/// Held while heads change; dropping it (or the process dying) releases it.
pub struct HubLock {
    _file: File,
}
