# Wetware build system
#
# Builds std/ components and examples as native wasm32-wasip3 artifacts.
# Publish to IPFS with: ww push std/

P3_BUILD := scripts/build_wasip3_component.sh
P3_TARGET_DIR ?= $(CURDIR)/target/wasip3

P3_CLI_ENV := --allow-import wasi:cli/environment@0.3.0 --allow-import wasi:cli/exit@0.3.0
P3_CLI_TYPES := --allow-import wasi:cli/types@0.3.0
P3_STDIN := --allow-import wasi:cli/stdin@0.3.0
P3_STDOUT := --allow-import wasi:cli/stdout@0.3.0
P3_STDERR := --allow-import wasi:cli/stderr@0.3.0
P3_TERMINAL_IN := --allow-import wasi:cli/terminal-input@0.3.0 --allow-import wasi:cli/terminal-stdin@0.3.0
P3_TERMINAL_OUT := --allow-import wasi:cli/terminal-output@0.3.0 --allow-import wasi:cli/terminal-stdout@0.3.0 --allow-import wasi:cli/terminal-stderr@0.3.0
P3_MONOTONIC_CLOCK := --allow-import wasi:clocks/types@0.3.0 --allow-import wasi:clocks/monotonic-clock@0.3.0
P3_SYSTEM_CLOCK := --allow-import wasi:clocks/types@0.3.0 --allow-import wasi:clocks/system-clock@0.3.0
P3_FILESYSTEM := --allow-import wasi:filesystem/types@0.3.0 --allow-import wasi:filesystem/preopens@0.3.0 $(P3_SYSTEM_CLOCK)
P3_RANDOM := --allow-import wasi:random/random@0.3.0 --allow-import wasi:random/insecure-seed@0.3.0
P3_INSECURE_SEED := --allow-import wasi:random/insecure-seed@0.3.0
P3_TRANSPORT := --allow-import wetware:transport/connection@0.2.0
P3_ROUTING := --allow-import wetware:routing/key@0.1.0
P3_READINESS := --allow-import wetware:kernel-runtime/readiness@1.0.0

.PHONY: all host std kernel status examples chess echo counter discovery oracle snap-hello-rs clean run-kernel
.PHONY: publish-std try-publish-std publish test-deps test test-wasm test-p3-fixture authority-probe routing-key-probe
.PHONY: p3-chess p3-counter p3-discovery p3-echo p3-oracle p3-snap-hello-rs
.PHONY: container-build container-run container-dev container-clean
.PHONY: agent-skills

all: std try-publish-std examples host

# --- Host --------------------------------------------------------------------

host:
	cargo build --release

test-deps:
	git submodule update --init contracts/stem/lib/forge-std

test: test-deps
	cargo test --workspace

test-p3-fixture:
	bash scripts/check_native_p3_fixture.sh

# Build the disposable real-WASM adversarial guest used by the T1 confinement
# harness. The integration test also builds it on demand in a separate target
# directory so clean CI runs do not depend on a checked-in binary.
authority-probe:
	$(P3_BUILD) \
		--name authority-probe \
		--manifest tests/fixtures/authority-probe/Cargo.toml \
		--artifact authority_probe \
		--target-dir $(CURDIR)/target/authority-probe \
		$(P3_CLI_ENV) $(P3_CLI_TYPES) $(P3_STDIN) $(P3_STDOUT) $(P3_STDERR) \
		$(P3_TERMINAL_IN) $(P3_TERMINAL_OUT) $(P3_FILESYSTEM) \
		$(P3_MONOTONIC_CLOCK) $(P3_SYSTEM_CLOCK) $(P3_RANDOM) $(P3_TRANSPORT)

routing-key-probe:
	$(P3_BUILD) \
		--name routing-key-probe \
		--manifest tests/fixtures/routing-key-probe/Cargo.toml \
		--artifact routing_key_probe \
		--target-dir $(CURDIR)/target/routing-key-probe \
		$(P3_CLI_ENV) $(P3_CLI_TYPES) $(P3_STDIN) $(P3_STDOUT) $(P3_STDERR) \
		$(P3_TERMINAL_IN) $(P3_TERMINAL_OUT) $(P3_MONOTONIC_CLOCK) $(P3_SYSTEM_CLOCK) \
		$(P3_ROUTING)

# --- Std components ----------------------------------------------------------

std: kernel status

kernel:
	$(P3_BUILD) \
		--name kernel \
		--manifest std/kernel/Cargo.toml \
		--artifact kernel \
		--target-dir $(P3_TARGET_DIR) \
		--output std/kernel/bin/main.wasm \
		$(P3_CLI_ENV) $(P3_CLI_TYPES) $(P3_STDIN) $(P3_STDOUT) $(P3_STDERR) \
		$(P3_TERMINAL_IN) $(P3_TERMINAL_OUT) $(P3_FILESYSTEM) \
		$(P3_MONOTONIC_CLOCK) $(P3_INSECURE_SEED) $(P3_TRANSPORT) $(P3_READINESS)

status:
	$(P3_BUILD) \
		--name status \
		--manifest std/status/Cargo.toml \
		--artifact status \
		--target-dir $(P3_TARGET_DIR) \
		--output std/status/bin/status.wasm \
		$(P3_CLI_ENV) $(P3_CLI_TYPES) $(P3_STDIN) $(P3_STDOUT) $(P3_STDERR) \
		$(P3_TERMINAL_IN) $(P3_TERMINAL_OUT) $(P3_MONOTONIC_CLOCK) \
		$(P3_SYSTEM_CLOCK) $(P3_INSECURE_SEED) $(P3_TRANSPORT)

# --- Examples ----------------------------------------------------------------

examples: chess echo counter discovery oracle snap-hello-rs

chess: p3-chess

echo: p3-echo

counter: p3-counter

discovery: p3-discovery

oracle: p3-oracle

snap-hello-rs: p3-snap-hello-rs

p3-chess:
	$(P3_BUILD) \
		--name chess \
		--manifest examples/chess/Cargo.toml \
		--artifact chess \
		--package chess \
		--target-dir $(P3_TARGET_DIR) \
		--output examples/chess/bin/chess-demo.wasm \
		$(P3_CLI_ENV) $(P3_CLI_TYPES) $(P3_STDIN) $(P3_STDOUT) $(P3_STDERR) \
		$(P3_TERMINAL_IN) $(P3_TERMINAL_OUT) $(P3_MONOTONIC_CLOCK) $(P3_SYSTEM_CLOCK) \
		$(P3_RANDOM) $(P3_TRANSPORT) $(P3_ROUTING)

p3-counter:
	$(P3_BUILD) \
		--name counter \
		--manifest examples/counter/Cargo.toml \
		--artifact counter \
		--target-dir $(P3_TARGET_DIR) \
		--output examples/counter/bin/counter.wasm \
		$(P3_CLI_ENV) $(P3_CLI_TYPES) $(P3_STDIN) $(P3_STDOUT) $(P3_STDERR) \
		$(P3_TERMINAL_IN) $(P3_TERMINAL_OUT) $(P3_MONOTONIC_CLOCK) $(P3_SYSTEM_CLOCK)

p3-discovery:
	$(P3_BUILD) \
		--name discovery \
		--manifest examples/discovery/Cargo.toml \
		--artifact discovery \
		--package discovery \
		--target-dir $(P3_TARGET_DIR) \
		--output examples/discovery/bin/discovery.wasm \
		$(P3_CLI_ENV) $(P3_CLI_TYPES) $(P3_STDIN) $(P3_STDOUT) $(P3_STDERR) \
		$(P3_TERMINAL_IN) $(P3_TERMINAL_OUT) $(P3_MONOTONIC_CLOCK) $(P3_SYSTEM_CLOCK) \
		$(P3_RANDOM) $(P3_TRANSPORT) $(P3_ROUTING)

p3-echo:
	$(P3_BUILD) \
		--name echo \
		--manifest examples/echo/Cargo.toml \
		--artifact echo \
		--target-dir $(P3_TARGET_DIR) \
		--output examples/echo/bin/echo.wasm \
		$(P3_CLI_ENV) $(P3_CLI_TYPES) $(P3_STDIN) $(P3_STDOUT) $(P3_STDERR) \
		$(P3_TERMINAL_IN) $(P3_TERMINAL_OUT) $(P3_MONOTONIC_CLOCK) $(P3_SYSTEM_CLOCK)

p3-oracle:
	$(P3_BUILD) \
		--name oracle \
		--manifest examples/oracle/Cargo.toml \
		--artifact oracle \
		--target-dir $(P3_TARGET_DIR) \
		--output examples/oracle/bin/oracle.wasm \
		$(P3_CLI_ENV) $(P3_CLI_TYPES) $(P3_STDIN) $(P3_STDOUT) $(P3_STDERR) \
		$(P3_TERMINAL_IN) $(P3_TERMINAL_OUT) \
		$(P3_MONOTONIC_CLOCK) $(P3_SYSTEM_CLOCK) $(P3_RANDOM) \
		$(P3_TRANSPORT) $(P3_ROUTING)

p3-snap-hello-rs:
	$(P3_BUILD) \
		--name snap-hello-rs \
		--manifest examples/snap-hello-rs/Cargo.toml \
		--artifact snap_hello_rs \
		--target-dir $(P3_TARGET_DIR) \
		--output examples/snap-hello-rs/bin/snap-hello-rs.wasm \
		$(P3_CLI_ENV) $(P3_CLI_TYPES) $(P3_STDIN) $(P3_STDOUT) $(P3_STDERR) \
		$(P3_TERMINAL_IN) $(P3_TERMINAL_OUT) $(P3_MONOTONIC_CLOCK) $(P3_SYSTEM_CLOCK)

# --- Publish std namespace to IPFS ------------------------------------------
# CI-only: assembles the ww namespace tree, publishes to IPFS, writes CID.
# Local builds skip this — empty CID triggers HostPathLoader fallback.
#
# Usage:
#   make publish-std                    # publish and write CID, without pinning
#   make publish-std IPNS_KEY=wetware   # pin durably and publish to IPNS name
#
# Local durable publishes do not prune old pins; remove obsolete local pins
# manually when they are no longer needed.

IPNS_KEY ?=

# Best-effort publish: runs as part of `make all`. If Kubo isn't running,
# the build continues without a CID (HostPathLoader fallback).
# Default IPNS publication is host-owned and runs through `ww perform update`
# plus the daemon republisher. This build target only imports the tree.
try-publish-std: std
	@$(MAKE) publish-std 2>/dev/null \
		&& echo "  std namespace published to IPFS (host IPNS publish is daemon-owned)" \
		|| echo "  std namespace publish skipped (Kubo not running)"

publish-std: std
	@echo "Assembling std namespace tree..."
	$(eval STD_TREE := $(shell mktemp -d))
	@mkdir -p $(STD_TREE)/kernel/bin
	@cp std/kernel/bin/main.wasm $(STD_TREE)/kernel/bin/main.wasm
	@echo "Publishing to IPFS..."
	@CID=$$(ipfs add --pin=false -r --cid-version=1 -Q $(STD_TREE)) && \
		echo "$$CID" > target/std-namespace.cid && \
		echo "  CID: $$CID" && \
		if [ -n "$(IPNS_KEY)" ]; then \
			echo "Pinning and publishing to IPNS key $(IPNS_KEY)..." && \
			ipfs pin add "$$CID" && \
			ipfs name publish --key=$(IPNS_KEY) /ipfs/$$CID; \
		fi
	@rm -rf $(STD_TREE)
	@echo "CID written to target/std-namespace.cid"

# --- Publish release tree to IPFS --------------------------------------------
# Local equivalent of CI publish-ipfs. Publishes the repo working tree
# (minus .git/target) with your local binary at bin/{os}/{arch}/ww.
# Updates IPNS at releases.wetware.run if ww-release key exists.
#
# Usage:
#   make publish          # publish tree + durable local pin + IPNS update
#   make publish SKIP_PIN=1  # publish tree only (no durable pin)
#
# Local durable publishes do not prune old pins; remove obsolete local pins
# manually when they are no longer needed.

SKIP_PIN ?=

publish: host
	@echo "Assembling release tree..."
	$(eval RELEASE_TREE := $(shell mktemp -d))
	@rsync -a --exclude .git --exclude target . "$(RELEASE_TREE)/"
	@mkdir -p "$(RELEASE_TREE)/bin/$$(uname -s | tr A-Z a-z)/$$(uname -m)"
	@cp target/release/ww "$(RELEASE_TREE)/bin/$$(uname -s | tr A-Z a-z)/$$(uname -m)/ww"
	@cd "$(RELEASE_TREE)" && { \
		echo "# sha256"; \
		find bin/ -type f | sort | xargs shasum -a 256; \
		echo ""; \
		if command -v b3sum >/dev/null 2>&1; then \
			echo "# blake3"; \
			find bin/ -type f | sort | xargs b3sum; \
		fi; \
	} > CHECKSUMS.txt
	@echo "Publishing to IPFS..."
	@CID=$$(ipfs add --pin=false -rQ --cid-version=1 "$(RELEASE_TREE)") && \
		echo "  CID: $$CID" && \
		echo "$$CID" > target/release.cid && \
		if [ -z "$(SKIP_PIN)" ] && ipfs key list | grep -q ww-release 2>/dev/null; then \
			echo "Pinning and publishing to IPNS..." && \
			ipfs pin add "$$CID" && \
			ipfs name publish --key=ww-release /ipfs/$$CID; \
		else \
			echo "  (skip pin/IPNS — no ww-release key or SKIP_PIN set)"; \
		fi
	@rm -rf "$(RELEASE_TREE)"
	@echo "Done. CID written to target/release.cid"

# --- Test WASM components ----------------------------------------------------
# Build every first-party component and verify its P3 command architecture.
# Runtime integration tests consume these validated artifacts from CI.
test-wasm: std examples authority-probe routing-key-probe
	bash scripts/check_first_party_wasip3.sh

# --- Run ---------------------------------------------------------------------

run-kernel: kernel status
	cargo run -- run std/status

# --- Clean -------------------------------------------------------------------

clean:
	cargo clean
	rm -f std/kernel/bin/main.wasm
	rm -f std/status/bin/status.wasm
	$(MAKE) -C examples/chess clean
	$(MAKE) -C examples/echo clean
	$(MAKE) -C examples/counter clean
	$(MAKE) -C examples/discovery clean
	$(MAKE) -C examples/oracle clean
	$(MAKE) -C examples/snap-hello-rs clean

# --- Agent skills ------------------------------------------------------------
# Generate .claude/skills/ from .agents/skills/ (vendor-neutral source of truth).

agent-skills:
	bash .agents/generate.sh

# --- Container ---------------------------------------------------------------

CONTAINER_ENGINE ?= podman
CONTAINER_TAG    ?= wetware:latest

container-build:
	$(CONTAINER_ENGINE) build \
		--build-arg WW_BUILD_GIT_SHA=$$(git rev-parse HEAD) \
		-t $(CONTAINER_TAG) .

container-run:
	$(CONTAINER_ENGINE) run --rm -it -p 8080:8080 $(CONTAINER_TAG)

container-dev: container-build
	$(CONTAINER_ENGINE) run --rm -it \
		-v $(PWD)/config:/app/config:ro \
		-p 8080:8080 $(CONTAINER_TAG)

container-clean:
	$(CONTAINER_ENGINE) rmi $(CONTAINER_TAG) || true
