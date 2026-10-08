#!/usr/bin/env python3
"""Production DockerCliRuntime byte bridge proof inside an OWNED Linux VM.

No stock/Cloud APIs, credentials, mutable image fallback, or global cleanup.
Upload the frozen native helper, driver, fixture and build proof, then invoke
it in an owned DHI nested-Docker VM.
This component gate does NOT replace final public stock shell/Kit/Cloud UAT.
Python is only the independent oracle/driver, never the product implementation.
"""
import argparse
import hashlib
import io
import json
import os
from pathlib import Path
import re
import shutil
import struct
import subprocess
import sys
import tarfile
import time
import uuid


def sha(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def run(args, *, timeout=45, data=None):
    return subprocess.run(args, input=data, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=timeout)


def checked(args, **kwargs):
    result = run(args, **kwargs)
    if result.returncode:
        raise RuntimeError(f"probe control failed: {args[0]} status {result.returncode}; no fallback")
    return result.stdout


def unpack(raw):
    assert raw.startswith(b"BYTEPROBE1"), raw.hex()
    offset = 10
    count = struct.unpack_from(">I", raw, offset)[0]
    offset += 4
    fields = []
    for _ in range(count + 7):
        length = struct.unpack_from(">I", raw, offset)[0]
        offset += 4
        fields.append(raw[offset:offset + length])
        offset += length
    assert offset == len(raw)
    return fields[:count], fields[count:]


def native_probe(args, stage, proof, artifacts):
    """Independent binary producer -> actual static Rust decoder -> native fixture."""
    def blob(value):
        return struct.pack(">I", len(value)) + value

    def encode(argv, env, cwd):
        return (b"SBXBYT01" + struct.pack(">I", len(argv)) + b"".join(blob(word) for word in argv)
                + struct.pack(">I", len(env)) + b"".join(blob(name) + blob(value) for name, value in env.items()) + blob(cwd))

    workspace = args.evidence / "native-workspace"
    workspace.mkdir(mode=0o700)
    raw_cwd = os.fsencode(workspace) + b"/raw-\xff\xfe"
    os.mkdir(raw_cwd)
    fixture = os.fsencode(stage.resolve() / "byte-fixture")
    helper = stage.resolve() / "marsh-byte-exec"
    environment = {"PATH": "/usr/bin:/bin", "STATIC_IMAGE_VALUE": "image-static", "HOME": str(workspace.resolve()), "USER": "probe", "LOGNAME": "probe"}
    data = b"stdin\0\xff\xfe\n"
    words = [b"fixed-entry", b"\xff\xfe", b"", bytes(range(1, 256))]
    payload = encode([fixture] + words, {b"PROBE_VALUE": bytes(range(1, 256))}, raw_cwd)
    carrier = args.evidence / "native-payload"
    carrier.write_bytes(payload)
    carrier.chmod(0o444)
    result = subprocess.run([str(helper), "--payload", str(carrier.resolve())], input=data, env=environment,
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=10)
    assert result.returncode == 37
    actual, fields = unpack(result.stdout)
    assert actual == words
    assert fields == [bytes(range(1, 256)), b"image-static", os.fsencode(workspace.resolve()), b"probe", b"probe", raw_cwd, data]
    assert result.stderr == b"stderr\0\xff\xfe\n"
    marker = Path(os.fsdecode(raw_cwd + b"/observed-\xff.bin"))
    assert marker.read_bytes() == b"file\0\xff\xfe\n"
    marker.unlink()
    negatives = [b"", payload[:-1], payload + b"x",
                 encode([fixture, b"NUL\0word"], {}, raw_cwd),
                 encode([fixture], {b"DOCKER_HOST": b"secret-canary"}, raw_cwd),
                 encode([fixture], {b"PROBE_VALUE": b"x" * 16385}, raw_cwd)]
    for negative in negatives:
        carrier.chmod(0o600)
        carrier.write_bytes(negative)
        carrier.chmod(0o444)
        rejected = subprocess.run([str(helper), "--payload", str(carrier.resolve())], env=environment,
                                  stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=10)
        assert rejected.returncode == 125 and not rejected.stdout
        assert rejected.stderr == b"marsh: native byte launch failed\n"
        assert not marker.exists()
    carrier.chmod(0o600)
    carrier.write_bytes(encode([b"/usr/bin/yes"], {}, raw_cwd))
    carrier.chmod(0o444)
    reader, writer = os.pipe()
    os.close(reader)
    try:
        signaled = subprocess.run([str(helper), "--payload", str(carrier.resolve())], env=environment,
                                  stdout=writer, stderr=subprocess.PIPE, timeout=10)
    finally:
        os.close(writer)
    assert signaled.returncode == -13 and not signaled.stderr, "Rust SIGPIPE ignore leaked into native image command"
    nested = args.evidence / "nested-payload"
    effect = workspace / "must-not-run-via-injected-helper"
    nested.write_bytes(encode([b"/usr/bin/touch", os.fsencode(effect.resolve())], {}, raw_cwd))
    nested.chmod(0o444)
    alias = workspace / "launcher-alias"
    alias.symlink_to(helper)
    carrier.chmod(0o600)
    carrier.write_bytes(encode([os.fsencode(alias.absolute()), b"--payload", os.fsencode(nested.resolve())], {}, raw_cwd))
    carrier.chmod(0o444)
    denied = subprocess.run([str(helper), "--payload", str(carrier.resolve())], env=environment,
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=10)
    assert denied.returncode == 126 and not effect.exists(), f"injected helper became the image command via an alias: status={denied.returncode}, effect={effect.exists()}"
    (args.evidence / "stdout.bin").write_bytes(result.stdout)
    (args.evidence / "stderr.bin").write_bytes(result.stderr)
    report = {"passed": True, "qualification": "native helper only; Docker/stock/Cloud NOT run", "artifacts": artifacts,
              "build_proof": proof, "raw_argv_env_cwd_stdin_output_file_status": True, "sigpipe13": True, "injected_helper_alias_denied": True, "negative_no_effect_cases": len(negatives) + 1}
    (args.evidence / "result.json").write_text(json.dumps(report, indent=2))
    print(json.dumps({"passed": True, "native_only": True, "evidence": str(args.evidence)}))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--driver", type=Path, required=True)
    parser.add_argument("--helper", type=Path, required=True)
    parser.add_argument("--fixture", type=Path, required=True)
    parser.add_argument("--build-proof", type=Path, required=True)
    parser.add_argument("--native", action="store_true", help="native-only helper proof, no Docker/stock/Cloud")
    parser.add_argument("--image", help="exact immutable source image, no mutable fallback")
    parser.add_argument("--evidence", type=Path, required=True)
    parser.add_argument("--namespace", help="unique marsh-bytes-... owned container namespace")
    args = parser.parse_args()
    assert sys.platform == "linux"
    if not args.native:
        assert os.geteuid() == 0, "run only as root in the owned Linux nested-Docker VM"
        assert re.fullmatch(r"(?:[a-z0-9./:_-]+@)?sha256:[a-f0-9]{64}", args.image or "")
        assert re.fullmatch(r"marsh-bytes-[a-z0-9-]{8,64}", args.namespace or "")
    assert not args.evidence.exists()
    args.evidence.mkdir(mode=0o700, parents=True)
    proof = json.loads(args.build_proof.read_text())
    artifacts = {name: sha(path) for name, path in [("driver", args.driver), ("helper", args.helper), ("fixture", args.fixture)]}
    recorded = proof["artifacts"]
    assert set(recorded) in (set(artifacts), set(artifacts) | {"shell-decoder-probe"}), "unknown build artifact scope"
    assert all(recorded[name] == digest for name, digest in artifacts.items()), "exact built native artifacts required"
    assert proof["root_cargo_lock_sha256"] and proof["source_sha256"]
    stage = args.evidence / "artifacts"
    stage.mkdir(mode=0o755)
    for path, name in [(args.driver, "byte_exec_probe"), (args.helper, "marsh-byte-exec"), (args.fixture, "byte-fixture")]:
        shutil.copyfile(path, stage / name)
        (stage / name).chmod(0o755)
    if args.native:
        native_probe(args, stage, proof, artifacts)
        return
    driver = stage / "byte_exec_probe"
    baseline_containers = set(checked(["docker", "container", "ls", "-aq", "--no-trunc"]).decode().split())
    worker_home = args.evidence.resolve() / "worker-home"
    worker_home.mkdir(mode=0o700)
    carrier_root = worker_home / ".marsh-byte-carriers"
    driver_env = {"PATH": "/usr/bin:/bin", "HOME": str(worker_home)}
    baseline = json.loads(checked(["docker", "image", "inspect", args.image]))[0]
    assert baseline["Os"] == "linux" and baseline["Architecture"] == "arm64", "explicit ARM64 probe only"
    # No RUN command, shell, credentials or mutable base. The resulting exact
    # config ID (not this transient tag) is used for every runtime job below.
    dockerfile = f'FROM {args.image}\nCOPY byte-fixture /byte-fixture\nENV STATIC_IMAGE_VALUE=image-static\nENTRYPOINT ["/byte-fixture","fixed-entry"]\nCMD ["default-cmd"]\n'
    (stage / "Dockerfile").write_text(dockerfile)
    tag = args.namespace + ":fixture"
    owned_tags = [tag, args.namespace + ":cmd-only", args.namespace + ":fixed-path-collision", args.namespace + ":missing", args.namespace + ":denied", args.namespace + ":foreign"]
    existing_tags = set(checked(["docker", "image", "ls", "--format", "{{.Repository}}:{{.Tag}}"]).decode().splitlines())
    assert not existing_tags.intersection(owned_tags), "namespace already owned; do not overwrite"
    checked(["docker", "build", "--network=none", "--pull=false", "--platform=linux/arm64", "-t", tag, str(stage)], timeout=180)
    image = checked(["docker", "image", "inspect", "--format", "{{.Id}}", tag]).strip().decode("ascii")
    image_config = json.loads(checked(["docker", "image", "inspect", image]))[0]
    (args.evidence / "image-config.json").write_text(json.dumps(image_config, indent=2))
    cmd_tag = args.namespace + ":cmd-only"
    (stage / "Dockerfile").write_text(f'FROM {args.image}\nCOPY byte-fixture /byte-fixture\nENV STATIC_IMAGE_VALUE=image-static\nENTRYPOINT []\nCMD ["/byte-fixture","default-cmd"]\n')
    checked(["docker", "build", "--network=none", "--pull=false", "--platform=linux/arm64", "-t", cmd_tag, str(stage)], timeout=180)
    cmd_image = checked(["docker", "image", "inspect", "--format", "{{.Id}}", cmd_tag]).strip().decode("ascii")
    (args.evidence / "cmd-only-image-config.json").write_bytes(checked(["docker", "image", "inspect", cmd_image]))
    collision_tag = args.namespace + ":fixed-path-collision"
    (stage / "Dockerfile").write_text(f'FROM {args.image}\nCOPY byte-fixture /run/marsh/byte-exec/helper\nENV STATIC_IMAGE_VALUE=image-static\nENTRYPOINT ["/run/marsh/byte-exec/helper","fixed-entry"]\nCMD ["default-cmd"]\n')
    checked(["docker", "build", "--network=none", "--pull=false", "--platform=linux/arm64", "-t", collision_tag, str(stage)], timeout=180)
    collision_image = checked(["docker", "image", "inspect", "--format", "{{.Id}}", collision_tag]).strip().decode("ascii")
    (args.evidence / "collision-image-config.json").write_bytes(checked(["docker", "image", "inspect", collision_image]))
    status_images = {}
    for name, entrypoint in [("missing", "/marsh-command-does-not-exist"), ("denied", "/not-executable")]:
        (stage / "not-executable").write_bytes(b"never execute this data\n")
        (stage / "not-executable").chmod(0o644)
        (stage / "Dockerfile").write_text(f'FROM {args.image}\nCOPY not-executable /not-executable\nENTRYPOINT ["{entrypoint}"]\nCMD []\n')
        status_tag = args.namespace + ":" + name
        checked(["docker", "build", "--network=none", "--pull=false", "--platform=linux/arm64", "-t", status_tag, str(stage)], timeout=180)
        status_images[name] = checked(["docker", "image", "inspect", "--format", "{{.Id}}", status_tag]).strip().decode("ascii")
    empty_archive = io.BytesIO()
    with tarfile.open(fileobj=empty_archive, mode="w"):
        pass
    foreign_image = checked(["docker", "image", "import", "--platform=linux/amd64", "--change", 'CMD ["/unused"]', "-", args.namespace + ":foreign"], data=empty_archive.getvalue()).strip().decode("ascii")
    assert json.loads(checked(["docker", "image", "inspect", foreign_image]))[0]["Architecture"] == "amd64"
    # Independently owned sibling detects unintended broad signals/cleanup.
    canary = checked(["docker", "create", "--name", args.namespace + "-canary", "--network=none", "--user", "65534:65534", image, "wait-signal"]).strip().decode("ascii")
    checked(["docker", "start", canary])
    ownership = {"baseline_containers": sorted(baseline_containers), "owned_tags": owned_tags, "status_images": status_images, "canary": canary, "containers": [], "image": image, "tag": tag, "cmd_image": cmd_image, "cmd_tag": cmd_tag, "collision_image": collision_image, "collision_tag": collision_tag}
    ownership_file = args.evidence / "ownership.json"
    ownership_file.write_text(json.dumps(ownership, indent=2))
    project = Path("/run/marsh/grants") / (args.namespace + "-" + uuid.uuid4().hex[:12]) / "project"
    project.mkdir(parents=True, mode=0o777)
    project.chmod(0o777)
    raw_child = os.fsencode(project) + b"/raw-\xff\xfe"
    os.mkdir(raw_child, mode=0o777)
    os.chmod(raw_child, 0o777)
    rows = []
    carrier_dirs = set()

    def reclaim(source, name):
        output = args.evidence / (name + "-recovery")
        recovered = subprocess.run([str(driver), str(source), str(output), "reclaim"], env=driver_env,
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=30)
        assert recovered.returncode == 0 and not recovered.stdout and not recovered.stderr
        return json.loads((output / "result.json").read_text())["reclaimed"]

    def canary_alive():
        assert checked(["docker", "inspect", "--format", "{{.State.Running}}", canary]).strip() == b"true"

    def delete_exact(cid):
        checked(["docker", "rm", "--force", cid])
        assert not checked(["docker", "container", "ls", "-aq", "--no-trunc", "--filter", "id=" + cid]).strip()

    def case(name, tail, value, *, mode="normal", cwd=b"/work", signal=False, job_image=None, expected_argv=None, status=None, tty=False, reject_platform=False):
        spec = {"image": job_image or image, "argv": [list(word) for word in tail],
                "identity": {"uid": 65534, "gid": 65534},
                "session_environment": {"HOME": "/work", "MARSH_SELECTED_HOME": "/work", "USER": "nobody", "LOGNAME": "nobody"},
                "exported_environment": {"PROBE_VALUE": list(value)}, "working_directory": list(cwd),
                "mounts": [{"source": str(project), "target": "/work", "access": "read_write"}],
                "resources": {"cpu_millis": 500, "memory_bytes": 134217728, "pids": 32, "writable_bytes": 67108864, "output_bytes": 1048576, "wall_seconds": 30},
                "terminal": tty, "terminal_size": {"rows": 24, "columns": 80} if tty else None}
        source = args.evidence / (name + ".json")
        source.write_text(json.dumps(spec))  # harmless fixture data only
        output = args.evidence / name
        started = time.monotonic()
        before_ids = set(checked(["docker", "container", "ls", "-aq", "--no-trunc"]).decode().split()) if reject_platform else None
        child = subprocess.Popen([str(driver), str(source), str(output), mode], env=driver_env, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        if signal:
            deadline = time.monotonic() + 15
            while time.monotonic() < deadline:
                if (output / "container-id").exists():
                    cid = (output / "container-id").read_text()
                    state = run(["docker", "inspect", "--format", "{{.State.Running}}", cid])
                    if state.returncode == 0 and state.stdout.strip() == b"true":
                        # Wait for fixture output, not merely the helper/init PID.
                        if (output / "stdout.bin").exists() and (output / "stdout.bin").stat().st_size:
                            checked(["docker", "kill", "--signal", "TERM", cid])
                            break
                time.sleep(.02)
            else:
                raise AssertionError("signal fixture never became observable; ownership retained")
        stdout, stderr = child.communicate(timeout=45)
        result = json.loads((output / "result.json").read_text())
        row = {"name": name, "driver_status": child.returncode, "elapsed_ms": (time.monotonic() - started) * 1000, "result": result}
        if reject_platform:
            assert child.returncode == 125 and result["create"] is False and "platform mismatch" in result["error"]
            assert not (output / "container-id").exists()
            assert before_ids == set(checked(["docker", "container", "ls", "-aq", "--no-trunc"]).decode().split())
            assert not carrier_root.exists() or not list(carrier_root.iterdir())
            canary_alive()
            row["zero_container_effects"] = True
            rows.append(row)
            (args.evidence / "cases.json").write_text(json.dumps(rows, indent=2))
            return
        if mode in ("lost-create", "create-conflict"):
            assert child.returncode == 125 and result["create"] is False
            if mode == "create-conflict":
                assert "conflict" in result["error"].lower() and "already in use" in result["error"].lower(), result
                assert len(result["error"].encode()) < 4608
                assert "carrier-secret-canary" not in result["error"] and "PROBE_VALUE" not in result["error"]
                row["actual_docker_stderr_preserved_without_payload"] = True
            attempt = re.search(r"marsh-bytes-[0-9a-f]{32}", result["error"]).group(0)
            inspect = json.loads(checked(["docker", "inspect", attempt]))[0]
            cid = inspect["Id"]
        elif mode == "orphan":
            assert child.returncode == 0 and result["orphan"] is True
            cid = result["container"]
        else:
            assert child.returncode == 0 and not stdout and not stderr
            cid = result["container"]
            expected_status = status if status is not None else (143 if signal else 37)
            assert result["execution"] == expected_status, result
            assert result["attached_exit"] == expected_status, result
            if tty:
                assert (output / "stdout.bin").read_bytes().replace(b"\r", b"") == b"TTY:1:1:1:fffe\n"
                assert not (output / "stderr.bin").read_bytes()
            elif status is not None:
                assert not (output / "stdout.bin").read_bytes()
                assert (output / "stderr.bin").read_bytes(), "fixed exec failure diagnostic required"
            else:
                got_argv, fields = unpack((output / "stdout.bin").read_bytes())
                assert got_argv == (expected_argv if expected_argv is not None else [b"fixed-entry"] + (tail or [b"default-cmd"]))
                expected_stdin = b"" if signal else b"stdin\0\xff\xfe\n"
                assert fields == [value, b"image-static", b"/work", b"nobody", b"nobody", cwd, expected_stdin], fields
                assert (output / "stderr.bin").read_bytes() == b"stderr\0\xff\xfe\n"
                if not signal:
                    location = raw_child if cwd != b"/work" else os.fsencode(project)
                    assert Path(os.fsdecode(location + b"/observed-\xff.bin")).read_bytes() == b"file\0\xff\xfe\n"
        ownership["containers"].append(cid)
        ownership_file.write_text(json.dumps(ownership, indent=2))
        if mode in ("lost-create", "create-conflict", "failed-delete", "orphan"):
            inspect = json.loads(checked(["docker", "inspect", cid]))[0]
            assert inspect["HostConfig"]["Init"] is True
            assert inspect["Config"]["User"] == "65534:65534"
            assert "no-new-privileges=true" in inspect["HostConfig"]["SecurityOpt"]
            mounts = [mount for mount in inspect["Mounts"] if mount["Destination"].startswith("/.marsh-bytes-")]
            assert len(mounts) == 2 and all(not mount["RW"] for mount in mounts)
            assert all(Path(mount["Source"]).is_file() for mount in mounts), "live bind source deleted on uncertain result"
            for mount in mounts:
                carrier_dirs.add(Path(mount["Source"]).parent)
            row["retained_after_driver_exit"] = True
            assert reclaim(source, name + "-referenced") == 0
            for directory in {Path(mount["Source"]).parent for mount in mounts}:
                assert (directory / "container").read_text() == cid, "actual attempt identity must survive worker restart"
            # A second exact owned container can keep the source live even when
            # the original ID/name has gone. It is NOT removed by reclamation.
            reference = checked(["docker", "create", "--name", args.namespace + "-reference-" + name,
                "--network=none", "--mount", "type=bind,source=" + mounts[0]["Source"] + ",target=/held-source,readonly",
                image, "wait-signal"]).strip().decode("ascii")
            ownership["containers"].append(reference)
            ownership_file.write_text(json.dumps(ownership, indent=2))
            delete_exact(cid)
            assert reclaim(source, name + "-other-reference") == 0
            assert all(Path(mount["Source"]).is_file() for mount in mounts)
            assert json.loads(checked(["docker", "inspect", reference]))[0]["Id"] == reference
            delete_exact(reference)
            assert reclaim(source, name + "-absent") == 1
            assert all(not Path(mount["Source"]).parent.exists() for mount in mounts)
            row["restart_reclaimed_after_complete_absence"] = True
        else:
            assert result["deleted"] is True
            assert not checked(["docker", "container", "ls", "-aq", "--no-trunc", "--filter", "id=" + cid]).strip()
            observed = json.loads((output / "container-inspect.json").read_text())
            mounts = [mount for mount in observed["Mounts"] if mount["Destination"].startswith("/.marsh-bytes-")]
            if name == "utf8-fast-path":
                assert not mounts, "UTF8 must not install native byte carriers"
                row["no_carrier_mounts"] = True
            else:
                expected_carrier = any(any(byte >= 128 for byte in word) for word in tail) or any(byte >= 128 for byte in value) or any(byte >= 128 for byte in cwd)
                assert len(mounts) == (2 if expected_carrier else 0)
            assert all(not Path(mount["Source"]).parent.exists() for mount in mounts)
            row["carrier_sources_absent"] = True
        assert not carrier_root.exists() or not list(carrier_root.iterdir()), "successful case left retained capacity"
        canary_alive()
        rows.append(row)
        (args.evidence / "cases.json").write_text(json.dumps(rows, indent=2))

    # Failure deliberately retains all ownership and test artifacts; there is
    # no broad finally-rm which could erase a live bind source or foreign VM.
    case("utf8-fast-path", [b"normal"], b"ascii")
    case("raw-tail-env", [b"tail", b"\xff\xfe", b"", bytes(range(1, 256))], bytes(range(1, 256)))
    case("image-default-cmd", [], b"\xff\xfe")
    case("cmd-only-default", [], b"\xff\xfe", job_image=cmd_image, expected_argv=[b"default-cmd"])
    case("cmd-only-tail", [b"/byte-fixture", b"\xff\xfe"], b"\xff", job_image=cmd_image, expected_argv=[b"\xff\xfe"])
    case("tail-not-an-entrypoint", [b"/bin/false", b"\xff"], b"\xff")
    case("source-command-at-old-helper-path", [b"\xff\xfe"], b"\xff", job_image=collision_image)
    case("raw-cwd", [b"cwd", b"\xff"], b"\xfe", cwd=b"/work/raw-\xff\xfe")
    case("signal", [b"wait-signal", b"\xff"], b"\xfe", signal=True)
    case("tty", [b"tty-probe", b"\xff\xfe"], b"\xfe", tty=True)
    for failure, code in [("missing", 127), ("denied", 126)]:
        case(failure + "-utf8", [b"ascii"], b"ascii", job_image=status_images[failure], status=code)
        case(failure + "-raw", [b"\xff"], b"\xfe", job_image=status_images[failure], status=code)
    case("foreign-platform-utf8", [b"ascii"], b"ascii", job_image=foreign_image, reject_platform=True)
    case("foreign-platform-raw", [b"\xff"], b"\xfe", job_image=foreign_image, reject_platform=True)
    case("orphan-restart", [b"\xff"], b"\xfe", mode="orphan")
    case("lost-create", [b"\xff"], b"\xfe", mode="lost-create")
    case("failed-delete", [b"\xff"], b"\xfe", mode="failed-delete")
    case("create-conflict", [b"\xff"], b"carrier-secret-canary-\xfe", mode="create-conflict")
    canary_alive()
    delete_exact(canary)
    # The product, not the harness, reclaimed every carrier using actual Docker
    # observations. No manual carrier unlink or global cache cleanup is allowed.
    assert all(not directory.exists() for directory in carrier_dirs)
    assert not carrier_root.exists() or not list(carrier_root.iterdir())
    if carrier_root.exists():
        carrier_root.rmdir()
    worker_home.rmdir()
    for location in [os.fsencode(project), raw_child]:
        marker = Path(os.fsdecode(location + b"/observed-\xff.bin"))
        if marker.exists():
            assert marker.is_file() and not marker.is_symlink()
            marker.unlink()
    os.rmdir(raw_child)
    project.rmdir()
    project.parent.rmdir()
    for owned_tag in owned_tags:
        checked(["docker", "image", "rm", owned_tag])
    remaining_tags = set(checked(["docker", "image", "ls", "--format", "{{.Repository}}:{{.Tag}}"]).decode().splitlines())
    assert not remaining_tags.intersection(owned_tags)
    assert baseline_containers == set(checked(["docker", "container", "ls", "-aq", "--no-trunc"]).decode().split()), "foreign container baseline changed"
    result = {"passed": True, "qualification": "production-runtime component, public stock+Cloud still required", "source_image": args.image,
              "source_config_id": baseline["Id"], "fixture_image": image, "artifacts": artifacts, "build_proof": proof,
              "namespace": args.namespace, "cases": rows, "canary_deleted": True,
              "baseline_containers_preserved": sorted(baseline_containers), "owned_tags_absent": owned_tags,
              "owned_project_absent": not project.parent.exists(), "carrier_root_absent": not carrier_root.exists()}
    (args.evidence / "result.json").write_text(json.dumps(result, indent=2))
    print(json.dumps({"passed": True, "cases": len(rows), "evidence": str(args.evidence)}))


if __name__ == "__main__":
    main()
