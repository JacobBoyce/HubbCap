# HubbCap

Version control that gets out of your way. The command is `cap`.

You work, then type `cap done`. Everything you changed is saved and shared. When two people change the same file, nobody is told to merge first: the hub keeps both versions (a *superposition*), your version stays on your disk, and you sort it out whenever you like.

The design comes from the "Real Problems with Git" discussion (Casey, Prime, Teej, Vegan). The full design doc covers the model, storage, sync rules and roadmap.

## Status

**Milestone 1, local core: done.** `init`, `done`, `status` and `verify` work against a hub that is a plain folder (a local path or a network share).

Next is Milestone 2, two machines: `cap clone`, `cap get`, and alternates written beside your files (`hero.alt-teej.psd`).

## Try it

```sh
cargo build --release
# the binary is target/release/cap (cap.exe on Windows)

cd my-project
cap init --hub ../my-hub --user jacob --machine desktop
cap done                       # save and share everything
cap done "player jump works"   # same, but names this save as a checkpoint
cap status                     # what's changed, what's waiting on the hub
cap verify                     # check every stored object is intact
```

`cap init` writes a `.capignore` (same syntax as `.gitignore`) that already skips Unity's `Library/`, `Temp/`, `Obj/` and `Logs/` folders, plus common build output.

## How it stores things

- Files are split into content-defined chunks (FastCDC, ~1 MiB average), compressed with zstd, and named by their BLAKE3 hash. Editing the middle of a 2 GB file stores only the few chunks that changed.
- Every write goes to a temporary file, is flushed to disk, then renamed into place. A crash leaves the old file or the complete new one, never a mix.
- Every read checks the content against its hash; corruption is reported, never used.
- `cap done` never changes your files. If the hub can't be reached, the work is saved locally and uploads on the next `cap done`.
- The hub records, per file, its current version(s). A new version replaces only the version it was built on, so two people's changes can never overwrite each other.

## Code layout

| File | What it does |
| --- | --- |
| `src/main.rs` | Command-line interface |
| `src/ops.rs` | The commands: `init`, `done`, `status`, `verify` |
| `src/workcopy.rs` | Your project folder: config, state, scanning, `.capignore` |
| `src/hub.rs` | The shared hub: heads per file, locking |
| `src/store.rs` | Content-addressed object store and chunking |
| `src/model.rs` | Blob, Version and Snapshot records |
| `src/fsutil.rs` | Crash-safe file writes |
| `tests/cli.rs` | End-to-end tests, including the crash test |

## Tests

```sh
cargo test
```

The Milestone 1 gate is `killing_done_midway_never_loses_or_corrupts_work`. It kills `cap done` at 16 different points during real work, then checks that no file changed on disk, that the next `cap done` finishes the job, and that every object verifies. CI runs the suite on Windows, macOS and Linux.

Safety rules: no `unsafe` code (`#![forbid(unsafe_code)]`), and clippy must pass with no warnings.
