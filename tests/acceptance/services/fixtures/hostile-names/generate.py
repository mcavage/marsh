#!/usr/bin/env python3
"""Regenerate compose.yaml from names.json (hostile text in service and volume
labels; service IDs and volume names stay valid). Not copied into projects."""
import json
import pathlib

HERE = pathlib.Path(__file__).resolve().parent
BB = "busybox:1.37@sha256:a7dbc319b1438115a207e11930dc4bb3e643dcb3ec0bdd694bb36cd8fad289ec"
names = json.loads((HERE / "names.json").read_text())["names"]


def q(text):  # a JSON string is a valid YAML double-quoted scalar; `$` is escaped for Compose interpolation
    return json.dumps(text, ensure_ascii=True).replace("$", "$$")


lines = ["# Valid service IDs and volume names; hostile text lives in the labels (names.json).", "services:"]
for svc, vol in (("web", "data"), ("worker", "cache")):
    lines += [f"  {svc}:", f"    image: {BB}", '    command: ["sleep", "1d"]', "    volumes:", f"      - {vol}:/d", "    labels:"]
    lines += [f"      nfx.hostile.{e['id']}: {q(e['name'])}" for e in names]
lines += ["", "volumes:"]
for vol in ("data", "cache"):
    lines += [f"  {vol}:", "    labels:"] + [f"      nfx.hostile.{e['id']}: {q(e['name'])}" for e in names]
(HERE / "compose.yaml").write_text("\n".join(lines) + "\n")
