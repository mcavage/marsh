#!/usr/bin/env python3
"""Move the stable channel tag (:latest) of packaged Kit images to a main build.

Stable marsh follows each packaged Kit's :latest tag (its automatic updates,
at each daemon start and daily). Release CI runs this after every main build whose
CI passed, so an agent bump reaches stable users without a marsh release.

A Kit image moves only when both hold:

  * its org.marsh.kit.contract label is in the range the newest stable marsh
    supports (`supported` reads it from that release's kit_source.rs), so a
    stable user is never offered an image that needs a newer marsh; marsh
    checks the label again before switching;
  * its Kit descriptor (which names the agent, ACP adapter and package
    versions the image provides, and its network policy) differs from the one
    :latest has now. Builds are not bit-for-bit reproducible, so without this
    every main build would hand every user a new digest with nothing new in it.

Subcommands:
  supported FILE                     print LOW-HIGH from a kit_source.rs, or
                                     nothing when it declares no range
  promote --commands FILE --supported LOW-HIGH [--tag latest] [--apply]
                                     dry run unless --apply

Exits 1 if any image could not be inspected or tagged, after trying the rest.
"""
from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys

CONTRACT_LABEL = "org.marsh.kit.contract"
DESCRIPTOR = "vnd.docker.sandbox.kit.descriptor"
RANGE = re.compile(
    r"^pub const SUPPORTED_KIT_CONTRACTS: RangeInclusive<u32> = (\d+)\.\.=(\d+);$", re.M
)


def supported(source: str) -> tuple[int, int] | None:
    match = RANGE.search(source)
    return (int(match.group(1)), int(match.group(2))) if match else None


def parse_range(value: str) -> tuple[int, int] | None:
    match = re.fullmatch(r"(\d+)-(\d+)", value.strip())
    return (int(match.group(1)), int(match.group(2))) if match else None


def decide(contract, accepted, descriptor, current):
    """(move?, reason) for one image."""
    if accepted is None:
        return False, "the newest stable marsh declares no supported Kit contract"
    low, high = accepted
    if contract is None or not low <= contract <= high:
        return False, f"contract {contract if contract is not None else 'none'} is outside {low}-{high}"
    if descriptor is None:
        return False, "the build has no Kit descriptor"
    if current is not None and current == descriptor:
        return False, "same Kit descriptor as the current tag"
    return True, f"contract {contract}"


class Docker:
    def __init__(self, program: str):
        self.program = program

    def run(self, *arguments: str) -> str:
        result = subprocess.run(
            [self.program, "buildx", "imagetools", *arguments],
            capture_output=True,
            text=True,
            timeout=300,
            check=False,
        )
        if result.returncode != 0:
            raise RuntimeError(result.stderr.strip() or f"docker exited {result.returncode}")
        return result.stdout

    def contract(self, reference: str) -> int | None:
        image = json.loads(self.run("inspect", reference, "--format", "{{json .Image}}"))
        if "config" not in image:
            image = image.get("linux/arm64", {})
        value = ((image.get("config") or {}).get("Labels") or {}).get(CONTRACT_LABEL, "")
        return int(value) if str(value).strip().isdigit() else None

    def descriptor(self, reference: str) -> str | None:
        """The arm64 manifest's Kit descriptor annotation."""
        top = json.loads(self.run("inspect", "--raw", reference))
        manifest = top
        for entry in top.get("manifests", []):
            platform = entry.get("platform", {})
            if platform.get("os") == "linux" and platform.get("architecture") == "arm64":
                manifest = json.loads(
                    self.run("inspect", "--raw", f"{repository(reference)}@{entry['digest']}")
                )
                break
        return (manifest.get("annotations") or {}).get(DESCRIPTOR)

    def current_descriptor(self, tag: str) -> str | None:
        """None when the tag does not exist yet."""
        try:
            return self.descriptor(tag)
        except RuntimeError as error:
            if "not found" in str(error).lower():
                return None
            raise


def repository(reference: str) -> str:
    """REPOSITORY of REPOSITORY@DIGEST or REPOSITORY:TAG (a registry port is kept)."""
    if "@" in reference:
        return reference.rsplit("@", 1)[0]
    head, _, last = reference.rpartition("/")
    return f"{head}/{last.split(':', 1)[0]}" if head else last.split(":", 1)[0]


def promote(args) -> int:
    accepted = parse_range(args.supported) if args.supported else None
    with open(args.commands, encoding="utf-8") as handle:
        references = sorted(set(json.load(handle).values()))
    docker = Docker(args.docker)
    failed = False
    for reference in references:
        target = f"{repository(reference)}:{args.tag}"
        try:
            move, reason = decide(
                docker.contract(reference),
                accepted,
                docker.descriptor(reference),
                docker.current_descriptor(target),
            )
            if move and args.apply:
                docker.run("create", "--tag", target, reference)
            verb = ("moved" if args.apply else "would move") if move else "kept"
            print(f"{verb} {target}: {reason}")
        except (RuntimeError, ValueError, json.JSONDecodeError, subprocess.TimeoutExpired) as error:
            failed = True
            print(f"failed {target}: {error}", file=sys.stderr)
    return 1 if failed else 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    commands = parser.add_subparsers(dest="command", required=True)
    read = commands.add_parser("supported")
    read.add_argument("source")
    move = commands.add_parser("promote")
    move.add_argument("--commands", required=True)
    move.add_argument("--supported", default="")
    move.add_argument("--tag", default="latest")
    move.add_argument("--apply", action="store_true")
    move.add_argument("--docker", default="docker")
    args = parser.parse_args()
    if args.command == "supported":
        with open(args.source, encoding="utf-8") as handle:
            found = supported(handle.read())
        if found:
            print(f"{found[0]}-{found[1]}")
        return 0
    return promote(args)


if __name__ == "__main__":
    sys.exit(main())
