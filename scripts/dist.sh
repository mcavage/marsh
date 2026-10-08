#!/bin/sh
# Assemble a release tarball from already-built artifacts.
#
#   scripts/dist.sh VERSION OUTDIR
#
# Inputs (environment, with the Makefile's defaults):
#   TARGET_DIR       host build dir with release/{marsh,marshd,marsh-mcp}
#   GUEST_ARTIFACTS  linux/arm64 guest binaries and licenses/ (make build-linux)
#   KIT_COMMANDS     command registry; every value must be repository@sha256:...
#   SHELL_IMAGE      file holding the published shell image reference
#   MAN_DIR          rendered man pages (make man)
#   NOTICES_DIR      host notices (scripts/third-party-notices.py)
#   LOCAL            1 = local build (`make dist-local`): unpinned source Kits
#                    and a local shell image are allowed; the tarball is named
#                    marsh-VERSION-local-darwin-arm64 and marked as a local
#                    build (LOCAL-BUILD.txt, libexec/marsh/local-build, and
#                    `marsh --version`). It only works where that shell image
#                    is in the stock SBX image store (the building Mac).
#
# Output: OUTDIR/marsh-VERSION-darwin-arm64.tar.gz and its .sha256 file.
# The tarball unpacks to marsh-VERSION-darwin-arm64/{bin,libexec,share}, the
# same layout `make install` writes under PREFIX.
set -eu

version=${1:?usage: dist.sh VERSION OUTDIR}
outdir=${2:?usage: dist.sh VERSION OUTDIR}
TARGET_DIR=${TARGET_DIR:-target}
GUEST_ARTIFACTS=${GUEST_ARTIFACTS:-$TARGET_DIR/libexec/marsh}
KIT_COMMANDS=${KIT_COMMANDS:-$TARGET_DIR/kit-release/commands.json}
SHELL_IMAGE=${SHELL_IMAGE:-$GUEST_ARTIFACTS/shell-image}
MAN_DIR=${MAN_DIR:-$TARGET_DIR/man}
NOTICES_DIR=${NOTICES_DIR:-$TARGET_DIR/notices}

die() { echo "dist: $*" >&2; exit 1; }

case $version in
  [0-9]*.[0-9]*.[0-9]*) ;;
  *) die "VERSION must look like 1.2.3 (got $version)" ;;
esac

LOCAL=${LOCAL:-0}
if test "$LOCAL" = 1; then
  test -f "$KIT_COMMANDS" || die "missing $KIT_COMMANDS (make dev-artifacts)"
  test -s "$SHELL_IMAGE" || die "missing $SHELL_IMAGE (make dev-artifacts)"
else
# A release registry pins every Kit by digest; a source path would make the
# install build Kits on the user's Mac.
python3 - "$KIT_COMMANDS" <<'EOF' || die "$KIT_COMMANDS must map every command to repository@sha256:DIGEST (release Kits are published by CI). For an installable tarball from your local dev artifacts, run: make dist-local"
import json, re, sys
commands = json.load(open(sys.argv[1]))
ok = isinstance(commands, dict) and commands and all(
    isinstance(v, str) and re.fullmatch(r"[a-z0-9][a-z0-9._:/-]*@sha256:[0-9a-f]{64}", v)
    for v in commands.values())
sys.exit(0 if ok else 1)
EOF
grep -Eq '^[a-z0-9][a-z0-9._:/-]*@sha256:[0-9a-f]{64}$' "$SHELL_IMAGE" ||
  die "$SHELL_IMAGE must hold one repository@sha256:DIGEST line (the published shell image). For a local tarball, run: make dist-local"
fi

if test "$LOCAL" = 1; then
  name="marsh-$version-local-darwin-arm64"
else
  name="marsh-$version-darwin-arm64"
fi
stage=$(mktemp -d "${TMPDIR:-/tmp}/marsh-dist.XXXXXX")
trap 'rm -rf "$stage"' EXIT
root="$stage/$name"
mkdir -p "$root/bin" "$root/libexec/marsh" "$root/share/man/man1" "$root/share/licenses/marsh"

for binary in marsh marshd marsh-mcp; do
  test -x "$TARGET_DIR/release/$binary" || die "missing $TARGET_DIR/release/$binary (make build-host)"
  install -m 755 "$TARGET_DIR/release/$binary" "$root/bin/$binary"
done
ln -s marsh "$root/bin/msh"

for guest in marsh marsh-local marsh-byte-exec marsh-worker marsh-relay marshd; do
  test -f "$GUEST_ARTIFACTS/$guest-linux-arm64" || die "missing $GUEST_ARTIFACTS/$guest-linux-arm64 (make build-linux)"
  install -m 755 "$GUEST_ARTIFACTS/$guest-linux-arm64" "$root/libexec/marsh/$guest-linux-arm64"
done
install -m 644 "$KIT_COMMANDS" "$root/libexec/marsh/commands.json"
install -m 644 packaging/agents.json "$root/libexec/marsh/agents.json"
install -m 644 "$SHELL_IMAGE" "$root/libexec/marsh/shell-image"
if test -f "$SHELL_IMAGE.build.json"; then
  install -m 644 "$SHELL_IMAGE.build.json" "$root/libexec/marsh/shell-image.build.json"
fi
test -d "$GUEST_ARTIFACTS/licenses" || die "missing $GUEST_ARTIFACTS/licenses"
cp -R "$GUEST_ARTIFACTS/licenses" "$root/libexec/marsh/licenses"
if test "$LOCAL" = 1; then
  # Source Kits (commands.json names kits/NAME); never the dev marker.
  if test -d "$GUEST_ARTIFACTS/kits"; then
    cp -R "$GUEST_ARTIFACTS/kits" "$root/libexec/marsh/kits"
  fi
  revision=$(git rev-parse --short HEAD 2>/dev/null || echo unknown)
  if test -n "$(git status --porcelain 2>/dev/null)"; then revision="$revision+dirty"; fi
  note="$revision $(date -u +%Y-%m-%d)"
  printf '%s\n' "$note" > "$root/libexec/marsh/local-build"
  {
    echo "marsh $version LOCAL BUILD ($note) - not a release."
    echo
    echo "Built by 'make dist-local' from a source checkout. Kits are unpinned source"
    echo "directories (built on first use), and the shell image is a local image that"
    echo "exists only in the stock SBX image store of the Mac that built it:"
    cat "$SHELL_IMAGE"
    echo "Install it on that Mac only. 'marsh --version' says \"local build\"."
  } > "$root/LOCAL-BUILD.txt"
fi

for page in "$MAN_DIR"/*.1; do
  test -f "$page" || die "no man pages in $MAN_DIR (make man)"
  install -m 644 "$page" "$root/share/man/man1/"
done

install -m 644 LICENSE NOTICE THIRD_PARTY.md "$root/share/licenses/marsh/"
for notice in THIRD-PARTY-NOTICES.txt rust-package-notices.json embedded-native-notices.json; do
  test -f "$NOTICES_DIR/$notice" || die "missing $NOTICES_DIR/$notice (scripts/third-party-notices.py)"
  install -m 644 "$NOTICES_DIR/$notice" "$root/share/licenses/marsh/$notice"
done
install -m 644 README.md LICENSE NOTICE "$root/"
test ! -e "$root/libexec/marsh/dev-enabled" || die "refusing to package the dev-enabled marker"

mkdir -p "$outdir"
tarball="$outdir/$name.tar.gz"
# Stable member order and ownership so the same inputs give the same bytes.
(cd "$stage" && find "$name" -print | LC_ALL=C sort > "$stage/files")
COPYFILE_DISABLE=1 tar -C "$stage" -czf "$tarball" --uid 0 --gid 0 --uname root --gname wheel \
  --no-recursion -T "$stage/files" 2>/dev/null ||
  COPYFILE_DISABLE=1 tar -C "$stage" -czf "$tarball" --owner 0 --group 0 --no-recursion -T "$stage/files"
(cd "$outdir" && shasum -a 256 "$name.tar.gz" > "$name.tar.gz.sha256")
echo "$tarball"
cat "$tarball.sha256"
