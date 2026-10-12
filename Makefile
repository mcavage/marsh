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
RUST_IMAGE ?= rust:1.95-bookworm@sha256:6258907abe69656e41cd992e0b705cdcfabcbbe3db374f92ed2d47121282d4a1
BUILD_CACHE_ID ?= marsh-$(shell pwd -P | cksum | cut -d' ' -f1)
ACCEPTANCE_EVIDENCE ?=
ACCEPTANCE_HOST_FEATURES ?=
# The acceptance fixture Kit: a local `make kit-publish-fixture` output, else
# the ref fetched by `make fixture-ref` (published by release CI).
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
MODEL_FLAGS ?= --quick
MODEL_CACHE ?= $(abspath $(TARGET_DIR))/tlc-cache
# The MCP load callers (#[ignore]d: they need MARSH_LOAD_TEST_BIN, a built marsh
# with its sibling marsh-mcp and marshd).
MCP_LOAD_CALLERS := (package(marsh-mcp) & binary(mcp_load)) | (package(marsh-daemon) & test(/^mcp_load_tests::/))
# Export serving is a Mac host boundary, not a Linux contributor capability.
MCP_TEST_FILES := test_stdio_protocol.py test_guest_export_boundary.py test_publication_cli.py \
	$(if $(filter Darwin,$(shell uname -s)),test_export_stdio_protocol.py)

.PHONY: help build build-host build-linux prepare-shell-image test test-rust test-harness test-model mcp kits kit-publish kit-publish-fixture install install-preflight uninstall acceptance acceptance-smoke perf man site site-check dist dist-local fixture-ref formula

help:
	@printf '%s\n' \
		'Tests (CONTRIBUTING.md):' \
		'make test              The VM-free gate CI runs: test-rust, test-harness, test-model' \
		'make test-rust         fmt, clippy, every workspace test (cargo-nextest), doctests, MCP load callers' \
		'make test-harness      Python/shell harnesses: onboarding, MCP callers, publisher, tool self-tests' \
		'make test-model        Quick TLA+ sweep (MODEL_FLAGS= for every config)' \
		'make check [DEV_KIT=ref]  Mac: fast real smoke of the installed dev product (warm scope in target/check)' \
		'make check-reset       Mac: stop the make check scope and delete target/check' \
		'make verify [DEV_KIT=ref]  Mac, before pushing: check + the highest-signal sbx suites, concurrently' \
		'make regress [DEV_KIT=ref]  Mac, before a release: every sbx suite, then make test with the full model' \
		'                        ONLY=/SKIP= suites, families or scenario ids; REGRESS_JOBS=1 one at a time' \
		'make acceptance        Build a fresh candidate and run the full local-v3 stock-SBX gate (release run)' \
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
		'make dist              Package a release tarball (needs pinned KIT_COMMANDS and shell-image)' \
		'make dev-inner         Inside marsh --dev: build a Linux candidate into $$MARSH_DEV_SCRATCH/artifacts' \
		'make dev-inner-run CMD=...  Inside marsh --dev: run the inner candidate via the broker'

build: build-host build-linux

build-host:
	$(CARGO) build --workspace --release --locked --target-dir "$(TARGET_DIR)" $(if $(HOST_FEATURES),--features "$(HOST_FEATURES)")

build-linux:
	@mkdir -p "$(GUEST_ARTIFACTS)"
	$(DOCKER) buildx build --platform linux/arm64 \
		--build-arg "RUST_IMAGE=$(RUST_IMAGE)" \
		--build-arg "BUILD_CACHE_ID=$(BUILD_CACHE_ID)" \
		--target export --output "type=local,dest=$(GUEST_ARTIFACTS)" \
		-f packaging/linux-arm64/Dockerfile .
	$(INSTALL) -m 644 "$(KIT_COMMANDS)" "$(GUEST_ARTIFACTS)/commands.json"
	$(INSTALL) -m 644 packaging/agents.json "$(GUEST_ARTIFACTS)/agents.json"
	$(MAKE) prepare-shell-image
	$(PYTHON) scripts/stage-kits.py --commands "$(KIT_COMMANDS)" --output "$(GUEST_ARTIFACTS)"

prepare-shell-image:
	$(PYTHON) scripts/prepare-shell-image.py --docker "$(DOCKER)" --sbx "$(SBX_EXECUTABLE)" \
		$(if $(SHELL_IMAGE_REPOSITORY),--repository "$(SHELL_IMAGE_REPOSITORY)",) --output "$(GUEST_ARTIFACTS)/shell-image" \
		$(if $(filter 1,$(SHELL_IMAGE_INSECURE_REGISTRY)),--insecure-registry,)

# `make test` is the VM-free gate, and CI runs these same targets
# (.github/workflows/ci.yml): test-rust and test-harness on Linux and macOS,
# the model in its own job. Nothing here starts a VM or needs credentials.
test: test-rust test-harness test-model

# Every workspace test, one process per test (cargo-nextest; .config/nextest.toml
# fails a hung test by name). nextest skips doctests, so cargo runs those.
# Then the #[ignore]d MCP load callers, which drive the built marsh, marsh-mcp
# and marshd binaries (no VM).
test-rust:
	@$(CARGO) nextest --version >/dev/null 2>&1 || { echo 'make test: cargo-nextest is required: brew install cargo-nextest (or cargo install --locked cargo-nextest)' >&2; exit 1; }
	$(CARGO) fmt --all -- --check
	$(CARGO) clippy --workspace --all-targets --locked --target-dir "$(TARGET_DIR)" -- -D warnings
	$(CARGO) nextest run --workspace --locked --target-dir "$(TARGET_DIR)" --profile "$(NEXTEST_PROFILE)"
	$(CARGO) test --workspace --doc --locked --target-dir "$(TARGET_DIR)"
	$(CARGO) build --workspace --bins --locked --target-dir "$(TARGET_DIR)"
	MARSH_LOAD_TEST_BIN="$(TEST_BIN)/marsh" $(CARGO) nextest run --workspace --locked --target-dir "$(TARGET_DIR)" \
		--profile "$(NEXTEST_PROFILE)" --run-ignored only -E '$(MCP_LOAD_CALLERS)'

# Python and shell harnesses against the debug binaries: shipped-entrypoint
# journeys (onboarding), MCP protocol callers, the Kit publisher, and the
# self-tests of the acceptance, perf and release tooling.
test-harness:
	$(CARGO) build --workspace --bins --locked --target-dir "$(TARGET_DIR)"
	$(PYTHON) tests/acceptance/onboarding.py --marsh "$(TEST_BIN)/marsh" --shell "$(TEST_BIN)/marsh-local" \
		--evidence "$(abspath $(TARGET_DIR))/onboarding.json"
	MARSH_BINARY="$(TEST_BIN)/marsh" MARSH_MCP_BIN="$(TEST_BIN)/marsh-mcp" \
		$(PYTHON) tests/parallel_unittest.py -j 3 -W error $(addprefix tests/mcp/,$(MCP_TEST_FILES))
	$(PYTHON) -W error -m unittest discover -s tests/acceptance -p 'test_*.py'
	$(PYTHON) -W error -m unittest discover -s tests/release -p 'test_*.py'
	$(PYTHON) -m unittest discover -s tests/perf -p 'test_*.py'
	sh tests/kits/selected-home.sh
	sh tests/kits/cli-adapters.sh
	sh tests/kits/shell-adapter.sh
	$(MAKE) --no-print-directory "$(PUBLISHER_VENV)/.ready"
	"$(PUBLISHER_VENV)/bin/python" tests/parallel_unittest.py -j 3 -W error tests/kits/test_publisher_cli.py

# The publisher's one dependency, hash-pinned.
$(PUBLISHER_VENV)/.ready: scripts/requirements-publish.txt
	rm -rf "$(PUBLISHER_VENV)"
	$(PYTHON) -m venv "$(PUBLISHER_VENV)"
	"$(PUBLISHER_VENV)/bin/python" -m pip install --quiet --require-hashes --no-deps --only-binary=:all: -r scripts/requirements-publish.txt
	@touch "$@"

# The TLA+ model (docs/model/README.md). `make test` runs the quick sweep with
# verdicts cached by spec hash; MODEL_FLAGS= runs every config (make regress).
test-model:
	TLC_CACHE="$(MODEL_CACHE)" docs/model/check.sh $(MODEL_FLAGS)

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
	$(INSTALL) -m 644 "$(TARGET_DIR)/notices/rust-package-notices.json" "$(PREFIX)/share/licenses/marsh/rust-package-notices.json"
	$(INSTALL) -m 644 "$(TARGET_DIR)/notices/embedded-native-notices.json" "$(PREFIX)/share/licenses/marsh/embedded-native-notices.json"
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
	$(INSTALL) -m 644 "$(GUEST_ARTIFACTS)/agents.json" "$(PREFIX)/libexec/marsh/agents.json"
	$(INSTALL) -m 644 "$(GUEST_ARTIFACTS)/shell-image" "$(PREFIX)/libexec/marsh/shell-image"
	$(INSTALL) -m 644 "$(GUEST_ARTIFACTS)/shell-image.build.json" "$(PREFIX)/libexec/marsh/shell-image.build.json"
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

# Release packaging. CI (.github/workflows/release.yml) builds the inputs:
# host binaries, guest artifacts, published Kit digests, and the shell image.
dist: man
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
	curl -fsSL "https://github.com/$(GITHUB_REPOSITORY)/releases/latest/download/fixture-ref.txt" -o "$(FIXTURE_REF_FILE).tmp"
	@grep -Eq '^[a-z0-9][a-z0-9._:/-]*@sha256:[0-9a-f]{64}$$' "$(FIXTURE_REF_FILE).tmp" || { echo 'make fixture-ref: downloaded file is not an immutable reference' >&2; rm -f "$(FIXTURE_REF_FILE).tmp"; exit 1; }
	@mv "$(FIXTURE_REF_FILE).tmp" "$(FIXTURE_REF_FILE)"
	@cat "$(FIXTURE_REF_FILE)"

acceptance:
	@test -n "$(ACCEPTANCE_KIT)" || { echo 'make acceptance: publish the fixture with make kit-publish-fixture, or set ACCEPTANCE_KIT to an immutable Kit OCI reference'; exit 1; }
	@test -x "$(SBX_EXECUTABLE)" || { echo 'make acceptance: install stock SBX v0.45.0 or newer on PATH, or set MARSH_SBX=/absolute/sbx'; exit 1; }
	$(call observed-acceptance,run)

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

# Developer loop: `make dev` builds host, guest, the shell image (which carries
# the dev tooling) and packaged source Kits (parallel, incremental) and installs
# a ready dev product outside the checkout in DEV_PREFIX. The install's
# libexec/marsh/dev-enabled marker enables `marsh --dev` (the dev broker) on its
# daemon; no environment variables are needed.
DEV_PREFIX ?= $(HOME)/.marsh-dev
DEV_GUEST ?= target/libexec/marsh
DEV_JOBS ?= 8
DEV_PARALLEL ?= 5
DEV_CACHE_ID ?= marsh-dev-$(shell pwd -P | cksum | cut -d' ' -f1)
DEV_KIT ?= $(ACCEPTANCE_KIT)
DEV_SUPPORT_SCRIPTS := $(addprefix scripts/,prepare-shell-image.py build_inputs.py image_observations.py owned_process.py stock_sdk.py)
DEV_CANONICAL_INPUTS := $(shell find packaging/image-repair packaging/dhi-notices/collected -type f 2>/dev/null)
DEV_SHELL_IMAGE_INPUTS := packaging/shell-image packaging/shell/Dockerfile $(DEV_SUPPORT_SCRIPTS) $(DEV_CANONICAL_INPUTS) \
	$(shell find packaging/shell/pi -type f 2>/dev/null)
DEV_KIT_INPUTS := $(KIT_COMMANDS) scripts/stage-kits.py scripts/publish-kits.py scripts/npm-notices.mjs \
	$(DEV_CANONICAL_INPUTS) $(wildcard $(shell git ls-files kits 2>/dev/null))

.PHONY: dev dev-artifacts dev-host dev-guest dev-install dev-kit-images
dev:
	@$(MAKE) --no-print-directory -j$(DEV_PARALLEL) dev-artifacts
	@$(MAKE) --no-print-directory dev-install
	@$(MAKE) --no-print-directory dev-kit-images

dev-artifacts: dev-host dev-guest $(DEV_GUEST)/shell-image $(DEV_GUEST)/.kits.stamp

dev-host:
	$(CARGO) build --workspace --release --locked --target-dir target $(if $(ACCEPTANCE_HOST_FEATURES),--features "$(ACCEPTANCE_HOST_FEATURES)")

dev-guest:
	@mkdir -p "$(DEV_GUEST)"
	$(DOCKER) buildx build --platform linux/arm64 --progress=quiet \
		--build-arg "RUST_IMAGE=$(RUST_IMAGE)" --build-arg "BUILD_CACHE_ID=$(DEV_CACHE_ID)" \
		--build-arg "CARGO_JOBS=$(DEV_JOBS)" \
		--target export --output "type=local,dest=$(DEV_GUEST)" \
		-f packaging/linux-arm64/Dockerfile .
	$(INSTALL) -m 644 "$(KIT_COMMANDS)" "$(DEV_GUEST)/commands.json"
	$(INSTALL) -m 644 packaging/agents.json "$(DEV_GUEST)/agents.json"
	@rm -f "$(DEV_GUEST)"/dev-shell-image*; : > "$(DEV_GUEST)/dev-enabled"

# Image targets rebuild only when their inputs change; delete the file to force
# a re-import.
$(DEV_GUEST)/shell-image: $(DEV_SHELL_IMAGE_INPUTS)
	@mkdir -p "$(DEV_GUEST)"
	$(PYTHON) scripts/prepare-shell-image.py --docker "$(DOCKER)" --sbx "$(SBX_EXECUTABLE)" --output "$@"

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
	printf 'Run: %s/bin/marsh --dev -c pi-dev\n' "$$prefix"

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
.PHONY: check check-reset regress verify
check:
	@test -n "$(DEV_KIT)" || { echo 'make check: run make fixture-ref, or set DEV_KIT to an immutable fixture Kit OCI reference'; exit 1; }
	@$(PYTHON) tests/check.py --prefix "$(DEV_PREFIX)" --sbx "$(SBX_EXECUTABLE)" --kit "$(DEV_KIT)"

check-reset:
	@$(PYTHON) tests/check.py --reset --prefix "$(DEV_PREFIX)" --sbx "$(SBX_EXECUTABLE)"

VERIFY_DEPS ?= dev
REGRESS_JOBS ?= 3
REGRESS = $(PYTHON) tests/regress.py --jobs $(REGRESS_JOBS) --prefix "$(DEV_PREFIX)" --sbx "$(SBX_EXECUTABLE)" \
	--kit "$(DEV_KIT)" $(foreach s,$(ONLY),--only $(s)) $(foreach s,$(SKIP),--skip $(s))
verify: $(VERIFY_DEPS)
	@test -n "$(DEV_KIT)" || { echo 'make verify: run make fixture-ref, or set DEV_KIT to an immutable fixture Kit OCI reference'; exit 1; }
	$(REGRESS) --tier verify

regress: $(VERIFY_DEPS)
	@test -n "$(DEV_KIT)" || { echo 'make regress: run make fixture-ref, or set DEV_KIT to an immutable fixture Kit OCI reference'; exit 1; }
	$(REGRESS) --tier regress
	$(if $(ONLY),,$(MAKE) --no-print-directory test MODEL_FLAGS= MODEL_CACHE=)

.PHONY: dev-inner dev-inner-run dev-inner-stop

# Inside `marsh --dev` (Linux): build a candidate into the host scratch and run
# it. Its daemon reaches stock SBX only through the session's `sbx` shim; the
# Mac target/ is never written. Artifacts and the VM-local cargo target persist.
DEV_SCRATCH ?= $(MARSH_DEV_SCRATCH)
DEV_INNER_ARTIFACTS ?= $(DEV_SCRATCH)/artifacts
DEV_INNER_TARGET ?= /var/tmp/marsh-dev-target
DEV_INNER_ENV = RUSTUP_HOME=/usr/local/rustup CARGO_HOME=/usr/local/cargo RUSTUP_TOOLCHAIN=1.95.0 \
	CARGO_TARGET_DIR="$(DEV_INNER_TARGET)" CARGO_INCREMENTAL=0 CARGO_PROFILE_RELEASE_DEBUG=0
DEV_INNER_RUN_ENV = env -u MARSH_DAEMON_SOCKET -u MARSH_DAEMON_TOKEN -u MARSH_EXTERNAL_SESSION \
	MARSH_HOME="$(DEV_SCRATCH)/home" MARSH_CONTROL_HOME="$(DEV_SCRATCH)/control" \
	MARSH_SBX="$(DEV_SCRATCH)/tmp/bin/sbx" TMPDIR="$(DEV_SCRATCH)/tmp" MARSH_ENABLE_DEV_SCOPES=1
CMD ?= true
DEV_INNER_FLAGS ?=

dev-inner:
	@test "$$(uname -s)" = Linux -a -n "$(DEV_SCRATCH)" || { echo 'make dev-inner: run inside marsh --dev' >&2; exit 1; }
	$(DEV_INNER_ENV) cargo build --locked --release \
		-p marsh --bin marsh --bin marsh-local -p marsh-daemon --bin marsh-relay -p marsh-backend --bin marshd
	$(DEV_INNER_ENV) CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER="$$(RUSTUP_HOME=/usr/local/rustup RUSTUP_TOOLCHAIN=1.95.0 rustc --print sysroot)/lib/rustlib/aarch64-unknown-linux-gnu/bin/rust-lld" \
		RUSTFLAGS='-C target-feature=+crt-static' cargo build --locked --release --target aarch64-unknown-linux-musl \
		-p marsh-worker --bin marsh-worker -p marsh-runtime --bin marsh-byte-exec
	@set -eu; out="$(DEV_INNER_ARTIFACTS)"; rel="$(DEV_INNER_TARGET)/release"; musl="$(DEV_INNER_TARGET)/aarch64-unknown-linux-musl/release"; \
	mkdir -p "$$out/bin" "$$out/libexec/marsh"; \
	install -m 755 "$$rel/marsh" "$$out/bin/marsh"; install -m 755 "$$rel/marshd" "$$out/bin/marshd"; \
	install -m 755 "$$rel/marsh" "$$out/libexec/marsh/marsh-linux-arm64"; \
	install -m 755 "$$rel/marsh-local" "$$out/libexec/marsh/marsh-local-linux-arm64"; \
	install -m 755 "$$rel/marsh-relay" "$$out/libexec/marsh/marsh-relay-linux-arm64"; \
	install -m 755 "$$rel/marshd" "$$out/libexec/marsh/marshd-linux-arm64"; \
	install -m 755 "$$musl/marsh-worker" "$$out/libexec/marsh/marsh-worker-linux-arm64"; \
	install -m 755 "$$musl/marsh-byte-exec" "$$out/libexec/marsh/marsh-byte-exec-linux-arm64"; \
	python3 -c 'import json,sys; c=json.load(open(sys.argv[1])); k=sys.argv[2]; c.update({"fixture": k} if k else {}); json.dump(c, open(sys.argv[3],"w"), indent=2)' \
		"$(KIT_COMMANDS)" "$(DEV_KIT)" "$$out/libexec/marsh/commands.json"; \
	install -m 644 packaging/agents.json "$$out/libexec/marsh/agents.json"; \
	python3 scripts/stage-kits.py --commands "$(KIT_COMMANDS)" --output "$$out/libexec/marsh"; \
	printf '%s\n' "$$MARSH_DEV_SHELL_TEMPLATE" > "$$out/libexec/marsh/shell-image"; \
	echo "dev-inner: candidate in $$out (prefix $$MARSH_VM_PREFIX)"

dev-inner-run:
	@test -x "$(DEV_INNER_ARTIFACTS)/bin/marsh" || { echo 'make dev-inner-run: run make dev-inner first' >&2; exit 1; }
	@mkdir -p "$(DEV_SCRATCH)/home" "$(DEV_SCRATCH)/control"
	cd "$(CURDIR)" && $(DEV_INNER_RUN_ENV) "$(DEV_INNER_ARTIFACTS)/bin/marsh" $(DEV_INNER_FLAGS) -c '$(CMD)'

dev-inner-stop:
	-cd "$(CURDIR)" && $(DEV_INNER_RUN_ENV) "$(DEV_INNER_ARTIFACTS)/bin/marsh" stop
