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

```
cargo install --git https://github.com/Nam0101/mirako
```

On the remote, either run the same command or, when both machines share the OS and
architecture (e.g. two Apple-silicon Macs):

```
mirako remote-install --host m4          # copies this binary to ~/.local/bin/mirako there
```

The remote needs whatever the command needs (JDK, Android SDK, …) reachable from a
non-interactive ssh shell. On macOS put `JAVA_HOME`/`ANDROID_HOME`/`PATH` in `~/.zshenv`.

## Configure

`mirako init --global` writes `~/.config/mirako/config.toml`:

```toml
host = "m4"                      # ssh host or ~/.ssh/config alias
remote_folder = "~/mirako"       # one sub-folder per project on the remote
remote_bin = "~/.local/bin/mirako"
fallback = true                  # run locally when the host is unreachable
gc_days = 7                      # remove a project's remote copy unused for this long (Gradle's caches there too); 0 = never
# gc_after_pull = ["build/intermediates", "build/tmp"]   # deleted on the remote after every pull: saves disk, costs a clean build next time
# ssh = ["ssh", "-o", "BatchMode=yes"]
# exclude_local  = ["build"]
# exclude_remote = ["src"]
# exclude_common = [".gradle", ".idea", ".git", ".kotlin", ".mirako", "local.properties", "mirako.toml", ".DS_Store"]
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

## Use

```
mirako ./gradlew assembleDebug              # push → run → pull
mirako run --no-pull -- ./gradlew test      # skip the download
mirako push | mirako pull | mirako check    # the phases on their own
mirako gc [--days N] [--dry-run]            # what is on the remote, remove the stale copies
mirako --help
```

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
mirako gradle-shim install     # writes ~/.gradle/init.d/mirako.gradle
```

From then on every Gradle build on this machine, including the ones Android Studio starts,
runs on the remote: the init script points Gradle at an empty stub project, registers a single
`mirako` task that invokes `mirako run -- ./gradlew <your args>`, and streams the output back.
Deploy-to-device works because the APK is pulled into `app/build/outputs` before Gradle
finishes.

- one build locally: `./gradlew <task> -x mirako` (or `-Pmirako.disabled`)
- `updateDaemonJvm` and `wrapper` (they edit the project's Gradle config) always run locally
- one project always local: `mirako.enabled=false` in its `local.properties`
- host unreachable: the build simply runs locally
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

The protocol is length-prefixed bincode frames on the agent's stdin/stdout (`mirako serve`),
so nothing listens on a port and ssh handles auth and encryption.

## License

MIT
