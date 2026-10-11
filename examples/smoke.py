#!/usr/bin/env python3
"""Build a real jj fixture, remove its checkout, and optionally mount/load it.

Fixture setup uses jj; jj-workfuse itself does not shell out for repository reads.
Run after `cargo build`: python3 examples/smoke.py [--fuse].
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import tempfile
import time


def run(*args, cwd=None, env=None):
    return subprocess.run(
        list(map(str, args)), cwd=cwd, env=env, check=True,
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=30,
    ).stdout


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fuse", action="store_true")
    parser.add_argument("--binary", type=Path, default=Path(__file__).resolve().parents[1] / "target/debug/jj-workfuse")
    args = parser.parse_args()
    binary = args.binary.resolve()
    preferred = Path.home() / "tmp-opencode"
    base = preferred if preferred.is_dir() else None
    with tempfile.TemporaryDirectory(prefix="jj-workfuse-smoke-", dir=base) as tmp:
        root = Path(tmp)
        repo, mount = root / "source", root / "mount"
        mount.mkdir()
        # Avoid mutating or relying on the developer's live jj configuration.
        config = root / "jj-config.toml"
        config.write_text('user.name = "Smoke"\nuser.email = "smoke@example.invalid"\n')
        env = {**os.environ, "JJ_CONFIG": str(config), "JJ_CONFIG_NO_DEFAULT": "1"}
        run("jj", "git", "init", repo, env=env)
        (repo / "plugin").mkdir()
        (repo / "plugin/nested").mkdir()
        entry = b'module.exports = { answer: require("./nested/helper.cjs") };\n'
        (repo / "plugin/index.cjs").write_bytes(entry)
        (repo / "plugin/nested/helper.cjs").write_text("module.exports = 42;\n")
        (repo / "plugin/run").write_text("#!/bin/sh\nprintf 'workfuse\\n'\n")
        (repo / "plugin/run").chmod(0o755)
        (repo / "plugin/link").symlink_to("index.cjs")
        (repo / "unrelated").mkdir()
        outside = b"unrelated\n" * 100_000
        (repo / "unrelated/payload").write_bytes(outside)
        run("jj", "commit", "-m", "unmaterialized plugin fixture", cwd=repo, env=env)
        revision = run("jj", "log", "--no-graph", "-r", "@-", "-T", "commit_id", cwd=repo, env=env).decode().strip()
        run("jj", "new", "root()", cwd=repo, env=env)
        assert not (repo / "plugin").exists()
        assert not (repo / "unrelated").exists()
        select = [repo, "--revision", revision, "--subdir", "plugin"]
        before = run("jj", "op", "log", "--ignore-working-copy", "--no-graph", "-n", "1", "-T", "id", cwd=repo, env=env)
        provenance = json.loads(run(binary, "inspect", *select))
        assert provenance["source_commit"] == revision
        assert provenance["root"]["kind"] == "directory"
        assert run(binary, "cat", *select, "index.cjs") == entry
        entries = json.loads(run(binary, "ls", *select))
        assert [e["path"] for e in entries] == ["index.cjs", "link", "nested", "run"]
        index_id = entries[0]["terms"][0]["id"]
        assert run(binary, "object", *select, "--kind", "file", "--id", index_id) == entry
        objects = json.loads(run(binary, "objects", *select))
        assert len(objects) == 6  # Two trees, three files, one symlink.
        outside_id = hashlib.sha1(f"blob {len(outside)}\0".encode() + outside).hexdigest()
        denied = subprocess.run([str(binary), "object", *map(str, select), "--kind", "file", "--id", outside_id], capture_output=True, timeout=30)
        assert denied.returncode != 0 and denied.stdout == b""
        assert b"not reachable" in denied.stderr
        history = json.loads(run(binary, "history", *select))
        assert history[-1]["source_commit"] == revision
        assert history[-1]["id"].startswith("wf-commit-v1:")
        print("PASS: CLI/native subtree reads and filtered objects from an unmaterialized jj revision")
        if args.fuse:
            if not shutil.which("fusermount3"):
                raise RuntimeError("FUSE blocker: fusermount3 is not installed")
            proc = subprocess.Popen([str(binary), "mount", *map(str, select), str(mount)], stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            try:
                deadline = time.monotonic() + 10
                while not os.path.ismount(mount):
                    if proc.poll() is not None:
                        _, stderr = proc.communicate()
                        raise RuntimeError(f"FUSE mount failed (exit {proc.returncode}):\n{stderr.decode()}")
                    if time.monotonic() >= deadline:
                        raise RuntimeError("FUSE mount did not become ready")
                    time.sleep(0.02)
                assert (mount / "index.cjs").read_bytes() == entry
                assert sorted(p.name for p in mount.iterdir()) == ["index.cjs", "link", "nested", "run"]
                assert (mount / "link").read_bytes() == (mount / "index.cjs").read_bytes()
                assert os.readlink(mount / "link") == "index.cjs"
                assert run(mount / "run") == b"workfuse\n"
                try:
                    (mount / "new-file").write_text("must fail")
                except OSError as error:
                    assert error.errno == 30, error  # EROFS
                else:
                    raise AssertionError("write unexpectedly succeeded")
                if shutil.which("node"):
                    assert run("node", "-e", f"console.log(require({json.dumps(str(mount / 'index.cjs'))}).answer)") == b"42\n"
                    print("PASS: real FUSE mount, executable, symlink, EROFS, and Node require()")
                else:
                    print("PASS: real FUSE mount; Node consumer unavailable")
                proc.send_signal(signal.SIGTERM)
                _, stderr = proc.communicate(timeout=10)
                assert proc.returncode == 0, stderr.decode()
                assert not os.path.ismount(mount), "SIGTERM did not unmount"
            finally:
                if os.path.ismount(mount):
                    subprocess.run(["fusermount3", "-u", str(mount)], check=False, timeout=10)
                if proc.poll() is None:
                    proc.terminate()
                    proc.wait(timeout=10)
        after = run("jj", "op", "log", "--ignore-working-copy", "--no-graph", "-n", "1", "-T", "id", cwd=repo, env=env)
        assert before == after, "source operation changed"
        assert not (repo / "plugin").exists()
        assert not (repo / "unrelated").exists()
        print("PASS: no source operation mutation or tree materialization")


if __name__ == "__main__":
    main()
