#!/bin/sh
# Safety regression for tests/breaks/daemon_boundary.sh: whitespace in TMPDIR
# must never make its cleanup touch a daemon it did not start.
#
#   tests/breaks/daemon_boundary_safety.sh FEATURE_PREFIX DEFAULT_MARSH
#
# TMPDIR for the run under test is "<base>/safety case". Split at the space, the
# first fragment is "<base>/safety". A separately owned DECOY daemon (started
# here, by exact pid) runs with `--home <base>/safety/home`, so its command line
# contains that truncated path and a lock in the runtime directory a split path
# would look up. A cleanup that splits paths, or matches by substring, adopts
# and stops the decoy. Each case must leave the decoy running with the identity
# recorded when it started.
#
#   S1  a normal run (all five checks pass): exit 0, the decoy untouched, the
#       run's own temporary directory removed
#   S2  the CLI auto-starts the N1 daemon and then fails before the script learns
#       its pid (cleanup adopts it from the owner lock): exit 1, the decoy
#       untouched, the auto-started daemon (exact pid from its own status
#       answer) stopped
#
# BOUNDARY_SCRIPT overrides the script under test (to run the regression against
# an older version). Needs stock sbx on PATH for startup probes; no VM is
# created. Hold the VM lock when running it.
# shellcheck disable=SC2329 # cleanup is invoked through traps
set -eu

prefix=${1:?usage: daemon_boundary_safety.sh FEATURE_PREFIX DEFAULT_MARSH}
default_marsh=${2:?usage: daemon_boundary_safety.sh FEATURE_PREFIX DEFAULT_MARSH}
prefix=$(cd "$prefix" && pwd -P)
default_marsh=$(cd "$(dirname "$default_marsh")" && pwd -P)/$(basename "$default_marsh")
here=$(cd "$(dirname "$0")" && pwd -P)
boundary=${BOUNDARY_SCRIPT:-$here/daemon_boundary.sh}
sbx=$(command -v sbx) || { echo "daemon_boundary_safety: stock sbx must be on PATH" >&2; exit 2; }

# All of this run's state lives under $base, created atomically here. Removing
# it, after its device and inode are checked against the ones recorded below, is
# the only deletion this script does. It never deletes daemon runtime files
# (/tmp/marsh-UID/...): a clean stop removes the decoy's socket, lock and token, and anything
# that remains is reported. Those paths are keyed by homes inside $base, an
# unguessable temporary name, so nobody else creates them unless they guess it.
#
# SAFETY_FIXED_BASE=PATH (test seam for daemon_boundary_preflight.sh): use PATH,
# which must not exist, instead of a random temporary directory. mkdir (without
# -p) fails if it exists, so a pre-existing path is never reused or deleted.
if [ -n "${SAFETY_FIXED_BASE:-}" ]; then
  mkdir "$SAFETY_FIXED_BASE"
  base=$SAFETY_FIXED_BASE
else
  base=$(mktemp -d "${TMPDIR:-/tmp}/marsh-boundary-safety.XXXXXX")
fi
base=$(cd "$base" && pwd -P)
chmod 700 "$base"
dir_id() { stat -c '%d:%i' "$1" 2>/dev/null || stat -f '%d:%i' "$1"; }
base_id=$(dir_id "$base")
spaced="$base/safety case"
mkdir "$spaced"
decoy_home="$base/safety/home"
mkdir -p "$decoy_home" "$base/safety/control"
chmod 700 "$decoy_home" "$base/safety/control"
decoy_pid=""
decoy_identity=""
candidate=""   # the decoy's runtime path, for the report only
failed=0

alive() { kill -0 "$1" 2>/dev/null; }
identity() { ps -ww -o lstart= -o command= -p "$1" 2>/dev/null | sed 's/^ *//'; }
runtime_dir() {
  python3 -c '
import hashlib, os, sys
home = os.path.realpath(sys.argv[1])
root = os.environ.get("BREAKS_RUNTIME_ROOT") or f"/tmp/marsh-{os.getuid()}"
print(f"{root}/{hashlib.sha256(home.encode()).hexdigest()[:16]}")' "$1"
}

cleaned=0
cleanup() {
  status=$?
  [ "$cleaned" = 0 ] || exit "$status"
  cleaned=1
  trap - EXIT INT TERM HUP
  # Only the decoy this script started, and only while its pid still has the
  # identity recorded when it started.
  if [ -n "$decoy_pid" ] && alive "$decoy_pid" && [ "$(identity "$decoy_pid")" = "$decoy_identity" ]; then
    kill -TERM "$decoy_pid" 2>/dev/null || true
    i=0
    while alive "$decoy_pid" && [ "$(identity "$decoy_pid")" = "$decoy_identity" ] && [ "$i" -lt 60 ]; do
      i=$((i + 1)); sleep 0.5
    done
    if alive "$decoy_pid" && [ "$(identity "$decoy_pid")" = "$decoy_identity" ]; then
      kill -KILL "$decoy_pid" 2>/dev/null || true
      sleep 1
    fi
    if alive "$decoy_pid" && [ "$(identity "$decoy_pid")" = "$decoy_identity" ]; then
      echo "daemon_boundary_safety: CLEANUP FAILED: could not stop decoy pid $decoy_pid" >&2
      status=1
    fi
  fi
  # Runtime paths that remain are reported, never removed.
  if [ -n "$candidate" ]; then
    for path in "$candidate" "$candidate.startup-lock"; do
      if [ -e "$path" ] || [ -L "$path" ]; then
        echo "daemon_boundary_safety: note: $path remains (not removed)" >&2
      fi
    done
  fi
  if [ "$(dir_id "$base")" = "$base_id" ] && [ ! -L "$base" ]; then
    rm -rf "$base" || status=1
  else
    echo "daemon_boundary_safety: CLEANUP FAILED: $base is no longer the directory this run created; not removing it" >&2
    status=1
  fi
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
trap 'exit 129' HUP

# Preflight: the runtime directory the decoy will create must not exist (a
# dangling symlink counts as existing). Refusing deletes nothing.
candidate=$(runtime_dir "$decoy_home")
for path in "$candidate" "$candidate.startup-lock"; do
  if [ -e "$path" ] || [ -L "$path" ]; then
    echo "daemon_boundary_safety: $path already exists; refusing to continue" >&2
    exit 2
  fi
done

# Start the decoy: a feature marshd for <base>/safety/home, owned by this script.
MARSH_HOME=$decoy_home MARSH_CONTROL_HOME="$base/safety/control" MARSH_SBX=$sbx \
  "$prefix/bin/marshd" --home "$decoy_home" >"$base/decoy.log" 2>&1 &
decoy_pid=$!
decoy_identity=$(identity "$decoy_pid")
i=0
until [ -S "$candidate/s" ] || ! alive "$decoy_pid"; do
  i=$((i + 1)); [ "$i" -le 120 ] || break; sleep 0.5
done
if ! [ -S "$candidate/s" ] || ! alive "$decoy_pid"; then
  echo "daemon_boundary_safety: the decoy did not start" >&2; cat "$base/decoy.log" >&2; exit 1
fi
echo "decoy: pid $decoy_pid, $decoy_identity"

# decoy_untouched CASE: still running, same identity, and still the answering
# daemon for its home.
decoy_untouched() {
  if ! alive "$decoy_pid" || [ "$(identity "$decoy_pid")" != "$decoy_identity" ]; then
    echo "FAIL $1: the decoy daemon (pid $decoy_pid) was stopped or replaced" >&2
    failed=1
    return
  fi
  answered=$(MARSH_HOME=$decoy_home MARSH_CONTROL_HOME="$base/safety/control" MARSH_SBX=$sbx \
    "$prefix/bin/marsh" status --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["endpoint_owner"]["pid"])')
  if [ "$answered" != "$decoy_pid" ]; then
    echo "FAIL $1: the decoy home is now answered by pid $answered, not $decoy_pid" >&2
    failed=1
    return
  fi
  echo "PASS $1: the decoy daemon (pid $decoy_pid) is untouched and still answering"
}

# S1: a normal run with the whitespace TMPDIR.
set +e
TMPDIR=$spaced sh "$boundary" "$prefix" "$default_marsh" >"$base/s1.log" 2>&1
s1=$?
set -e
if [ "$s1" != 0 ]; then echo "FAIL S1: the run under test exited $s1" >&2; tail -20 "$base/s1.log" >&2; failed=1; fi
decoy_untouched S1
if [ -n "$(ls -A "$spaced")" ]; then echo "FAIL S1: the run left files under $spaced" >&2; failed=1; fi

# S2: the CLI fails after auto-starting a daemon whose pid the script never saw.
fake="$base/fake"
mkdir -p "$fake/bin"
cp "$prefix/bin/marsh" "$fake/bin/marsh-real"
ln -s "$prefix/bin/marshd" "$fake/bin/marshd"
cat > "$fake/bin/marsh" <<EOF
#!/bin/sh
n=\$(cat "$base/count" 2>/dev/null || echo 0)
n=\$((n + 1))
echo "\$n" > "$base/count"
if [ "\$n" -ge 3 ]; then
  "$fake/bin/marsh-real" status --json > "$base/s2-status.json"
  exit 1
fi
exec "$fake/bin/marsh-real" "\$@"
EOF
chmod 755 "$fake/bin/marsh"
set +e
TMPDIR=$spaced sh "$boundary" "$fake" "$default_marsh" >"$base/s2.log" 2>&1
s2=$?
set -e
if [ "$s2" != 1 ]; then echo "FAIL S2: expected the injected failure (exit 1), got $s2" >&2; tail -20 "$base/s2.log" >&2; failed=1; fi
decoy_untouched S2
if [ ! -f "$base/s2-status.json" ]; then
  echo "FAIL S2: the failure was not injected after an auto-start" >&2; failed=1
else
  started=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["endpoint_owner"]["pid"])' "$base/s2-status.json")
  if alive "$started"; then
    echo "FAIL S2: the auto-started daemon (pid $started) was left running" >&2; failed=1
  else
    echo "PASS S2: the auto-started daemon (pid $started) was stopped by the run's cleanup"
  fi
fi
if [ -n "$(ls -A "$spaced")" ]; then echo "FAIL S2: the run left files under $spaced" >&2; failed=1; fi

exit "$failed"
