# `marsh split apply` in the shell VM, run as
#   sh -c "$(cat this)" apply ROOT OBJECTS PATCH MODE
# MODE `numstat`: print `git apply --numstat -z` (NUL-delimited paths).
# MODE `raw3way`: `git apply --3way` with git's own output and status.
# MODE `json`: print NUL-separated fields: `R` RESULT `M` MESSAGE, then, for
# conflicts, `U` and every unmerged path. The patch is applied by git alone:
# plain `git apply` is all or nothing, and `--3way` writes nothing when it
# fails, so there is nothing to roll back (a rollback would overwrite an
# editor's save or a `git add` that came meanwhile). Paths are never parsed
# from a quoted listing.
cd -- "$1" || exit 2
GIT_ALTERNATE_OBJECT_DIRECTORIES=$2
export GIT_ALTERNATE_OBJECT_DIRECTORIES
patch=$3
mode=$4

if [ "$mode" = numstat ]; then
  git apply --numstat -z -- "$patch"
  exit $?
fi

repo=no
if git rev-parse --git-dir >/dev/null 2>&1 && [ -z "$(git rev-parse --show-cdup 2>/dev/null)" ]; then
  repo=yes
fi
log=$(mktemp "${TMPDIR:-/tmp}/marsh-apply.XXXXXX") || exit 2
trap 'rm -f "$log"' EXIT

# The project's index was last written by the Mac's Git, whose stat data
# differ from what this VM sees, so `--3way` would call every file "not
# matching the index". Refresh it here (content decides; a lock held by
# another Git just leaves it alone).
if [ "$repo" = yes ]; then git update-index -q --refresh >/dev/null 2>&1 || :; fi

# Object ids of every index entry (gitlinks aside) that the project's own
# object store lacks: `--3way` stages blobs it read from the split's store
# (the alternate), and an index that depends on it breaks once the split goes.
missing_oids() {
  git ls-files -s -z | tr '\0' '\n' |
    sed -n '/^160000 /d; s/^[0-7]* \([0-9a-f]\{40,64\}\) [0-3][[:space:]].*/\1/p' | sort -u |
    env GIT_ALTERNATE_OBJECT_DIRECTORIES= git cat-file --batch-check |
    sed -n 's/^\([0-9a-f]*\) missing$/\1/p'
}

# Copy them in, check each copy's id, and check the index resolves without
# the alternate.
self_contained() {
  for oid in $(missing_oids); do
    got=$(git cat-file blob "$oid" | env GIT_ALTERNATE_OBJECT_DIRECTORIES= git hash-object -w --stdin) || return 1
    [ "$got" = "$oid" ] || return 1
  done
  [ -z "$(missing_oids)" ]
}

diagnostic() { sed -n '1,4p' "$log" | tr '\n' ' ' | cut -c1-400; }

if [ "$mode" = raw3way ]; then
  git apply --3way -- "$patch"
  rc=$?
  if [ "$repo" = yes ] && ! self_contained; then
    echo "marsh: split apply: the project's index still needs the split's objects" >&2
    [ "$rc" = 0 ] && rc=1
  fi
  exit "$rc"
fi

finish() { printf 'R\0%s\0M\0%s\0' "$1" "$2"; }
unmerged() { git diff --name-only --diff-filter=U -z; }

if [ "$repo" = yes ] && [ -n "$(git ls-files -u | head -n 1)" ]; then
  finish failed "the project has unresolved merge conflicts; resolve them first"
  exit 0
fi

if git apply --check -- "$patch" >"$log" 2>&1; then
  if git apply -- "$patch" >"$log" 2>&1; then
    finish clean "applied cleanly"
  else
    finish unknown "the patch checked out but git failed to apply it ($?): $(diagnostic); check the project with git status"
  fi
  exit 0
fi
if [ "$repo" != yes ]; then
  finish failed "does not apply: $(diagnostic)"
  exit 0
fi

git apply --3way -- "$patch" >"$log" 2>&1
rc=$?
if [ -n "$(git ls-files -u | head -n 1)" ]; then
  if self_contained; then
    finish conflicts "applied with --3way; resolve the conflict markers in the listed files"
  else
    finish unknown "applied with --3way, but the project's index could not be made independent of the split's objects; check the project with git status"
  fi
  printf 'U\0'
  unmerged
elif [ "$rc" = 0 ]; then
  if self_contained; then
    finish clean "merged with --3way; the changes are staged"
  else
    finish unknown "merged with --3way, but the project's index could not be made independent of the split's objects; check the project with git status"
  fi
elif [ "$rc" = 1 ]; then
  finish failed "does not apply, even with --3way: $(diagnostic)"
else
  finish unknown "git apply --3way ended with status $rc: $(diagnostic); check the project with git status"
fi
