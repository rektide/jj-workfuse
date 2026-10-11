# v1 projection and object contract

This is the implemented initial slice, not a synthetic jj repository or a claim
that source commits have been rewritten in-place.

## Native access and pinning

`jj-lib = 0.46.0` is pinned in `Cargo.toml` and `Cargo.lock`. The loader uses its
default Git and Simple backend factories. A workspace path, `.jj`, a path inside
a workspace, or the shared `.jj/repo` metadata directory can be supplied.
jj's workspace-loader factory resolves `.jj/repo` indirection for additional
workspaces. Existing local working-copy metadata is read only to discover the
workspace name; it is **not snapshotted**. A direct repository path needs no
working-copy state at all. Use a commit ID/bookmark, or explicitly select a
`--workspace` when using `@` there.

The loader obtains exactly one operation head and loads that operation explicitly.
It does not call `load_at_head()`: that jj API can reconcile concurrent operations
and write history. Concurrent operation heads are an error requiring explicit
reconciliation with jj. A revision must evaluate to exactly one commit. Built-in
revset syntax is available, but user/repository jj configuration and aliases are
not loaded; user-relative predicates consequently use jj-lib's default user.

After selection, the commit and operation are fixed even if bookmarks/workspaces
move. This is a committed object-store view, not a view of unsnapshotted source
files. No command performs a checkout, export, source working-copy mutation, or
per-read subprocess invocation. Native jj index rebuilds/caches and legacy Git
extra-metadata repair **may write repository metadata**; read-only describes the
filesystem interface and source history, not a promise that jj-lib never touches
a cache. The source store and its referenced objects must remain available;
mounts do not register a GC protection lease or fetch missing remote objects.

## Scope and lazy trees

Selection is a normalized, UTF-8 repository-relative path. `.` and the empty
string select the root. Absolute paths, `..`, redundant separators, `.` inside
paths, backslashes and NUL are rejected. API traversal does not follow symlinks.

To find a subtree, the core reads only directory objects along the selection
boundary, using jj's merged-tree value semantics. Enumeration reads only the
requested directory's tree terms. No recursive whole-tree export or hidden
checkout is made. The FUSE adapter assigns stable, per-mount sequential inodes
as directories are discovered; its root's `..` is its root. Inodes are never
reused within a mount, but are not durable identities across mounts.

Root states are represented rather than silently dropped:

| State | Meaning | Mountable? |
| --- | --- | --- |
| `directory` | Resolved tree, or a merge of trees/absences | Yes |
| `absent` | Selected path does not exist | No |
| `file`, `symlink`, `git_submodule` | A resolved non-directory at the selected path | No |
| `conflict` | An unresolved non-directory/type conflict | No |
| `blocked` | A non-directory/type conflict at an ancestor obstructs a nested selection | No |

Blocked roots record `blocked_at` without exposing that outside ancestor's blob
or tree. Historical absence and a resolved empty tree are distinct states.

The mount exposes file bytes, executable bits, directories and recorded symlink
targets. Modes are 0444/0555 for regular files, 0555 for directories; timestamps
are deliberately fixed at the Unix epoch because source trees contain no per-file
timestamps. Symlink targets are unchanged and **can refer outside the mount**.
This is not a filesystem security sandbox. Read-only loading can execute source
code: use only trusted plugins or isolate the consumer separately.

## Reachability is not availability or filesystem visibility

`objects` / `Projection::objects()` enumerate **typed references** reachable from
the selected root's merge terms, recursively, including positive and negative
conflict evidence. Reading all trees for an explicit recursive inventory is
intentional; normal mounting never requests this inventory. Blobs are not read
to list the inventory, and a listed blob can still be missing or corrupt.

The granted object kinds are `tree`, `file`, and `symlink`. Commit, full source
root and boundary-ancestor tree objects are not granted merely because they are
available in the repository. A source object used both inside and outside the
scope is valid if it has an occurrence inside. Symlink targets, copy-history
provenance and Git submodule commit IDs never grant traversal into another scope
or repository. Their references are not recursively followed.

`object --kind ... --id ...` / `read_object()` first prove reachability in this
projection. A hex ID available elsewhere in the store is rejected. File/symlink
objects yield recorded bytes; tree objects yield JSON entries, **not raw Git or
jj backend serialization**. JSON tree references to submodule commits are opaque
boundary references, not a commit getter. Each guard currently walks tree
reachability again; there is no mutable, stale global allow-list.

Filesystem visibility differs from conflict-evidence reachability. All-tree
conflicts are traversed with jj's trivial merge rules. A file/file, delete/modify
or tree/file conflict stays a visible `conflict` entry. FUSE reports such entries
as regular-file placeholders of size zero, but `open` and `read` return `EIO`.
The core JSON retains every alternating `+ - + ...` term; explicit guarded object
reads can inspect each side. It never fabricates conflict markers or silently
chooses a side. A tree/file conflict's tree-side children are available as scoped
evidence, not as filesystem paths beneath that conflicted entry. Git submodules
are visible placeholders; opening them returns `EOPNOTSUPP`, never a fetch.

Missing selected tree objects fail their lookup/list; missing selected blobs fail
attribute/read operations with `EIO`. Missing unrelated objects do not force
materialization or invalidate a usable selected subtree. There is no remote
fetch, promisor integration, corruption repair, or stronger availability audit.

## Explicit history, identities and provenance

`history` / `Projection::history(max_commits)` projects the selected revision's
**entire ancestor DAG**, including jj's root. Output is deterministic,
parents-before-children, with ordered synthetic parent IDs. Every source vertex
is retained, including outside-only/no-subtree-change commits and merges. This
is deliberately not history simplification: no parent rewriting, flattening,
rename inference or discard of no-change commits is hidden in a path predicate.
`changed_from_parents` gives one projected-tree equality comparison per parent,
not a merge-base diff or a claim about the author's intention.

Cross-boundary moves appear as deletion/insertion at the selected path. Copy and
rename metadata is not a traversal grant and is not reconstructed. Commits before
the subtree exists retain `absent` roots; type changes and obstructed nested
selections retain their explicit states. Merges retain source parent order and
all selected conflict terms. Outside-only conflicts disappear when jj's subtree
value trivially resolves. Descriptions, authors, signatures and full source
commit payloads are not copied into the projection.

Synthetic IDs are **not jj commit IDs** and are never stored in the source repo:

```text
wf-tree-v1:<sha256>   = SHA256("jj-workfuse-v1/tree\0" || JSON([subdir, root_entry]))
wf-commit-v1:<sha256> = SHA256("jj-workfuse-v1/commit\0" ||
                             JSON([source_commit_id, projected_tree_id, projected_parent_ids]))
```

`JSON` here means compact `serde_json` encoding of the declared Rust structures,
in their declared field order, with optional fields omitted as in CLI output.
`root_entry.path` is the empty string; its terms preserve selected backend tree /
blob IDs and executable bits, not an original full-tree identity. Tree IDs are
backend object references: v1 does not canonicalize equivalent content across
backends or different conflict representations. The selected path is part of
scope identity. Source commit provenance intentionally distinguishes vertices
with the same projected tree. Operation/repository paths do not enter the
synthetic hashes, so re-opening the same source graph at a later operation does
not change its projected identities. This is a versioned local projection
protocol, not an interoperable Git/jj object format or signature scheme.

`inspect` records repository metadata location, source operation and commit,
selected path, projected tree ID and root state. Computing a projected commit
ID requires its parent graph; `history` does that explicitly. The guard fails
when ancestor count exceeds `--max-commits`; no partial graph is silently offered.
Historical object reads can select a vertex's `source_commit` with the same
`--subdir`. Creating a separate imported/synthetic jj repository is deferred.

## Caches and deferred semantics

The adapter's file-content LRU is byte-budgeted (`--cache-mib`, default 64).
Visited inode, directory and size metadata and jj-lib's own caches are outside
that budget. Tree listings do not stat every file. A kernel file lookup requests
attributes, and the current jj backend API has no generic cheap file-size query:
the first file attribute query scans **that file's** content. Sizes are cached.
Oversized files are not retained by the adapter and range reads stream/skip to
their offset. Native backends can still decompress/load an entire requested blob
internally; the byte budget is not a strict total-process-memory ceiling.

FUSE uses the host kernel's immutable file cache and a single request-dispatch
loop. Full writable workspaces, overlays, jj transactions/write-back, moving
bookmark follow mode, history simplification, remote fetching, submodule mounts,
conflict-marker materialization, xattrs, durable inodes and a synthetic repository
format are not implemented. All mount options are read-only, nodev, nosuid and
kernel permission checks; allow-other is not enabled. The CLI runs in the
foreground and unmounts on SIGINT/SIGTERM or external `fusermount3 -u`. SIGKILL
can leave a disconnected mount requiring explicit unmount.
