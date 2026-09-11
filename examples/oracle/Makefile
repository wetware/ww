# Oracle — gas price feed with named vat RPC + Terminal auth
#
# Usage: make -C examples/oracle

ROOT := ../..

.PHONY: all clean test

all:
	$(MAKE) -C $(ROOT) p3-oracle

test:
	cargo test --manifest-path Cargo.toml

clean:
	rm -f bin/oracle.wasm
