# Oracle — gas price feed with named vat RPC + Terminal auth
#
# Usage: make -C examples/oracle

# Default to rustup cargo so `make -C examples/oracle` works without shell profile
CARGO ?= cargo

WASM_TARGET := wasm32-wasip2

.PHONY: all clean test demo

all:
	$(CARGO) build --target $(WASM_TARGET) --release \
		--manifest-path Cargo.toml
	@mkdir -p bin
	cp target/$(WASM_TARGET)/release/oracle.wasm bin/oracle.wasm

test:
	$(CARGO) test --manifest-path Cargo.toml

demo: all
	@echo ""
	@echo "=== Price Oracle Demo ==="
	@echo ""
	@echo "Run the oracle service (terminal 1):"
	@echo "  cargo run -- run --port 2025 crates/kernel examples/oracle"
	@echo ""
	@echo "Run a consumer (terminal 2):"
	@echo "  WW_CONSUMER=1 cargo run -- run --port 2026 crates/kernel examples/oracle"
	@echo ""

clean:
	rm -f bin/oracle.wasm
	$(CARGO) clean --manifest-path Cargo.toml
