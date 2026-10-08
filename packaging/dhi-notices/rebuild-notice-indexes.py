#!/usr/bin/env python3
"""Deterministic active-index migration from a retained seed + exact acquisitions.

Never edits the seed or acquisition evidence. Replaces source-code name matches
with actual leading permission excerpts (if any), refreshes source bindings and
current dispositions, and removes only unreferenced derived text in NEW output.
"""

import argparse
import copy
import hashlib
import json
from pathlib import Path
import shutil

from notice_rules import is_source_code, legal_header, excluded_material
from public_sources import private_output, read_input


def sha(data):
    return hashlib.sha256(data).hexdigest()


def encode(value):
    return (json.dumps(value, indent=2) + "\n").encode()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--seed", type=Path, required=True)
    parser.add_argument("--materials", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    output = private_output(args.output)
    seed_hashes = {}
    total = 0
    for path in sorted(args.seed.rglob("*")):
        if "__pycache__" in path.parts:
            continue
        if path.is_symlink():
            raise ValueError("symlink seed input")
        if not path.is_file():
            continue
        relative = str(path.relative_to(args.seed))
        data = read_input(path)
        total += len(data)
        if total > 256 * 1024**2 or len(seed_hashes) >= 20000:
            raise ValueError("seed bounds")
        seed_hashes[relative] = sha(data)
        target = output / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(data)
    vocabulary = read_input(Path(__file__).with_name("collected") / "notice-rules.json")
    (output / "notice-rules.json").write_bytes(vocabulary)
    changes = []
    source_names = {
        "0.159.2": "codex-rust",
        "0.155.1": "codex-0.155.1-rust",
        "0.156.1": "codex-0.156.1-rust",
    }
    for version, name in source_names.items():
        regenerated = args.materials / ("rust-v2-" + version)
        document = json.loads(read_input(regenerated / "inventory.json"))
        if document["failures"]:
            raise ValueError("failed Rust regeneration")
        (output / "source-records" / (name + ".json")).write_bytes(encode(document))
        for text in (regenerated / "texts").iterdir():
            destination = output / "texts" / text.name
            if destination.exists() and destination.read_bytes() != text.read_bytes():
                raise ValueError("text hash collision")
            destination.write_bytes(text.read_bytes())

    # Completion is an actual fresh collector result, not a retyping of old
    # excerpts with a second header parser. Keep its exact origin/hash metadata.
    completed = args.materials / "exact-completion"
    completion_name = "source-records/primary-exact-crate-declarations-and-headers.json"
    (output / completion_name).write_bytes(read_input(completed / "inventory.json"))
    for text in (completed / "texts").iterdir():
        (output / "texts" / text.name).write_bytes(read_input(text))

    # Embedded skills are source assets, not test fixtures. Bind each selected
    # release separately even when two releases ship byte-identical licences.
    provider_path = output / "agent-notices.json"
    providers = json.loads(read_input(provider_path))
    profiles_path = output / "derived-profiles.json"
    profiles = json.loads(read_input(profiles_path))
    adapters_path = output / "adapter-payloads.json"
    adapters = json.loads(read_input(adapters_path))
    for version in ("0.155.1", "0.156.1"):
        source = json.loads(read_input(output / "source-records" / (source_names[version] + ".json")))
        archive = next(row for row in source["archives"]
                       if row["url"].startswith("https://codeload.github.com/openai/codex/tar.gz/"))
        commit = archive["url"].rsplit("/", 1)[-1]
        selected = [n for n in archive["notices"]
                    if "/codex-rs/skills/src/assets/samples/" in n["source_path"]]
        if not selected:
            raise ValueError("selected Codex source has no embedded skill notices")
        required = []
        for notice in selected:
            member = notice["source_path"].split("/", 1)[1]
            name = "codex-" + version + "-" + member.replace("/", "-")
            data = read_input(output / notice["file"])
            if sha(data) != notice["sha256"]:
                raise ValueError("embedded skill notice differs from exact source inventory")
            (output / "provider-notices" / name).write_bytes(data)
            row = {"file": name, "url": "https://raw.githubusercontent.com/openai/codex/" + commit + "/" + member,
                   "commit": commit, "member": member, "sha256": notice["sha256"],
                   "source_archive": archive["acquisition"],
                   "scope": "Exact source archive member; URL identifies the same immutable source, not a separate raw-URL acquisition or redistribution approval."}
            providers["notices"] = [n for n in providers["notices"] if n["file"] != name]
            providers["notices"].append(row)
            required.append({"file": "provider-notices/" + name, "sha256": notice["sha256"]})
        scopes = (profiles["codex-kit"][version] if version == "0.155.1"
                  else adapters["adapters"]["codex-acp"]["architectures"])
        for policy in scopes.values():
            if version == "0.156.1":
                # Preserve the canonical adapter collector's provider-first
                # ordering so a fresh CLI collection reproduces exact bytes.
                prefix = "codex-" + version + "-"
                provider_refs = [{"file": "provider-notices/" + n["file"], "sha256": n["sha256"]}
                                 for n in providers["notices"] if n["file"].startswith(prefix)]
                policy["required_notices"] = provider_refs + [n for n in policy["required_notices"]
                    if not n["file"].startswith("provider-notices/" + prefix)]
            else:
                policy["required_notices"] = list({n["file"]: n for n in policy["required_notices"] + required}.values())
    provider_path.write_bytes(encode(providers))
    profiles_path.write_bytes(encode(profiles))
    adapters_path.write_bytes(encode(adapters))

    # Reuse the canonical native archive collector for the actual archives whose
    # filename classifications changed. Preserve other acquired archive rows.
    native_path = args.materials / "native-r1/inventory.json"
    native_bytes = read_input(native_path)
    native_refresh = json.loads(native_bytes)
    if native_refresh["failures"]:
        raise ValueError("failed native notice recollection")
    (output / "source-records/primary-native-refresh.json").write_bytes(native_bytes)
    native_index = output / "source-records/primary-native-notices-v2.json"
    native = json.loads(read_input(native_index))
    components_path = output / "native-build-notices.json"
    components = json.loads(read_input(components_path))
    for refresh in native_refresh["archives"]:
        for row in native["archives"]:
            if row["url"] == refresh["url"]:
                if row["sha256"] != refresh["sha256"]:
                    raise ValueError("native refresh archive identity changed")
                row.update(notices=refresh["notices"], acquisition=refresh["acquisition"])
        for component in components:
            if any(n.get("url") == refresh["url"] for n in component["notices"]):
                component["notices"] = [dict(n, url=refresh["url"], archive_sha256=refresh["sha256"])
                                        for n in refresh["notices"]]
                component["required_notices"] = [{"file": n["file"], "sha256": n["sha256"]}
                                                 for n in component["notices"]]
    native["notice_refresh_source"] = "source-records/primary-native-refresh.json"
    native["notice_refresh_source_sha256"] = sha(native_bytes)
    native_index.write_bytes(encode(native))
    components_path.write_bytes(encode(components))
    for text in (native_path.parent / "texts").iterdir():
        (output / "texts" / text.name).write_bytes(read_input(text))

    upstream = {}
    for package, commit in [
        ("difflib", "8748d65b8c631a244af70f9d9f0339b6e7e9d29c"),
        ("fax", "5d70a161e33b62305f8bce7af3f7fbd011cfaf5d"),
    ]:
        data = read_input(args.materials / (package + "-LICENSE"))
        acquisition = json.loads(
            read_input(args.materials / (package + "-LICENSE.json"))
        )
        if sha(data) != acquisition["sha256"]:
            raise ValueError("primary upstream licence changed")
        ref = {
            "file": "texts/" + sha(data) + ".txt",
            "sha256": sha(data),
            "url": acquisition["url"],
            "commit": commit,
        }
        (output / ref["file"]).write_bytes(data)
        upstream[package] = {
            "notices": [ref],
            "acquisition": acquisition,
            "version_caveat": (
                "Licence introduced with0.3.0 before0.4.0;0.4.0 crate include list omits it and has no VCS record. Preserve upstream copyright Kevin B. Knapp verbatim; no inferred author identity or exact0.4.0 tree guarantee."
                if package == "difflib"
                else "Successor clarification added2025-09-15 after0.2.6 source40b226eb (2025-08-31). Not claimed present in the old published archive. Preserve Copyright ©2021 The pdf-rs contributers verbatim."
            ),
        }
    (output / "source-records/upstream-difflib-fax.json").write_bytes(encode(upstream))
    template = {
        "file": "provider-notices/MIT-template.txt",
        "sha256": "b05785f9f18e6716bab63424b11454513b9943a222595b70411009202fc592b5",
        "role": "informational standard conditions only; not an independently acquired upstream grant, copyright or resolved rights guarantee",
    }

    def transform(value):
        if isinstance(value, list):
            return [item for item in (transform(v) for v in value) if item is not None]
        if not isinstance(value, dict):
            return value
        value = {k: transform(v) for k, v in value.items()}
        file = value.get("file", "")
        origins = [value.get(k) for k in ["source_path", "member", "path", "source"]]
        code = next(
            (p for p in origins if isinstance(p, str) and (is_source_code(p) or excluded_material(p))), None
        )
        if file.startswith("texts/") and code and "byte_range" not in value:
            if value.get(
                "source_sha256"
            ) and "Verbatim bundled legal comment" in value.get("scope", ""):
                value["material_type"] = "embedded-license-comment"
            else:
                data = (output / file).read_bytes()
                if sha(data) != value["sha256"]:
                    raise ValueError("seed notice identity mismatch")
                header = None if excluded_material(code) else legal_header(data)
                if header:
                    new_hash = sha(header)
                    (output / "texts" / (new_hash + ".txt")).write_bytes(header)
                    changes.append(
                        {
                            "source": code,
                            "old_file": file,
                            "action": "exact-leading-legal-excerpt",
                            "sha256": new_hash,
                        }
                    )
                    value.update(
                        file="texts/" + new_hash + ".txt",
                        sha256=new_hash,
                        material_type="source-notice-excerpt",
                        source_file_sha256=sha(data),
                        byte_range=[0, len(header)],
                    )
                else:
                    changes.append(
                        {
                            "source": code,
                            "old_file": file,
                            "action": "drop-code-not-notice",
                        }
                    )
                    return None
        if value.get("license_expression") == "Apache-2.0" and "resolution" in value:
            value["resolution"] = (
                "Apache-2.0 declared by the package (not an alternative election); original source headers and full standard conditions retained"
            )
        if (
            "crate_url" in value
            and "notice_material_resolved" in value
            and "notices" in value
        ):
            name = value.get("name")
            if name in {"difflib", "fax", "fax_derive"}:
                supplement = upstream["difflib" if name == "difflib" else "fax"]
                value["notices"] += [
                    r for r in supplement["notices"] if r not in value["notices"]
                ]
                value.update(
                    notice_material_resolved=True,
                    resolution="Primary upstream material supplied with explicit version/attribution caveat",
                    upstream_caveat=supplement["version_caveat"],
                    rights_guarantee="not asserted",
                )
            elif name in {"debugserver-types", "deno_core_icudata"}:
                if template not in value["notices"]:
                    value["notices"].append(template)
                value["resolution"] = (
                    "Original publisher MIT declaration retained with informational standard permission text; no upstream full license text exists"
                )
        if (
            "scope" in value
            and value.get("path")
            == "/opt/docker/sbom/clipboard-bridge/.spdx.clipboard-bridge.json"
        ):
            value["scope"] = (
                "Observed public SPDX hash; reconciled in source-records/clipboard-spdx-reconciliation.json, including preserved x/sys vendor metadata discrepancies."
            )
        if value.get("schema") == "marsh.exact-source-notice-completions/v1":
            value["unresolved"] = [r["crate_url"] for r in value["packages"] if not r["notice_material_resolved"]]
            value["scope"] = "Exact publisher declarations and original headers, with current primary-material supplements and version caveats; Apache alternatives only where declared, not an enabled-link proof or legal approval."
        value.pop("apache_alternative_declarations_and_attributions_preserved", None)
        return value

    documents = {}
    original_origins = {}

    def index_origins(value):
        if isinstance(value, list):
            for item in value:
                index_origins(item)
        elif isinstance(value, dict):
            if isinstance(value.get("file"), str):
                origins = {value[k] for k in ("source_path", "member", "path", "source")
                           if isinstance(value.get(k), str)}
                original_origins.setdefault(value["file"], set()).update(origins)
            for item in value.values():
                index_origins(item)

    for path in sorted(output.rglob("*.json")):
        relative = str(path.relative_to(output))
        original = json.loads(read_input(path))
        index_origins(original)
        changed = transform(copy.deepcopy(original))
        if changed != original:
            path.write_bytes(encode(changed))
        documents[relative] = changed

    # Mandatory-reference lists intentionally omit source metadata. Apply the
    # exact source-derived replacement to those references as well; otherwise a
    # removed whole source file would still ship solely as an old requirement.
    replacements = {}
    for change in changes:
        replacement = ({"file": "texts/" + change["sha256"] + ".txt",
                        "sha256": change["sha256"]}
                       if "sha256" in change else None)
        previous = replacements.setdefault(change["old_file"], replacement)
        if previous != replacement:
            raise ValueError("inconsistent source-derived notice replacement")

    # Bare required lists have no origin. Refuse a hash shared with a legitimate
    # origin instead of globally deleting/replacing that legitimate document.
    changed_origins = {}
    for change in changes:
        changed_origins.setdefault(change["old_file"], set()).add(change["source"])
    for file, origins in changed_origins.items():
        if original_origins.get(file, set()) - origins:
            raise ValueError("source-derived replacement shares a legitimate notice hash: " + file)

    def rewrite_references(value):
        if isinstance(value, list):
            return [r for r in (rewrite_references(v) for v in value) if r is not None]
        if not isinstance(value, dict):
            return value
        if value.get("file") in replacements:
            replacement = replacements[value["file"]]
            if replacement is None:
                return None
            value = dict(value, **replacement)
        return {key: rewrite_references(item) for key, item in value.items()}

    for relative, document in list(documents.items()):
        changed = rewrite_references(document)
        if changed != document:
            documents[relative] = changed
            (output / relative).write_bytes(encode(changed))

    go = documents["go-module-notices.json"]
    go_missing = {
        r["path"] + "@" + r["version"] for r in go["modules"] if not r["notices"]
    }
    if go_missing != set(go["missing_notice"]):
        raise ValueError(
            "code exclusion exposed an unresolved Go legal-text gap; acquire before publishing"
        )

    # Newly exposed code-only archive gaps receive the already acquired exact-VCS
    # workspace-root supplements, never a made-up permission/copyright.
    supplement_path = args.materials / "new-source-supplements/inventory.json"
    supplementary = json.loads(read_input(supplement_path))
    if supplementary["unresolved"]:
        raise ValueError("new source-only gaps have no exact upstream supplement")
    supplemental_name = "source-records/regenerated-source-gap-supplements.json"
    documents[supplemental_name] = transform(supplementary)
    (output / supplemental_name).write_bytes(encode(documents[supplemental_name]))
    for text in (supplement_path.parent / "texts").iterdir():
        (output / "texts" / text.name).write_bytes(text.read_bytes())
    supplement_refs = [
        n for row in supplementary["supplements"] for n in row["notices"]
    ]
    primary = documents["primary-source-notices.json"]
    primary["collections"] = [c for c in primary["collections"] if c["name"] != "exact-source-regeneration-supplements"]
    primary["collections"].append(
        {
            "name": "exact-source-regeneration-supplements",
            "source_inventory": supplemental_name,
            "source_inventory_sha256": sha(encode(documents[supplemental_name])),
            "notices": supplement_refs,
            "scope": "Exact VCS workspace-root legal materials for archives formerly miscounted as code-name notices.",
        }
    )
    current_scope = args.materials / "source-dependency-scope.json"
    if not current_scope.is_file():
        raise ValueError("regenerate current configured root scope first")
    current_scope_name = "source-records/source-dependency-scope-current.json"
    (output / current_scope_name).write_bytes(read_input(current_scope))
    documents[current_scope_name] = json.loads(read_input(current_scope))
    old_scope_name = primary["source_configuration"]
    primary["source_configuration"] = current_scope_name
    if old_scope_name != current_scope_name:
        (output / old_scope_name).unlink()  # Prior source/evidence retained in seed.
        documents.pop(old_scope_name, None)
    status = primary["notice_material_status"]
    completed_rows = documents[completion_name]["packages"]
    apache = [row for row in completed_rows if any(
        n.get("role", "").startswith("standard Apache-2.0 conditions") for n in row["notices"])]
    plain = sum(row["license_expression"] == "Apache-2.0" for row in apache)
    status.update(apache_declarations_and_attributions_preserved=len(apache),
                  apache_plain_declarations=plain, apache_alternative_elections=len(apache) - plain)
    status["unresolved_original_MIT_wrapper_texts"] = [
        r
        for r in status["unresolved_original_MIT_wrapper_texts"]
        if r["name"] in {"debugserver-types", "deno_core_icudata"}
    ]
    status["upstream_material_clarifications"] = upstream
    status["permission_text_policy"] = (
        template["role"] + "; same policy for npm and Rust, no author/year synthesis"
    )
    status["current_resolution_index"] = "primary-source-notices.json"
    status["scope"] = "Exact publisher declarations and retained original headers; alternative elections only where the declaration offers alternatives. Not a legal opinion, inferred copyright, compiled-link attestation or runtime qualification."
    for row in status["unresolved_original_MIT_wrapper_texts"]:
        row["scope"] = "Informational standard MIT conditions accompany the original declaration; original full-text/rights disposition remains unresolved. Exact VCS trees were rechecked; see source-records/external-wrapper-source-recheck.json."

    status["historical_nonlegal_filename_matches"] = (
        "All current code-suffix matches are removed or explicit byte-range legal excerpts; see notice-regeneration.json"
    )
    status["correction"] = (
        "Active Rust collections were regenerated from exact archives with the shared vocabulary. Earlier index bytes are retained outside the shipped tree; acquisition receipts were not rewritten."
    )

    def refs_from_source(source):
        if isinstance(source, list):
            return (
                source
                if all("file" in r for r in source)
                else [n for row in source for n in row["notices"]]
            )
        if "notices" in source:
            return source["notices"]
        rows = source.get(
            "archives",
            source.get("supplements", source.get("packages", source.get("components"))),
        )
        return [n for row in rows for n in row["notices"]]

    # Source inventories first, then their small aggregate indexes. Source hashes
    # refer to the regenerated bytes, never a self-declared old artifact identity.
    for relative, document in documents.items():
        if relative.startswith("source-records/"):
            path = output / relative
            if json.loads(read_input(path)) != document:
                path.write_bytes(encode(document))
    for name in [
        "rust-notices.json",
        "selected-rust-notices.json",
        "primary-source-notices.json",
    ]:
        document = documents[name]
        for collection in document["collections"]:
            source_path = output / collection["source_inventory"]
            collection["source_inventory_sha256"] = sha(read_input(source_path))
            collection["notices"] = refs_from_source(
                json.loads(read_input(source_path))
            )
        if name == "primary-source-notices.json":
            current = {
                r["file"]: {"file": r["file"], "sha256": r["sha256"]}
                for c in document["collections"]
                for r in c["notices"]
            }
            document["required_runtime_notices"] = [current[k] for k in sorted(current)]
        (output / name).write_bytes(encode(document))
    for name in ["rust-coverage-status.json", "selected-rust-coverage-status.json"]:
        old = documents[name]
        new = {
            "schema": "marsh.current-notice-disposition/v2",
            "historical_input_sha256": sha(read_input(args.seed / name)),
            "current_resolution_index": "primary-source-notices.json",
            "unresolved_original_texts": status[
                "unresolved_original_MIT_wrapper_texts"
            ],
            "upstream_material_clarifications": upstream,
            "scope": "Current disposition, not stale raw archive missing-file counts or runtime qualification. Historical input is retained in regeneration seed evidence.",
        }
        (output / name).write_bytes(encode(new))
    # Neutral, fixed statements; only the LGPL component list is derived.
    boundaries = {
        "schema": "marsh.canonical-notice-boundaries/v2",
        "verification_claim": "A passing verifier run establishes the checked bytes, metadata, package versions and the presence of every required notice. It is not a legal opinion, a build signature, or a grant of redistribution rights.",
        "shipping_notice_materials": {
            "current_index": "primary-source-notices.json",
            "scope": "Original native source license documents, exact crate declarations and source copyright headers are retained. Apache-2.0 is elected only where the publisher's declaration offers it.",
            "remaining": "debugserver-types 0.5.0 and deno_core_icudata 0.77.0 declare MIT but ship no license file upstream; the declaration is retained with the standard MIT text for information, as for the npm package proxy-agent-negotiate. difflib and fax license files come from upstream with recorded version caveats.",
            "icu_data": "The icudata package contains ICU 77 data, which is under the Unicode/ICU terms (retained separately), not MIT.",
            "proxy_agent_negotiate": "Standard MIT text accompanies the publisher's MIT declaration for information; it is not an upstream grant or copyright notice."
        },
        "distribution_permissions": {
            "proprietary_components": "Claude Code and the Claude Agent SDK (Anthropic) and the clipboard-bridge binary in the DHI base (Docker) are proprietary and are distributed under their vendors' terms, not an open-source license. This inventory grants no rights to them.",
            "dhi_image_license": "The DHI license text covers DHI definitions, patches and build scripts under Apache-2.0; included binaries keep their own terms.",
            "clipboard_spdx": "The clipboard-bridge SPDX document is retained unchanged and reconciled in source-records/clipboard-spdx-reconciliation.json. Its golang.org/x/sys v0.46.0 dependency notice is collected separately."
        },
        "source_and_attestation_limits": [
            "npm archives pass their SHA-512 checks and SLSA subject/source commits are recorded; Sigstore signatures are not verified.",
            "Rust notices are collected from each release's Cargo.lock and declared features; they are not a record of what is actually linked. Upstream release builds do not pass --locked, and the v8 crate records dirty=true at its upstream commit.",
            "Corresponding source for DHI packages and vendor patches is referenced through the published arm64 DHI source-image links; no amd64 source link is recorded.",
            "The native Claude Code binary's embedded dependencies are not inventoried; its version-matched npm legal texts are retained."
        ]
    }
    native = documents["source-records/primary-native-notices-v2.json"]
    disposition = []
    for archive in native["archives"]:
        if not any(token in archive["url"] for token in ("/glib/", "/gstreamer/", "/gst-plugins-", "/proxy-libintl/")):
            continue
        legal = [n for n in archive["notices"] if n.get("member", "").endswith("/COPYING")
                 or "/LICENSES/LGPL-" in n.get("member", "")]
        disposition.append({
            "source_archive_url": archive["url"], "source_archive_sha256": archive["sha256"],
            "original_legal_documents": legal,
            "disposition": "The upstream source archive and its LGPL / Library GPL terms are identified. A distributor of the shipped library must provide the complete corresponding source, including any downstream modifications and build scripts, as those terms require.",
        })
    disposition.append({
        "component": "codex bubblewrap",
        "source": "Codex vendored source at the selected release commit; its COPYING is the Library GPL version 2.",
        "original_legal_documents": [
            {
                "file": "provider-notices/codex-0.155.1-codex-rs-vendor-bubblewrap-COPYING",
                "sha256": "b7993225104d90ddd8024fd838faf300bea5e83d91203eab98e29512acebd69c"
            }
        ],
        "disposition": "The pinned vendored source and license text are retained. A distributor of the bwrap executable must provide the complete corresponding source, including the selected downstream patches and build scripts, as its terms require."
    })
    boundaries["lgpl_corresponding_source"] = {
        "status": "Upstream sources and license terms identified.",
        "primary_terms": "The GNU Library GPL v2 or LGPL v2.1 texts (sections 4 and 6 as applicable) govern source and combined-work delivery. Each component keeps its own declaration.",
        "components": disposition,
        "scope": "Identifies the upstream source and terms for each LGPL component; it does not claim the upstream archives reproduce the shipped binaries.",
    }
    (output / "qualification-boundaries.json").write_bytes(encode(boundaries))

    used = set()

    def referenced(value):
        if isinstance(value, list):
            for item in value:
                referenced(item)
        elif isinstance(value, dict):
            if isinstance(value.get("file"), str) and value["file"].startswith(
                "texts/"
            ):
                used.add(value["file"])
            for item in value.values():
                referenced(item)

    for path in output.rglob("*.json"):
        referenced(json.loads(read_input(path)))
    removed = []
    for path in (output / "texts").iterdir():
        if str(path.relative_to(output)) not in used:
            removed.append(path.name)
            path.unlink()  # NEW derived output only; seed/prior receipts untouched.
    record = {
        "schema": "marsh.notice-regeneration/v1",
        "seed_tree_sha256": sha(encode(seed_hashes)),
        "vocabulary_sha256": sha(
            read_input(Path(__file__).with_name("collected") / "notice-rules.json")
        ),
        "changes": changes,
        "unreferenced_derived_texts_removed": sorted(removed),
        "scope": "Deterministic regeneration from retained source seed, exact archive collector results and primary acquisitions; no original receipt or source data deletion.",
    }
    (output / "notice-regeneration.json").write_bytes(encode(record))
    print(
        json.dumps(
            {"changes": len(changes), "removed_unreferenced_texts": len(removed)},
            indent=2,
        )
    )


if __name__ == "__main__":
    main()
