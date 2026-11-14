# The dev install

Status: implemented.

`make dev` builds everything from the checkout and installs a development copy
outside it; run that copy from any project:

```sh
make dev                              # Mac: build everything, install ~/.marsh-dev
~/.marsh-dev/bin/marsh                # run the dev install
```

`make dev` incrementally builds the host binaries, Linux arm64 guest, shell
template, and packaged source Kits in parallel. The Kit staging
script (`scripts/stage-kits.py`) combines each Kit's Git files with the
canonical `image-base/` support and retained `dhi-notices/` files used by
the publisher, so stock SBX can build the staged sources. Installation writes
`bin/` and `libexec/marsh/` under `DEV_PREFIX` (default `~/.marsh-dev`).

The install then runs `marshd --prebuild-kit-images DEV_PREFIX/libexec/marsh`.
The daemon's Buildx path builds each packaged Kit's job image into the
owner-only cache at `~/Library/Caches/marsh/kit-images`
(`MARSH_KIT_IMAGE_CACHE` overrides), which is never guest-mounted. Builds are
keyed by source fingerprint and skipped when unchanged; `DEV_KIT_IMAGES=0`
opts out. Any daemon for the account can load a cached archive into a cold Kit
VM, checking it against stock SBX's outer image digest and the loaded image ID.
A mismatch evicts the entry. Cache writes use atomic rename, and retention
keeps the 24 most recently used entries, with none unused for 14 days.

Published OCI-digest Kit images share the cache, keyed by digest. After a cold
VM pulls one, the daemon saves it in the background with `docker image save`,
under a `marsh-<digest>` tag in the same repository so it loads with a name.
The next cold VM loads the archive instead of pulling, accepting it only when
the pinned `repository@sha256:` reference resolves to that digest in the VM's
content-addressed image store. A mismatch evicts the entry and pulls the image.

Stock SBX's own Kit builder cache is not warmed because its only public entry
is a real `sbx create`. The install must be outside the checkout: the daemon
refuses guest mounts that overlap its artifacts, and `make dev` rejects a
`DEV_PREFIX` inside the checkout.

`libexec/marsh/dev-enabled` (an empty marker `make dev` writes) marks a dev build:
`marsh --version` says `(dev build)`, its daemon follows no release channel, and
`marshd --check-config` accepts source Kits. `make install` ships no such file.

`marsh --dev`, which opened a dev shell whose `sbx` was a confined host broker, was
removed; `marsh --dev` now exits with a message. A daemon that finds what it left in
its ownership map (development grants' child VMs and the dev shell VM) marks them
retired and removes them by recorded UUID when it starts, even if the dev shell VM is
still running: whatever runs in it (inner daemons, builds) ends. It never deletes the
host scratch under `~/Library/Caches/marsh/dev`; remove that by hand. The shell image
no longer has `pi-dev`, in any shell; its settings in the guest home's `.pi-dev` are
left in place.
