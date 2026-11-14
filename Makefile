.DEFAULT_GOAL := help

CARGO ?= cargo
DOCKER ?= docker
INSTALL ?= install
PYTHON ?= python3
PREFIX ?= $(HOME)/.local
MCP_MARSH ?= $(PREFIX)/bin/marsh
MCP_BIN ?= $(PREFIX)/bin/marsh-mcp
override SBX_EXECUTABLE := $(shell candidate="$${MARSH_SBX:-$$(command -v sbx 2>/dev/null)}"; test -n "$$candidate" && realpath "$$candidate" 2>/dev/null)
TARGET_DIR ?= $(if $(CARGO_TARGET_DIR),$(CARGO_TARGET_DIR),target)
HOST_FEATURES ?=
GUEST_ARTIFACTS ?= $(TARGET_DIR)/libexec/marsh
KIT_COMMANDS ?= packaging/commands.json
KIT_RELEASE_COMMANDS ?= $(TARGET_DIR)/kit-release/commands.json
KIT_FIXTURE_COMMANDS ?= $(TARGET_DIR)/kit-release/fixture-commands.json
KIT_BUILD_INPUTS ?=
KIT_PREPARED_INPUTS ?= $(TARGET_DIR)/kit-release/build-inputs.json
KIT_FIXTURE_INPUTS ?= $(TARGET_DIR)/kit-release/fixture-inputs.json
SHELL_IMAGE_REPOSITORY ?=
SHELL_IMAGE_INSECURE_REGISTRY ?= 0
RUST_IMAGE ?= rust:1.99-bookworm@sha256:48b7f435b8fc6378ce43b44f15e3a47fd8919b2825f835c39bd4bd3da50af5b8
BUILD_CACHE_ID ?= marsh-$(shell pwd -P | cksum | cut -d' ' -f1)
ACCEPTANCE_EVIDENCE ?=
ACCEPTANCE_HOST_FEATURES ?=
# The acceptance fixture Kit: a local `make kit-publish-fixture` output, else
# the ref fetched by `make fixture-ref` (the newest release, nightly or stable,
# that publishes one).
FIXTURE_REF_FILE ?= $(TARGET_DIR)/fixture-ref
ACCEPTANCE_KIT ?= $(shell $(PYTHON) -c 'import json,pathlib; p=pathlib.Path("$(KIT_FIXTURE_COMMANDS)"); r=pathlib.Path("$(FIXTURE_REF_FILE)"); print(json.loads(p.read_text())["fixture"] if p.is_file() else (r.read_text().strip() if r.is_file() else ""))')
PERF_KIT ?= $(ACCEPTANCE_KIT)
PERF_SAMPLES ?= 30
PERF_WARMUPS ?= 5
PERF_OUTPUT ?=
SOURCE_REVISION ?= $(shell git rev-parse HEAD)
VERSION ?= $(shell sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
MAN_DIR ?= $(TARGET_DIR)/man
DIST_DIR ?= $(TARGET_DIR)/dist
SITE_DIR ?= $(TARGET_DIR)/site
KIT_REPOSITORY_NAME_PREFIX ?=
KIT_TAG ?=
FORMULA_CHANNEL ?= stable
GITHUB_REPOSITORY ?= mcavage/marsh
# make test (CI runs the same targets with NEXTEST_PROFILE=ci).
NEXTEST_PROFILE ?= default
TEST_BIN = $(abspath $(TARGET_DIR))/debug
PUBLISHER_VENV ?= $(abspath $(TARGET_DIR))/publisher-venv
# The pinned PyYAML has wheels through Python 3.14: prefer the newest such
# interpreter installed, else $(PYTHON).
PUBLISHER_PYTHON ?= $(firstword $(foreach version,3.14 3.13 3.12 3.11 3.10,$(shell command -v python$(version) 2>/dev/null)) $(PYTHON))
# Each isolated Python test gets a process; set to 1 to diagnose serially.
HARNESS_JOBS ?= 3
MODEL_FLAGS ?= --quick
MODEL_CACHE ?= $(abspath $(TARGET_DIR))/tlc-cache
TLA2TOOLS_RELEASE := tools-tla2tools-2026-10-06-94d0c50
TLA2TOOLS_JAR ?= $(abspath $(TARGET_DIR))/$(TLA2TOOLS_RELEASE).jar
TLA2TOOLS_SHA256 := 7beec0f04818732a62fa193731711a99aa4f11279499b2360a7d156c519ea78d
# The callers of the built binaries, #[ignore]d without them: the MCP load
# callers (MARSH_LOAD_TEST_BIN, a built marsh beside marsh-mcp and marshd) and
# the real-CLI ACP callers (MARSH_ACP_CALLER_BIN). Every #[ignore]d test is one.
BINARY_CALLERS := (package(marsh-mcp) & binary(mcp_load)) | (package(marsh-daemon) & test(/^mcp_load_tests::/)) \
	| (package(marsh-acp) & binary(fixture_caller))
# Their packages: building only these test binaries, not the workspace's.
BINARY_CALLER_PACKAGES := -p marsh-mcp -p marsh-daemon -p marsh-acp
# The binary callers' host publications write under $HOME: give them their own.
CALLER_HOME = $(abspath $(TARGET_DIR))/caller-home
# Everything the Rust tests start inherits MARSH_TEST_RUN (unless a test clears
# its environment). After the tests, an orphaned process still running from
# $(TEST_BIN) or carrying this run's marker has leaked: it is printed and
# killed, and the run fails.
TEST_RUN_TOKEN := $(shell od -An -N8 -tx1 /dev/urandom | tr -d ' \n')
TEST_RUN_START := $(shell date +%s)
# A credential-shaped canary (not a credential) exported to the tests and the
# harnesses. Test shells start from an allowlisted environment; their checks
# fail if a credential-shaped name, this one included, reaches them.
TEST_CANARY = MARSH_TEST_CANARY_API_KEY=canary-not-a-credential
LEAK_CHECK = $(PYTHON) tests/leaked_processes.py --bin "$(TEST_BIN)" --token "$(TEST_RUN_TOKEN)" --since $(TEST_RUN_START)
# Export serving is a Mac host boundary, not a Linux contributor capability.
MCP_TEST_FILES := test_stdio_protocol.py test_guest_export_boundary.py test_publication_cli.py \
	$(if $(filter Darwin,$(shell uname -s)),test_export_stdio_protocol.py)

.PHONY: help build build-host build-linux prepare-shell-image prepare-svc-engine-image test test-rust test-harness test-model lint test-unit test-callers test-bash test-entrypoints test-tools mcp kits kit-publish kit-publish-fixture install install-preflight uninstall acceptance acceptance-smoke perf man site site-check dist dist-local fixture-ref formula

help:
	@printf '%s\n' \
		'Tests (CONTRIBUTING.md):' \
		'make test              The VM-free gate CI runs: test-rust, test-harness, test-model' \
		'make test-rust         lint + test-unit + test-callers:' \
		'  make lint            fmt, clippy' \
		'  make test-unit       every workspace test (cargo-nextest), doctests (TEST_SCOPE=host: macOS host side)' \
		'  make test-callers    MCP load and ACP callers against the built binaries' \
		'make test-harness      test-bash + test-entrypoints + test-tools:' \
		'  make test-bash       Bash differential suites (Linux)' \
		'  make test-entrypoints  onboarding journeys and MCP protocol callers' \
		'  make test-tools      publisher and tooling self-tests (no Rust build)' \
		'make test-model        Quick TLA+ sweep (MODEL_FLAGS= for every config)' \
		'make check [DEV_KIT=ref]  Mac: fast real smoke of the installed dev product (warm scope in target/check)' \
		'make check-reset       Mac: stop the make check scope and delete target/check (run before removing a worktree)' \
		'make check-gc          Mac: retire warm check scopes whose checkout was deleted (also run by check/verify/regress)' \
		'make verify [DEV_KIT=ref]  Mac, before pushing: check + the highest-signal sbx suites, concurrently' \
		'make regress [DEV_KIT=ref]  Mac: every sbx suite, then make test with the full model (manual)' \
		'                        ONLY=/SKIP= suites, families or scenario ids; REGRESS_JOBS=1 one at a time' \
		'make services-e2e TIER=smoke|full|perf|soak|break [ONLY=ID,ID] [BREAK=mode] [DRIVER=internal|product]  Mac: services E2E (real sbx, takes the VM lock)' \
		'make services-e2e-gc [DRY_RUN=1]  Mac: remove services E2E leftovers of finished runs (takes the VM lock)' \
		'make acceptance        Build a fresh candidate and run the full local-v3 stock-SBX gate (manual)' \
		'make acceptance-smoke  Build a fresh candidate and run the stock-SBX smoke gate' \
		'make perf              Measure the assembled warm registered-command path' \
		'' \
		'Build and install:' \
		'make dev               Build everything (host, guest, images, Kits) and install the dev product in $$(DEV_PREFIX)' \
		'make build             Build artifacts and import the repaired shell locally (no push)' \
		'make prepare-shell-image  Prepare locally; SHELL_IMAGE_REPOSITORY opts into publication' \
		'make install           Install marsh (SBX remains Homebrew-managed)' \
		'make uninstall         Remove installed marsh files under PREFIX; preserve user state' \
		'make mcp               Run installed host stdio MCP server for this workspace' \
		'make kits              Validate native Kit v3 source with Buildx' \
		'make kit-publish KIT_REPOSITORY_PREFIX=registry/repository  Publish Kits and pin their digests' \
		'make kit-publish-fixture KIT_REPOSITORY_PREFIX=registry/repository  Publish acceptance fixture' \
		'make fixture-ref       Fetch the published acceptance fixture ref into $$(FIXTURE_REF_FILE)' \
		'make man               Render man pages into $$(MAN_DIR)' \
		'make site              Build the runmar.sh site into $$(SITE_DIR) and check its links' \
		'make site-check        make site, plus every relative link in tracked Markdown' \
		'make dist              Package a release tarball (needs pinned KIT_COMMANDS and shell-image)'

build: build-host build-linux

# The build records what cargo reports it compiled (every unit's features) and
# the hash of each executable (scripts/provenance.py); `make dist` verifies it.
# HOST_CARGO_BUILD wraps one host cargo build: its first argument is the target dir.
HOST_CARGO_BUILD = $(PYTHON) scripts/provenance.py build --manifest "$(1)/release/provenance.json" --reset -- \
	$(CARGO) build --workspace --release --locked --target-dir "$(1)"

build-host:
	$(call HOST_CARGO_BUILD,$(TARGET_DIR)) $(if $(HOST_FEATURES),--features "$(HOST_FEATURES)")

build-linux:
	@mkdir -p "$(GUEST_ARTIFACTS)"
	$(DOCKER) buildx build --platform linux/arm64 \
		--build-arg "RUST_IMAGE=$(RUST_IMAGE)" \
		--build-arg "BUILD_CACHE_ID=$(BUILD_CACHE_ID)" --build-arg "RELEASE_PACKAGING=1" \
		--target export --output "type=local,dest=$(GUEST_ARTIFACTS)" \
		-f packaging/linux-arm64/Dockerfile .
	$(INSTALL) -m 644 "$(KIT_COMMANDS)" "$(GUEST_ARTIFACTS)/commands.json"
	$(MAKE) prepare-shell-image
	$(MAKE) prepare-svc-engine-image
	$(PYTHON) scripts/stage-kits.py --commands "$(KIT_COMMANDS)" --output "$(GUEST_ARTIFACTS)"

prepare-shell-image:
	$(PYTHON) scripts/prepare-shell-image.py --docker "$(DOCKER)" --sbx "$(SBX_EXECUTABLE)" \
		$(if $(SHELL_IMAGE_REPOSITORY),--repository "$(SHELL_IMAGE_REPOSITORY)",) --output "$(GUEST_ARTIFACTS)/shell-image" \
		$(if $(filter 1,$(SHELL_IMAGE_INSECURE_REGISTRY)),--insecure-registry,)

# The services engine image (packaging/svc-engine): a tar plus a digest pin in
# the guest artifacts, loaded offline into the services VM's boot engine.
prepare-svc-engine-image:
	$(PYTHON) scripts/prepare-svc-engine-image.py --docker "$(DOCKER)" --output "$(GUEST_ARTIFACTS)"

# `make test` is the VM-free gate, and CI runs these same targets
# (.github/workflows/ci.yml), each piece in its own job: test-rust and
# test-harness on Linux, their host side on macOS, the model in its own job.
# Nothing here starts a VM or needs credentials.
test: test-rust test-harness test-model

# TEST_SCOPE=host (CI's macOS lane) runs only the host side of test-unit. The
# guest worker, the shared contract values, the Brush-against-Bash audit (the
# shell runs in the Linux guest) and the probe that marsh-acp's public API
# hides process spawning (a `cargo check`) are platform-neutral: Linux runs
# them.
TEST_SCOPE ?= all
HOST_SCOPE_EXCLUDES := --exclude marsh-worker --exclude marsh-contracts
HOST_SCOPE_FILTER := not binary_id(marsh::bash_audit) and not (binary_id(marsh-acp::subprocess_test) and test(=production_builds_do_not_expose_host_process_spawning))
# TEST_PACKAGES runs only those packages' tests; TEST_EXCLUDE leaves packages
# out of the workspace (CI splits the macOS workspace tests between two jobs).
TEST_PACKAGES ?=
TEST_EXCLUDE ?=
TEST_SELECTION = $(if $(TEST_PACKAGES),$(addprefix -p ,$(TEST_PACKAGES)),--workspace $(addprefix --exclude ,$(TEST_EXCLUDE)) \
	$(if $(filter host,$(TEST_SCOPE)),$(HOST_SCOPE_EXCLUDES)))
NEXTEST_SCOPE = $(TEST_SELECTION) $(if $(filter host,$(TEST_SCOPE)),-E '$(HOST_SCOPE_FILTER)')
DOCTEST_SCOPE = $(TEST_SELECTION)

# test-rust = lint + test-unit + test-callers.
test-rust: lint test-unit test-callers

lint:
	$(CARGO) fmt --all -- --check
	$(PYTHON) scripts/check-provenance-wiring.py
	sh scripts/check-no-vendored-deps.sh
	$(CARGO) clippy --workspace --all-targets --locked --target-dir "$(TARGET_DIR)" -- -D warnings

# Every workspace test, one process per test (cargo-nextest; .config/nextest.toml
# fails a hung test by name). nextest skips doctests, so cargo runs those.
# Every step that runs tests is followed, pass or fail, by $(LEAK_CHECK).
test-unit:
	@case "$(TEST_SCOPE)" in all|host) ;; *) echo 'make: TEST_SCOPE must be all or host' >&2; exit 1;; esac
	@$(CARGO) nextest --version >/dev/null 2>&1 || { echo 'make test: cargo-nextest is required: brew install cargo-nextest (or cargo install --locked cargo-nextest)' >&2; exit 1; }
	@# Build first, without this run's marker: a compiler wrapper's server
	@# (sccache) started by the build would otherwise count as leaked.
	$(CARGO) nextest run --no-run $(NEXTEST_SCOPE) --locked --target-dir "$(TARGET_DIR)"
	$(TEST_CANARY) MARSH_TEST_RUN=$(TEST_RUN_TOKEN) $(CARGO) nextest run $(NEXTEST_SCOPE) --locked --target-dir "$(TARGET_DIR)" \
		--profile "$(NEXTEST_PROFILE)" || { $(LEAK_CHECK); exit 1; }
	$(LEAK_CHECK)
	$(CARGO) test $(DOCTEST_SCOPE) --doc --locked --target-dir "$(TARGET_DIR)"

# The #[ignore]d binary callers and the ACP host caller, which drive the built
# marsh, marsh-mcp and marshd binaries (no VM) with an isolated HOME.
test-callers:
	@$(CARGO) nextest --version >/dev/null 2>&1 || { echo 'make test: cargo-nextest is required: brew install cargo-nextest (or cargo install --locked cargo-nextest)' >&2; exit 1; }
	$(CARGO) build --workspace --bins --locked --target-dir "$(TARGET_DIR)"
	rm -rf "$(CALLER_HOME)" && mkdir -m 700 "$(CALLER_HOME)"
	CARGO_HOME="$${CARGO_HOME:-$$HOME/.cargo}" RUSTUP_HOME="$${RUSTUP_HOME:-$$HOME/.rustup}" HOME="$(CALLER_HOME)" \
		MARSH_LOAD_TEST_BIN="$(TEST_BIN)/marsh" MARSH_ACP_CALLER_BIN="$(TEST_BIN)/marsh" MARSH_TEST_RUN=$(TEST_RUN_TOKEN) $(TEST_CANARY) \
		$(CARGO) nextest run $(BINARY_CALLER_PACKAGES) --locked --target-dir "$(TARGET_DIR)" \
		--profile "$(NEXTEST_PROFILE)" --run-ignored only -E '$(BINARY_CALLERS)' || { $(LEAK_CHECK); exit 1; }
	$(TEST_CANARY) MARSH_TEST_RUN=$(TEST_RUN_TOKEN) $(PYTHON) tests/acceptance/acp-fixture/typed_host_caller.py \
		--marsh "$(TEST_BIN)/marsh" || { $(LEAK_CHECK); exit 1; }
	$(LEAK_CHECK)

# Python and shell harnesses. test-bash: (Linux only) the Bash differential
# suites against GNU Bash 5.3.0 on PATH, gated on
# tests/acceptance/bash_divergences.json. test-entrypoints: shipped-entrypoint
# journeys (onboarding) and MCP protocol callers against the debug binaries.
# test-tools: the Kit publisher and the self-tests of the acceptance, perf,
# release and eval (fakemarsh) tooling; they build nothing.
test-harness: test-bash test-entrypoints test-tools

test-bash:
	@if [ "$$(uname -s)" = Linux ]; then \
		$(CARGO) build -p marsh --bin marsh-local --locked --target-dir "$(TARGET_DIR)" && \
		rm -rf "$(abspath $(TARGET_DIR))/bash-suites" && \
		$(PYTHON) tests/acceptance/bash_suites.py --candidate "$(TEST_BIN)/marsh-local" \
			--oracle "$$(command -v bash)" --evidence "$(abspath $(TARGET_DIR))/bash-suites"; \
	else \
		echo 'make test-harness: skipping the Bash differential suites (tests/acceptance/bash_suites.py): Linux only; macOS cannot create their non-UTF-8 file names'; \
	fi

test-entrypoints:
	$(CARGO) build --workspace --bins --locked --target-dir "$(TARGET_DIR)"
	$(PYTHON) tests/acceptance/onboarding.py --marsh "$(TEST_BIN)/marsh" --shell "$(TEST_BIN)/marsh-local" \
		--evidence "$(abspath $(TARGET_DIR))/onboarding.json"
	MARSH_BINARY="$(TEST_BIN)/marsh" MARSH_MCP_BIN="$(TEST_BIN)/marsh-mcp" \
		$(PYTHON) tests/parallel_unittest.py -j $(HARNESS_JOBS) -W error $(addprefix tests/mcp/,$(MCP_TEST_FILES))

test-tools:
	$(PYTHON) tests/parallel_unittest.py -j $(HARNESS_JOBS) -W error \
		tests/acceptance/test_*.py tests/release/test_*.py \
		evals/benchmark/test_*.py evals/reach/test_*.py tests/test_parallel_unittest.py
	$(PYTHON) -m unittest discover -s tests/perf -p 'test_*.py'
	sh tests/kits/selected-home.sh
	sh tests/kits/cli-adapters.sh
	sh tests/kits/shell-adapter.sh
	sh tests/kits/acp-proxy.sh
	node --test tests/kits/pi-acp-patch.test.mjs
	$(MAKE) --no-print-directory "$(PUBLISHER_VENV)/.ready"
	"$(PUBLISHER_VENV)/bin/python" tests/parallel_unittest.py -j $(HARNESS_JOBS) -W error tests/kits/test_publisher_cli.py \
		tests/release/test_release_workflow.py

# The publisher's one dependency, hash-pinned.
$(PUBLISHER_VENV)/.ready: scripts/requirements-publish.txt
	rm -rf "$(PUBLISHER_VENV)"
	"$(PUBLISHER_PYTHON)" -m venv "$(PUBLISHER_VENV)"
	"$(PUBLISHER_VENV)/bin/python" -m pip install --quiet --require-hashes --no-deps --only-binary=:all: -r scripts/requirements-publish.txt
	@touch "$@"

# The TLA+ model (docs/model/README.md). `make test` runs the quick sweep with
# verdicts cached by spec hash; MODEL_FLAGS= runs every config (make regress).
test-model: $(TLA2TOOLS_JAR)
	TLA2TOOLS_JAR="$(TLA2TOOLS_JAR)" TLC_CACHE="$(MODEL_CACHE)" docs/model/check.sh $(MODEL_FLAGS)

# The pinned TLA+ tools: the model needs TLC 2.20+ (v1.8.0), which upstream
# publishes only as a rolling prerelease whose asset is replaced in place, so
# the checked build is mirrored unmodified as its own release here. To move to
# a newer build, re-check the model with it, mirror it the same way, and update
# TLA2TOOLS_RELEASE and TLA2TOOLS_SHA256 together.
$(TLA2TOOLS_JAR):
	@mkdir -p "$(dir $@)"
	curl -fsSL -o "$@.tmp" https://github.com/mcavage/marsh/releases/download/$(TLA2TOOLS_RELEASE)/tla2tools.jar
	@echo "$(TLA2TOOLS_SHA256)  $@.tmp" | { command -v sha256sum >/dev/null && sha256sum -c - || shasum -a 256 -c -; } >/dev/null
	@mv "$@.tmp" "$@"

mcp:
	@test -x "$(MCP_BIN)" || { echo 'make mcp: install first with make install, or set MCP_BIN=/absolute/marsh-mcp'; exit 1; }
	@test -x "$(MCP_MARSH)" || { echo 'make mcp: install marsh outside this checkout, or set MCP_MARSH=/absolute/marsh'; exit 1; }
	@test -x "$(SBX_EXECUTABLE)" || { echo 'make mcp: install stock SBX v0.45.0 or newer on PATH, or set MARSH_SBX=/absolute/sbx'; exit 1; }
	@exec "$(MCP_BIN)" serve --workspace "$(CURDIR)" --marsh "$(MCP_MARSH)" --sbx "$(SBX_EXECUTABLE)"

kits:
	$(PYTHON) scripts/prepare-kit-inputs.py --commands "$(KIT_COMMANDS)" --output "$(KIT_PREPARED_INPUTS)" \
		$(if $(KIT_BUILD_INPUTS),--extra-inputs "$(KIT_BUILD_INPUTS)",)
	$(PYTHON) scripts/publish-kits.py --validate-only --commands "$(KIT_COMMANDS)" \
		--build-inputs "$(KIT_PREPARED_INPUTS)"
	$(PYTHON) scripts/prepare-kit-inputs.py --commands tests/acceptance/fixture/commands.json --output "$(KIT_FIXTURE_INPUTS)"
	$(PYTHON) scripts/publish-kits.py --validate-only --commands tests/acceptance/fixture/commands.json \
		--build-inputs "$(KIT_FIXTURE_INPUTS)"

kit-publish:
	@test -n "$(KIT_REPOSITORY_PREFIX)" || { echo 'make kit-publish: set KIT_REPOSITORY_PREFIX=registry/repository'; exit 1; }
	$(PYTHON) scripts/prepare-kit-inputs.py --commands "$(KIT_COMMANDS)" --output "$(KIT_PREPARED_INPUTS)" \
		$(if $(KIT_BUILD_INPUTS),--extra-inputs "$(KIT_BUILD_INPUTS)",)
	$(PYTHON) scripts/publish-kits.py --repository-prefix "$(KIT_REPOSITORY_PREFIX)" \
		$(if $(KIT_TAG),--tag "$(KIT_TAG)",) \
		--commands "$(KIT_COMMANDS)" --build-inputs "$(KIT_PREPARED_INPUTS)" \
		--output "$(KIT_RELEASE_COMMANDS)"

kit-publish-fixture:
	@test -n "$(KIT_REPOSITORY_PREFIX)" || { echo 'make kit-publish-fixture: set KIT_REPOSITORY_PREFIX=registry/repository'; exit 1; }
	$(PYTHON) scripts/prepare-kit-inputs.py --commands tests/acceptance/fixture/commands.json --output "$(KIT_FIXTURE_INPUTS)"
	$(PYTHON) scripts/publish-kits.py --repository-prefix "$(KIT_REPOSITORY_PREFIX)" \
		$(if $(KIT_TAG),--tag "$(KIT_TAG)",) \
		$(if $(KIT_REPOSITORY_NAME_PREFIX),--repository-name-prefix "$(KIT_REPOSITORY_NAME_PREFIX)",) \
		--commands tests/acceptance/fixture/commands.json --build-inputs "$(KIT_FIXTURE_INPUTS)" --output "$(KIT_FIXTURE_COMMANDS)"

install-preflight:
	@test "$$(uname -s)" = Darwin -a "$$(uname -m)" = arm64 || { echo 'make install: the host product requires an ARM64 Mac; Linux contributors can use make test or make build-host' >&2; exit 1; }

install: install-preflight
	$(MAKE) build
	$(MAKE) man
	$(PYTHON) scripts/third-party-notices.py --cargo "$(CARGO)" --output "$(TARGET_DIR)/notices"
	$(INSTALL) -d "$(PREFIX)/share/man/man1"
	$(INSTALL) -m 644 "$(MAN_DIR)"/*.1 "$(PREFIX)/share/man/man1/"
	$(INSTALL) -d "$(PREFIX)/libexec/marsh"
	$(INSTALL) -d "$(PREFIX)/bin"
	$(INSTALL) -d "$(PREFIX)/share/licenses/marsh"
	$(INSTALL) -m 644 LICENSE "$(PREFIX)/share/licenses/marsh/LICENSE"
	$(INSTALL) -m 644 NOTICE "$(PREFIX)/share/licenses/marsh/NOTICE"
	$(INSTALL) -m 644 "$(TARGET_DIR)/notices/THIRD-PARTY-NOTICES.txt" "$(PREFIX)/share/licenses/marsh/THIRD-PARTY-NOTICES.txt"
	$(INSTALL) -m 755 "$(TARGET_DIR)/release/marsh" "$(PREFIX)/bin/marsh"
	ln -sfn marsh "$(PREFIX)/bin/msh"
	$(INSTALL) -m 755 "$(TARGET_DIR)/release/marshd" "$(PREFIX)/bin/marshd"
	$(INSTALL) -m 755 "$(TARGET_DIR)/release/marsh-mcp" "$(PREFIX)/bin/marsh-mcp"
	$(INSTALL) -m 755 "$(GUEST_ARTIFACTS)/marsh-linux-arm64" "$(PREFIX)/libexec/marsh/marsh-linux-arm64"
	$(INSTALL) -m 755 "$(GUEST_ARTIFACTS)/marsh-local-linux-arm64" "$(PREFIX)/libexec/marsh/marsh-local-linux-arm64"
	$(INSTALL) -m 755 "$(GUEST_ARTIFACTS)/marsh-byte-exec-linux-arm64" "$(PREFIX)/libexec/marsh/marsh-byte-exec-linux-arm64"
	$(INSTALL) -m 755 "$(GUEST_ARTIFACTS)/marsh-worker-linux-arm64" "$(PREFIX)/libexec/marsh/marsh-worker-linux-arm64"
	$(INSTALL) -m 755 "$(GUEST_ARTIFACTS)/marsh-relay-linux-arm64" "$(PREFIX)/libexec/marsh/marsh-relay-linux-arm64"
	$(INSTALL) -m 755 "$(GUEST_ARTIFACTS)/marshd-linux-arm64" "$(PREFIX)/libexec/marsh/marshd-linux-arm64"
	$(INSTALL) -m 644 "$(GUEST_ARTIFACTS)/commands.json" "$(PREFIX)/libexec/marsh/commands.json"
	$(INSTALL) -m 644 "$(GUEST_ARTIFACTS)/shell-image" "$(PREFIX)/libexec/marsh/shell-image"
	$(INSTALL) -m 644 "$(GUEST_ARTIFACTS)/shell-image.build.json" "$(PREFIX)/libexec/marsh/shell-image.build.json"
	$(INSTALL) -m 644 "$(GUEST_ARTIFACTS)/svc-engine.tar" "$(PREFIX)/libexec/marsh/svc-engine.tar"
	$(INSTALL) -m 644 "$(GUEST_ARTIFACTS)/svc-engine-image.json" "$(PREFIX)/libexec/marsh/svc-engine-image.json"
	rm -rf "$(PREFIX)/libexec/marsh/licenses"
	cp -R "$(GUEST_ARTIFACTS)/licenses" "$(PREFIX)/libexec/marsh/licenses"
	rm -rf "$(PREFIX)/libexec/marsh/kits"
	cp -R "$(GUEST_ARTIFACTS)/kits" "$(PREFIX)/libexec/marsh/kits"

uninstall:
	@# A release tarball extracted straight into PREFIX also leaves its top-level
	@# README.md, LICENSE, NOTICE, and LOCAL-BUILD.txt there. Remove each only when
	@# it is marsh's (same bytes as the installed licenses, or marsh's own header),
	@# so a prefix's unrelated files of the same name survive.
	@for file in LICENSE NOTICE; do \
		if test -f "$(PREFIX)/$$file" && cmp -s "$(PREFIX)/$$file" "$(PREFIX)/share/licenses/marsh/$$file"; then \
			echo rm -f "$(PREFIX)/$$file"; rm -f "$(PREFIX)/$$file"; fi; \
	done
	@if test -f "$(PREFIX)/README.md" && head -n 1 "$(PREFIX)/README.md" | grep -q '^# marsh'; then \
		echo rm -f "$(PREFIX)/README.md"; rm -f "$(PREFIX)/README.md"; fi
	@if test -f "$(PREFIX)/LOCAL-BUILD.txt" && head -n 1 "$(PREFIX)/LOCAL-BUILD.txt" | grep -q '^marsh .* LOCAL BUILD'; then \
		echo rm -f "$(PREFIX)/LOCAL-BUILD.txt"; rm -f "$(PREFIX)/LOCAL-BUILD.txt"; fi
	rm -f "$(PREFIX)/bin/marsh" "$(PREFIX)/bin/msh" "$(PREFIX)/bin/marshd" "$(PREFIX)/bin/marsh-mcp"
	rm -rf "$(PREFIX)/libexec/marsh" "$(PREFIX)/share/licenses/marsh"
	rm -f "$(PREFIX)/share/man/man1/marsh.1" "$(PREFIX)/share/man/man1/msh.1"
	@printf '%s\n' 'Removed installed marsh files. Selected homes, scopes, receipts, and stock SBX resources remain.'

man:
	$(PYTHON) scripts/man.py "$(MAN_DIR)"

site:
	$(PYTHON) scripts/sitegen.py --out "$(SITE_DIR)"
	$(PYTHON) scripts/check-links.py "$(SITE_DIR)"

# Relative links in every tracked Markdown doc, plus the built site.
site-check: site
	$(PYTHON) scripts/check-links.py --markdown .

# Release packaging refuses the test-breaks feature (docs/design/test-breaks.md).
# This fails fast on the feature inputs this Makefile passes to cargo. The
# compile-time refusal (MARSH_RELEASE_PACKAGING=1) and the packaged-binary check
# in scripts/dist.sh are the backstops.
.PHONY: check-release-features
check-release-features:
	@case " $(HOST_FEATURES) $(ACCEPTANCE_HOST_FEATURES) $(DEV_FEATURES) " in *test-breaks*) echo 'make: test-breaks is a test-build feature; release packaging refuses it (drop it from HOST_FEATURES, ACCEPTANCE_HOST_FEATURES and DEV_FEATURES)' >&2; exit 1;; esac

# Release packaging. CI (.github/workflows/release.yml) builds the inputs:
# host binaries, guest artifacts, published Kit digests, and the shell image.
dist: check-release-features man
	@test -n "$(VERSION)" || { echo 'make dist: set VERSION'; exit 1; }
	$(PYTHON) scripts/third-party-notices.py --cargo "$(CARGO)" --output "$(TARGET_DIR)/notices"
	TARGET_DIR="$(TARGET_DIR)" GUEST_ARTIFACTS="$(GUEST_ARTIFACTS)" KIT_COMMANDS="$(KIT_COMMANDS)" \
		MAN_DIR="$(MAN_DIR)" NOTICES_DIR="$(TARGET_DIR)/notices" \
		sh scripts/dist.sh "$(VERSION)" "$(DIST_DIR)"

# A local-build tarball from `make dev`'s artifacts: source Kits and the
# locally imported shell image (it exists only in this Mac's SBX image store).
# Marked "local build" in the tarball and in `marsh --version`; never a release.
dist-local: install-preflight man
	@$(MAKE) --no-print-directory -j$(DEV_PARALLEL) dev-artifacts
	$(PYTHON) scripts/third-party-notices.py --cargo "$(CARGO)" --output "$(TARGET_DIR)/notices"
	LOCAL=1 TARGET_DIR=target GUEST_ARTIFACTS="$(DEV_GUEST)" KIT_COMMANDS="$(DEV_GUEST)/commands.json" \
		MAN_DIR="$(MAN_DIR)" NOTICES_DIR="$(TARGET_DIR)/notices" \
		sh scripts/dist.sh "$(VERSION)" "$(DIST_DIR)"

# Homebrew formula for the tap, from a dist tarball's checksum
# (FORMULA_CHANNEL=nightly renders marsh-nightly.rb).
formula:
	$(PYTHON) scripts/render-formula.py --version "$(VERSION)" --repository "$(GITHUB_REPOSITORY)" --channel "$(FORMULA_CHANNEL)" \
		--sha256 "$$(cut -d' ' -f1 "$(DIST_DIR)/marsh-$(VERSION)-darwin-arm64.tar.gz.sha256")" \
		--output "$(DIST_DIR)/$(if $(filter nightly,$(FORMULA_CHANNEL)),marsh-nightly,marsh).rb"

fixture-ref:
	@mkdir -p "$(dir $(FIXTURE_REF_FILE))"
	@# The newest release that publishes one, nightlies included: dev tests
	@# want the fixture built from the newest code, and a stable release's
	@# pinned image is not guaranteed to outlive the next ones.
	curl -fsSL "$$(curl -fsSL "https://api.github.com/repos/$(GITHUB_REPOSITORY)/releases?per_page=30" | $(PYTHON) -c 'import json,sys; print(next(a["browser_download_url"] for r in json.load(sys.stdin) if not r["draft"] for a in r["assets"] if a["name"] == "fixture-ref.txt"))')" -o "$(FIXTURE_REF_FILE).tmp"
	@grep -Eq '^[a-z0-9][a-z0-9._:/-]*@sha256:[0-9a-f]{64}$$' "$(FIXTURE_REF_FILE).tmp" || { echo 'make fixture-ref: downloaded file is not an immutable reference' >&2; rm -f "$(FIXTURE_REF_FILE).tmp"; exit 1; }
	@mv "$(FIXTURE_REF_FILE).tmp" "$(FIXTURE_REF_FILE)"
	@cat "$(FIXTURE_REF_FILE)"

acceptance:
	@test -n "$(ACCEPTANCE_KIT)" || { echo 'make acceptance: publish the fixture with make kit-publish-fixture, or set ACCEPTANCE_KIT to an immutable Kit OCI reference'; exit 1; }
	@test -x "$(SBX_EXECUTABLE)" || { echo 'make acceptance: install stock SBX v0.45.0 or newer on PATH, or set MARSH_SBX=/absolute/sbx'; exit 1; }
	$(call observed-acceptance,run)

# A break-mode candidate for the pen test: an explicitly identified path, never a release pass. It builds the host
# with the test-breaks feature (the receipt records HOST_FEATURES=test-breaks) and writes its evidence under a
# `breaks-candidate` name. The acceptance harness strips MARSH_TEST_BREAK, so modes reach this candidate only
# through the services harness, which launches its daemon explicitly (`marshd --home H --test-breaks-from-env`).
# The guest binaries need the Dockerfile's CARGO_FEATURES (H2); until it is merged they stay normal builds.
.PHONY: acceptance-breaks
acceptance-breaks:
	@echo 'make acceptance-breaks: building a TEST-BREAKS candidate (not a release pass; never ship its output)' >&2
	@$(MAKE) --no-print-directory acceptance ACCEPTANCE_HOST_FEATURES=test-breaks \
		ACCEPTANCE_EVIDENCE="$${TMPDIR:-/tmp}/marsh-acceptance-breaks-candidate-evidence"

acceptance-smoke:
	@test -n "$(ACCEPTANCE_KIT)" || { echo 'make acceptance-smoke: publish the fixture with make kit-publish-fixture, or set ACCEPTANCE_KIT to an immutable Kit OCI reference'; exit 1; }
	@test -x "$(SBX_EXECUTABLE)" || { echo 'make acceptance-smoke: install stock SBX v0.45.0 or newer on PATH, or set MARSH_SBX=/absolute/sbx'; exit 1; }
	$(call observed-acceptance,smoke)

# Build and evidence stay outside the guest-writable checkout. Every invocation
# gets an empty output tree; historical target/release files cannot qualify it.
define observed-acceptance
@set -eu; \
candidate_root=$$(mktemp -d "$${TMPDIR:-/tmp}/marsh-acceptance.XXXXXX"); \
candidate_root=$$(cd "$$candidate_root" && pwd -P); \
evidence="$(ACCEPTANCE_EVIDENCE)"; \
test -n "$$evidence" || evidence="$$candidate_root/evidence/$(1)"; \
$(PYTHON) scripts/build-candidate.py \
	--source-tree "$(CURDIR)" --source-revision "$(SOURCE_REVISION)" \
	--target-dir "$$candidate_root/target" \
	--marsh "$$candidate_root/target/release/marsh" \
	--guest-artifacts "$$candidate_root/guest" \
	--registry "$(KIT_COMMANDS)" --receipt "$$candidate_root/build.json" \
	--cargo "$(CARGO)" --docker "$(DOCKER)" --sbx "$(SBX_EXECUTABLE)" \
	--host-features "$(ACCEPTANCE_HOST_FEATURES)" --rust-image "$(RUST_IMAGE)" \
	--shell-image-repository "$(SHELL_IMAGE_REPOSITORY)" $(if $(filter 1,$(SHELL_IMAGE_INSECURE_REGISTRY)),--shell-image-insecure-registry,); \
$(PYTHON) tests/acceptance/$(1).py \
	--marsh "$$candidate_root/target/release/marsh" \
	--guest-artifacts "$$candidate_root/guest" \
	--build-receipt "$$candidate_root/build.json" \
	--sbx "$(SBX_EXECUTABLE)" --kit "$(ACCEPTANCE_KIT)" \
	--evidence "$$evidence" --source-revision "$(SOURCE_REVISION)" \
	--source-tree "$(CURDIR)"
endef

perf:
	@test -n "$(PERF_KIT)" || { echo 'make perf: set PERF_KIT to an immutable Kit OCI reference'; exit 1; }
	@test -x "$(SBX_EXECUTABLE)" || { echo 'make perf: install stock SBX v0.45.0 or newer on PATH, or set MARSH_SBX=/absolute/sbx'; exit 1; }
	@set -eu; \
	candidate_root=$$(mktemp -d "$${TMPDIR:-/tmp}/marsh-perf.XXXXXX"); \
	candidate_root=$$(cd "$$candidate_root" && pwd -P); \
	output="$(PERF_OUTPUT)"; \
	test -n "$$output" || output="$$candidate_root/evidence/result.json"; \
	$(PYTHON) scripts/build-candidate.py \
		--source-tree "$(CURDIR)" --source-revision "$(SOURCE_REVISION)" \
		--target-dir "$$candidate_root/target" \
		--marsh "$$candidate_root/target/release/marsh" \
		--guest-artifacts "$$candidate_root/guest" \
		--registry "$(KIT_COMMANDS)" --receipt "$$candidate_root/build.json" \
		--cargo "$(CARGO)" --docker "$(DOCKER)" --sbx "$(SBX_EXECUTABLE)" \
		--host-features "$(ACCEPTANCE_HOST_FEATURES)" --rust-image "$(RUST_IMAGE)" \
		--shell-image-repository "$(SHELL_IMAGE_REPOSITORY)" $(if $(filter 1,$(SHELL_IMAGE_INSECURE_REGISTRY)),--shell-image-insecure-registry,); \
	$(PYTHON) tests/perf/run.py \
		--marsh "$$candidate_root/target/release/marsh" \
		--guest-artifacts "$$candidate_root/guest" \
		--build-receipt "$$candidate_root/build.json" \
		--source-tree "$(CURDIR)" --source-revision "$(SOURCE_REVISION)" \
		--sbx "$(SBX_EXECUTABLE)" --kit "$(PERF_KIT)" \
		--samples "$(PERF_SAMPLES)" --warmups "$(PERF_WARMUPS)" --output "$$output"

# Developer loop: `make dev` builds host, guest, the shell image and packaged
# source Kits (parallel, incremental) and installs a ready dev product outside
# the checkout in DEV_PREFIX. The install's libexec/marsh/dev-enabled marker
# marks a dev build: `marsh --version` says so, its daemon follows no release
# channel, and `marshd --check-config` accepts source Kits.
DEV_PREFIX ?= $(HOME)/.marsh-dev
# Cargo features for the host and Linux builds, e.g. DEV_FEATURES=test-breaks
# (docs/design/test-breaks.md). Empty by default.
DEV_FEATURES ?=
DEV_GUEST ?= target/libexec/marsh
DEV_JOBS ?= 8
DEV_PARALLEL ?= 5
DEV_CACHE_ID ?= marsh-dev-$(shell pwd -P | cksum | cut -d' ' -f1)
DEV_KIT ?= $(ACCEPTANCE_KIT)
DEV_SUPPORT_SCRIPTS := $(addprefix scripts/,prepare-shell-image.py build_inputs.py image_observations.py owned_process.py stock_sdk.py)
DEV_CANONICAL_INPUTS := $(shell find packaging/image-base packaging/dhi-notices/collected -type f 2>/dev/null)
DEV_SHELL_IMAGE_INPUTS := packaging/shell-image packaging/shell/Dockerfile $(DEV_SUPPORT_SCRIPTS) $(DEV_CANONICAL_INPUTS) \
	$(shell find packaging/shell/editors -type f 2>/dev/null)
DEV_KIT_INPUTS := $(KIT_COMMANDS) scripts/stage-kits.py scripts/publish-kits.py \
	$(DEV_CANONICAL_INPUTS) $(wildcard $(shell git ls-files kits 2>/dev/null))

.PHONY: dev dev-artifacts dev-host dev-guest dev-install dev-kit-images
dev:
	@$(MAKE) --no-print-directory -j$(DEV_PARALLEL) dev-artifacts
	@$(MAKE) --no-print-directory dev-install
	@$(MAKE) --no-print-directory dev-kit-images

dev-artifacts: dev-host dev-guest $(DEV_GUEST)/shell-image $(DEV_GUEST)/svc-engine-image.json $(DEV_GUEST)/.kits.stamp

dev-host:
	$(call HOST_CARGO_BUILD,target) $(if $(ACCEPTANCE_HOST_FEATURES),--features "$(ACCEPTANCE_HOST_FEATURES)") $(if $(DEV_FEATURES),--features "$(DEV_FEATURES)")

dev-guest:
	@mkdir -p "$(DEV_GUEST)"
	$(DOCKER) buildx build --platform linux/arm64 --progress=quiet \
		--build-arg "RUST_IMAGE=$(RUST_IMAGE)" --build-arg "BUILD_CACHE_ID=$(DEV_CACHE_ID)" \
		--build-arg "CARGO_JOBS=$(DEV_JOBS)" --build-arg "CARGO_FEATURES=$(DEV_FEATURES)" \
		--target export --output "type=local,dest=$(DEV_GUEST)" \
		-f packaging/linux-arm64/Dockerfile .
	$(INSTALL) -m 644 "$(KIT_COMMANDS)" "$(DEV_GUEST)/commands.json"
	@: > "$(DEV_GUEST)/dev-enabled"

# Image targets rebuild only when their inputs change; delete the file to force
# a re-import.
$(DEV_GUEST)/shell-image: $(DEV_SHELL_IMAGE_INPUTS)
	@mkdir -p "$(DEV_GUEST)"
	$(PYTHON) scripts/prepare-shell-image.py --docker "$(DOCKER)" --sbx "$(SBX_EXECUTABLE)" --output "$@"

$(DEV_GUEST)/svc-engine-image.json: packaging/svc-engine/Dockerfile packaging/shell-image packaging/shell/Dockerfile scripts/prepare-svc-engine-image.py
	@mkdir -p "$(DEV_GUEST)"
	$(PYTHON) scripts/prepare-svc-engine-image.py --docker "$(DOCKER)" --output "$(DEV_GUEST)"

# Source Kits with the canonical inputs publication adds (scripts/stage-kits.py).
$(DEV_GUEST)/.kits.stamp: $(DEV_KIT_INPUTS)
	$(PYTHON) scripts/stage-kits.py --commands "$(KIT_COMMANDS)" --output "$(DEV_GUEST)"
	@touch "$@"

dev-install:
	@set -eu; prefix="$(DEV_PREFIX)"; checkout=$$(pwd -P); \
	case "$$prefix" in /*) ;; *) echo 'make dev: DEV_PREFIX must be absolute' >&2; exit 1;; esac; \
	mkdir -p "$$prefix/bin" "$$prefix/libexec/marsh"; real=$$(cd "$$prefix" && pwd -P); \
	case "$$real/" in "$$checkout"/*) echo 'make dev: DEV_PREFIX must be outside the checkout' >&2; exit 1;; esac; \
	rsync -a target/release/marsh target/release/marshd target/release/marsh-mcp "$$prefix/bin/"; \
	ln -sfn marsh "$$prefix/bin/msh"; \
	rsync -a --delete --exclude '.kits*' "$(DEV_GUEST)/" "$$prefix/libexec/marsh/"; \
	printf 'marsh dev product: %s\n' "$$prefix"; \
	printf 'Run: %s/bin/marsh\n' "$$prefix"

# Packaged Kit job images through the daemon's own Buildx path into the
# per-user Kit image cache (~/Library/Caches/marsh/kit-images), keyed by source
# fingerprint: a first Kit use then pays only VM boot and image load. Unchanged
# Kits are skipped; DEV_KIT_IMAGES=0 opts out.
DEV_KIT_IMAGES ?= 1
dev-kit-images:
	@if [ "$(DEV_KIT_IMAGES)" = 1 ]; then "$(DEV_PREFIX)/bin/marshd" --prebuild-kit-images "$(DEV_PREFIX)/libexec/marsh"; fi

# The sbx-backed tiers (CONTRIBUTING.md), all on a Mac against the installed
# dev product: `make check` (seconds), `make verify` (the pre-push gate,
# minutes), `make regress` (every sbx-backed suite, then make test with the
# full model). tests/regress.py is the one scheduler: isolated scopes, bounded
# adaptive concurrency (REGRESS_JOBS=1 runs one suite at a time), per-suite
# logs in target/regress/. ONLY= / SKIP= take suite names, families, or
# scenario ids (ONLY=smoke, ONLY=workspaces, ONLY=W01-plain-bash,P02-self-spawn-bash-c;
# python3 tests/regress.py --list --kit REF). With ONLY, make regress runs just
# those suites. VERIFY_DEPS= skips the incremental `make dev` rebuild.
.PHONY: check check-reset check-gc regress verify
check:
	@test -n "$(DEV_KIT)" || { echo 'make check: run make fixture-ref, or set DEV_KIT to an immutable fixture Kit OCI reference'; exit 1; }
	@$(TEST_CANARY) $(PYTHON) tests/check.py --prefix "$(DEV_PREFIX)" --sbx "$(SBX_EXECUTABLE)" --kit "$(DEV_KIT)"

check-reset:
	@$(PYTHON) tests/check.py --reset --prefix "$(DEV_PREFIX)" --sbx "$(SBX_EXECUTABLE)"

# Warm scopes are registered outside the checkout; this retires (by recorded
# VM name and UUID only) those whose home was deleted with its worktree.
check-gc:
	@$(PYTHON) tests/acceptance/scope_registry.py gc --sbx "$(SBX_EXECUTABLE)"

VERIFY_DEPS ?= dev
REGRESS_JOBS ?= 3
REGRESS = $(TEST_CANARY) $(PYTHON) tests/regress.py --jobs $(REGRESS_JOBS) --prefix "$(DEV_PREFIX)" --sbx "$(SBX_EXECUTABLE)" \
	--kit "$(DEV_KIT)" $(foreach s,$(ONLY),--only $(s)) $(foreach s,$(SKIP),--skip $(s))
verify: $(VERIFY_DEPS)
	@test -n "$(DEV_KIT)" || { echo 'make verify: run make fixture-ref, or set DEV_KIT to an immutable fixture Kit OCI reference'; exit 1; }
	$(REGRESS) --tier verify

regress: $(VERIFY_DEPS)
	@test -n "$(DEV_KIT)" || { echo 'make regress: run make fixture-ref, or set DEV_KIT to an immutable fixture Kit OCI reference'; exit 1; }
	$(REGRESS) --tier regress
	$(if $(ONLY),,$(MAKE) --no-print-directory test MODEL_FLAGS= MODEL_CACHE=)

# Services E2E (docs TEST-PLAN A.5): real sbx, one VM lock (~/dev/narwhal-fork-spike/harness/lock),
# isolated scope with MARSH_VM_PREFIX=marsh-xnfx01-. TIER=smoke|full|perf|soak|break, ONLY=ID,ID,
# BREAK=mode[,mode] verifies one break (the test-breaks install, BREAKS_PREFIX, default $(DEV_PREFIX)-breaks;
# a defect's holds and faults are added for you); TIER=break runs the three-pass protocol,
# DRIVER=internal|product. `make dev DEV_FEATURES=test-breaks DEV_PREFIX=...-breaks` builds the breaks install.
TIER ?= smoke
DRIVER ?= product
BREAKS_PREFIX ?= $(DEV_PREFIX)-breaks
LOCK_WAIT ?= 0
H2_TREE ?=
SERVICES_E2E_ARGS ?=
comma := ,
.PHONY: services-e2e services-e2e-gc
services-e2e:
	@$(TEST_CANARY) $(PYTHON) tests/acceptance/services/services_uat.py --prefix "$(DEV_PREFIX)" --breaks-prefix "$(BREAKS_PREFIX)" \
		--sbx "$(SBX_EXECUTABLE)" --tier "$(TIER)" --driver "$(DRIVER)" --lock-wait "$(LOCK_WAIT)" \
		$(if $(DEV_KIT),--kit "$(DEV_KIT)") $(if $(H2_TREE),--h2-tree "$(H2_TREE)") \
		$(foreach s,$(subst $(comma), ,$(ONLY)),--only $(s)) $(if $(BREAK),--break "$(BREAK)") \
		$(if $(EVIDENCE),--evidence "$(EVIDENCE)") $(SERVICES_E2E_ARGS)

# Remove nfx-/marsh-xnfx01- resources, run records and roots of runs that are confirmed dead (takes the VM lock).
# DRY_RUN=1 only reports. A run never removes another run's resources; this does, once their run is dead.
services-e2e-gc:
	@$(PYTHON) tests/acceptance/services/services_uat.py --gc --sbx "$(SBX_EXECUTABLE)" --lock-wait "$(LOCK_WAIT)" $(if $(DRY_RUN),--dry-run)
