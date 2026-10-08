"""Read-only local SDK image observation using source-defined Unix discovery.

Derivation: sandboxlib/{constants_unix,socket_path}.go, storagepaths and CLI
NewRootCmd (snapshot hashes retained in the audit SDK manifest). No SDK symlink
creation/reset and no remote HTTP/Cloud fallback. This is not SDK binary attestation.
"""
from __future__ import annotations
import http.client
import json
import os
from pathlib import Path
import posixpath
import re
import socket
import sys
import threading
import time
from urllib.parse import quote

MAX_BODY = 1024 * 1024
SDK_ENV = ("DOCKER_SANDBOXES_API", "DOCKER_SANDBOXES_APP_NAME", "SANDBOXES_STORAGE_ROOT", "XDG_STATE_HOME")
# Go strings.TrimSpace uses unicode.IsSpace, not Python's additional U+001C..1F.
GO_SPACE = "\t\n\v\f\r \u0085\u00a0\u1680\u2000\u2001\u2002\u2003\u2004\u2005\u2006\u2007\u2008\u2009\u200a\u2028\u2029\u202f\u205f\u3000"


def storage_override(environment) -> str:
    """Exact v0.25 OverridesFromBase trimming; relative LOCAL roots are denied."""
    value = environment.get("SANDBOXES_STORAGE_ROOT", "").strip(GO_SPACE)
    if value and not Path(value).is_absolute():
        raise ValueError("LOCAL SDK storage override must be absolute")
    return value


def _join(*parts) -> Path:
    # Go filepath.Join/Clean on Unix is lexical (not symlink resolution) and
    # collapses repeated leading separators, unlike POSIX normpath's // case.
    value = posixpath.normpath(posixpath.join(*(str(part) for part in parts)))
    return Path("/" + value.lstrip("/") if value.startswith("/") else value)


def endpoint(environment=None, *, platform=None) -> str:
    env = os.environ if environment is None else environment
    platform = sys.platform if platform is None else platform
    if platform not in ("darwin", "linux"):
        raise ValueError("LOCAL shell authority supports Darwin/Linux Unix SDK endpoints only")
    suffix = env.get("DOCKER_SANDBOXES_APP_NAME", "")
    if suffix and not re.fullmatch(r"[A-Za-z0-9_-]{1,20}", suffix):
        raise ValueError("invalid DOCKER_SANDBOXES_APP_NAME suffix")
    limit = 103 if platform == "darwin" else 107
    selected = env.get("DOCKER_SANDBOXES_API", "")
    if selected:
        # Match the native LOCAL parser without URL decoding/normalization that
        # could select a different socket from the explicit raw path.
        if any(character in selected for character in "\0\r\n%?#"):
            raise ValueError("ambiguous characters in LOCAL SDK socket selection")
        if selected.startswith("unix://"):
            path = selected.removeprefix("unix://")
        elif "://" in selected:
            raise ValueError("LOCAL shell authority refuses remote SDK endpoints")
        else:
            path = selected
    else:
        home = env.get("HOME", "")
        if home and not Path(home).is_absolute():
            raise ValueError("SDK endpoint discovery needs an absolute HOME")
        app = "sandboxes" + ("-" + suffix if suffix else "")
        namespace = "com.docker.sandboxes" if platform == "darwin" else "sandboxes"
        override = storage_override(env)
        if override:
            # Exact storagekit v0.25 overrides.go: TrimSpace; Join(base, kind).
            base = _join(override, "state")
        elif not home:
            raise ValueError("SDK default discovery needs HOME or an absolute storage/API override")
        elif platform == "darwin":
            base = _join(home, "Library/Application Support")
        else:
            base = _join(env.get("XDG_STATE_HOME") or _join(home, ".local/state"))
        if not base.is_absolute():
            raise ValueError("LOCAL SDK state base must be absolute")
        state = _join(base, namespace, app, "sandboxd")
        path = str(_join(state, "sandboxd.sock"))
        if len(os.fsencode(path)) > limit:
            short = (_join(home, ".sbx/run", "d_" + suffix if suffix else "d") if home else
                     Path(f"/tmp/sboxd-{os.getuid()}-{app}"))
            if not short.is_symlink() or os.readlink(short) != str(state) or not short.is_dir():
                raise ValueError("SDK short socket link is absent/stale; start the selected stock daemon, never repair it from the observer")
            path = str(short / "sandboxd.sock")
    if (not path.startswith("/") or any(c in path for c in "\0\r\n%?#")
            or len(os.fsencode(path)) > limit):
        raise ValueError("invalid/overlong local SDK socket path")
    return "unix://" + path


def validate_local_environment(environment=None) -> None:
    """Reject remote/invalid selection before any builder or template command."""
    env = os.environ if environment is None else environment
    suffix = env.get("DOCKER_SANDBOXES_APP_NAME", "")
    if suffix and not re.fullmatch(r"[A-Za-z0-9_-]{1,20}", suffix):
        raise ValueError("invalid DOCKER_SANDBOXES_APP_NAME suffix")
    if env.get("DOCKER_SANDBOXES_API"):
        endpoint(env)
        return  # Explicit API selects the service; unused storage is not a veto.
    storage_override(env)
    if env.get("XDG_STATE_HOME") and not Path(env["XDG_STATE_HOME"]).is_absolute():
        raise ValueError("LOCAL SDK XDG_STATE_HOME must be absolute")


class UnixConnection(http.client.HTTPConnection):
    def __init__(self, path, timeout):
        super().__init__("localhost", timeout=timeout)
        self.path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(self.timeout)
        self.sock.connect(self.path)


def inspect_image(reference: str, *, environment=None, timeout=5) -> dict:
    if not re.fullmatch(r"docker\.io/library/marsh-(?:shell-local|dev-shell):sha256-[0-9a-f]{64}", reference):
        raise ValueError("SDK inspection requires an exact produced LOCAL image alias")
    address = endpoint(environment)
    connection = UnixConnection(address.removeprefix("unix://"), timeout)
    timer = None
    try:
        connection.connect()
        owned_socket = connection.sock
        def expire():
            try:
                owned_socket.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
        timer = threading.Timer(timeout, expire)
        timer.daemon = True
        timer.start()
        connection.request("GET", "/docker/images/inspect?name=" + quote(reference, safe=""),
                           headers={"Accept": "application/json", "Connection": "close"})
        response = connection.getresponse()
        if response.status != 200:
            raise ValueError(f"SDK image observation failed (HTTP {response.status})")
        length = response.getheader("Content-Length")
        if length is not None and (not length.isdecimal() or int(length) > MAX_BODY):
            raise ValueError("SDK image response exceeds observation bound")
        raw = response.read(MAX_BODY + 1)
        if len(raw) > MAX_BODY:
            raise ValueError("SDK image response exceeds observation bound")
        value = json.loads(raw)
        identifier = value.get("id") if isinstance(value, dict) else None
        if not isinstance(identifier, str) or not re.fullmatch(r"sha256:[0-9a-f]{64}", identifier):
            raise ValueError("SDK did not report a FULL content-addressed image ID")
        # No config/env/credential-bearing payload retained. Only the identity.
        return {"endpoint": address, "reference": reference, "image_id": identifier,
                "observed_unix_ns": time.time_ns()}
    finally:
        if timer is not None:
            timer.cancel()
        connection.close()
