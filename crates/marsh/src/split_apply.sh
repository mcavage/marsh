# `marsh split apply --json`: run in the shell VM as
#   sh -c "$(cat this)" apply ROOT OBJECTS PATCH FILES [raw3way]
# FILES is the lane's `files` listing ("A|M|D<TAB>path" per line). The reply
# is NUL-separated: `R` RESULT `M` MESSAGE, then `C` PATH per conflicted path.
# The outcome is read from the tree and the index, never from git's text.
cd -- "$1" || exit 2
GIT_ALTERNATE_OBJECT_DIRECTORIES=$2
export GIT_ALTERNATE_OBJECT_DIRECTORIES
patch=$3
list=$4
tab=$(printf '\t')
snap=$(mktemp -d "${TMPDIR:-/tmp}/marsh-apply.XXXXXX") || {
  printf 'R\0failed\0M\0cannot make a scratch directory\0'
  exit 0
}
trap 'rm -rf "$snap"' EXIT

repo=no
if git rev-parse --git-dir >/dev/null 2>&1 && [ -z "$(git rev-parse --show-cdup 2>/dev/null)" ]; then
  repo=yes
fi

while IFS= read -r line; do
  p=${line#*"$tab"}
  [ -n "$p" ] && printf '%s\n' "$p"
done <"$list" >"$snap/paths"

fingerprint() {
  if [ -L "$1" ]; then
    printf 'L:%s\n' "$(readlink -- "$1")"
  elif [ -f "$1" ]; then
    if [ -x "$1" ]; then mode=x; else mode=f; fi
    printf '%s:%s\n' "$mode" "$(cksum <"$1")"
  elif [ -e "$1" ]; then
    echo other
  else
    echo absent
  fi
}

index_file() { git rev-parse --git-path index 2>/dev/null; }

snapshot() {
  n=0
  while IFS= read -r p; do
    n=$((n + 1))
    fingerprint "$p" >"$snap/$n.fp"
    if [ -f "$p" ] && [ ! -L "$p" ]; then cp -p -- "$p" "$snap/$n.file"; fi
  done <"$snap/paths"
  if [ "$repo" = yes ] && [ -f "$(index_file)" ]; then cp -p -- "$(index_file)" "$snap/index"; fi
}

# Whether anything the apply may touch differs from the snapshot.
changed() {
  n=0
  while IFS= read -r p; do
    n=$((n + 1))
    [ "$(fingerprint "$p")" = "$(cat "$snap/$n.fp")" ] || return 0
  done <"$snap/paths"
  if [ "$repo" = yes ]; then
    if [ -f "$snap/index" ]; then
      cmp -s -- "$snap/index" "$(index_file)" || return 0
    elif [ -f "$(index_file)" ]; then
      return 0
    fi
  fi
  return 1
}

restore() {
  n=0
  while IFS= read -r p; do
    n=$((n + 1))
    was=$(cat "$snap/$n.fp")
    [ "$(fingerprint "$p")" = "$was" ] && continue
    rm -f -- "$p"
    case $was in
      absent | other) ;;
      L:*) ln -s -- "${was#L:}" "$p" ;;
      *)
        mkdir -p -- "$(dirname -- "$p")"
        cp -p -- "$snap/$n.file" "$p"
        ;;
    esac
  done <"$snap/paths"
  if [ "$repo" = yes ]; then
    if [ -f "$snap/index" ]; then cp -p -- "$snap/index" "$(index_file)"; else rm -f -- "$(index_file)"; fi
  fi
}

unmerged() { git -c core.quotepath=off diff --name-only --diff-filter=U 2>/dev/null | sort; }

diagnostic() { sed -n '1,4p' "$1" | tr '\n' ' ' | cut -c1-400; }

finish() { # RESULT MESSAGE
  printf 'R\0%s\0M\0%s\0' "$1" "$2"
}

# `--3way` stages blobs it read from the split's object store (the alternate);
# copy the ones the index now names into the project's own objects, so the
# index stays valid after the split is removed.
import_objects() {
  while IFS= read -r p; do
    git ls-files -s -- "$p" 2>/dev/null | while read -r mode sha stage rest; do
      [ "$mode" = 160000 ] && continue
      env GIT_ALTERNATE_OBJECT_DIRECTORIES= git cat-file -e "$sha" 2>/dev/null && continue
      git cat-file blob "$sha" | env GIT_ALTERNATE_OBJECT_DIRECTORIES= git hash-object -w --stdin >/dev/null
    done
  done <"$snap/paths"
}

# The project's index was last refreshed by the Mac's Git, whose stat data
# (inode, device, ctime) differ from what this VM sees, so `--3way` would call
# every file "not matching the index". Refresh it here (content decides).
if [ "$repo" = yes ]; then git update-index -q --refresh >/dev/null 2>&1 || :; fi

# `split apply --3way` without `--json`: git's own output and status.
if [ "$5" = raw3way ]; then
  git apply --3way -- "$patch"
  rc=$?
  import_objects
  exit "$rc"
fi

snapshot
[ "$repo" = yes ] && unmerged >"$snap/u0" || : >"$snap/u0"

if git apply -- "$patch" >"$snap/log1" 2>&1; then
  finish clean "applied cleanly"
  exit 0
fi
restored=
if changed; then restore; restored=" (partial writes were undone)"; fi
if [ "$repo" != yes ]; then
  finish failed "does not apply: $(diagnostic "$snap/log1")$restored"
  exit 0
fi

if git apply --3way -- "$patch" >"$snap/log2" 2>&1; then rc=0; else rc=$?; fi
unmerged >"$snap/u1"
grep -vxF -f "$snap/u0" "$snap/u1" >"$snap/new" || :
if [ -s "$snap/new" ] || [ "$rc" = 0 ]; then import_objects; fi
if [ -s "$snap/new" ]; then
  finish conflicts "applied with --3way; resolve the conflict markers in the listed files"
  while IFS= read -r p; do printf 'C\0%s\0' "$p"; done <"$snap/new"
  exit 0
fi
if [ "$rc" = 0 ]; then
  finish clean "merged with --3way; the changes are staged"
  exit 0
fi
restored=
if changed; then restore; restored=" (partial writes were undone)"; fi
finish failed "does not apply, even with --3way: $(diagnostic "$snap/log2")$restored"
