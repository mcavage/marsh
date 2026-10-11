#!/usr/bin/env python3
"""Publish native Kit v3 sources from private, ignore-aware Git snapshots.

All selected descriptors, arguments and explicit files are checked and staged
before the first Buildx invocation. Buildx remains the full native Kit validator.
A later build/registry failure can leave pushed images, but never a partial
command registry. Requires scripts/requirements-publish.txt.
"""

import argparse
from contextlib import ExitStack
import importlib.util
import json
import os
from pathlib import Path
import re
import selectors
import stat
import subprocess
import sys
import tempfile
import time
import unicodedata
import uuid


DIGEST = re.compile(r"sha256:[0-9a-f]{64}\Z")
REPOSITORY = re.compile(r"[a-z0-9][a-z0-9.-]*(?::[0-9]{1,5})?/[a-z0-9][a-z0-9._/-]*\Z")
TAG = re.compile(r"[A-Za-z0-9_][A-Za-z0-9._-]{0,127}\Z")
ARGUMENT = re.compile(r"[A-Za-z][A-Za-z0-9_]*\Z")
RULES_PATH = Path(__file__).resolve().parents[1] / "crates/marsh-contracts/src/command_registry_rules.json"
# Explicit local loading preserves the historical importlib API without
# depending on caller cwd/PYTHONPATH or importing PyYAML in image helpers.
SHARED_PATH = Path(__file__).with_name("build_inputs.py")
_SHARED_SPEC = importlib.util.spec_from_file_location("marsh_build_inputs", SHARED_PATH)
_SHARED = importlib.util.module_from_spec(_SHARED_SPEC)
_SHARED_SPEC.loader.exec_module(_SHARED)
for _name in ("JSON_LIMIT", "FILE_LIMIT", "STAGE_LIMIT", "MAX_FILES", "parts",
              "directory", "regular_file", "read_file", "copy_regular",
              "file_record", "tree_inventory", "json_bytes", "sha"):
    globals()[_name] = getattr(_SHARED, _name)


def unique(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError(f"duplicate field: {key}")
        result[key] = value
    return result


def read_json(path):
    return json.loads(read_file(path), object_pairs_hook=unique)


def descriptor_document(path):
    try:
        import yaml
    except ImportError as error:
        raise ValueError("publisher requires PyYAML; install scripts/requirements-publish.txt in a Python venv") from error

    class Loader(yaml.SafeLoader):
        nodes = 0
        depth = 0

        def compose_node(self, parent, index):
            self.nodes += 1
            self.depth += 1
            if self.nodes > MAX_FILES or self.depth > 64 or self.check_event(yaml.AliasEvent):
                raise ValueError("Kit YAML exceeds bounds or uses aliases; use explicit declarations")
            try:
                return super().compose_node(parent, index)
            finally:
                self.depth -= 1

        def construct_mapping(self, node, deep=False):
            return unique((self.construct_object(key, deep=deep),
                           self.construct_object(value, deep=deep)) for key, value in node.value)

    try:
        document = yaml.load(read_file(path), Loader=Loader)
    except yaml.YAMLError as error:
        raise ValueError(f"invalid Kit YAML: {path.name}") from error
    if (not isinstance(document, dict) or document.get("schemaVersion") != "3"
            or document.get("kind") != "workload"
            or not isinstance(document.get("dockerfile"), str)):
        raise ValueError(f"expected native Kit v3 workload descriptor: {path.name}")
    return document


def descriptor_arguments(document, supplied):
    declared = document.get("args", {})
    if not isinstance(declared, dict) or len(declared) > 64:
        raise ValueError("Kit args must be a bounded mapping")
    if set(supplied) - set(declared):
        raise ValueError("unknown Kit argument; use descriptor argument names, not raw Docker build args")
    effective = {}
    for name, rule in declared.items():
        if (not isinstance(name, str) or not ARGUMENT.fullmatch(name)
                or not isinstance(rule, dict)
                or set(rule) - {"default", "pattern", "description", "buildArg"}):
            raise ValueError("unsupported native Kit argument declaration")
        if "buildArg" in rule and (not isinstance(rule["buildArg"], str)
                                   or not ARGUMENT.fullmatch(rule["buildArg"])):
            raise ValueError("invalid native Kit buildArg name")
        if "description" in rule and not isinstance(rule["description"], str):
            raise ValueError("invalid native Kit argument description")
        pattern = rule.get("pattern")
        if pattern is not None and (not isinstance(pattern, str) or len(pattern) > 4096):
            raise ValueError("invalid native Kit argument pattern")
        # Bound/type-check defaults and overrides, but leave pattern semantics
        # to the native frontend, which validates the EFFECTIVE value.
        values = ([rule["default"]] if "default" in rule else [])
        if name in supplied:
            values.append(supplied[name])
        elif "default" not in rule:
            raise ValueError(f"missing required Kit argument: {name}")
        for value in values:
            if (not isinstance(value, str) or len(value) > 4096
                    or any(c in value for c in "\x00\r\n")):
                raise ValueError(f"invalid Kit argument value: {name}")
            # Only the native frontend's Go regexp implementation decides
            # pattern admission. Python re is NOT that dialect (\\z, POSIX
            # classes, backreferences). Full cache-only builds validate native
            # semantics; this frontend has no supported validate subrequest.
        effective[name] = supplied[name] if name in supplied else rule["default"]
    return effective


def source_path(root, value):
    if not isinstance(value, str) or not value or len(value) > 4096:
        raise ValueError("Kit source must be a nonempty path")
    source = Path(os.path.abspath(root / value))
    if not source.is_relative_to(root):
        raise ValueError("Kit source must be a directory beneath --source-root")
    # Recover actual directory spelling by device/inode, not case-folding. On
    # case-sensitive volumes distinct names stay distinct; APFS aliases merge.
    current = root
    for component in source.relative_to(root).parts:
        with directory(current / component) as child:
            identity = (os.fstat(child).st_dev, os.fstat(child).st_ino)
        with directory(current) as parent:
            names = [entry.name for entry in os.scandir(parent)
                     if entry.is_dir(follow_symlinks=False)
                     and (entry.stat(follow_symlinks=False).st_dev,
                          entry.stat(follow_symlinks=False).st_ino) == identity]
        if len(names) != 1:
            raise ValueError("ambiguous Kit directory identity")
        current /= names[0]
    return current


def configs_by_source(root, selected, inputs):
    if not isinstance(inputs, dict) or len(inputs) > command_rules()["max_commands"]:
        raise ValueError("build inputs must map selected Kit source paths to configurations")
    configs = {}
    for spelling, config in inputs.items():
        source = source_path(root, spelling)
        if source not in selected:
            raise ValueError("build inputs name an unselected Kit source")
        if not isinstance(config, dict) or set(config) - {"args", "files"}:
            raise ValueError("build input fields must be args and files")
        arguments, files = config.get("args", {}), config.get("files", {})
        if (not isinstance(arguments, dict) or len(arguments) > 64
                or any(not ARGUMENT.fullmatch(key) or not isinstance(value, str)
                       for key, value in arguments.items())):
            raise ValueError("invalid descriptor arguments")
        if not isinstance(files, dict) or len(files) > MAX_FILES:
            raise ValueError("invalid explicit file mapping")
        for destination, origin in files.items():
            parts(destination)
            basename = unicodedata.normalize("NFKC", Path(destination).name).casefold()
            if basename == ".dockerignore" or basename.endswith(".dockerignore"):
                raise ValueError("explicit files cannot add Docker ignore controls")
            if not isinstance(origin, str) or not Path(origin).is_absolute():
                raise ValueError("explicit file origins must be absolute paths")
        normalized = {"args": arguments, "files": files}
        if source in configs and configs[source] != normalized:
            raise ValueError("conflicting build inputs for canonical Kit source aliases")
        configs[source] = normalized
    return configs


def unreachable_worktree_root(source):
    """The enclosing linked worktree whose gitdir is not reachable here, or None.

    A linked worktree's `.git` is a `gitdir:` file naming the main checkout's
    `.git/worktrees/NAME`. Inside the `marsh --dev` shell only the worktree
    itself is mounted, so Git cannot open it.
    """
    for directory in [Path(source).resolve(), *Path(source).resolve().parents]:
        marker = directory / ".git"
        if marker.is_dir():
            return None
        if marker.is_file():
            text = marker.read_text(errors="replace").strip()
            if not text.startswith("gitdir:"):
                return None
            gitdir = Path(text[len("gitdir:"):].strip())
            if not gitdir.is_absolute():
                gitdir = directory / gitdir
            return None if gitdir.is_dir() else directory
    return None


def git_files(source, *, untracked=False):
    environment = {key: value for key, value in os.environ.items() if not key.startswith("GIT_")}
    environment.update(GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL=os.devnull)
    options = ["-c", "core.fsmonitor=false", "-c", "core.excludesFile=/dev/null", "ls-files"]
    worktree = unreachable_worktree_root(source)
    if worktree is None:
        return _git_paths(["git", "-C", str(source), *options, *([] if untracked else ["--cached"]),
                           "--others", "--exclude-per-directory=.gitignore", "-z", "--", "."], environment)
    # A linked worktree whose repository is not mounted (marsh --dev): list the
    # same .gitignore-aware files against an empty private index. Only files that
    # are tracked yet ignored can differ from the host's staging.
    print(f"publish-kits: {worktree} is a linked worktree without its repository; "
          "listing .gitignore-aware files without the index", file=sys.stderr)
    with tempfile.TemporaryDirectory(prefix="marsh-kit-index-") as scratch:
        environment.update(GIT_DIR=scratch, GIT_WORK_TREE=str(worktree))
        subprocess.run(["git", "init", "-q", "--bare", scratch], check=True, env={
            key: value for key, value in environment.items() if key not in ("GIT_DIR", "GIT_WORK_TREE")},
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        return _git_paths(["git", "-C", str(source), *options, "--others",
                           "--exclude-per-directory=.gitignore", "-z", "--", "."], environment)


def _git_paths(command, environment):
    # Bound output while it is produced, not after communicate() has allocated
    # an arbitrary repository's complete file list. Only this child is killed.
    with subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                          env=environment) as child, selectors.DefaultSelector() as ready:
        ready.register(child.stdout, selectors.EVENT_READ)
        data = bytearray()
        count = 0
        deadline = time.monotonic() + 15
        try:
            while True:
                remaining = deadline - time.monotonic()
                if remaining <= 0 or not ready.select(remaining):
                    raise ValueError("Git input enumeration exceeded its deadline")
                block = os.read(child.stdout.fileno(), 65536)
                if not block:
                    break
                data.extend(block)
                count += block.count(0)
                if len(data) > MAX_FILES * 4097 or count > MAX_FILES:
                    raise ValueError("Git input enumeration count/byte limit exceeded")
            if child.wait(timeout=max(0.01, deadline - time.monotonic())):
                raise ValueError("Kit sources must be in a Git worktree for ignore-aware staging")
        finally:
            if child.poll() is None:
                child.kill()
                child.wait()
    paths = sorted(set(os.fsdecode(value) for value in bytes(data).split(b"\0") if value))
    if len(paths) > MAX_FILES:
        raise ValueError("Kit source file-count limit exceeded")
    for path in paths:
        parts(path)
    return paths


PREPARED_SCHEMA = "marsh.prepared-kit-inputs/v2"
BUNDLED = frozenset("kits/" + name for name in (
    "marsh-shell", "marsh-claude", "marsh-codex", "marsh-pi"))
FIXTURE = "tests/acceptance/fixture"
# One Kit per agent; each carries its ACP adapter's npm lock and notices.
COLLECTORS = frozenset("kits/" + name for name in (
    "marsh-claude", "marsh-codex", "marsh-pi"))


def admitted_inventory(source, budget=None):
    records = {}
    budget = budget if budget is not None else [STAGE_LIMIT]
    for relative in git_files(source):
        path = source / relative
        # Do not resurrect tracked deletions; every admitted special file fails.
        if not path.exists() and not path.is_symlink():
            continue
        records[relative] = file_record(path, budget=budget)
    return records


def canonical_inputs(root, selected, configs):
    """One canonical source expansion, only for exact bundled directory identities."""
    sources = {source: str(source.relative_to(root)) for source in selected}
    canonical = {}
    if any(name in BUNDLED or name == FIXTURE for name in sources.values()):
        for destination, origin in (("dhi-notices", root / "packaging/dhi-notices/collected"),
                                    ("image-repair", root / "packaging/image-repair")):
            inventory = tree_inventory(origin)
            if not inventory["files"]:
                raise ValueError(f"canonical tree is empty: {origin}")
            canonical[destination] = {"origin": str(origin), "inventory": inventory}
    if any(name in COLLECTORS for name in sources.values()):
        origin = root / "scripts/npm-notices.mjs"
        canonical["collect-notices.mjs"] = {"origin": str(origin), "file": file_record(origin)}
    expanded = {}
    for source, name in sources.items():
        config = configs.get(source, {"args": {}, "files": {}})
        files = dict(config["files"])
        reserved = []
        if name in BUNDLED or name == FIXTURE:
            reserved = ["dhi-notices", "image-repair"]
            if name in COLLECTORS:
                reserved.append("collect-notices.mjs")
        # Reserve entire namespaces (NFKC/case variants too), not only known files.
        for path in [*git_files(source), *files]:
            first = unicodedata.normalize("NFKC", parts(path)[0]).casefold()
            if first in reserved:
                raise ValueError(f"source/extra inputs cannot replace or add to canonical namespace: {path}")
        for destination in reserved:
            item = canonical[destination]
            if "file" in item:
                files[destination] = item["origin"]
            else:
                files.update({destination + "/" + relative: str(Path(item["origin"]) / relative)
                              for relative in item["inventory"]["files"]})
        if len(files) > MAX_FILES:
            raise ValueError("expanded explicit input file-count limit exceeded")
        expanded[source] = {"args": config["args"], "files": files}
    return expanded, canonical


def command_rules():
    return read_json(RULES_PATH)


def read_commands(path):
    data = read_file(path, command_rules()["max_document_bytes"])
    return json.loads(data, object_pairs_hook=unique)


def selected_sources(root, commands):
    rules = command_rules()
    if (not isinstance(commands, dict) or not commands or len(commands) > rules["max_commands"]
            or len(json.dumps(commands, separators=(",", ":")).encode()) > rules["max_document_bytes"]):
        raise ValueError("--commands exceeds shared command registry bounds")
    for command, source in commands.items():
        if (not isinstance(command, str) or not command or len(command.encode()) > rules["max_name_bytes"]
                or any(character not in rules["allowed_name_characters"] for character in command)
                or any(command.startswith(prefix) for prefix in rules["forbidden_name_prefixes"])
                or command in rules["reserved_names"] or not isinstance(source, str) or not source):
            raise ValueError("invalid or reserved Kit command name/reference under shared registry rules")
    return {command: source_path(root, spelling) for command, spelling in commands.items()}


def capture(root, selected, inputs, temporary):
    """Measure actual inputs AND copied bytes, then recheck the source inventory."""
    configs = configs_by_source(root, set(selected.values()), inputs)
    configs, canonical = canonical_inputs(root, set(selected.values()), configs)
    budget = [STAGE_LIMIT]
    source_records = {source: admitted_inventory(source, budget) for source in set(selected.values())}
    explicit = {source: {name: file_record(Path(origin), budget=budget) for name, origin in config["files"].items()}
                for source, config in configs.items()}
    plans = stage_all(set(selected.values()), configs, temporary)
    sources = {}
    for source, (context, descriptor, arguments) in plans.items():
        expected = dict(source_records[source])
        for name, record in explicit[source].items():
            if name in expected:
                raise ValueError("explicit file collides with Kit source")
            expected[name] = record
        expected = {name: {**record, "mode": 0o644 | (record["mode"] & 0o111)}
                    for name, record in expected.items()}
        actual = tree_inventory(context)["files"]
        if actual != expected:
            raise ValueError("staged bytes differ from admitted source inventory")
        if admitted_inventory(source) != source_records[source]:
            raise ValueError("Kit source changed during preparation/staging")
        if any(file_record(Path(configs[source]["files"][name])) != record
               for name, record in explicit[source].items()):
            raise ValueError("explicit/canonical input changed during preparation/staging")
        sources[str(source.relative_to(root))] = {
            "source_tree": sha(json_bytes(source_records[source])),
            "staged_tree": sha(json_bytes(actual)), "file_count": len(actual),
            "staged_bytes": sum(record["size"] for record in actual.values()),
            "arguments": arguments, "descriptor": descriptor.name,
            "descriptor_sha256": actual[descriptor.name]["sha256"],
        }
    if canonical_inputs(root, set(selected.values()),
                        configs_by_source(root, set(selected.values()), inputs))[1] != canonical:
        raise ValueError("canonical input tree changed during staging")
    return plans, {"sources": sources, "canonical": canonical}


def bindings_record(paths):
    return {str(Path(os.path.abspath(path))): file_record(path) for path in paths}


def shell_verifier_dependencies():
    root = Path(__file__).resolve().parents[1]
    return [root / "scripts" / name for name in
            ("image_observations.py", "owned_process.py", "build_inputs.py", "package_observations.py")] + [
                root / "tests/acceptance/provenance.py"]


def canonical_directory(path):
    path = Path(path)
    if not path.is_absolute() or ".." in path.parts or path.resolve(strict=True) != path:
        raise ValueError("source/evidence paths must be canonical absolute paths without aliases")
    with directory(path):
        pass
    return path


def verified_shell_image(image_file, *, source_tree, build_receipt=None):
    """Narrow consumer of image_observations' host-observed API; never helper-only JSON."""
    if build_receipt is None:
        raise ValueError("--shell-build-receipt is required with --shell-image")
    source_tree = canonical_directory(source_tree)
    image_file, build_receipt = Path(image_file), Path(build_receipt)
    for path in (image_file, build_receipt):
        canonical_directory(path.parent)
        if not path.is_absolute() or path.resolve(strict=True) != path:
            raise ValueError("image/receipt must use exact canonical observed paths, not aliases")
    # Precheck with bounded no-follow reads BEFORE host_only_path can create a
    # missing parent. The authoritative semantic verification is image_observations'.
    read_file(image_file)
    value = json.loads(read_file(build_receipt, 16 * 1024 * 1024), object_pairs_hook=unique)
    if (not isinstance(value, dict) or not isinstance(value.get("guest_artifacts"), str)
            or not isinstance(value.get("marsh"), str)):
        raise ValueError("observed build receipt lacks export paths")
    guest = canonical_directory(value["guest_artifacts"])
    host = canonical_directory(Path(value["marsh"]).parent)
    if any(build_receipt.is_relative_to(path) for path in (source_tree, guest, host)):
        raise ValueError("shell build receipt must be host-only outside source and exported artifacts")
    proof_path = image_file.with_name("shell-image.build.json")
    before = bindings_record([image_file, proof_path, build_receipt, *shell_verifier_dependencies()])
    # Explicit trusted module directories support importlib callers too.
    # Refuse preloaded modules from a different checkout instead of reusing a
    # same-named verifier injected by caller cwd/PYTHONPATH.
    dependencies = shell_verifier_dependencies()
    for path in dependencies:
        loaded = sys.modules.get(path.stem)
        if loaded is not None and Path(getattr(loaded, "__file__", "")).resolve() != path:
            raise ValueError("strong verifier dependency came from a different source tree")
    sys.path.insert(0, str(Path(__file__).resolve().parent))
    module = importlib.import_module("image_observations")
    reference, observed = module.verified_shell_image(
        image_file, source_tree=source_tree, build_receipt=build_receipt, require_publication=True)
    if bindings_record([Path(path) for path in before]) != before:
        raise ValueError("shell producer proof or shared verifier changed during verification")
    return reference, list(dict.fromkeys([*observed, *dependencies]))


def verify_shell_images(root, inputs, requests):
    """Re-run semantic candidate/artifact verification, not merely JSON hashing."""
    if not isinstance(requests, list) or len(requests) > command_rules()["max_commands"]:
        raise ValueError("invalid prepared shell image verification requests")
    configs = configs_by_source(root, {source_path(root, name) for name in inputs}, inputs)
    verified, bindings = set(), []
    for request in requests:
        if not isinstance(request, dict) or set(request) != {"source", "image_file", "source_tree", "build_receipt"}:
            raise ValueError("invalid prepared shell image request fields")
        if not all(isinstance(value, str) and value for value in request.values()):
            raise ValueError("invalid prepared shell image request paths")
        source = source_path(root, request["source"])
        if source not in configs or source in verified or Path(request["source_tree"]) != root:
            raise ValueError("shell image request must bind one selected source in the exact candidate tree")
        reference, paths = verified_shell_image(Path(request["image_file"]), source_tree=root,
                                                build_receipt=Path(request["build_receipt"]))
        if configs[source]["args"].get("shellImage") != reference:
            raise ValueError("prepared shellImage argument differs from observed published candidate")
        verified.add(source)
        bindings.extend(paths)
    for source, config in configs.items():
        if (str(source.relative_to(root)) == FIXTURE
                and "shellImage" in config["args"] and source not in verified):
            raise ValueError("shellImage override requires a strong prepared shell_images request")
    return list(dict.fromkeys(bindings))


def seal_prepared(root, commands_path, inputs, bindings=(), *, shell_images=()):
    """v2 interface for canonical preparers; no Docker or output effects."""
    selected = selected_sources(root, read_commands(commands_path))
    requests = list(shell_images)
    paths = [Path(__file__), SHARED_PATH, RULES_PATH, commands_path, *bindings,
             *verify_shell_images(root, inputs, requests)]
    before = bindings_record(paths)
    with tempfile.TemporaryDirectory(prefix="marsh-kit-prepare-") as name:
        _, state = capture(root, selected, inputs, Path(name).resolve())
    verify_shell_images(root, inputs, requests)
    if bindings_record(paths) != before:
        raise ValueError("preparation scripts/configuration changed")
    return {"schema": PREPARED_SCHEMA, "inputs": inputs,
            "receipt": {"bindings": before, "state": state, "shell_images": requests}}


def check_bindings(receipt):
    bindings = receipt["bindings"]
    if not isinstance(bindings, dict) or len(bindings) > 128:
        raise ValueError("invalid prepared source bindings")
    if bindings_record([Path(path) for path in bindings]) != bindings:
        raise ValueError("prepared source script/configuration changed; prepare again")


def check_output(path, selected, *, create=False):
    path = Path(os.path.abspath(path))
    if any(path.is_relative_to(source) for source in selected):
        raise ValueError("output must be outside all selected Kit sources")
    identities = set()
    for source in selected:
        with directory(source) as fd:
            info = os.fstat(fd)
            identities.add((info.st_dev, info.st_ino))
    # Lexical checks alone miss case aliases on case-insensitive APFS.
    for ancestor in path.parents:
        if ancestor.exists() or ancestor.is_symlink():
            with directory(ancestor) as fd:
                info = os.fstat(fd)
                if (info.st_dev, info.st_ino) in identities:
                    raise ValueError("output must be outside all selected Kit source identities")
    # Check the nearest existing parent without creating anything first.
    parent = path.parent
    while not parent.exists() and not parent.is_symlink():
        parent = parent.parent
    with directory(parent):
        pass
    if path.parent.exists():
        with directory(path.parent) as fd:
            info = os.fstat(fd)
            if info.st_uid != os.getuid() or info.st_mode & 0o077:
                raise ValueError("output directory must be owner-private (0700)")
    if path.exists() or path.is_symlink():
        with regular_file(path, JSON_LIMIT):
            pass
    if create:
        with directory(path.parent, create=True):
            pass
    return path


def check_output_inputs(path, selected, bindings, requests=(), *, inputs=None, canonical=None):
    """Never let a receipt/registry output overwrite the evidence it consumes."""
    protected = set(selected)
    consumed = [Path(source) for source in bindings]
    if inputs:
        for config in inputs.values():
            consumed.extend(Path(origin) for origin in config.get("files", {}).values())
    if canonical:
        if not isinstance(canonical, dict):
            raise ValueError("invalid canonical input inventory")
        for item in canonical.values():
            origin = Path(item["origin"])
            if "inventory" in item:
                protected.add(origin)
            else:
                consumed.append(origin)
    for request in requests:
        value = json.loads(read_file(Path(request["build_receipt"]), 16 * 1024 * 1024),
                           object_pairs_hook=unique)
        protected.update((Path(request["source_tree"]), Path(value["guest_artifacts"]),
                          Path(value["marsh"]).parent))
    path = check_output(path, protected)
    for source in consumed:
        if path == source or (path.exists() and os.path.samefile(path, source)):
            raise ValueError("output would overwrite a source/proof input; choose a separate host path")
    return path


def stage_all(selected, configs, temporary):
    plans = {}
    repositories = set()
    budget = [STAGE_LIMIT]
    for index, source in enumerate(sorted(selected)):
        if not re.fullmatch(r"[a-z0-9][a-z0-9._-]*", source.name) or source.name in repositories:
            raise ValueError("Kit directory names must be unique lowercase repository components")
        repositories.add(source.name)
        context = temporary / f"source-{index}"
        context.mkdir(mode=0o700)
        paths = git_files(source)
        for relative in paths:
            path = source / relative
            # Tracked deletions are not resurrected from the index.
            if not path.exists() and not path.is_symlink():
                continue
            copy_regular(path, context / relative, budget)
        descriptors = list(context.glob("*.yaml"))
        if len(descriptors) != 1:
            raise ValueError(f"expected one included Kit descriptor: {source}")
        descriptor = descriptors[0]
        document = descriptor_document(descriptor)
        dockerfile = document["dockerfile"].removeprefix("./")
        parts(dockerfile)
        read_file(context / dockerfile)
        config = configs.get(source, {"args": {}, "files": {}})
        arguments = descriptor_arguments(document, config["args"])
        if len(paths) + len(config["files"]) > MAX_FILES:
            raise ValueError("combined Kit file-count limit exceeded")
        for relative, origin in config["files"].items():
            copy_regular(Path(origin), context / relative, budget)
        # COPY preserves directory permissions. Keep only the outer temporary
        # parent private; descendants must be traversable by image users.
        context.chmod(0o755)
        for path in context.rglob("*"):
            if path.is_dir():
                path.chmod(0o755)
        plans[source] = (context, descriptor, arguments)
    return plans


def publish_registry(parent, name, values, *, immutable=False):
    encoded = json_bytes(values)
    if len(encoded) > JSON_LIMIT:
        raise ValueError("output JSON exceeds 4 MiB limit")
    temporary = ".commands-" + uuid.uuid4().hex + ".json"
    try:
        fd = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW,
                     mode=0o600, dir_fd=parent)
        with os.fdopen(fd, "wb") as stream:
            stream.write(encoded)
            stream.flush()
            os.fsync(stream.fileno())
        if immutable:
            # link is atomic O_EXCL publication of the completely fsync'd file.
            # No existing receipt is overwritten, even on an identical rerun.
            try:
                os.link(temporary, name, src_dir_fd=parent, dst_dir_fd=parent,
                        follow_symlinks=False)
            except FileExistsError:
                fd = os.open(name, _SHARED.FILE_FLAGS, dir_fd=parent)
                with os.fdopen(fd, "rb") as old:
                    info = os.fstat(old.fileno())
                    if (not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid()
                            or info.st_mode & 0o077 or info.st_nlink != 1
                            or old.read(JSON_LIMIT + 1) != encoded):
                        raise ValueError("existing content-addressed receipt differs or is uncontrolled")
            os.unlink(temporary, dir_fd=parent)
        else:
            os.replace(temporary, name, src_dir_fd=parent, dst_dir_fd=parent)
        os.fsync(parent)
    finally:
        try:
            os.unlink(temporary, dir_fd=parent)
        except FileNotFoundError:
            pass


def verify_output_parent(path, retained):
    with directory(path) as current:
        before, after = os.fstat(retained), os.fstat(current)
        if ((before.st_dev, before.st_ino) != (after.st_dev, after.st_ino)
                or after.st_uid != os.getuid() or after.st_mode & 0o077):
            raise ValueError("output directory identity or permissions changed during build")


def build(source, plan, temporary, output, phase):
    context, descriptor, arguments = plan
    metadata = temporary / f"{source.name}-{phase}.json"
    build_args = [item for key, value in sorted(arguments.items())
                  for item in ("--build-arg", f"{key}={value}")]
    subprocess.run(
        ["docker", "buildx", "build", "--platform", "linux/arm64",
         "--file", str(descriptor), "--output", output,
         "--metadata-file", str(metadata),
         *build_args, str(context)], check=True, timeout=1800,
    )
    return metadata


def run(args):
    prefix = (args.repository_prefix or "").rstrip("/")
    name_prefix = args.repository_name_prefix
    root = Path(os.path.abspath(args.source_root))
    with directory(root):
        pass
    selected = selected_sources(root, read_commands(args.commands))
    document = read_json(args.build_inputs) if args.build_inputs else {}
    prepared = isinstance(document, dict) and document.get("schema") == PREPARED_SCHEMA
    if isinstance(document, dict) and "schema" in document and not prepared:
        raise ValueError("unsupported prepared input schema; prepare again")
    if not prepared and any(str(source.relative_to(root)) in BUNDLED | {FIXTURE}
                            for source in selected.values()):
        raise ValueError("bundled Kits require prepare-kit-inputs.py v2 receipt")
    if prepared and set(document) != {"schema", "inputs", "receipt"}:
        raise ValueError("invalid prepared input document")
    inputs = document["inputs"] if prepared else document
    receipt = document["receipt"] if prepared else None
    configs_by_source(root, set(selected.values()), inputs)
    runtime_bindings = bindings_record([Path(__file__), SHARED_PATH, RULES_PATH, args.commands,
                                       *([args.build_inputs] if args.build_inputs else [])])
    if prepared:
        check_bindings(receipt)
        required_bindings = [Path(__file__), SHARED_PATH, RULES_PATH, args.commands]
        if any(str(source.relative_to(root)) in BUNDLED | {FIXTURE} for source in selected.values()):
            required_bindings.append(Path(__file__).with_name("prepare-kit-inputs.py"))
        for required in required_bindings:
            path = str(Path(os.path.abspath(required)))
            if receipt["bindings"].get(path) != file_record(Path(path)):
                raise ValueError("prepared receipt does not bind current publisher/commands")
        for path in verify_shell_images(root, inputs, receipt.get("shell_images", [])):
            if receipt["bindings"].get(str(path)) != file_record(path):
                raise ValueError("prepared receipt does not bind strong verifier/proof dependencies")
    if not args.validate_only:
        prospective = {command: f"{prefix}/{name_prefix}{source.name}@sha256:" + "0" * 64
                       for command, source in selected.items()}
        if len(json_bytes(prospective)) > command_rules()["max_document_bytes"]:
            raise ValueError("published command registry would exceed shared document byte limit")
        args.output = check_output_inputs(
            args.output, set(selected.values()),
            [*runtime_bindings, *(receipt["bindings"] if prepared else [])],
            receipt.get("shell_images", []) if prepared else [], inputs=inputs,
            canonical=receipt["state"]["canonical"] if prepared else None)
    with tempfile.TemporaryDirectory(prefix="marsh-kit-publish-") as temporary_name, ExitStack() as held:
        retained_output = None
        temporary = Path(temporary_name).resolve(strict=True)
        plans, state = capture(root, selected, inputs, temporary)
        if prepared and state != receipt["state"]:
            raise ValueError("prepared source/staged tree changed; prepare again before Docker")

        def recheck():
            if retained_output is not None:
                verify_output_parent(args.output.parent, retained_output)
            if bindings_record([Path(path) for path in runtime_bindings]) != runtime_bindings:
                raise ValueError("publisher/configuration changed during build")
            if prepared:
                check_bindings(receipt)
                verify_shell_images(root, inputs, receipt.get("shell_images", []))
            for source, (context, _, _) in plans.items():
                actual = sha(json_bytes(tree_inventory(context)["files"]))
                if actual != state["sources"][str(source.relative_to(root))]["staged_tree"]:
                    raise ValueError("private staged context changed during build")
            with tempfile.TemporaryDirectory(prefix="marsh-kit-recheck-") as name:
                _, current = capture(root, selected, inputs, Path(name).resolve())
            if current != state:
                raise ValueError("source/canonical tree changed during build; prepare again")

        # No contents/argument values: actionable paths before ANY remote send.
        # Include canonical origins too, once each rather than once per Kit.
        announced = {str(source / relative) for source in plans
                     for relative in git_files(source, untracked=True)}
        for item in state["canonical"].values():
            origin = Path(item["origin"])
            if "inventory" in item:
                announced.update(str(origin / relative) for relative in git_files(origin, untracked=True)
                                 if relative in item["inventory"]["files"])
            elif origin.name in git_files(origin.parent, untracked=True):
                announced.add(str(origin))
        for path in sorted(announced):
            print("Untracked build input (sent to builder; review before continuing): " +
                  json.dumps(path), file=sys.stderr, flush=True)
        for path in sorted({origin for config in inputs.values() for origin in config.get("files", {}).values()}):
            print("Explicit build input (sent to builder): " + json.dumps(path), file=sys.stderr, flush=True)
        recheck()
        if not args.validate_only:
            check_output(args.output, set(selected.values()), create=True)
            retained_output = held.enter_context(directory(args.output.parent))
            verify_output_parent(args.output.parent, retained_output)
        # The pinned frontend does NOT support --call=validate. Its native
        # validation is a full build: run EVERY cache-only build before ANY push.
        # This can execute Dockerfile instructions and change builder cache.
        for source, plan in plans.items():
            build(source, plan, temporary, "type=cacheonly", "check")
            print(f"Validated native Kit: {source.name}", flush=True)
        recheck()
        if args.validate_only:
            return
        check_output(args.output, set(selected.values()), create=True)
        with directory(args.output.parent) as output_parent:
            verify_output_parent(args.output.parent, output_parent)
            published = {}
            try:
                for source, plan in plans.items():
                    recheck()
                    repository = f"{prefix}/{name_prefix}{source.name}"
                    output = f"type=image,name={repository}:{args.tag},push=true"
                    if args.insecure_registry:
                        output += ",registry.insecure=true"
                    metadata = build(source, plan, temporary, output, "publish")
                    value = read_json(metadata)
                    digest = value.get("containerimage.digest") if isinstance(value, dict) else None
                    if not isinstance(digest, str) or not DIGEST.fullmatch(digest):
                        raise ValueError(f"Buildx did not report an immutable manifest digest for {source.name}")
                    published[source] = f"{repository}@{digest}"
                    print(f"{source.name}: {published[source]}", flush=True)
                recheck()
                verify_output_parent(args.output.parent, output_parent)
                registry = {command: published[source] for command, source in selected.items()}
                publication = {"schema": "marsh.kit-publication/v2", "registry": registry,
                               "registry_sha256": sha(json_bytes(registry)),
                               "prepared_document_sha256": runtime_bindings[str(Path(os.path.abspath(args.build_inputs)))]["sha256"] if prepared else None,
                               "bindings": runtime_bindings,
                               "prepared_receipt": receipt, "state": state,
                               "published": {str(source.relative_to(root)): ref for source, ref in published.items()}}
                # Immutable/content-addressed record FIRST, registry LAST. Failure
                # can leave an unreferenced receipt, never a new registry without
                # durable proof. Previous receipts are never replaced or deleted.
                receipt_name = args.output.name + ".publication-" + sha(json_bytes(publication))[7:] + ".json"
                publish_registry(output_parent, receipt_name, publication, immutable=True)
                verify_output_parent(args.output.parent, output_parent)
                publish_registry(output_parent, args.output.name, registry)
                print(f"Publication receipt: {args.output.parent / receipt_name}", flush=True)
                print(f"Pinned command registry: {args.output}", flush=True)
            except (ValueError, OSError, TypeError, KeyError, RecursionError, subprocess.SubprocessError) as error:
                raise ValueError(f"publication may have occurred (including :{args.tag} tag moves); "
                                 f"previous local registry retained until atomic commit: {error}") from error


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repository-prefix", help="registry/repository prefix without a tag or digest")
    parser.add_argument("--tag", default="release",
                        help="moving tag to push on each Kit image (default: release); consumers pin by digest")
    parser.add_argument("--commands", type=Path, default=Path("packaging/commands.json"))
    parser.add_argument("--source-root", type=Path, default=Path("."))
    parser.add_argument("--repository-name-prefix", default="",
                        help="prefix each Kit repository name; useful for unique flat Docker Hub repositories")
    parser.add_argument("--output", type=Path, default=Path("target/kit-release/commands.json"))
    parser.add_argument("--insecure-registry", action="store_true", help="allow an explicitly selected HTTP registry")
    parser.add_argument("--build-inputs", type=Path, help="nonsecret descriptor args and explicit file inputs keyed by Kit source")
    parser.add_argument("--validate-only", action="store_true", help="run full cache-only builds (may execute Dockerfile instructions); no push or registry write")
    args = parser.parse_args()
    try:
        if not TAG.fullmatch(args.tag):
            raise ValueError("--tag must be a valid image tag")
        if args.repository_name_prefix and not re.fullmatch(r"[a-z0-9][a-z0-9._-]{0,63}", args.repository_name_prefix):
            raise ValueError("--repository-name-prefix must be at most 64 lowercase repository-name characters")
        if not args.validate_only:
            if not args.output.name or args.output.name in (".", ".."):
                raise ValueError("--output must name a registry file")
            if not REPOSITORY.fullmatch((args.repository_prefix or "").rstrip("/")):
                raise ValueError("--repository-prefix must be registry/repository without a tag or digest")
        run(args)
    except (ValueError, OSError, TypeError, KeyError, RecursionError, subprocess.SubprocessError) as error:
        parser.exit(1, f"publish-kits: {error}\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
