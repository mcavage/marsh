#!/bin/sh
# The services crates' test-breaks features forward to marsh-contracts/test-breaks,
# so release packaging (MARSH_RELEASE_PACKAGING=1) must refuse every one of them at
# compile time (SECURITY-DESIGN A10), and must still build them without the feature.
#
#   tests/breaks/services_release_guard.sh
set -eu
cd "$(dirname "$0")/../.."
target=${CARGO_TARGET_DIR:-target}/release-guard
log=$(mktemp)
trap 'rm -f "$log"' EXIT
status=0

refuses() {
  if MARSH_RELEASE_PACKAGING=1 CARGO_TARGET_DIR=$target cargo check --locked "$@" >"$log" 2>&1; then
    echo "FAIL release packaging accepted: cargo check $*" >&2
    status=1
  elif grep -q "the test-breaks feature is for test builds only" "$log"; then
    echo "PASS refused: cargo check $*"
  else
    echo "FAIL failed for another reason: cargo check $*" >&2
    tail -5 "$log" >&2
    status=1
  fi
}

builds() {
  if MARSH_RELEASE_PACKAGING=1 CARGO_TARGET_DIR=$target cargo check --locked "$@" >"$log" 2>&1; then
    echo "PASS builds: cargo check $*"
  else
    echo "FAIL release packaging refused a build without the feature: cargo check $*" >&2
    tail -5 "$log" >&2
    status=1
  fi
}

refuses -p marsh-services --features test-breaks
refuses -p marsh-sbx --features test-breaks
refuses -p marsh --features test-breaks
refuses -p marsh-backend --features test-breaks
refuses --workspace --all-features
builds -p marsh-services
builds -p marsh-sbx
builds -p marsh-backend
exit $status
