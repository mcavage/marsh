#!/bin/sh
# Usage: newest-release.sh X.Y.Z < tag names, one per line
# Prints true when vX.Y.Z is the highest stable version tag (vX.Y.Z only;
# nightly and other tags are ignored), false when a higher one exists.
# release.yml runs it in each job that moves :latest, :release, :repaired,
# the GitHub "Latest" mark or the formula, at the time that job runs: a
# re-run of failed jobs reuses the version job's outputs, which a newer
# release may have overtaken since.
set -eu
[ $# -eq 1 ] || { echo "usage: newest-release.sh X.Y.Z < tags" >&2; exit 2; }
highest=$({ grep -E '^v[0-9]+\.[0-9]+\.[0-9]+$' || true; printf 'v%s\n' "$1"; } | sort -V | tail -n 1)
if [ "$highest" = "v$1" ]; then echo true; else echo false; fi
