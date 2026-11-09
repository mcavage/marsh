#!/bin/sh
# Regression: the boundary tests' preflight refusals, and a runtime another
# actor creates first, never cost anyone else a file.
#
#   tests/breaks/daemon_boundary_preflight.sh FEATURE_PREFIX DEFAULT_MARSH
#
# Everything this test creates lives under ONE root made atomically by mktemp.
# Removing that root, after its device and inode are checked against the ones
# recorded at creation, is the only deletion this test does. The scripts under
# test (tests/breaks/daemon_boundary.sh, tests/breaks/daemon_boundary_safety.sh)
# are pointed at a fake runtime root inside it (BREAKS_RUNTIME_ROOT) and at
# fixed work directories inside it (BOUNDARY_FIXED_WORK, SAFETY_FIXED_BASE), so
# the colliding paths are known in advance and nothing under /tmp/marsh-UID is
# created, read or deleted. Fixtures are created exclusively (noclobber, mkdir
# without -p, ln without -f); a fixture that cannot be created aborts the case
# and nothing is recorded or cleaned for it.
#
# Cases, for each of the two scripts:
#   collision-runtime|lock|dangling  the runtime directory the first daemon would
#       create already exists (a full runtime directory with s, l, t and a
#       startup lock; only the startup lock; a dangling symlink). The script
#       must exit 2 saying it refuses, leave every pre-existing file unchanged,
#       and remove only its own work directory.
#   race  the stub marshd it launches plays the other actor: it creates the
#       runtime path (s, l, t and a startup lock) after the preflight and then
#       fails to start. The script fails, and all four of that actor's files
#       must still be there, unchanged.
# And one self-check: creating a fixture where something already exists fails
# and leaves the occupant untouched.
#
# SAFETY_SCRIPT / BOUNDARY_SCRIPT override the scripts under test (to run the
# regression against a mutant). No real daemon is started and no sbx is used.
# shellcheck disable=SC2329 # cleanup is invoked through traps
set -eu

prefix_arg=${1:?usage: daemon_boundary_preflight.sh FEATURE_PREFIX DEFAULT_MARSH}
default_arg=${2:?usage: daemon_boundary_preflight.sh FEATURE_PREFIX DEFAULT_MARSH}
: "$prefix_arg" "$default_arg"   # accepted for a uniform command line; the stubs below stand in for them
here=$(cd "$(dirname "$0")" && pwd -P)
safety=${SAFETY_SCRIPT:-$here/daemon_boundary_safety.sh}
boundary=${BOUNDARY_SCRIPT:-$here/daemon_boundary.sh}
command -v sbx >/dev/null || { echo "daemon_boundary_preflight: stock sbx must be on PATH (the scripts look it up)" >&2; exit 2; }

root=$(mktemp -d "${TMPDIR:-/tmp}/marsh-boundary-preflight.XXXXXX")
root=$(cd "$root" && pwd -P)
chmod 700 "$root"
dir_id() { stat -c '%d:%i' "$1" 2>/dev/null || stat -f '%d:%i' "$1"; }
root_id=$(dir_id "$root")
failed=0

cleanup() {
  status=$?
  trap - EXIT INT TERM HUP
  if [ -d "$root" ] && [ ! -L "$root" ] && [ "$(dir_id "$root")" = "$root_id" ]; then
    rm -rf "$root" || status=1
  else
    echo "daemon_boundary_preflight: CLEANUP FAILED: $root is no longer the directory this run created; not removing it" >&2
    status=1
  fi
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
trap 'exit 129' HUP

rrt="$root/runtime-root"   # stands in for /tmp/marsh-UID
mkdir "$rrt"

# Exclusive fixture creation: a failure aborts the calling case (or script).
mk_dir() { mkdir "$1" || { echo "fixture: could not create $1 exclusively" >&2; exit 1; }; }
mk_file() { ( set -C; printf '%s\n' "$2" > "$1" ) || { echo "fixture: could not create $1 exclusively" >&2; exit 1; }; }
mk_link() { ln -s "$2" "$1" || { echo "fixture: could not create $1 exclusively" >&2; exit 1; }; }

runtime_dir() {
  python3 -c '
import hashlib, os, sys
home = os.path.realpath(sys.argv[1])
print(f"{sys.argv[2]}/{hashlib.sha256(home.encode()).hexdigest()[:16]}")' "$1" "$rrt"
}

# Self-check: exclusive creation leaves an occupant alone.
mk_file "$root/occupied" "occupant"
if ( mk_file "$root/occupied" "intruder" ) 2>/dev/null || ( mk_link "$root/occupied" "x" ) 2>/dev/null || ( mk_dir "$root/occupied" ) 2>/dev/null; then
  echo "FAIL fixture-exclusive: a fixture was created over an existing path" >&2; failed=1
elif [ "$(cat "$root/occupied")" != "occupant" ]; then
  echo "FAIL fixture-exclusive: the occupant was changed" >&2; failed=1
else
  echo "PASS fixture-exclusive: creating a fixture over an existing path fails and leaves it untouched"
fi

# Stubs: a prefix whose marshd plays "the other actor" (creates the runtime
# path of its --home under BREAKS_RUNTIME_ROOT, then fails to start).
mkdir -p "$root/stub/bin"
printf '#!/bin/sh\nexit 0\n' > "$root/stub/bin/marsh"
cat > "$root/stub/bin/marshd" <<'EOF'
#!/bin/sh
home=$2
rt=$(python3 -c '
import hashlib, os, sys
home = os.path.realpath(sys.argv[1])
root = os.environ["BREAKS_RUNTIME_ROOT"]
print(f"{root}/{hashlib.sha256(home.encode()).hexdigest()[:16]}")' "$home")
mkdir "$rt" || exit 1
for f in s l t; do echo "sibling $f" > "$rt/$f"; done
echo "sibling lock" > "$rt.startup-lock"
exit 1
EOF
chmod 755 "$root/stub/bin/marsh" "$root/stub/bin/marshd"
stub="$root/stub"

# snapshot FILE...: path and content hash of each, or MISSING.
snapshot() {
  for f in "$@"; do
    printf '%s ' "$f"
    if [ -f "$f" ]; then shasum -a 256 < "$f"; else echo MISSING; fi
  done
}

# collision NAME SCRIPT ENVVAR FIXED HOME SHAPE
collision() {
  name=$1 script=$2 envvar=$3 fixed=$4 home=$5 shape=$6
  rt=$(runtime_dir "$home")
  files=""
  case $shape in
    runtime)
      mk_dir "$rt"
      for f in s l t; do mk_file "$rt/$f" "pre-existing $name $f"; done
      mk_file "$rt.startup-lock" "pre-existing $name lock"
      files="$rt/s $rt/l $rt/t $rt.startup-lock" ;;
    lock)
      mk_file "$rt.startup-lock" "pre-existing $name lock"
      files="$rt.startup-lock" ;;
    dangling)
      mk_link "$rt" "$root/does-not-exist-$name" ;;
  esac
  # shellcheck disable=SC2086 # the fixture paths contain no spaces: they are under $root
  before=$(snapshot $files; [ ! -L "$rt" ] || readlink "$rt")
  set +e
  env BREAKS_RUNTIME_ROOT="$rrt" "$envvar=$fixed" TMPDIR="$root" sh "$script" "$stub" "$stub/bin/marsh" >"$root/$name.log" 2>&1
  code=$?
  set -e
  # shellcheck disable=SC2086
  after=$(snapshot $files; [ ! -L "$rt" ] || readlink "$rt")
  ok=1
  if [ "$code" != 2 ]; then echo "FAIL $name: expected exit 2, got $code" >&2; ok=0; fi
  grep -q "already exists; refusing" "$root/$name.log" || { echo "FAIL $name: it did not refuse on the collision" >&2; ok=0; }
  if [ "$before" != "$after" ]; then echo "FAIL $name: a pre-existing path was changed or removed" >&2; ok=0; fi
  if [ -e "$fixed" ]; then echo "FAIL $name: the script left its own directory $fixed behind" >&2; ok=0; fi
  if [ "$ok" = 1 ]; then
    echo "PASS $name: refused (exit 2); every pre-existing path is unchanged; the script removed only its own directory"
  else
    failed=1; tail -5 "$root/$name.log" >&2
  fi
}

# race NAME SCRIPT ENVVAR FIXED HOME: the launched stub creates the runtime first.
race() {
  name=$1 script=$2 envvar=$3 fixed=$4 home=$5
  rt=$(runtime_dir "$home")
  set +e
  env BREAKS_RUNTIME_ROOT="$rrt" "$envvar=$fixed" TMPDIR="$root" sh "$script" "$stub" "$stub/bin/marsh" >"$root/$name.log" 2>&1
  code=$?
  set -e
  ok=1
  if [ "$code" = 0 ]; then echo "FAIL $name: the run succeeded although its daemon never started" >&2; ok=0; fi
  for f in s l t; do
    [ "$(cat "$rt/$f" 2>/dev/null)" = "sibling $f" ] || { echo "FAIL $name: the other actor's $rt/$f was removed or changed" >&2; ok=0; }
  done
  [ "$(cat "$rt.startup-lock" 2>/dev/null)" = "sibling lock" ] || { echo "FAIL $name: the other actor's startup lock was removed or changed" >&2; ok=0; }
  if [ -e "$fixed" ]; then echo "FAIL $name: the script left its own directory $fixed behind" >&2; ok=0; fi
  if [ "$ok" = 1 ]; then
    echo "PASS $name: exit $code (the daemon never started); the other actor's four files are untouched"
  else
    failed=1; tail -5 "$root/$name.log" >&2
  fi
}

for shape in runtime lock dangling; do
  fixed="$root/fixed-safety-$shape"
  collision "safety-$shape" "$safety" SAFETY_FIXED_BASE "$fixed" "$fixed/safety/home" "$shape"
done
for shape in runtime lock dangling; do
  fixed="$root/fixed-boundary-$shape"
  collision "boundary-$shape" "$boundary" BOUNDARY_FIXED_WORK "$fixed" "$fixed/p1/home" "$shape"
done
fixed="$root/fixed-safety-race"
race safety-race "$safety" SAFETY_FIXED_BASE "$fixed" "$fixed/safety/home"
fixed="$root/fixed-boundary-race"
race boundary-race "$boundary" BOUNDARY_FIXED_WORK "$fixed" "$fixed/p1/home"

exit "$failed"
