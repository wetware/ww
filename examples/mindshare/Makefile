# Mindshare — symmetric p2p context sharing for LLMs
#
# Usage: make -C examples/mindshare

# Default to rustup cargo so `make -C examples/mindshare` works without shell profile
CARGO ?= cargo

WASM_TARGET := wasm32-wasip2

.PHONY: all clean

all:
	$(CARGO) build --target $(WASM_TARGET) --release \
		--manifest-path Cargo.toml
	@mkdir -p bin
	cp target/$(WASM_TARGET)/release/mindshare.wasm bin/mindshare.wasm
	@# Copy compiled schema bytes next to the WASM binary.
	@MINDSHARE_OUT=$$(find target/$(WASM_TARGET)/release/build -path '*/mindshare-*/out/mindshare_schema.bin' | head -1) && \
		if [ -n "$$MINDSHARE_OUT" ]; then \
			cp "$$MINDSHARE_OUT" bin/mindshare.capnpc; \
		else \
			echo "WARNING: mindshare_schema.bin not found"; \
		fi
	@MINDSHARE_BUNDLE=$$(find target/$(WASM_TARGET)/release/build -path '*/mindshare-*/out/mindshare_schema_bundle.bin' | head -1) && \
		if [ -n "$$MINDSHARE_BUNDLE" ]; then \
			$(CARGO) run -q --manifest-path ../../crates/schema-id/Cargo.toml --bin schema-bundle-inject -- bin/mindshare.wasm "$$MINDSHARE_BUNDLE"; \
		else \
			echo "WARNING: mindshare_schema_bundle.bin not found"; \
		fi

clean:
	rm -f bin/mindshare.wasm bin/mindshare.capnpc
	$(CARGO) clean --manifest-path Cargo.toml
