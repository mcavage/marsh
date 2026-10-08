"""Bounded owner-controlled build input reads and normalized private copies.

Shared by image and native Kit preparation; stdlib only, no Docker effects.
"""
from contextlib import contextmanager
import hashlib
import json
import os
from pathlib import Path
import stat

JSON_LIMIT = 4 * 1024 * 1024
FILE_LIMIT = 128 * 1024 * 1024
STAGE_LIMIT = 512 * 1024 * 1024
MAX_FILES = 10000
DIRECTORY_FLAGS = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC
FILE_FLAGS = os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC


def parts(value):
    if (not isinstance(value, str) or not value or len(value) > 4096
            or value.startswith("/") or "\\" in value or "\0" in value
            or any(part in ("", ".", "..") for part in value.split("/"))):
        raise ValueError("expected a nonempty relative file path without traversal")
    return value.split("/")


@contextmanager
def directory(path, *, create=False):
    """Retain each real directory while walking; never follow a parent symlink."""
    if ".." in Path(path).parts:
        raise ValueError("directory paths must not contain '..'; use a canonical path")
    path = Path(os.path.abspath(path))
    fd = os.open(path.anchor, DIRECTORY_FLAGS)
    try:
        for component in path.parts[1:]:
            try:
                child = os.open(component, DIRECTORY_FLAGS, dir_fd=fd)
            except FileNotFoundError:
                if not create:
                    raise
                try:
                    os.mkdir(component, 0o700, dir_fd=fd)
                except FileExistsError:
                    pass
                child = os.open(component, DIRECTORY_FLAGS, dir_fd=fd)
            info = os.fstat(child)
            # Root-owned system ancestors (including sticky /tmp) are trusted.
            if (info.st_uid not in (0, os.getuid()) or
                    (info.st_mode & 0o022 and not (info.st_uid == 0 and info.st_mode & stat.S_ISVTX))):
                os.close(child)
                raise ValueError(f"directory is not owner-controlled: {path}")
            os.close(fd)
            fd = child
        yield fd
    finally:
        os.close(fd)


def fingerprint(metadata):
    return (metadata.st_dev, metadata.st_ino, metadata.st_mode, metadata.st_uid,
            metadata.st_size, metadata.st_mtime_ns, metadata.st_ctime_ns, metadata.st_nlink)


@contextmanager
def regular_file(path, limit):
    """NONBLOCK makes FIFO/device refusals prompt. Hardlinks are not inputs."""
    if ".." in Path(path).parts:
        raise ValueError("input paths must not contain '..'; use a canonical path")
    path = Path(os.path.abspath(path))
    with directory(path.parent) as parent:
        fd = os.open(path.name, FILE_FLAGS, dir_fd=parent)
        with os.fdopen(fd, "rb") as stream:
            before = os.fstat(stream.fileno())
            if (not stat.S_ISREG(before.st_mode) or before.st_uid != os.getuid()
                    or before.st_mode & 0o022 or before.st_nlink != 1 or before.st_size > limit):
                raise ValueError(f"expected owner-controlled single-link regular file (limit {limit} bytes): {path}")
            yield stream, before
            if fingerprint(before) != fingerprint(os.fstat(stream.fileno())):
                raise ValueError(f"input changed while reading: {path}")
            current = os.stat(path.name, dir_fd=parent, follow_symlinks=False)
            if fingerprint(before) != fingerprint(current):
                raise ValueError(f"input replaced while reading: {path}")


def read_file(path, limit=JSON_LIMIT):
    with regular_file(path, limit) as (stream, before):
        data = stream.read(limit + 1)
        if len(data) > limit or len(data) != before.st_size:
            raise ValueError(f"input size changed or exceeded limit: {path}")
        return data


def copy_regular(source, destination, budget):
    with regular_file(source, FILE_LIMIT) as (stream, before):
        if before.st_size > budget[0]:
            raise ValueError("combined Kit staging exceeds 512 MiB")
        destination.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
        with destination.open("xb") as output:
            remaining = before.st_size
            while remaining:
                block = stream.read(min(remaining, 1024 * 1024))
                if not block:
                    raise ValueError("staged input was truncated")
                output.write(block)
                remaining -= len(block)
            if stream.read(1):
                raise ValueError("staged input grew during copy")
            os.fchmod(output.fileno(), 0o644 | (stat.S_IMODE(before.st_mode) & 0o111))
        budget[0] -= before.st_size


def json_bytes(value):
    return (json.dumps(value, sort_keys=True, indent=2) + "\n").encode()


def sha(data):
    return "sha256:" + hashlib.sha256(data).hexdigest()


def file_record(path, *, budget=None):
    with regular_file(path, FILE_LIMIT) as (stream, info):
        if budget is not None:
            if info.st_size > budget[0]:
                raise ValueError("combined input tree exceeds 512 MiB byte limit")
            budget[0] -= info.st_size
        digest = hashlib.sha256()
        size = 0
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            size += len(chunk)
            if size > info.st_size:
                raise ValueError(f"input grew during hashing: {path}")
            digest.update(chunk)
        if size != info.st_size:
            raise ValueError(f"input truncated during hashing: {path}")
        return {"sha256": "sha256:" + digest.hexdigest(), "size": size,
                "mode": stat.S_IMODE(info.st_mode)}


def tree_inventory(root):
    """No-follow bounded scan, including empty directory membership.

    Returns {files: {relative: {sha256, size, mode}}, directories: [relative]}.
    Every directory and file must be caller-owned, no group/world writes.
    """
    root = Path(os.path.abspath(root))
    files, directories = {}, []
    budget, count = STAGE_LIMIT, 0

    def visit(path, depth=0):
        nonlocal budget, count
        if depth > 64:
            raise ValueError("input tree directory depth exceeds 64")
        with directory(path) as fd:
            before = os.fstat(fd)
            if before.st_uid != os.getuid() or before.st_mode & 0o022:
                raise ValueError(f"input directory must be caller-owned without other writers: {path}")
            names = []
            with os.scandir(fd) as entries:
                for entry in entries:
                    count += 1
                    if count > MAX_FILES:
                        raise ValueError("input tree file/directory-count limit exceeded")
                    names.append(entry.name)
            for name in sorted(names):
                child = path / name
                relative = str(child.relative_to(root))
                parts(relative)
                info = os.stat(name, dir_fd=fd, follow_symlinks=False)
                if stat.S_ISDIR(info.st_mode):
                    directories.append(relative)
                    visit(child, depth + 1)
                elif stat.S_ISREG(info.st_mode):
                    if info.st_size > budget:
                        raise ValueError("input tree exceeds 512 MiB byte limit")
                    record = file_record(child)
                    budget -= record["size"]
                    files[relative] = record
                else:
                    raise ValueError(f"canonical build input must be regular: {child}")
            if fingerprint(before) != fingerprint(os.fstat(fd)):
                raise ValueError(f"input tree changed during scan: {path}")
            with directory(path) as current:
                if fingerprint(before) != fingerprint(os.fstat(current)):
                    raise ValueError(f"input directory replaced during scan: {path}")
    visit(root)
    return {"files": files, "directories": sorted(directories)}
