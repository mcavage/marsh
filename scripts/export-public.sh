#!/bin/sh
# Export the current tracked tree as a fresh, single-commit git repository for
# public release. Nothing is pushed and no remote is configured.
#
#   [MAIL=...] scripts/export-public.sh [DEST]
#
# The commit is authored and committed as $AUTHOR_NAME (default mcavage)
# <$MAIL> (default mark@bluesnoop.com, the mcavage GitHub account's address),
# and the exported repo's git identity is set to the same.
#
# DEST defaults to /private/tmp/marsh-public and must not exist. Only files
# tracked at HEAD are exported (uncommitted changes are refused); local state
# such as target/, .pi-agent/ and .marsh-evidence/ is never tracked and so never
# exported. EXCLUDE below drops tracked paths that are not meant for the public
# repository.
set -eu

: "${MAIL:=mark@bluesnoop.com}"
AUTHOR_NAME=${AUTHOR_NAME:-mcavage}
DEST=${1:-/private/tmp/marsh-public}

# Tracked paths (git pathspecs) excluded from the export.
EXCLUDE='
:(exclude,glob)**/__pycache__/**
:(exclude,glob)**/*_TTrace_*
'

root=$(git rev-parse --show-toplevel)
cd "$root"
if [ -e "$DEST" ]; then
  echo "export-public: $DEST already exists; remove it first" >&2
  exit 1
fi
if [ -n "$(git status --porcelain --untracked-files=no)" ]; then
  echo "export-public: tracked files have uncommitted changes; commit or stash them" >&2
  exit 1
fi

source_commit=$(git rev-parse HEAD)
mkdir -p "$DEST"
# shellcheck disable=SC2086 # EXCLUDE is a newline-separated pathspec list.
git archive --format=tar HEAD -- . $EXCLUDE | tar -x -C "$DEST"

cd "$DEST"
git init -q -b main
git add -A
GIT_AUTHOR_NAME=$AUTHOR_NAME GIT_AUTHOR_EMAIL=$MAIL \
GIT_COMMITTER_NAME=$AUTHOR_NAME GIT_COMMITTER_EMAIL=$MAIL \
  git -c commit.gpgsign=false -c core.hooksPath=/dev/null commit -q -m "Initial commit"
git config user.name "$AUTHOR_NAME"
git config user.email "$MAIL"
if [ -n "$(git remote)" ]; then
  echo "export-public: unexpected remote in $DEST" >&2
  exit 1
fi
printf 'Exported %s (%s files) from %s to %s\n' \
  "$(git rev-parse --short HEAD)" "$(git ls-files | wc -l | tr -d ' ')" "$source_commit" "$DEST"
