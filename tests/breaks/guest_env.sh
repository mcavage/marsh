#!/bin/sh
# Guest boundary: feature-built Linux guest binaries never read break modes
# from their environment (docs/design/test-breaks.md, "The boundary").
#
#   tests/breaks/guest_env.sh GUEST_DIR
#
# GUEST_DIR holds *-linux-arm64 binaries built with the test-breaks feature
# (`docker buildx build --build-arg CARGO_FEATURES=test-breaks ...`, export
# target). The script runs the guest marshd, launched directly with
# MARSH_TEST_BREAK=demo and the harness flag `--test-breaks-from-env` (the
# strongest selection a host daemon honors), and a guest marsh with the same
# variable, in a linux/arm64 container. Stock SBX is replaced by a fake that answers only the
# startup probes, so no VM is created. Expected: status reports no test_breaks.
set -eu

guest=${1:?usage: guest_env.sh GUEST_DIR}
guest=$(cd "$guest" && pwd -P)
for binary in marsh-linux-arm64 marshd-linux-arm64; do
  test -f "$guest/$binary" || { echo "guest_env: missing $guest/$binary" >&2; exit 2; }
done
repo=$(cd "$(dirname "$0")/../.." && pwd -P)
image=${RUST_IMAGE:-$(sed -n 's/^RUST_IMAGE ?= //p' "$repo/Makefile")}

# All state lives under $work, made atomically by mktemp. Removing it, after its
# device and inode are checked against the ones recorded here, is the only
# deletion this script does.
work=$(mktemp -d "${TMPDIR:-/tmp}/marsh-breaks-guest.XXXXXX")
work=$(cd "$work" && pwd -P)
dir_id() { stat -c '%d:%i' "$1" 2>/dev/null || stat -f '%d:%i' "$1"; }
work_id=$(dir_id "$work")
cleanup() {
  status=$?
  trap - EXIT INT TERM HUP
  if [ -d "$work" ] && [ ! -L "$work" ] && [ "$(dir_id "$work")" = "$work_id" ]; then
    rm -rf "$work" || status=1
  else
    echo "guest_env: CLEANUP FAILED: $work is no longer the directory this run created; not removing it" >&2
    status=1
  fi
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
trap 'exit 129' HUP
mkdir "$work/in" "$work/out"
cp "$guest/marsh-linux-arm64" "$guest/marshd-linux-arm64" "$work/in/"

cat > "$work/in/sbx" <<'EOF'
#!/bin/sh
# Fake stock SBX: startup probes only. Never creates a VM.
case "$1" in
  version) echo "sbx version v0.51.0 0123456789abcdef0123456789abcdef01234567" ;;
  create) echo "Usage: sbx create ... sandbox kit reference" ;;
  *) echo "[]" ;;
esac
EOF

cat > "$work/in/run.sh" <<'EOF'
#!/bin/sh
set -eu
mkdir -p /w/bin /w/libexec/marsh /w/scope
cp /in/marsh-linux-arm64 /w/bin/marsh && cp /in/marshd-linux-arm64 /w/bin/marshd && cp /in/sbx /w/bin/sbx
chmod 755 /w/bin/*
digest=$(printf 'a%.0s' $(seq 1 64))
printf '{"shell":"example.test/shell@sha256:%s"}\n' "$digest" > /w/libexec/marsh/commands.json
printf 'example.test/shell@sha256:%s\n' "$digest" > /w/libexec/marsh/shell-image
export MARSH_SBX=/w/bin/sbx MARSH_HOME=/w/scope MARSH_CONTROL_HOME=/w/control
MARSH_TEST_BREAK=demo /w/bin/marshd --home /w/scope --test-breaks-from-env > /w/out/daemon.log 2>&1 &
pid=$!
# Wait for the launched daemon's endpoint (the CLI must not reach a daemon it started).
i=0
until ls /tmp/marsh-0/*/s >/dev/null 2>&1 || ! kill -0 "$pid" 2>/dev/null; do
  i=$((i + 1)); test "$i" -le 60 || break; sleep 1
done
kill -0 "$pid" 2>/dev/null || { echo "guest_env: launched daemon exited" >&2; cat /w/out/daemon.log >&2; exit 1; }
MARSH_TEST_BREAK=demo /w/bin/marsh status --json > /w/out/status.json
python3 - <<'PY'
import json
status = json.load(open("/w/out/status.json"))
print("guest status (daemon had the flag and MARSH_TEST_BREAK=demo; CLI too):", status.get("test_breaks", "<absent>"))
raise SystemExit(0 if "test_breaks" not in status else 1)
PY
/w/bin/marsh stop > /w/out/stop.txt 2>&1 || true
EOF
chmod 755 "$work/in/sbx" "$work/in/run.sh"

docker run --rm --platform linux/arm64 -v "$work/in:/in:ro" -v "$work:/w" \
  -e HOME=/root -e USER=root -e LOGNAME=root "$image" sh /in/run.sh
echo "PASS guest_env: no test_breaks reported by the feature-built guest"
