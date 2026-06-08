# Wetware build system
#
# Builds std/ components and places artifacts at <component>/boot/main.wasm.
# Publish to IPFS with: ww push std/

WASM_TARGET := wasm32-wasip2

.PHONY: all host std kernel shell status examples chess echo counter discovery oracle snap-hello-rs clean run-kernel
.PHONY: publish-std try-publish-std publish test-deps test test-wasm
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

# --- Std components ----------------------------------------------------------

std: kernel shell status

kernel:
	cargo build -p kernel --target $(WASM_TARGET) --release --manifest-path std/kernel/Cargo.toml
	@mkdir -p std/kernel/bin
	cp std/kernel/target/$(WASM_TARGET)/release/kernel.wasm std/kernel/bin/main.wasm

shell:
	cargo build -p shell --target $(WASM_TARGET) --release --manifest-path std/shell/Cargo.toml
	@mkdir -p std/shell/bin
	cp std/shell/target/$(WASM_TARGET)/release/shell.wasm std/shell/bin/shell.wasm

status:
	cargo build -p status --target $(WASM_TARGET) --release --manifest-path std/status/Cargo.toml
	@mkdir -p std/status/bin
	cp std/status/target/$(WASM_TARGET)/release/status.wasm std/status/bin/status.wasm

# --- Examples ----------------------------------------------------------------

examples: chess echo counter discovery oracle snap-hello-rs

chess:
	$(MAKE) -C examples/chess

echo:
	$(MAKE) -C examples/echo

counter:
	$(MAKE) -C examples/counter

discovery:
	$(MAKE) -C examples/discovery

oracle:
	$(MAKE) -C examples/oracle

snap-hello-rs:
	$(MAKE) -C examples/snap-hello-rs

# --- Publish std namespace to IPFS ------------------------------------------
# CI-only: assembles the ww namespace tree, publishes to IPFS, writes CID.
# Local builds skip this — empty CID triggers HostPathLoader fallback.
#
# Usage:
#   make publish-std                    # publish and write CID
#   make publish-std IPNS_KEY=wetware   # also publish to IPNS name

IPNS_KEY ?=

# Best-effort publish: runs as part of `make all`. If Kubo isn't running,
# the build continues without a CID (HostPathLoader fallback).
# Reads IPNS key from ~/.ww/etc/ns/ww if available (provisioned by `ww perform install`).
try-publish-std: std
	@KEY=$$(grep '^ipns=' ~/.ww/etc/ns/ww 2>/dev/null | cut -d= -f2 | tr -d ' '); \
	if [ -n "$$KEY" ]; then \
		$(MAKE) publish-std IPNS_KEY=ww 2>/dev/null \
			&& echo "  std namespace published to IPFS" \
			|| echo "  std namespace publish skipped (Kubo not running)"; \
	else \
		$(MAKE) publish-std 2>/dev/null \
			&& echo "  std namespace published to IPFS (no IPNS key)" \
			|| echo "  std namespace publish skipped (Kubo not running)"; \
	fi

publish-std: std
	@echo "Assembling std namespace tree..."
	$(eval STD_TREE := $(shell mktemp -d))
	@mkdir -p $(STD_TREE)/lib/ww
	@mkdir -p $(STD_TREE)/kernel/bin
	@mkdir -p $(STD_TREE)/shell/bin
	@cp std/lib/ww/*.glia $(STD_TREE)/lib/ww/
	@cp std/kernel/bin/main.wasm $(STD_TREE)/kernel/bin/main.wasm
	@cp std/shell/bin/shell.wasm $(STD_TREE)/shell/bin/shell.wasm
	@echo "Publishing to IPFS..."
	@CID=$$(ipfs add -r --cid-version=1 -Q $(STD_TREE)) && \
		echo "$$CID" > target/std-namespace.cid && \
		echo "  CID: $$CID" && \
		if [ -n "$(IPNS_KEY)" ]; then \
			echo "Publishing to IPNS key $(IPNS_KEY)..." && \
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
#   make publish          # publish tree + pin + IPNS update
#   make publish SKIP_PIN=1  # publish tree only (no remote pin)

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
	@CID=$$(ipfs add -rQ --cid-version=1 "$(RELEASE_TREE)") && \
		echo "  CID: $$CID" && \
		echo "$$CID" > target/release.cid && \
		if [ -z "$(SKIP_PIN)" ] && ipfs key list | grep -q ww-release 2>/dev/null; then \
			echo "Pinning and publishing to IPNS..." && \
			ipfs name publish --key=ww-release /ipfs/$$CID; \
		else \
			echo "  (skip pin/IPNS — no ww-release key or SKIP_PIN set)"; \
		fi
	@rm -rf "$(RELEASE_TREE)"
	@echo "Done. CID written to target/release.cid"

# --- Test WASM components ----------------------------------------------------
# Scaffolding for WASM component tests. Today this is a no-op because the
# WASM crates (kernel, shell, status) have no test suite yet. When tests are
# added, this target will run them. CI calls this after building WASM.
#
# WASM crates can't run cargo test on the host (they depend on wasip2).
# Tests should be either:
#   - Host-side integration tests that spawn cells and test via RPC
#   - wasmtime-based test harness for unit tests
#
# For now, just verify the binaries were produced.
test-wasm: std
	@echo "Verifying WASM artifacts..."
	@test -f std/kernel/bin/main.wasm  || { echo "FAIL: kernel WASM missing"; exit 1; }
	@test -f std/shell/bin/shell.wasm  || { echo "FAIL: shell WASM missing"; exit 1; }
	@test -f std/status/bin/status.wasm || { echo "FAIL: status WASM missing"; exit 1; }
	@echo "WASM artifacts OK (no test suite yet — see Makefile for guidance)"

# --- Run ---------------------------------------------------------------------

run-kernel: kernel
	cargo run -- run std/kernel

# --- Clean -------------------------------------------------------------------

clean:
	cargo clean
	rm -f std/kernel/bin/main.wasm
	rm -f std/shell/bin/shell.wasm
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
		--build-arg GIT_COMMIT=$$(git rev-parse --short HEAD) \
		-t $(CONTAINER_TAG) .

container-run:
	$(CONTAINER_ENGINE) run --rm -it -p 8080:8080 $(CONTAINER_TAG)

container-dev: container-build
	$(CONTAINER_ENGINE) run --rm -it \
		-v $(PWD)/config:/app/config:ro \
		-p 8080:8080 $(CONTAINER_TAG)

container-clean:
	$(CONTAINER_ENGINE) rmi $(CONTAINER_TAG) || true
