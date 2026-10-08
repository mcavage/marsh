#!/usr/bin/env python3
"""Host-only read-only capture of an immutable public index and platform.

Run it on the host, not in a VM. Docker uses the caller's explicit client
configuration under the caller's authority; this program never opens, copies or
prints credentials. No image/container/build/push or agent execution occurs.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import stat
import subprocess
import time

LIMIT = 4 * 1024**2


def digest(path):
    h = hashlib.sha256()
    with path.open("rb") as source:
        while data := source.read(1024**2):
            h.update(data)
    return h.hexdigest()


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--docker", type=Path, required=True)
    p.add_argument(
        "--buildx",
        type=Path,
        required=True,
        help="Exact buildx executable invoked directly (no plugin discovery)",
    )
    p.add_argument(
        "--docker-config",
        type=Path,
        required=True,
        help="Explicit caller-authorized Docker client config; contents are never read by this collector",
    )
    p.add_argument(
        "--docker-home",
        type=Path,
        required=True,
        help="Existing authorized Docker helper HOME; no credentials are read by this collector",
    )
    p.add_argument("--reference", required=True)
    p.add_argument("--platform", choices=["linux/arm64", "linux/amd64"], required=True)
    p.add_argument("--evidence", type=Path, required=True)
    a = p.parse_args()
    if not re.fullmatch(
        r"(?:dhi.io/(?:sbx-templates|debian-base)(?::[A-Za-z0-9._-]+)?|(?:docker.io/library/)?rust(?::[A-Za-z0-9._-]+)?)@sha256:[0-9a-f]{64}",
        a.reference,
    ):
        p.error("reviewed public base repository and immutable index digest required")
    out = a.evidence.absolute()
    for parent in out.parents:
        if not stat.S_ISDIR(parent.lstat().st_mode):
            raise ValueError("symlink/non-directory output ancestor")
    out.mkdir(mode=0o700, exist_ok=False)
    if out.stat().st_uid != os.getuid() or stat.S_IMODE(out.stat().st_mode) != 0o700:
        raise ValueError("owner-private evidence required")
    docker = a.docker.resolve(strict=True)
    buildx = a.buildx.resolve(strict=True)
    hashes = {"docker_sha256": digest(docker), "buildx_sha256": digest(buildx)}
    commands = []
    env = {
        # Docker's configured credential helper is installed beside the selected
        # client. Retain these explicit tool directories without inheriting an
        # arbitrary caller PATH or reading credentials ourselves.
        "PATH": os.pathsep.join((str(docker.parent), str(buildx.parent), "/usr/bin", "/bin")),
        "HOME": str(a.docker_home.absolute()),
        "DOCKER_CONFIG": str(a.docker_config.absolute()),
    }

    def raw(label, reference):
        argv = [str(buildx), "imagetools", "inspect", "--raw", reference]
        started = time.monotonic()
        with (
            (out / (label + ".json")).open("xb") as stdout,
            (out / (label + ".stderr")).open("xb") as stderr,
        ):
            # Kernel bounds limit files while Docker runs, not only after reading.
            import resource

            def bound():
                resource.setrlimit(resource.RLIMIT_FSIZE, (LIMIT, LIMIT))

            result = subprocess.run(
                argv,
                env=env,
                stdout=stdout,
                stderr=stderr,
                timeout=120,
                preexec_fn=bound,
            )
        commands.append(
            {
                "argv": argv,
                "status": result.returncode,
                "seconds": time.monotonic() - started,
                "stdout_sha256": digest(out / (label + ".json")),
                "stderr_sha256": digest(out / (label + ".stderr")),
            }
        )
        if result.returncode:
            raise ValueError("read-only index fetch failed; evidence retained")
        return json.loads((out / (label + ".json")).read_bytes())

    record = {
        "schema": "marsh.primary-index-capture/v2",
        "reference": a.reference,
        "platform": a.platform,
        **hashes,
        "docker_path": str(docker),
        "buildx_path": str(buildx),
        "docker_home": str(a.docker_home.absolute()),
        "commands": commands,
        "index_file": "index.json",
        "platform_file": "platform.json",
        "error": None,
    }
    try:
        index = raw("index", a.reference)
        if digest(out / "index.json") != a.reference.rsplit("@sha256:", 1)[1]:
            raise ValueError("raw index does not hash to requested pin")
        os_name, architecture = a.platform.split("/")
        selected = [
            r
            for r in index["manifests"]
            if r.get("platform", {}).get("os") == os_name
            and r.get("platform", {}).get("architecture") == architecture
        ]
        if len(selected) != 1:
            raise ValueError("ambiguous/missing platform")
        descriptor = selected[0]
        child = a.reference.split("@", 1)[0] + "@" + descriptor["digest"]
        raw("platform", child)
        if (
            digest(out / "platform.json")
            != descriptor["digest"].removeprefix("sha256:")
            or (out / "platform.json").stat().st_size != descriptor["size"]
        ):
            raise ValueError("platform bytes differ from primary index")
        record.update(
            index_sha256=digest(out / "index.json"),
            platform_digest=descriptor["digest"],
        )
        if (
            digest(docker) != hashes["docker_sha256"]
            or digest(buildx) != hashes["buildx_sha256"]
        ):
            raise ValueError("index tool changed during capture")
    except Exception as error:
        record["error"] = str(error)
    (out / "index-proof.json").write_text(json.dumps(record, indent=2) + "\n")
    print(
        json.dumps(
            {"proof": str(out / "index-proof.json"), "passed": record["error"] is None}
        )
    )
    return int(record["error"] is not None)


if __name__ == "__main__":
    raise SystemExit(main())
