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
GITHUB_REPOSITORY ?= mcavage/marsh
# Export serving is a Mac host boundary, not a Linux contributor capability.
# Publication CLI runs once in mcp-load-callers, not again in discovery.
MCP_TEST_FILES := $(filter-out test_publication_cli.py,$(if $(filter Darwin,$(shell uname -s)),$(notdir $(wildcard tests/mcp/test_*.py)),test_stdio_protocol.py test_guest_export_boundary.py))

.PHONY: help build build-host build-linux prepare-shell-image test onboarding mcp mcp-test kits kit-publish kit-publish-fixture install install-preflight uninstall acceptance acceptance-smoke perf dev-check man site site-check dist dist-local fixture-ref formula

help:
	@printf '%s\n' \
		'make build             Build artifacts and import the repaired shell locally (no push)' \
		'make prepare-shell-image  Prepare locally; SHELL_IMAGE_REPOSITORY opts into publication' \
		'make test              Run format, lint, and workspace tests' \
		'make onboarding        Run credential-free production shell journeys' \
		'make mcp               Run installed host stdio MCP server for this workspace' \
		'make mcp-test          Run the host MCP server tests' \
		'make install           Install marsh (SBX remains Homebrew-managed)' \
		'make uninstall         Remove installed marsh files under PREFIX; preserve user state' \
		'make kits              Validate native Kit v3 source with Buildx' \
		'make kit-publish KIT_REPOSITORY_PREFIX=registry/repository  Publish Kits and pin their digests' \
		'make kit-publish-fixture KIT_REPOSITORY_PREFIX=registry/repository  Publish acceptance fixture' \
		'make dev-check         Run focused checks in a Linux contributor or development shell' \
		'make man               Render man pages into $$(MAN_DIR)' \
		'make site              Build the runmar.sh site into $$(SITE_DIR) and check its links' \
		'make dist              Package a release tarball (needs pinned KIT_COMMANDS and shell-image)' \
		'make fixture-ref       Fetch the published acceptance fixture ref into $$(FIXTURE_REF_FILE)' \
		'make acceptance        Run the full local-v3 stock-SBX gate' \
		'make acceptance-smoke  Run the assembled stock-SBX smoke gate' \
		'make perf              Measure the assembled warm registered-command path' \
		'make dev               Build everything (host, guest, images, Kits) and install the dev product in $$(DEV_PREFIX)' \
		'make check [DEV_KIT=ref]  Fast real smoke of the installed dev product (warm scope in target/check)' \
		'make check-reset       Stop the make check scope and delete target/check' \
		'make regress [DEV_KIT=ref]  Run every existing suite sequentially (before a release)' \
		'make dev-smoke [DEV_KIT=ref]  Run the acceptance smoke against the installed dev product (no receipt)' \
		'make dev-acceptance DEV_KIT=ref  Run the full acceptance gate against the installed dev product' \
		'make dev-split DEV_KIT=ref  Run the split/join observation against the installed dev product' \
		'make dev-processes DEV_KIT=ref  Run the nested-process acceptance against the installed dev product' \
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

test:
	$(CARGO) fmt --all -- --check
	$(CARGO) clippy --workspace --all-targets --locked --target-dir "$(TARGET_DIR)" -- -D warnings
	$(CARGO) test --workspace --locked --target-dir "$(TARGET_DIR)"
	$(CARGO) build --workspace --locked --target-dir "$(TARGET_DIR)"
	sh tests/kits/selected-home.sh
	sh tests/kits/cli-adapters.sh
	sh tests/kits/shell-adapter.sh
	$(PYTHON) -m unittest discover -s tests/acceptance -p 'test_*.py'
	$(PYTHON) -m unittest discover -s tests/perf -p 'test_*.py'
	$(MAKE) mcp-load-callers
	@set -eu; for suite in $(MCP_TEST_FILES); do \
		MARSH_BINARY="$(abspath $(TARGET_DIR))/mcp-callers/bin/marsh" MARSH_MCP_BIN="$(abspath $(TARGET_DIR))/mcp-callers/bin/marsh-mcp" \
		$(PYTHON) -m unittest discover -s tests/mcp -p "$$suite"; \
	done

mcp:
	@test -x "$(MCP_BIN)" || { echo 'make mcp: install first with make install, or set MCP_BIN=/absolute/marsh-mcp'; exit 1; }
	@test -x "$(MCP_MARSH)" || { echo 'make mcp: install marsh outside this checkout, or set MCP_MARSH=/absolute/marsh'; exit 1; }
	@test -x "$(SBX_EXECUTABLE)" || { echo 'make mcp: install stock SBX v0.45.0 or newer on PATH, or set MARSH_SBX=/absolute/sbx'; exit 1; }
	@exec "$(MCP_BIN)" serve --workspace "$(CURDIR)" --marsh "$(MCP_MARSH)" --sbx "$(SBX_EXECUTABLE)"

onboarding:
	$(CARGO) build --locked -p marsh --bin marsh --bin marsh-local --target-dir "$(TARGET_DIR)"
	$(PYTHON) tests/acceptance/onboarding.py \
		--marsh "$(abspath $(TARGET_DIR))/debug/marsh" \
		--shell "$(abspath $(TARGET_DIR))/debug/marsh-local"

.PHONY: mcp-load-callers
MCP_TEST_EVIDENCE ?= $(abspath $(TARGET_DIR))/mcp-test-evidence
mcp-load-callers:
	$(PYTHON) tests/mcp/run_publication_callers.py --cargo "$(CARGO)" \
		--target-dir "$(TARGET_DIR)" --evidence "$(MCP_TEST_EVIDENCE)" $(if $(MCP_LONG_PREPARE),--long-prepare,) $(if $(MCP_LONG_ROLLBACK),--long-rollback,) $(if $(MCP_ACP_CALLERS),--acp,)

mcp-test:
	$(CARGO) test -p marsh-mcp --locked --target-dir "$(TARGET_DIR)"
	$(MAKE) mcp-load-callers
	@printf '%s\n' 'MCP caller profile: $(MCP_TEST_FILES) (Mac exporter startup is not qualified on Linux)'
	@set -eu; for suite in $(MCP_TEST_FILES); do \
		MARSH_BINARY="$(abspath $(TARGET_DIR))/mcp-callers/bin/marsh" MARSH_MCP_BIN="$(abspath $(TARGET_DIR))/mcp-callers/bin/marsh-mcp" \
		$(PYTHON) -m unittest discover -s tests/mcp -p "$$suite"; \
	done

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
		--commands "$(KIT_COMMANDS)" --build-inputs "$(KIT_PREPARED_INPUTS)" \
		--output "$(KIT_RELEASE_COMMANDS)"

kit-publish-fixture:
	@test -n "$(KIT_REPOSITORY_PREFIX)" || { echo 'make kit-publish-fixture: set KIT_REPOSITORY_PREFIX=registry/repository'; exit 1; }
	$(PYTHON) scripts/prepare-kit-inputs.py --commands tests/acceptance/fixture/commands.json --output "$(KIT_FIXTURE_INPUTS)"
	$(PYTHON) scripts/publish-kits.py --repository-prefix "$(KIT_REPOSITORY_PREFIX)" \
		$(if $(KIT_REPOSITORY_NAME_PREFIX),--repository-name-prefix "$(KIT_REPOSITORY_NAME_PREFIX)",) \
		--commands tests/acceptance/fixture/commands.json --build-inputs "$(KIT_FIXTURE_INPUTS)" --output "$(KIT_FIXTURE_COMMANDS)"

dev-check:
	@case "$$(uname -s)" in Linux) ;; *) echo 'make dev-check: run in a Linux contributor or development shell'; exit 1;; esac
	@sh scripts/bootstrap-rust-guest.sh -- $(CARGO) fmt --all -- --check
	@sh scripts/bootstrap-rust-guest.sh -- $(CARGO) test --locked -p marsh --test smoke executes_an_ordinary_command_with_exact_streams_and_status -- --exact

install-preflight:
	@test "$$(uname -s)" = Darwin -a "$$(uname -m)" = arm64 || { echo 'make install: the host product requires an ARM64 Mac; Linux contributors can use make onboarding or make build-host' >&2; exit 1; }

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

# Homebrew formula for the tap, from a dist tarball's checksum.
formula:
	$(PYTHON) scripts/render-formula.py --version "$(VERSION)" --repository "$(GITHUB_REPOSITORY)" \
		--sha256 "$$(cut -d' ' -f1 "$(DIST_DIR)/marsh-$(VERSION)-darwin-arm64.tar.gz.sha256")" \
		--output "$(DIST_DIR)/marsh.rb"

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
# Evidence must be outside the mounted project and free of symlinks (/var -> /private/var).
DEV_SMOKE_EVIDENCE ?= $(shell cd "$${TMPDIR:-/tmp}" && pwd -P)/marsh-dev-smoke-$(shell id -u)
DEV_ACCEPTANCE_EVIDENCE ?= $(shell cd "$${TMPDIR:-/tmp}" && pwd -P)/marsh-dev-acceptance-$(shell id -u)
DEV_KIT ?= $(ACCEPTANCE_KIT)
DEV_SUPPORT_SCRIPTS := $(addprefix scripts/,prepare-shell-image.py build_inputs.py image_observations.py owned_process.py stock_sdk.py)
DEV_CANONICAL_INPUTS := $(shell find packaging/image-repair packaging/dhi-notices/collected -type f 2>/dev/null)
DEV_SHELL_IMAGE_INPUTS := packaging/shell-image packaging/shell/Dockerfile $(DEV_SUPPORT_SCRIPTS) $(DEV_CANONICAL_INPUTS) \
	$(shell find packaging/shell/pi -type f 2>/dev/null)
DEV_KIT_INPUTS := $(KIT_COMMANDS) scripts/stage-kits.py scripts/publish-kits.py scripts/npm-notices.mjs \
	$(DEV_CANONICAL_INPUTS) $(wildcard $(shell git ls-files kits 2>/dev/null))

.PHONY: dev dev-artifacts dev-host dev-guest dev-install dev-kit-images dev-smoke dev-acceptance
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

dev-smoke: dev
	@test -n "$(DEV_KIT)" || { echo 'make dev-smoke: run make fixture-ref, or set DEV_KIT to an immutable fixture Kit OCI reference'; exit 1; }
	@mkdir -p "$(DEV_SMOKE_EVIDENCE)" && chmod 700 "$(DEV_SMOKE_EVIDENCE)"
	$(PYTHON) tests/acceptance/smoke.py --marsh "$(DEV_PREFIX)/bin/marsh" \
		--guest-artifacts "$(DEV_PREFIX)/libexec/marsh" --sbx "$(SBX_EXECUTABLE)" --kit "$(DEV_KIT)" \
		--evidence "$(DEV_SMOKE_EVIDENCE)" --source-revision "$(SOURCE_REVISION)" --source-tree "$(CURDIR)"

# The full observation gate against the installed dev product (no receipt).
dev-acceptance: dev
	@test -n "$(DEV_KIT)" || { echo 'make dev-acceptance: set DEV_KIT to an immutable fixture Kit OCI reference'; exit 1; }
	@mkdir -p "$(DEV_ACCEPTANCE_EVIDENCE)" && chmod 700 "$(DEV_ACCEPTANCE_EVIDENCE)"
	$(PYTHON) tests/acceptance/run.py --marsh "$(DEV_PREFIX)/bin/marsh" \
		--guest-artifacts "$(DEV_PREFIX)/libexec/marsh" --sbx "$(SBX_EXECUTABLE)" --kit "$(DEV_KIT)" \
		--evidence "$(DEV_ACCEPTANCE_EVIDENCE)" --source-revision "$(SOURCE_REVISION)" --source-tree "$(CURDIR)"
	@$(MAKE) --no-print-directory dev-split-run

# split/join observation (CONTRACT 19): the workspaces harness against the
# installed dev product (DEV_KIT needs the fixture's `pipeline` mode).
.PHONY: dev-split dev-split-run
dev-split: dev
	@$(MAKE) --no-print-directory dev-split-run

dev-split-run:
	@$(MAKE) --no-print-directory dev-workspaces

# Daemon-owned workspaces (docs/design/workspaces-acceptance.md) against the installed
# dev product. DEV_KIT may be an OCI ref or tests/acceptance/fixture (nested
# scenarios need its `pipeline` mode). ONLY=W01-plain-bash selects one scenario.
DEV_WORKSPACES_EVIDENCE ?= $(shell cd "$${TMPDIR:-/tmp}" && pwd -P)/marsh-dev-workspaces-$(shell id -u)
.PHONY: dev-workspaces
dev-workspaces:
	@test -n "$(DEV_KIT)" || { echo 'make dev-workspaces: set DEV_KIT to a fixture Kit (OCI ref or tests/acceptance/fixture)'; exit 1; }
	@mkdir -p "$(DEV_WORKSPACES_EVIDENCE)" && chmod 700 "$(DEV_WORKSPACES_EVIDENCE)"
	$(PYTHON) tests/acceptance/workspaces_uat.py --prefix "$(DEV_PREFIX)" --sbx "$(SBX_EXECUTABLE)" \
		--kit "$(DEV_KIT)" --evidence "$(DEV_WORKSPACES_EVIDENCE)" --source-revision "$(SOURCE_REVISION)" \
		--source-tree "$(CURDIR)" $(foreach s,$(ONLY),--only $(s))

# Nested processes (docs/design/processes-acceptance.md) against the installed dev
# product. DEV_KIT may be an OCI ref or tests/acceptance/fixture (the scenarios
# need its exec/bench/cap-flood modes). ONLY=P02-self-spawn-bash-c selects one.
DEV_PROCESSES_EVIDENCE ?= $(shell cd "$${TMPDIR:-/tmp}" && pwd -P)/marsh-dev-processes-$(shell id -u)
.PHONY: dev-processes
dev-processes:
	@test -n "$(DEV_KIT)" || { echo 'make dev-processes: set DEV_KIT to a fixture Kit (OCI ref or tests/acceptance/fixture)'; exit 1; }
	@mkdir -p "$(DEV_PROCESSES_EVIDENCE)" && chmod 700 "$(DEV_PROCESSES_EVIDENCE)"
	$(PYTHON) tests/acceptance/processes_uat.py --prefix "$(DEV_PREFIX)" --sbx "$(SBX_EXECUTABLE)" \
		--kit "$(DEV_KIT)" --evidence "$(DEV_PROCESSES_EVIDENCE)" --source-revision "$(SOURCE_REVISION)" \
		--source-tree "$(CURDIR)" $(foreach s,$(ONLY),--only $(s))

# Shell choice (docs/shells.md: --shell bash|zsh) against the installed dev
# product. ONLY=S03-split-join selects one scenario; SHELLS=zsh one shell.
DEV_SHELLS_EVIDENCE ?= $(shell cd "$${TMPDIR:-/tmp}" && pwd -P)/marsh-dev-shells-$(shell id -u)
.PHONY: dev-shells
dev-shells:
	@test -n "$(DEV_KIT)" || { echo 'make dev-shells: set DEV_KIT to a fixture Kit (OCI ref or tests/acceptance/fixture)'; exit 1; }
	@mkdir -p "$(DEV_SHELLS_EVIDENCE)" && chmod 700 "$(DEV_SHELLS_EVIDENCE)"
	$(PYTHON) tests/acceptance/shells_uat.py --prefix "$(DEV_PREFIX)" --sbx "$(SBX_EXECUTABLE)" \
		--kit "$(DEV_KIT)" --evidence "$(DEV_SHELLS_EVIDENCE)" --source-revision "$(SOURCE_REVISION)" \
		--source-tree "$(CURDIR)" $(foreach s,$(ONLY),--only $(s)) $(foreach s,$(SHELLS),--shell $(s))

# Fast real smoke (tests/check.py): one persistent isolated scope in
# target/check whose daemon and VMs stay warm between runs. Run after make dev.
.PHONY: check check-reset regress
check:
	@test -n "$(DEV_KIT)" || { echo 'make check: run make fixture-ref, or set DEV_KIT to an immutable fixture Kit OCI reference'; exit 1; }
	@$(PYTHON) tests/check.py --prefix "$(DEV_PREFIX)" --sbx "$(SBX_EXECUTABLE)" --kit "$(DEV_KIT)"

check-reset:
	@$(PYTHON) tests/check.py --reset --prefix "$(DEV_PREFIX)" --sbx "$(SBX_EXECUTABLE)"

# Every existing suite, sequentially, against the dev product (slow).
REGRESS_EVIDENCE ?= $(shell cd "$${TMPDIR:-/tmp}" && pwd -P)/marsh-regress-$(shell id -u)
regress:
	@test -n "$(DEV_KIT)" || { echo 'make regress: run make fixture-ref, or set DEV_KIT to an immutable fixture Kit OCI reference'; exit 1; }
	@mkdir -p "$(REGRESS_EVIDENCE)" && chmod 700 "$(REGRESS_EVIDENCE)"
	@printf '{"fixture": "%s"}\n' "$(DEV_KIT)" > "$(REGRESS_EVIDENCE)/fixture-commands.json"
	$(MAKE) --no-print-directory dev-smoke
	$(MAKE) --no-print-directory dev-acceptance
	$(MAKE) --no-print-directory dev-processes
	$(MAKE) --no-print-directory dev-shells
	$(PYTHON) tests/acceptance/acp_uat.py --marsh "$(DEV_PREFIX)/bin/marsh" --guest-artifacts "$(DEV_PREFIX)/libexec/marsh" \
		--sbx "$(SBX_EXECUTABLE)" --evidence "$(REGRESS_EVIDENCE)/acp" --source-revision "$(SOURCE_REVISION)" --source-tree "$(CURDIR)"
	$(PYTHON) tests/acceptance/mcp_load_uat.py --marsh "$(DEV_PREFIX)/bin/marsh" --guest-artifacts "$(DEV_PREFIX)/libexec/marsh" \
		--sbx "$(SBX_EXECUTABLE)" --kit fixture --commands "$(REGRESS_EVIDENCE)/fixture-commands.json" \
		--evidence "$(REGRESS_EVIDENCE)/mcp-load" --source-revision "$(SOURCE_REVISION)" --source-tree "$(CURDIR)"
	$(PYTHON) tests/acceptance/self_dev.py --prefix "$(DEV_PREFIX)" --sbx "$(SBX_EXECUTABLE)" --kit "$(DEV_KIT)" \
		--evidence "$(REGRESS_EVIDENCE)/self-dev"
	$(MAKE) --no-print-directory mcp-test
	$(CARGO) test --workspace --locked --target-dir "$(TARGET_DIR)"
	docs/model/check.sh

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
