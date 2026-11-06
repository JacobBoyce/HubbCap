//! A working copy: your project folder plus its hidden `.cap/` folder.
//!
//! ```text
//! project/
//!   .cap/
//!     config.toml   who you are, which machine, where the hub is
//!     state.json    what each file looked like at your last `done`
//!     objects/      everything this machine has saved
//!   .capignore      files HubbCap should leave alone (.gitignore syntax)
//! ```

use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::fsutil::atomic_write;
use crate::hash::Hash;
use crate::store::{Store, blob_of_file};

pub const CAP_DIR: &str = ".cap";
pub const IGNORE_FILE: &str = ".capignore";

/// Files modified this close to the previous scan are always re-hashed, since
/// a same-second edit can leave size and modified time unchanged.
const RACY_WINDOW_NS: u64 = 2_000_000_000;

pub const DEFAULT_IGNORE: &str = "\
# Files HubbCap leaves alone. Same syntax as .gitignore.

# Other version control
.git/
.svn/
.hg/

# Operating system clutter
.DS_Store
Thumbs.db
desktop.ini

# Unity (regenerated on every machine)
/[Ll]ibrary/
/[Tt]emp/
/[Oo]bj/
/[Ll]ogs/
/[Uu]ser[Ss]ettings/
/[Bb]uild/
/[Bb]uilds/

# Common build output
target/
bin/
obj/
node_modules/
";

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Config {
    pub user: String,
    pub machine: String,
    pub hub: Option<PathBuf>,
}

/// What a tracked file looked like at the last `done`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The version this file is built on (its parent for the next `done`).
    pub version: Hash,
    /// `None` when the file was deleted.
    pub blob: Option<Hash>,
    pub size: u64,
    pub mtime_ns: u64,
}

/// Everything a working copy remembers between commands. Written as one file,
/// atomically, so it is always self-consistent.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct State {
    pub files: BTreeMap<String, Entry>,
    /// Snapshots saved here but not yet uploaded to the hub, oldest first.
    pub pending: Vec<Hash>,
    pub last_snapshot: Option<Hash>,
    /// When the hub found our new version identical to one already there, our
    /// version is recorded as an alias of that one until the uploads finish.
    pub aliases: BTreeMap<Hash, Hash>,
    pub scanned_at_ns: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
}

#[derive(Clone, Debug)]
pub struct Change {
    pub path: String,
    pub kind: ChangeKind,
    pub blob: Option<Hash>,
    pub size: u64,
    pub mtime_ns: u64,
}

#[derive(Debug, Default)]
pub struct Scan {
    pub changes: Vec<Change>,
    /// Files whose content is unchanged but whose modified time moved.
    pub refreshed: Vec<(String, u64, u64)>,
    pub started_ns: u64,
}

pub struct WorkCopy {
    pub root: PathBuf,
    pub config: Config,
    pub store: Store,
}

impl WorkCopy {
    fn cap_dir(root: &Path) -> PathBuf {
        root.join(CAP_DIR)
    }

    /// Creates a new working copy in `root`.
    pub fn create(root: &Path, config: Config) -> Result<WorkCopy> {
        let cap = Self::cap_dir(root);
        if cap.exists() {
            bail!("{} is already a HubbCap folder", root.display());
        }
        fs::create_dir_all(cap.join("objects"))?;
        atomic_write(
            &cap.join("config.toml"),
            toml::to_string(&config)?.as_bytes(),
        )?;
        let wc = WorkCopy {
            root: root.to_path_buf(),
            config,
            store: Store::new(cap.join("objects")),
        };
        wc.save_state(&State::default())?;
        let ignore = root.join(IGNORE_FILE);
        if !ignore.exists() {
            atomic_write(&ignore, DEFAULT_IGNORE.as_bytes())?;
        }
        Ok(wc)
    }

    /// Finds the working copy containing `start` by walking up its folders.
    pub fn find(start: &Path) -> Result<WorkCopy> {
        let start = normalize(start)?;
        for dir in start.ancestors() {
            let config_path = Self::cap_dir(dir).join("config.toml");
            if config_path.is_file() {
                let text = fs::read_to_string(&config_path)?;
                let config: Config = toml::from_str(&text)
                    .with_context(|| format!("reading {}", config_path.display()))?;
                return Ok(WorkCopy {
                    root: dir.to_path_buf(),
                    config,
                    store: Store::new(Self::cap_dir(dir).join("objects")),
                });
            }
        }
        bail!("this folder isn't a HubbCap folder yet. Run `cap init` to start tracking it.")
    }

    fn state_path(&self) -> PathBuf {
        Self::cap_dir(&self.root).join("state.json")
    }

    pub fn load_state(&self) -> Result<State> {
        let path = self.state_path();
        let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_slice(&bytes).with_context(|| format!("reading {}", path.display()))
    }

    pub fn save_state(&self, state: &State) -> Result<()> {
        atomic_write(&self.state_path(), &serde_json::to_vec_pretty(state)?)
    }

    /// Compares the folder with `state`. With `store`, new content is saved
    /// into it as it is hashed; without, the scan writes nothing.
    pub fn scan(&self, state: &State, store: Option<&Store>) -> Result<Scan> {
        let mut scan = Scan {
            started_ns: now_ns(),
            ..Scan::default()
        };
        let mut seen = std::collections::BTreeSet::new();

        for (rel, abs) in self.tracked_files()? {
            let meta = fs::metadata(&abs).with_context(|| format!("reading {}", abs.display()))?;
            let size = meta.len();
            let mtime_ns = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0);
            seen.insert(rel.clone());

            let known = state.files.get(&rel).filter(|e| e.blob.is_some());
            if let Some(entry) = known {
                let stat_same = entry.size == size && entry.mtime_ns == mtime_ns;
                let racy = mtime_ns + RACY_WINDOW_NS >= state.scanned_at_ns;
                if stat_same && !racy {
                    continue;
                }
            }

            let blob = blob_of_file(&abs, store)?;
            match known {
                Some(entry) if entry.blob == Some(blob) => {
                    scan.refreshed.push((rel, size, mtime_ns));
                }
                _ => scan.changes.push(Change {
                    kind: if known.is_some() {
                        ChangeKind::Modified
                    } else {
                        ChangeKind::Added
                    },
                    path: rel,
                    blob: Some(blob),
                    size,
                    mtime_ns,
                }),
            }
        }

        for (path, entry) in &state.files {
            if entry.blob.is_some() && !seen.contains(path) {
                scan.changes.push(Change {
                    path: path.clone(),
                    kind: ChangeKind::Deleted,
                    blob: None,
                    size: 0,
                    mtime_ns: 0,
                });
            }
        }
        scan.changes.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(scan)
    }

    /// Every file HubbCap should track, as (`a/b.txt`, absolute path).
    fn tracked_files(&self) -> Result<Vec<(String, PathBuf)>> {
        let mut walker = ignore::WalkBuilder::new(&self.root);
        walker
            .standard_filters(false)
            .hidden(false)
            .follow_links(false)
            .add_custom_ignore_filename(IGNORE_FILE)
            .filter_entry(|entry| entry.file_name() != CAP_DIR);

        let mut files = Vec::new();
        for entry in walker.build() {
            let entry = entry?;
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            let abs = entry.into_path();
            let Some(rel) = relative_path(&self.root, &abs) else {
                eprintln!(
                    "cap: skipping {} (its name isn't valid UTF-8)",
                    abs.display()
                );
                continue;
            };
            if is_alternate(&rel) {
                continue;
            }
            files.push((rel, abs));
        }
        Ok(files)
    }
}

/// `root/a/b.txt` → `a/b.txt`, always with forward slashes.
fn relative_path(root: &Path, abs: &Path) -> Option<String> {
    let rel = abs.strip_prefix(root).ok()?;
    let mut parts = Vec::new();
    for component in rel.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_str()?.to_string()),
            _ => return None,
        }
    }
    Some(parts.join("/"))
}

/// Alternates (`hero.alt-teej.psd`) are written by HubbCap beside your file
/// and are never tracked as files of their own.
pub fn is_alternate(rel: &str) -> bool {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    name.contains(".alt-")
}

/// Absolute path with `.` and `..` resolved by name (without following links,
/// and without needing the path to exist yet).
pub fn normalize(path: &Path) -> Result<PathBuf> {
    let absolute = std::path::absolute(path)?;
    let mut out = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    Ok(out)
}

pub fn now_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// Best guess at this computer's name.
pub fn default_machine_name() -> String {
    let candidates = [
        std::env::var("COMPUTERNAME").ok(),
        std::env::var("HOSTNAME").ok(),
        fs::read_to_string("/etc/hostname").ok(),
    ];
    candidates
        .into_iter()
        .flatten()
        .map(|name| clean_name(&name))
        .find(|name| !name.is_empty())
        .unwrap_or_else(|| "this-machine".to_string())
}

pub fn default_user_name() -> String {
    let name = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_default();
    let name = clean_name(&name);
    if name.is_empty() {
        "me".to_string()
    } else {
        name
    }
}

/// Names end up in file names (`hero.alt-teej@laptop.psd`), so keep them to
/// lowercase letters, digits, `-` and `_`.
pub fn clean_name(name: &str) -> String {
    name.trim()
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}
