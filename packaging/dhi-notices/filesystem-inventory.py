#!/usr/bin/env python3
"""Hash the static filesystem of a pinned public image; execute no payload code.

Run as root in a disposable container with --network none and no user mounts.
The explicitly excluded paths are kernel pseudo-filesystems and Docker-injected
container identity/config files, not image payload. Every other entry is recorded.
"""
import hashlib
import json
import os
import stat

EXCLUDED = {"/proc", "/sys", "/dev", "/etc/hostname", "/etc/hosts", "/etc/resolv.conf", "/.dockerenv"}
MAX_ENTRIES = 250000
MAX_BYTES = 40 * 1024**3


def main():
    rows = []
    byte_count = 0
    hashes = {}
    pending = ["/"]
    while pending:
        path = pending.pop()
        if path in EXCLUDED:
            continue
        metadata = os.lstat(path)
        row = {"path": path, "mode": stat.S_IMODE(metadata.st_mode), "uid": metadata.st_uid, "gid": metadata.st_gid}
        if stat.S_ISDIR(metadata.st_mode):
            row["type"] = "directory"
            with os.scandir(path) as entries:
                pending.extend(sorted((entry.path for entry in entries), reverse=True))
        elif stat.S_ISREG(metadata.st_mode):
            byte_count += metadata.st_size
            if byte_count > MAX_BYTES:
                raise ValueError("static image byte budget exceeded")
            row.update(type="file", size=metadata.st_size, links=metadata.st_nlink)
            key = (metadata.st_dev, metadata.st_ino, metadata.st_size)
            if key not in hashes:
                digest = hashlib.sha256()
                descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC)
                with os.fdopen(descriptor, "rb") as source:
                    actual = os.fstat(source.fileno())
                    if (actual.st_dev, actual.st_ino, actual.st_size) != key:
                        raise ValueError("static image entry changed while opening")
                    count = 0
                    while chunk := source.read(1024 * 1024):
                        count += len(chunk)
                        if count > metadata.st_size:
                            raise ValueError("static image file grew")
                        digest.update(chunk)
                    if count != metadata.st_size:
                        raise ValueError("static image file changed size")
                hashes[key] = digest.hexdigest()
            row["sha256"] = hashes[key]
        elif stat.S_ISLNK(metadata.st_mode):
            row.update(type="symlink", target=os.readlink(path))
        else:
            row.update(type="special", device=metadata.st_rdev)
        rows.append(row)
        if len(rows) > MAX_ENTRIES:
            raise ValueError("static image entry budget exceeded")
    print(json.dumps({"schema": "marsh.public-image-filesystem/v1", "excluded_runtime_paths": sorted(EXCLUDED),
        "entries": sorted(rows, key=lambda row: row["path"]), "regular_file_bytes": byte_count,
        "scope": "Complete static pinned public-image entry/hash inventory outside explicitly recorded runtime paths; no package or agent execution."}, sort_keys=True))


if __name__ == "__main__":
    main()
