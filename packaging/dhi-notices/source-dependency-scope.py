#!/usr/bin/env python3
"""Conservative source-only Rust dependency reachability for published Linux bins.

No Cargo/build scripts/toolchain execution. Includes ALL optional dependencies and
unknown cfgs, excludes dev-dependencies and only definitely foreign OS/arch cfgs.
This is not Cargo's resolved feature graph, linked-code proof or an attestation.
"""

import argparse
import hashlib
import json
from pathlib import Path, PurePosixPath
import re
import tomllib
from public_sources import members, read_input


def definitely_foreign(target):
    # Only elementary positive constraints are excluded. Compound cfgs remain
    # conservative unless a top-level all() requires a foreign OS/architecture.
    if not target.startswith("cfg("):
        return target not in {"aarch64-unknown-linux-musl", "x86_64-unknown-linux-musl"}
    if target in {
        "cfg(windows)",
        'cfg(target_os = "windows")',
        'cfg(target_os = "android")',
        'cfg(target_os = "macos")',
        'cfg(target_os = "ios")',
    }:
        return True
    return False


def configured_features(
    packages, manifests, workspaces, by_name, roots, arch, target_env="musl"
):
    """Cargo-declared normal/default feature fixed point, with unknowns explicit.

    This describes checked public manifests and lock identities, not arbitrary
    build.rs effects or the actual CI compiler/link result.
    """

    def target_value(expression):
        values = {
            "target_os": "linux",
            "target_arch": arch,
            "target_env": target_env,
            "target_family": "unix",
            "target_pointer_width": "64",
            "target_endian": "little",
            "target_vendor": "unknown",
        }
        text = expression.strip()
        if not text.startswith("cfg("):
            return (
                text
                == ("aarch64" if arch == "aarch64" else "x86_64")
                + "-unknown-linux-"
                + target_env
            )
        tokens = re.findall(r'[A-Za-z_][A-Za-z_0-9]*|"[^"\n]*"|[(),=]', text)
        position = 0

        def parse():
            nonlocal position
            word = tokens[position]
            position += 1
            if (
                word in {"cfg", "all", "any", "not"}
                and position < len(tokens)
                and tokens[position] == "("
            ):
                position += 1
                args = []
                while tokens[position] != ")":
                    args.append(parse())
                    if tokens[position] == ",":
                        position += 1
                position += 1
                if word == "cfg":
                    return args[0]
                if word == "not":
                    return None if args[0] is None else not args[0]
                if word == "all":
                    return False if False in args else None if None in args else True
                return True if True in args else None if None in args else False
            if position < len(tokens) and tokens[position] == "=":
                position += 1
                value = tokens[position].strip('"')
                position += 1
                return values[word] == value if word in values else None
            return {"unix": True, "windows": False}.get(word)

        try:
            return parse()
        except (IndexError, ValueError):
            return None

    requested = {}
    pending = []
    dependencies = {}
    unresolved = []
    unknown_cfg = []
    for root in roots:
        for key in by_name.get(root, []):
            requested.setdefault(key, set()).add("default")
            pending.append(key)
    while pending:
        key = pending.pop()
        if key not in manifests:
            continue
        doc = manifests[key]
        deps = dict(doc.get("dependencies", {}))
        non_normal_aliases = set(doc.get("build-dependencies", {})) | set(
            doc.get("dev-dependencies", {})
        )
        for table in doc.get("target", {}).values():
            non_normal_aliases.update(table.get("dependencies", {}))
            non_normal_aliases.update(table.get("build-dependencies", {}))
            non_normal_aliases.update(table.get("dev-dependencies", {}))
        for cfg, table in doc.get("target", {}).items():
            value = target_value(cfg)
            if value is False:
                continue
            if value is None:
                unknown_cfg.append({"package": list(key), "cfg": cfg})
            deps.update(table.get("dependencies", {}))
        normalized = {}
        for alias, spec in deps.items():
            if isinstance(spec, str):
                spec = {"version": spec}
            if spec.get("workspace"):
                base = workspaces[key].get("dependencies", {}).get(alias, {})
                if isinstance(base, str):
                    base = {"version": base}
                spec = {
                    **base,
                    **spec,
                    "features": list(
                        set(base.get("features", []) + spec.get("features", []))
                    ),
                }
            normalized[alias] = spec
        features = doc.get("features", {})
        enabled = {n for n, s in normalized.items() if not s.get("optional", False)}
        weak = {}
        strong = {}
        todo = list(requested[key])
        expanded = set()
        while todo:
            token = todo.pop()
            if token in expanded:
                continue
            expanded.add(token)
            if token.startswith("dep:"):
                enabled.add(token[4:])
            elif "/" in token:
                alias, feature = token.split("/", 1)
                if alias.endswith("?"):
                    weak.setdefault(alias[:-1], set()).add(feature)
                else:
                    enabled.add(alias)
                    strong.setdefault(alias, set()).add(feature)
            elif token in features:
                todo.extend(features[token])
            elif token in normalized and normalized[token].get("optional"):
                enabled.add(token)
            # 'default' absent means no default features; other absent tokens are
            # retained as unresolved, never silently claimed validated.
            elif token != "default" and token not in non_normal_aliases:
                unresolved.append({"parent": list(key), "unknown_feature": token})
        locked = {}
        for item in packages[key].get("dependencies", []):
            fields = item.split()
            choices = by_name.get(fields[0], [])
            if len(fields) > 1:
                choices = [k for k in choices if k[1] == fields[1]]
            locked.setdefault(fields[0], []).extend(choices)
        edges = []
        for alias in enabled:
            spec = normalized.get(alias)
            if spec is None:
                if alias not in non_normal_aliases:
                    unresolved.append(
                        {"parent": list(key), "unknown_dependency_alias": alias}
                    )
                continue
            name = spec.get("package", alias)
            choices = locked.get(name, [])
            if not choices:
                unresolved.append(
                    {"parent": list(key), "enabled_dependency_absent_from_lock": name}
                )
                continue
            if len(choices) > 1:
                # Narrow only simple published caret/exact numeric constraints.
                # Unsupported semver syntax remains a labelled over-approximation.
                constraint = spec.get("version", "")
                match = re.fullmatch(r"([=^]?)([0-9]+(?:\.[0-9]+){0,2})", constraint)
                if match:
                    prefix = tuple(map(int, match[2].split(".")))
                    lower = prefix + (0,) * (3 - len(prefix))
                    index = next((i for i, x in enumerate(lower) if x), len(prefix) - 1)
                    upper = lower[:index] + (lower[index] + 1,) + (0,) * (2 - index)
                    candidates = []
                    for child in choices:
                        if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", child[1]):
                            continue
                        v = tuple(map(int, child[1].split(".")))
                        if v == lower if match[1] == "=" else lower <= v < upper:
                            candidates.append(child)
                    if candidates:
                        choices = candidates
                if len(choices) > 1:
                    unresolved.append(
                        {
                            "parent": list(key),
                            "dependency": name,
                            "ambiguous_lock_versions": [list(x) for x in choices],
                        }
                    )
            selected = (
                set(spec.get("features", []))
                | strong.get(alias, set())
                | weak.get(alias, set())
            )
            if spec.get("default-features", spec.get("default_features", True)):
                selected.add("default")
            for child in choices:
                edges.append(child)
                if child not in requested or not selected.issubset(requested[child]):
                    requested.setdefault(child, set()).update(selected)
                    pending.append(child)
        dependencies[key] = sorted(set(edges))
    unique = lambda rows: list(
        {json.dumps(r, sort_keys=True): r for r in rows}.values()
    )
    return {
        "target": arch + "-unknown-linux-" + target_env,
        "root_packages": roots,
        "packages": [
            {
                "name": k[0],
                "version": k[1],
                "requested_features": sorted(v),
                "normal_dependencies": [list(x) for x in dependencies.get(k, [])],
            }
            for k, v in sorted(requested.items())
        ],
        "unknown_cfg": unique(unknown_cfg),
        "unresolved": unique(unresolved),
        "scope": "Fixed point of published normal/default feature declarations against exact source lock; includes unknown cfgs conservatively. Workspace dependency features are additive. Build/dev dependencies are separate, and release cargo commands do not pass --locked. This is source-configuration evidence, NOT compiler output, binary linkage or a verified attestation.",
    }


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--work", type=Path, required=True)
    p.add_argument("--output", type=Path, required=True)
    a = p.parse_args()
    cache = a.work / "downloads"
    manifest_cache = {}
    results = []
    for version, prefix, directory in [
        ("0.159.2", "codex", "codex-rust"),
        ("0.155.1", "0.155.1", "codex-0.155.1-rust"),
        ("0.156.1", "0.156.1", "codex-0.156.1-rust"),
    ]:
        lock_bytes = read_input(a.work / (prefix + "-lock"), 4 * 1024**2)
        lock = tomllib.loads(lock_bytes.decode())
        packages = {(x["name"], x["version"]): x for x in lock["package"]}
        by_name = {}
        for key in packages:
            by_name.setdefault(key[0], []).append(key)
        inventory = json.loads(read_input(a.work / directory / "inventory.json"))
        manifests = {}
        origins = {}
        workspaces = {}
        for archive in inventory["archives"]:
            url = archive["url"]
            path = cache / hashlib.sha256(url.encode()).hexdigest()
            expected = archive.get("expected_sha256") or archive.get(
                "acquisition", {}
            ).get("sha256")
            if not expected:
                from public_sources import digest

                expected = digest(path)
            if url not in manifest_cache:
                manifest_cache[url] = [
                    (
                        name,
                        tomllib.loads(data.decode()),
                        hashlib.sha256(data).hexdigest(),
                    )
                    for name, data, mode in members(path, expected)
                    if PurePosixPath(name).name == "Cargo.toml" and len(data) < 1024**2
                ]
            candidates = manifest_cache[url]
            for wanted in archive["packages"]:
                key = (wanted["name"], wanted["version"])
                matches = [
                    (name, doc, sha)
                    for name, doc, sha in candidates
                    if doc.get("package", {}).get("name") == key[0]
                ]
                if not matches:
                    continue
                # Actual package root, not its same-name fixtures/examples.
                name, doc, sha = min(
                    matches, key=lambda r: len(PurePosixPath(r[0]).parts)
                )
                parents = [
                    (n, d)
                    for n, d, h in candidates
                    if "workspace" in d
                    and PurePosixPath(name).is_relative_to(PurePosixPath(n).parent)
                ]
                ws = (
                    max(parents, key=lambda r: len(PurePosixPath(r[0]).parts))[1][
                        "workspace"
                    ]
                    if parents
                    else {}
                )
                manifests[key] = doc
                workspaces[key] = ws
                origins[key] = {
                    "archive": url,
                    "archive_sha256": expected,
                    "member": name,
                    "sha256": sha,
                }
        edges = {}
        build_edges = {}
        missing = []
        excluded = []
        for key, doc in manifests.items():
            normal = dict(doc.get("dependencies", {}))
            build = dict(doc.get("build-dependencies", {}))
            for cfg, block in doc.get("target", {}).items():
                if definitely_foreign(cfg):
                    excluded.append(
                        {
                            "parent": list(key),
                            "cfg": cfg,
                            "dependencies": list(block.get("dependencies", {})),
                        }
                    )
                    continue
                normal.update(block.get("dependencies", {}))
                build.update(block.get("build-dependencies", {}))
            locked = {}
            for item in packages[key].get("dependencies", []):
                fields = item.split()
                name = fields[0]
                choices = by_name.get(name, [])
                if len(fields) > 1:
                    choices = [x for x in choices if x[1] == fields[1]]
                locked.setdefault(name, []).extend(choices)

            def resolve(deps):
                selected = []
                for alias, spec in deps.items():
                    if isinstance(spec, str):
                        spec = {"version": spec}
                    if spec.get("workspace"):
                        spec = (
                            {
                                **workspaces[key]
                                .get("dependencies", {})
                                .get(alias, {}),
                                **spec,
                            }
                            if isinstance(
                                workspaces[key].get("dependencies", {}).get(alias), dict
                            )
                            else spec
                        )
                    name = spec.get("package", alias)
                    choices = locked.get(name, [])
                    if not choices:
                        # Cargo.lock may omit unused optional features; don't
                        # manufacture a version or pretend full resolution.
                        missing.append(
                            {
                                "parent": list(key),
                                "dependency": name,
                                "declaration": spec,
                            }
                        )
                        continue
                    selected.extend(choices)
                return sorted(set(selected))

            edges[key] = resolve(normal)
            build_edges[key] = resolve(build)
        roots = ["codex-cli", "codex-code-mode-host", "codex-bwrap", "codex-voice-host"]
        reach = {}
        for name in roots:
            starts = by_name.get(name, [])
            seen = set()
            pending = list(starts)
            while pending:
                key = pending.pop()
                if key in seen:
                    continue
                seen.add(key)
                pending.extend(edges.get(key, []))
            reach[name] = {
                "root_candidates": [list(k) for k in starts],
                "normal_optional_superset": [list(k) for k in sorted(seen)],
                "missing_manifests": [
                    list(k) for k in sorted(seen) if k not in manifests
                ],
                "unresolved_declarations": [
                    r for r in missing if tuple(r["parent"]) in seen
                ],
            }
        configured = [
            configured_features(
                packages,
                manifests,
                workspaces,
                by_name,
                ["codex-cli", "codex-code-mode-host", "codex-responses-api-proxy"],
                arch,
            )
            for arch in ["aarch64", "x86_64"]
        ]
        auxiliary = []
        for arch in ["aarch64", "x86_64"]:
            bwrap = configured_features(
                packages, manifests, workspaces, by_name, ["codex-bwrap"], arch
            )
            bwrap["build_system"] = "separate Cargo --bin bwrap release invocation"
            voice = configured_features(
                packages,
                manifests,
                workspaces,
                by_name,
                ["codex-voice-host"],
                arch,
                "gnu",
            )
            voice["build_system"] = (
                "Actual release builds voice with Bazel for GNU Linux. This Cargo-declaration graph is supporting source scope, not a claim of resolved Bazel features."
            )
            auxiliary.extend([bwrap, voice])
        results.append(
            {
                "version": version,
                "configured_normal_features": configured,
                "configured_auxiliary_features": auxiliary,
                "lock_sha256": hashlib.sha256(lock_bytes).hexdigest(),
                "roots": reach,
                "explicit_foreign_target_edges": excluded,
                "manifest_origins": {
                    "@".join(k): v for k, v in sorted(origins.items())
                },
                "root_feature_declarations": {
                    name: manifests[key].get("features", {})
                    for name in roots
                    for key in by_name.get(name, [])
                    if key in manifests
                },
                "scope": "Source normal-dependency reachability including all optional dependencies and unknown cfgs. Dev/build dependencies are not declared linked. Unresolved declarations/missing manifests remain explicit; no exact enabled-feature closure or CI execution/signature is asserted.",
            }
        )
    import os, stat

    for parent in a.output.absolute().parents:
        if not stat.S_ISDIR(parent.lstat().st_mode):
            raise ValueError("symlink/non-directory output ancestor")
    st = a.output.absolute().parent.lstat()
    if st.st_uid != os.getuid() or st.st_mode & 0o077:
        raise ValueError("output parent must be private")
    with a.output.open("x") as output:
        output.write(
            json.dumps(
                {
                    "schema": "marsh.source-linux-dependency-scope/v1",
                    "releases": results,
                },
                indent=2,
            )
            + "\n"
        )


if __name__ == "__main__":
    main()
