#!/usr/bin/env python3
"""Real host-proof CLI against a recording Docker peer, NEVER host image proof."""

import argparse
import hashlib
import json
from pathlib import Path
import stat
import shutil
import socketserver
import tempfile
import threading
import subprocess
import sys

HERE = Path(__file__).absolute().parent
ROOT = HERE.parent.parent


def hashed(data):
    return hashlib.sha256(data).hexdigest()


def dump(path, obj):
    path.write_text(json.dumps(obj, indent=2) + "\n")


def main():
    global ROOT, HERE
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--work", type=Path, required=True)
    p.add_argument("--driver-source", type=Path, help="Archived driver for defect RED evidence")
    a = p.parse_args()
    w = a.work.absolute()
    w.mkdir(mode=0o700, exist_ok=False)
    fixture = w / "root"
    shutil.copytree(HERE, fixture / "packaging/dhi-notices", ignore=shutil.ignore_patterns("__pycache__"))
    shutil.copytree(ROOT / "packaging/image-repair", fixture / "packaging/image-repair", ignore=shutil.ignore_patterns("__pycache__"))
    inputs = ["packaging/shell/Dockerfile",
              "scripts/npm-notices.mjs", "kits/marsh-codex/marsh-entrypoint.sh",
              "kits/marsh-codex/release-checksums.txt", "kits/marsh-claude/release-checksums.txt",
              "packaging/shell-image"]
    inputs += [str(f.relative_to(ROOT)) for pattern in ("*/*.dockerfile", "*/notices/overrides.json")
               for f in (ROOT / "kits").glob(pattern)]
    for name in inputs:
        target = fixture / name
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(ROOT / name, target)
    if a.driver_source:
        shutil.copy2(a.driver_source, fixture / "packaging/dhi-notices/host-proof.py")
    ROOT, HERE = fixture, fixture / "packaging/dhi-notices"
    bundle = HERE / "collected"
    records = {
        str(f.relative_to(bundle)): {
            "sha256": hashed(f.read_bytes()),
            "size": f.stat().st_size,
            "mode": stat.S_IMODE(f.stat().st_mode),
        }
        for f in sorted(bundle.rglob("*"))
        if f.is_file()
        and f.name not in {"image-verification.json", "base-image-verification.json"}
    }
    reviewed = {
        str(f.relative_to(ROOT)): {"sha256": hashed(f.read_bytes()),
                                  "size": f.stat().st_size,
                                  "mode": stat.S_IMODE(f.stat().st_mode)}
        for f in ROOT.rglob("*") if f.is_file()
    }
    source = w / "reviewed-source.json"
    dump(source, reviewed)
    tree_hash = hashed(
        json.dumps(records, sort_keys=True, separators=(",", ":")).encode()
    )
    response = {
        "schema": "marsh.dhi-notices-verification/v2",
        "stage": "base",
        "profile": "base",
        "selected_version": None,
        "scope_kind": "base-payload-only",
        "verifier_sha256": records["verify.py"]["sha256"],
        "bundle_tree_sha256": tree_hash,
        "adapter_scope": None,
    }
    reply = w / "reply.json"
    dump(reply, response)
    image = {"Id": "sha256:" + "1" * 64, "Os": "linux", "Architecture": "arm64"}
    tool = w / "docker-recorder"
    # An actual owned Unix peer records every effect; the selected executable
    # is a tiny protocol client, not a real Docker client/daemon.
    peer_directory = tempfile.TemporaryDirectory(prefix="marsh-proof-peer-", dir="/private/tmp" if Path("/private/tmp").is_dir() else None)
    socket_path = str(Path(peer_directory.name) / "peer.sock")
    class Handler(socketserver.StreamRequestHandler):
        def handle(self):
            argv = json.loads(self.rfile.readline())
            with (w / "calls.jsonl").open("a") as out:
                out.write(json.dumps(argv) + "\n")
            if argv[0] == "version":
                result = {}
            elif argv[:2] == ["image", "inspect"]:
                result = [image]
            elif argv[0] == "run":
                result = json.loads((w / ("baked.json" if "/dev/stdin" in argv else "reply.json")).read_text())
                if "/dev/stdin" in argv and baked_base is not None:
                    result["marsh-image-repair/base-images.json"] = baked_base
            else:
                result = {"unexpected": argv}
            self.wfile.write(json.dumps(result).encode() + b"\n")
    server = socketserver.UnixStreamServer(socket_path, Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    tool.write_text(
        "#!" + sys.executable + "\n"
        "import json,os,socket,sys\n"
        "peer=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM)\n"
        "peer.connect(os.environ['DOCKER_HOST'].removeprefix('unix://'))\n"
        "peer.sendall(json.dumps(sys.argv[1:]).encode()+b'\\n')\n"
        "data=peer.makefile('rb').readline()\n"
        "sys.stdout.buffer.write(data)\n"
    )
    tool.chmod(0o755)
    # Synthetic immutable OCI bytes exercise the primary index/descriptor/hash
    # boundary, not an assertion that any registry/image was contacted.
    platform = (
        b'{"schemaVersion":2,"config":{"digest":"sha256:'
        + b"2" * 64
        + b'"},"layers":[]}'
    )
    child = "sha256:" + hashed(platform)
    index = json.dumps(
        {
            "schemaVersion": 2,
            "manifests": [
                {
                    "digest": child,
                    "size": len(platform),
                    "platform": {"os": "linux", "architecture": "arm64"},
                }
            ],
        },
        separators=(",", ":"),
    ).encode()
    ref = "dhi.io/sbx-templates@sha256:" + hashed(index)
    (w / "index.json").write_bytes(index)
    (w / "platform.json").write_bytes(platform)
    index_record = {
        "schema": "marsh.primary-index-capture/v1",
        "reference": ref,
        "platform": "linux/arm64",
        "docker_sha256": hashed(tool.read_bytes()),
        "index_file": "index.json",
        "platform_file": "platform.json",
        "index_sha256": hashed(index),
        "platform_digest": child,
        "commands": [
            {
                "argv": [str(tool), "buildx", "imagetools", "inspect", "--raw", r],
                "status": 0,
                "stdout_sha256": h,
            }
            for r, h in [
                (ref, hashed(index)),
                (ref.split("@")[0] + "@" + child, hashed(platform)),
            ]
        ],
        "error": None,
    }
    capture = w / "index-proof.json"
    dump(capture, index_record)
    build = w / "build.json"
    dump(build, {"fixture": "opaque producer binding only; NOT Oracle validation"})
    import base64

    repair = {
        "repair_script_sha256": hashed(
            (ROOT / "packaging/image-repair/repair.py").read_bytes()
        ),
        "manifest_sha256": hashed(
            (ROOT / "packaging/image-repair/artifacts.json").read_bytes()
        ),
    }

    def saved(obj):
        data = json.dumps(obj).encode()
        return {"sha256": hashed(data), "base64": base64.b64encode(data).decode()}

    # Preserve the exact source serialization, not a JSON-equivalent rewrite.
    base_bytes = (ROOT / 'packaging/image-repair/base-images.json').read_bytes()
    baked_base = {'sha256': hashed(base_bytes), 'base64': base64.b64encode(base_bytes).decode()}
    good_base = dict(baked_base)
    dump(
        w / "baked.json",
        {
            "marsh-dhi/base-image-verification.json": saved(response),
            "marsh-image-repair/repair-receipt.json": saved(repair),
        },
    )
    results = []

    def run(name, expected, baked=False):
        argv = [
            sys.executable,
            "-B",
            str(HERE / "host-proof.py"),
            "--docker",
            str(tool),
            "--docker-host",
            "unix://" + socket_path,
            "--image",
            image["Id"],
            "--platform",
            "linux/arm64",
            "--evidence",
            str(w / name),
            "--reviewed-source",
            str(source),
            "--index-record",
            str(capture),
        ]
        if baked:
            argv += [
                "--baked",
                "--recipe",
                str(ROOT / "packaging/shell/Dockerfile"),
                "--build-receipt",
                str(build),
                "--shell-base-index",
                ref,
            ]
        else:
            argv += ["--supporting-base-bind"]
        r = subprocess.run(argv, capture_output=True, timeout=90)
        (w / (name + ".stdout")).write_bytes(r.stdout)
        (w / (name + ".stderr")).write_bytes(r.stderr)
        reason = {
            'consistent-container-edit-denied': 'externally reviewed tree',
            'stray-container-file-denied': 'externally reviewed tree',
            'primary-platform-corruption-denied': 'primary platform bytes',
            'wrong-index-platform-denied': 'index-capture platform assertion',
            'wrong-baked-stage-denied': 'baked receipt is stale/wrong scope',
            'unreviewed-local-source-denied': 'externally reviewed source tree',
            'stale-repair-denied': 'externally reviewed source tree',
            'stale-driver-denied': 'externally reviewed source tree',
            'stale-artifacts-denied': 'externally reviewed source tree',
            'stale-base-images-denied': 'externally reviewed source tree',
            'unknown-source-denied': 'externally reviewed source tree',
            'unpinned-from-denied': 'unpinned/unknown recipe FROM',
            'unknown-argument-from-denied': 'unpinned/unknown recipe FROM',
            'forward-alias-from-denied': 'unpinned/unknown recipe FROM',
            'wrong-repair-receipt-denied': 'repair receipt is not from reviewed',
            'missing-baked-final-denied': 'required baked receipt missing: marsh-dhi/image-verification.json',
            'missing-base-images-denied': 'required baked repair input missing:',
            'changed-base-images-denied': 'baked base-images is not from reviewed repair inputs',
            'escape-directive-from-denied': 'unsupported Dockerfile parser directive: escape',
            'skip-directive-from-denied': 'unsupported Dockerfile parser directive: check',
            'stale-release-checksums-denied': 'externally reviewed source tree',
            'stale-shell-image-denied': 'externally reviewed source tree',

        }.get(name)
        proof_path = w / name / 'proof.json'
        error = (json.loads(proof_path.read_text()).get('error') or '') if proof_path.exists() else r.stderr.decode()
        right_reason = reason is None or reason in error
        results.append(
            {
                "control": name,
                "argv": argv,
                "exit": r.returncode,
                "expected_success": expected,
                "as_expected": (r.returncode == 0) == expected and right_reason,
                "expected_error": reason,
                "error": error,
            }
        )

    run("positive-external-anchor", True)
    altered = dict(response, bundle_tree_sha256="0" * 64)
    dump(reply, altered)
    run("consistent-container-edit-denied", False)
    dump(reply, response)
    altered = dict(response, bundle_tree_sha256="a" * 64)
    dump(reply, altered)
    run("stray-container-file-denied", False)
    dump(reply, response)
    (w / "platform.json").write_bytes(platform + b" ")
    run("primary-platform-corruption-denied", False)
    (w / "platform.json").write_bytes(platform)
    altered = dict(index_record, platform="linux/amd64")
    dump(capture, altered)
    run("wrong-index-platform-denied", False)
    dump(capture, index_record)
    # The fixture has staged0644 files, matching real canonical source modes.
    assert all(row["mode"] == 0o644 for row in records.values())
    run("baked-no-bind-positive", True, baked=True)
    proof = json.loads((w / "baked-no-bind-positive/proof.json").read_text())
    assert proof["baked"] and all("--mount" not in c["argv"] for c in proof["commands"])
    changed = dict(response, stage="final")
    dump(
        w / "baked.json",
        {
            "marsh-dhi/base-image-verification.json": saved(changed),
            "marsh-image-repair/repair-receipt.json": saved(repair),
        },
    )
    run("wrong-baked-stage-denied", False, baked=True)
    dump(w / "baked.json", {"marsh-dhi/base-image-verification.json": saved(response),
                           "marsh-image-repair/repair-receipt.json": saved(repair)})
    wrong_repair = dict(repair, repair_script_sha256="a" * 64)
    dump(w / "baked.json", {"marsh-dhi/base-image-verification.json": saved(response),
                           "marsh-image-repair/repair-receipt.json": saved(wrong_repair)})
    run("wrong-repair-receipt-denied", False, baked=True)
    dump(w / "baked.json", {"marsh-dhi/base-image-verification.json": saved(response),
                           "marsh-image-repair/repair-receipt.json": saved(repair)})
    baked_base = None
    run('missing-base-images-denied', False, baked=True)
    baked_base = saved({'fixture': 'coherent replacement of baked base index'})
    run('changed-base-images-denied', False, baked=True)
    baked_base = good_base
    dump(reply, dict(response, stage='final'))
    run('missing-baked-final-denied', False, baked=True)
    dump(reply, response)
    run('baked-restored-positive', True, baked=True)
    direct = dict(index_record, schema="marsh.primary-index-capture/v2",
                  buildx_path=str(tool), buildx_sha256=hashed(tool.read_bytes()), docker_home=str(w))
    direct["commands"] = [dict(command, argv=[str(tool), *command["argv"][2:]])
                          for command in index_record["commands"]]
    dump(capture, direct)
    run("v2-direct-index-binding-positive", True, baked=True)
    dump(capture, index_record)
    # Mutate actual isolated source AFTER review, never the shared repository.
    for name, rel in [("stale-repair-denied", "packaging/image-repair/repair.py"),
                      ("stale-driver-denied", "packaging/dhi-notices/host-proof.py"),
                      ("stale-artifacts-denied", "packaging/image-repair/artifacts.json"),
                      ("stale-base-images-denied", "packaging/image-repair/base-images.json"),
                      ("stale-release-checksums-denied", "kits/marsh-codex/release-checksums.txt"),
                      ("stale-shell-image-denied", "packaging/shell-image")]:
        path = ROOT / rel
        old = path.read_bytes()
        path.write_bytes(old + b"\n")
        calls_before = (w / "calls.jsonl").read_bytes()
        run(name, False)
        results[-1]["pre_effect"] = (w / "calls.jsonl").read_bytes() == calls_before
        results[-1]["as_expected"] &= results[-1]["pre_effect"]
        path.write_bytes(old)
    stray = ROOT / "packaging/image-repair/unreviewed.json"
    stray.write_text("{}\n")
    run("unknown-source-denied", False)
    stray.unlink()
    recipe = ROOT / "packaging/shell/Dockerfile"
    original = recipe.read_bytes()
    for name, extra, passes in [
            ("unpinned-from-denied", "FROM debian:latest AS extra\n", False),
            ("unknown-argument-from-denied", "FROM ${UNREVIEWED} AS extra\n", False),
            ("forward-alias-from-denied", "FROM future AS extra\n", False),
            ("earlier-alias-accepted", "FROM " + ref + " AS earlier\nFROM earlier AS extra\n", True),
            ("escape-directive-from-denied", "# escape=`\nLABEL hidden=yes \\\nFROM debian:latest\n", False),
            ("skip-directive-from-denied", "# check=skip=all\n", False)]:
        recipe.write_bytes(extra.encode() + original)
        review = dict(reviewed)
        review["packaging/shell/Dockerfile"] = {"sha256": hashed(recipe.read_bytes()),
                                             "size": recipe.stat().st_size, "mode": 0o644}
        dump(source, review)
        calls_before = (w / 'calls.jsonl').read_bytes()
        run(name, passes, baked=True)
        if not passes:
            results[-1]['pre_effect'] = (w / 'calls.jsonl').read_bytes() == calls_before
            results[-1]['as_expected'] &= results[-1]['pre_effect']
    recipe.write_bytes(original)
    dump(source, reviewed)
    # Actual reviewed-source edit is rejected before any peer effect.
    review = json.loads(source.read_text())
    review["packaging/dhi-notices/collected/verify.py"]["sha256"] = "f" * 64
    dump(source, review)
    calls = (w / "calls.jsonl").read_bytes()
    run("unreviewed-local-source-denied", False)
    assert (w / "calls.jsonl").read_bytes() == calls
    dump(
        w / "results.json",
        {
            "scope": "Real host-proof CLI and owned filesystem with owned Unix protocol peer/synthetic OCI, NOT actual Docker/index/build qualification",
            "controls": results,
        },
    )
    server.shutdown()
    server.server_close()
    peer_directory.cleanup()
    print(json.dumps(results, indent=2))
    return int(not all(r["as_expected"] for r in results))


if __name__ == "__main__":
    raise SystemExit(main())
