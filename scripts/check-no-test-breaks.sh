#!/bin/sh
# Fail if any binary was built with the test-breaks feature. Checked per file:
#   1. the feature's provenance marker. Every shipped executable's build script
#      records it when the feature is enabled for that crate or for a marsh-*
#      crate it depends on (crates/marsh-provenance), whether or not any
#      test-only code is linked in; marsh-contracts also keeps it, and
#   2. the environment name MARSH_TEST_BREAK (defense in depth).
# A release artifact has neither (docs/design/test-breaks.md). This is the
# backstop to the build's provenance manifest (scripts/provenance.py), which
# does not depend on the binary's contents.
#
#   scripts/check-no-test-breaks.sh FILE...
set -eu

test $# -gt 0 || { echo "usage: check-no-test-breaks.sh FILE..." >&2; exit 2; }
status=0
for file in "$@"; do
  if ! test -f "$file"; then
    echo "check-no-test-breaks: missing $file" >&2
    status=1
    continue
  fi
  # Outside any `if` condition, so a failing strings(1) stops the script
  # instead of reading as "no match".
  text=$(strings -a "$file")
  if printf '%s\n' "$text" | grep -q "marsh-feature:test-breaks"; then
    echo "check-no-test-breaks: $file was built with test-breaks (the build recorded its provenance in the binary); refusing to package it" >&2
    status=1
  elif printf '%s\n' "$text" | grep -q MARSH_TEST_BREAK; then
    echo "check-no-test-breaks: $file contains MARSH_TEST_BREAK; refusing to package it" >&2
    status=1
  else
    echo "check-no-test-breaks: ok $file"
  fi
done
exit $status
