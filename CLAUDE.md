# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`mirako` is a single-binary Rust CLI (one crate, no workspace) that runs a build on a remote host over ssh: content-hash sync the project up, run the command there, pull the outputs back. The same binary is the client (`mirako run …`) and the remote agent (`mirako serve`). README.md is the user-facing doc and is accurate; keep it in sync when CLI flags, config keys or the Gradle shim change.

## Commands

```
cargo build --release          # binary at target/release/mirako (lto, strip)
cargo test                     # unit tests only (patterns, delta, rewrite)
cargo test delta::             # one module's tests
cargo test insertion_in_the_middle_only_sends_the_change
cargo clippy
cargo fmt                      # rustfmt.toml: max_width = 140
```

There is no integration test harness; end-to-end behaviour is checked manually against a real ssh host (`mirako check`, then `mirako ./gradlew assembleDebug` in an Android project). `mirako gradle-shim print` shows the generated init script without installing it.

## Architecture

Flow of one `mirako run`: `main.rs` (clap) → `config.rs` (global toml overlaid by project `mirako.toml`, `--host` wins) → `client::run` which opens one `Session` (an `ssh host 'mirako serve'` child, stdio piped) and does push → exec → pull on that single connection, then prints stats.

- `proto.rs` — the wire protocol: `u32` big-endian length + bincode frame, `Req` (client→agent) / `Resp` (agent→client). Transfers are pipelined: `Put`/`Delta`/`Delete`/`Symlink` are sent without waiting; only `Flush` (answered by `Ack { failed }`) and `Manifest`/`Sigs`/`Fetch`/`Exec` are request/response. The client also queues `Exec` and the pull's `Manifest` right behind the push's `Flush` (one round trip for the three phases); after an `Ack` with failures the agent refuses `Exec` until a clean `Flush`, and the client drops those replies, resends the files whole and queues again. `VERSION` is the crate version and both sides must match exactly (client bails otherwise), so bumping `Cargo.toml` is a protocol break that requires `mirako remote-install`.
- `client.rs` — `Session::{push, exec_output, pull}`; `run` drives the phases and scans the local pull scope on a thread while the command runs. Push mirrors local → remote (sends deletes); pull never deletes locally. Fallback: if ssh fails and `fallback = true`, `run_local` runs the command locally with `MIRAKO_LOCAL=1`.
- `server.rs` — `serve()` loop. Stateful: `Manifest` must precede `Put`/`Delta`/`Delete` (it sets the root and opens the index). Exec sets `MIRAKO_REMOTE=1` and pumps stdout/stderr as `Resp::Output` frames from two threads sharing a mutexed writer. Logs go to stderr only; stdout is the protocol channel, so never `println!` in server code.
- `index.rs` — blake3 manifest with a persistent cache keyed by (size, mtime); cache file lives under the OS cache dir (`~/Library/Caches/mirako/<hash>.idx`), one per project root path. Hashing is parallel via rayon.
- `delta.rs` — rsync algorithm (64 KB blocks, weak rolling checksum + truncated blake3). Used in both directions for files in `delta_worthwhile` range (256 KB–1 GB) that exist on both sides. A delta that rebuilds to the wrong hash is reported back and the file is resent whole.
- `xfer.rs` — receiving side shared by client and agent (`Inbox`): writes to `.mirako.<name>.tmp` next to the destination and renames on completion. `safe_join` rejects `..` and absolute paths; every relative path from the wire must go through it.
- `patterns.rs` — rsync-like excludes compiled to globset (`build` = any depth, `/x` = anchored, `*` never crosses `/`, `!x` keeps a path inside an excluded dir; the walk only descends into ancestors of an include). `exclude_local` (not uploaded) / `exclude_remote` (not downloaded) / `exclude_common` (neither); the `*_extra` config keys append.
- `rewrite.rs` — replaces the remote project path with the local one in streamed build output, line-buffered.
- `shim.rs` — the Gradle init script as a string constant (`INIT_SCRIPT`, `__BIN__` placeholder). It redirects Gradle to an empty stub project under `~/.gradle/mirako/stubs/` and registers one `mirako` Exec task; it bails out (local build) on `updateDaemonJvm`/`wrapper` tasks, `MIRAKO_REMOTE`, `MIRAKO_LOCAL`, `-x mirako`, `-Pmirako.disabled`, `mirako.enabled=false` in `local.properties`, or a failed `mirako check`. Edits here are Groovy inside a Rust raw string; test with `mirako gradle-shim print`.

Paths on the wire are always `/`-separated and relative to the project root; `~/` in `remote_folder`/`remote_bin` is expanded by the remote agent (`server::expand_home`), not the client. Unix-only (uses `std::os::unix`).
