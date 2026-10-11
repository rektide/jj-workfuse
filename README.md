# jj-workfuse

**Aspirational design — not yet an implemented or verified interface.**

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

## Desired experience

Conceptually:

```text
jj-workfuse mount <repository> --revision <revset> --subdir plugin <mountpoint>
```

This is illustrative syntax, not a promised CLI contract.

The mount exposes `plugin/` as its root, loads directories and file blobs on
demand, and can be used by ordinary package loaders, editors, and build tools.
Reading one file should not export the entire commit, create a hidden full
checkout, or require the selected revision to have an existing jj workspace.

A pinned revision gives a stable view. A later follow mode may advance a named
selection explicitly, with coherent cache invalidation and reader semantics.
Do not silently turn a moving bookmark into a supposedly immutable mount.

## The heart: filtered revisions and valid objects

FUSE is the filesystem adapter. The central problem is defining a correct
sub-workspace view of jj's commits, trees, and reachable objects.

- Resolve a revision in the source repository, then project only its selected
  subtree. Keep source revision and subtree provenance recoverable.
- Enumerate and read only objects valid for that view. Distinguish repository
  object availability, subtree reachability, and filesystem visibility.
- A source commit ID identifies its complete original tree. Never claim that
  a filtered tree has that same identity. If projected commits/history are
  exposed, define their new identities and source mappings explicitly.
- Decide and document how commits with no selected-path changes, parent
  relationships, merges, renames across the subtree boundary, and conflicts
  appear in filtered history. A path filter alone does not answer these.
- Do not require a full materialized working tree to resolve or traverse the
  selected immutable tree. Establish what repository metadata and object-store
  access jj-lib actually requires, including repositories with multiple
  workspaces and Git-backed storage.
- Filesystem traversal must stay inside the selected view. Symlinks retain
  their recorded meaning; document their ability to reference paths outside
  the mount rather than pretending a subtree mount is a filesystem sandbox.

The first build should implement a useful, coherent slice of this model and
state exactly which projection/history operations it supports. A directory
export wrapped in FUSE is not the intended endpoint.

## Build direction

1. Investigate jj's native Rust APIs and object model. Prefer direct object
   access through a version-pinned jj-lib over parsing human-readable CLI
   output or shelling out on every filesystem read.
2. Build the subtree/revision projection independently of FUSE, with meaningful
   fixtures covering nested trees, revision changes, merges/conflicts, and
   filtered object visibility.
3. Add a read-only lazy FUSE mount: lookup, directory enumeration, attributes,
   reads, executable bits, symlinks, and clear unsupported-operation errors.
   Read-only is the first usable milestone, not the full meaning of a writable
   jj workspace.
4. Exercise it with a real consumer loading a subdirectory from an unmaterialized
   revision, and prove that unrelated tree contents were not materialized.
5. Explore writable overlays and jj transaction/write-back semantics after the
   immutable view works. Writes must not accidentally mutate source workspaces
   or present filtered history as the original repository history.

Actual FUSE mounting may require `/dev/fuse` and host permissions. Test the
projection and filesystem operation layer without a mount as well, and report
mount verification separately from unit/integration verification.

## Open decisions

- Is filtered history exposed through a dedicated API/CLI, a synthetic jj
  repository, or only provenance initially? What do consumers need to see?
- How are jj conflicts represented as filesystem entries without silently
  selecting one side?
- How should sparse/missing objects and remote fetching behave?
- What cache budget and inode identity scheme preserve revision consistency?
- How can a later write-back view define commits that affect only its subtree
  while preserving changes elsewhere in the source repository?
- Can mounts outlive their source working-copy directory while retaining access
  to the shared repository store?

## Origin

Requested by rektide in OpenCode session `ses_ed3c601a6ffeWv3Kh6sSEj3q4W`
(silent-meadow), 2026-10-11, while discussing an inline cbirds plugin:
“expose arbitrary jj repos sub-workspaces,” avoiding full materialization;
“filtering commits seeing only valid objects is the real heart of this work.”

Related consumer: [opencode-cbirds](../opencode-cbirds/README.md).
