#!/bin/sh
# Install marsh — marshal your agents.
#
#   curl -fsSL https://runmar.sh/install | sh
#   curl -fsSL https://runmar.sh/install | sh -s -- --prefix /usr/local
#
# Options (or environment):
#   --prefix DIR     install under DIR (MARSH_PREFIX; default ~/.local)
#   --version X.Y.Z  install that release (MARSH_VERSION; default the latest)
#   --tarball FILE   install from a local release tarball; FILE.sha256 must
#                    sit beside it (MARSH_TARBALL; for testing)
#   --yes            do not ask before using sudo
#
# The script downloads marsh-VERSION-darwin-arm64.tar.gz and its .sha256 from
# GitHub Releases, checks the checksum, and copies bin/, libexec/marsh/ and
# share/ into the prefix. It changes nothing else: no shell profile edits, no
# sbx changes, no background services.
set -eu

REPO=${MARSH_REPO:-mcavage/marsh}
PREFIX=${MARSH_PREFIX:-$HOME/.local}
VERSION=${MARSH_VERSION:-}
TARBALL=${MARSH_TARBALL:-}
YES=0
SBX_MIN=0.45.0
SBX_TESTED=0.46.0

say() { printf '%s\n' "$*"; }
warn() { printf 'marsh install: warning: %s\n' "$*" >&2; }
die() { printf 'marsh install: %s\n' "$*" >&2; exit 1; }
usage() {
  cat <<'USAGE'
Install marsh (marshal your agents) on an Apple Silicon Mac.

  curl -fsSL https://runmar.sh/install | sh -s -- [OPTIONS]

  --prefix DIR     install under DIR (default ~/.local)
  --version X.Y.Z  install that release (default: the latest)
  --tarball FILE   install from a local release tarball, FILE.sha256 beside it
  --yes            use sudo without asking when DIR is not writable
USAGE
}

while [ $# -gt 0 ]; do
  case $1 in
    --prefix) [ $# -ge 2 ] || die "--prefix needs a directory"; PREFIX=$2; shift 2 ;;
    --prefix=*) PREFIX=${1#--prefix=}; shift ;;
    --version) [ $# -ge 2 ] || die "--version needs X.Y.Z"; VERSION=$2; shift 2 ;;
    --version=*) VERSION=${1#--version=}; shift ;;
    --tarball) [ $# -ge 2 ] || die "--tarball needs a file"; TARBALL=$2; shift 2 ;;
    --yes|-y) YES=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) die "unknown option: $1 (try --help)" ;;
  esac
done
VERSION=${VERSION#v}

# version_ge A B: true when dotted version A >= B (numeric fields).
version_ge() {
  # shellcheck disable=SC2086
  set -- $(printf '%s %s' "$1" "$2" | tr '.' ' ')
  [ "$1" -gt "$4" ] && return 0; [ "$1" -lt "$4" ] && return 1
  [ "$2" -gt "$5" ] && return 0; [ "$2" -lt "$5" ] && return 1
  [ "$3" -ge "$6" ]
}

# --- Prerequisites ---------------------------------------------------------

[ "$(uname -s)" = Darwin ] || die "marsh runs on macOS on Apple Silicon. This is $(uname -s)."
arch=$(uname -m)
if [ "$arch" != arm64 ]; then
  if [ "$(sysctl -n hw.optional.arm64 2>/dev/null || echo 0)" = 1 ]; then
    die "this shell runs under Rosetta ($arch). Run the installer from a native arm64 terminal."
  fi
  die "marsh needs an Apple Silicon Mac (found $arch)."
fi
for tool in curl tar shasum mktemp; do
  command -v "$tool" >/dev/null 2>&1 || die "missing $tool"
done
case $PREFIX in /*) ;; *) die "--prefix must be an absolute path (got $PREFIX)" ;; esac

sbx_note=
if sbx_path=$(command -v sbx 2>/dev/null); then
  sbx_line=$(sbx version 2>/dev/null | sed -n 's/^sbx version: v\{0,1\}\([0-9][0-9]*\.[0-9][0-9]*\.[0-9][0-9]*\).*/\1/p' | head -n 1)
  if [ -z "$sbx_line" ]; then
    warn "could not read the version of $sbx_path; marsh needs Docker Sandboxes (sbx) $SBX_MIN or newer."
  elif ! version_ge "$sbx_line" "$SBX_MIN"; then
    die "Docker Sandboxes (sbx) $sbx_line is too old; marsh needs $SBX_MIN or newer. Upgrade: brew upgrade docker/tap/sbx"
  elif ! version_ge "$sbx_line" "$SBX_TESTED"; then
    warn "sbx $sbx_line works but marsh is tested with $SBX_TESTED or newer."
  fi
else
  sbx_note="Install Docker Sandboxes first:  brew install docker/tap/sbx"
  warn "sbx (Docker Sandboxes) is not on PATH. marsh installs, but will not start without it."
fi

# --- Download and verify ---------------------------------------------------

work=$(mktemp -d "${TMPDIR:-/tmp}/marsh-install.XXXXXX")
trap 'rm -rf "$work"' EXIT INT TERM

if [ -n "$TARBALL" ]; then
  [ -f "$TARBALL" ] || die "no such file: $TARBALL"
  [ -f "$TARBALL.sha256" ] || die "missing checksum file $TARBALL.sha256"
  name=$(basename "$TARBALL")
  cp "$TARBALL" "$work/$name"
  cp "$TARBALL.sha256" "$work/$name.sha256"
else
  if [ -z "$VERSION" ]; then
    latest=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$REPO/releases/latest") ||
      die "cannot reach GitHub to find the latest release"
    VERSION=${latest##*/tag/v}
    case $VERSION in
      [0-9]*.[0-9]*.[0-9]*) ;;
      *) die "no published release found at https://github.com/$REPO/releases" ;;
    esac
  fi
  name="marsh-$VERSION-darwin-arm64.tar.gz"
  base="https://github.com/$REPO/releases/download/v$VERSION"
  say "Downloading marsh $VERSION"
  curl -fSL --progress-bar -o "$work/$name" "$base/$name" || die "download failed: $base/$name"
  curl -fsSL -o "$work/$name.sha256" "$base/$name.sha256" || die "download failed: $base/$name.sha256"
fi

(cd "$work" && shasum -a 256 -c "$name.sha256" >/dev/null 2>&1) ||
  die "checksum mismatch for $name; not installing"
tar -xzf "$work/$name" -C "$work"
src="$work/${name%.tar.gz}"
[ -x "$src/bin/marsh" ] && [ -d "$src/libexec/marsh" ] || die "$name does not look like a marsh release"

# --- Install ---------------------------------------------------------------

sudo=
mkdir -p "$PREFIX" 2>/dev/null || true
if [ ! -w "$PREFIX" ] || { [ -e "$PREFIX/bin" ] && [ ! -w "$PREFIX/bin" ]; }; then
  command -v sudo >/dev/null 2>&1 || die "$PREFIX is not writable"
  if [ "$YES" != 1 ]; then
    if [ -r /dev/tty ]; then
      printf 'Installing into %s needs sudo. Continue? [y/N] ' "$PREFIX" >/dev/tty
      read -r answer </dev/tty || answer=
      case $answer in y|Y|yes|YES) ;; *) die "cancelled" ;; esac
    else
      die "$PREFIX is not writable; rerun with --yes to use sudo, or choose --prefix"
    fi
  fi
  sudo=sudo
fi

if pgrep -x marshd >/dev/null 2>&1; then
  warn "a marsh daemon is running. It keeps the old binaries until it stops; run 'marsh stop' when idle."
fi

$sudo mkdir -p "$PREFIX/bin" "$PREFIX/libexec" "$PREFIX/share/man/man1" "$PREFIX/share/licenses"
# Replace libexec/marsh as a whole so files from an older release do not linger.
$sudo rm -rf "$PREFIX/libexec/marsh.new"
$sudo cp -R "$src/libexec/marsh" "$PREFIX/libexec/marsh.new"
$sudo rm -rf "$PREFIX/libexec/marsh.old"
if [ -d "$PREFIX/libexec/marsh" ]; then $sudo mv "$PREFIX/libexec/marsh" "$PREFIX/libexec/marsh.old"; fi
$sudo mv "$PREFIX/libexec/marsh.new" "$PREFIX/libexec/marsh"
$sudo rm -rf "$PREFIX/libexec/marsh.old"
for binary in marshd marsh-mcp marsh; do
  $sudo cp "$src/bin/$binary" "$PREFIX/bin/$binary.new"
  $sudo chmod 755 "$PREFIX/bin/$binary.new"
  $sudo mv -f "$PREFIX/bin/$binary.new" "$PREFIX/bin/$binary"
done
$sudo ln -sfn marsh "$PREFIX/bin/msh"
$sudo cp "$src"/share/man/man1/*.1 "$PREFIX/share/man/man1/"
$sudo rm -rf "$PREFIX/share/licenses/marsh"
$sudo cp -R "$src/share/licenses/marsh" "$PREFIX/share/licenses/marsh"

installed=$("$PREFIX/bin/marsh" --version 2>/dev/null) || die "installed $PREFIX/bin/marsh does not run"

# --- Next steps ------------------------------------------------------------

say ""
say "Installed $installed in $PREFIX/bin (marsh, msh, marshd, marsh-mcp)."
case :$PATH: in
  *:"$PREFIX/bin":*) ;;
  *) say ""
     say "$PREFIX/bin is not on your PATH. Add it, for example:"
     say "  echo 'export PATH=\"$PREFIX/bin:\$PATH\"' >> ~/.zshrc && exec zsh" ;;
esac
say ""
say "Next:"
[ -n "$sbx_note" ] && say "  $sbx_note"
say "  sbx login                      # sign in to Docker Sandboxes, once"
say "  cd ~/your/project && marsh     # open the shell"
say "  man marsh                      # the manual; docs at https://runmar.sh/docs/"
