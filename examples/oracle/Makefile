# Oracle — gas price feed with named vat RPC + Terminal auth
#
# Usage: make -C examples/oracle

# Default to rustup cargo so `make -C examples/oracle` works without shell profile
CARGO ?= cargo

WASM_TARGET := wasm32-wasip2

.PHONY: all clean test

all:
	$(CARGO) build --target $(WASM_TARGET) --release \
		--manifest-path Cargo.toml
	@mkdir -p bin
	cp target/$(WASM_TARGET)/release/oracle.wasm bin/oracle.wasm

test:
	$(CARGO) test --manifest-path Cargo.toml

clean:
	rm -f bin/oracle.wasm
	$(CARGO) clean --manifest-path Cargo.toml
