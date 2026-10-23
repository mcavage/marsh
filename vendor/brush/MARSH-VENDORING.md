# marsh Brush pin

This source tree is pinned from `reubeno/brush` commit
`cc73400c5a3bd13c48bdfc9d89e1b9dfe694fbf6`. It is checked in so a normal
`make build` does not depend on an unpublished fork.

The marsh source deltas are the seven area patches in
`../../docs/upstream/brush/`; its README lists them and records what the move
from the previous pin (`1389a8e8af6bec56fdfb9ab60a8c898bc14aa961`, 101 per-fix
patches) kept, took from upstream, and did not port. Product routing and
composition (registered-command process shims, `split`/`join`, `fanout`/`collect`,
`cloud`) stay in these patches; everything else is a Bash-compatibility change.

This directory contains only the runtime crate source, manifests, licenses,
and build metadata marsh needs: `brush-parser`, `brush-core`, `brush-clap-adapter`,
`brush-builtins`, `brush-coreutils-builtins`, `brush-experimental-builtins`,
`brush-interactive` and `brush-shell`. Upstream integration tests, snapshots,
examples, benches, and development tooling are intentionally omitted, and so are
the crates' dev-dependencies. `brush-shell` has no `brush` binary here, only the
library entry marsh calls (`entry::run`, `entry::run_from`).

Each local delta is covered by `crates/marsh/tests/smoke.rs`,
`crates/marsh/tests/bash_audit.rs` and `crates/marsh/tests/split_join.rs`, or by
the Bash differential suites (`tests/acceptance/bash_suites.py`, gated on
`tests/acceptance/bash_divergences.json`).

## Reproducing or refreshing the pin

Check out the exact upstream revision, apply the patches with `git apply`, and
copy each crate's `Cargo.toml`, `LICENSE` (`brush-clap-adapter` has none; use the
root one), `build.rs` and `about.*` (if present) and `src/`, plus the root
`Cargo.toml`, `clippy.toml`, `rustfmt.toml`, `LICENSE`, `README.md`, `.gitignore`
and `.cargo/config.toml`. The patches already carry the stripped manifests and the
workspace member list. `cargo fmt --all` formats this tree too, so it must stay
rustfmt-clean under `rustfmt.toml` here.

To change shell behavior, edit the tree under `vendor/brush/`, then regenerate the
affected patch as the difference between the pinned revision and the edited files
for that patch's area. Verify by applying all seven to a clean checkout and
comparing with this tree.

Do not carry product scheduling, daemon, or sandbox code into Brush.
