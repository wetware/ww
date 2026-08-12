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
	@echo "Build the legacy Glia kernel:"
	@echo "  make kernel-glia"
	@echo ""
	@echo "Run the oracle service (terminal 1):"
	@echo "  cargo run -- run --listen /ip4/127.0.0.1/tcp/2025 --with-http-admin off --kernel file:std/kernel-glia/bin/main.wasm std/kernel-glia examples/oracle"
	@echo ""
	@echo "Run a consumer (terminal 2):"
	@echo "  WW_CONSUMER=1 cargo run -- run --listen /ip4/127.0.0.1/tcp/2026 --with-http-admin off --kernel file:std/kernel-glia/bin/main.wasm std/kernel-glia examples/oracle"
	@echo ""

clean:
	rm -f bin/oracle.wasm
	$(CARGO) clean --manifest-path Cargo.toml
