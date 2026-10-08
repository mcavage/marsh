#!/usr/bin/env python3
"""Real shipped verifier CLI controls, actual public npm bytes, no Docker/agent execution.

Linux user/mount namespaces isolate all fixed-path fixtures. Base binaries,
clipboard/Corepack and Claude are explicitly stubbed. Public npm payload,
canonical legal texts and root-observed image licence/SPDX bytes are NOT stubbed.
Every work directory and result is retained. This is not host-image evidence.
"""
import argparse
import ctypes
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys

HERE = Path(__file__).absolute().parent
sys.path.insert(0, str(HERE))
from public_sources import digest, members


def dump(path, value):
    path.write_text(json.dumps(value, indent=2) + "\n")


def source_order(text):
    # Docker instruction ordering, including multiple effects inside one RUN.
    logical = text.replace("\\\n", " ")
    instructions = [line.strip() for line in logical.splitlines() if line.strip() and not line.lstrip().startswith("#")]
    copies = [i for i, line in enumerate(instructions) if line.startswith("COPY ") and any(s in line for s in ("--from=claude", "--from=codex", "--from=rust"))]
    gates = [i for i, line in enumerate(instructions) if line.startswith("RUN ") and "verify.py --copied-agents --receipt" in line]
    if len(gates) != 1 or not copies or gates[0] <= max(copies):
        raise ValueError("final copied gate must follow all artifact/agent COPYs")
    gate = gates[0]
    suffix = instructions[gate].split("verify.py --copied-agents --receipt", 1)[1]
    if suffix.strip() or any(line.startswith(("RUN ", "COPY ", "ADD ")) for line in instructions[gate + 1:]):
        raise ValueError("payload effects after final copied gate")


def prepare(work, cache, architecture):
    bundle = work / "bundle"
    shutil.copytree(HERE / "collected", bundle, ignore=shutil.ignore_patterns("__pycache__", "image-verification.json", "base-image-verification.json"))
    acquisitions = json.loads((bundle / "source-records/public-acquisitions.json").read_text())
    npm = work / "npm-global"
    for archive in acquisitions["codex-" + architecture + "-npm"]:
        path = cache / hashlib.sha256(archive["url"].encode()).hexdigest()
        if digest(path) != archive["sha256"]:
            raise ValueError("public npm SHA256 mismatch")
        import base64
        if base64.b64encode(bytes.fromhex(digest(path, "sha512"))).decode() != archive["integrity"].split("-", 1)[1]:
            raise ValueError("public npm SHA512 mismatch")
        expected = {row["member"]: row for row in archive["members"]}
        for name, data, mode in members(path, archive["sha256"]):
            row = expected[name]
            if hashlib.sha256(data).hexdigest() != row["sha256"]:
                raise ValueError("public member mismatch")
            relative = Path(row["path"]).relative_to("/usr/local/share/npm-global")
            target = npm / relative
            target.parent.mkdir(mode=0o755, parents=True, exist_ok=True)
            target.write_bytes(data)
            target.chmod(mode)
    (npm / "bin").mkdir()
    (npm / "bin/codex").symlink_to("../lib/node_modules/@openai/codex/bin/codex.js")
    selected = work / "selected-kit-bin"
    selected.mkdir()
    for binary in ["codex", "codex-code-mode-host"]:
        archive = acquisitions["kit-" + architecture + "-" + binary]
        path = cache / hashlib.sha256(archive["url"].encode()).hexdigest()
        content = list(members(path, archive["sha256"]))
        if len(content) != 1 or hashlib.sha256(content[0][1]).hexdigest() != archive["members"][0]["sha256"]:
            raise ValueError("selected Kit archive correspondence failed")
        (selected / binary).write_bytes(content[0][1])
        (selected / binary).chmod(0o755)
    fakebin = work / "usr-local-bin"
    fakebin.mkdir()
    for name in ["clipboard-bridge", "claude"]:
        (fakebin / name).write_bytes(("FIXTURE-STUB " + name + "\n").encode())
        (fakebin / name).chmod(0o755)
    corepack = work / "corepack.cjs"
    corepack.write_bytes(b"FIXTURE-STUB Corepack\n")
    inventory_name = "installed-package-notices.json" if architecture == "arm64" else "installed-package-notices-amd64.json"
    inv = json.loads((bundle / inventory_name).read_text()); inv["binaries"] = []
    dump(bundle / inventory_name, inv)
    js = json.loads((bundle / "bundled-js-notices.json").read_text())
    for row in js:
        row.update(path=str(corepack), source_sha256=digest(corepack))
    dump(bundle / "bundled-js-notices.json", js)
    deltas = json.loads((bundle / "agent-payload-deltas.json").read_text())
    deltas["architectures"][architecture]["clipboard_variants"].append({"image": "FIXTURE-STUB-NOT-IMAGE", "sha256": digest(fakebin / "clipboard-bridge")})
    dump(bundle / "agent-payload-deltas.json", deltas)
    obligations = json.loads((bundle / "payload-obligations.json").read_text())
    claude = obligations["agents"][architecture]["claude"]["binaries"][0]
    claude.update(sha256=digest(fakebin / "claude"), size=(fakebin / "claude").stat().st_size)
    opt = work / "opt"
    opt.mkdir()
    import base64
    primary = json.loads((bundle / "source-records/public-image-primary-receipt.json").read_text())
    primary_files = {r["path"]: r for r in primary["files"]}
    for row in obligations["image_files"]:
        path = opt / Path(row["path"]).relative_to("/opt")
        path.parent.mkdir(parents=True, exist_ok=True)
        data = base64.b64decode(primary_files[row["path"]]["base64"], validate=True)
        if hashlib.sha256(data).hexdigest() != row["sha256"]:
            raise ValueError("actual root-provided primary legal bytes changed")
        path.write_bytes(data)
    dump(bundle / "payload-obligations.json", obligations)
    stub = work / "stubbin"
    stub.mkdir()
    (stub / "pkgs").write_text("".join(r["package"] + "\t" + r["version"] + "\n" for r in inv["packages"]))
    (stub / "dpkg").write_text('#!/bin/sh\nprintf "%s\\n" "${FIXTURE_ARCH:-' + architecture + '}"\n')
    (stub / "dpkg-query").write_text('#!/bin/sh\nexec /bin/cat "' + str(stub / "pkgs") + '"\n')
    for name in ["dpkg", "dpkg-query"]:
        (stub / name).chmod(0o755)
    dump(work / "fixture-scope.json", {"verifier_sha256": digest(bundle / "verify.py"), "architecture": architecture,
        "actual_public_npm": acquisitions["codex-" + architecture + "-npm"],
        "stubs": ["base binaries omitted", "dpkg package query", "Corepack", "clipboard", "copied Claude", "base-only uid/gid normalized to0 for selected Kit positive (one-ID user namespace)"],
        "actual_primary_image_legal_files": [r["sha256"] for r in primary["files"]],
        "scope": "Real unmodified verifier subprocess with real npm bytes in isolated mount namespace; NOT an actual Docker image or legal completeness proof"})


def inside(work, architecture):
    libc = ctypes.CDLL(None, use_errno=True)
    if libc.mount(b"none", b"/", None, (1 << 18) | 16384, None):
        raise OSError(ctypes.get_errno(), "private mount namespace failed")
    share = work / "usr-local-share"
    share.mkdir()
    (share / "npm-global").mkdir()
    for src, dst in [(share, "/usr/local/share"), (work / "npm-global", "/usr/local/share/npm-global"), (work / "usr-local-bin", "/usr/local/bin"), (work / "opt", "/opt")]:
        if libc.mount(str(src).encode(), dst.encode(), None, 4096, None):
            raise OSError(ctypes.get_errno(), "fixture bind failed: " + dst)
    bundle = work / "bundle"
    env = {"PATH": str(work / "stubbin") + ":/usr/bin:/bin", "LANG": "C.UTF-8"}
    args = [sys.executable, "-I", "-S", str(bundle / "verify.py"), "--copied-agents", "--receipt"]
    outcomes = []

    def call(name, expected, *, environment=None):
        process = subprocess.run(args, env=environment or env, capture_output=True, timeout=120)
        (work / (name + ".stdout")).write_bytes(process.stdout)
        (work / (name + ".stderr")).write_bytes(process.stderr)
        success = process.returncode == 0
        outcome = {"control": name, "argv": list(args), "exit": process.returncode, "expected_success": expected,
                   "as_expected": success == expected, "stdout_sha256": hashlib.sha256(process.stdout).hexdigest(),
                   "stderr_sha256": hashlib.sha256(process.stderr).hexdigest(), "verifier_sha256": digest(bundle / "verify.py")}
        if success:
            receipt = json.loads(process.stdout)
            if receipt["verifier_sha256"] != digest(bundle / "verify.py") or len(receipt["bundle_tree_sha256"]) != 64:
                raise AssertionError("receipt source fence absent")
        outcomes.append(outcome)
        print(json.dumps(outcome), flush=True)
        return success

    if not call("positive-real-npm", True):
        dump(work / "results.json", outcomes)
        return 1
    index = bundle / "agent-notices.json"
    original = index.read_bytes(); document = json.loads(original)
    removed = {}
    for row in document["notices"]:
        if row["file"].startswith("codex-0.159.2-"):
            path = bundle / "provider-notices" / row["file"]
            removed[path] = path.read_bytes(); path.unlink()
    document["notices"] = [r for r in document["notices"] if not r["file"].startswith("codex-0.159.2-")]
    dump(index, document)
    call("N1-delete-codex-notices-and-index", False)
    index.write_bytes(original)
    for path, data in removed.items():path.write_bytes(data)
    native = next((work / "npm-global").glob("lib/node_modules/@openai/codex/node_modules/@openai/*/vendor/*/bin/codex"))
    for name, mode in [("N2-setuid",0o4755),("N2b-setgid",0o2755),("N2c-no-exec",0o644)]:
        native.chmod(mode);call(name,False);native.chmod(0o755)
    # UID changes are unavailable in a one-ID unprivileged user namespace;
    # mutate expected ownership instead, still exercising actual lstat comparison.
    document = json.loads(original)
    for row in document["agents_by_architecture"][architecture]["codex"]["payload_entries"]:
        if row["path"].endswith("/bin/codex"):row["mode"] = 0o4755
    dump(index,document);call("N2d-metadata-expectation-not-ignored",False);index.write_bytes(original)
    extra = work / "npm-global/lib/node_modules/unreviewed-extra"
    extra.mkdir();(extra / "index.js").write_text("not executed\n")
    link = work / "npm-global/bin/unreviewed";link.symlink_to("../lib/node_modules/unreviewed-extra/index.js")
    call("N3-extra-package-and-binlink",False)
    link.unlink();(extra / "index.js").unlink();extra.rmdir()
    launcher = work / "npm-global/lib/node_modules/@openai/codex/bin/codex.js"
    data = launcher.read_bytes();launcher.write_bytes(data+b"\n// corruption\n")
    call("N4-tampered-launcher",False);launcher.write_bytes(data)
    extra = launcher.parent / "unreviewed";extra.write_bytes(b"extra")
    call("N5-extra-package-file",False);extra.unlink()
    package = work / "npm-global/lib/node_modules/@openai"
    hidden = work / "hidden-openai";package.rename(hidden)
    call("N6-missing-codex",False)
    saved_args=list(args)
    args[:]=[sys.executable,'-I','-S',str(bundle/'verify.py'),'--expect-agent','codex']
    call('expected-base-marker-missing-denied',False)
    assert b'expected public base agent identity marker' in (work/'expected-base-marker-missing-denied.stderr').read_bytes()
    args[:]=saved_args
    hidden.rename(package)
    claude = work / "usr-local-bin/claude";real = claude.with_name("claude.real")
    claude.rename(real);claude.symlink_to("claude.real")
    call("N7-symlink-claude",False);claude.unlink();real.rename(claude)
    call("N9-unsupported-architecture",False,environment=dict(env,FIXTURE_ARCH="riscv64"))
    receipt = bundle / "image-verification.json"
    receipt.rename(work / "retained-positive-receipt.json")
    sentinel = work / "receipt-sentinel";sentinel.write_bytes(b"must remain\n")
    receipt.symlink_to(sentinel)
    call("receipt-symlink-denied",False)
    assert sentinel.read_bytes()==b"must remain\n"
    receipt.unlink()
    call("restored-positive",True)
    args.extend(['--expect-agent','codex'])
    call('explicit-base-flavor-positive',True)
    del args[-2:]
    spdx = work / "opt/docker/sbom/clipboard-bridge/.spdx.clipboard-bridge.json"
    data = spdx.read_bytes();spdx.write_bytes(data+b"\ncorrupt\n")
    call("primary-SPDX-corruption-denied",False);spdx.write_bytes(data)
    image_license = work / "opt/docker/.license.txt"
    saved_license = work / "saved-image-license";image_license.rename(saved_license)
    call("primary-image-license-missing-denied",False);saved_license.rename(image_license)
    primary_index = bundle / "primary-source-notices.json"
    primary_original = primary_index.read_bytes();primary_doc=json.loads(primary_original)
    primary_doc["image_reconciliation"] = str(work / "must-not-read.json")
    sentinel_data = b'{"sentinel":"NOT-READ-BY-VERIFIER"}'
    (work / "must-not-read.json").write_bytes(sentinel_data);dump(primary_index,primary_doc)
    call("primary-reconciliation-path-escape-denied",False)
    assert b"NOT-READ-BY-VERIFIER" not in (work / "primary-reconciliation-path-escape-denied.stdout").read_bytes()
    primary_index.write_bytes(primary_original)
    v8_license = bundle / "texts/1c6356fb751d45f0c53093ebf8a7f5e580e802f51999178e19d60f3ec39e147d.txt"
    saved_v8 = work / "saved-v8-license";v8_license.rename(saved_v8)
    call("actual-V8-license-missing-denied",False);saved_v8.rename(v8_license)
    # An AMD-only fixture notice must be accepted on ARM too: bundle coverage
    # is the architecture union, not accidental per-platform text equality.
    amd_index = bundle / "installed-package-notices-amd64.json"
    amd_original = amd_index.read_bytes()
    amd_document = json.loads(amd_original)
    text = b"FIXTURE ONLY: architecture-specific notice union control\n"
    text_hash = hashlib.sha256(text).hexdigest()
    text_path = bundle / "texts" / (text_hash + ".txt")
    text_path.write_bytes(text)
    amd_document["packages"][0]["notices"].append({"file": "texts/" + text_path.name, "sha256": text_hash})
    dump(amd_index, amd_document)
    call("architecture-only-text-union",True)
    amd_index.write_bytes(amd_original);text_path.unlink()
    index.write_bytes(b'{"schema":1,"schema":2}')
    call("duplicate-index-key-denied",False);index.write_bytes(original)
    provider = bundle / "provider-notices/codex-0.159.2-LICENSE"
    saved_provider = work / "saved-provider-text"
    provider.rename(saved_provider);provider.symlink_to(saved_provider)
    call("provider-symlink-denied",False);provider.unlink();saved_provider.rename(provider)
    # Real selected 0.155.1 native bytes, without executing them. Base uid/gid
    # must be normalized only in this fixture because one-ID user namespaces
    # cannot manufacture the public image's uid1000 while copied entries use0.
    args[:] = [sys.executable, "-I", "-S", str(bundle / "verify.py"), "--profile", "codex-kit", "--version", "0.155.1", "--receipt"]
    for binary in ["codex", "codex-code-mode-host"]:
        shutil.copy2(work / "selected-kit-bin" / binary, work / "usr-local-bin" / binary)
    call("base-wrong-owner-denied", False)
    document = json.loads(original)
    for row in document["agents_by_architecture"][architecture]["codex"]["payload_entries"]:
        row.update(uid=0, gid=0)
    dump(index, document)
    call("selected-kit-0.155.1-real-binaries", True)
    args[args.index("0.155.1")] = "0.159.2"
    call("unsupported-repin-no-upgrade", False)
    args[args.index("0.159.2")] = "0.155.1"
    selected_binary = work / "usr-local-bin/codex"
    selected_binary.chmod(0o4755)
    call("selected-kit-setuid-denied", False)
    selected_binary.chmod(0o755)
    call("selected-kit-restored-positive", True)
    index.write_bytes(original)
    dump(work / "results.json",outcomes)
    return int(not all(o["as_expected"] for o in outcomes))


def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument("--cache",type=Path)
    p.add_argument("--work",type=Path,required=True)
    p.add_argument("--architecture",choices=["arm64","amd64"],default="arm64")
    p.add_argument("--inside",action="store_true")
    a=p.parse_args();work=a.work.absolute()
    if a.inside:return inside(work,a.architecture)
    work.mkdir(mode=0o700,parents=True,exist_ok=False)
    prepare(work,a.cache.absolute(),a.architecture)
    recipe=(HERE.parent/'shell/Dockerfile').read_text()
    order={"actual_recipe_sha256":hashlib.sha256(recipe.encode()).hexdigest()}
    try:source_order(recipe);order["current_pass"]=True
    except ValueError as e:order.update(current_pass=False,error=str(e))
    # These mutations must fail even when the current recipe is repaired.
    valid='COPY --from=claude a /a\nCOPY --from=codex b /b\nRUN smoke\nRUN python3 verify.py --copied-agents --receipt\nUSER agent\n'
    source_order(valid)
    for invalid in [valid+'COPY injected /usr/local/bin/codex\n',valid.replace('--receipt','--receipt && claude --version'),valid.replace('RUN python3','COPY --from=rust c /c\nRUN python3')+'RUN chmod 4755 /usr/local/bin/codex\n']:
        try:source_order(invalid)
        except ValueError:pass
        else:raise AssertionError('source-order negative accepted')
    dump(work/'source-order.json',order)
    command=['unshare','-rm',sys.executable,'-I',str(Path(__file__).absolute()),'--inside','--work',str(work),'--architecture',a.architecture]
    result=subprocess.run(command,timeout=1800)
    dump(work/'runner.json',{'argv':command,'exit':result.returncode,'source_order':order})
    return result.returncode or int(not order["current_pass"])


if __name__=='__main__':
    raise SystemExit(main())
