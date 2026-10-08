# mirako

Remote builds over ssh: a project on this machine is mirrored to a build machine, a command runs there, and what it produced comes back.

## Language

**Client**:
The machine the developer works on, and the `mirako` process running there.
_Avoid_: local side, laptop

**Host**:
The build machine reached over ssh.
_Avoid_: server, remote, build box

**Agent**:
The `mirako serve` process on the host that one client session talks to.
_Avoid_: daemon, server

**Project copy**:
The host's mirror of one project, one folder per project root under the remote folder.
_Avoid_: remote project, workspace

**Run**:
One push, then the command on the host, then one pull, over a single session.
_Avoid_: build, job

**Push**:
Making the project copy equal to the client's tree within the upload scope, deletions included.
_Avoid_: upload, sync

**Pull**:
Bringing the host's changed files within the download scope back to the client; it never deletes on the client.
_Avoid_: download, fetch

**Upload scope**:
The files of the project a push mirrors, after the local and common excludes.

**Download scope**:
The files of the project copy a pull may bring back, after the remote and common excludes.
_Avoid_: outputs (a task's outputs are a narrower, Gradle-side notion)

**Cold push**:
A push into a project copy that does not exist yet: first run on a host, or after gc removed the copy for being unused.
_Avoid_: cold sync, initial sync

**Gradle shim**:
The init script that sends a Gradle build of this machine to the host as a run.
_Avoid_: plugin, hook

**Local fallback**:
Running the command on the client because the host cannot be reached.
