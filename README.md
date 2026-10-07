# mirako

Run your builds on another machine without noticing.

```
mirako ./gradlew assembleDebug
```

mirako syncs the project to a remote host over ssh, runs the command there, and brings the
build outputs back. It is a single Rust binary, installed on both ends, in the spirit of
[Mainframer](https://github.com/buildfoundation/mainframer) and
[Mirakle](https://github.com/Adambl4/mirakle) but without rsync:

- **Content-hash sync.** Both sides keep a blake3 index (re-hashing only files whose size or
  mtime changed), exchange manifests once, and transfer only what differs. A warm sync of a
  5 000-file Android project is ~100 ms.
- **Block deltas for big outputs.** Files ≥ 256 KB that exist on both sides travel as rsync-style
  block deltas (64 KB blocks, rolling checksum + blake3). An 84 MB APK rebuilt after a
  one-line change comes back as ~200 KB on the wire.
- **zstd, pipelined, one ssh session.** Every phase (push, exec, pull) runs on the same
  connection; transfers stream without per-file round trips, and the requests of each phase
  are queued ahead of the replies they do not need: the whole round trip of a no-op run is
  three link latencies (handshake + push manifest, flush + exec, pull).
- **Never deletes locally.** The upload mirrors the remote to your sources (deletes there);
  the download only adds or replaces files. Your `.git`, `local.properties` and IDE state
  are untouched.
- **Output paths rewritten.** Remote paths in the build log are replaced by local ones, so
  error links in the terminal and the IDE keep working.
- **Android Studio support** through a 60-line Gradle init script that hands every build to
  mirako (and silently builds locally when the host is down).

## Install

Every [release](https://github.com/Nam0101/mirako/releases) has binaries for macOS (arm64),
Linux (x86_64 and arm64, static) and Windows (x86_64): put the one for your machine on `PATH`
as `mirako`. Or build it:

```
cargo install --git https://github.com/Nam0101/mirako
```

Then, once:

```
mirako setup --host m4                   # global config, Gradle init script, agent on the host, handshake
```

`setup` first makes sure `ssh m4` works without a password (if not, it generates a key when
there is none and runs `ssh-copy-id`, which asks for the host's password once), reports what
the agent's non-interactive shell on the host sees (java, `ANDROID_HOME`, free disk) with a
warning for each missing piece, writes the Gradle init script, and handshakes, which puts this
binary on the host as `~/.local/bin/mirako` (`remote_bin`). A host with another OS or
architecture gets the agent built there instead, by its own cargo from this version's release
tag (`cargo install --git … --tag v<version>`, a few minutes), so Rust must be installed on it.
From then on the client keeps both in step: a handshake that finds no agent, or one of another
version, installs it the same way and retries, and `mirako run` rewrites the Gradle init script
when it is out of date, so after a `cargo install` nothing else is needed. The pieces on their
own: `mirako init --global`, `mirako gradle-shim install`, `mirako remote-install --host m4`.

The remote needs whatever the command needs (JDK, Android SDK, …) reachable from a
non-interactive ssh shell. On macOS put `JAVA_HOME`/`ANDROID_HOME`/`PATH` in `~/.zshenv`.

### Windows

Windows is a client only: the builds run on a macOS or Linux host, reached with the `ssh` that
ships with Windows. What differs from a Unix client:

- `mirako setup` cannot set up key authentication there (no `ssh-copy-id`): make `ssh <host>`
  work without a password first.
- The agent is always built on the host by its cargo, as for any host of another OS, so Rust
  must be installed on the host.
- Windows has no permission bits: every file is uploaded as 0755, which lets `./gradlew` run on
  the host. It also needs LF line endings there (`gradlew text eol=lf` in `.gitattributes`).
- Symlinks among the downloaded files are not created; one line on stderr counts them
  (`exclude_remote_extra` leaves them out).
- With `fallback = true`, a `./gradlew …` that cannot reach the host runs the `gradlew.bat`
  next to it.

## Configure

`mirako init --global` writes `~/.config/mirako/config.toml`:

```toml
host = "m4"                      # ssh host or ~/.ssh/config alias
remote_folder = "~/mirako"       # one sub-folder per project on the remote
remote_bin = "~/.local/bin/mirako"
fallback = true                  # run locally when the host is unreachable
gc_days = 7                      # remove a project's remote copy unused for this long (Gradle's caches there too); 0 = never
# gc_after_pull = ["build/intermediates", "build/tmp"]   # deleted on the remote after every pull: saves disk, costs a clean build next time
# shim_check = false             # Gradle shim: skip the ~0.1 s handshake before each build (a dead host then fails, or falls back, inside `mirako run`)
# env = ["KEYSTORE_PASSWORD", "ORG_GRADLE_PROJECT_*"]    # variables of this machine the remote command gets (a name, or a prefix ending in *)
# ssh = ["ssh", "-o", "BatchMode=yes"]
# exclude_local  = ["build"]
# exclude_remote = ["src"]
# exclude_common = [".gradle", ".idea", ".git", ".kotlin", ".mirako", "mirako.toml", ".DS_Store"]
```

A `mirako.toml` next to `gradlew` overrides any of these per project (`mirako init` writes a
sample). The `*_extra` keys append instead of replacing:

```toml
exclude_remote_extra = ["build/intermediates", "!build/intermediates/apk_ide_redirect_file", "!build/intermediates/apk", "build/tmp", "build/kotlin", "build/kspCaches"]
```

Patterns are rsync-like: `build` matches at any depth, `build/intermediates` matches that
relative path at any depth, `/local.properties` is anchored at the project root, `*.log` is a
glob that never crosses `/`. A `!pattern` keeps that path even if an earlier pattern excludes
it (Android Studio deploys through `apk_ide_redirect_file`, which points into
`build/intermediates/apk`, so both come back).

- `exclude_local`: not uploaded (your local build outputs)
- `exclude_remote`: not downloaded (sources on the remote)
- `exclude_common`: never synced either way

`local.properties` is uploaded without its `sdk.dir`, `ndk.dir` and `cmake.dir` lines: the keys a
build reads from it (API keys, the secrets plugin) are there on the host, and the SDK is still
found through the host's `ANDROID_HOME`. It is never downloaded. This goes for every file of
that name in the project (an included build has its own). A `local.properties` written by hand
on the host is replaced like any other file, which 0.5 did not do: the host's SDK path belongs
in `ANDROID_HOME` (`mirako setup` checks it). To keep the file off the host altogether, add it
to `exclude_local_extra`; a copy an earlier run uploaded stays there until you delete it.

`env` names the environment variables of this machine that the remote command gets on top of
the host's own (signing passwords, `ORG_GRADLE_PROJECT_*` properties): a name, or a prefix
ending in `*`. They travel inside the ssh connection and are not written to the host's disk.

## Use

```
mirako ./gradlew assembleDebug              # push → run → pull
mirako run --no-pull -- ./gradlew test      # skip the download
mirako push | mirako pull | mirako check    # the phases on their own
mirako gc [--days N] [--dry-run]            # what is on the remote, remove the stale copies
mirako --help
```

Stopping a run (Ctrl-C, the stop button of Android Studio) stops the command on the host as
well: when the connection closes the agent sends SIGTERM to the command and everything it
started, and SIGKILL to what is left two seconds later at most. Two runs of one project never
overlap: the second waits for the first.

### Keeping the remote's disk in check

Every project gets a copy under `remote_folder`, build outputs included, and it stays there
after you stop working on the project. After each run, and on `mirako gc`, the agent removes
the copies that have not been synced for more than `gc_days` days (default 7; the copy of the
project being built is never touched) and tells Gradle on the host to drop cache entries and
wrapper distributions unused for the same time (`~/.gradle/init.d/mirako-gc.gradle`, removed
again with `gc_days = 0`). A copy is recognised by the agent's own index of it, so other
directories in `remote_folder` are listed but never removed. Leftover `.mirako.*.tmp` files
from an interrupted transfer are deleted by the next scan on either side.

`gc_after_pull` goes further: the patterns are deleted inside the project's remote copy after
every pull, which is the one place the intermediates you never download can be reclaimed.
The next build is then a clean build there, so leave it unset unless the disk is the problem.

Use `ssh` `ControlMaster`/`ControlPersist` in `~/.ssh/config` so the connection is reused
between builds; the handshake then costs ~70 ms.

## Android Studio / `./gradlew`

```
mirako gradle-shim install     # writes ~/.gradle/init.d/mirako.gradle (`mirako setup` does this too)
```

From then on every Gradle build on this machine, including the ones Android Studio starts,
runs on the remote: the init script points Gradle at an empty stub project, registers a single
`mirako` task that invokes `mirako run -- ./gradlew <your args>`, and streams the output back.
Deploy-to-device works because the APK is pulled into `app/build/outputs` before Gradle
finishes.

- one build locally: `./gradlew <task> -x mirako` (or `-Pmirako.disabled`)
- `updateDaemonJvm` and `wrapper` (they edit the project's Gradle config) always run locally
- one project always local: `mirako.enabled=false` in its `local.properties`
- host unreachable: the build simply runs locally. The script asks `mirako check` first (one
  handshake, ~0.1 s); `shim_check = false` in the global config skips that, and a dead host then
  fails the build, or with `fallback = true` runs it locally inside `mirako run`
- the script follows the binary: `mirako run`/`check` rewrite it when this binary's copy of it
  is out of date (after a `cargo install`, or a `shim_check` change); a mirako started from
  another path never touches it
- back to local builds for good: delete `~/.gradle/init.d/mirako.gradle`

## How it works

```
╭──────────── local ────────────╮        ssh         ╭──────────── remote ───────────╮
│ scan + blake3 index (cached)  │ ◀─── manifest ──── │ scan + blake3 index (cached)  │
│ diff → Put / Delta / Delete   │ ═══ zstd frames ═▶ │ apply into tmp, rename        │
│ Exec                          │ ──────────────────▶│ ./gradlew …  (MIRAKO_REMOTE=1)│
│ rewrite paths, print          │ ◀═══ Output ═══════│                               │
│ meanwhile: scan outputs, sigs │ ── Pull ─────────▶ │ scan, diff against the client │
│ apply into tmp, rename        │ ◀═ Put / Delta ═══ │ rolling-checksum delta vs sig │
╰───────────────────────────────╯                    ╰───────────────────────────────╯
```

The download is driven by the client's own manifest: while the command runs, the client
scans its copy of the download scope, computes block signatures of its big files and sends
both up, so the moment the command exits the agent streams exactly what differs, deltas
included, without another exchange.

The protocol is length-prefixed postcard frames on the agent's stdin/stdout (`mirako serve`),
so nothing listens on a port and ssh handles auth and encryption.

## License

MIT
