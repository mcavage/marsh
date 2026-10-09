#!/bin/sh
# The static, VM-free gate: formatting, lints, every Rust test, and the quick
# TLA+ sweep. No sbx, no VM, no registry. Run by `make verify-static`.
#
#   cargo fmt --check -> clippy (workspace, all targets) -> tests
#   docs/model/check.sh --quick                       (beside the cargo steps)
#
# Tests run under cargo-nextest when installed (every test binary at once, one
# process per test; the same tests, about half the wall time of `cargo test`),
# else `cargo test`. The quick sweep skips the configs marked slow in
# docs/model (`make regress` runs the full model). TLC results are cached in
# $TARGET_DIR/tlc-cache by spec/config/tool hash, so an unchanged model costs
# nothing; delete that directory to force a full quick sweep.
#
# VERIFY_STATIC_MODEL=0 skips the model; CARGO, TARGET_DIR, TLC_RUNNER=java,
# TLA2TOOLS_JAR, TLC_CORES are honored.
set -u
cd "$(dirname "$0")/.."
CARGO=${CARGO:-cargo}
TARGET_DIR=${TARGET_DIR:-target}
case "$TARGET_DIR" in /*) ;; *) TARGET_DIR=$PWD/$TARGET_DIR ;; esac

now() { date +%s; }
step() { # name command...
  _name=$1; shift; _t=$(now)
  echo "== $_name: $*"
  "$@"; _rc=$?
  echo "== $_name: $(( $(now) - _t ))s, exit $_rc"
  return $_rc
}

start=$(now)
model_pid=
if [ "${VERIFY_STATIC_MODEL:-1}" = 1 ]; then
  log=$(mktemp "${TMPDIR:-/tmp}/marsh-model.XXXXXX")
  trap 'rm -f "$log"' EXIT
  ( t=$(now); TLC_CACHE="$TARGET_DIR/tlc-cache" docs/model/check.sh --quick; rc=$?
    echo "== model: $(( $(now) - t ))s, exit $rc"; exit $rc ) >"$log" 2>&1 &
  model_pid=$!
fi

rc=0
step fmt "$CARGO" fmt --all -- --check &&
step clippy "$CARGO" clippy --workspace --all-targets --locked --target-dir "$TARGET_DIR" -- -D warnings &&
if "$CARGO" nextest --version >/dev/null 2>&1; then
  # nextest does not run doctests; the workspace has none today, but keep them covered.
  step tests "$CARGO" nextest run --workspace --locked --no-fail-fast --target-dir "$TARGET_DIR" &&
  step doctests "$CARGO" test --workspace --doc --locked --target-dir "$TARGET_DIR"
else
  echo "(cargo-nextest not installed: using cargo test; 'brew install cargo-nextest' makes this step about 2x faster)"
  step tests "$CARGO" test --workspace --locked --target-dir "$TARGET_DIR"
fi || rc=$?

if [ -n "$model_pid" ]; then
  wait "$model_pid" || rc=1
  cat "$log"
fi
echo "== verify-static: $(( $(now) - start ))s, $([ $rc -eq 0 ] && echo ok || echo FAILED)"
exit $rc
