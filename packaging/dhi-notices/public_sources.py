"""Bounded public-source acquisition. No package code is executed or extracted.

Caller supplies version/commit-pinned HTTPS URLs and an owner-private, fresh cache.
TLS acquisition and checksums are not signature verification or a legal grant.
"""
import base64
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import stat
import re
import tempfile
import tarfile
import urllib.parse
import urllib.request

MAX_ARCHIVE = 1024 * 1024 * 1024
MAX_MEMBER = 512 * 1024 * 1024
MAX_ENTRIES = 100000
MAX_EXPANDED = 3 * 1024**3


def identity(st):
    # Publishing an acquisition with link/unlink changes ctime (some shared
    # filesystems report it lazily). Content, identity, permissions and mtime
    # must remain stable; archive hashes also bind the actual downloaded bytes.
    return (st.st_dev, st.st_ino, st.st_mode, st.st_uid, st.st_gid,
            st.st_size, st.st_mtime_ns)


def read_input(path, limit=32 * 1024**2):
    """Bounded regular no-follow preparation input, with no symlink ancestors."""
    path = Path(path).absolute()
    for parent in path.parents:
        if not stat.S_ISDIR(parent.lstat().st_mode):
            raise ValueError("non-directory/symlink input ancestor")
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(fd, "rb") as source:
        before = os.fstat(source.fileno())
        if not stat.S_ISREG(before.st_mode) or before.st_size > limit:
            raise ValueError("bounded regular preparation input required")
        data = source.read(limit + 1)
        if len(data) != before.st_size or identity(os.fstat(source.fileno())) != identity(before):
            raise ValueError("preparation input changed")
    return data


def private_output(path):
    """Create a fresh result directory under an existing owner-private parent."""
    path = Path(path).absolute()
    for ancestor in path.parents:
        if not stat.S_ISDIR(ancestor.lstat().st_mode):
            raise ValueError("non-directory/symlink output ancestor")
    st = path.parent.lstat()
    if st.st_uid != os.getuid() or st.st_mode & 0o077:
        raise ValueError("result parent must be owner-private")
    path.mkdir(mode=0o700, exist_ok=False)
    return path


def digest(path, algorithm="sha256"):
    h = hashlib.new(algorithm)
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(fd, "rb") as f:
        before = os.fstat(f.fileno())
        if not stat.S_ISREG(before.st_mode) or before.st_size > MAX_ARCHIVE:
            raise ValueError("not a bounded regular source")
        count = 0
        while chunk := f.read(1024 * 1024):
            count += len(chunk)
            if count > before.st_size:
                raise ValueError("source grew")
            h.update(chunk)
        if count != before.st_size or identity(os.fstat(f.fileno())) != identity(before):
            raise ValueError("source changed")
    return h.hexdigest()


def acquire(url, directory, *, sha256=None, integrity=None, limit=MAX_ARCHIVE):
    parsed = urllib.parse.urlsplit(url)
    gitiles = parsed.hostname in {"chromium.googlesource.com", "v8.googlesource.com"}
    if (parsed.scheme != "https" or parsed.username or parsed.password or parsed.fragment
            or parsed.query and not (gitiles and parsed.query in {"format=TEXT", "format=JSON"}
                                     or parsed.hostname == "api.github.com" and parsed.query == "recursive=1")):
        raise ValueError("only public version/commit-pinned HTTPS source URLs")
    if "/archive/refs/tags/" in parsed.path and not sha256:
        raise ValueError("tag archive requires an independently pinned SHA256")
    patterns = {
        "raw.githubusercontent.com": r"/[^/]+/[^/]+/[0-9a-f]{40}/.+",
        "codeload.github.com": r"/[^/]+/[^/]+/tar.gz/[0-9a-f]{40}",
        "github.com": r"/[^/]+/[^/]+/(releases/download/[^/]+/.+|archive/refs/tags/[^/]+\.tar\.gz)",
        "chromium.googlesource.com": r"/.+/\+/[0-9a-f]{40}/.*|/.+/\+archive/[0-9a-f]{40}\.tar\.gz",
        "v8.googlesource.com": r"/.+/\+/[0-9a-f]{40}/.*|/.+/\+archive/[0-9a-f]{40}\.tar\.gz",
        "downloads.xiph.org": r"/releases/opus/opus-[0-9.]+\.tar\.gz",
        "download.gnome.org": r"/sources/glib/[0-9.]+/glib-[0-9.]+\.tar\.xz",
        "gstreamer.freedesktop.org": r"/src/[a-z-]+/[a-z-]+-[0-9.]+\.tar\.xz",
        "api.github.com": r"/repos/[^/]+/[^/]+/git/(tags|commits|trees)/[0-9a-f]{40}",
        "mirrors.edge.kernel.org": r"/pub/linux/libs/security/linux-privs/libcap2/libcap-[0-9.]+\.tar\.xz",
        "static.crates.io": r"/crates/[A-Za-z0-9_-]+/[A-Za-z0-9_.+-]+\.crate",
        "bcr.bazel.build": r"/modules/[A-Za-z0-9_-]+/[A-Za-z0-9_.+-]+/(source.json|MODULE.bazel)",
        "crates.io": r"/api/v1/crates/[A-Za-z0-9_-]+/[A-Za-z0-9_.+-]+",
        "static.rust-lang.org": r"/dist/(?:[0-9]{4}-[0-9]{2}-[0-9]{2}/)?(channel-rust-[0-9.]+\.toml(?:\.sha256)?|rustc-[0-9.]+-[A-Za-z0-9_-]+\.tar\.xz(?:\.sha256)?)",
        "musl.libc.org": r"/releases/musl-[0-9.]+\.tar\.gz",
        "registry.npmjs.org": r"/.+[/@-][0-9]+\.[0-9]+\.[0-9]+[^/]*",
        "downloads.claude.ai": r"/claude-code-releases/[0-9]+\.[0-9]+\.[0-9]+/.+",
    }
    if parsed.hostname not in patterns or not re.fullmatch(patterns[parsed.hostname], parsed.path):
        raise ValueError("unreviewed public source host or mutable source selector: " + url)
    directory = Path(directory).absolute()
    for parent in (directory, *directory.parents):
        if not stat.S_ISDIR(parent.lstat().st_mode):
            raise ValueError("symlink/non-directory source-cache ancestor")
    st = directory.lstat()
    if not stat.S_ISDIR(st.st_mode) or st.st_uid != os.getuid() or st.st_mode & 0o077:
        raise ValueError("acquisition directory must be owner-private")
    key = hashlib.sha256(url.encode()).hexdigest()
    path = directory / key
    if not path.exists():
        request = urllib.request.Request(url, headers={"User-Agent": "marsh-public-notice-collector"})
        descriptor, temporary = tempfile.mkstemp(prefix=".acquire-", dir=directory)
        try:
            with os.fdopen(descriptor, "wb") as out, urllib.request.urlopen(request, timeout=90) as response:
                if urllib.parse.urlsplit(response.url).scheme != "https":
                    raise ValueError("non-HTTPS public source redirect")
                count = 0
                downloaded = hashlib.sha256()
                while chunk := response.read(1024 * 1024):
                    count += len(chunk)
                    if count > limit:
                        raise ValueError("public download bound exceeded")
                    out.write(chunk)
                    downloaded.update(chunk)
                out.flush()
                os.fsync(out.fileno())
            if digest(temporary) != downloaded.hexdigest():
                raise ValueError("downloaded bytes changed before publication")
            # Link publishes complete bytes exclusively, never overwrites a cache.
            try:
                os.link(temporary, path)
            except FileExistsError:
                if digest(path) != digest(temporary):
                    raise ValueError("concurrent public source mismatch")
        finally:
            os.unlink(temporary)
    actual = digest(path)
    if sha256 and actual != sha256:
        raise ValueError("public source SHA256 mismatch: " + url)
    if integrity:
        algorithm, expected = integrity.split("-", 1)
        if algorithm != "sha512" or base64.b64encode(bytes.fromhex(digest(path, "sha512"))).decode() != expected:
            raise ValueError("public source SHA512 mismatch: " + url)
    record = {"url": url, "sha256": actual, "size": path.stat().st_size,
              "integrity": integrity, "signature_verified": False}
    receipt = directory / (key + ".json")
    try:
        with receipt.open("x") as out:
            json.dump(record, out, indent=2)
            out.write("\n")
    except FileExistsError:
        # The returned record comes from this call's own hash verification;
        # retain the original acquisition record rather than overwrite it.
        pass
    return path, record


def members(path, expected_sha256):
    """Verify compressed bytes before interpreting members; never extract paths."""
    if digest(path) != expected_sha256:
        raise ValueError("archive changed before parsing")
    total = 0
    seen = set()
    with tarfile.open(path, "r:*") as archive:
        for count, member in enumerate(archive, 1):
            p = PurePosixPath(member.name)
            if (count > MAX_ENTRIES or p.is_absolute() or ".." in p.parts
                    or "\\" in member.name or member.name in seen):
                raise ValueError("unsafe/duplicate archive member")
            seen.add(member.name)
            total += member.size
            if total > MAX_EXPANDED or member.size > MAX_MEMBER:
                raise ValueError("expanded archive bound exceeded")
            if member.isfile():
                data = archive.extractfile(member).read(member.size + 1)
                if len(data) != member.size:
                    raise ValueError("member length changed")
                yield member.name, data, member.mode
            elif not (member.isdir() or member.issym() or member.islnk()):
                raise ValueError("special archive member")
            # Source archive links are never followed or written.
