# Running jj-workfuse as a systemd mount

`jj-workfuse mount` is designed for service managers: it stays in the
foreground, treats the FUSE session as established as soon as the mount
exists, reports `READY=1` over `sd_notify(3)` at that moment, and turns
`SIGTERM` (systemd's default stop signal) into a graceful unmount that exits
`0`. When readers still hold files open, the unmount lazy-detaches
(`MNT_DETACH`) instead of hanging, so normal stops never need `SIGKILL`.

Two ready-to-copy unit templates live next to this README:

- [`jj-workfuse@.service`](jj-workfuse@.service) — system instance template,
  one mount per instance name.
- [`jj-workfuse-user@.service`](jj-workfuse-user@.service) — the same for
  `systemctl --user`, no root required.

Both rely on the `JJ_WORKFUSE_*` environment variables the mount command
reads (CLI arguments still win when given):

| Variable | Meaning | Default |
| --- | --- | --- |
| `JJ_WORKFUSE_REPOSITORY` | jj workspace, path beneath it, `.jj`, or `.jj/repo` | required |
| `JJ_WORKFUSE_REVISION` | pinned single-commit revset | `@` |
| `JJ_WORKFUSE_SUBDIR` | source directory projected as the mount root | `.` |
| `JJ_WORKFUSE_WORKSPACE` | workspace identity for `@` | — |
| `JJ_WORKFUSE_MOUNTPOINT` | existing empty mount directory | required |
| `JJ_WORKFUSE_CACHE_MIB` | file-content cache budget in MiB | `64` |

## Quick test with a transient unit

```sh
mkdir -p ~/mnt/demo
systemd-run --user --unit=jjwf-demo -p Type=notify \
  -p Environment=JJ_WORKFUSE_REPOSITORY="$HOME/src/some-repo" \
  -p Environment=JJ_WORKFUSE_REVISION=main \
  -p Environment=JJ_WORKFUSE_SUBDIR=plugin \
  -p Environment=JJ_WORKFUSE_MOUNTPOINT="$HOME/mnt/demo" \
  /path/to/jj-workfuse mount
systemctl --user status jjwf-demo   # active (running), shows the pinned STATUS line
ls ~/mnt/demo
systemctl --user stop jjwf-demo     # unmounts, exits 0
```

The unit reports `activating` until the projection resolves *and* the kernel
mount exists; only then does `READY=1` flip it to `active`. Units that need
the mounted tree can order themselves with `After=`/`Requires=` on this
service.

## Installing a persistent mount (system scope)

1. Place the binary where units can see it, e.g. `/usr/local/bin/jj-workfuse`.
2. `cp examples/systemd/jj-workfuse@.service /etc/systemd/system/`
3. Write `/etc/jj-workfuse/<name>.conf` (see the table above), for example
   `/etc/jj-workfuse/demo.conf`:

   ```ini
   JJ_WORKFUSE_REPOSITORY=/home/rektide/src/some-repo
   JJ_WORKFUSE_REVISION=main
   JJ_WORKFUSE_SUBDIR=plugin
   JJ_WORKFUSE_MOUNTPOINT=/mnt/demo
   ```

4. `mkdir -p /mnt/demo && systemctl daemon-reload`
5. `systemctl enable --now jj-workfuse@demo`

For user scope use `jj-workfuse-user@.service` in
`~/.config/systemd/user/`, drop-ins in `~/.config/jj-workfuse/<name>.conf`,
and `systemctl --user enable --now jj-workfuse-user@demo`.

## Notes that matter for FUSE under systemd

- **Keep the mount namespace shared.** The templates deliberately avoid
  `PrivateTmp=`, `PrivateMounts=`, `ProtectSystem=`, and friends: those
  options give the service its own mount namespace and the FUSE mount would
  be invisible to everything outside the unit. Add sandboxing only if you
  understand that trade-off.
- **`ExecStopPost` cleans up crashes.** If the process is `SIGKILL`ed (OOM,
  `systemctl kill`), the kernel keeps a disconnected mount. The templates'
  `ExecStopPost=-/usr/bin/fusermount3 -u $JJ_WORKFUSE_MOUNTPOINT` removes it;
  the leading `-` keeps the normal already-unmounted case successful.
- **Startup failures are real failures.** An unresolvable revision, a
  missing object, or a non-empty mountpoint exits non-zero before `READY=1`,
  so the unit lands in `failed`, not `active`. With `Restart=on-failure` the
  service retries; a stale mount from the crash is swept by `ExecStopPost`
  first (add `RestartSec=` headroom for busy mounts).
- **Order after the source storage.** If the repository lives on removable
  or network storage, add `RequiresMountsFor=/path/to/repo` (or a matching
  `After=`/`Requires=`) to the unit so starts wait for the source.
- **User units and `/dev/fuse`.** User-scope units need `fusermount3`
  installed and `/dev/fuse` accessible, exactly like a terminal mount; no
  root privilege is involved. System-scope units running as an unprivileged
  `User=` need the same, plus write access to the mountpoint directory.
- **fstab/automount units are not the mechanism here.** `mount`-driven units
  would need a `mount.fuse.jjworkfuse` helper that backgrounded the daemon,
  fighting the foreground service model above; use the service templates.
