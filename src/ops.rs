//! The commands. Each prints plain English and never destroys work.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::hash::Hash;
use crate::hub::{Applied, Hub};
use crate::model::{Blob, Snapshot, Version, age, now};
use crate::store::Kind;
use crate::workcopy::{ChangeKind, Config, Entry, State, WorkCopy, clean_name, normalize};

const LIST_LIMIT: usize = 20;

pub struct InitOptions {
    pub hub: Option<PathBuf>,
    pub user: Option<String>,
    pub machine: Option<String>,
}

/// `cap init`: start tracking a folder.
pub fn init(dir: &Path, options: InitOptions) -> Result<()> {
    let dir = normalize(dir)?;
    let user = options
        .user
        .map(|u| clean_name(&u))
        .unwrap_or_else(crate::workcopy::default_user_name);
    let machine = options
        .machine
        .map(|m| clean_name(&m))
        .unwrap_or_else(crate::workcopy::default_machine_name);
    if user.is_empty() || machine.is_empty() {
        bail!("user and machine names need at least one letter or digit");
    }

    let hub = match options.hub {
        Some(hub) => {
            let hub = normalize(&hub)?;
            if hub.starts_with(&dir) {
                bail!(
                    "put the hub outside your project folder, so it isn't tracked as part of the project"
                );
            }
            Hub::create_or_open(&hub)?;
            Some(hub)
        }
        None => None,
    };

    WorkCopy::create(
        &dir,
        Config {
            user: user.clone(),
            machine: machine.clone(),
            hub: hub.clone(),
        },
    )?;

    println!("Now tracking {} as {user}@{machine}.", dir.display());
    match hub {
        Some(hub) => println!("Hub: {}", hub.display()),
        None => println!("No hub yet, so work will be saved on this machine only."),
    }
    println!("Run `cap done` whenever you want to save and share your work.");
    Ok(())
}

/// What happened when uploading.
enum Upload {
    Shared {
        superposed: Vec<(String, Vec<String>)>,
    },
    NothingToShare,
    NoHub,
    Offline(String),
}

/// `cap done`: save everything that changed and share it.
pub fn done(start: &Path, message: Option<String>) -> Result<()> {
    let wc = WorkCopy::find(start)?;
    let mut state = wc.load_state()?;

    // Steps 1-4 happen on this machine and always succeed if the disk works.
    let scan = wc.scan(&state, Some(&wc.store))?;
    let time = now();
    let changes = scan.changes.clone();

    if !changes.is_empty() || message.is_some() {
        let mut versions = Vec::new();
        for change in &changes {
            let parents = state
                .files
                .get(&change.path)
                .map(|e| vec![e.version])
                .unwrap_or_default();
            let version = Version {
                path: change.path.clone(),
                blob: change.blob,
                parents,
                author: wc.config.user.clone(),
                machine: wc.config.machine.clone(),
                time,
            };
            let hash = wc.store.put_json(Kind::Version, &version)?;
            versions.push(hash);
            state.files.insert(
                change.path.clone(),
                Entry {
                    version: hash,
                    blob: change.blob,
                    size: change.size,
                    mtime_ns: change.mtime_ns,
                },
            );
        }
        let snapshot = Snapshot {
            versions,
            parent: state.last_snapshot,
            author: wc.config.user.clone(),
            machine: wc.config.machine.clone(),
            time,
            message: message.clone(),
        };
        let hash = wc.store.put_json(Kind::Snapshot, &snapshot)?;
        state.last_snapshot = Some(hash);
        state.pending.push(hash);
    }
    for (path, size, mtime_ns) in &scan.refreshed {
        if let Some(entry) = state.files.get_mut(path) {
            entry.size = *size;
            entry.mtime_ns = *mtime_ns;
        }
    }
    state.scanned_at_ns = scan.started_ns;
    // The save point: from here on the work is safe on this machine.
    wc.save_state(&state)?;

    // Step 5: share. Any failure here just leaves the work queued.
    let upload = match upload(&wc, &mut state) {
        Ok(upload) => upload,
        Err(e) => Upload::Offline(format!("{e:#}")),
    };

    print_done(&changes, message.as_deref(), &state, &upload);
    Ok(())
}

fn print_done(
    changes: &[crate::workcopy::Change],
    message: Option<&str>,
    state: &State,
    upload: &Upload,
) {
    if changes.is_empty() {
        match message {
            Some(msg) => println!("Nothing changed; marked a checkpoint: \"{msg}\"."),
            None if state.pending.is_empty()
                && matches!(upload, Upload::NothingToShare | Upload::NoHub) =>
            {
                println!("Nothing changed since your last done.");
                return;
            }
            None => {}
        }
    } else {
        let count = |kind| changes.iter().filter(|c| c.kind == kind).count();
        let mut parts = Vec::new();
        for (kind, word) in [
            (ChangeKind::Added, "new"),
            (ChangeKind::Modified, "changed"),
            (ChangeKind::Deleted, "deleted"),
        ] {
            let n = count(kind);
            if n > 0 {
                parts.push(format!("{n} {word}"));
            }
        }
        let files = if changes.len() == 1 { "file" } else { "files" };
        match message {
            Some(msg) => println!(
                "Saved {} {files} ({}) as checkpoint \"{msg}\".",
                changes.len(),
                parts.join(", ")
            ),
            None => println!("Saved {} {files} ({}).", changes.len(), parts.join(", ")),
        }
        print_change_list(changes);
    }

    match upload {
        Upload::Shared { superposed } => {
            println!("Shared with the hub.");
            if !superposed.is_empty() {
                let n = superposed.len();
                println!();
                println!(
                    "{n} {} now {} more than one version on the hub, because someone else changed {} too:",
                    if n == 1 { "file" } else { "files" },
                    if n == 1 { "has" } else { "have" },
                    if n == 1 { "it" } else { "them" },
                );
                for (path, others) in superposed {
                    println!("  {path}  (also changed by {})", others.join(", "));
                }
                println!(
                    "Your version stays here. Nothing is lost, and you can sort it out whenever you like."
                );
            }
        }
        Upload::NothingToShare => println!("Shared with the hub."),
        Upload::NoHub => println!("Saved on this machine only (no hub is set up)."),
        Upload::Offline(reason) => {
            println!("Saved on this machine. Couldn't share with the hub just now ({reason}).");
            println!("It will upload the next time you run `cap done`.");
        }
    }
}

fn print_change_list(changes: &[crate::workcopy::Change]) {
    for change in changes.iter().take(LIST_LIMIT) {
        let word = match change.kind {
            ChangeKind::Added => "new     ",
            ChangeKind::Modified => "changed ",
            ChangeKind::Deleted => "deleted ",
        };
        println!("  {word} {}", change.path);
    }
    if changes.len() > LIST_LIMIT {
        println!("  … and {} more", changes.len() - LIST_LIMIT);
    }
}

/// Uploads every pending snapshot, oldest first. Each one is copied, then
/// applied under the hub lock, then removed from the queue.
fn upload(wc: &WorkCopy, state: &mut State) -> Result<Upload> {
    let Some(hub_path) = &wc.config.hub else {
        return Ok(Upload::NoHub);
    };
    if state.pending.is_empty() {
        return Ok(Upload::NothingToShare);
    }
    let hub = match Hub::open(hub_path) {
        Ok(hub) => hub,
        Err(e) => return Ok(Upload::Offline(e.to_string())),
    };
    let who = format!("{}@{}", wc.config.user, wc.config.machine);
    let mut superposed: BTreeMap<String, Vec<String>> = BTreeMap::new();

    while let Some(snapshot_hash) = state.pending.first().copied() {
        let snapshot: Snapshot = wc.store.get_json(Kind::Snapshot, &snapshot_hash)?;

        // Copy content first: chunks, blobs, versions, then the snapshot.
        let mut versions = Vec::new();
        for version_hash in &snapshot.versions {
            let version: Version = wc.store.get_json(Kind::Version, version_hash)?;
            if let Some(blob_hash) = version.blob {
                let blob: Blob = wc.store.get_json(Kind::Blob, &blob_hash)?;
                for chunk in &blob.chunks {
                    wc.store.copy_to(&hub.store, chunk)?;
                }
                wc.store.copy_to(&hub.store, &blob_hash)?;
            }
            wc.store.copy_to(&hub.store, version_hash)?;
            versions.push((*version_hash, version));
        }
        wc.store.copy_to(&hub.store, &snapshot_hash)?;

        // Then update the heads, briefly holding the lock.
        {
            let _lock = hub.lock()?;
            for (hash, version) in &versions {
                let parents: Vec<Hash> = version
                    .parents
                    .iter()
                    .map(|p| *state.aliases.get(p).unwrap_or(p))
                    .collect();
                match hub.apply(*hash, version, &parents)? {
                    Applied::Clean => {
                        superposed.remove(&version.path);
                    }
                    Applied::SameAs(same) => {
                        state.aliases.insert(*hash, same);
                        if let Some(entry) = state.files.get_mut(&version.path)
                            && entry.version == *hash
                        {
                            entry.version = same;
                        }
                        superposed.remove(&version.path);
                    }
                    Applied::Superposition(_) => {
                        let mut others = Vec::new();
                        for head in hub.heads(&version.path)? {
                            if head != *hash {
                                let who = hub.version(&head)?.who();
                                if !others.contains(&who) {
                                    others.push(who);
                                }
                            }
                        }
                        superposed.insert(version.path.clone(), others);
                    }
                }
            }
            hub.set_machine_snapshot(&who, &snapshot_hash)?;
        }

        state.pending.remove(0);
        if state.pending.is_empty() {
            state.aliases.clear();
        }
        wc.save_state(state)?;
    }
    Ok(Upload::Shared {
        superposed: superposed.into_iter().collect(),
    })
}

/// `cap status`: what's changed here and what's waiting on the hub.
pub fn status(start: &Path) -> Result<()> {
    let wc = WorkCopy::find(start)?;
    let state = wc.load_state()?;
    let scan = wc.scan(&state, None)?;
    let mut all_clear = true;

    if !scan.changes.is_empty() {
        all_clear = false;
        let n = scan.changes.len();
        println!(
            "You have {n} unsaved {} (run `cap done` to save and share):",
            if n == 1 { "change" } else { "changes" }
        );
        print_change_list(&scan.changes);
        println!();
    }

    if !state.pending.is_empty() {
        all_clear = false;
        let n = state.pending.len();
        println!(
            "{n} {} saved here but not yet shared with the hub. `cap done` will try again.",
            if n == 1 { "save is" } else { "saves are" }
        );
        println!();
    }

    match &wc.config.hub {
        None => println!("No hub is set up, so nothing is shared."),
        Some(hub_path) => match Hub::open(hub_path) {
            Err(_) => {
                all_clear = false;
                println!("Can't reach the hub at {} right now.", hub_path.display());
            }
            Ok(hub) => {
                let me = format!("{}@{}", wc.config.user, wc.config.machine);
                let mut superposed = Vec::new();
                let mut newer = Vec::new();
                for (path, heads) in hub.all_heads()? {
                    let mine = state.files.get(&path).map(|e| e.version);
                    let resolve = |h: &Hash| *state.aliases.get(h).unwrap_or(h);
                    if heads.len() > 1 {
                        let mut others = Vec::new();
                        let mut oldest = i64::MAX;
                        for head in &heads {
                            if Some(resolve(head)) == mine {
                                continue;
                            }
                            let version = hub.version(head)?;
                            oldest = oldest.min(version.time);
                            let who = if version.who() == me {
                                "you".to_string()
                            } else {
                                version.who()
                            };
                            if !others.contains(&who) {
                                others.push(who);
                            }
                        }
                        superposed.push((path, others, oldest));
                    } else if let Some(head) = heads.first()
                        && Some(resolve(head)) != mine
                    {
                        let version = hub.version(head)?;
                        let deleted_both = version.blob.is_none() && mine.is_none();
                        if !deleted_both {
                            newer.push((path, version.who()));
                        }
                    }
                }
                if !superposed.is_empty() {
                    all_clear = false;
                    let n = superposed.len();
                    println!(
                        "{n} {} more than one version on the hub:",
                        if n == 1 { "file has" } else { "files have" }
                    );
                    for (path, others, oldest) in superposed.iter().take(LIST_LIMIT) {
                        println!(
                            "  {path}  (other version from {}, {})",
                            others.join(", "),
                            age(*oldest)
                        );
                    }
                    println!(
                        "Nothing is lost. Alternates will appear beside your files once `cap get` arrives."
                    );
                    println!();
                }
                if !newer.is_empty() {
                    all_clear = false;
                    let n = newer.len();
                    println!(
                        "{n} {} newer on the hub than here:",
                        if n == 1 { "file is" } else { "files are" }
                    );
                    for (path, who) in newer.iter().take(LIST_LIMIT) {
                        println!("  {path}  (from {who})");
                    }
                    if n > LIST_LIMIT {
                        println!("  … and {} more", n - LIST_LIMIT);
                    }
                    println!();
                }
            }
        },
    }

    if all_clear {
        println!("Everything is saved and shared.");
    }
    Ok(())
}

/// `cap verify`: check that every stored object is intact and every file the
/// hub knows can be rebuilt.
pub fn verify(start: &Path) -> Result<bool> {
    let wc = WorkCopy::find(start)?;
    let mut problems = Vec::new();

    let local = wc.store.all_hashes()?;
    for hash in &local {
        if let Err(e) = wc.store.get(hash) {
            problems.push(format!("here: {e:#}"));
        }
    }
    let mut hub_count = 0;
    if let Some(hub_path) = &wc.config.hub {
        let hub = Hub::open(hub_path).context("checking the hub")?;
        let objects = hub.store.all_hashes()?;
        hub_count = objects.len();
        for hash in &objects {
            if let Err(e) = hub.store.get(hash) {
                problems.push(format!("hub: {e:#}"));
            }
        }
        for (path, heads) in hub.all_heads()? {
            for head in heads {
                let rebuilt = hub.version(&head).and_then(|v| match v.blob {
                    Some(blob) => {
                        let blob_obj: Blob = hub.store.get_json(Kind::Blob, &blob)?;
                        for chunk in &blob_obj.chunks {
                            hub.store.get(chunk)?;
                        }
                        Ok(())
                    }
                    None => Ok(()),
                });
                if let Err(e) = rebuilt {
                    problems.push(format!("hub can't rebuild {path}: {e:#}"));
                }
            }
        }
    }

    if problems.is_empty() {
        println!(
            "Checked {} objects here and {hub_count} on the hub. Everything is intact.",
            local.len()
        );
        Ok(true)
    } else {
        println!("Found {} problems:", problems.len());
        for problem in &problems {
            println!("  {problem}");
        }
        Ok(false)
    }
}
