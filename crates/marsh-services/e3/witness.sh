#!/usr/bin/env bash
# In-process witnesses (no VM): every test must pass as shipped, and fail when
# the defect it guards is put back (`E3_TEST_BREAKS`, read only by the unit-test
# build). Exits nonzero if a witness passes under its break, or fails without.
#
#   e3/witness.sh            all rows
#   e3/witness.sh normal     only the unbroken run
#
# Tests that need Linux (poll semantics) report SKIP elsewhere; run this inside
# the E3 build container (e3/test-linux.sh) for them.
set -uo pipefail
export CARGO_INCREMENTAL=0
cd "$(dirname "$0")/../../.."

P=policy::endpoint::tests
T=policy::tests
L=ledger::tests
ROWS=(
  "$L::a_failed_write_is_an_error_and_changes_nothing ledger-persist-ignore"
  "$L::a_failed_rename_is_an_error_too ledger-persist-ignore"
  "$L::a_full_disk_is_an_error_and_changes_nothing ledger-persist-ignore"
  "$L::concurrent_claims_have_exactly_one_winner volume-claim-racy"
  "$P::a_silent_hijacked_stream_is_closed_at_the_idle_limit hijack-deadlines-off"
  "$P::a_vanished_client_frees_the_slot hijack-deadlines-off"
  "$P::a_client_that_stops_reading_is_cut hijack-deadlines-off"
  "$P::the_project_cap_counts_every_workspace_root project-cap-per-root"
  "$P::a_ledger_write_failure_refuses_and_leaves_no_engine_object ledger-persist-ignore"
  "$P::a_freeze_waits_for_a_mutation_whose_body_is_still_arriving freeze-no-wait"
  "$P::a_freeze_waits_for_a_mutation_whose_body_is_still_arriving capture-api-gate-off"
  "$P::a_freeze_waits_for_a_mutation_stuck_in_the_engine freeze-no-wait"
  "$P::a_list_that_cannot_be_filtered_fails_closed list-fail-open"
  "$P::a_list_that_cannot_be_filtered_fails_closed lane-authority-off"
  "$P::volumes_from_keeps_the_pin_alive_after_the_original_is_deleted pin-inherit-off"
  "$P::volumes_from_keeps_the_pin_alive_after_the_original_is_deleted network-name-forwarded"
  "$T::non_recursive_bind_mounts_are_refused bind-nonrecursive-allowed"
  "$T::private_propagation_is_refused_everywhere private-propagation-allowed"
  "$T::commits_go_only_into_the_workspaces_own_repository commit-any-repo"
  "$T::update_limits_are_validated_after_merging_with_the_current_ones update-limits-unmerged"
  "$T::update_refuses_every_spelling_of_unlimited services-resource-limit-off"
  "$T::update_refuses_every_spelling_of_unlimited update-limits-unmerged"
  "$T::update_does_not_change_the_restart_policy restart-update-allowed"
  "$T::network_create_takes_no_options network-mtu-allowed"
  "$T::build_outputs_is_a_closed_schema_rebuilt_before_forwarding outputs-open"
  "$T::names_that_look_like_a_full_id_are_refused fullid-name-allowed"
  "$T::networks_are_forwarded_by_id network-name-forwarded"
  "$T::volumes_from_pins_are_inherited_by_the_new_container pin-inherit-off"
  "$T::anonymous_volumes_become_labelled_mounts anon-volume-unlabeled"
)

PASS=0; FAIL=0; SKIP=0
run() { # run TEST [BREAK]  -> sets RC, OUT
  OUT=$(E3_TEST_BREAKS="${2:-}" cargo test -p marsh-services --lib -- --exact "$1" 2>&1); RC=$?
}
ran_none() { grep -q "running 0 tests" <<<"$OUT"; }
ok()   { PASS=$((PASS+1)); printf 'ok   %s\n' "$1"; }
bad()  { FAIL=$((FAIL+1)); printf 'BAD  %s\n' "$1"; printf '%s\n' "$OUT" | tail -15 | sed 's/^/       /'; }

echo "== the whole suite, as shipped"
OUT=$(cargo test -p marsh-services --lib 2>&1); RC=$?
if [ $RC -eq 0 ]; then ok "unit suite: $(grep -oE '[0-9]+ passed' <<<"$OUT" | head -1)"; else bad "unit suite fails without any break"; fi
[ "${1:-}" = normal ] && { echo "== pass=$PASS fail=$FAIL"; [ $FAIL -eq 0 ]; exit; }

echo "== each witness under its defect"
for row in "${ROWS[@]}"; do
  t=${row%% *}; b=${row##* }
  run "$t"
  if ran_none; then SKIP=$((SKIP+1)); printf 'SKIP %-70s (not on this platform)\n' "${t##*::}"; continue; fi
  [ $RC -eq 0 ] || { bad "$t fails without a break"; continue; }
  run "$t" "$b"
  if [ $RC -ne 0 ] && grep -q "test result: FAILED. 0 passed; 1 failed" <<<"$OUT"; then
    ok "$(printf '%-62s fails under %s' "${t##*::}" "$b")"
  else
    bad "$t PASSED under $b: the witness does not detect the defect"
  fi
done
echo "== pass=$PASS fail=$FAIL skip=$SKIP"
[ $FAIL -eq 0 ]
