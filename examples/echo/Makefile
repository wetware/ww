# Echo cell — minimal WASI stdin/stdout echo
#
# Usage: make -C examples/echo

ROOT := ../..

.PHONY: all clean

all:
	$(MAKE) -C $(ROOT) p3-echo

clean:
	rm -f bin/echo.wasm
