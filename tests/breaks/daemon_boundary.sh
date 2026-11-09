#!/bin/sh
# Break-mode boundary on macOS (docs/design/test-breaks.md, "The boundary").
#
#   tests/breaks/daemon_boundary.sh FEATURE_PREFIX DEFAULT_MARSH
#
# FEATURE_PREFIX is an install prefix built with the test-breaks feature
# (`make dev DEV_FEATURES=test-breaks DEV_PREFIX=PREFIX`, or the same bin/ and
# libexec/marsh layout). DEFAULT_MARSH is a `marsh` CLI built WITHOUT the
# feature (the mixed-binary control). Each case uses its own scope under a
# temporary directory and never touches another daemon.
#
# The harness launch path is:   marshd --home H --test-breaks-from-env
# with MARSH_TEST_BREAK in the daemon's own environment. Cases:
#   P1  harness launch (flag + MARSH_TEST_BREAK=demo), then `marsh status --json`:
#       test_breaks is ["demo"]                                       (positive)
#   N0  the same daemon launched WITHOUT the flag: it ignores the variable
#   N1  `MARSH_TEST_BREAK=demo marsh status --json`, feature CLI, no daemon: the
#       CLI auto-starts one; it reports no test_breaks
#   N2  a daemon launched without the mode, then a CLI with the variable: none
#   N3  mixed binaries: DEFAULT CLI + FEATURE marshd as siblings, no daemon,
#       MARSH_TEST_BREAK=demo, auto-start: none
#
# Cleanup: every daemon this script started is stopped, and waited for, on every
# exit path (normal, failure, INT, TERM, HUP). A daemon that cannot be stopped
# fails the script. Safety: only exact pids this run started and recorded (start
# time and exact command line verified before each signal) are signalled, never
# anything found by pattern. All state lives under ONE root made atomically by
# mktemp; removing it, after its device and inode are checked, is the only
# deletion this script does. It never deletes daemon runtime files
# (/tmp/marsh-UID/...): a clean stop removes the daemon's socket, lock and token, and anything
# that remains is reported. Those paths are keyed by homes inside the unguessable
# root, so nobody else creates them unless they guess its name.
# shellcheck disable=SC2329 # the cleanup functions are invoked through traps
set -eu

prefix=${1:?usage: daemon_boundary.sh FEATURE_PREFIX DEFAULT_MARSH}
default_marsh=${2:?usage: daemon_boundary.sh FEATURE_PREFIX DEFAULT_MARSH}
prefix=$(cd "$prefix" && pwd -P)
default_marsh=$(cd "$(dirname "$default_marsh")" && pwd -P)/$(basename "$default_marsh")
for binary in marsh marshd; do
  test -x "$prefix/bin/$binary" || { echo "daemon_boundary: missing $prefix/bin/$binary" >&2; exit 2; }
done
test -x "$default_marsh" || { echo "daemon_boundary: missing $default_marsh" >&2; exit 2; }
sbx=$(command -v sbx) || { echo "daemon_boundary: stock sbx must be on PATH (startup probes only; no VM is created)" >&2; exit 2; }

# BOUNDARY_FIXED_WORK=PATH (test seam for daemon_boundary_preflight.sh): use
# PATH, which must not exist, instead of a random temporary directory. mkdir
# (without -p) fails if it exists, so a pre-existing path is never reused.
if [ -n "${BOUNDARY_FIXED_WORK:-}" ]; then
  mkdir "$BOUNDARY_FIXED_WORK"
  work=$BOUNDARY_FIXED_WORK
else
  work=$(mktemp -d "${TMPDIR:-/tmp}/marsh-breaks-boundary.XXXXXX")
fi
work=$(cd "$work" && pwd -P)
chmod 700 "$work"
# The one root of this run's state. Removing it, after its device and inode are
# checked against the ones recorded here, is the only deletion this script does.
dir_id() { stat -c '%d:%i' "$1" 2>/dev/null || stat -f '%d:%i' "$1"; }
work_id=$(dir_id "$work")
remove_work() {
  if [ -d "$work" ] && [ ! -L "$work" ] && [ "$(dir_id "$work")" = "$work_id" ]; then
    rm -rf "$work"
  else
    echo "daemon_boundary: CLEANUP FAILED: $work is no longer the directory this run created; not removing it" >&2
    return 1
  fi
}
failed=0
# Bookkeeping lives in files under $work, never in word-split variables, so no
# path (TMPDIR may contain spaces or glob characters) is ever split or expanded:
#   $work/scopes/NAME/{home,runtime}   one directory per scope this run created
#   $work/owned/PID                    identity of each daemon this run started
mkdir "$work/scopes" "$work/owned"

# The endpoint runtime directory a daemon creates for a home: /tmp/marsh-UID/SCOPE16.
# BREAKS_RUNTIME_ROOT replaces /tmp/marsh-UID for the preflight tests only
# (daemon_boundary_preflight.sh), whose fixtures then live inside its own root.
# The path is keyed by a home inside this run's unguessable temporary root, so
# nobody else creates it unless they guess that root's name. This script never
# deletes anything under it (a clean stop leaves at most an empty directory and a startup lock).
runtime_dir() {
  python3 -c '
import hashlib, os, sys
home = os.path.realpath(sys.argv[1])
root = os.environ.get("BREAKS_RUNTIME_ROOT") or f"/tmp/marsh-{os.getuid()}"
print(f"{root}/{hashlib.sha256(home.encode()).hexdigest()[:16]}")' "$1"
}

alive() { kill -0 "$1" 2>/dev/null; }

# The exact command lines a daemon of scope HOME can have: a marshd of this
# run's install (as spelled, or with symlinks resolved, which is how a CLI
# starts its sibling), --home exactly HOME, optionally the harness flag.
# Compared as whole lines, never as substrings.
resolved() {
  python3 -c 'import os, sys; print(os.path.realpath(sys.argv[1]))' "$1"
}
expected_commands() {
  for exe in "$prefix/bin/marshd" "$(resolved "$prefix/bin/marshd")" \
             "$work/mixed/bin/marshd" "$(resolved "$work/mixed/bin/marshd")"; do
    printf '%s\n' "$exe --home $1" "$exe --home $1 --test-breaks-from-env"
  done
}

start_time() { ps -ww -o lstart= -p "$1" 2>/dev/null | sed 's/^ *//'; }
command_line() { ps -ww -o command= -p "$1" 2>/dev/null | sed 's/^ *//'; }

# scope_home NAME: the home of a scope this run created (empty if none).
scope_home() {
  if [ -f "$work/scopes/$1/home" ]; then cat "$work/scopes/$1/home"; fi
}

# daemon_ok PID SCOPE: PID's command line is exactly one of SCOPE's expected
# commands.
daemon_ok() {
  home=$(scope_home "$2")
  [ -n "$home" ] || return 1
  cmd=$(command_line "$1")
  [ -n "$cmd" ] || return 1
  expected_commands "$home" | {
    found=1
    while IFS= read -r line; do
      if [ "$line" = "$cmd" ]; then found=0; fi
    done
    exit "$found"
  }
}

# record_daemon PID SCOPE: record a daemon this run started, but only after its
# command line is verified to be exactly one of SCOPE's expected commands.
record_daemon() {
  if ! daemon_ok "$1" "$2"; then
    echo "daemon_boundary: pid $1 is not exactly a marshd of scope $2 ($(command_line "$1")); not recording it" >&2
    return 1
  fi
  {
    start_time "$1"
    command_line "$1"
    printf '%s\n' "$2"
  } > "$work/owned/$1"
}

# stop_daemon PID: stop one recorded daemon. SIGTERM, wait up to 30s, SIGKILL,
# wait up to 10s. Before each signal the pid must still have the recorded start
# time and command line AND still be exactly one of its scope's expected
# commands; a reused pid is never touched.
stop_daemon() {
  rec="$work/owned/$1"
  mine() {
    [ "$(start_time "$1")" = "$(sed -n 1p "$rec")" ] &&
      [ "$(command_line "$1")" = "$(sed -n 2p "$rec")" ] &&
      daemon_ok "$1" "$(sed -n 3p "$rec")"
  }
  alive "$1" || return 0
  mine "$1" || return 0   # the pid was reused: the daemon we started is gone
  kill -TERM "$1" 2>/dev/null || true
  i=0
  while alive "$1" && mine "$1" && [ "$i" -lt 60 ]; do i=$((i + 1)); sleep 0.5; done
  if alive "$1" && mine "$1"; then
    kill -KILL "$1" 2>/dev/null || true
    i=0
    while alive "$1" && mine "$1" && [ "$i" -lt 20 ]; do i=$((i + 1)); sleep 0.5; done
  fi
  if alive "$1" && mine "$1"; then
    echo "daemon_boundary: CLEANUP FAILED: could not stop daemon pid $1" >&2
    return 1
  fi
}

# adopt_from_lock SCOPE: a daemon the CLI auto-started whose pid this run never
# saw (the CLI failed before the daemon answered). SCOPE must be a scope this
# run created (its record exists under $work/scopes). The owner lock sits in
# the runtime directory this run recorded as absent before the daemon started;
# the pid there is recorded only if it is exactly one of SCOPE's daemons.
adopt_from_lock() {
  home=$(scope_home "$1")
  [ -n "$home" ] || return 0
  [ "$(runtime_dir "$home")" = "$(cat "$work/scopes/$1/runtime")" ] || return 0
  lock="$(cat "$work/scopes/$1/runtime")/l"
  [ -f "$lock" ] || return 0
  lp=$(tr -dc '0-9' < "$lock")
  [ -n "$lp" ] || return 0
  [ ! -f "$work/owned/$lp" ] || return 0
  if alive "$lp"; then record_daemon "$lp" "$1" || true; fi
}

cleaned=0
cleanup() {
  status=$?
  [ "$cleaned" = 0 ] || exit "$status"
  cleaned=1
  trap - EXIT INT TERM HUP
  cleanup_failed=0
  for sd in "$work"/scopes/*; do
    [ -d "$sd" ] || continue
    adopt_from_lock "$(basename "$sd")"
  done
  for rec in "$work"/owned/*; do
    [ -f "$rec" ] || continue
    stop_daemon "$(basename "$rec")" || cleanup_failed=1
  done
  # Runtime paths that remain are reported, never removed: they are keyed by
  # this run's unique homes, so they cannot belong to anyone else. Only a daemon
  # this run started that is still alive fails the run.
  for sd in "$work"/scopes/*; do
    [ -f "$sd/runtime" ] || continue
    rt=$(cat "$sd/runtime")
    for path in "$rt" "$rt.startup-lock"; do
      if [ -e "$path" ] || [ -L "$path" ]; then
        echo "daemon_boundary: note: $path remains (not removed)" >&2
      fi
    done
  done
  if [ "$cleanup_failed" = 0 ]; then
    remove_work || cleanup_failed=1
  else
    echo "daemon_boundary: scopes kept for inspection under $work" >&2
  fi
  if [ "$cleanup_failed" != 0 ] && [ "$status" = 0 ]; then
    status=1
  fi
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
trap 'exit 129' HUP

# Wait until the launched daemon ($1 = pid) has bound the endpoint of $2.
wait_ready() {
  socket="$(runtime_dir "$2")/s"
  i=0
  until test -S "$socket" || ! alive "$1"; do
    i=$((i + 1)); test "$i" -le 120 || break; sleep 0.5
  done
  test -S "$socket" && alive "$1"
}

# Reads `marsh status --json` on stdin; prints test_breaks ("<absent>" when
# missing) and the pid of the daemon that answered.
fields() {
  python3 -c '
import json, sys
d = json.load(sys.stdin)
print(",".join(d.get("test_breaks", [])) or "<absent>", d["endpoint_owner"]["pid"])'
}

new_scope() {
  scope_name=$1
  scope="$work/$1"
  mkdir -p "$scope/home" "$scope/control"
  chmod 700 "$scope/home" "$scope/control"
  rt=$(runtime_dir "$scope/home")
  # The runtime path is keyed by this fresh home, so it cannot exist yet; if it
  # does (a dangling symlink counts), refuse without touching anything.
  for path in "$rt" "$rt.startup-lock"; do
    if [ -e "$path" ] || [ -L "$path" ]; then
      echo "daemon_boundary: $path already exists; refusing to continue" >&2
      exit 2
    fi
  done
  mkdir "$work/scopes/$1"
  printf '%s\n' "$scope/home" > "$work/scopes/$1/home"
  printf '%s\n' "$rt" > "$work/scopes/$1/runtime"
  export MARSH_HOME="$scope/home" MARSH_CONTROL_HOME="$scope/control" MARSH_SBX="$sbx"
}

# launch NAME BREAK FLAG...: start the feature marshd directly in a new scope,
# with MARSH_TEST_BREAK=BREAK (empty: unset) and the given extra arguments.
launch() {
  name=$1 mode=$2; shift 2
  new_scope "$name"
  if [ -n "$mode" ]; then
    MARSH_TEST_BREAK=$mode "$prefix/bin/marshd" --home "$MARSH_HOME" "$@" >"$scope/daemon.log" 2>&1 &
  else
    "$prefix/bin/marshd" --home "$MARSH_HOME" "$@" >"$scope/daemon.log" 2>&1 &
  fi
  pid=$!
  record_daemon "$pid" "$scope_name" || exit 1
  wait_ready "$pid" "$MARSH_HOME" || { echo "FAIL $name: launched daemon did not start" >&2; cat "$scope/daemon.log" >&2; exit 1; }
}

# check NAME WANT GOT: GOT is "<test_breaks> <answering pid>"; the first field
# must equal WANT.
check() {
  name=$1 want=$2 got=$3
  if [ "${got%% *}" = "$want" ]; then
    echo "PASS $name: test_breaks=$want (answered by pid ${got#* })"
  else
    echo "FAIL $name: want test_breaks=$want, got '$got'" >&2
    failed=1
  fi
}

# answered NAME GOT PID: the answering daemon is the one this script launched.
answered() {
  test "${2#* }" = "$3" || { echo "FAIL $1: answered by pid ${2#* }, not the launched daemon $3" >&2; failed=1; }
}

# P1: the harness launch path.
launch p1 demo --test-breaks-from-env
got=$("$prefix/bin/marsh" status --json | fields)
check P1 demo "$got"
answered P1 "$got" "$pid"

# N0: the same variable, no flag: a feature daemon ignores it.
launch n0 demo
got=$("$prefix/bin/marsh" status --json | fields)
check N0 "<absent>" "$got"
answered N0 "$got" "$pid"

# N1: the CLI's environment auto-starts a daemon; the started one must not inherit it.
new_scope n1
got=$(MARSH_TEST_BREAK=demo "$prefix/bin/marsh" status --json | fields)
record_daemon "${got#* }" "$scope_name"
check N1 "<absent>" "$got"

# N2: a daemon launched without the mode ignores a later CLI environment.
launch n2 ""
got=$(MARSH_TEST_BREAK=demo "$prefix/bin/marsh" status --json | fields)
check N2 "<absent>" "$got"
answered N2 "$got" "$pid"

# N3: a default CLI (no feature) next to a feature marshd: the CLI auto-starts
# the feature daemon. Selection must not depend on the CLI's feature set.
mixed="$work/mixed"
mkdir -p "$mixed/bin"
cp "$default_marsh" "$mixed/bin/marsh"
ln -s "$prefix/bin/marshd" "$mixed/bin/marshd"
if strings -a "$mixed/bin/marsh" | grep -q 'marsh-feature:test-breaks'; then
  echo "FAIL N3: DEFAULT_MARSH was built with test-breaks; it must be a default build" >&2
  exit 2
fi
new_scope n3
got=$(MARSH_TEST_BREAK=demo "$mixed/bin/marsh" status --json | fields)
record_daemon "${got#* }" "$scope_name"
check N3 "<absent>" "$got"

exit $failed
