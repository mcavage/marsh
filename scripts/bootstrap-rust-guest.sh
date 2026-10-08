#!/bin/sh
# Prepare the selected marsh home for Linux development, then run a command.
set -eu

if [ "${1:-}" != -- ] || [ "$#" -lt 2 ]; then
    echo 'usage: bootstrap-rust-guest.sh -- command [args...]' >&2
    exit 2
fi
shift

if [ "$(uname -s)" != Linux ]; then
    echo 'Rust bootstrap requires Linux' >&2
    exit 2
fi
case "${HOME:-}" in
    /*) ;;
    *) echo 'Rust bootstrap requires an absolute selected HOME' >&2; exit 2 ;;
esac

# A contributor VM can use an already installed toolchain without a daemon
# grant. Only installation into a selected development home needs attachment.
if [ -d "$HOME/.cargo/bin" ]; then
    PATH=$HOME/.cargo/bin:$PATH
    export PATH
fi

# The development shell image carries the toolchain. Use it directly instead
# of installing a second copy into the selected home.
if command -v rustc >/dev/null 2>&1 &&
    rustc --version 2>/dev/null | grep -q '^rustc 1\.95\.0 ' &&
    command -v cargo >/dev/null 2>&1 &&
    cargo fmt --version >/dev/null 2>&1 &&
    cargo clippy --version >/dev/null 2>&1; then
    CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-/tmp/marsh-linux-target-$(id -u)}
    CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-2}
    export CARGO_TARGET_DIR CARGO_BUILD_JOBS
    exec "$@"
fi

if [ -z "${MARSH_DAEMON_SOCKET:-}" ] || [ -z "${MARSH_DAEMON_TOKEN:-}" ]; then
    echo 'Install Rust 1.95.0 with rustfmt and Clippy, or run bootstrap in an attached marsh development shell' >&2
    exit 2
fi

RUSTUP_HOME=$HOME/.rustup
CARGO_HOME=$HOME/.cargo
RUSTUP_TOOLCHAIN=1.95.0
export RUSTUP_HOME CARGO_HOME RUSTUP_TOOLCHAIN
PATH=$CARGO_HOME/bin:$PATH
export PATH
CARGO_TARGET_DIR=/tmp/marsh-linux-target-$(id -u)
export CARGO_TARGET_DIR
CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-2}
export CARGO_BUILD_JOBS

ready() {
    [ -x "$CARGO_HOME/bin/rustup" ] && [ -x "$CARGO_HOME/bin/cargo" ] &&
        "$CARGO_HOME/bin/rustup" run 1.95.0 rustc --version 2>/dev/null |
        grep -q '^rustc 1\.95\.0 ' &&
        "$CARGO_HOME/bin/rustup" component list --installed --toolchain 1.95.0 |
        grep -q '^rustfmt-' &&
        "$CARGO_HOME/bin/rustup" component list --installed --toolchain 1.95.0 |
        grep -q '^clippy-'
}

mkdir -p "$CARGO_HOME" "$RUSTUP_HOME" "$CARGO_TARGET_DIR"
# This lock lives on the VM's disk, not the host-backed selected home.
if ! command -v flock >/dev/null 2>&1; then
    echo 'Rust bootstrap requires flock in the development VM' >&2
    exit 1
fi
exec 9>"/tmp/marsh-rust-bootstrap-$(id -u).lock"
flock -x 9

if ! ready; then
    if [ ! -x "$CARGO_HOME/bin/rustup" ]; then
        if ! command -v curl >/dev/null 2>&1; then
            echo 'Rust bootstrap requires curl in the development VM' >&2
            exit 1
        fi
        bootstrap_dir=$(mktemp -d /tmp/marsh-rustup.XXXXXXXX)
        trap 'rm -rf "$bootstrap_dir"' EXIT HUP INT TERM
        echo 'Installing Rust 1.95.0 in the selected development home...' >&2
        curl --proto '=https' --tlsv1.2 --fail --silent --show-error \
            https://sh.rustup.rs -o "$bootstrap_dir/rustup-init.sh"
        sh "$bootstrap_dir/rustup-init.sh" -y --no-modify-path \
            --profile minimal --default-toolchain 1.95.0
        rm -rf "$bootstrap_dir"
        trap - EXIT HUP INT TERM
    fi
    "$CARGO_HOME/bin/rustup" toolchain install 1.95.0 --profile minimal
    "$CARGO_HOME/bin/rustup" component add --toolchain 1.95.0 rustfmt clippy
    if ! ready; then
        echo 'Rust bootstrap did not produce Rust 1.95.0 with rustfmt and clippy' >&2
        exit 1
    fi
fi

# Brush loads .bashrc for its ordinary interactive shell. Keep a separate,
# replaceable environment file and leave existing user configuration intact.
env_file=$HOME/.marsh-self-dev-env
env_tmp=$(mktemp "$HOME/.marsh-self-dev-env.XXXXXXXX")
cat >"$env_tmp" <<'EOF'
# marsh self-development environment (managed by bootstrap-rust-guest.sh)
export RUSTUP_HOME="$HOME/.rustup"
export CARGO_HOME="$HOME/.cargo"
export RUSTUP_TOOLCHAIN=1.95.0
case ":$PATH:" in *":$CARGO_HOME/bin:"*) ;; *) PATH="$CARGO_HOME/bin:$PATH" ;; esac
export PATH
export CARGO_TARGET_DIR="/tmp/marsh-linux-target-$(id -u)"
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-2}"
EOF
mv -f "$env_tmp" "$env_file"
for profile in "$HOME/.bashrc" "$HOME/.profile"; do
    if [ ! -e "$profile" ]; then
        : >"$profile"
    fi
    if ! grep -Fq '# marsh self-development environment' "$profile"; then
        printf '\n%s\n%s\n' '# marsh self-development environment' \
            '[ -f "$HOME/.marsh-self-dev-env" ] && . "$HOME/.marsh-self-dev-env"' >>"$profile"
    fi
done

flock -u 9
exec "$@"
