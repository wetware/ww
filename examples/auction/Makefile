# Fuel auction — ComputeProvider vat cell with RFQ protocol
#
# Usage: make -C examples/auction

# Default to rustup cargo so `make -C examples/auction` works without shell profile
CARGO ?= cargo

WASM_TARGET := wasm32-wasip2

.PHONY: all clean

all:
	$(CARGO) build --target $(WASM_TARGET) --release \
		--manifest-path Cargo.toml
	@mkdir -p bin
	cp target/$(WASM_TARGET)/release/auction.wasm bin/auction.wasm
	@# Copy compiled schema bytes next to the WASM binary.
	@AUCTION_OUT=$$(find target/$(WASM_TARGET)/release/build -path '*/auction-*/out/auction_schema.bin' | head -1) && \
		if [ -n "$$AUCTION_OUT" ]; then \
			cp "$$AUCTION_OUT" bin/auction.capnpc; \
		else \
			echo "WARNING: auction_schema.bin not found"; \
		fi
	@AUCTION_BUNDLE=$$(find target/$(WASM_TARGET)/release/build -path '*/auction-*/out/auction_schema_bundle.bin' | head -1) && \
		if [ -n "$$AUCTION_BUNDLE" ]; then \
			$(CARGO) run -q --manifest-path ../../crates/schema-id/Cargo.toml --bin schema-bundle-inject -- bin/auction.wasm "$$AUCTION_BUNDLE"; \
		else \
			echo "WARNING: auction_schema_bundle.bin not found"; \
		fi

clean:
	rm -f bin/auction.wasm bin/auction.capnpc
	$(CARGO) clean --manifest-path Cargo.toml
