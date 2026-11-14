#!/bin/sh
# Usage: newest-release.sh X.Y.Z < tag names, one per line
#        newest-release.sh --github X.Y.Z
# Prints true when vX.Y.Z is the highest stable version tag (vX.Y.Z only;
# nightly and other tags are ignored), false when a higher one exists.
# Exits non-zero, printing nothing, when vX.Y.Z itself is not in the listing:
# a failed, empty or truncated listing (or a deleted tag) is never "newest".
#
# --github lists $GITHUB_REPOSITORY's tags with gh. A gh failure, or a
# listing that does not show vX.Y.Z yet (the API can lag a fresh push), is
# retried after each delay in NEWEST_RELEASE_DELAYS (default 4 8 16 32: five
# tries over about a minute), then the script fails; it never answers from a
# listing it could not verify.
#
# release.yml runs it in each job that moves :latest, :release, :repaired,
# the GitHub "Latest" mark or the formula, at the time that job runs: a
# re-run of failed jobs reuses the version job's outputs, which a newer
# release may have overtaken since. Callers fail their step on any non-zero
# exit.
set -eu
usage() { echo "usage: newest-release.sh [--github] X.Y.Z" >&2; exit 2; }

# decide X.Y.Z < tags: the answer, or exit 3 when vX.Y.Z is not listed.
decide() {
  tags=$(cat)
  printf '%s\n' "$tags" | grep -qxF "v$1" || return 3
  highest=$(printf '%s\n' "$tags" | grep -E '^v[0-9]+\.[0-9]+\.[0-9]+$' | sort -V | tail -n 1)
  if [ "$highest" = "v$1" ]; then echo true; else echo false; fi
}

if [ "${1:-}" != --github ]; then
  [ $# -eq 1 ] || usage
  decide "$1" && exit 0
  echo "newest-release.sh: v$1 is not in the tag listing; refusing to decide" >&2
  exit 1
fi
[ $# -eq 2 ] || usage
version=$2
try=1
for delay in ${NEWEST_RELEASE_DELAYS-4 8 16 32} last; do
  if refs=$(gh api --paginate "repos/$GITHUB_REPOSITORY/git/matching-refs/tags/v" --jq '.[].ref'); then
    answer=$(printf '%s\n' "$refs" | sed 's#^refs/tags/##' | decide "$version") && { echo "$answer"; exit 0; }
    echo "newest-release.sh: try $try: v$version is not in the tag listing" >&2
  else
    echo "newest-release.sh: try $try: listing the tags failed" >&2
  fi
  [ "$delay" != last ] || break
  sleep "$delay"
  try=$((try + 1))
done
echo "newest-release.sh: could not list v$version's tag after $try tries; refusing to decide" >&2
exit 1
