# Chess engine — Cap'n Proto vat service with service-name discovery
#
# Usage: make -C examples/chess

# Default to rustup cargo so `make -C examples/chess` works without shell profile
CARGO ?= cargo

WASM_TARGET := wasm32-wasip2
ROOT := ../..

.PHONY: all clean

all:
	$(CARGO) build -p chess --target $(WASM_TARGET) --release
	@mkdir -p bin
	cp $(ROOT)/target/$(WASM_TARGET)/release/chess.wasm bin/chess-demo.wasm
	@# Copy compiled schema bytes next to the WASM binary.
	@CHESS_OUT=$$(find $(ROOT)/target/$(WASM_TARGET)/release/build -path '*/chess-*/out/chess_engine_schema.bin' | head -1) && \
		if [ -n "$$CHESS_OUT" ]; then \
			cp "$$CHESS_OUT" bin/chess-demo.capnpc; \
		else \
			echo "WARNING: chess_engine_schema.bin not found"; \
		fi

clean:
	rm -f bin/chess-demo.wasm bin/chess-demo.capnpc
