# jj-workfuse

**Implemented initial slice:** a read-only, lazy Linux FUSE mount plus an
independently usable revision/tree/object projection API. Native object access
uses version-pinned `jj-lib = 0.46.0`, not checkout/export or per-read commands.

Mount just the part of a Jujutsu repository you need, from the revision you
want, without checking out or materializing the entire tree.

Written in Rust. Linux FUSE is the first target.

## Why

A plugin may live in `plugin/` inside a much larger feature repository. Loading
that plugin should not require keeping a full feature checkout on disk. More
generally, a tool should be able to consume a sub-workspace of an arbitrary jj
repository through ordinary filesystem paths while jj's object store remains
the source of its contents.

For example, OpenCode could load the plugin directory from a mounted revision
of `opencode-cbirds`, even when that revision has no materialized workspace.
The repository/object store still has to be accessible; avoiding a checkout
does not mean avoiding storage for the objects being read.

## Build and use

Requires Rust 1.97.1 or newer. Mounting also needs `/dev/fuse` and host mount
permission; non-root Linux usually needs `fusermount3` (the `fuse3` package).
No libfuse development library is required.

```sh
cargo build --release --locked
mkdir -p ~/mnt/cbirds-plugin
target/release/jj-workfuse mount ~/src/opencode-cbirds \
  --revision '<commit-id-or-bookmark>' --subdir plugin ~/mnt/cbirds-plugin
```

The process stays in the foreground. Ctrl-C/SIGTERM unmounts; another terminal
can use `fusermount3 -u ~/mnt/cbirds-plugin`. The mountpoint must be empty. Put
build outputs and installed dependencies outside the read-only view.

Every selection and mount option also reads a `JJ_WORKFUSE_*` environment
variable (`REPOSITORY`, `REVISION`, `SUBDIR`, `WORKSPACE`, `MOUNTPOINT`,
`CACHE_MIB`); explicit CLI arguments win. This exists for service managers,
see below.

## Running under systemd

The mount command is a well-behaved service process: foreground by design,
`SIGTERM` (systemd's stop signal) unmounts and exits `0`, and it sends
`sd_notify` `READY=1` with the pinned provenance exactly when the kernel
mount exists — so `Type=notify` units order dependents correctly and start
failures (unresolvable revision, non-empty mountpoint) show up as real unit
failures. A filled-in example unit and instance templates live in
[examples/systemd](examples/systemd/README.md); a quick test:

```sh
systemd-run --user --unit=jjwf-demo -p Type=notify \
  -p Environment=JJ_WORKFUSE_REPOSITORY=$HOME/src/opencode-cbirds \
  -p Environment=JJ_WORKFUSE_REVISION=main -p Environment=JJ_WORKFUSE_SUBDIR=plugin \
  -p Environment=JJ_WORKFUSE_MOUNTPOINT=$HOME/mnt/cbirds-plugin \
  target/release/jj-workfuse mount
```

The mount exposes `plugin/` as its root, preserving executable bits and recorded
symlink targets. It lazily traverses selected trees and reads requested blobs;
there is no hidden checkout. **The selected revision need not be materialized.**
Only the local repository metadata and referenced objects need to remain available.

Every invocation resolves and pins one source operation and commit. Bookmarks
moving later do not change a mount. `@` is the default, but means jj's stored
working-copy commit, **not unsnapshotted disk edits**. Workspace paths, paths
beneath them, `.jj`, and the shared `.jj/repo` metadata directory are accepted:

```sh
target/release/jj-workfuse inspect /some/repo/.jj/repo \
  --revision '<commit-id>' --subdir plugin
# Or select an existing workspace identity explicitly for @:
target/release/jj-workfuse inspect /some/repo/.jj/repo \
  --workspace default --revision @ --subdir plugin
```

Standard local Git-backed and Simple-backed jj stores are supported. Version
compatibility is tied to the pinned library, not every past/future/custom backend.
User jj configuration and custom revset aliases are not loaded. Revision
expressions must select exactly one commit. Concurrent operation heads fail
rather than implicitly rewriting/reconciling source history.

## Projection without FUSE

All commands share `repository`, `--revision`, `--subdir`, and `--workspace`:

```sh
jj-workfuse inspect /repo --revision main --subdir plugin
jj-workfuse ls /repo --revision main --subdir plugin nested
jj-workfuse cat /repo --revision main --subdir plugin nested/index.js
jj-workfuse objects /repo --revision main --subdir plugin
jj-workfuse object /repo --revision main --subdir plugin --kind file --id '<id>'
jj-workfuse history /repo --revision main --subdir plugin --max-commits 10000
```

Inspection/listing/history are JSON. `cat` and guarded file/symlink object reads
emit bytes; tree object reads emit JSON entries. Rust callers can use
`SourceRepo::open(...).project(...)`, `Projection::{entry,list,read,objects,
read_object,history}` and the directly testable `ReadOnlyFs` operation layer.

## The heart: filtered revisions and valid objects

FUSE is only the filesystem adapter. The core defines a scoped, immutable view:

- `objects` includes only typed tree/file/symlink references reachable from the
  selected root, including conflict-side evidence. Unrelated source objects,
  original commit payloads and boundary ancestor trees are not an access grant.
  `object` rejects available-but-out-of-scope IDs. Availability is checked when
  reading: reachable references can still name missing objects.
- History retains **every source ancestor**, including unchanged vertices,
  merges and the root, with explicitly new `wf-commit-v1:` identities and
  source mappings. Parent order/topology is preserved. Projected trees use
  `wf-tree-v1:` identities. Neither is an original jj commit/tree identity or
  an imported synthetic repository. An explicit size guard never silently
  truncates the graph.
- Cross-boundary moves are deletion/insertion. Absent, non-directory, blocked
  and conflicted historical roots remain explicit. Conflicts retain all jj
  merge terms; all-tree conflicts can be traversed, while conflicted file/type
  entries fail reads instead of choosing a side. Git submodules are opaque,
  unsupported placeholders, not a route to another repository's commits.
- No source snapshot, checkout, export, commit write or per-read subprocess is
  performed. Native jj metadata/index caches may be rebuilt. Symlinks preserve
  their targets and **may escape the mount when followed by a host consumer**:
  this is not a security sandbox.

The exact identity, history, object-access, conflict and cache contract lives in
[design/projection.md](design/projection.md). This intentionally conservative
first API avoids claiming a history simplification algorithm it has not defined.

## Verification

```sh
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo fmt --check
cargo build --locked
python3 examples/smoke.py          # Real jj CLI fixture, no mount needed
python3 examples/smoke.py --fuse   # Actual mount + Node require() if installed
```

Rust tests build real, unmaterialized repositories through jj-lib and test the
core and filesystem operations independently of FUSE. The smoke example creates
a revision with a plugin and unrelated content, removes its materialized
checkout, loads the plugin through FUSE, checks executable/symlink/read-only
behavior, and confirms source operation/working-tree state did not change.
Setup uses the jj CLI; production reads do not. See
[verification](doc/verification.md) for the recorded environment and results.

## Limits and direction

The file-content cache defaults to 64 MiB (`--cache-mib`). Visited metadata,
kernel caches and native backend caches are outside that budget. The first file
attribute request scans that file for size; oversized blobs use uncached range
reads. No whole-tree content scan is required for mounting.

Writable overlays/write-back, bookmark follow mode, remote missing-object
fetching, history simplification, conflict-marker files and a synthetic jj repo
are deferred. The long-term aspiration remains a useful sub-workspace interface,
not merely a directory export. Read-only is the first milestone, not the full
meaning of a writable jj workspace. Future write-back must preserve changes
outside its selected subtree and define new history identities deliberately.

The original aspirational README is preserved in commit `786a1a61`.

## Origin

Requested by rektide in OpenCode session `ses_ed3c601a6ffeWv3Kh6sSEj3q4W`
(silent-meadow), 2026-10-11, while discussing an inline cbirds plugin:
“expose arbitrary jj repos sub-workspaces,” avoiding full materialization;
“filtering commits seeing only valid objects is the real heart of this work.”

Related consumer: [opencode-cbirds](../opencode-cbirds/README.md).
