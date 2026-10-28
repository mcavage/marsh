# `marsh split apply` in the shell VM, run as
#   sh -c "$(cat this)" apply ROOT OBJECTS PATCH MODE
# MODE `numstat`: print `git apply --numstat -z` (NUL-delimited paths).
# MODE `raw3way`: `git apply --3way` with git's own output and status.
# MODE `json`: print NUL-separated fields: `R` RESULT `M` MESSAGE, then, for
# conflicts, `U` and every unmerged path. The patch is applied by git alone:
# it checks the whole patch before writing (so a patch that does not apply
# writes nothing), and `--3way` writes nothing when it fails; a filesystem
# error while writing can still leave either partial, which is why that case
# is reported `unknown`. There is nothing to roll back (a rollback would
# overwrite an editor's save or a `git add` that came meanwhile). Paths are
# never parsed from a quoted listing.
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
trap 'rm -f "$log" "$log".*' EXIT

# The project's index was last written by the Mac's Git, whose stat data
# differ from what this VM sees, so `--3way` would call every file "not
# matching the index". Refresh it here, just before a 3-way and after the
# blobs are imported (content decides; a lock held by another Git just leaves
# it alone).
refresh_index() { git update-index -q --refresh >/dev/null 2>&1 || :; }

# `--3way` reads the blobs a patch names (`index OLD..NEW`) from the split's
# object store (the alternate) and stages entries that name them. Copy each
# into the project's own objects BEFORE the index is touched, checking every
# copy's id, so an interrupted or failed import leaves the index as it was
# and a staged entry never depends on the split's store. Nothing here parses
# a path: ids come from the patch's header lines, and files, not pipes, carry
# the lists so that a failing step fails the apply.
import_patch_blobs() {
  grep -a '^index [0-9a-f]*\.\.[0-9a-f]*' "$patch" >"$log.index" 2>/dev/null
  [ $? -le 1 ] || return 1
  sed -n 's/^index \([0-9a-f]*\)\.\..*/\1/p' "$log.index" >"$log.all" || return 1
  sed -n 's/^index [0-9a-f]*\.\.\([0-9a-f]*\).*/\1/p' "$log.index" >>"$log.all" || return 1
  grep -v '^0*$' "$log.all" >"$log.some"
  [ $? -le 1 ] || return 1
  sort -u "$log.some" >"$log.oids" || return 1
  while IFS= read -r short; do
    # A patch may carry abbreviated ids; the split's store resolves them. An
    # id no store has (a post-image git computes itself) needs no copy.
    oid=$(git rev-parse --verify -q "$short^{blob}" 2>/dev/null) || continue
    env GIT_ALTERNATE_OBJECT_DIRECTORIES= git cat-file -e "$oid" 2>/dev/null && continue
    git cat-file blob "$oid" >"$log.blob" 2>/dev/null || return 1
    got=$(env GIT_ALTERNATE_OBJECT_DIRECTORIES= git hash-object -w -- "$log.blob") || return 1
    [ "$got" = "$oid" ] || return 1
  done <"$log.oids"
  rm -f "$log.index" "$log.all" "$log.some" "$log.oids" "$log.blob"
}

# After a 3-way: every object the index names resolves without the split's
# store. Ids come from `ls-files --format` (no paths), via files.
index_resolves() {
  git ls-files --format='%(objectmode) %(objectname)' >"$log.idx" || return 1
  sed -n 's/^\(100644\|100755\|120000\) \([0-9a-f]\{40,64\}\)$/\2/p' "$log.idx" | sort -u >"$log.ids" || return 1
  env GIT_ALTERNATE_OBJECT_DIRECTORIES= git cat-file --batch-check <"$log.ids" >"$log.chk" || return 1
  ! grep -q ' missing$' "$log.chk"
}

diagnostic() { sed -n '1,4p' "$log" | tr '\n' ' ' | cut -c1-400; }

if [ "$mode" = raw3way ]; then
  if [ "$repo" = yes ] && ! import_patch_blobs; then
    echo "marsh: split apply: the split's objects could not be copied into the project; nothing was applied" >&2
    exit 1
  fi
  refresh_index
  git apply --3way -- "$patch"
  rc=$?
  if [ "$repo" = yes ] && ! index_resolves; then
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

if ! import_patch_blobs; then
  finish failed "the split's objects could not be copied into the project; nothing was applied"
  exit 0
fi
refresh_index
git apply --3way -- "$patch" >"$log" 2>&1
rc=$?
if [ -n "$(git ls-files -u | head -n 1)" ]; then
  if index_resolves; then
    finish conflicts "applied with --3way; resolve the conflict markers in the listed files"
  else
    finish unknown "applied with --3way, but the project's index could not be made independent of the split's objects; check the project with git status"
  fi
  printf 'U\0'
  unmerged
elif [ "$rc" = 0 ]; then
  if index_resolves; then
    finish clean "merged with --3way; the changes are staged"
  else
    finish unknown "merged with --3way, but the project's index could not be made independent of the split's objects; check the project with git status"
  fi
elif [ "$rc" = 1 ]; then
  finish failed "does not apply, even with --3way: $(diagnostic)"
else
  finish unknown "git apply --3way ended with status $rc: $(diagnostic); check the project with git status"
fi
