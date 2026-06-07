# Discovery — Greeter vat service with service-name peer discovery
#
# Usage: make -C examples/discovery

# Default to rustup cargo so `make -C examples/discovery` works without shell profile
CARGO ?= cargo

WASM_TARGET := wasm32-wasip2
ROOT := ../..

.PHONY: all clean

all:
	$(CARGO) build -p discovery --target $(WASM_TARGET) --release
	@mkdir -p bin
	cp $(ROOT)/target/$(WASM_TARGET)/release/discovery.wasm bin/discovery.wasm
	@# Copy compiled schema bytes next to the WASM binary.
	@GREETER_OUT=$$(find $(ROOT)/target/$(WASM_TARGET)/release/build -path '*/discovery-*/out/greeter_schema.bin' | head -1) && \
		if [ -n "$$GREETER_OUT" ]; then \
			cp "$$GREETER_OUT" bin/discovery.capnpc; \
		else \
			echo "WARNING: greeter_schema.bin not found"; \
		fi

clean:
	rm -f bin/discovery.wasm bin/discovery.capnpc
