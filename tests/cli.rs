//! End-to-end tests that drive the real `cap` binary.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

use hubbcap::hub::Hub;
use hubbcap::workcopy::WorkCopy;

const CAP: &str = env!("CARGO_BIN_EXE_cap");

/// A project folder and a hub folder inside one temporary directory.
struct Setup {
    _temp: tempfile::TempDir,
    root: PathBuf,
    hub: PathBuf,
}

impl Setup {
    fn new() -> Setup {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        Setup {
            hub: root.join("hub"),
            root,
            _temp: temp,
        }
    }

    /// Creates `name/` and runs `cap init` in it.
    fn machine(&self, name: &str, user: &str) -> PathBuf {
        let dir = self.root.join(name);
        fs::create_dir_all(&dir).unwrap();
        let out = cap(
            &dir,
            &[
                "init",
                "--hub",
                self.hub.to_str().unwrap(),
                "--user",
                user,
                "--machine",
                name,
            ],
        );
        assert!(out.status.success(), "{}", text(&out));
        dir
    }
}

fn cap(dir: &Path, args: &[&str]) -> Output {
    Command::new(CAP)
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap()
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn ok(dir: &Path, args: &[&str]) -> String {
    let out = cap(dir, args);
    assert!(out.status.success(), "cap {args:?} failed:\n{}", text(&out));
    text(&out)
}

/// Deterministic pseudo-random bytes (xorshift), so tests need no extra crate.
fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

/// The content the hub holds for `path`, which must have exactly one head.
fn hub_content(hub: &Path, path: &str) -> Option<Vec<u8>> {
    let hub = Hub::open(hub).unwrap();
    let heads = hub.heads(path).unwrap();
    assert_eq!(
        heads.len(),
        1,
        "{path} should have one version on the hub, has {}",
        heads.len()
    );
    let version = hub.version(&heads[0]).unwrap();
    version.blob.map(|blob| hub.store.read_blob(&blob).unwrap())
}

#[test]
fn done_saves_and_shares_then_status_is_clear() {
    let setup = Setup::new();
    let dir = setup.machine("desktop", "jacob");
    fs::write(dir.join("readme.txt"), "hello").unwrap();
    fs::create_dir_all(dir.join("Library")).unwrap();
    fs::write(dir.join("Library/cache.bin"), "ignored by default").unwrap();

    assert!(ok(&dir, &["status"]).contains("unsaved"));
    let out = ok(&dir, &["done"]);
    assert!(out.contains("Saved 2 files"), "{out}"); // readme + .capignore
    assert!(out.contains("Shared with the hub."), "{out}");
    assert!(ok(&dir, &["status"]).contains("Everything is saved and shared."));
    assert!(ok(&dir, &["done"]).contains("Nothing changed"));

    assert_eq!(hub_content(&setup.hub, "readme.txt").unwrap(), b"hello");
    assert!(
        Hub::open(&setup.hub)
            .unwrap()
            .heads("Library/cache.bin")
            .unwrap()
            .is_empty()
    );
}

#[test]
fn edits_and_deletes_replace_the_previous_version() {
    let setup = Setup::new();
    let dir = setup.machine("desktop", "jacob");
    fs::write(dir.join("a.txt"), "one").unwrap();
    fs::write(dir.join("b.txt"), "bee").unwrap();
    ok(&dir, &["done"]);

    std::thread::sleep(Duration::from_millis(20));
    fs::write(dir.join("a.txt"), "two").unwrap();
    fs::remove_file(dir.join("b.txt")).unwrap();
    let out = ok(&dir, &["done", "second save"]);
    assert!(out.contains("1 changed, 1 deleted"), "{out}");
    assert!(out.contains("checkpoint \"second save\""), "{out}");

    assert_eq!(hub_content(&setup.hub, "a.txt").unwrap(), b"two");
    assert_eq!(
        hub_content(&setup.hub, "b.txt"),
        None,
        "b.txt should be recorded as deleted"
    );
    assert!(ok(&dir, &["verify"]).contains("Everything is intact"));
}

#[test]
fn large_file_edit_only_uploads_the_changed_chunks() {
    let setup = Setup::new();
    let dir = setup.machine("desktop", "jacob");
    let mut art = noise(12 * 1024 * 1024, 7);
    fs::write(dir.join("hero.psd"), &art).unwrap();
    ok(&dir, &["done"]);
    let before = Hub::open(&setup.hub)
        .unwrap()
        .store
        .all_hashes()
        .unwrap()
        .len();

    // Change 1 KB in the middle of a 12 MB file.
    for byte in &mut art[6_000_000..6_001_024] {
        *byte = byte.wrapping_add(1);
    }
    fs::write(dir.join("hero.psd"), &art).unwrap();
    ok(&dir, &["done"]);
    let added = Hub::open(&setup.hub)
        .unwrap()
        .store
        .all_hashes()
        .unwrap()
        .len()
        - before;

    // A version, a snapshot, a blob and a handful of chunks — not ~12 chunks.
    assert!(added <= 7, "uploading a small edit added {added} objects");
    assert_eq!(hub_content(&setup.hub, "hero.psd").unwrap(), art);
}

#[test]
fn two_machines_editing_the_same_file_make_a_superposition() {
    let setup = Setup::new();
    let desktop = setup.machine("desktop", "jacob");
    let laptop = setup.machine("laptop", "teej");
    fs::write(desktop.join("player.lua"), "jacob's version").unwrap();
    fs::write(laptop.join("player.lua"), "teej's version").unwrap();
    fs::write(desktop.join("shared.txt"), "same content").unwrap();
    fs::write(laptop.join("shared.txt"), "same content").unwrap();

    ok(&desktop, &["done"]);
    let out = ok(&laptop, &["done"]);
    assert!(
        out.contains("player.lua  (also changed by jacob@desktop)"),
        "{out}"
    );
    assert!(
        !out.contains("shared.txt  (also"),
        "identical content isn't a conflict:\n{out}"
    );

    let hub = Hub::open(&setup.hub).unwrap();
    assert_eq!(hub.heads("player.lua").unwrap().len(), 2);
    assert_eq!(hub.heads("shared.txt").unwrap().len(), 1);

    let status = ok(&desktop, &["status"]);
    assert!(
        status.contains("player.lua  (other version from teej@laptop"),
        "{status}"
    );

    // Both versions are intact, and each machine still has its own file.
    assert_eq!(
        fs::read(desktop.join("player.lua")).unwrap(),
        b"jacob's version"
    );
    assert_eq!(
        fs::read(laptop.join("player.lua")).unwrap(),
        b"teej's version"
    );
}

#[test]
fn done_while_the_hub_is_offline_queues_the_upload() {
    let setup = Setup::new();
    let dir = setup.machine("desktop", "jacob");
    let away = setup.root.join("hub-unplugged");
    fs::rename(&setup.hub, &away).unwrap();

    fs::write(dir.join("notes.txt"), "written on a plane").unwrap();
    let out = ok(&dir, &["done"]);
    assert!(
        out.contains("Saved on this machine. Couldn't share"),
        "{out}"
    );
    assert!(ok(&dir, &["status"]).contains("not yet shared"));

    fs::rename(&away, &setup.hub).unwrap();
    let out = ok(&dir, &["done"]);
    assert!(out.contains("Shared with the hub."), "{out}");
    assert_eq!(
        hub_content(&setup.hub, "notes.txt").unwrap(),
        b"written on a plane"
    );
}

/// Milestone 1 gate: kill `cap done` at many points mid-run; nothing may be
/// lost or corrupted, and the next `cap done` must finish the job.
#[test]
fn killing_done_midway_never_loses_or_corrupts_work() {
    let setup = Setup::new();
    let dir = setup.machine("desktop", "jacob");
    let mut files: Vec<(String, Vec<u8>)> = (0..24)
        .map(|i| {
            (
                format!("dir{}/file{i}.bin", i % 4),
                noise(150_000 + i * 20_000, i as u64),
            )
        })
        .collect();
    for (path, content) in &files {
        let full = dir.join(path.as_str());
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, content).unwrap();
    }

    for round in 0..16u64 {
        // Change a few files each round, so every round has real work to do.
        for (i, (path, content)) in files.iter_mut().enumerate() {
            if (i as u64 + round).is_multiple_of(3) {
                *content = noise(content.len(), round * 100 + i as u64);
                let full = dir.join(path.as_str());
                fs::create_dir_all(full.parent().unwrap()).unwrap();
                fs::write(full, &*content).unwrap();
            }
        }

        let mut child = Command::new(CAP)
            .arg("done")
            .current_dir(&dir)
            .env("CAP_TEST_DELAY_MS", "3")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        // Spread the kill points across a typical run (about 180 ms here).
        std::thread::sleep(Duration::from_millis(5 + (round * 53) % 170));
        let _ = child.kill();
        let _ = child.wait();

        // The working copy is never touched by `done`.
        for (path, content) in &files {
            assert_eq!(
                &fs::read(dir.join(path.as_str())).unwrap(),
                content,
                "round {round}: {path} changed on disk"
            );
        }
        // Whatever state it was killed in, the next run completes it.
        let out = ok(&dir, &["done"]);
        assert!(!out.contains("Couldn't share"), "round {round}: {out}");
        for (path, content) in &files {
            assert_eq!(
                &hub_content(&setup.hub, path).unwrap(),
                content,
                "round {round}: hub has the wrong {path}"
            );
        }
        assert!(
            ok(&dir, &["verify"]).contains("Everything is intact"),
            "round {round}"
        );
        assert!(
            ok(&dir, &["status"]).contains("Everything is saved and shared."),
            "round {round}"
        );
    }

    // The working copy's own record agrees with the hub.
    let wc = WorkCopy::find(&dir).unwrap();
    assert!(wc.load_state().unwrap().pending.is_empty());
}

#[test]
fn two_dones_at_the_same_moment_keep_both_versions() {
    let setup = Setup::new();
    let desktop = setup.machine("desktop", "jacob");
    let laptop = setup.machine("laptop", "teej");
    for i in 0..30 {
        fs::write(desktop.join(format!("f{i}.txt")), format!("desktop {i}")).unwrap();
        fs::write(laptop.join(format!("f{i}.txt")), format!("laptop {i}")).unwrap();
    }
    let spawn = |dir: &Path| {
        Command::new(CAP)
            .arg("done")
            .current_dir(dir)
            .env("CAP_TEST_DELAY_MS", "2")
            .output()
    };
    let (a, b) = std::thread::scope(|s| {
        let a = s.spawn(|| spawn(&desktop));
        let b = s.spawn(|| spawn(&laptop));
        (a.join().unwrap().unwrap(), b.join().unwrap().unwrap())
    });
    assert!(
        a.status.success() && b.status.success(),
        "{}\n{}",
        text(&a),
        text(&b)
    );

    // Neither machine's version was lost to the other's update.
    let hub = Hub::open(&setup.hub).unwrap();
    for i in 0..30 {
        let heads = hub.heads(&format!("f{i}.txt")).unwrap();
        assert_eq!(heads.len(), 2, "f{i}.txt should keep both versions");
    }
}
