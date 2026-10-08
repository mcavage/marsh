# Develop marsh from marsh (lean dev broker)

Status: built (E2E 1 and E2E 2 below pass).

Goal: in an marsh session at the checkout's Mac path, edit marsh, build a Linux candidate,
and run it, with every stock VM the candidate needs going through a host-daemon broker.

```sh
make dev                                       # Mac: build everything, install ~/.marsh-dev
~/.marsh-dev/bin/marsh --dev                   # Mac, in the checkout
DEV_KIT=<fixture-ref> make dev-inner           # in the dev shell
make dev-inner-run CMD="fixture identity"      # inner candidate via the broker
```

To work on marsh with Pi, after `make dev`, from the Mac checkout:

```sh
~/.marsh-dev/bin/marsh --dev -c pi-dev
```

`make dev` builds, in parallel and incrementally, the host binaries, the linux/arm64 guest,
the shell template (one image for every shell VM, dev tooling included), and the packaged source Kits
(`scripts/stage-kits.py`: each Kit's Git files plus the canonical `image-repair/`,
`dhi-notices/` and `collect-notices.mjs` inputs that `publish-kits.py` adds, so stock SBX
builds them as is). It then installs to `DEV_PREFIX` (default `~/.marsh-dev`: `bin/` and
`libexec/marsh/`) and runs `marshd --prebuild-kit-images DEV_PREFIX/libexec/marsh`: the
daemon's own Buildx path builds each packaged Kit's job image into the per-user Kit image
cache (`~/Library/Caches/marsh/kit-images`, `MARSH_KIT_IMAGE_CACHE` overrides; owner-only,
never guest-mounted), keyed by source fingerprint and skipped when unchanged
(`DEV_KIT_IMAGES=0` opts out). Every daemon of the account then loads the archive into a cold
Kit VM instead of building, still checking it against stock SBX's outer image digest and the
loaded image ID (a mismatch evicts the entry). Entries are written by atomic rename; the 24
most recently used are kept, none unused for 14 days. Published (OCI digest) Kit images share
the cache keyed by their digest: after a cold VM pulls one, the daemon saves it in the
background (`docker image save` under a `marsh-<digest>` tag in the same repository, so it
loads named), and the next cold VM loads the archive instead of pulling, accepting it only
when the pinned `repository@sha256:` reference then resolves to that digest in the VM's
content-addressed image store (otherwise the entry is evicted and the image pulled). Stock SBX's own Kit builder cache is
not warmed: its only public entry is a real `sbx create`. The install must be outside the checkout (the daemon refuses guest
mounts that overlap its own artifacts); `make dev` refuses a `DEV_PREFIX` inside it.

`libexec/marsh/dev-enabled` (an empty marker `make dev` writes) is what turns `--dev` on: a
daemon whose install carries it admits dev grants. `make install` ships no such file, so a
production daemon still needs `MARSH_ENABLE_DEV_SCOPES=1`. The gate is install-time rather
than per-launch: the dev install is a separate prefix the operator built for development,
and the grant is still opt-in per session (`--dev`). `--dev` only enables the broker: there is
no separate dev image.

## Dev shell

`marsh --dev` attaches to one warm dev shell VM per daemon (`vm_name(Shell, "dev")`, kept
apart from the ordinary shell VM because it holds the broker grant) made from the one shell
template (`libexec/marsh/shell-image`, built from `packaging/shell`): the repaired DHI shell,
which already has gcc/make/git/python3/node/npm, plus Rust 1.95 with the aarch64 gnu and musl
targets, plus `pi-dev` (see [Pi in the dev shell](#pi-in-the-dev-shell)). Ordinary shells have
the same tools. Its VM-local cargo target survives sessions. Kit jobs (ACP
agents included) still get no daemon socket or relay. Grants exist only on a dev install's
daemon or one started with `MARSH_ENABLE_DEV_SCOPES=1`; `--dev` is not tied to a particular
home. The trust domain
is the shell VM.

## Reaching the host: the existing relay + an `sbx` shim

- `PublicRequest::DevSbx { argv, pty, cwd }`, `RequestAuthority::RelayDevelopment`. The
  session is the relay token's owner, never a payload field. After admission the
  connection streams stdin (credited), stdout, stderr, and exit frames
  (`dev_broker/stream.rs`, salvaged from the old attach framing). `cwd` lets the daemon's
  `sbx mount VM .` (mount from inside the admitted source) resolve the same way on the host.
- At attach the host copies the **outer** build's guest `marsh` to `S/tmp/bin/sbx` and writes
  `S/tmp/bin/sbx-relay.json` (relay socket and token paths). Invoked as `sbx`, the binary is
  the shim (`crates/marsh/src/sbx_shim.rs`). The host owns the protocol, so a candidate
  cannot break its own broker hop. The sidecar matters because daemons run `sbx` with a
  cleared environment.
- The shell gets `PATH=S/tmp/bin:…`, `MARSH_DEV_SCRATCH`, `MARSH_DEV_DEPTH`,
  `MARSH_VM_PREFIX`, `MARSH_DEV_SHELL_TEMPLATE` (the host shell template).
- The inner candidate runs with `MARSH_SBX=S/tmp/bin/sbx` through the unmodified `StockSbx`
  + `SystemCommandRunner` path. `MARSH_VM_PREFIX` makes it name VMs `${prefix}{s,k}-<8>`.

## The grant

Created at attach (`DevBroker::create_grant`), bound to the relay session, persisted in the
ownership map file under `grants`: `{session, prefix, roots, scratch, max_vms=8, revoked,
cleanup_uncertain, names: name -> uuid | null(intent)}`.

- **Names.** Prefix `${MARSH_VM_PREFIX:-marsh-}x<5 base36>-`. `create --name N` and
  `run --name N` need `^prefix(x[0-9a-z]{5}-){0,2}[skr]-[0-9a-z]{8}$`; N is persisted as an
  intent before the call and adopted by name on the next inventory (also in `store_view`), or
  dropped. A `run` without `--name` is given `${prefix}r-<8>` by the broker, so stock never
  picks its `<agent>-<workdir>` default or reattaches to a host VM.
- **Existing VMs.** An op needs N in this grant's `names` with its recorded UUID in the
  cached inventory (one refresh on mismatch). Host names, other grants' names, and foreign
  VMs are refused before any stock call.
- **Roots.** The pinned project and `S = ~/Library/Caches/marsh/dev/<sha256(project)[..16]>/`
  (`MARSH_DEV_CACHE_ROOT` overrides). Leaves `home control tmp artifacts cache` are mounted
  at the same paths. One live grant per project. Sources must be normalized descendants of a
  root, resolve to themselves (no symlink), with identity checked before and after the stock
  call; a mismatch unmounts and revokes. The reopen inside `sbx mount` stays model A4's gap.

| verb | allowed (own VM unless noted) | refused |
|---|---|---|
| `version`, `--help`, `create --help` | pass-through | |
| `ls [--json]` | rows filtered to this grant | other rows |
| `create` | name rule; `--quiet --pull --skills off\|readonly --cpus -m --memory --deny-network`, `-e K=V`; `--template` = the host shell template (tag or tag@digest; a local template reaches stock as its tag, and the created VM's image digest must match or the VM is removed and the call fails — after a `run` session, the grant is revoked); agent `shell`, `oci://…@sha256:…`, or a Kit dir under roots; workspaces under roots | `--env-file`, bare `-e K`, `--kit`, `-p`, `--clone`, `--cloud`, `--skills readwrite`, built-in agents (use `run`), other options, over `max_vms` |
| `exec` | `-i -t -u -w --detach-keys`, `-e K=V` | `--env-file`, bare `-e K`, `--privileged`, other options |
| `mount`, `umount` | `HOST[:CTR[:ro\|rw]]`, HOST (relative to `cwd`) under roots | anything else |
| `cp` | host → own VM, SRC under roots | VM → host, `-L` |
| `run` | new child (name rule, counts toward `max_vms`) or `--name` of an own VM (reattach); `-d --pull --skills off\|readonly --cpus -m --memory --deny-network`, `-e K=V`, `-t/--template` = host template; agent = a stock built-in (`claude codex shell …`), `oci://…@sha256:…`, or a Kit dir under roots; workspaces (`:ro`/`:rw`) under roots; no workspace = the caller's cwd, which must be a host path under roots (normally the natural project path); everything after `--` passes to the agent | `--cloud`, `--clone`, `--kit*`, `--env-file`, bare `-e K`, `--profile`, `-p`, `--static-mcp`, `--skills readwrite`, cloud-only flags, bare non-agent names (host sandbox names, registry refs, git URLs), other options |
| `stop`, `rm [--force]`, `inspect [--json]` | own VM | `template mcp policy secret ports …` |

**Terminals.** The shim sends the caller's terminal size when its stdin and stdout are
terminals. The host gives the stock process a PTY for `exec -t` and an attached `run` (not
`-d`), and says so in the start frame; the shim then puts its terminal in raw mode (restored on
exit) and forwards size changes. Keys such as Ctrl-C reach the agent through the PTY. A built-in
agent runs with stock's own proxy-managed credentials, as in any `sbx run`; none enter the
grant.

**Revocation** (`revoke_session` at the end of every `--dev` shell, including relay loss;
`revoke_all` at `marsh stop`/`marsh reset` and at daemon start): persist `revoked` → fence and wait
for in-flight calls (killed and reaped by the stream supervisor) → fresh inventory (adopts
sent intents) → `rm --force` names whose recorded UUID is present → verify absence → delete
`S/{home,control,tmp}` → drop the record. Failure sets `cleanup_uncertain` and is retried at
the next start. Nothing is replayed. `S/artifacts` and `S/cache` persist.

**Nesting.** The inner daemon runs the same broker for its own `--dev` child (prefix
`${prefix}x<5>-`, scratch under `S/tmp/dev`), and its shim reaches stock through the outer
shim. The host checks only the outer grant: the name pattern caps depth, roots admit
descendants, `max_vms` counts the subtree. Daemons refuse `--dev` at `MARSH_DEV_DEPTH ≥ 3`.
Streams nest one hop per level; mind the relay's 16-connection cap.

## Inner build/run

- `make dev-inner` (Linux, inside `--dev`): release gnu build of `marsh marsh-local
  marsh-relay marshd` and static musl `marsh-worker marsh-byte-exec` with a VM-local
  `CARGO_TARGET_DIR`, installed to `S/artifacts/{bin,libexec/marsh}` with `commands.json`
  (plus `fixture` = `DEV_KIT` when set), `agents.json`, `kits/`, and `shell-image` = the host
  template. The Mac `target/` is never written.
- `make dev-inner-run CMD=…` runs the inner client with `MARSH_HOME=S/home`,
  `MARSH_CONTROL_HOME=S/control`, `TMPDIR=S/tmp`, `MARSH_SBX=S/tmp/bin/sbx`, relay variables
  unset. `DEV_INNER_FLAGS=--dev` opens a depth-2 dev child. `make dev-inner-stop` stops it.

## E2E 1

`python3 tests/acceptance/self_dev.py --sbx $(command -v sbx) --kit <fixture-ref>
[--prefix ~/.marsh-dev] [--depth2|--depth3] [--relay-kill|--only-relay-kill]`: runs the `make dev` install with a dedicated
home/control/cache (no dev variables), records an `sbx ls` baseline, runs `marsh --dev -c
'make dev-inner && make dev-inner-run CMD="fixture identity"'` plus refusal probes (exec into
the dev shell VM and a foreign VM, mount `~/.ssh`, symlink escape under `S/tmp`,
`create --env-file`, a non-prefixed name, `template`, `cp` VM → host, `run --cloud`, `run` with
`~/.ssh`, `run --name` of a host VM) and `sbx run -d shell` without `--name`. It asserts the
`r-` child under the prefix and that `sbx exec` in it sees the project at its natural path, the
identity output, receipts in `S/control`, prefixed child VMs, then after exit: no prefixed
VM, no grant record, no `S/{home,control,tmp}`, artifacts kept, baseline VMs unchanged, and
no new VM after the outer `marsh stop`.

`--depth3` adds, inside the depth-2 shell, a depth-3 `--dev` child opened by the depth-2 inner
daemon: `MARSH_DEV_DEPTH=3`, `sbx ls`, a child VM created/exec'd/removed three broker hops from
stock, a foreign exec refused, a third `make dev-inner`, and its inner daemon
(`MARSH_DEV_DEPTH=3`) refusing a depth-4 `--dev`. `--relay-kill` opens a second
session that kills its relay (`pkill -x marsh-relay`) during a brokered `sbx exec`: the call fails
visibly (125, "dev broker stream lost"), later `sbx` calls fail, session end revokes the grant and
removes its child VM, no host `sbx exec` is left running, and the shell VM is cleanup-uncertain
until `marsh reset`, after which a new `--dev` session works.

## Pi in the dev shell

The shell image installs the Pi coding agent 1.0.0 (`@earendil-works/pi-coding-agent`,
`packaging/shell/pi`, npm-locked; regenerate the lock with
`npm install --package-lock-only --min-release-age=0` if a local npm enforces a minimum
release age, and keep `resolved` URLs on registry.npmjs.org) and a `pi-dev` launcher.
Pi runs in the dev shell VM, so its bash tool runs there too, with the session's `PATH` (the
`sbx` shim, registered-command shims, Rust) and environment: `make dev-inner`,
`make dev-inner-run` and the broker behave as they do for the operator. Pi's tool shell is the
VM's `/bin/bash` (Pi's own choice), not Brush. In the session `pi` is the registered `pi` Kit
command (a job container that cannot reach the broker); use `pi-dev`.

- **Credentials.** Stock SBX gives every shell VM proxy-managed sentinels for the stored global
  service secrets (`sbx secret set -g anthropic`; `SBX_CRED_ANTHROPIC_MODE=apikey`) and injects the
  real key at its proxy. No credential enters the image, the scratch, or marsh.
- **State.** `PI_CODING_AGENT_DIR=S/cache/pi-dev/agent` (survives `--dev` sessions; outside `--dev`,
  `$HOME/.pi-dev/agent`). The first run seeds `settings.json` (Anthropic `claude-opus-5-5`,
  install telemetry off); after that it is the operator's file.
- **Not imported.** No host agent profile, extensions, themes or skills. A personal agent
  built on Pi belongs in its own Kit ([plan](../plan/custom-agent-kit.md)).

## E2E 2

`python3 tests/acceptance/pi_dev.py --sbx $(command -v sbx) --kit <fixture-ref>
[--prefix ~/.marsh-dev]` (one billed model turn): uses the dev install like E2E 1,
runs `pi-dev -p --mode json` in `marsh --dev` asking Pi to run one command (echo a nonce with
`$SANDBOX_NAME` and the grant prefix, `make dev-inner`, `make dev-inner-run CMD="fixture
identity"`) and report the identity line. It asserts the bash tool result carries the nonce with
the dev shell VM's recorded name and the identity output, the model reported it, the inner build
landed in `S/artifacts` during the run, receipts are in `S/control`, child VMs carry the grant
prefix, Pi's session is in `S/cache/pi-dev`, and then the E2E 1 cleanup checks.
