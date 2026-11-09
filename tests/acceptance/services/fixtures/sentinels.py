#!/usr/bin/env python3
"""Read stack A's sentinels and compare them with a fixture's sentinels.json.

    read_fixture(run, project, project_dir=None, manifest=...) -> dict
    compare(observed, manifest) -> list[str]        (empty = equal)
    checkpoint_manifest(manifest, observed, missing=(), volume_only=()) -> M(C)

RUN is `run(argv) -> (returncode, stdout)`: it executes a docker command on the
engine under observation (a `vx` form, or `sbx exec VM docker ...`). PROJECT is the
Compose project name (`<project>-db-1` etc.). PROJECT_DIR, when given, is the
project folder on this machine, for the ignored files' hashes and the sqlite
value. Reads are CONFINED: every path component under PROJECT_DIR is opened with
O_NOFOLLOW relative to its parent directory descriptor, only regular files are
read (never a FIFO, device or symlink), and an unsafe path or a race is an
error entry, never a read outside the fixture. Nothing here writes.

M(C) follows TEST-PLAN §C: `restorable` is the fixture's declared set and is
never reduced to fit a result; observed missing captures go in `missing`
(separate); expected data that could not be read raises, it is never skipped.
CLI, for a plain `docker` on PATH:   sentinels.py PROJECT [PROJECT_DIR]
"""
import hashlib
import json
import os
import pathlib
import re
import stat
import sys
import tempfile

HERE = pathlib.Path(__file__).resolve().parent
MANIFEST = HERE / "stack-a" / "sentinels.json"
SQLITE_LIMIT = 64 * 1024 * 1024


class UnsafePath(Exception):
    """A path under the fixture is a link, not a regular file, or changed under us."""


def open_confined(root: str, relative: str) -> int:
    """A read-only descriptor of the regular file ROOT/RELATIVE, opened without
    following any link below ROOT (ROOT itself is the trusted anchor)."""
    parts = pathlib.PurePosixPath(relative).parts
    if not parts or pathlib.PurePosixPath(relative).is_absolute() or ".." in parts or "." in parts:
        raise UnsafePath(f"unsafe relative path {relative!r}")
    directory = os.open(os.path.realpath(root), os.O_RDONLY | os.O_DIRECTORY)
    try:
        for part in parts[:-1]:
            try:
                child = os.open(part, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=directory)
            except OSError as error:
                raise UnsafePath(f"{relative}: {part!r} is not a plain directory ({error.strerror})") from None
            os.close(directory)
            directory = child
        try:
            fd = os.open(parts[-1], os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=directory)
        except OSError as error:
            raise UnsafePath(f"{relative}: cannot open without following links ({error.strerror})") from None
        if not stat.S_ISREG(os.fstat(fd).st_mode):
            os.close(fd)
            raise UnsafePath(f"{relative}: not a regular file")
        return fd
    finally:
        os.close(directory)


def confined_sha256(root: str, relative: str) -> str:
    fd = open_confined(root, relative)
    digest = hashlib.sha256()
    with os.fdopen(fd, "rb") as handle:
        for block in iter(lambda: handle.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def confined_sqlite(root: str, relative: str, query: str) -> list:
    import sqlite3
    fd = open_confined(root, relative)
    with os.fdopen(fd, "rb") as handle:
        data = handle.read(SQLITE_LIMIT + 1)
    if len(data) > SQLITE_LIMIT:
        raise UnsafePath(f"{relative}: larger than {SQLITE_LIMIT} bytes")
    with tempfile.TemporaryDirectory(prefix="nfx-sqlite-") as scratch:
        copy = pathlib.Path(scratch) / "copy.sqlite"
        copy.write_bytes(data)
        db = sqlite3.connect(f"file:{copy}?mode=ro", uri=True)
        try:
            return [row[0] for row in db.execute(query)]
        finally:
            db.close()


def _fresh(run, project: str, name: str, spec: dict):
    if spec["type"] == "redis-key":
        rc, out = run(["docker", "exec", f"{project}-{name}-1", "redis-cli", "get", spec["key"]])
        return out.strip() if rc == 0 else {"error": f"rc={rc}"}
    if spec["type"] == "mail-count":
        rc, out = run(["docker", "exec", f"{project}-db-1", "bash", "-c",
                       "exec 3<>/dev/tcp/mail/8025; printf 'GET /api/v1/messages HTTP/1.0\\r\\nHost: mail\\r\\n\\r\\n' >&3; cat <&3"])
        parts = re.split(r"\r?\n\r?\n", out, maxsplit=1)
        try:
            return json.loads(parts[1])["total"] if rc == 0 and len(parts) == 2 else {"error": f"rc={rc}"}
        except (ValueError, KeyError):
            return {"error": "mailpit reply is not JSON"}
    if spec["type"] == "container-file":
        rc, out = run(["docker", "exec", f"{project}-{name}-1", "sh", "-c", f"test -f {spec['file']} && cat {spec['file']}"])
        return out.strip() if rc == 0 else (None if rc == 1 else {"error": f"rc={rc}"})
    if spec["type"] == "pg-table-present":
        rc, out = run(["docker", "exec", f"{project}-{name}-1", "psql", "-U", spec["user"], "-d", spec["database"], "-Atc",
                       f"select to_regclass('{spec['table']}') is not null"])
        return (out.strip() == "t") if rc == 0 else {"error": f"rc={rc}"}
    raise ValueError(f"unknown sentinel type {spec['type']!r}")


def _flag(count):
    """True/False for a column count; a failed query stays an error (not a False observation)."""
    return count if isinstance(count, dict) else count == "1"


def read_fixture(run, project: str, project_dir=None, manifest: dict | None = None) -> dict:
    manifest = manifest or json.loads(MANIFEST.read_text())
    db = manifest["db"]

    def psql(sql: str):
        rc, out = run(["docker", "exec", f"{project}-db-1", "psql", "-U", db["user"], "-d", db["database"], "-Atc", sql])
        return out.strip() if rc == 0 else {"error": f"rc={rc}"}

    def app(script: str):
        rc, out = run(["docker", "exec", f"{project}-app-1", "sh", "-c", script])
        return out.strip() if rc == 0 else {"error": f"rc={rc}"}

    markers = psql("select name from markers order by name")
    tokens = app("ls /uploads | grep '^tok-' | sort || true")
    observed = {
        "db": {
            "items": psql("select count(*) from items"),
            "note_column": _flag(psql("select count(*) from information_schema.columns "
                                      "where table_name='items' and column_name='note'")),
            "row1": psql("select name from items where id = 1"),
            "markers": markers if isinstance(markers, dict) else [m for m in markers.split("\n") if m],
            "tokens": psql("select count(*) from tokens"),
        },
        "app": {
            "seed_bin_sha256": app("sha256sum /uploads/seed.bin | cut -d' ' -f1"),
            "seed_bin_bytes": app("wc -c < /uploads/seed.bin"),
            "token_files": tokens if isinstance(tokens, dict) else [t for t in tokens.split("\n") if t],
        },
        "fresh": {name: _fresh(run, project, name, svc["sentinel"])
                  for name, svc in manifest["services"].items() if svc["class"] == "fresh" and "sentinel" in svc},
    }
    if project_dir is not None:
        hashes, errors = {}, {}
        for name in manifest["ignored"]["files"]:
            try:
                hashes[name] = confined_sha256(project_dir, name)
            except (UnsafePath, OSError) as error:
                hashes[name], errors[name] = None, str(error)
        observed["ignored"] = {"sha256": hashes, "errors": errors}
        spec = manifest["ignored"]["sqlite"]
        try:
            observed["ignored"]["sqlite"] = confined_sqlite(project_dir, spec["path"], spec["query"])
        except Exception as error:  # UnsafePath, sqlite3.Error, OSError: all unreadable
            observed["ignored"]["sqlite"] = None
            errors[spec["path"]] = str(error)
    return observed


read_stack_a = read_fixture  # the name the first version had


def _errors(node, path=""):
    if isinstance(node, dict):
        if "error" in node and len(node) == 1:
            yield f"{path}: unreadable ({node['error']})"
        else:
            for key, value in node.items():
                yield from _errors(value, f"{path}.{key}" if path else key)


def incomplete(manifest: dict, observed: dict) -> list[str]:
    """Everything the fixture DECLARES that OBSERVED lacks or could not read. A legitimate
    observation of absence (note_column False, a file that is not there: None where the
    sentinel type allows it, an empty token set) is not a problem; a missing key, an error
    entry, a missing ignored hash or an unread ignored section is."""
    problems = list(_errors(observed))

    def need(group, key, kinds=(str,), none_ok=False):
        node = observed.get(group, {})
        if key not in node:
            problems.append(f"{group}.{key}: not read")
        elif node[key] is None and not none_ok:
            problems.append(f"{group}.{key}: no value")
        elif node[key] is not None and not isinstance(node[key], kinds) and not (isinstance(node[key], dict) and "error" in node[key]):
            problems.append(f"{group}.{key}: unexpected {type(node[key]).__name__}")

    for key in ("items", "row1", "tokens"):
        need("db", key)
    need("db", "note_column", (bool,))
    need("db", "markers", (list,))
    for key in ("seed_bin_sha256", "seed_bin_bytes"):
        need("app", key)
    need("app", "token_files", (list,))
    for name, svc in manifest["services"].items():
        if svc["class"] == "fresh" and "sentinel" in svc:
            kind = svc["sentinel"]["type"]
            node = observed.get("fresh", {})
            if name not in node:
                problems.append(f"fresh.{name}: not read")
            elif node[name] is None and kind != "container-file":
                problems.append(f"fresh.{name}: no value")
    ignored = manifest["ignored"]
    seen = observed.get("ignored")
    if seen is None:
        problems.append("ignored: not read (no project directory)")
    else:
        for name in ignored["files"]:
            if name not in seen.get("sha256", {}):
                problems.append(f"ignored.{name}: not read")
            elif seen["sha256"][name] is None:
                problems.append(f"ignored.{name}: unreadable ({seen.get('errors', {}).get(name, 'no hash')})")
        if seen.get("sqlite") is None:
            problems.append(f"ignored.{ignored['sqlite']['path']}: unreadable ({seen.get('errors', {}).get(ignored['sqlite']['path'], 'not read')})")
    return problems


def checkpoint_manifest(manifest: dict, observed: dict, missing=(), volume_only=()) -> dict:
    """M(C) (TEST-PLAN delta 19) from the fixture's declared service classes and the
    harness's own pre-capture reads (OBSERVED). MISSING names declared services or
    volumes the harness saw absent from the snapshot (via vsnap()) or removed by its
    own fault; it is recorded separately and never shrinks `restorable`. Raises ValueError
    unless EVERY declared sentinel (database, app, each fresh service, every ignored file and
    the SQLite value) was read successfully: M is never built from partial data."""
    problems = incomplete(manifest, observed)
    if problems:
        raise ValueError("checkpoint manifest is incomplete: " + "; ".join(problems))
    services = manifest["services"]
    declared = set(services) | {v for s in services.values() for v in s.get("volumes", [])}
    unknown = sorted(set(missing) - declared)
    if unknown:
        raise ValueError(f"missing names are not declared by the fixture: {unknown}")
    return {
        "restorable": sorted(n for n, s in services.items() if s["class"] == "restorable"),
        "volume_only": sorted(volume_only),
        "missing": sorted(set(missing)),
        "fresh": sorted(n for n, s in services.items() if s["class"] == "fresh"),
        "unsupported": sorted(n for n, s in services.items() if s["class"] == "unsupported"),
        "sentinels": {"db": observed["db"], "app": observed["app"],
                      "ignored": observed.get("ignored", {}).get("sha256"),
                      "ignored_sqlite": observed.get("ignored", {}).get("sqlite"),
                      "fresh": observed["fresh"]},
    }


def compare(observed: dict, manifest: dict | None = None) -> list[str]:
    """Differences between OBSERVED (at rest) and the fixture's declared sentinels;
    unreadable or missing expected data is a difference."""
    manifest = manifest or json.loads(MANIFEST.read_text())
    problems = incomplete(manifest, observed)

    def check(label, got, want):
        if got != want:
            problems.append(f"{label}: observed {got!r}, manifest {want!r}")

    want, got = manifest["db"]["expected"], observed.get("db", {})
    check("db.items", got.get("items"), str(want["items"]))
    check("db.note_column", got.get("note_column"), want["note_column"])
    check("db.markers", got.get("markers"), want["markers"])
    check("db.tokens", got.get("tokens"), str(want["tokens"]))
    check("db.row1", got.get("row1"), want["row1"])
    check("app.seed_bin_sha256", observed.get("app", {}).get("seed_bin_sha256"), manifest["app"]["seed_bin_sha256"])
    check("app.seed_bin_bytes", observed.get("app", {}).get("seed_bin_bytes"), str(manifest["app"]["seed_bin_bytes"]))
    check("app.token_files", observed.get("app", {}).get("token_files"), manifest["app"]["token_files"])
    for name, svc in manifest["services"].items():
        if svc["class"] == "fresh" and "sentinel" in svc:
            if name in observed.get("fresh", {}):
                check(f"fresh.{name}", observed["fresh"][name], svc["sentinel"]["at_rest"])
    ignored = manifest["ignored"]
    if "ignored" in observed:
        seen = observed["ignored"]
        for name, digest in ignored["static_sha256"].items():
            check(f"ignored.{name}", seen.get("sha256", {}).get(name), digest)
        check("ignored.sqlite", seen.get("sqlite"), ignored["sqlite"]["expected"])
    return problems


if __name__ == "__main__":
    import subprocess

    def local(argv):
        done = subprocess.run(argv, capture_output=True, text=True)
        return done.returncode, done.stdout

    seen = read_fixture(local, sys.argv[1], sys.argv[2] if len(sys.argv) > 2 else None)
    print(json.dumps(seen, indent=1))
    found = compare(seen)
    print("\n".join(found) or "sentinels match the manifest")
    sys.exit(1 if found else 0)
