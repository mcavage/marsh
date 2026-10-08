#!/usr/bin/env python3
"""Repair reviewed image dependencies from checksum-pinned upstream archives.

Build-time only: run as root with isolated Python. No package lifecycle scripts
or credentials are used. Every archive member is validated before replacement.
"""
import argparse
import ast
import base64
import csv
import hashlib
import io
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import subprocess
import tarfile
import tempfile
import urllib.request
import zipfile

HERE = Path(__file__).resolve().parent
NOTICE = Path("/usr/local/share/licenses/marsh-image-repair")
LIMIT = 16 * 1024 * 1024


def digest(data):
    return hashlib.sha256(data).hexdigest()


def checked_directory(path):
    if not path.is_dir() or path.is_symlink() or path.resolve() != path:
        raise ValueError("unexpected dependency directory: " + str(path))


def tree_hash(path):
    hasher = hashlib.sha256()
    for file in sorted(path.rglob("*")):
        if file.is_file() and "__pycache__" not in file.parts:
            hasher.update(str(file.relative_to(path)).encode() + b"\0" + file.read_bytes())
    return hasher.hexdigest()


def fetch(asset, cache):
    if cache:
        data = (cache / asset["filename"]).read_bytes()
    else:
        with urllib.request.urlopen(asset["url"], timeout=60) as response:
            data = response.read(LIMIT + 1)
    if len(data) > LIMIT or digest(data) != asset["sha256"]:
        raise ValueError("artifact size or checksum mismatch: " + asset["filename"])
    return data


def member_path(name, prefix):
    path = PurePosixPath(name)
    if path.is_absolute() or ".." in path.parts or "\\" in name:
        raise ValueError("unsafe archive member")
    if not path.parts or path.parts[0] != prefix:
        return None
    return Path(*path.parts[1:])


def unpack_npm(data):
    files = {}
    with tarfile.open(fileobj=io.BytesIO(data), mode="r:gz") as archive:
        for member in archive:
            relative = member_path(member.name, "package")
            if relative is None:
                raise ValueError("unexpected npm archive prefix")
            if member.isdir():
                continue
            if not member.isfile() or member.size > LIMIT or str(relative) in files:
                raise ValueError("unsupported npm archive member")
            files[str(relative)] = archive.extractfile(member).read()
    return files


def unpack_urllib3(data):
    files = {}
    license_texts = {}
    with zipfile.ZipFile(io.BytesIO(data)) as archive:
        for member in archive.infolist():
            path = PurePosixPath(member.filename)
            if path.is_absolute() or ".." in path.parts or "\\" in member.filename:
                raise ValueError("unsafe wheel member")
            if member.file_size > LIMIT or (member.external_attr >> 16) & 0o170000 == 0o120000:
                raise ValueError("unsupported wheel member")
            if member.is_dir():
                continue
            relative = member_path(member.filename, "urllib3")
            if relative is not None:
                if str(relative) in files:
                    raise ValueError("duplicate wheel member")
                files[str(relative)] = archive.read(member)
            elif "/licenses/" in member.filename:
                license_texts[path.name] = archive.read(member)
    # Apply pip's existing namespace adaptation to the upstream security release.
    # Each changed Python file must still parse, and no absolute urllib3/idna
    # import may remain in executable code.
    for name, data in list(files.items()):
        if not name.endswith(".py"):
            continue
        text = data.decode()
        text = re.sub(r"(?m)^(\s*)import (urllib3|idna)(?:\s*#.*)?$",
                      r"\1from pip._vendor import \2", text)
        for module, alias in [("urllib3.connection", "urllib3_connection"),
                              ("urllib3.contrib.pyopenssl", "pyopenssl")]:
            text = text.replace("import " + module, "import pip._vendor." + module + " as " + alias)
            text = text.replace(module + ".", alias + ".")
        for node in ast.walk(ast.parse(text)):
            if isinstance(node, ast.Import) and any(n.name.split(".")[0] in ("urllib3", "idna") for n in node.names):
                raise ValueError("unadapted vendor import: " + name)
            if isinstance(node, ast.ImportFrom) and node.level == 0 and (node.module or "").split(".")[0] in ("urllib3", "idna"):
                raise ValueError("unadapted vendor import: " + name)
        files[name] = text.encode()
    files.update({"LICENSE-" + name: data for name, data in license_texts.items()})
    return files


def replace(path, files, component):
    checked_directory(path)
    if not files or sum(map(len, files.values())) > LIMIT:
        raise ValueError("empty or oversized dependency")
    with tempfile.TemporaryDirectory(prefix=".marsh-repair-", dir=path.parent) as temporary:
        stage = Path(temporary, "package")
        stage.mkdir()
        for name, data in files.items():
            file = stage / name
            file.parent.mkdir(parents=True, exist_ok=True)
            file.write_bytes(data)
            file.chmod(0o644)
            if re.match(r"(?i)^(?:license|licence|notice|copying)", file.name):
                notice = NOTICE / component / name
                notice.parent.mkdir(parents=True, exist_ok=True)
                notice.write_bytes(data)
        shutil.rmtree(path)
        stage.rename(path)


def urllib3_version(data):
    versions = [node.value.value for node in ast.parse(data).body
                if isinstance(node, ast.Assign) and isinstance(node.value, ast.Constant)
                and any(isinstance(target, ast.Name) and target.id == "__version__" for target in node.targets)]
    if len(versions) != 1 or versions[0] not in {"2.7.0", "2.8.0"}:
        raise ValueError("unreviewed pip vendored urllib3")
    return versions[0]


def vendor_metadata(name, data, asset):
    text = data.decode().replace("urllib3==2.7.0", "urllib3==2.8.0")
    if name.endswith(".json"):
        bom = json.loads(text)
        for component in bom.get("components", []):
            if component.get("name") == "urllib3":
                component["version"] = "2.8.0"
                component["hashes"] = [{"alg": "SHA-256", "content": asset["sha256"]}]
                component["description"] = "Upstream urllib3 2.8.0 wheel with marsh-recorded pip namespace adaptation; hash identifies upstream wheel."
        text = json.dumps(bom, indent=2).replace("urllib3@2.7.0", "urllib3@2.8.0") + "\n"
    return text.encode()


def repair_bootstrap_wheel(files, asset):
    path = Path("/usr/share/python-wheels/pip-26.2.1-py3-none-any.whl")
    if path.is_symlink() or path.resolve() != path or not path.is_file():
        raise ValueError("unexpected pip bootstrap wheel")
    original = path.read_bytes()
    entries = {}
    with zipfile.ZipFile(io.BytesIO(original)) as archive:
        for member in archive.infolist():
            name = member.filename
            relative = PurePosixPath(name)
            if (relative.is_absolute() or ".." in relative.parts or "\\" in name
                    or member.file_size > LIMIT or name in entries
                    or (member.external_attr >> 16) & 0o170000 == 0o120000):
                raise ValueError("unsafe pip bootstrap wheel")
            if not member.is_dir():
                entries[name] = archive.read(member)
    if sum(map(len, entries.values())) > LIMIT:
        raise ValueError("oversized pip bootstrap wheel")
    prefix = "pip/_vendor/urllib3/"
    before = urllib3_version(entries[prefix + "_version.py"])
    info = "pip-26.2.1.dist-info/"
    if any(name in entries for name in [info + "RECORD.jws", info + "RECORD.p7s"]):
        raise ValueError("cannot preserve a signature on a modified bootstrap wheel")
    entries = {name: data for name, data in entries.items() if not name.startswith(prefix)}
    entries.update({prefix + name: data for name, data in files.items()})
    for name in ["pip/_vendor/vendor.txt", "pip/_vendor/bom.cdx.json"]:
        if name in entries:
            entries[name] = vendor_metadata(name, entries[name], asset)
    record = info + "RECORD"
    entries.pop(record)
    output = io.StringIO(newline="")
    writer = csv.writer(output, lineterminator="\n")
    for name, data in sorted(entries.items()):
        encoded = base64.urlsafe_b64encode(hashlib.sha256(data).digest()).rstrip(b"=").decode()
        writer.writerow([name, "sha256=" + encoded, str(len(data))])
    writer.writerow([record, "", ""])
    entries[record] = output.getvalue().encode()
    with tempfile.NamedTemporaryFile(dir=path.parent, prefix=".marsh-pip-wheel-", delete=False) as temporary:
        stage = Path(temporary.name)
        try:
            with zipfile.ZipFile(temporary, "w", compression=zipfile.ZIP_DEFLATED) as archive:
                for name, data in sorted(entries.items()):
                    member = zipfile.ZipInfo(name, date_time=(1980, 1, 1, 0, 0, 0))
                    member.compress_type = zipfile.ZIP_DEFLATED
                    member.external_attr = 0o100644 << 16
                    archive.writestr(member, data)
            temporary.flush()
            os.fsync(temporary.fileno())
            stage.chmod(0o644)
            stage.replace(path)
        finally:
            stage.unlink(missing_ok=True)
    return {"path": str(path), "name": "pip bootstrap vendored urllib3", "before": before,
            "after": "2.8.0", "wheel_before_sha256": digest(original),
            "wheel_after_sha256": digest(path.read_bytes()), "record_entries": len(entries),
            "adaptation": "pip._vendor namespace imports; complete wheel RECORD regenerated"}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--artifacts-dir", type=Path)
    args = parser.parse_args()
    manifest = json.loads((HERE / "artifacts.json").read_text())
    assets = manifest["artifacts"]
    prepared = {name: fetch(asset, args.artifacts_dir) for name, asset in assets.items()}
    NOTICE.mkdir(parents=True, exist_ok=True)
    receipt = {"schema": 1, "manifest_sha256": digest((HERE / "artifacts.json").read_bytes()),
               "repair_script_sha256": digest(Path(__file__).read_bytes()), "components": []}
    targets = [(Path("/usr/lib/nodejs/npm/node_modules/brace-expansion"), "brace-expansion", {"5.0.5", "5.0.9", "5.0.12"}),
               (Path("/usr/lib/nodejs/npm/node_modules/undici"), "undici", {"6.26.0", "6.28.0", "6.28.1"}),
               (Path("/usr/lib/nodejs/npm/node_modules/ip-address"), "ip-address", {"10.2.0", "10.3.1", "10.7.1"})]
    for path, name, reviewed in targets:
        checked_directory(path)
        package = json.loads((path / "package.json").read_text())
        if package.get("name") != name or package.get("version") not in reviewed:
            raise ValueError("unreviewed npm dependency: " + str(path))
        files = unpack_npm(prepared[name])
        incoming = json.loads(files["package.json"])
        if incoming["name"] != name or incoming["version"] != assets[name]["version"]:
            raise ValueError("upstream package identity mismatch")
        replace(path, files, name)
        receipt["components"].append({"path": str(path), "name": name, "before": package["version"],
                                      "after": incoming["version"], "tree_sha256": tree_hash(path)})
    vendor = Path("/usr/lib/python3/dist-packages/pip/_vendor")
    path = vendor / "urllib3"
    checked_directory(path)
    before = urllib3_version((path / "_version.py").read_text())
    urllib3_files = unpack_urllib3(prepared["urllib3"])
    replace(path, urllib3_files, "urllib3")
    receipt["components"].append({"path": str(path), "name": "urllib3", "before": before,
                                  "after": "2.8.0", "tree_sha256": tree_hash(path),
                                  "adaptation": "pip._vendor namespace imports"})
    for metadata in [vendor / "vendor.txt", vendor / "bom.cdx.json"]:
        if metadata.exists():
            metadata.write_bytes(vendor_metadata(metadata.name, metadata.read_bytes(), assets["urllib3"]))
    receipt["components"].append(repair_bootstrap_wheel(urllib3_files, assets["urllib3"]))
    # Real installed resolution, not package-lock declarations.
    subprocess.run(["node", "-e", "for (const n of ['brace-expansion','undici','ip-address']) { const p='/usr/lib/nodejs/npm/node_modules/'+n; require(p); console.log(n,require(p+'/package.json').version); }"], check=True)
    subprocess.run(["python3", "-I", "-c", "from pip._vendor import urllib3; from pip._vendor.urllib3.util import parse_url; assert urllib3.__version__ == '2.8.0'; assert parse_url('https://example.com').host == 'example.com'"], check=True)
    subprocess.run(["npm", "--version"], check=True)
    subprocess.run(["python3", "-I", "-m", "pip", "--version"], check=True)
    # The public venv caller must not reinstall a vulnerable copy from ensurepip.
    with tempfile.TemporaryDirectory(prefix="marsh-repaired-venv-") as temporary:
        environment = Path(temporary) / "venv"
        subprocess.run(["python3", "-I", "-m", "venv", str(environment)], check=True)
        subprocess.run([str(environment / "bin/python"), "-I", "-c",
                        "from pip._vendor import urllib3; assert urllib3.__version__ == '2.8.0'"], check=True)
        subprocess.run([str(environment / "bin/python"), "-I", "-m", "pip", "--version"], check=True)
    shutil.copyfile(HERE / "artifacts.json", NOTICE / "artifacts.json")
    if (HERE / "base-images.json").exists():
        shutil.copyfile(HERE / "base-images.json", NOTICE / "base-images.json")
    (NOTICE / "repair-receipt.json").write_text(json.dumps(receipt, indent=2) + "\n")
    print(json.dumps(receipt, indent=2))


if __name__ == "__main__":
    main()
