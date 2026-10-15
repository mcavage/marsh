# Proposal: `marsh init`

Status: proposed onboarding direction, **not an implemented command**. The public
Pix/private dotfile cutover does not depend on it.

An interactive host-side command should ask what the user wants in their selected
marsh home, show a plan, and scaffold ordinary files rather than a pack/profile
registry. Candidate choices are a minimal home, an optional separately registered
agent Kit, starter shell/editor config, and private skill/MCP examples.

Default behavior should be filesystem-only: no VM/daemon start, image pull,
credential lookup, browser login, model request, service launch, Git staging or
commit. Any optional network/registration step needs its own explicit consent.
Credentials stay in SBX/secret managers or ignored integration-owned stores.

Preserve existing settings, authentication, sessions and user work. Never follow
an unexpected destination symlink or overwrite an existing file implicitly.
Offer conflicts as skips or reviewable explicit changes with backups. Write a
default-deny `.gitignore` that tracks selected source/config, not the physical
whole home; exclude auth, integrations, sessions, caches, builds and backups.

Keep the packaged agent commands unchanged when adding a personal command such
as `pix`. Scope optional MCP examples to that command, not every agent. Explain
that shared-home discovery separation is not filesystem confidentiality.

Before implementation, agree the exact questions/defaults, noninteractive
behavior, destination/ownership rules and partial-failure/retry contract. Tests
must cover fresh homes, repeated runs, conflicts, symlinks, restrictive modes,
data preservation, and absence of implicit service/authentication side effects.
