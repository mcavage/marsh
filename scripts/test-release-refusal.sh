#!/bin/sh
# Regression: release packaging refuses feature-enabled binaries however they
# were built (docs/design/test-breaks.md, "Releases"). Every case starts from a
# tree of ALL-NORMAL artifacts and changes exactly what its name says, so the
# refusal can only come from that change. Each case asserts that packaging
# fails and that the refusal NAMES the specific file (and no other file).
#
# Normal tree: default host binaries and default guest binaries, each with the
# provenance manifest its own build wrote.
#
#   CTRL     nothing changed                       -> packages (make dist)
#   X-<file> one real test-breaks artifact swapped in, for each of the nine
#            shipped files (host marsh, marshd, marsh-mcp; the six guest
#            binaries). The manifest still describes the normal build.
#            -> both layers name <file>: its manifest hash no longer matches,
#               and the binary carries the marker
#   M-<file> the same swap, with the feature build's own receipt (hashes match,
#            resolved features show test-breaks), for host marsh and the guest
#            worker                                -> the receipt names <file>
#   L-marsh  a feature-enabled executable with the build-script wiring but NO
#            live marsh-contracts reference, and a forged self-consistent clean
#            receipt                               -> the marker alone refuses it
#   P-marsh  the reviewer's synthetic: a feature-forwarding executable with no
#            live contracts reference and no wiring at all, so the binary check
#            says "ok"; its receipt is the tool's record of the real build
#                                                   -> the receipt alone refuses
#            it (through make dist)
#   G-REUSE  the round-3 group-reuse attack: a normal build recorded after the
#            feature probe under the probe's label. The tool refuses to rebind
#            the probe's artifact, groups are content-addressed and never
#            rewritten, and the probe stays refused (through make dist)
#   MAL-*    malformed or inconsistent receipts (missing group, no feature data,
#            a group relabelled clean under its old id, an artifact retargeted
#            to a clean group, a missing hash, unknown version, truncated file,
#            not an object), each on the feature probe, and a missing group on
#            all-normal artifacts                    -> refused, naming the files
#   R1       an --all-features host marsh AND a test-breaks guest worker together
#            (through make dist)                    -> both named, independently
#   NOMAN    the host receipt is missing            -> the host files are refused
#
# All state (artifacts, builds, probes, cases, logs, even TMPDIR for the tools it
# runs) lives under ONE root made atomically by mktemp. Removing that root,
# after its device and inode are checked against the ones recorded at creation,
# is the only deletion this script does.
#
# Needs cargo (the pinned toolchain) and docker with linux/arm64 builds. Takes
# several minutes on a cold cache.
#
#   scripts/test-release-refusal.sh
# shellcheck disable=SC2329 # cleanup is invoked through traps
set -eu

repo=$(cd "$(dirname "$0")/.." && pwd -P)
cd "$repo"
image=$(sed -n 's/^RUST_IMAGE ?= //p' Makefile)
work=$(mktemp -d "${TMPDIR:-/tmp}/marsh-release-refusal.XXXXXX")
work=$(cd "$work" && pwd -P)
dir_id() { stat -c '%d:%i' "$1" 2>/dev/null || stat -f '%d:%i' "$1"; }
work_id=$(dir_id "$work")
cleanup() {
  status=$?
  trap - EXIT INT TERM HUP
  if [ -d "$work" ] && [ ! -L "$work" ] && [ "$(dir_id "$work")" = "$work_id" ]; then
    rm -rf "$work" || status=1
  else
    echo "test-release-refusal: CLEANUP FAILED: $work is no longer the directory this run created; not removing it" >&2
    status=1
  fi
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
trap 'exit 129' HUP
art="$work/art"
mkdir "$art" "$work/tmp"
TMPDIR="$work/tmp"   # the tools below (dist.sh, make) stage their files inside the root too
export TMPDIR
failed=0

fail() { echo "FAIL $*" >&2; failed=1; }

# Receipt editing for the fixtures (tampering simulations); never used by packaging.
cat > "$work/mf.py" <<'PY'
import hashlib, json, sys

def gid(units):
    return "g-" + hashlib.sha256(json.dumps(units, sort_keys=True, separators=(",", ":")).encode()).hexdigest()[:16]

def load(path):
    return json.load(open(path))

def save(doc, path):
    json.dump(doc, open(path, "w"), indent=2)

op = sys.argv[1]
if op == "swap":          # swap BASE DONOR NAME OUT: BASE with NAME's entry and group taken from DONOR
    base, donor, name, out = load(sys.argv[2]), load(sys.argv[3]), sys.argv[4], sys.argv[5]
    entry = donor["artifacts"][name]
    base["groups"][entry["group"]] = donor["groups"][entry["group"]]
    base["artifacts"][name] = entry
    save(base, out)
elif op == "forge-clean":  # forge-clean IN NAME OUT: a self-consistent receipt with test-breaks removed
    doc, name, out = load(sys.argv[2]), sys.argv[3], sys.argv[4]
    entry = doc["artifacts"][name]
    group = doc["groups"].pop(entry["group"])
    group["units"] = {pkg: [f for f in feats if f != "test-breaks"] for pkg, feats in group["units"].items()}
    new = gid(group["units"])
    doc["groups"][new] = group
    entry["group"] = new
    save(doc, out)
elif op == "mutate":       # mutate IN NAME VARIANT OUT
    path, name, variant, out = sys.argv[2], sys.argv[3], sys.argv[4], sys.argv[5]
    if variant == "truncated":
        text = open(path).read()
        open(out, "w").write(text[: len(text) // 2])
        sys.exit(0)
    doc = load(path)
    entry = doc["artifacts"][name]
    if variant == "not-an-object":
        json.dump(["not", "a", "receipt"], open(out, "w"))
        sys.exit(0)
    if variant == "missing-group":
        del doc["groups"][entry["group"]]
    elif variant == "no-units":
        del doc["groups"][entry["group"]]["units"]
    elif variant == "relabeled-group":      # clean features under the old id
        group = doc["groups"][entry["group"]]
        group["units"] = {pkg: [f for f in feats if f != "test-breaks"] for pkg, feats in group["units"].items()}
    elif variant == "retargeted-artifact":  # point at another (clean) group
        other = next(g for g in doc["groups"] if g != entry["group"])
        entry["group"] = other
    elif variant == "missing-sha":
        del entry["sha256"]
    elif variant == "bad-version":
        doc["version"] = 1
    else:
        sys.exit("unknown variant " + variant)
    save(doc, out)
PY
mf() { python3 "$work/mf.py" "$@"; }

host_files="marsh marshd marsh-mcp"
guest_files="marsh-linux-arm64 marsh-local-linux-arm64 marsh-byte-exec-linux-arm64 marsh-worker-linux-arm64 marsh-relay-linux-arm64 marshd-linux-arm64"

build_all() {
echo "== building artifacts in $art"
# Normal host tree: the real build target writes the binaries and the manifest.
make build-host TARGET_DIR="$art/host-normal" >/dev/null 2>&1
# A real test-breaks host build, its own manifest (resolved features) included.
make build-host TARGET_DIR="$art/host-feat" HOST_FEATURES=test-breaks >/dev/null 2>&1
# The reviewer's R1 case: an --all-features host CLI.
cargo build -q -p marsh --bin marsh --release --locked --all-features --target-dir "$art/allfeat" 2>&1

docker buildx build --platform linux/arm64 --progress=quiet --build-arg RELEASE_PACKAGING=1 \
  --build-arg "RUST_IMAGE=$image" --target export --output "type=local,dest=$art/guest-normal" \
  -f packaging/linux-arm64/Dockerfile .
docker buildx build --platform linux/arm64 --progress=quiet --build-arg CARGO_FEATURES=test-breaks \
  --build-arg "RUST_IMAGE=$image" --target export --output "type=local,dest=$art/guest-feat" \
  -f packaging/linux-arm64/Dockerfile .

digest=$(printf 'a%.0s' $(seq 1 64))
printf '{"claude":"example.test/claude@sha256:%s","codex":"example.test/codex@sha256:%s","pi":"example.test/pi@sha256:%s","shell":"example.test/shell@sha256:%s"}\n' \
  "$digest" "$digest" "$digest" "$digest" > "$art/commands.json"
printf 'example.test/shell@sha256:%s\n' "$digest" > "$art/guest-normal/shell-image"
# dist.sh also requires the services engine image (S2); synthetic, like the shell image.
tar -cf "$art/guest-normal/svc-engine.tar" -T /dev/null
printf '{"synthetic":true}\n' > "$art/guest-normal/svc-engine-image.json"

# A normal marshd and marsh-mcp from a separate build, recorded by the same tool
# (for the group-reuse case).
python3 scripts/provenance.py build --manifest "$art/host-extra.json" --reset -- \
  cargo build -q --release --locked -p marsh-backend --bin marshd -p marsh-mcp --bin marsh-mcp --target-dir "$art/host-extra" >/dev/null 2>&1

# Synthetic feature-forwarding executables. Each declares
# `test-breaks = ["marsh-contracts/test-breaks"]` and compiles test-only
# behavior under it, but never calls anything in marsh-contracts, so the
# dependency is not linked. `wired` has the build script and embed! that every
# shipped marsh executable has; `unwired` has neither.
probe() {
  name=$1 wired=$2
  dir="$art/probe-$name"
  mkdir -p "$dir/src"
  {
    printf '[package]\nname = "probe-%s"\nversion = "0.0.0"\nedition = "2024"\npublish = false\n\n' "$name"
    printf '[workspace]\n\n[features]\ntest-breaks = ["marsh-contracts/test-breaks"]\n\n'
    printf '[dependencies]\nmarsh-contracts = { path = "%s/crates/marsh-contracts" }\n' "$repo"
    if [ "$wired" = 1 ]; then
      printf 'marsh-provenance = { path = "%s/crates/marsh-provenance" }\n\n' "$repo"
      printf '[build-dependencies]\nmarsh-provenance = { path = "%s/crates/marsh-provenance", features = ["build"] }\n' "$repo"
      printf 'fn main() {\n    marsh_provenance::build::emit();\n}\n' > "$dir/build.rs"
    fi
  } > "$dir/Cargo.toml"
  {
    printf 'fn main() {\n'
    [ "$wired" != 1 ] || printf '    let _ = marsh_provenance::embed!();\n'
    printf '    #[cfg(feature = "test-breaks")]\n    println!("test-only behavior is active");\n    #[cfg(not(feature = "test-breaks"))]\n    println!("normal behavior");\n}\n'
  } > "$dir/src/main.rs"
  # Built through the provenance wrapper: the receipt comes from cargo's report.
  python3 scripts/provenance.py build --manifest "$art/probe-$name.json" --reset --label "probe-$name" --rename "probe-$name=marsh" -- \
    cargo build --release --all-features --offline --manifest-path "$dir/Cargo.toml" --target-dir "$art/probe-target-$name" >/dev/null 2>&1
  cp "$art/probe-target-$name/release/probe-$name" "$art/probe-$name.bin"
}
probe wired 1
probe unwired 0
test "$("$art/probe-unwired.bin")" = "test-only behavior is active" || { fail "setup: unwired probe is not feature-enabled"; exit 1; }
test "$("$art/probe-wired.bin")" = "test-only behavior is active" || { fail "setup: wired probe is not feature-enabled"; exit 1; }
if strings -a "$art/probe-unwired.bin" | grep -q 'marsh-feature:test-breaks\|MARSH_TEST_BREAK'; then
  fail "setup: the unwired probe carries a marker, so it is not the reviewer's markerless case"; exit 1
fi
strings -a "$art/probe-wired.bin" | grep -q 'marsh-feature:test-breaks' ||
  { fail "setup: the wired probe does not carry the build-script marker"; exit 1; }

}
build_all

# new_case NAME [nomanifest]: an all-normal tree (symlinks to the normal artifacts, copies of
# the two manifests) at $work/cases/NAME/{host/release,guest}.
new_case() {
  root="$work/cases/$1"
  mkdir -p "$root/host/release" "$root/guest"
  for nf in $host_files; do ln -s "$art/host-normal/release/$nf" "$root/host/release/$nf"; done
  if [ "${2:-}" != nomanifest ]; then cp "$art/host-normal/release/provenance.json" "$root/host/release/provenance.json"; fi
  for nf in $guest_files shell-image svc-engine.tar svc-engine-image.json; do ln -s "$art/guest-normal/$nf" "$root/guest/$nf"; done
  ln -s "$art/guest-normal/licenses" "$root/guest/licenses"
  cp "$art/guest-normal/provenance.json" "$root/guest/provenance.json"
}

# swap CASE host|guest FILE SOURCE: replace one normal artifact by SOURCE.
swap() {
  case $2 in host) path="$work/cases/$1/host/release/$3" ;; *) path="$work/cases/$1/guest/$3" ;; esac
  ln -sfn "$4" "$path"
}

# run_dist CASE sh|make: package the case tree. The log is $work/CASE.log.
run_dist() {
  root="$work/cases/$1"
  set +e
  if [ "$2" = make ]; then
    make dist VERSION=1.2.3 TARGET_DIR="$root/host" GUEST_ARTIFACTS="$root/guest" \
      KIT_COMMANDS="$art/commands.json" DIST_DIR="$root/dist" >"$work/$1.log" 2>&1
  else
    TARGET_DIR="$root/host" GUEST_ARTIFACTS="$root/guest" KIT_COMMANDS="$art/commands.json" \
      sh scripts/dist.sh 1.2.3 "$root/dist" >"$work/$1.log" 2>&1
  fi
  status=$?
  set -e
  return $status
}

# starts CASE PREFIX: some line of the case's log starts with the literal PREFIX
# (paths may contain spaces or regex characters, so no pattern matching).
starts() { awk -v p="$2" 'index($0, p) == 1 { found = 1 } END { exit !found }' "$work/$1.log"; }

# Lines in which packaging refuses a file (not the "ok" lines), and the set of
# artifact paths of this case they name, relative to the case directory.
refusal_lines() { grep -E '^(check-no-test-breaks|provenance): ' "$work/$1.log" | grep -v -E '^[a-z-]+: ok ' || true; }
named_files() {
  refusal_lines "$1" | awk -v root="$work/cases/$1/" '
    { s = $0
      while ((i = index(s, root)) > 0) {
        s = substr(s, i + length(root))
        n = match(s, /[ :;]/)
        print (n ? substr(s, 1, n - 1) : s)
      } }' | grep -v 'provenance.json$' | sort -u || true
}

# refused CASE FILE...: packaging failed, and exactly these files were named.
refused() {
  name=$1; shift
  if [ "$status" = 0 ]; then fail "$name: packaging accepted the swapped artifact"; return; fi
  expected=$(for f in "$@"; do echo "${f#"$work/cases/$name/"}"; done | sort -u)
  named=$(named_files "$name")
  if [ "$named" = "$expected" ]; then
    echo "PASS $name: refused, naming exactly: $(echo "$expected" | tr '\n' ' ')"
  else
    fail "$name: refusal named {$(echo "$named" | tr '\n' ' ')}, expected {$(echo "$expected" | tr '\n' ' ')}"
    refusal_lines "$name" >&2
  fi
}

# layer CASE marker|manifest FILE: that layer's refusal line names FILE.
layer() {
  case $2 in
    marker)
      starts "$1" "check-no-test-breaks: $3 was built with test-breaks" ||
        starts "$1" "check-no-test-breaks: $3 contains MARSH_TEST_BREAK" || fail "$1: the $2 layer did not refuse $3" ;;
    manifest)
      starts "$1" "provenance: $3 was built with" || starts "$1" "provenance: $3 does not match" ||
        starts "$1" "provenance: $3 is not in" || fail "$1: the $2 layer did not refuse $3" ;;
  esac
}

H() { echo "$work/cases/$1/host/release/$2"; }
G() { echo "$work/cases/$1/guest/$2"; }

echo "== CTRL: all-normal artifacts must package (through make dist)"
new_case CTRL
if run_dist CTRL make && test -f "$work/cases/CTRL/dist/marsh-1.2.3-darwin-arm64.tar.gz"; then
  if ! { grep -qF "provenance: ok $(H CTRL marsh)" "$work/CTRL.log" &&
         grep -qF "provenance: ok $(G CTRL marsh-worker-linux-arm64)" "$work/CTRL.log"; }; then
    fail "CTRL: packaged, but the manifest was not checked"
  fi
  echo "PASS CTRL: normal artifacts packaged"
else
  fail "CTRL: normal artifacts were refused"
  grep -E '^(check-no-test-breaks|provenance|dist):' "$work/CTRL.log" >&2 || true
fi

echo "== X-<file>: one test-breaks artifact swapped in, manifest unchanged"
for f in $host_files; do
  new_case "X-$f"; swap "X-$f" host "$f" "$art/host-feat/release/$f"
  run_dist "X-$f" sh || true
  refused "X-$f" "$(H "X-$f" "$f")"
  layer "X-$f" marker "$(H "X-$f" "$f")"; layer "X-$f" manifest "$(H "X-$f" "$f")"
done
for f in $guest_files; do
  new_case "X-$f"; swap "X-$f" guest "$f" "$art/guest-feat/$f"
  run_dist "X-$f" sh || true
  refused "X-$f" "$(G "X-$f" "$f")"
  layer "X-$f" marker "$(G "X-$f" "$f")"; layer "X-$f" manifest "$(G "X-$f" "$f")"
done

echo "== M-<file>: the feature build's own receipt (hashes match; resolved features)"
new_case M-marsh; swap M-marsh host marsh "$art/host-feat/release/marsh"
mf swap "$art/host-normal/release/provenance.json" "$art/host-feat/release/provenance.json" marsh "$work/cases/M-marsh/host/release/provenance.json"
run_dist M-marsh sh || true
refused M-marsh "$(H M-marsh marsh)"
layer M-marsh marker "$(H M-marsh marsh)"
starts M-marsh "provenance: $(H M-marsh marsh) was built with test-breaks" || fail "M-marsh: the receipt did not name marsh"
grep -q "does not match" "$work/M-marsh.log" && fail "M-marsh: refused on a hash mismatch, not on the resolved features"

new_case M-worker; swap M-worker guest marsh-worker-linux-arm64 "$art/guest-feat/marsh-worker-linux-arm64"
mf swap "$art/guest-normal/provenance.json" "$art/guest-feat/provenance.json" marsh-worker-linux-arm64 "$work/cases/M-worker/guest/provenance.json"
run_dist M-worker sh || true
refused M-worker "$(G M-worker marsh-worker-linux-arm64)"
layer M-worker marker "$(G M-worker marsh-worker-linux-arm64)"
starts M-worker "provenance: $(G M-worker marsh-worker-linux-arm64) was built with test-breaks" || fail "M-worker: the receipt did not name the worker"
grep -q "does not match" "$work/M-worker.log" && fail "M-worker: refused on a hash mismatch, not on the resolved features"

echo "== L-marsh: wired probe, forged self-consistent clean receipt: only the marker can refuse it"
new_case L-marsh; swap L-marsh host marsh "$art/probe-wired.bin"
mf swap "$art/host-normal/release/provenance.json" "$art/probe-wired.json" marsh "$work/L.honest.json"
mf forge-clean "$work/L.honest.json" marsh "$work/cases/L-marsh/host/release/provenance.json"
run_dist L-marsh sh || true
refused L-marsh "$(H L-marsh marsh)"
layer L-marsh marker "$(H L-marsh marsh)"
starts L-marsh "provenance: ok $(H L-marsh marsh)" || fail "L-marsh: the forged receipt was not accepted, so the marker layer was not isolated"

echo "== P-marsh: the reviewer's synthetic (unwired, no live contracts reference), through make dist"
new_case P-marsh; swap P-marsh host marsh "$art/probe-unwired.bin"
mf swap "$art/host-normal/release/provenance.json" "$art/probe-unwired.json" marsh "$work/cases/P-marsh/host/release/provenance.json"
run_dist P-marsh make || true
refused P-marsh "$(H P-marsh marsh)"
layer P-marsh manifest "$(H P-marsh marsh)"
starts P-marsh "check-no-test-breaks: ok $(H P-marsh marsh)" || fail "P-marsh: the binary check flagged it, so the receipt layer was not isolated"

echo "== G-REUSE: a normal build recorded after the feature probe, with the probe's label; through make dist"
# The round-3 attack: record a clean build "under the same group" as the probe.
# Groups are content-addressed and never rewritten, so the probe's group keeps
# its features and the artifact keeps its group.
cp "$art/probe-unwired.json" "$work/G.json"
if python3 scripts/provenance.py build --manifest "$work/G.json" -- \
     cargo build -q --release --locked -p marsh --bin marsh --target-dir "$art/host-extra" >"$work/G.rebind.log" 2>&1; then
  fail "G-REUSE: the tool rebound an artifact name that was already recorded"
elif ! grep -q "refusing to rebind" "$work/G.rebind.log"; then
  fail "G-REUSE: the rebind failed, but not on the rebind refusal"
fi
python3 scripts/provenance.py build --manifest "$work/G.json" --label "probe-unwired" -- \
  cargo build -q --release --locked -p marsh-backend --bin marshd -p marsh-mcp --bin marsh-mcp --target-dir "$art/host-extra" >/dev/null 2>&1
new_case G-REUSE; swap G-REUSE host marsh "$art/probe-unwired.bin"
swap G-REUSE host marshd "$art/host-extra/release/marshd"; swap G-REUSE host marsh-mcp "$art/host-extra/release/marsh-mcp"
cp "$work/G.json" "$work/cases/G-REUSE/host/release/provenance.json"
run_dist G-REUSE make || true
refused G-REUSE "$(H G-REUSE marsh)"
layer G-REUSE manifest "$(H G-REUSE marsh)"
starts G-REUSE "provenance: ok $(H G-REUSE marshd)" || fail "G-REUSE: the normal marshd was not accepted"

echo "== MAL-*: malformed or inconsistent receipts fail closed (the honest feature-probe receipt, then damaged)"
# Group-level damage names the file whose build group is damaged; whole-file
# damage names every host file.
mal() {
  variant=$1 expect=$2   # expect: marsh (one file) | all (every host file)
  new_case "MAL-$variant"; swap "MAL-$variant" host marsh "$art/probe-unwired.bin"
  mf swap "$art/host-normal/release/provenance.json" "$art/probe-unwired.json" marsh "$work/MAL-$variant.honest.json"
  mf mutate "$work/MAL-$variant.honest.json" marsh "$variant" "$work/cases/MAL-$variant/host/release/provenance.json"
  run_dist "MAL-$variant" sh || true
  if [ "$expect" = marsh ]; then refused "MAL-$variant" "$(H "MAL-$variant" marsh)"
  else refused "MAL-$variant" "$(H "MAL-$variant" marsh)" "$(H "MAL-$variant" marshd)" "$(H "MAL-$variant" marsh-mcp)"; fi
}
mal missing-group marsh
mal no-units marsh
mal relabeled-group marsh
mal retargeted-artifact marsh
mal missing-sha marsh
mal bad-version all
mal truncated all
mal not-an-object all
# The same damage on an all-normal tree must also refuse (nothing is trusted by default).
new_case MAL-normal-missing-group
mf mutate "$art/host-normal/release/provenance.json" marsh missing-group "$work/cases/MAL-normal-missing-group/host/release/provenance.json"
run_dist MAL-normal-missing-group make || true
refused MAL-normal-missing-group "$(H MAL-normal-missing-group marsh)" "$(H MAL-normal-missing-group marshd)" "$(H MAL-normal-missing-group marsh-mcp)"

echo "== R1: --all-features host marsh and a test-breaks guest worker together, through make dist"
new_case R1
swap R1 host marsh "$art/allfeat/release/marsh"
swap R1 guest marsh-worker-linux-arm64 "$art/guest-feat/marsh-worker-linux-arm64"
run_dist R1 make || true
refused R1 "$(H R1 marsh)" "$(G R1 marsh-worker-linux-arm64)"
for side in marker manifest; do
  layer R1 $side "$(H R1 marsh)"; layer R1 $side "$(G R1 marsh-worker-linux-arm64)"
done

echo "== NOMAN: no host manifest"
new_case NOMAN nomanifest
run_dist NOMAN sh || true
refused NOMAN "$(H NOMAN marsh)" "$(H NOMAN marshd)" "$(H NOMAN marsh-mcp)"

if [ "$failed" = 0 ]; then echo "ALL PASS"; else echo "FAILURES" >&2; fi
exit $failed
