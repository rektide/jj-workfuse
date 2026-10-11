# Initial build verification

Recorded 2026-10-11 in OpenCode session
`ses_ed39d2cfcffeuUR5Hvd5egcZQJ` (proud-canyon), building for parent session
`ses_ed3c601a6ffeWv3Kh6sSEj3q4W` (silent-meadow).

## Expected help and actual result

The expected help was an ordinary filesystem path to a selected jj subtree,
without needing the selected revision's materialized working tree, backed by a
separately testable model of valid projected revisions and object visibility.

The implemented result meets that initial read-only use: a real Node consumer
loaded an entrypoint and its nested helper through FUSE from an unmaterialized
revision. No source operation or working-tree files were changed by projection
or mounting. The core exposes a topology-preserving projected ancestor graph
and rejects available-but-out-of-scope object IDs. This does **not** establish
writable workspace semantics, an interoperable synthetic repository, performance
at production repository scales, or history simplification.

## Environment

- Linux; `/dev/fuse` exists and was successfully opened.
- `fusermount3` installed; actual mounting is permitted here.
- Rust `1.101.0-nightly (5c543b0b8 2026-09-29)`.
- Native dependency `jj-lib = 0.46.0`; lockfile committed.
- Smoke fixture CLI: jj `0.45.1-33d3a595429b7e3b54a8ea769560247913bc8a1b`.
- Node installed and used as the mounted-filesystem consumer.
- Test/smoke fixture directories under `/home/rektide/tmp-opencode` for the final
  verification runs; all are independently owned, ephemeral fixtures.

## Checks run successfully

```sh
TMPDIR=/home/rektide/tmp-opencode cargo test --locked
cargo check --locked
cargo clippy --locked --all-targets -- -D warnings
cargo fmt --check
cargo build --release --locked
python3 examples/smoke.py --fuse --binary target/release/jj-workfuse
```

**15 Rust integration tests passed**, with no ignored tests. They create genuine
jj repositories and write object-store revisions through native APIs, without
checking out the selected trees:

- Standard Simple and internal Git object stores; external bare Git storage.
- No materialized selected subtree or unrelated content required.
- Direct `.jj/repo` access with working-copy metadata absent.
- Linked metadata paths and a real second jj workspace, including its `@`.
- Pinned source operations/revisions despite subsequent workspace movement.
- Selected subtree tree/file/symlink grants; available outside IDs rejected.
- Unrelated missing tree deliberately makes a whole-tree walk fail, while the
  selected subtree's reads, inventory and history continue to work.
- Missing selected blobs distinguish reference reachability from availability;
  listing remains possible, attributes/reads fail with `EIO`.
- Ancestor root, unchanged outside-only vertices, deterministic synthetic IDs,
  explicit ancestor-count guard, ordered parent relationships and merge topology.
- Cross-boundary move represented as deletion; absent/type-changed/blocked roots.
- File/file, delete/modify and tree/file conflicts; explicit term preservation
  and guarded side-object reads; outside-only conflicts resolve out of scope.
- Opaque submodules never grant commit access or fetch another repository.
- Lazy directory discovery without blob reads; executable permissions, Unicode
  names, binary contents, symlink targets, offsets, EOF, invalid offsets, read-only
  opens, bounded content-cache eviction and oversized uncached range reads.
- Concurrent operation heads rejected without reconciliation or head mutation.
- Absolute/traversing/ambiguous paths rejected.

## Actual FUSE smoke, separately verified

`examples/smoke.py` uses jj only for fixture setup. It creates a plugin subtree
and an unrelated large payload, commits them, then moves the source workspace
to jj's root so neither subtree remains materialized. The Rust tests above also
cover fixtures whose selected trees were never materialized in the first place.

The release binary's smoke check passed all of these:

1. CLI provenance, listing, file bytes, guarded object bytes, object inventory,
   out-of-scope object rejection and synthetic history.
2. A real read-only FUSE mount exposing only the plugin subtree.
3. Nested Node `require()` returning `42` through the mounted entrypoint/helper.
4. Directory listing, file reads, recorded symlink target and followed symlink.
5. Execution of a recorded executable shell script through the mount.
6. Creating a file denied with `EROFS`.
7. SIGTERM exited successfully and unmounted cleanly.
8. Source jj operation unchanged; plugin and unrelated source directories stayed
   unmaterialized after all reads and unmounting.

No mount blocker was encountered. No live OpenCode configuration, consumer
repository or external ticket store was changed.

## Remaining limitations

See [the projection contract](../design/projection.md) for the exact supported
semantics. In particular, retained no-change history is intentional; IDs are
projection-protocol IDs rather than jj commits. Read-only source history does
not imply jj never rebuilds native metadata/index caches. Symlinks are not
sandboxed, missing objects are not fetched, mounts do not protect source objects
against GC, file-size attributes scan the requested file, and the content-cache
budget excludes native/backend/kernel and visited-metadata caches. Writable
overlays, write-back, follow mode and synthetic repository import are deferred.
